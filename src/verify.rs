//! The pre-release check: a command whose output is compared with a committed
//! snapshot (SPEC §15).
//!
//! A **benchmark is a measurement, not a defect class**.  It has no meaningful
//! "baseline that may only sink" — the value moves in both directions and
//! only matters at the moment of a release.  So the pre-release check is **not**
//! a ratchet: it has **no direction**.  It runs in the release pipeline, not
//! in the commit gate: it needs the infrastructure the release needs, and a
//! permanent gate over a measurement would be noise.
//!
//! The mechanic is the drift gate (SPEC §7) with a different comparison: the
//! drift gate compares a *generated file* with its committed counterpart, the
//! pre-release check compares a *command output* with a committed snapshot.
//!
//! The engine never mutates the tree: the command writes to a temporary path
//! and the snapshot is only written by `--update`.  The snapshot itself — the
//! one file the engine owns — is restored after the command, byte-, mode- and
//! time-identical.

use std::fs;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use crate::Error;
use crate::capfs::RootDir;
use crate::config::{Config, Verify};
use crate::diff;
use crate::fsutil;
use crate::git::{Git, HeadEntry};
use crate::path;

/// The placeholder in the argv that names the output path.
///
/// The command tells the engine where to write by carrying the placeholder;
/// the engine replaces it with the temporary output path — **wherever it
/// appears in an argument**, so `--output=<out>` names the output as well.
pub const PLACEHOLDER: &str = "<out>";

/// The status of one pre-release check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyStatus {
    /// The committed snapshot is what the command produces.
    Ok,
    /// The committed snapshot differs from what the command produces.
    Drift,
    /// The snapshot is not committed yet.
    New,
}

impl VerifyStatus {
    /// The name used in the JSON report.
    pub fn as_str(self) -> &'static str {
        match self {
            VerifyStatus::Ok => "ok",
            VerifyStatus::Drift => "drift",
            VerifyStatus::New => "new",
        }
    }

    /// True when the check failed.
    pub fn is_failure(self) -> bool {
        matches!(self, VerifyStatus::Drift)
    }
}

/// The outcome of one pre-release check.
#[derive(Debug, Clone)]
pub struct VerifyEntry {
    /// The name of the check.
    pub name: String,
    /// The status of the check.
    pub status: VerifyStatus,
    /// The unified diff (committed → produced), if the snapshot drifted.
    pub diff: Option<String>,
    /// A human readable explanation, if there is something to say.
    pub detail: Option<String>,
}

/// What a verification run does.
#[derive(Debug)]
pub struct Options<'a> {
    /// The repository root.
    pub root: &'a Path,
    /// Write the produced output into the snapshots.
    pub update: bool,
    /// The one check to run, by name.
    pub name: Option<&'a str>,
}

/// The result of a verification run.
#[derive(Debug, Default)]
pub struct Outcome {
    /// One entry per check, sorted by name.
    pub entries: Vec<VerifyEntry>,
    /// Notes about what was skipped and why.
    pub notes: Vec<String>,
    /// The debug log (`--debug`).
    pub debug: Vec<String>,
}

impl Outcome {
    /// The number of checks whose snapshot differs from the output.
    pub fn drifted(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.status.is_failure())
            .count()
    }
}

/// Runs the pre-release checks of the `verify` entries of the config.
pub fn run(config: &Config, git: &Git, options: &Options) -> Result<Outcome, Error> {
    let mut outcome = Outcome::default();
    let filesystem = RootDir::open(options.root)
        .map_err(|error| Error::new(format!("cannot open the repository root: {error}")))?;

    if !git.is_repo() {
        // The committed comparisons are skipped; the checks still run (SPEC 8).
        outcome
            .notes
            .push("the verify comparison is skipped: not a git repository".to_string());
    }

    // The entries are sorted by name: the report is a stable diff.
    let mut entries: Vec<&Verify> = config.verify.iter().collect();
    entries.sort_by(|left, right| left.name.cmp(&right.name));

    let mut selected = 0;
    for entry in entries {
        // The snapshot lies inside the repository root: a path that points
        // outside it would let the engine write outside the tree it owns.
        let subject = format!("the check {}: the snapshot {}", entry.name, entry.snapshot);
        let resolved =
            path::inside_repository(options.root, &entry.snapshot).map_err(|refusal| {
                Error::new(refusal.message(&subject, "must lie inside the repository root"))
            })?;
        if let Some(filter) = options.name {
            if filter != entry.name {
                continue;
            }
        }
        selected += 1;
        let result = run_one(
            entry,
            &resolved,
            &filesystem,
            git,
            options,
            &mut outcome.debug,
        )?;
        outcome.entries.push(result);
    }

    if let Some(filter) = options.name {
        if selected == 0 {
            return Err(Error::new(format!("no check for {filter}")));
        }
    }

    Ok(outcome)
}

/// Runs one pre-release check.
fn run_one(
    entry: &Verify,
    resolved: &Path,
    filesystem: &RootDir,
    git: &Git,
    options: &Options,
    debug: &mut Vec<String>,
) -> Result<VerifyEntry, Error> {
    let root = options.root;
    let snapshot = root.join(&entry.snapshot);
    let display = path::display(Some(root), &snapshot);
    let resolved_relative = filesystem
        .relative(resolved)
        .map_err(|error| Error::new(format!("the check {}: {error}", entry.name)))?;
    let named_relative = named_target(root, filesystem, &entry.snapshot, &resolved_relative);
    let name = entry.name.as_str();

    // The snapshot has to be a regular file: the committed content of a
    // symbolic link is its target, and comparing the *output* with the
    // target's content would say nothing about the output.  The drift gate
    // resolves a link instead — there the *file* is the subject, here the
    // *output* is (SPEC §15).
    if let Ok(metadata) = filesystem.symlink_metadata(&named_relative) {
        if is_link_or_reparse_point(&metadata) {
            return Err(Error::new(format!(
                "the check {name}: the snapshot {display} is a symbolic link — \
                 the snapshot has to be a regular file"
            )));
        }
        if metadata.is_dir() {
            return Err(Error::new(format!(
                "the check {name}: the snapshot {display} is a directory, not a file"
            )));
        }
    } else if let Ok(metadata) = filesystem.symlink_metadata(&resolved_relative) {
        // The named path can be unwalkable even though its resolved target
        // exists (for example `a/..` when `a` is missing). A directory at the
        // resolved file position is still not a snapshot file.
        if metadata.is_dir() {
            return Err(Error::new(format!(
                "the check {name}: the snapshot {display} is a directory, not a file"
            )));
        }
    }

    // The snapshot is the one file the engine owns: its bytes, its mode and
    // its time are restored after the command, whatever the command did to
    // it.  A command that writes elsewhere in the tree is the command's
    // doing — the engine cannot undo that.
    let subject = format!("the check {name}: the snapshot {}", entry.snapshot);
    let created =
        command_directories(root, filesystem, &entry.snapshot, resolved).map_err(|refusal| {
            Error::new(refusal.message(&subject, "must lie inside the repository root"))
        })?;
    // The recorded candidates are absolute: the guard revalidates them against
    // the canonical root and performs every operation through the capability.
    let created: Vec<PathBuf> = created
        .into_iter()
        .map(|directory| filesystem.path().join(directory))
        .collect();
    let mut guard = Guard::take(resolved, root, entry.snapshot.clone(), created, filesystem)
        .map_err(|error| Error::new(format!("the check {name}: cannot read {display}: {error}")))?;

    // The marker: the file is touched with an old time, so that the engine
    // can tell whether the command wrote it — even when it wrote the same
    // bytes over it.
    guard.mark().map_err(|error| {
        Error::new(format!(
            "the check {name}: cannot prepare the restore of {display} — the working tree may be modified: {error}"
        ))
    })?;

    let produced = produce(entry, root, debug);

    // The restore happens before the error is reported: a command that wrote
    // into the tree must not leave the snapshot behind in a changed state.
    if let Err(error) = guard.restore() {
        return Err(Error::new(format!(
            "the check {name}: the restore of {display} failed — the working tree may be modified: \
             {error}"
        )));
    }

    let produced = produced?;

    // The committed lookup follows the path the system resolves the named
    // snapshot to. A linked directory is a blob in Git, so Git cannot traverse
    // it to find the target file. The resolved path is already contained by
    // `inside_repository`; this only makes it repository-relative. Keep the
    // configured path for user-facing messages and the diff.
    let lookup = path::relative_to(&path::canonical_root(root), resolved)
        .unwrap_or_else(|| entry.snapshot.clone());
    let (status, committed) = compare(git, name, &lookup, &entry.snapshot, &produced)?;

    let mut detail = None;
    let mut diff = None;

    match status {
        VerifyStatus::Ok => {}
        VerifyStatus::New => {
            detail = Some("not committed yet".to_string());
        }
        VerifyStatus::Drift => {
            let committed = committed.unwrap_or_default();
            diff = Some(diff::unified(&entry.snapshot, &committed, &produced));
            detail = Some("differs from the committed snapshot".to_string());
        }
    }

    // `--update` writes the produced output into the snapshot: the snapshot
    // is machine-written, never hand-edited (SPEC §15).  The check stays
    // **failed**: the committed snapshot is still stale until the file is
    // committed — the same rule the drift gate follows (SPEC §7).
    if options.update && guard.write_snapshot(&produced).map_err(|error| {
        Error::new(format!(
            "the check {name}: the update of {display} failed — the working tree may be modified: {error}"
        ))
    })? {
        detail = Some(match status {
            VerifyStatus::Drift => "the snapshot was written to the working tree; commit it — \
                 the committed snapshot is still stale"
                .to_string(),
            VerifyStatus::New => "the snapshot was written to the working tree; it is not \
                 committed yet"
                .to_string(),
            VerifyStatus::Ok => "the snapshot was written to the working tree".to_string(),
        });
    }

    Ok(VerifyEntry {
        name: entry.name.clone(),
        status,
        diff,
        detail,
    })
}

