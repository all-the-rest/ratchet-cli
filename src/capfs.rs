//! Filesystem operations rooted at an open repository directory capability.
//!
//! Paths passed to this module are repository-relative and resolved paths are
//! normalized before they arrive here. The `Dir` remains open across child
//! commands, so a changed ancestor can neither redirect an operation outside
//! the repository nor replace the root capability itself.

use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::SystemTime;

use cap_std::ambient_authority;
use cap_std::fs::{Dir, Metadata, OpenOptions};

/// An open capability for one canonical repository root.
pub struct RootDir {
    path: PathBuf,
    dir: Dir,
}

impl RootDir {
    /// Opens the canonical root with the ambient authority needed only for this
    /// initial operation.
    pub fn open(path: &Path) -> io::Result<Self> {
        let path = fs::canonicalize(path)?;
        let dir = Dir::open_ambient_dir(&path, ambient_authority())?;
        Ok(Self { path, dir })
    }

    /// The canonical path used for diagnostics and translating validated paths.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Converts an already resolved absolute path into a clean path under this
    /// root. `.` names the root itself.
    pub fn relative(&self, absolute: &Path) -> io::Result<PathBuf> {
        let relative = absolute.strip_prefix(&self.path).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "the resolved path is outside the repository root",
            )
        })?;
        let mut clean = PathBuf::new();
        for component in relative.components() {
            match component {
                Component::Normal(name) => clean.push(name),
                Component::CurDir => {}
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "the resolved path is not normalized",
                    ));
                }
            }
        }
        if clean.as_os_str().is_empty() {
            clean.push(".");
        }
        Ok(clean)
    }

    /// Queries metadata relative to the root, following the final link.
    pub fn metadata(&self, path: &Path) -> io::Result<Metadata> {
        self.dir.metadata(path)
    }

    /// Queries metadata relative to the root without following the final link.
    pub fn symlink_metadata(&self, path: &Path) -> io::Result<Metadata> {
        self.dir.symlink_metadata(path)
    }

    /// Reads a file through a capability-opened handle.
    pub fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.dir.read(path)
    }

    /// Opens a file through this capability and returns its already-rooted OS
    /// handle for handle-relative reads, metadata, and timestamp operations.
    pub fn open_file(&self, path: &Path) -> io::Result<fs::File> {
        self.dir.open(path).map(|file| file.into_std())
    }

    /// Reads the contents of a link without rejecting an absolute link target.
    /// Callers must resolve the target under the repository root separately
    /// before operating on the target itself.
    pub fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
        self.dir.read_link_contents(path)
    }

    /// Creates one empty directory and retains its identity/pinning handles.
    pub fn create_tracked_dir(&self, path: &Path) -> io::Result<TrackedDir> {
        self.dir.create_dir(path)?;
        self.track_dir(path)
    }

    /// Opens and tracks an existing directory. Used only for identity checks
    /// in tests and for capturing a directory immediately after creation.
    pub fn track_dir(&self, path: &Path) -> io::Result<TrackedDir> {
        #[cfg(windows)]
        {
            let pin = self.pin_directory(path)?;
            if !pin.metadata()?.is_dir() {
                return Err(io::Error::other(
                    "the created path is no longer a directory",
                ));
            }
            let metadata = self.dir.symlink_metadata(path)?;
            if !metadata.is_dir() {
                return Err(io::Error::other(
                    "the created path is no longer a directory",
                ));
            }
            Ok(TrackedDir {
                path: path.to_path_buf(),
                pin: Some(pin),
            })
        }
        #[cfg(not(windows))]
        {
            let handles = self.open_directory_chain(path)?;
            let handle = handles.last().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "the repository root is not a child",
                )
            })?;
            let metadata = handle.dir_metadata()?;
            if !metadata.is_dir() {
                return Err(io::Error::other(
                    "the created path is no longer a directory",
                ));
            }
            let identity = DirectoryIdentity::capture(&metadata)?;
            Ok(TrackedDir {
                path: path.to_path_buf(),
                identity,
                handles,
            })
        }
    }

    /// Creates an empty directory through the root capability.
    pub fn create_dir(&self, path: &Path) -> io::Result<()> {
        self.dir.create_dir(path)
    }

    /// Creates a directory chain one component at a time. On failure, newly
    /// created empty directories are rolled back through this same root.
    pub fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        let mut prefix = PathBuf::new();
        let mut created = Vec::new();
        for component in path.components() {
            match component {
                Component::Normal(name) => prefix.push(name),
                Component::CurDir => continue,
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "directory paths must stay relative to the repository root",
                    ));
                }
            }
            match self.dir.create_dir(&prefix) {
                Ok(()) => created.push(prefix.clone()),
                Err(error)
                    if error.kind() == io::ErrorKind::AlreadyExists
                        && self
                            .dir
                            .metadata(&prefix)
                            .is_ok_and(|metadata| metadata.is_dir()) =>
                {
                    // A directory already exists; it is not ours to remove.
                }
                Err(error) => {
                    for directory in created.iter().rev() {
                        let _ = self.dir.remove_dir(directory);
                    }
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    /// Writes a file atomically inside its capability-opened parent directory.
    pub fn write_atomic(
        &self,
        target: &Path,
        content: &[u8],
        mode: Option<u32>,
        modified: Option<SystemTime>,
    ) -> io::Result<()> {
        let parent_path = target.parent().unwrap_or_else(|| Path::new("."));
        let name = target.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "the target has no file name")
        })?;
        let parent = self.dir.open_dir(parent_path)?;
        let temporary = temporary_name(name);

        let result = (|| {
            let mut file = parent
                .open_with(&temporary, OpenOptions::new().write(true).create_new(true))?
                .into_std();
            file.write_all(content)?;
            if let Some(mode) = mode {
                set_mode(&file, mode)?;
            }
            if let Some(modified) = modified {
                let _ = file.set_times(fs::FileTimes::new().set_modified(modified));
            }
            drop(file);
            parent.rename(&temporary, &parent, name)?;
            Ok(())
        })();

        if result.is_err() {
            let _ = parent.remove_file(&temporary);
        }
        result
    }

    /// Writes bytes directly through a root-relative capability.
    pub fn write(&self, path: &Path, content: &[u8]) -> io::Result<()> {
        self.dir.write(path, content)
    }

    /// Sets the time through a file handle opened relative to the root.
    pub fn set_modified(&self, path: &Path, time: SystemTime) -> io::Result<()> {
        self.open_file(path)?
            .set_times(fs::FileTimes::new().set_modified(time))
    }

    /// Sets file mode through an already capability-opened file handle.
    pub fn set_mode(&self, path: &Path, mode: u32) -> io::Result<()> {
        let file = self.open_file(path)?;
        set_mode(&file, mode)
    }

    /// Removes a file or link using the root capability.
    pub fn remove_file(&self, path: &Path) -> io::Result<()> {
        match self.dir.symlink_metadata(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
            Ok(metadata) if metadata.is_dir() => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the target is a directory",
            )),
            Ok(_) => self.dir.remove_file(path),
        }
    }

    /// Creates a symlink entry relative to the root, preserving its raw target
    /// where cap-std permits that spelling. On Windows, cap-std rejects an
    /// absolute target even when it resolves inside the repository.
    pub fn create_symlink(&self, raw: &Path, target: &Path) -> io::Result<()> {
        #[cfg(windows)]
        {
            self.dir.symlink_file(raw, target)
        }
        #[cfg(not(windows))]
        {
            self.dir.symlink_contents(raw, target)
        }
    }

    /// Reads a directory relative to the root.
    pub fn read_dir(&self, path: &Path) -> io::Result<cap_std::fs::ReadDir> {
        self.dir.read_dir(path)
    }

    /// Removes an empty directory through the root capability.
    pub fn remove_dir(&self, path: &Path) -> io::Result<()> {
        self.dir.remove_dir(path)
    }

    /// Rechecks the captured directory and removes it if empty.
    ///
    /// On Unix the captured handles are dropped before the root-relative
    /// `remove_dir`. On Windows the retained pin holds a handle with the
    /// operating system's default share mode, so it never blocks the removal;
    /// it is released once the directory is gone so Windows completes the
    /// deferred deletion and an ancestor can be removed in turn. Before each
    /// removal attempt the pin's object is compared with the object the path
    /// names now, so a replacement at the tracked name is refused. A
    /// replacement in the final check-to-operation interval remains the
    /// documented narrow race.
    pub fn remove_tracked_dir(&self, directory: &mut TrackedDir) -> io::Result<bool> {
        if !self.verify_tracked_dir(directory)? {
            return Ok(false);
        }
        #[cfg(windows)]
        let empty = {
            let mut entries = self.dir.read_dir(&directory.path)?;
            match entries.next() {
                None => true,
                Some(Ok(_)) => false,
                Some(Err(error)) => return Err(error),
            }
        };
        #[cfg(not(windows))]
        let empty = {
            let mut entries = directory
                .handles
                .last()
                .ok_or_else(|| io::Error::other("the tracked directory handle is missing"))?
                .entries()?;
            match entries.next() {
                None => true,
                Some(Ok(_)) => false,
                Some(Err(error)) => return Err(error),
            }
        };
        if !empty {
            return Ok(false);
        }
        if !self.verify_tracked_dir(directory)? {
            return Ok(false);
        }

        #[cfg(windows)]
        let removed = {
            let removed = self.dir.remove_dir(&directory.path);
            if removed.is_ok() {
                // The pin opens with delete sharing, so it never blocked the
                // removal. Releasing it now lets Windows finish the deferred
                // deletion.
                drop(directory.pin.take());
            }
            removed
        };
        #[cfg(not(windows))]
        let removed = {
            directory.handles.clear();
            self.dir.remove_dir(&directory.path)
        };

        match removed {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Verifies that the path still resolves to the tracked directory.
    pub fn verify_tracked_dir(&self, directory: &TrackedDir) -> io::Result<bool> {
        let metadata = match self.dir.symlink_metadata(&directory.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        if !metadata.is_dir() {
            return Err(io::Error::other(
                "the created path is no longer a directory; refusing cleanup",
            ));
        }
        #[cfg(windows)]
        {
            // The pin no longer denies rename or delete, so the object it
            // refers to is compared with the object the path names now. Both
            // metadata records are read live, so legitimate changes to the
            // directory's contents cannot make them differ.
            let Some(pin) = directory.pin.as_ref() else {
                return Ok(false);
            };
            if !same_windows_directory(&pin.metadata()?, &metadata) {
                return Err(io::Error::other(
                    "the directory identity changed; refusing cleanup",
                ));
            }
            Ok(true)
        }
        #[cfg(not(windows))]
        {
            if !directory.identity.matches(&metadata)? {
                return Err(io::Error::other(
                    "the directory identity changed; refusing cleanup",
                ));
            }
            Ok(true)
        }
    }

    /// Opens a Windows pin handle that keeps the operating system's default
    /// share mode (read, write and delete).
    ///
    /// The handle is opened relative to the retained root capability, but with
    /// no access rights and without cap-std's directory flag. cap-std strips
    /// `FILE_SHARE_DELETE` from directory opens, and such a handle would make a
    /// generator's own rename or removal of the directory fail. Keeping delete
    /// sharing means the pin never blocks cleanup either; it only keeps the
    /// tracked object alive so the identity check can compare it with whatever
    /// the path names now. The pin never reads or writes through the name.
    #[cfg(windows)]
    fn pin_directory(&self, path: &Path) -> io::Result<fs::File> {
        use cap_std::fs::OpenOptionsExt;

        // FILE_FLAG_BACKUP_SEMANTICS permits opening a directory handle.
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;

        self.dir
            .open_with(
                path,
                OpenOptions::new()
                    .access_mode(0)
                    .custom_flags(FILE_FLAG_BACKUP_SEMANTICS),
            )
            .map(|file| file.into_std())
    }

    #[cfg(not(windows))]
    fn open_directory_chain(&self, path: &Path) -> io::Result<Vec<Dir>> {
        let mut handles = vec![self.dir.open_dir(".")?];
        for component in path.components() {
            match component {
                Component::Normal(name) => {
                    let next = handles
                        .last()
                        .expect("the root handle is retained")
                        .open_dir(name)?;
                    handles.push(next);
                }
                Component::CurDir => {}
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "tracked directory paths must be normalized and root-relative",
                    ));
                }
            }
        }
        if handles.len() == 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the repository root itself is not a tracked child directory",
            ));
        }
        Ok(handles)
    }
}

