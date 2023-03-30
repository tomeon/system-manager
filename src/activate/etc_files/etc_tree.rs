use anyhow::Result;
use im::HashMap;
use serde::{Deserialize, Serialize};
use std::cmp::Eq;
use std::iter::Peekable;
use std::path::{Path, PathBuf};
use std::{fs, io, path};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EtcFileStatus {
    Managed,
    Unmanaged,
}

impl EtcFileStatus {
    fn merge(&self, other: &Self) -> Self {
        use EtcFileStatus::*;

        match (self, other) {
            (Unmanaged, Unmanaged) => Unmanaged,
            _ => Managed,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EtcTree {
    status: EtcFileStatus,
    path: PathBuf,
    // TODO directories and files are now both represented as a string associated with a nested
    // map. For files the nested map is simple empty.
    // We could potentially optimise this.
    nested: HashMap<String, EtcTree>,
}

impl AsRef<EtcTree> for EtcTree {
    fn as_ref(&self) -> &EtcTree {
        self
    }
}

impl Default for EtcTree {
    fn default() -> Self {
        Self::root_node()
    }
}

/// Data structure to represent files that are managed by system-manager.
///
/// This data will be serialised to disk and read on the next run.
///
/// We need these basic operations:
/// 1. Create a new root structure
/// 2. Persist to a file
/// 3. Import from a file
/// 4. Add a path to the tree, that will from then on be considered as managed
/// 5.
impl EtcTree {
    fn new(path: PathBuf) -> Self {
        Self::with_status(path, EtcFileStatus::Unmanaged)
    }

    fn with_status(path: PathBuf, status: EtcFileStatus) -> Self {
        Self {
            status,
            path,
            nested: HashMap::new(),
        }
    }

    pub fn root_node() -> Self {
        Self::new(PathBuf::from(path::MAIN_SEPARATOR_STR))
    }

    pub fn get_status<'a>(&'a self, path: &Path) -> &'a EtcFileStatus {
        fn go<'a, 'b, C>(tree: &'a EtcTree, mut components: C, path: &Path) -> &'a EtcFileStatus
        where
            C: Iterator<Item = path::Component<'b>>,
        {
            if let Some(component) = components.next() {
                match component {
                    path::Component::Normal(name) => tree
                        .nested
                        .get(name.to_string_lossy().as_ref())
                        .map(|subtree| go(subtree, components, path))
                        .unwrap_or(&EtcFileStatus::Unmanaged),
                    path::Component::RootDir => go(tree, components, path),
                    _ => todo!(),
                }
            } else {
                debug_assert!(tree.path == path);
                &tree.status
            }
        }
        go(self, path.components(), path)
    }

    pub fn is_managed(&self, path: &Path) -> bool {
        *self.get_status(path) == EtcFileStatus::Managed
    }

    // TODO is recursion OK here?
    // Should we convert to CPS and use a crate like tramp to TCO this?
    pub fn register_managed_entry(self, path: &Path) -> Self {
        fn go<'a, C>(mut tree: EtcTree, mut components: Peekable<C>, path: PathBuf) -> EtcTree
        where
            C: Iterator<Item = path::Component<'a>>,
        {
            if let Some(component) = components.next() {
                match component {
                    path::Component::Normal(name) => {
                        let new_path = path.join(component);
                        tree.nested = tree.nested.alter(
                            |maybe_subtree| {
                                Some(go(
                                    maybe_subtree.unwrap_or_else(|| {
                                        EtcTree::with_status(
                                            new_path.to_owned(),
                                            // We only label as managed the final path entry,
                                            // to label intermediate nodes as managed, we should
                                            // call this function for every one of them separately.
                                            components
                                                .peek()
                                                .map_or(EtcFileStatus::Managed, |_| {
                                                    EtcFileStatus::Unmanaged
                                                }),
                                        )
                                    }),
                                    components,
                                    new_path,
                                ))
                            },
                            name.to_string_lossy().to_string(),
                        );
                        tree
                    }
                    path::Component::RootDir => {
                        go(tree, components, path.join(path::MAIN_SEPARATOR_STR))
                    }
                    _ => panic!(
                        "Unsupported path provided! At path component: {:?}",
                        component
                    ),
                }
            } else {
                tree
            }
        }

        go(self, path.components().peekable(), PathBuf::new())
    }

    pub fn deactivate<F>(self, delete_action: &F) -> Option<EtcTree>
    where
        F: Fn(&Path, &EtcFileStatus) -> bool,
    {
        let new_tree = self.nested.keys().fold(self.clone(), |mut new_tree, name| {
            new_tree.nested = new_tree.nested.alter(
                |subtree| subtree.and_then(|subtree| subtree.deactivate(delete_action)),
                name.to_owned(),
            );
            new_tree
        });

        // We clean up nodes that are empty and unmanaged.
        // These represent intermediate directories that already existed, so we
        // are not responsible for cleaning them up (we don't run the delete_action
        // closure on their paths).
        if new_tree.nested.is_empty() {
            if let EtcFileStatus::Managed = new_tree.status {
                if delete_action(&new_tree.path, &new_tree.status) {
                    None
                } else {
                    Some(new_tree)
                }
            } else {
                None
            }
        } else {
            Some(new_tree)
        }
    }

    pub fn update_state<F>(self, other: Self, delete_action: &F) -> Option<Self>
    where
        F: Fn(&Path, &EtcFileStatus) -> bool,
    {
        let to_deactivate = other
            .nested
            .clone()
            .relative_complement(self.nested.clone());
        let to_merge = other.nested.intersection(self.nested.clone());

        let deactivated = to_deactivate
            .into_iter()
            .fold(self, |mut new_tree, (name, subtree)| {
                subtree
                    .deactivate(delete_action)
                    .into_iter()
                    .for_each(|subtree| {
                        new_tree.nested.insert(name.to_owned(), subtree);
                    });
                new_tree
            });

        let merged = to_merge
            .into_iter()
            .fold(deactivated, |mut new_tree, (name, other_tree)| {
                new_tree.nested = new_tree.nested.alter(
                    |subtree| {
                        subtree.and_then(|subtree| {
                            subtree.update_state(other_tree.clone(), delete_action).map(
                                |mut new_tree| {
                                    new_tree.status = new_tree.status.merge(&other_tree.status);
                                    new_tree
                                },
                            )
                        })
                    },
                    name,
                );
                new_tree
            });

        // If our invariants are properly maintained, then we should never end up
        // here with dangling unmanaged nodes.
        debug_assert!(!merged.nested.is_empty() || merged.status == EtcFileStatus::Managed);

        Some(merged)
    }

    pub fn write_to_file(&self, state_file: &Path) -> Result<()> {
        log::info!("Writing state info into file: {}", state_file.display());
        let writer = io::BufWriter::new(fs::File::create(state_file)?);

        serde_json::to_writer(writer, self)?;
        Ok(())
    }

    pub fn from_file(state_file: &Path) -> Result<Self> {
        if state_file.is_file() {
            log::info!("Reading state info from {}", state_file.display());
            let reader = io::BufReader::new(fs::File::open(state_file)?);
            match serde_json::from_reader(reader) {
                Ok(created_files) => return Ok(created_files),
                Err(e) => {
                    log::error!("Error reading the state file, ignoring.");
                    log::error!("{:?}", e);
                }
            }
        }
        Ok(Self::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use itertools::Itertools;

    impl EtcTree {
        pub fn deactivate_managed_entry<F>(self, path: &Path, delete_action: &F) -> Self
        where
            F: Fn(&Path, &EtcFileStatus) -> bool,
        {
            fn go<'a, C, F>(
                mut tree: EtcTree,
                path: PathBuf,
                mut components: Peekable<C>,
                delete_action: &F,
            ) -> EtcTree
            where
                C: Iterator<Item = path::Component<'a>>,
                F: Fn(&Path, &EtcFileStatus) -> bool,
            {
                log::debug!("Deactivating {}", path.display());

                if let Some(component) = components.next() {
                    match component {
                        path::Component::Normal(name) => {
                            let new_path = path.join(name);
                            tree.nested = tree.nested.alter(
                                |maybe_subtree| {
                                    maybe_subtree.and_then(|subtree| {
                                        if components.peek().is_some() {
                                            Some(go(subtree, new_path, components, delete_action))
                                        } else {
                                            subtree.deactivate(delete_action)
                                        }
                                    })
                                },
                                name.to_string_lossy().to_string(),
                            );
                            tree
                        }
                        path::Component::RootDir => go(
                            tree,
                            path.join(path::MAIN_SEPARATOR.to_string()),
                            components,
                            delete_action,
                        ),
                        _ => panic!(
                            "Unsupported path provided! At path component: {:?}",
                            component
                        ),
                    }
                } else {
                    tree
                }
            }
            go(
                self,
                PathBuf::new(),
                path.components().peekable(),
                delete_action,
            )
        }
    }

    #[test]
    fn etc_tree_get_status() {
        let tree1 = EtcTree::root_node()
            .register_managed_entry(&PathBuf::from("/").join("foo").join("bar"))
            .register_managed_entry(&PathBuf::from("/").join("foo2"))
            .register_managed_entry(&PathBuf::from("/").join("foo2").join("baz"))
            .register_managed_entry(&PathBuf::from("/").join("foo2").join("baz").join("bar"))
            .register_managed_entry(&PathBuf::from("/").join("foo2").join("baz2"))
            .register_managed_entry(&PathBuf::from("/").join("foo2").join("baz2").join("bar"))
            .register_managed_entry(&PathBuf::from("/").join("foo3").join("baz2").join("bar"))
            .register_managed_entry(&PathBuf::from("/").join("foo4"))
            .register_managed_entry(&PathBuf::from("/").join("foo4").join("baz"))
            .register_managed_entry(&PathBuf::from("/").join("foo4").join("baz").join("bar"))
            .register_managed_entry(&PathBuf::from("/").join("foo5"))
            .register_managed_entry(&PathBuf::from("/").join("foo5").join("baz"))
            .register_managed_entry(&PathBuf::from("/").join("foo5").join("baz2"))
            .register_managed_entry(&PathBuf::from("/").join("foo5").join("baz").join("bar"));

        assert!(tree1.is_managed(&PathBuf::from("/").join("foo5").join("baz").join("bar")));
        assert!(!tree1.is_managed(&PathBuf::from("/").join("foo")));
        assert!(!tree1.is_managed(&PathBuf::from("/").join("foo").join("nonexistent")));
    }

    #[test]
    fn etc_tree_register() {
        let tree = EtcTree::root_node()
            .register_managed_entry(&PathBuf::from("/").join("foo").join("bar"))
            .register_managed_entry(&PathBuf::from("/").join("foo2").join("baz").join("bar"))
            .register_managed_entry(&PathBuf::from("/").join("foo2").join("baz2").join("bar"));
        dbg!(&tree);
        assert_eq!(
            tree.nested.keys().sorted().collect::<Vec<_>>(),
            ["foo", "foo2"]
        );
        assert_eq!(
            tree.nested
                .get("foo2")
                .unwrap()
                .nested
                .get("baz")
                .unwrap()
                .nested
                .get("bar")
                .unwrap()
                .path,
            PathBuf::from("/").join("foo2").join("baz").join("bar")
        );
    }

    #[test]
    fn etc_tree_deactivate() {
        let tree1 = EtcTree::root_node()
            .register_managed_entry(&PathBuf::from("/").join("foo").join("bar"))
            .register_managed_entry(&PathBuf::from("/").join("foo2"))
            .register_managed_entry(&PathBuf::from("/").join("foo2").join("baz"))
            .register_managed_entry(&PathBuf::from("/").join("foo2").join("baz").join("bar"))
            .register_managed_entry(&PathBuf::from("/").join("foo2").join("baz2"))
            .register_managed_entry(&PathBuf::from("/").join("foo2").join("baz2").join("bar"))
            .register_managed_entry(&PathBuf::from("/").join("foo3").join("baz2").join("bar"))
            .register_managed_entry(&PathBuf::from("/").join("foo4"))
            .register_managed_entry(&PathBuf::from("/").join("foo4").join("baz"))
            .register_managed_entry(&PathBuf::from("/").join("foo4").join("baz").join("bar"))
            .register_managed_entry(&PathBuf::from("/").join("foo5"))
            .register_managed_entry(&PathBuf::from("/").join("foo5").join("baz"))
            .register_managed_entry(&PathBuf::from("/").join("foo5").join("baz2"))
            .register_managed_entry(&PathBuf::from("/").join("foo5").join("baz").join("bar"));
        let tree2 = tree1
            .clone()
            .deactivate_managed_entry(&PathBuf::from("/").join("foo4"), &|path, _status| {
                println!("Deactivating: {}", path.display());
                false
            })
            .deactivate_managed_entry(&PathBuf::from("/").join("foo2"), &|path, _status| {
                println!("Deactivating: {}", path.display());
                true
            })
            .deactivate_managed_entry(&PathBuf::from("/").join("foo3"), &|path, _status| {
                println!("Deactivating: {}", path.display());
                true
            })
            .deactivate_managed_entry(
                &PathBuf::from("/").join("foo5").join("baz"),
                &|path, _status| {
                    println!("Deactivating: {}", path.display());
                    true
                },
            );
        dbg!(&tree1);
        assert_eq!(
            tree2.nested.keys().sorted().collect::<Vec<_>>(),
            ["foo", "foo4", "foo5"]
        );
        assert!(tree2
            .nested
            .get("foo5")
            .unwrap()
            .nested
            .get("baz2")
            .unwrap()
            .nested
            .keys()
            .sorted()
            .collect::<Vec<_>>()
            .is_empty());
        assert_eq!(
            tree1.nested.keys().sorted().collect::<Vec<_>>(),
            ["foo", "foo2", "foo3", "foo4", "foo5"]
        );
    }

    #[test]
    fn etc_tree_update_state() {
        let tree1 = EtcTree::root_node()
            .register_managed_entry(&PathBuf::from("/").join("foo").join("bar"))
            .register_managed_entry(&PathBuf::from("/").join("foo2"))
            .register_managed_entry(&PathBuf::from("/").join("foo2").join("baz"))
            .register_managed_entry(&PathBuf::from("/").join("foo2").join("baz").join("bar"))
            .register_managed_entry(&PathBuf::from("/").join("foo2").join("baz2"))
            .register_managed_entry(&PathBuf::from("/").join("foo2").join("baz2").join("bar"))
            .register_managed_entry(&PathBuf::from("/").join("foo3").join("baz2").join("bar"));
        let tree2 = EtcTree::root_node()
            .register_managed_entry(&PathBuf::from("/").join("foo").join("bar"))
            .register_managed_entry(&PathBuf::from("/").join("foo3").join("bar"))
            .register_managed_entry(&PathBuf::from("/").join("foo4"))
            .register_managed_entry(&PathBuf::from("/").join("foo4").join("bar"))
            .register_managed_entry(&PathBuf::from("/").join("foo5"))
            .register_managed_entry(&PathBuf::from("/").join("foo5").join("bar"));
        let new_tree = tree1.update_state(tree2, &|path, _status| {
            println!("Deactivating path: {}", path.display());
            *path != PathBuf::from("/").join("foo5").join("bar")
        });
        assert_eq!(
            new_tree.unwrap().nested.keys().sorted().collect::<Vec<_>>(),
            ["foo", "foo2", "foo3", "foo5"]
        );
    }
}