/// The final name of the configured path, with its parent resolved before any
/// command runs. This lets `symlink_metadata` inspect a file link itself while
/// still using a root-relative capability path.
fn named_target(root: &Path, filesystem: &RootDir, relative: &str, resolved: &Path) -> PathBuf {
    let named = Path::new(relative);
    let Some(name) = named.file_name() else {
        return resolved.to_path_buf();
    };
    let parent = named.parent().unwrap_or_else(|| Path::new("."));
    let Ok(parent) = path::inside_repository(root, &parent.to_string_lossy()) else {
        return resolved.to_path_buf();
    };
    let Ok(parent) = filesystem.relative(&parent) else {
        return resolved.to_path_buf();
    };
    parent.join(name)
}

/// Runs the command of a check and returns the bytes it produced.
///
/// Three ways to say where the output goes:
///
/// * with `output_arg` — the argv gets the temporary output path appended:
///   the flag, then the path;
/// * with the `<out>` placeholder in the argv — the placeholder is replaced
///   by the temporary output path, **wherever it appears in an argument**:
///   `--output=<out>` and `<out>.txt` name the output as well;
/// * without either — the **stdout** of the command is the output.
///
/// The command runs with the working directory set to the repository root, and
/// it writes into a temporary directory: the tree is not the engine's to
/// write.
fn produce(entry: &Verify, root: &Path, debug: &mut Vec<String>) -> Result<Vec<u8>, Error> {
    let temporary = fsutil::Temporary::new("verify").map_err(|error| {
        Error::new(format!(
            "the check {} could not be started: {error}",
            entry.name
        ))
    })?;
    let name = path::file_name(Path::new(&entry.snapshot));
    let output = temporary.path().join(name);
    let output_path = output.to_string_lossy().into_owned();

    let mut argv = entry.command.clone();
    // The placeholder is replaced wherever it appears: a command that writes
    // `--output=<out>` has to write to the temporary file, or the engine
    // would compare the command's stdout with the snapshot and report a
    // green that means nothing.
    let placeholder = argv.iter().any(|argument| argument.contains(PLACEHOLDER));
    if placeholder {
        argv = argv
            .iter()
            .map(|argument| argument.replace(PLACEHOLDER, &output_path))
            .collect();
    }
    if let Some(output_arg) = &entry.output_arg {
        argv.push(output_arg.clone());
        argv.push(output_path.clone());
    }

    let (program, arguments) = argv.split_first().ok_or_else(|| {
        Error::new(format!(
            "the check {} could not be started: the command is empty",
            entry.name
        ))
    })?;

    let result = Command::new(program)
        .args(arguments)
        .current_dir(root)
        .output()
        .map_err(|error| {
            Error::with_output(
                format!("the check {} could not be started: {error}", entry.name),
                format!("{}: could not be started\n{error}", argv.join(" ")),
            )
        })?;

    let stdout = String::from_utf8_lossy(&result.stdout);
    let stderr = String::from_utf8_lossy(&result.stderr);
    let status = match result.status.code() {
        Some(code) => format!("status {code}"),
        None => "an unknown status".to_string(),
    };
    // The transcript shows the argv that was really executed — it belongs to
    // the diagnostic channel, never to the report (SPEC 6.2).
    let transcript = format!(
        "{}: {status}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        argv.join(" ")
    );
    debug.push(format!("check {}: {transcript}", entry.name));

    if !result.status.success() {
        return Err(Error::with_output(
            format!("the check {} exited with {status}", entry.name),
            transcript,
        ));
    }

    // The output goes to a file: the command was told where to write it.
    if placeholder || entry.output_arg.is_some() {
        return read_output(entry, temporary.path(), &transcript);
    }

    // Without an output path the stdout of the command is the output.
    Ok(result.stdout)
}

/// Reads the file the command wrote into the temporary directory.
///
/// The command was told where to write: the temporary directory holds the
/// output and nothing else.  A command that names the output with a suffix
/// (`<out>.txt`) writes a file under another name — the directory is the
/// contract, not the file name.
///
/// * nothing in the directory → the command wrote no output;
/// * more than one file → the output is ambiguous, and a guess would be a
///   comparison that means nothing.
fn read_output(entry: &Verify, temporary: &Path, transcript: &str) -> Result<Vec<u8>, Error> {
    let mut files: Vec<PathBuf> = match fs::read_dir(temporary) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.is_file())
            .collect(),
        Err(error) => {
            return Err(Error::with_output(
                format!("the check {} wrote no output", entry.name),
                format!("{transcript}\ncannot read the temporary directory: {error}"),
            ));
        }
    };
    // The files are sorted: the same tree produces the same bytes.
    files.sort();

    match files.len() {
        0 => Err(Error::with_output(
            format!("the check {} wrote no output", entry.name),
            format!("{transcript}\nthe command wrote nothing into the temporary directory"),
        )),
        1 => fs::read(&files[0]).map_err(|error| {
            Error::with_output(
                format!("the check {} wrote no output", entry.name),
                format!("{transcript}\ncannot read the output of the check: {error}"),
            )
        }),
        count => Err(Error::with_output(
            format!(
                "the check {} wrote {count} files — the output is ambiguous",
                entry.name
            ),
            format!(
                "{transcript}\nthe command wrote more than one file into the temporary directory"
            ),
        )),
    }
}

/// Compares the produced output with the committed snapshot.
///
/// * no repository → the comparison is skipped, the snapshot counts as `new`;
/// * the snapshot is not in the committed tree → `new`, informational and not
///   a failure;
/// * the committed path is a directory → an operational error;
/// * the committed snapshot cannot be read → an operational error.  A broken
///   reference must not disable a check (SPEC 8).
///
/// The message names the check **and** the path: whoever reads the error has
/// to know which check failed and which file it is about.
fn compare(
    git: &Git,
    name: &str,
    lookup: &str,
    named: &str,
    produced: &[u8],
) -> Result<(VerifyStatus, Option<Vec<u8>>), Error> {
    if !git.is_repo() {
        return Ok((VerifyStatus::New, None));
    }
    match git.head_entry(lookup) {
        HeadEntry::Missing => Ok((VerifyStatus::New, None)),
        HeadEntry::Directory => Err(Error::new(format!(
            "the check {name}: the committed snapshot {named} is a directory, not a file"
        ))),
        HeadEntry::Unreadable => Err(Error::new(format!(
            "the check {name}: the committed snapshot {named} cannot be read"
        ))),
        HeadEntry::Blob(object) => {
            let committed = git.show_object(&object).ok_or_else(|| {
                Error::new(format!(
                    "the check {name}: the committed snapshot {named} cannot be read"
                ))
            })?;
            if committed == produced {
                Ok((VerifyStatus::Ok, Some(committed)))
            } else {
                Ok((VerifyStatus::Drift, Some(committed)))
            }
        }
    }
}