/// Identity and retained handles for an engine-created directory.
#[derive(Debug)]
pub struct TrackedDir {
    path: PathBuf,
    // On Unix every component stays open as a capability, so lookups and the
    // final emptiness check are issued relative to the tracked parent. Identity
    // is the device and inode of the leaf, so a rename does not confuse it.
    #[cfg(not(windows))]
    identity: DirectoryIdentity,
    #[cfg(not(windows))]
    handles: Vec<Dir>,
    // On Windows a handle keeps the object alive until cleanup, and its
    // metadata is compared with the path's metadata to detect a replacement.
    // The handle keeps the operating system's default share mode (read, write
    // and delete), so a generator can still rename or remove the directory it
    // created.
    #[cfg(windows)]
    pin: Option<fs::File>,
}

impl TrackedDir {
    /// The repository-relative name of this directory.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Compares the tracked object's live metadata with the metadata the path
/// resolves to now.
///
/// Creation time alone is not enough: Windows file-system tunneling restores
/// it when a name is deleted and recreated shortly afterwards. The last write
/// time is not subject to tunneling, and both records describe the same live
/// object unless the path was replaced.
#[cfg(windows)]
fn same_windows_directory(pin: &fs::Metadata, current: &Metadata) -> bool {
    use cap_std::fs::MetadataExt as CapMetadataExt;
    use std::os::windows::fs::MetadataExt as StdMetadataExt;

    pin.file_attributes() == current.file_attributes()
        && pin.creation_time() == current.creation_time()
        && pin.last_write_time() == current.last_write_time()
        && pin.file_size() == current.file_size()
}

/// Identity of an engine-created directory on platforms that track one.
///
/// Windows tracks a retained handle instead (see `TrackedDir::pin`).
#[cfg(not(windows))]
#[derive(Debug, Clone, Copy)]
enum DirectoryIdentity {
    #[cfg(unix)]
    Unix { device: u64, inode: u64 },
}

#[cfg(not(windows))]
impl DirectoryIdentity {
    fn capture(metadata: &Metadata) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use cap_std::fs::MetadataExt;
            Ok(Self::Unix {
                device: metadata.dev(),
                inode: metadata.ino(),
            })
        }
        #[cfg(not(unix))]
        {
            let _ = metadata;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "filesystem directory identity is unavailable on this platform",
            ))
        }
    }

    fn matches(self, metadata: &Metadata) -> io::Result<bool> {
        #[cfg(unix)]
        {
            use cap_std::fs::MetadataExt;
            let Self::Unix { device, inode } = self;
            Ok(metadata.dev() == device && metadata.ino() == inode)
        }
        #[cfg(not(unix))]
        {
            let _ = (self, metadata);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "filesystem directory identity is unavailable on this platform",
            ))
        }
    }
}

