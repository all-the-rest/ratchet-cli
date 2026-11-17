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
        #[cfg(windows)]
        let name = path.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "the directory has no final name",
            )
        })?;
        Ok(TrackedDir {
            path: path.to_path_buf(),
            #[cfg(windows)]
            name: name.to_os_string(),
            identity,
            handles,
        })
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

    /// Rechecks the captured directory and removes it if empty. On Unix the
    /// captured handles are dropped before root-relative `remove_dir`. On
    /// Windows only the leaf handle is dropped, and removal is relative to its
    /// retained parent capability; the rest of the chain pins ancestors. A
    /// replacement in the final check-to-operation interval remains the
    /// documented narrow race.
    pub fn remove_tracked_dir(&self, directory: &mut TrackedDir) -> io::Result<bool> {
        if !self.verify_tracked_dir(directory)? {
            return Ok(false);
        }
        let mut entries = directory
            .handles
            .last()
            .ok_or_else(|| io::Error::other("the tracked directory handle is missing"))?
            .entries()?;
        match entries.next() {
            None => {}
            Some(Ok(_)) => return Ok(false),
            Some(Err(error)) => return Err(error),
        }
        if !self.verify_tracked_dir(directory)? {
            return Ok(false);
        }

        #[cfg(windows)]
        let removed = {
            let parent_index = directory.handles.len().checked_sub(2).ok_or_else(|| {
                io::Error::other("the tracked directory has no retained parent capability")
            })?;
            directory.handles.pop();
            directory.handles[parent_index].remove_dir(&directory.name)
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
        #[cfg(windows)]
        let metadata = {
            let parent_index = directory.handles.len().checked_sub(2).ok_or_else(|| {
                io::Error::other("the tracked directory has no retained parent capability")
            })?;
            directory.handles[parent_index].symlink_metadata(&directory.name)
        };
        #[cfg(not(windows))]
        let metadata = self.dir.symlink_metadata(&directory.path);

        let metadata = match metadata {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        if !metadata.is_dir() {
            return Err(io::Error::other(
                "the created path is no longer a directory; refusing cleanup",
            ));
        }
        if !directory.identity.matches(&metadata)? {
            return Err(io::Error::other(
                "the directory identity changed; refusing cleanup",
            ));
        }
        Ok(true)
    }

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

/// Identity and retained directory handles for an engine-created directory.
#[derive(Debug)]
pub struct TrackedDir {
    path: PathBuf,
    #[cfg(windows)]
    name: std::ffi::OsString,
    identity: DirectoryIdentity,
    // On Windows cap-std omits FILE_SHARE_DELETE for directory handles. Keeping
    // every component open pins the lookup chain until final cleanup validation.
    handles: Vec<Dir>,
}

impl TrackedDir {
    /// The repository-relative name of this directory.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[derive(Debug, Clone, Copy)]
enum DirectoryIdentity {
    #[cfg(unix)]
    Unix { device: u64, inode: u64 },
    #[cfg(windows)]
    WindowsPinned,
}

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
        #[cfg(windows)]
        {
            let _ = metadata;
            Ok(Self::WindowsPinned)
        }
        #[cfg(not(any(unix, windows)))]
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
        #[cfg(windows)]
        {
            let _ = (self, metadata);
            // The held capability chain pins each opened directory against
            // rename/replacement until the final root-relative removal.
            Ok(true)
        }
        #[cfg(not(any(unix, windows)))]
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

#[cfg(test)]
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