/// Directories the command could have created along the snapshot's named
/// traversal or at the resolved target. Every intermediate prefix is resolved
/// by `path`, so directories behind links and before a later `..` are included.
/// Paths are ordered deepest-first for removal.
fn command_directories(
    root: &Path,
    filesystem: &RootDir,
    relative: &str,
    resolved: &Path,
) -> Result<Vec<PathBuf>, path::Refusal> {
    let mut directories = Vec::new();
    if let Some(parent) = resolved.parent() {
        if let Ok(parent) = filesystem.relative(parent) {
            directories.extend(missing_directories(filesystem, &parent));
        }
    }

    let components: Vec<_> = Path::new(relative).components().collect();
    let mut prefix = PathBuf::new();
    for (index, component) in components.iter().enumerate() {
        match component {
            Component::Normal(name) => prefix.push(name),
            Component::ParentDir => prefix.push(".."),
            Component::CurDir => prefix.push("."),
            _ => {}
        }

        if index + 1 < components.len() {
            let prefix = prefix.to_string_lossy();
            let directory = path::inside_repository(root, &prefix)?;
            if let Ok(directory) = filesystem.relative(&directory) {
                directories.extend(missing_directories(filesystem, &directory));
            }
        }
    }

    directories.sort_by(|left, right| {
        right
            .components()
            .count()
            .cmp(&left.components().count())
            .then_with(|| left.cmp(right))
    });
    directories.dedup();
    Ok(directories)
}

/// Missing root-relative directories, deepest-first. Metadata is queried only
/// through the retained repository capability.
fn missing_directories(filesystem: &RootDir, directory: &Path) -> Vec<PathBuf> {
    let mut missing = Vec::new();
    let mut current = Some(directory);
    while let Some(candidate) = current {
        if candidate.as_os_str().is_empty() || candidate == Path::new(".") {
            break;
        }
        match filesystem.metadata(candidate) {
            Ok(metadata) if metadata.is_dir() => break,
            Ok(_) => break,
            Err(_) => missing.push(candidate.to_path_buf()),
        }
        current = candidate.parent();
    }
    missing
}

/// The identity captured while the corresponding filesystem object is held
/// open. On Unix it is the device and inode of the retained handle. On Windows
/// the retained handle *is* the identity: cap-std opens it without
/// `FILE_SHARE_DELETE`, so the name cannot be renamed or replaced while the
/// handle is alive. No pathname file ID is used, so the low-resolution file
/// index that can collide on ReFS never enters the decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileIdentity {
    #[cfg(unix)]
    Unix { device: u64, inode: u64 },
    #[cfg(windows)]
    WindowsPinned,
}

impl FileIdentity {
    fn capture(filesystem: &RootDir, path: &Path, handle: &fs::File) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;