fn temporary_name(name: &std::ffi::OsStr) -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let number = COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = name.to_string_lossy();
    PathBuf::from(format!(
        ".ratchet-restore-{}-{number}-{name}",
        std::process::id()
    ))
}

#[cfg(unix)]
fn set_mode(file: &fs::File, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(_file: &fs::File, _mode: u32) -> io::Result<()> {
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn root_relative_file_operations_reject_an_ancestor_redirected_outside() {
        use std::os::unix::fs::symlink;

        let root = crate::test_tempdir::TempDir::new().expect("a temporary root");
        let outside = crate::test_tempdir::TempDir::new().expect("an outside root");
        let root_dir = RootDir::open(root.path()).expect("the root capability opens");
        fs::create_dir(root.path().join("sub")).expect("the initial directory is created");
        let relative = Path::new("sub/file.txt");
        fs::write(root.path().join(relative), "inside").expect("the initial file is written");
        fs::rename(root.path().join("sub"), outside.path().join("moved"))
            .expect("the directory is moved outside");
        symlink(outside.path().join("moved"), root.path().join("sub"))
            .expect("the outside symlink is made");

        assert!(
            root_dir
                .write_atomic(relative, b"changed", None, None)
                .is_err()
        );
        assert_eq!(
            fs::read_to_string(outside.path().join("moved/file.txt"))
                .expect("the outside file is still readable"),
            "inside"
        );
    }

    #[cfg(unix)]
    #[test]
    fn tracked_cleanup_does_not_follow_an_outside_symlink() {
        use std::os::unix::fs::symlink;

        let root = crate::test_tempdir::TempDir::new().expect("a temporary root");
        let outside = crate::test_tempdir::TempDir::new().expect("an outside root");
        let root_dir = RootDir::open(root.path()).expect("the root capability opens");
        let mut tracked = root_dir
            .create_tracked_dir(Path::new("created"))
            .expect("the directory is tracked");
        fs::remove_dir(root.path().join("created")).expect("the directory is removed");
        symlink(outside.path(), root.path().join("created")).expect("the outside link is made");
        fs::create_dir(outside.path().join("keep")).expect("the outside directory is made");

        assert!(root_dir.remove_tracked_dir(&mut tracked).is_err());
        assert!(outside.path().join("keep").is_dir());
    }
}
