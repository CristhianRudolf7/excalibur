//! Metadata-only read authority. The opened descriptor is checked before bytes
//! enter snapshots, search results or output blobs.
use cap_std::fs::{Dir, File};
use std::path::{Path, PathBuf};

#[derive(Default)]
pub(crate) struct ReadAuthority {
    #[cfg(unix)]
    inventory: Option<unix::Inventory>,
}
impl ReadAuthority {
    pub(crate) fn check(
        &mut self,
        directory: &Dir,
        root: &Path,
        protected: &[PathBuf],
        file: &File,
    ) -> Result<(), String> {
        #[cfg(unix)]
        {
            use cap_std::fs::MetadataExt;
            let metadata = file.metadata().map_err(|e| e.to_string())?;
            if !metadata.is_file() {
                return Err("security policy: file reads require a regular file".into());
            }
            if metadata.nlink() <= 1 {
                return Ok(());
            }
            if self.inventory.is_none() {
                self.inventory = Some(unix::Inventory::build(directory, root, protected)?);
            }
            self.inventory.as_ref().unwrap().check(&metadata)
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::Storage::FileSystem::{
                BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
            };
            let _ = (directory, root, protected);
            let mut information: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
            if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut information) } == 0 {
                return Err("security policy: Windows file identity query failed".into());
            }
            match information.nNumberOfLinks {
                1 => Ok(()),
                0 => Err("security policy: Windows file link count is unavailable".into()),
                _ => Err("security policy: multiple-link reads require a native Windows hardlink inventory".into()),
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (directory, root, file);
            if protected.is_empty() {
                Ok(())
            } else {
                Err("security policy: protected reads require a native hardlink identity adapter on this target".into())
            }
        }
    }
}

#[cfg(unix)]
mod unix {
    use cap_std::fs::{Dir, Metadata, MetadataExt};
    use std::{
        collections::{BTreeMap, BTreeSet},
        ffi::{CString, OsString},
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::ffi::OsStrExt,
        },
        path::{Component, Path, PathBuf},
    };
    const MAX_ENTRIES: usize = 100_000;
    const MAX_DIRECTORIES: usize = 4096;
    struct Inode {
        links: u64,
        names: BTreeSet<(u64, u64, OsString)>,
    }
    pub(super) struct Inventory {
        inodes: BTreeMap<(u64, u64), Inode>,
    }
    impl Inventory {
        pub(super) fn build(
            root_dir: &Dir,
            root: &Path,
            protected: &[PathBuf],
        ) -> Result<Self, String> {
            let mut pending = vec![PathBuf::new()];
            let (mut entries, mut directories) = (0, 0);
            let mut inodes: BTreeMap<(u64, u64), Inode> = BTreeMap::new();
            while let Some(relative) = pending.pop() {
                let full = root.join(&relative);
                if protected.iter().any(|path| full.starts_with(path)) {
                    continue;
                }
                directories += 1;
                if directories > MAX_DIRECTORIES {
                    return Err(
                        "security policy: hardlink read inventory exceeds 4096 directories".into(),
                    );
                }
                let dir = open_directory(root_dir, &relative)?;
                let parent = dir.dir_metadata().map_err(|e| e.to_string())?;
                for entry in dir.entries().map_err(|e| e.to_string())? {
                    entries += 1;
                    if entries > MAX_ENTRIES {
                        return Err(
                            "security policy: hardlink read inventory exceeds 100000 entries"
                                .into(),
                        );
                    }
                    let entry = entry.map_err(|e| e.to_string())?;
                    let name = entry.file_name();
                    let path = relative.join(&name);
                    if protected
                        .iter()
                        .any(|excluded| root.join(&path).starts_with(excluded))
                    {
                        continue;
                    }
                    let metadata = dir.symlink_metadata(&name).map_err(|e| e.to_string())?;
                    if metadata.is_dir() {
                        pending.push(path);
                    } else if metadata.is_file() && metadata.nlink() > 1 {
                        let inode = inodes
                            .entry((metadata.dev(), metadata.ino()))
                            .or_insert_with(|| Inode {
                                links: metadata.nlink(),
                                names: BTreeSet::new(),
                            });
                        if inode.links != metadata.nlink() {
                            return Err("security policy: hardlink read inventory changed during inspection".into());
                        }
                        inode.names.insert((parent.dev(), parent.ino(), name));
                    }
                }
            }
            Ok(Self { inodes })
        }
        pub(super) fn check(&self, metadata: &Metadata) -> Result<(), String> {
            let allowed = self
                .inodes
                .get(&(metadata.dev(), metadata.ino()))
                .is_some_and(|inode| {
                    inode.links == metadata.nlink() && inode.links == inode.names.len() as u64
                });
            if allowed {
                Ok(())
            } else {
                Err("security policy: hardlink crosses the permitted file read authority".into())
            }
        }
    }
    fn open_directory(root: &Dir, relative: &Path) -> Result<Dir, String> {
        let mut dir = root.try_clone().map_err(|e| e.to_string())?;
        for component in relative.components() {
            let Component::Normal(name) = component else {
                return Err("security policy: invalid read inventory directory".into());
            };
            let name = CString::new(name.as_bytes()).map_err(|_| "invalid directory name")?;
            let fd = unsafe {
                libc::openat(
                    dir.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(
                    "security policy: read inventory directory changed or is inaccessible".into(),
                );
            }
            dir = Dir::from_std_file(unsafe { std::fs::File::from_raw_fd(fd) });
        }
        Ok(dir)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::ReadAuthority;
    #[test]
    fn cached_inventory_rechecks_the_open_descriptor_link_count() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("public"), b"synthetic").unwrap();
        std::fs::hard_link(root.join("public"), root.join("internal-link")).unwrap();
        let directory =
            cap_std::fs::Dir::open_ambient_dir(&root, cap_std::ambient_authority()).unwrap();
        let file = directory.open("public").unwrap();
        let mut authority = ReadAuthority::default();
        authority.check(&directory, &root, &[], &file).unwrap();
        std::fs::hard_link(root.join("public"), temp.path().join("outside-link")).unwrap();
        assert!(
            authority
                .check(&directory, &root, &[], &file)
                .unwrap_err()
                .contains("hardlink")
        );
    }
}