            let metadata = handle.metadata()?;
            let handle_identity = Self::Unix {
                device: metadata.dev(),
                inode: metadata.ino(),
            };
            if Self::capture_path(filesystem, path)? != handle_identity {
                return Err(io::Error::other(
                    "the filesystem path changed while its identity was captured",
                ));
            }
            Ok(handle_identity)
        }

        #[cfg(windows)]
        {
            let _ = (filesystem, path);
            reject_reparse_handle(handle)?;
            Ok(Self::WindowsPinned)
        }

        #[cfg(not(any(unix, windows)))]
        {
            let _ = (filesystem, path, handle);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "filesystem identity is unavailable on this platform",
            ))
        }
    }

    /// Captures the identity of the object a path names, without following a
    /// final symbolic link. Only Unix has a pathname identity; Windows relies
    /// on the retained no-delete handle instead.
    #[cfg(unix)]
    fn capture_path(filesystem: &RootDir, path: &Path) -> io::Result<Self> {
        use cap_std::fs::MetadataExt;

        let metadata = filesystem.symlink_metadata(&filesystem.relative(path)?)?;
        Ok(Self::Unix {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
}

/// A directory identity captured while a handle is retained. On Windows the
/// handle pins its name until the cleanup boundary immediately before removal.
struct TrackedDirectory {
    path: PathBuf,
    identity: FileIdentity,
    handle: Option<fs::File>,
}

impl TrackedDirectory {
    fn release_handle(&mut self) {
        // Windows directory handles deny delete sharing. Drop only the
        // candidate being removed; its tracked ancestors stay pinned.
        #[cfg(windows)]
        drop(self.handle.take());
        #[cfg(not(windows))]
        let _ = self;
    }
}

/// The state of the snapshot file: bytes, mode, time and object identity.
#[derive(Debug, Default)]
struct State {
    content: Option<Vec<u8>>,
    mode: Option<u32>,
    modified: Option<SystemTime>,
    identity: Option<FileIdentity>,
}

/// The snapshot guard retains the pre-command object and directory identities.
/// Before a later operation, the enclosing validation recaptures path/object
/// identities and finishes by re-resolving the configured snapshot, candidate,
/// and repository root. These sequential checks cannot bind a name or retained
/// handle atomically to the operation that follows; every engine-owned write,
/// removal and directory operation is therefore issued through the retained
/// root capability rather than through a pathname.
struct Guard<'a> {
    filesystem: &'a RootDir,
    target: PathBuf,
    snapshot_path: String,
    root: PathBuf,
    canonical_root: PathBuf,
    before: State,
    _before_handle: Option<fs::File>,
    marker: Option<SystemTime>,
    missing: Vec<PathBuf>,
    original_directories: Vec<TrackedDirectory>,
    created_directories: Vec<TrackedDirectory>,
}

impl<'a> Guard<'a> {
    /// Takes the snapshot and retains identity handles for the existing path
    /// chain and target object.
    fn take(
        target: &Path,
        root: &Path,
        snapshot_path: String,
        missing: Vec<PathBuf>,
        filesystem: &'a RootDir,
    ) -> io::Result<Self> {
        let canonical_root = filesystem.path().to_path_buf();
        if !target.starts_with(&canonical_root) {
            return Err(io::Error::other(
                "the resolved snapshot is outside the repository root",
            ));
        }
        let original_directories = capture_existing_chain(filesystem, &canonical_root, target)?;
        let mut guard = Guard {
            filesystem,
            target: target.to_path_buf(),
            snapshot_path,
            root: root.to_path_buf(),
            canonical_root,
            before: State::default(),
            _before_handle: None,
            marker: None,
            missing,
            original_directories,
            created_directories: Vec::new(),
        };
        let (before, handle) = guard.read_state_before_command()?;
        guard.before = before;
        guard._before_handle = handle;
        Ok(guard)
    }

    /// The clean, repository-relative path the capability operates on.
    fn relative(&self, path: &Path) -> io::Result<PathBuf> {
        self.filesystem.relative(path)
    }

    /// Touches the original file with an old time so same-byte writes remain
    /// detectable. Failure to set a timestamp is best effort, as before, but
    /// a changed path or object fails closed.
    fn mark(&mut self) -> io::Result<()> {
        let Some(identity) = self.before.identity else {
            return Ok(());
        };
        let file = self.open_target(identity, true)?;
        let marker = fsutil::marker_time();
        self.validate_target_identity(Some(identity))?;
        if file
            .set_times(fs::FileTimes::new().set_modified(marker))
            .is_ok()
        {
            self.marker = Some(marker);
        }
        Ok(())
    }

    /// Restores the snapshot, verifies it, and attempts cleanup even if a
    /// restore operation fails. Every cleanup failure is reported once, under
    /// one heading; the restore failure stands beside it.
    fn restore(&mut self) -> Result<(), String> {
        let mut cleanup = self.capture_command_directories();
        let mut failures = Vec::new();
        let restored = self
            .read_state()
            .map_err(|error| error.to_string())
            .and_then(|(after, handle)| {
                // A post-command Windows file handle denies delete sharing;
                // release it before restore renames or removes the pathname.
                #[cfg(windows)]
                drop(handle);
                #[cfg(not(windows))]
                let _handle = handle;
                self.restore_state(&after)
            });
        if let Err(error) = restored {
            failures.push(error);
        }
        cleanup.extend(self.remove_created_directories());
        if !cleanup.is_empty() {
            failures.push(format!(
                "could not safely remove the created directories: {}",
                cleanup.join(", ")
            ));
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("; "))
        }
    }

    /// Reads the current file only after the current resolved path and every
    /// retained parent identity have been revalidated.
    fn read_state(&self) -> io::Result<(State, Option<fs::File>)> {
        self.read_state_with_pinning(true)
    }

    /// Takes the initial snapshot with delete sharing so the Verify command
    /// can replace the file while this retained handle preserves its bytes.
    fn read_state_before_command(&self) -> io::Result<(State, Option<fs::File>)> {
        self.read_state_with_pinning(false)
    }

    fn read_state_with_pinning(
        &self,
        pin_file_name: bool,
    ) -> io::Result<(State, Option<fs::File>)> {
        let relative = self.relative(&self.target)?;
        self.validate_path(&self.target)?;
        match self.filesystem.symlink_metadata(&relative) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok((State::default(), None));
            }
            Err(error) => return Err(error),
            Ok(metadata) if is_link_or_reparse_point(&metadata) => {
                return Err(io::Error::other(
                    "the snapshot became a symbolic link; refusing to follow it",
                ));
            }
            Ok(metadata) if !metadata.is_file() => {
                return Err(io::Error::other("the snapshot is no longer a regular file"));
            }
            Ok(_) => {}
        }

        self.validate_path(&self.target)?;
        let mut file = open_snapshot_file(&self.target, true, false, pin_file_name, false)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(io::Error::other("the snapshot is no longer a regular file"));
        }
        self.validate_path(&self.target)?;
        let identity = FileIdentity::capture(self.filesystem, &self.target, &file)?;
        let content =
            read_with_validation(&mut file, || self.validate_target_identity(Some(identity)))?;
        self.validate_target_identity(Some(identity))?;
        let metadata = file.metadata()?;
        Ok((
            State {
                content: Some(content),
                mode: fsutil::mode_of(&metadata),
                modified: metadata.modified().ok(),
                identity: Some(identity),
            },
            Some(file),
        ))
    }

    fn restore_state(&self, after: &State) -> Result<(), String> {
        if !self.was_written(after) {
            return self.restore_time(after);
        }

        match &self.before.content {
            Some(content) => {
                self.write_atomic(
                    content,
                    self.before.mode,
                    self.before.modified,
                    after.identity,
                )
                .map_err(|error| error.to_string())?;
                self.verify_restored(content, self.before.mode)
            }
            None => {
                if let Some(identity) = after.identity {
                    self.validate_target_identity(Some(identity))
                        .map_err(|error| error.to_string())?;
                    let relative = self
                        .relative(&self.target)
                        .map_err(|error| error.to_string())?;
                    self.filesystem
                        .remove_file(&relative)
                        .map_err(|error| error.to_string())?;
                }
                Ok(())
            }
        }
    }

    /// True when the command wrote or replaced the file.
    fn was_written(&self, after: &State) -> bool {
        if after.content != self.before.content {
            return true;
        }
        if after.mode != self.before.mode {
            return true;
        }
        if after.identity != self.before.identity {
            return true;
        }
        match (self.marker, after.modified) {
            (Some(marker), Some(modified)) => modified != marker,
            (Some(_), None) => true,
            (None, _) => false,
        }
    }

    /// Restores the original timestamp on the same current file object. The
    /// timestamp itself remains best effort.
    fn restore_time(&self, after: &State) -> Result<(), String> {
        let (Some(before), Some(current), Some(identity)) =
            (self.before.modified, after.modified, after.identity)
        else {
            return Ok(());
        };
        if before == current {
            return Ok(());
        }
        let file = self
            .open_target(identity, true)
            .map_err(|error| error.to_string())?;
        self.validate_target_identity(Some(identity))
            .map_err(|error| error.to_string())?;
        let _ = file.set_times(fs::FileTimes::new().set_modified(before));
        Ok(())
    }

    /// Writes the snapshot back through the retained root capability: the
    /// bytes, the mode and the time are written to a temporary file in the
    /// target's directory and renamed there, so a redirected ancestor cannot
    /// carry the restore outside the repository. The target identity is
    /// validated immediately before and after the capability operation; the
    /// write itself is rooted instead of pathname-based.
    fn write_atomic(
        &self,
        content: &[u8],
        mode: Option<u32>,
        modified: Option<SystemTime>,
        expected_target: Option<FileIdentity>,
    ) -> io::Result<()> {
        self.validate_target_identity(expected_target)?;
        let relative = self.relative(&self.target)?;
        self.filesystem
            .write_atomic(&relative, content, mode, modified)?;
        let captured = self.capture_current()?;
        self.validate_target_identity(Some(captured))
    }

    /// Captures the identity of the current snapshot object through a
    /// capability-opened handle.
    fn capture_current(&self) -> io::Result<FileIdentity> {
        let relative = self.relative(&self.target)?;
        let file = self.filesystem.open_file(&relative)?;
        FileIdentity::capture(self.filesystem, &self.target, &file)
    }

    fn verify_restored(&self, content: &[u8], mode: Option<u32>) -> Result<(), String> {
        let relative = self
            .relative(&self.target)
            .map_err(|error| error.to_string())?;
        let mut file = self
            .filesystem
            .open_file(&relative)
            .map_err(|error| error.to_string())?;
        let metadata = file.metadata().map_err(|error| error.to_string())?;
        if !metadata.is_file() {
            return Err("the snapshot is no longer a regular file".to_string());
        }
        let identity = FileIdentity::capture(self.filesystem, &self.target, &file)
            .map_err(|error| error.to_string())?;
        self.validate_target_identity(Some(identity))
            .map_err(|error| error.to_string())?;
        let read =
            read_with_validation(&mut file, || self.validate_target_identity(Some(identity)))
                .map_err(|error| error.to_string())?;
        if read != content {
            return Err("the bytes differ from the snapshot".to_string());
        }
        if let Some(mode) = mode {
            self.validate_target_identity(Some(identity))
                .map_err(|error| error.to_string())?;
            let metadata = file.metadata().map_err(|error| error.to_string())?;
            if fsutil::mode_of(&metadata) != Some(mode) {
                return Err("the mode differs from the snapshot".to_string());
            }
        }
        Ok(())
    }

    /// Updates the snapshot after restore using the same containment checks as
    /// restore. Missing parent directories are created one at a time and
    /// recorded so a failed update can clean up only its own candidates.
    fn write_snapshot(&mut self, produced: &[u8]) -> Result<bool, String> {
        let (current, _) = self.read_state().map_err(|error| error.to_string())?;
        if current.content.as_deref() == Some(produced) {
            return Ok(false);
        }
        self.create_snapshot_directories()?;
        self.validate_target_identity(current.identity)
            .map_err(|error| error.to_string())?;
        let relative = self
            .relative(&self.target)
            .map_err(|error| error.to_string())?;
        self.filesystem
            .write(&relative, produced)
            .map_err(|error| error.to_string())?;
        let captured = self.capture_current().map_err(|error| error.to_string())?;
        self.validate_target_identity(Some(captured))
            .map_err(|error| error.to_string())?;
        Ok(true)
    }

    fn create_snapshot_directories(&mut self) -> Result<(), String> {
        let parent = self.target.parent().unwrap_or(&self.canonical_root);
        let relative = parent
            .strip_prefix(&self.canonical_root)
            .map_err(|_| "the snapshot directory is outside the repository root".to_string())?
            .to_path_buf();
        let mut candidate = self.canonical_root.clone();

        let create = (|| {
            for component in relative.components() {
                let Component::Normal(name) = component else {
                    continue;
                };
                candidate.push(name);
                self.validate_path(&candidate)?;
                let candidate_relative = self.relative(&candidate)?;
                match self.filesystem.symlink_metadata(&candidate_relative) {
                    Ok(metadata) if is_link_or_reparse_point(&metadata) || !metadata.is_dir() => {
                        return Err(io::Error::other(
                            "the snapshot directory is no longer a directory",
                        ));
                    }
                    Ok(_) => self
                        .ensure_tracked_directory(&candidate)
                        .map_err(io::Error::other)?,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        self.validate_path(&candidate)?;
                        self.filesystem.create_dir(&candidate_relative)?;
                        self.missing.push(candidate.clone());
                        self.sort_missing();
                        let directory = self.capture_directory_guarded(&candidate).map_err(|error| {
                            io::Error::other(format!(
                                "the new directory could not be identified and was left in place: {error}"
                            ))
                        })?;
                        self.created_directories.push(directory);
                        self.validate_path(&candidate)?;
                    }
                    Err(error) => return Err(error),
                }
            }
            Ok::<(), io::Error>(())
        })();

        if let Err(error) = create {
            let directory = candidate.clone();
            let message = format!(
                "cannot create the snapshot directory {}: {error}",
                path::display(Some(&self.root), &directory)
            );
            let rollback = self.remove_created_directories();
            if rollback.is_empty() {
                return Err(message);
            }
            return Err(format!(
                "{message}; rollback failed: {}",
                rollback.join(", ")
            ));
        }
        Ok(())
    }

    fn ensure_tracked_directory(&mut self, directory: &Path) -> Result<(), String> {
        if self
            .original_directories
            .iter()
            .any(|tracked| tracked.path == directory)
        {
            self.validate_path(directory)
                .map_err(|error| error.to_string())?;
            return Ok(());
        }
        if let Some(index) = self
            .created_directories
            .iter()
            .position(|tracked| tracked.path == directory)
        {
            self.validate_path(directory)
                .map_err(|error| error.to_string())?;
            if self.created_directories[index].handle.is_none() {
                let captured = self
                    .capture_directory_guarded(directory)
                    .map_err(|error| error.to_string())?;
                if captured.identity != self.created_directories[index].identity {
                    return Err(
                        "the snapshot directory identity changed after cleanup validation".into(),
                    );
                }
                self.created_directories[index].handle = captured.handle;
            }
            return Ok(());
        }
        let tracked = self
            .capture_directory_guarded(directory)
            .map_err(|error| error.to_string())?;
        self.created_directories.push(tracked);
        self.validate_path(directory)
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    /// Revalidates every recorded missing candidate after the command and
    /// retains identities for the directories the command actually created.
    fn capture_command_directories(&mut self) -> Vec<String> {
        let mut failures = Vec::new();
        for candidate in self.missing.clone() {
            if self
                .created_directories
                .iter()
                .any(|directory| directory.path == candidate)
            {
                continue;
            }
            let result = (|| {
                self.validate_path(&candidate)?;
                let relative = self.relative(&candidate)?;
                let metadata = match self.filesystem.symlink_metadata(&relative) {
                    Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                    Err(error) => return Err(error),
                    Ok(metadata) => metadata,
                };
                if is_link_or_reparse_point(&metadata) || !metadata.is_dir() {
                    return Err(io::Error::other(
                        "the recorded directory candidate was replaced; refusing cleanup",
                    ));
                }
                let tracked = self.capture_directory_guarded(&candidate)?;
                self.created_directories.push(tracked);
                self.validate_path(&candidate)?;
                Ok(())
            })();
            if let Err(error) = result {
                failures.push(format!(
                    "{}: left alone because cleanup validation failed: {error}",
                    path::display(Some(&self.root), &candidate),
                ));
            }
        }
        failures
    }

    /// Removes only the missing-before-command candidates whose current
    /// directory identity is still the one captured after the command. Returns
    /// the raw per-candidate failures; the caller sets the heading.
    fn remove_created_directories(&mut self) -> Vec<String> {
        let mut failures = Vec::new();
        for candidate in self.missing.clone() {
            let Some(index) = self
                .created_directories
                .iter()
                .position(|directory| directory.path == candidate)
            else {
                continue;
            };
            match self.validate_tracked_directory(&self.created_directories[index]) {
                Ok(false) => {
                    self.created_directories.remove(index);
                    continue;
                }
                Err(error) => {
                    failures.push(format!(
                        "{}: {error}",
                        path::display(Some(&self.root), &candidate)
                    ));
                    continue;
                }
                Ok(true) => {}
            }

            let relative = match self.relative(&candidate) {
                Ok(relative) => relative,
                Err(error) => {
                    failures.push(format!(
                        "{}: {error}",
                        path::display(Some(&self.root), &candidate)
                    ));
                    continue;
                }
            };
            let entries = match self.filesystem.read_dir(&relative) {
                Ok(entries) => entries,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    self.created_directories.remove(index);
                    continue;
                }
                Err(error) => {
                    failures.push(format!(
                        "{}: {error}",
                        path::display(Some(&self.root), &candidate)
                    ));
                    continue;
                }
            };
            match entries.into_iter().next() {
                Some(Ok(_)) => continue,
                Some(Err(error)) => {
                    failures.push(format!(
                        "{}: {error}",
                        path::display(Some(&self.root), &candidate)
                    ));
                    continue;
                }
                None => {}
            }

            match self.validate_tracked_directory(&self.created_directories[index]) {
                Ok(false) => {
                    self.created_directories.remove(index);
                }
                Err(error) => failures.push(format!(
                    "{}: {error}",
                    path::display(Some(&self.root), &candidate)
                )),
                Ok(true) => {
                    // The final identity/containment check above is complete.
                    // Windows cannot remove a directory while its own handle
                    // denies delete sharing, so release only this candidate;
                    // its parent chain remains pinned. The removal itself is
                    // relative to the retained root capability.
                    let expected_identity = self.created_directories[index].identity;
                    self.created_directories[index].release_handle();
                    let relative = match self.relative(&candidate) {
                        Ok(relative) => relative,
                        Err(error) => {
                            failures.push(format!(
                                "{}: {error}",
                                path::display(Some(&self.root), &candidate)
                            ));
                            continue;
                        }
                    };
                    match self.filesystem.remove_dir(&relative) {
                        Ok(()) => {
                            self.created_directories.remove(index);
                        }
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {
                            #[cfg(windows)]
                            failures.push(format!(
                                "{}: the directory disappeared after final cleanup validation",
                                path::display(Some(&self.root), &candidate)
                            ));
                            #[cfg(not(windows))]
                            self.created_directories.remove(index);
                        }
                        Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => {
                            #[cfg(windows)]
                            match self.capture_directory_guarded(&candidate) {
                                Ok(captured) if captured.identity == expected_identity => {
                                    self.created_directories[index].handle = captured.handle;
                                }
                                Ok(_) => failures.push(format!(
                                    "{}: directory identity changed while cleanup released its handle",
                                    path::display(Some(&self.root), &candidate)
                                )),
                                Err(capture) => failures.push(format!(
                                    "{}: could not repin the directory after cleanup: {capture}",
                                    path::display(Some(&self.root), &candidate)
                                )),
                            }
                            #[cfg(not(windows))]
                            let _ = (error, expected_identity);
                        }
                        Err(error) => failures.push(format!(
                            "{}: {error}",
                            path::display(Some(&self.root), &candidate)
                        )),
                    }
                }
            }
        }
        failures
    }

    /// Checks the repository root and retained identities, then finishes with
    /// fresh containment checks before a pathname operation.
    fn validate_path(&self, candidate: &Path) -> io::Result<()> {
        self.validate_containment(candidate)?;
        self.check_directory_records(candidate)?;
        // Directory identities are captured through handles above. Resolve
        // the root, configured snapshot, and candidate again only after those
        // captures so an outside redirection during validation fails closed.
        self.validate_containment(candidate)
    }

    fn validate_containment(&self, candidate: &Path) -> io::Result<()> {
        if fs::canonicalize(&self.root)? != self.canonical_root {
            return Err(io::Error::other(
                "the repository root changed after the snapshot was captured",
            ));
        }
        let current_target = path::inside_repository(&self.canonical_root, &self.snapshot_path)
            .map_err(|refusal| {
                io::Error::other(refusal.message(
                    "the configured snapshot path",
                    "must remain inside the repository root",
                ))
            })?;
        if current_target != self.target {
            return Err(io::Error::other(
                "the configured snapshot path no longer resolves to its authorized target",
            ));
        }
        let relative = candidate
            .strip_prefix(&self.canonical_root)
            .map_err(|_| io::Error::other("the path is outside the repository root"))?;
        let resolved = path::inside_repository(&self.canonical_root, &relative.to_string_lossy())
            .map_err(|refusal| {
            io::Error::other(refusal.message(
                "the snapshot path",
                "must remain inside the repository root",
            ))
        })?;
        if resolved != candidate {
            return Err(io::Error::other(
                "the path no longer resolves to its authorized in-root location",
            ));
        }
        if fs::canonicalize(&self.root)? != self.canonical_root {
            return Err(io::Error::other(
                "the repository root changed during path validation",
            ));
        }
        Ok(())
    }

    fn check_directory_records(&self, candidate: &Path) -> io::Result<()> {
        for tracked in self
            .original_directories
            .iter()
            .chain(self.created_directories.iter())
            .filter(|directory| candidate.starts_with(&directory.path))
        {
            self.check_directory_identity(tracked)?;
        }
        Ok(())
    }

    fn check_directory_identity(&self, tracked: &TrackedDirectory) -> io::Result<()> {
        self.validate_containment(&tracked.path)?;
        let metadata = self
            .filesystem
            .symlink_metadata(&self.relative(&tracked.path)?)?;
        if is_link_or_reparse_point(&metadata) || !metadata.is_dir() {
            return Err(io::Error::other(
                "a snapshot parent is no longer the captured directory",
            ));
        }
        self.validate_containment(&tracked.path)?;
        let handle = open_directory(self.filesystem, &tracked.path)?;
        let handle_metadata = handle.metadata()?;
        if is_reparse_point(&handle_metadata) || !handle_metadata.is_dir() {
            return Err(io::Error::other(
                "a snapshot parent is no longer the captured directory",
            ));
        }
        self.validate_containment(&tracked.path)?;
        if FileIdentity::capture(self.filesystem, &tracked.path, &handle)? != tracked.identity {
            return Err(io::Error::other(
                "a snapshot parent identity changed; refusing the filesystem operation",
            ));
        }
        // This must follow the handle/path identity comparison: the pathname
        // could have been redirected while that identity was being captured.
        self.validate_containment(&tracked.path)
    }

    fn validate_target_identity(&self, expected: Option<FileIdentity>) -> io::Result<()> {
        self.validate_target_identity_after(expected, || Ok(()))
    }

    // The callback is a no-op in normal operation. It lets tests deterministically
    // replace a path after identity capture and before the final pathname checks.
    fn validate_target_identity_after(
        &self,
        expected: Option<FileIdentity>,
        after_identity: impl FnOnce() -> io::Result<()>,
    ) -> io::Result<()> {
        self.validate_path(&self.target)?;
        match self
            .filesystem
            .symlink_metadata(&self.relative(&self.target)?)
        {
            Err(error) if error.kind() == io::ErrorKind::NotFound && expected.is_none() => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Err(error),
            Err(error) => return Err(error),
            Ok(_) if expected.is_none() => {
                return Err(io::Error::other(
                    "the snapshot target appeared or changed before the filesystem operation",
                ));
            }
            Ok(metadata) if is_link_or_reparse_point(&metadata) => {
                return Err(io::Error::other(
                    "the snapshot became a symbolic link; refusing to follow it",
                ));
            }
            Ok(metadata) if !metadata.is_file() => {
                return Err(io::Error::other("the snapshot is no longer a regular file"));
            }
            Ok(_) => {
                self.validate_path(&self.target)?;
                let file = open_snapshot_file(&self.target, true, false, true, false)?;
                self.validate_path(&self.target)?;
                let identity = FileIdentity::capture(self.filesystem, &self.target, &file)?;
                if Some(identity) != expected {
                    return Err(io::Error::other(
                        "the snapshot identity changed before the filesystem operation",
                    ));
                }
            }
        }

        match expected {
            Some(identity) => self.validate_final_file_identity_after(
                &self.target,
                identity,
                "the snapshot identity changed before the filesystem operation",
                after_identity,
            ),
            None => {
                after_identity()?;
                // Recheck absence after all retained-directory identity work;
                // containment is deliberately the last check before success.
                self.validate_path(&self.target)?;
                match self
                    .filesystem
                    .symlink_metadata(&self.relative(&self.target)?)
                {
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        self.validate_containment(&self.target)
                    }
                    Err(error) => Err(error),
                    Ok(_) => Err(io::Error::other(
                        "the snapshot target appeared or changed before the filesystem operation",
                    )),
                }
            }
        }
    }

    fn validate_final_file_identity(
        &self,
        file_path: &Path,
        expected: FileIdentity,
        changed_message: &str,
    ) -> io::Result<()> {
        self.validate_final_file_identity_after(file_path, expected, changed_message, || Ok(()))
    }

    fn validate_final_file_identity_after(
        &self,
        file_path: &Path,
        expected: FileIdentity,
        changed_message: &str,
        after_identity: impl FnOnce() -> io::Result<()>,
    ) -> io::Result<()> {
        // Recheck every retained ancestor first, then compare the current path
        // entry's identity. The hook is used only by tests to redirect the
        // path after identity capture and before the final containment check.
        self.validate_path(file_path)?;
        if capture_pinned_file_identity(self.filesystem, file_path)? != expected {
            return Err(io::Error::other(changed_message.to_string()));
        }
        after_identity()?;
        if capture_pinned_file_identity(self.filesystem, file_path)? != expected {
            return Err(io::Error::other(changed_message.to_string()));
        }
        // This must remain the final path check: a redirection during either
        // path-based identity lookup must be rejected before the operation.
        self.validate_containment(file_path)
    }

    fn open_target(&self, expected: FileIdentity, write_attributes: bool) -> io::Result<fs::File> {
        self.validate_target_identity(Some(expected))?;
        self.validate_path(&self.target)?;
        let file = open_snapshot_file(&self.target, true, false, true, write_attributes)?;
        self.validate_path(&self.target)?;
        if FileIdentity::capture(self.filesystem, &self.target, &file)? != expected {
            return Err(io::Error::other(
                "the snapshot identity changed while it was opened",
            ));
        }
        self.validate_final_file_identity(
            &self.target,
            expected,
            "the snapshot identity changed while it was opened",
        )?;
        Ok(file)
    }

    fn validate_tracked_directory(&self, tracked: &TrackedDirectory) -> io::Result<bool> {
        self.validate_containment(&tracked.path)?;
        match self
            .filesystem
            .symlink_metadata(&self.relative(&tracked.path)?)
        {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
            Ok(metadata) if is_link_or_reparse_point(&metadata) || !metadata.is_dir() => {
                return Err(io::Error::other(
                    "the created path is no longer the captured directory",
                ));
            }
            Ok(_) => {}
        }
        for parent in self
            .original_directories
            .iter()
            .chain(self.created_directories.iter())
            .filter(|parent| parent.path != tracked.path && tracked.path.starts_with(&parent.path))
        {
            self.check_directory_identity(parent)?;
        }
        let canonical = match fs::canonicalize(&tracked.path) {
            Ok(canonical) => canonical,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        if !path::inside_root_directory(&self.canonical_root, &canonical)
            || canonical != tracked.path
        {
            return Err(io::Error::other(
                "the directory no longer canonically resolves inside the repository root",
            ));
        }
        self.check_directory_identity(tracked)?;
        // Revalidate all retained ancestors and the directory itself after
        // the canonical and object-identity checks above.
        self.validate_path(&tracked.path)?;
        Ok(true)
    }

    fn capture_directory_guarded(&self, directory: &Path) -> io::Result<TrackedDirectory> {
        self.validate_path(directory)?;
        let metadata = self
            .filesystem
            .symlink_metadata(&self.relative(directory)?)?;
        if is_link_or_reparse_point(&metadata) || !metadata.is_dir() {
            return Err(io::Error::other("the path is no longer a directory"));
        }
        self.validate_path(directory)?;
        let handle = open_directory(self.filesystem, directory)?;
        let handle_metadata = handle.metadata()?;
        if is_reparse_point(&handle_metadata) || !handle_metadata.is_dir() {
            return Err(io::Error::other("the path changed while it was opened"));
        }
        self.validate_path(directory)?;
        let identity = FileIdentity::capture(self.filesystem, directory, &handle)?;
        let tracked = TrackedDirectory {
            path: directory.to_path_buf(),
            identity,
            handle: Some(handle),
        };
        self.validate_path(directory)?;
        if !self.identity_matches_path(&tracked)? {
            return Err(io::Error::other(
                "the directory identity changed while it was captured",
            ));
        }
        self.validate_containment(directory)?;
        Ok(tracked)
    }

    /// True when the named directory is still the object whose handle is
    /// retained. Unix compares the pathname identity with the handle identity;
    /// Windows relies on the pinned no-delete handle, which keeps the name
    /// attached to the captured object for as long as it is open.
    fn identity_matches_path(&self, tracked: &TrackedDirectory) -> io::Result<bool> {
        #[cfg(unix)]
        {
            Ok(FileIdentity::capture_path(self.filesystem, &tracked.path)? == tracked.identity)
        }
        #[cfg(not(unix))]
        {
            let _ = tracked;
            Ok(true)
        }
    }

    fn sort_missing(&mut self) {
        self.missing.sort_by(|left, right| {
            right
                .components()
                .count()
                .cmp(&left.components().count())
                .then_with(|| left.cmp(right))
        });
        self.missing.dedup();
    }
}

