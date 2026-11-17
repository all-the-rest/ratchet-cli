//! The filesystem helpers that the gates share.
//!
//! Three things live here, because two gates need them and a copy of a copy
//! is a copy that drifts:
//!
//! * the **temporary directory** that a command writes into — it is removed
//!   again when it goes out of scope, on every path;
//! * the **atomic write**: the content goes into a temporary file in the same
//!   directory and is then **renamed** over the target — a rename replaces a
//!   symbolic link instead of writing through it, and it does not need write
//!   permission on the target itself;
//! * the **mode** of a file, on the platforms that have one.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A temporary directory that is removed again when it goes out of scope.
///
/// The guard exists so that **every** path removes it: a successful run, a
/// failing command, and every early return in between.  A resource that grows
/// without bound is a defect even outside the repository.
#[derive(Debug)]
pub struct Temporary {
    /// The path of the directory.
    path: PathBuf,
}

impl Temporary {
    /// Creates a temporary directory named after the `prefix`.
    pub fn new(prefix: &str) -> io::Result<Self> {
        let path = temporary_directory(prefix);
        fs::create_dir_all(&path)?;
        Ok(Temporary { path })
    }

    /// The path of the directory.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// A unique temporary directory, named after the `prefix`.
fn temporary_directory(prefix: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    std::env::temp_dir().join(format!("ratchet-{prefix}-{}-{nanos}", std::process::id()))
}

/// Writes `content` to `path` through a temporary file in the same directory.
///
/// The temporary file carries the given mode and time and is then **renamed**
/// over the target: a rename replaces a symbolic link instead of writing
/// through it, and it does not need write permission on the target itself.
pub fn write_atomic(
    path: &Path,
    content: &[u8],
    mode: Option<u32>,
    modified: Option<SystemTime>,
) -> io::Result<()> {
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    let temporary = directory.join(temporary_name(path));

    let result = (|| -> io::Result<()> {
        fs::write(&temporary, content)?;
        if let Some(mode) = mode {
            set_mode(&temporary, mode)?;
        }
        if let Some(modified) = modified {
            // The time is restored on a best-effort basis: the bytes and the
            // mode are what the tree owes, and a filesystem that refuses the
            // time must not fail the restore.
            let _ = set_modified(&temporary, modified);
        }
        fs::rename(&temporary, path)?;
        Ok(())
    })();

    if result.is_err() {
        // The temporary file must not stay behind: it would be a change of
        // the tree that nobody asked for.
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// The name of the temporary file of a write.
///
/// The name carries the process id and a counter: two writes in the same run
/// never collide, and the file is gone before the run ends.
fn temporary_name(path: &Path) -> String {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let number = COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    format!(".ratchet-restore-{}-{number}-{name}", std::process::id())
}

/// Sets the time the file was written last.
pub fn set_modified(path: &Path, time: SystemTime) -> io::Result<()> {
    let file = fs::File::open(path)?;
    let times = fs::FileTimes::new().set_modified(time);
    file.set_times(times)
}

/// A time far enough in the past that every write lands after it.
///
/// The engine touches a file with this time before a command runs: a command
/// that writes the file sets a current time, and the difference between the
/// two tells the engine that the file was written — on every filesystem,
/// whatever its timestamp granularity is.
pub fn marker_time() -> SystemTime {
    SystemTime::now() - Duration::from_secs(3600)
}

/// The inode of a file, on the platforms that have one.
///
/// The inode survives a preserved time: a command that **replaces** the file
/// (`cp -p` and `mv`, the reproducible-build pattern) leaves a new one behind,
/// and that is the only signal that such a write happened.
#[cfg(unix)]
pub fn inode_of(metadata: &fs::Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(metadata.ino())
}

/// The inode of a file, on the platforms that have none.
#[cfg(not(unix))]
pub fn inode_of(_metadata: &fs::Metadata) -> Option<u64> {
    None
}

/// The directories that do not exist yet, from `directory` up to the first one
/// that exists.
///
/// The list is ordered from the deepest directory to the shallowest one: it is
/// the order in which the directories have to be **removed** — an empty parent
/// can only be removed after its child is gone.
pub fn missing_directories(directory: &Path) -> Vec<PathBuf> {
    let mut missing = Vec::new();
    let mut current = Some(directory);
    while let Some(candidate) = current {
        if candidate.as_os_str().is_empty() {
            break;
        }
        if !candidate.exists() {
            missing.push(candidate.to_path_buf());
        } else {
            break;
        }
        current = candidate.parent();
    }
    missing
}

/// A directory that could not be created, including any directories that
/// could not be removed while rolling back the attempt.
#[derive(Debug)]
pub struct DirectoryCreationError {
    /// The directory whose creation failed.
    pub directory: PathBuf,
    /// The cause of the creation failure.
    pub cause: io::Error,
    /// Directories created during this attempt that rollback could not remove.
    pub rollback: Vec<(PathBuf, io::Error)>,
}

impl DirectoryCreationError {
    /// A deterministic, caller-formatted description of any changes rollback
    /// could not undo.
    pub fn rollback_message(&self, display: impl Fn(&Path) -> String) -> String {
        if self.rollback.is_empty() {
            return String::new();
        }

        let failures = self
            .rollback
            .iter()
            .map(|(directory, error)| format!("{}: {error}", display(directory)))
            .collect::<Vec<_>>()
            .join(", ");
        format!("; rollback could not remove {failures} — the working tree was modified")
    }
}

/// Creates a directory and its missing parents one at a time.
///
/// Only directories successfully created by this attempt are returned, in
/// deepest-first order so they can be removed safely. If any later creation
/// fails, the new directories are removed in that same order. A rollback
/// failure is retained for the caller to report.
pub fn create_directories(directory: &Path) -> Result<Vec<PathBuf>, DirectoryCreationError> {
    create_directories_with(directory, |path| fs::create_dir(path))
}

fn create_directories_with<F>(
    directory: &Path,
    mut create: F,
) -> Result<Vec<PathBuf>, DirectoryCreationError>
where
    F: FnMut(&Path) -> io::Result<()>,
{
    let mut created = Vec::new();
    for candidate in missing_directories(directory).into_iter().rev() {
        if let Err(cause) = create(&candidate) {
            let rollback = rollback_directories(&created);
            return Err(DirectoryCreationError {
                directory: candidate,
                cause,
                rollback,
            });
        }
        created.push(candidate);
    }

    created.reverse();
    Ok(created)
}

fn rollback_directories(created: &[PathBuf]) -> Vec<(PathBuf, io::Error)> {
    let mut failures = Vec::new();
    for directory in created.iter().rev() {
        if let Err(error) = fs::remove_dir(directory) {
            if error.kind() != io::ErrorKind::NotFound {
                failures.push((directory.clone(), error));
            }
        }
    }
    failures
}

/// Removes the directories that are empty.
///
/// A directory that still holds something was not left behind by the command
/// alone — it is not the engine's to remove.
pub fn remove_empty_directories(directories: &[PathBuf]) -> Vec<(PathBuf, io::Error)> {
    let mut failures = Vec::new();
    for directory in directories {
        match fs::read_dir(directory) {
            Ok(mut entries) => match entries.next() {
                None => {
                    if let Err(error) = fs::remove_dir(directory) {
                        if error.kind() != io::ErrorKind::NotFound {
                            failures.push((directory.clone(), error));
                        }
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(error)) => failures.push((directory.clone(), error)),
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => failures.push((directory.clone(), error)),
        }
    }
    failures
}

/// The mode of a file, on the platforms that have one.
#[cfg(unix)]
pub fn mode_of(metadata: &fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(metadata.permissions().mode() & 0o7777)
}

/// The mode of a file, on the platforms that have none.
#[cfg(not(unix))]
pub fn mode_of(_metadata: &fs::Metadata) -> Option<u32> {
    None
}

/// Sets the mode of a file, on the platforms that have one.
#[cfg(unix)]
pub fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

/// Sets the mode of a file, on the platforms that have none.
#[cfg(not(unix))]
pub fn set_mode(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn a_failed_directory_chain_rolls_back_every_directory_it_created() {
        let root = crate::test_tempdir::TempDir::new().expect("a temporary directory");
        let too_long = "x".repeat(300);
        let target = root.path().join("ok").join(too_long);

        let error = create_directories(&target).expect_err("the component exceeds NAME_MAX");

        assert!(error.rollback.is_empty(), "rollback succeeded: {error:?}");
        assert!(
            !root.path().join("ok").exists(),
            "the earlier directory was left behind"
        );
    }

    #[test]
    fn a_failed_rollback_names_the_directory_left_behind() {
        let root = crate::test_tempdir::TempDir::new().expect("a temporary directory");
        let target = root.path().join("ok").join("later");
        let mut calls = 0;

        let error = create_directories_with(&target, |directory| {
            calls += 1;
            if calls == 1 {
                fs::create_dir(directory)?;
                fs::write(directory.join("kept.txt"), "external change")?;
                Ok(())
            } else {
                Err(io::Error::other("injected creation failure"))
            }
        })
        .expect_err("the second creation fails");

        assert_eq!(error.directory, target);
        assert_eq!(error.rollback.len(), 1);
        assert_eq!(error.rollback[0].0, root.path().join("ok"));
        let message = error.rollback_message(|path| {
            path.strip_prefix(root.path())
                .expect("the path is under the temp root")
                .display()
                .to_string()
        });
        assert!(message.contains("ok"), "{message}");
        assert!(message.contains("working tree was modified"), "{message}");
        assert!(root.path().join("ok/kept.txt").exists());
    }
}