/// Reads until EOF, validating the authorized path and object immediately
/// before every individual read call. In particular, a short read never
/// carries its earlier validation over to the next read.
fn read_with_validation<R: Read>(
    reader: &mut R,
    mut validate: impl FnMut() -> io::Result<()>,
) -> io::Result<Vec<u8>> {
    let mut content = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        validate()?;
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            return Ok(content);
        }
        content.extend_from_slice(&buffer[..read]);
    }
}

fn capture_existing_chain(
    filesystem: &RootDir,
    root: &Path,
    target: &Path,
) -> io::Result<Vec<TrackedDirectory>> {
    let parent = target.parent().unwrap_or(root);
    let relative = parent
        .strip_prefix(root)
        .map_err(|_| io::Error::other("the snapshot parent is outside the repository root"))?;
    let mut directories = vec![capture_directory(filesystem, root)?];
    let mut candidate = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        candidate.push(name);
        match filesystem.symlink_metadata(&filesystem.relative(&candidate)?) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => break,
            Err(error) => return Err(error),
            Ok(metadata) if is_link_or_reparse_point(&metadata) || !metadata.is_dir() => {
                return Err(io::Error::other(
                    "the resolved snapshot parent is not a regular directory",
                ));
            }
            Ok(_) => directories.push(capture_directory(filesystem, &candidate)?),
        }
    }
    Ok(directories)
}

fn capture_directory(filesystem: &RootDir, path: &Path) -> io::Result<TrackedDirectory> {
    let metadata = filesystem.symlink_metadata(&filesystem.relative(path)?)?;
    if is_link_or_reparse_point(&metadata) || !metadata.is_dir() {
        return Err(io::Error::other("the path is no longer a directory"));
    }
    let handle = open_directory(filesystem, path)?;
    let handle_metadata = handle.metadata()?;
    if is_reparse_point(&handle_metadata) || !handle_metadata.is_dir() {
        return Err(io::Error::other("the path changed while it was opened"));
    }
    let identity = FileIdentity::capture(filesystem, path, &handle)?;
    Ok(TrackedDirectory {
        path: path.to_path_buf(),
        identity,
        handle: Some(handle),
    })
}

/// Opens a retained directory handle for identity checks. The handle is the
/// pinning mechanism on Windows and the identity source on Unix; it is opened
/// after the caller validated the path against the root capability, and it is
/// read-only: every removal it guards is issued relative to that capability.
fn open_directory(filesystem: &RootDir, path: &Path) -> io::Result<fs::File> {
    #[cfg(unix)]
    {
        let _ = filesystem;
        fs::File::open(path)
    }

    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        const FILE_SHARE_READ: u32 = 0x0000_0001;
        const FILE_SHARE_WRITE: u32 = 0x0000_0002;
        // The capability confirms that the name still resolves inside the root;
        // the handle itself needs the no-delete share mode, which cap-std does
        // not expose for a single file.
        filesystem.symlink_metadata(&filesystem.relative(path)?)?;
        let mut options = fs::OpenOptions::new();
        options
            .access_mode(0)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
        let handle = options.open(path)?;
        let metadata = handle.metadata()?;
        if is_reparse_point(&metadata) || !metadata.is_dir() {
            return Err(io::Error::other(
                "the path is a reparse point or is no longer a directory",
            ));
        }
        Ok(handle)
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = (filesystem, path);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "directory handles are unavailable on this platform",
        ))
    }
}

/// True when capability metadata names a link or a Windows reparse point.
fn is_link_or_reparse_point(metadata: &cap_std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use cap_std::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }

    #[cfg(not(windows))]
    {
        metadata.is_symlink()
    }
}

fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }

    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

/// Configures final-component opening behavior for snapshot files. Windows
/// has no pathname file identity in this design: the retained no-delete handle
/// pins the name, so the post-command handle denies delete sharing while it is
/// open. The pre-command snapshot handle deliberately permits delete sharing
/// so the child may replace the file.
fn configure_snapshot_open(
    options: &mut fs::OpenOptions,
    pin_file_name: bool,
    write_attributes: bool,
) {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        const FILE_SHARE_READ: u32 = 0x0000_0001;
        const FILE_SHARE_WRITE: u32 = 0x0000_0002;
        const FILE_SHARE_DELETE: u32 = 0x0000_0004;
        const GENERIC_READ: u32 = 0x8000_0000;
        const FILE_WRITE_ATTRIBUTES: u32 = 0x0000_0100;

        let mut share_mode = FILE_SHARE_READ | FILE_SHARE_WRITE;
        if !pin_file_name {
            share_mode |= FILE_SHARE_DELETE;
        }
        options
            .share_mode(share_mode)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        if write_attributes {
            options.access_mode(GENERIC_READ | FILE_WRITE_ATTRIBUTES);
        }
    }

    #[cfg(not(windows))]
    {
        let _ = (options, pin_file_name, write_attributes);
    }
}

fn open_snapshot_file(
    path: &Path,
    read: bool,
    write: bool,
    pin_file_name: bool,
    write_attributes: bool,
) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(read).write(write);
    configure_snapshot_open(&mut options, pin_file_name, write_attributes);
    let file = options.open(path)?;
    reject_reparse_handle(&file)?;
    Ok(file)
}

/// Opens a pinned handle for the file and captures its identity.
fn capture_pinned_file_identity(filesystem: &RootDir, path: &Path) -> io::Result<FileIdentity> {
    let file = open_snapshot_file(path, true, false, true, false)?;
    FileIdentity::capture(filesystem, path, &file)
}

fn reject_reparse_handle(file: &fs::File) -> io::Result<()> {
    #[cfg(windows)]
    {
        if is_reparse_point(&file.metadata()?) {
            return Err(io::Error::other(
                "the opened path is a reparse point; refusing to follow it",
            ));
        }
    }

    #[cfg(not(windows))]
    {
        let _ = file;
    }
    Ok(())
}

#[cfg(test)]
mod io_validation_tests {
    use super::read_with_validation;
    use std::cell::RefCell;
    use std::io::{self, Read};
    use std::rc::Rc;

    struct PartialReader {
        bytes: &'static [u8],
        offset: usize,
        max_read: usize,
        events: Rc<RefCell<Vec<&'static str>>>,
    }

    impl Read for PartialReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.events.borrow_mut().push("read");
            let count = buffer
                .len()
                .min(self.max_read)
                .min(self.bytes.len() - self.offset);
            buffer[..count].copy_from_slice(&self.bytes[self.offset..self.offset + count]);
            self.offset += count;
            Ok(count)
        }
    }

    #[test]
    fn reads_revalidate_before_every_partial_read_and_eof() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let mut reader = PartialReader {
            bytes: b"abcde",
            offset: 0,
            max_read: 2,
            events: Rc::clone(&events),
        };

        let bytes = read_with_validation(&mut reader, || {
            events.borrow_mut().push("validate");
            Ok(())
        })
        .expect("the read succeeds");

        assert_eq!(bytes, b"abcde");
        assert_eq!(
            *events.borrow(),
            [
                "validate", "read", "validate", "read", "validate", "read", "validate", "read"
            ]
        );
    }

    #[test]
    fn read_validation_failure_stops_before_the_next_partial_read() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let mut reader = PartialReader {
            bytes: b"abcde",
            offset: 0,
            max_read: 2,
            events: Rc::clone(&events),
        };
        let mut validations = 0;

        let error = read_with_validation(&mut reader, || {
            events.borrow_mut().push("validate");
            validations += 1;
            if validations == 2 {
                Err(io::Error::other("path changed"))
            } else {
                Ok(())
            }
        })
        .expect_err("the second validation fails");

        assert_eq!(error.to_string(), "path changed");
        assert_eq!(*events.borrow(), ["validate", "read", "validate"]);
    }
}

#[cfg(all(test, unix))]
mod final_containment_tests {
    use super::{FileIdentity, Guard};
    use crate::capfs::RootDir;
    use crate::fsutil::Temporary;
    use std::fs;
    use std::io;
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;

    fn fixture(prefix: &str) -> (Temporary, PathBuf, PathBuf, PathBuf) {
        let temporary = Temporary::new(prefix).expect("temporary directory");
        let root = temporary.path().join("repo");
        fs::create_dir(&root).expect("repository directory");
        let root = fs::canonicalize(root).expect("canonical repository directory");
        let snapshots = root.join("snapshots");
        fs::create_dir_all(&snapshots).expect("snapshot directory");
        let target = snapshots.join("output.txt");
        fs::write(&target, b"original").expect("snapshot file");
        (temporary, root, snapshots, target)
    }

    fn guard<'a>(
        filesystem: &'a RootDir,
        root: &std::path::Path,
        target: &std::path::Path,
    ) -> (Guard<'a>, FileIdentity) {
        let guard = Guard::take(
            target,
            root,
            "snapshots/output.txt".to_string(),
            Vec::new(),
            filesystem,
        )
        .expect("guard");
        let identity = guard.before.identity.expect("snapshot identity");
        (guard, identity)
    }

    #[test]
    fn target_validation_rechecks_path_identity_after_handle_identity_capture() {
        let (_temporary, root, _snapshots, target) = fixture("verify-final-identity");
        let filesystem = RootDir::open(&root).expect("root capability");
        let (guard, identity) = guard(&filesystem, &root, &target);
        let previous = target.with_file_name("previous-output.txt");

        let error = guard
            .validate_target_identity_after(Some(identity), || {
                fs::rename(&target, &previous)?;
                fs::write(&target, b"replacement")?;
                Ok(())
            })
            .expect_err("the final path identity differs from the captured object");

        assert!(error.to_string().contains("snapshot identity changed"));
    }

    #[test]
    fn target_validation_runs_final_containment_after_identity_capture() {
        let (_temporary, canonical_root, snapshots, target) = fixture("verify-final-containment");
        let filesystem = RootDir::open(&canonical_root).expect("root capability");
        let (guard, identity) = guard(&filesystem, &canonical_root, &target);
        let root = guard.canonical_root.clone();
        let moved = root.join("snapshots-original");
        let outside = guard.root.join("outside");
        fs::create_dir(&outside).expect("outside directory");
        fs::hard_link(&target, outside.join("output.txt")).expect("same-identity outside link");

        let error = guard
            .validate_target_identity_after(Some(identity), || -> io::Result<()> {
                fs::rename(&snapshots, &moved)?;
                symlink(&outside, &snapshots)?;
                Ok(())
            })
            .expect_err("the final containment check rejects the outside link");

        // Containment reports the redirection in its own words when it runs
        // first; the retained capability refuses the absolute link target
        // itself when it reaches it first. Either way the operation fails
        // closed before anything is written through the replacement.
        let message = error.to_string();
        assert!(
            message.contains("authorized target")
                || message.contains("led outside of the filesystem"),
            "{message}"
        );
    }
}

#[cfg(all(test, windows))]
mod windows_pinning_tests {
    use super::{Guard, open_directory, open_snapshot_file};
    use crate::capfs::RootDir;
    use crate::fsutil::Temporary;
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output};

    fn fixture() -> (Temporary, PathBuf, PathBuf, PathBuf) {
        let temporary = Temporary::new("verify-windows-pinning").expect("temporary directory");
        let root = temporary.path().join("repo");
        fs::create_dir(&root).expect("repository directory");
        let root = fs::canonicalize(root).expect("canonical repository directory");
        let snapshots = root.join("snapshots");
        fs::create_dir(&snapshots).expect("snapshot directory");
        let target = snapshots.join("output.txt");
        fs::write(&target, b"original").expect("snapshot file");
        (temporary, root, snapshots, target)
    }

    fn guard<'a>(filesystem: &'a RootDir, root: &Path, target: &Path) -> Guard<'a> {
        Guard::take(
            target,
            root,
            "snapshots/output.txt".to_string(),
            Vec::new(),
            filesystem,
        )
        .expect("guard")
    }

    fn rename_from_child(
        directory: &Path,
        source_name: &str,
        new_name: &str,
    ) -> io::Result<Output> {
        let command = format!("ren \"{source_name}\" \"{new_name}\"");
        Command::new("cmd.exe")
            .args(["/D", "/C", &command])
            .current_dir(directory)
            .output()
    }

    #[test]
    fn child_cannot_rename_a_pinned_snapshot_parent() {
        let (_temporary, root, snapshots, target) = fixture();
        let filesystem = RootDir::open(&root).expect("root capability");
        let guard = guard(&filesystem, &root, &target);
        let output = rename_from_child(
            snapshots.parent().expect("snapshot parent"),
            "snapshots",
            "snapshots-renamed",
        )
        .expect("start child rename command");

        assert!(
            !output.status.success(),
            "child unexpectedly renamed a pinned parent: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(snapshots.is_dir());
        assert!(!guard.canonical_root.join("snapshots-renamed").exists());
    }

    #[test]
    fn retained_precommand_snapshot_handle_allows_file_replacement() {
        let (_temporary, root, _snapshots, target) = fixture();
        let filesystem = RootDir::open(&root).expect("root capability");
        let guard = guard(&filesystem, &root, &target);
        let previous = target.with_file_name("previous-output.txt");
        fs::rename(&target, &previous).expect("rename snapshot held with delete sharing");
        assert!(!target.exists());
        assert!(previous.is_file());
        assert_eq!(
            fs::read(&previous).expect("read previous snapshot"),
            b"original"
        );

        fs::write(&target, b"replacement").expect("replacement snapshot");
        assert_eq!(fs::read(&target).expect("read replacement"), b"replacement");
        drop(guard);
    }

    #[test]
    fn postcommand_pinned_snapshot_handle_blocks_rename_until_drop() {
        let temporary = Temporary::new("verify-windows-file-pinning").expect("temporary directory");
        let target = temporary.path().join("output.txt");
        let renamed = temporary.path().join("previous-output.txt");
        fs::write(&target, b"contents").expect("snapshot file");

        let pinned = open_snapshot_file(&target, true, false, true, false)
            .expect("open post-command pinned snapshot file");
        assert!(
            fs::rename(&target, &renamed).is_err(),
            "rename must be denied while the pinned handle is open"
        );
        assert!(target.is_file());
        assert!(!renamed.exists());

        drop(pinned);
        fs::rename(&target, &renamed).expect("rename after dropping pinned handle");
        assert!(!target.exists());
        assert!(renamed.is_file());
    }

    #[test]
    fn final_reparse_point_opens_are_refused() {
        let temporary = Temporary::new("verify-windows-reparse").expect("temporary directory");

        // A junction needs no privilege, so this half always asserts.
        let directory = temporary.path().join("directory");
        let junction = temporary.path().join("directory-junction");
        fs::create_dir(&directory).expect("regular directory");
        make_junction(&junction, &directory);
        let filesystem = RootDir::open(temporary.path()).expect("root capability");
        assert!(
            open_directory(&filesystem, &junction).is_err(),
            "directory reparse point must not be followed"
        );

        // A file symlink needs SeCreateSymbolicLinkPrivilege. Where it is not
        // available the test says so; the junction half above has already
        // asserted the refusal, so nothing passes silently.
        use std::os::windows::fs::symlink_file;

        let regular = temporary.path().join("regular.txt");
        let file_link = temporary.path().join("file-link.txt");
        fs::write(&regular, b"contents").expect("regular file");
        match symlink_file(&regular, &file_link) {
            Ok(()) => assert!(
                open_snapshot_file(&file_link, true, false, true, false).is_err(),
                "final file symlink must not be followed"
            ),
            Err(error) => assert!(
                matches!(
                    error.kind(),
                    io::ErrorKind::PermissionDenied | io::ErrorKind::Unsupported
                ),
                "could not create test file symlink: {error}"
            ),
        }
    }

    /// Creates a directory junction — a reparse point that needs no privilege.
    #[cfg(windows)]
    fn make_junction(link: &Path, target: &Path) {
        let status = Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .status()
            .expect("cmd runs");
        assert!(status.success(), "the junction {link:?} was not created");
    }
}
