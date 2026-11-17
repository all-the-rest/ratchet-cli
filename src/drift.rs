//! The drift gates: a generated file that differs from what its generator
//! produces (SPEC §7).
//!
//! Two generator modes exist:
//!
//! * with `output_arg` — the generator writes to a **temporary path**; the
//!   tree is never touched;
//! * without `output_arg` — the generator writes the file **in place**.  The
//!   engine then owns a **verified restore**: the original bytes and the file
//!   mode are snapshotted, the generator runs, the produced bytes are read and
//!   the file is restored — and the restore is verified afterwards.  A restore
//!   that fails ends the run with an operational error that names the file and
//!   says that the working tree was modified: a swallowed restore error is a
//!   mutated tree that nobody was told about.
//!
//! The comparison is against the **committed** file (`git show HEAD:<path>`),
//! not against the working-tree copy: the gate asks whether the repository
//! holds what the generator produces.

use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use crate::Error;
use crate::capfs::{RootDir, TrackedDir};
use crate::config::{Config, Generated, Tier};
use crate::diff;
use crate::fsutil;
use crate::generate::Generator;
use crate::git::{Git, HeadEntry};
use crate::path;

/// The status of one generated file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriftStatus {
    /// The committed file is what the generator produces.
    Ok,
    /// The committed file differs from what the generator produces.
    Drift,
    /// The file is not committed yet.
    New,
}

impl DriftStatus {
    /// The name used in the JSON report.
    pub fn as_str(self) -> &'static str {
        match self {
            DriftStatus::Ok => "ok",
            DriftStatus::Drift => "drift",
            DriftStatus::New => "new",
        }
    }

    /// True when the generated file is a failed gate.
    pub fn is_failure(self) -> bool {
        matches!(self, DriftStatus::Drift)
    }
}

/// The outcome of one drift gate.
#[derive(Debug, Clone)]
pub struct DriftEntry {
    /// The generated file, relative to the repository root.
    pub path: String,
    /// The status of the gate.
    pub status: DriftStatus,
    /// The unified diff (committed → produced), if the gate drifted.
    pub diff: Option<String>,
    /// A human readable explanation, if there is something to say.
    pub detail: Option<String>,
}

/// What a drift run does.
#[derive(Debug)]
pub struct Options<'a> {
    /// The repository root.
    pub root: &'a Path,
    /// Write the produced content into the generated files.
    pub fix: bool,
    /// The one generated file to run, relative to the repository root.
    pub path: Option<&'a str>,
    /// The one tier to run; `None` runs every tier.
    pub tier: Option<Tier>,
}

/// The configured path, its resolved link-entry name, and its resolved target.
struct TargetPaths<'a> {
    resolved: &'a Path,
    named: &'a Path,
    target: &'a Path,
}

/// The result of a drift run.
#[derive(Debug, Default)]
pub struct Outcome {
    /// One entry per generated file, sorted by path.
    pub entries: Vec<DriftEntry>,
    /// Notes about what was skipped and why.
    pub notes: Vec<String>,
    /// The debug log (`--debug`).
    pub debug: Vec<String>,
}

impl Outcome {
    /// The number of generated files that drifted.
    pub fn drifted(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.status.is_failure())
            .count()
    }
}

/// Runs the drift gates of the `generated` entries of the config.
pub fn run(config: &Config, git: &Git, options: &Options) -> Result<Outcome, Error> {
    let mut outcome = Outcome::default();
    let filesystem = RootDir::open(options.root)
        .map_err(|error| Error::new(format!("cannot open the repository root: {error}")))?;

    if !git.is_repo() {
        // The committed comparisons are skipped; the gate still runs (SPEC 8).
        outcome
            .notes
            .push("drift comparison skipped: not a git repository".to_string());
    }

    // The entries are sorted by path: the report is a stable diff.
    let mut entries: Vec<&Generated> = config.generated.iter().collect();
    entries.sort_by(|left, right| left.path.cmp(&right.path));

    let mut selected = 0;
    for entry in entries {
        // The generated file lies inside the repository root: a path that
        // points outside it would let the engine write outside the tree it
        // owns.
        let subject = format!("the generated file {}", entry.path);
        // The resolved path is the place the operating system puts the file:
        // the links of the named path have been followed.  It is the path the
        // engine touches — and the place the directory of the file is created.
        let resolved = match path::inside_repository(options.root, &entry.path) {
            Ok(resolved) => resolved,
            Err(refusal) => {
                return Err(Error::new(
                    refusal.message(&subject, "must lie inside the repository root"),
                ));
            }
        };
        if resolved == path::canonical_root(options.root) {
            return Err(Error::new(format!(
                "{subject} resolves to the repository root, not a file"
            )));
        }
        // A **dangling** link at the file position is refused.  The reason
        // depends on the mode: the in-place generator would write through the
        // link and create the file behind it — and the restore cannot take
        // that file back — while the temporary-output mode never touches
        // the tree, but the comparison would follow the link to a file that
        // is not there.
        let named_target = named_target(options.root, &filesystem, &entry.path, &resolved);
        let resolved_target = filesystem.relative(&resolved).map_err(|error| {
            Error::new(format!(
                "{subject} cannot be used through the repository root: {error}"
            ))
        })?;
        if is_dangling_link(&filesystem, &named_target, &resolved_target) {
            let reason = if entry.output_arg.is_some() {
                "the comparison would follow it to a file that is not there"
            } else {
                "the generator would write through it, and the restore could not take the file \
                 back"
            };
            return Err(Error::new(format!(
                "{subject} is a symbolic link that points at nothing — {reason}"
            )));
        }
        if let Some(filter) = options.path {
            if filter != entry.path {
                continue;
            }
        }
        // A slow generator does not run in the fast tier: a fast tier that
        // waits for a database, a browser or the network is not fast (SPEC 7).
        if let Some(tier) = options.tier {
            if entry.tier != tier {
                outcome.notes.push(format!(
                    "the drift gate {} is skipped: it belongs to the tier `{}`",
                    entry.path,
                    entry.tier.as_str()
                ));
                continue;
            }
        }
        selected += 1;
        let target_paths = TargetPaths {
            resolved: &resolved,
            named: &named_target,
            target: &resolved_target,
        };
        let result = run_one(
            entry,
            git,
            options,
            &mut outcome.debug,
            &target_paths,
            &filesystem,
        )?;
        outcome.entries.push(result);
    }

    if let Some(filter) = options.path {
        if selected == 0 {
            return Err(Error::new(format!("no drift gate for {filter}")));
        }
    }

    Ok(outcome)
}

/// Runs one drift gate.
fn run_one(
    entry: &Generated,
    git: &Git,
    options: &Options,
    debug: &mut Vec<String>,
    paths: &TargetPaths<'_>,
    filesystem: &RootDir,
) -> Result<DriftEntry, Error> {
    let root = options.root;

    let produced = match &entry.output_arg {
        Some(output_arg) => {
            // The generator writes to a temporary path: the tree is untouched.
            let generator = Generator {
                command: entry.command.clone(),
                output_arg: output_arg.clone(),
            };
            let artifact = generator
                .run(Path::new(&entry.path), root)
                .map_err(|error| Error::with_output(error.message, error.output))?;
            debug.push(format!("generator {}: {}", entry.path, artifact.output));
            artifact.bytes
        }
        None => produce_in_place(entry, root, options, debug, paths, filesystem)?,
    };

    // The comparison is against the committed file (SPEC §7).  The lookup
    // uses the file the system resolves the named path to — the same file the
    // generator wrote; the report and the messages keep the named path.
    let lookup = committed_path(&entry.path, paths.target);
    let (status, committed) = compare(git, &lookup, &entry.path, &produced)?;

    let mut detail = None;
    let mut diff = None;

    match status {
        DriftStatus::Ok => {}
        DriftStatus::New => {
            detail = Some("not committed yet".to_string());
        }
        DriftStatus::Drift => {
            let committed = committed.unwrap_or_default();
            diff = Some(diff::unified(&entry.path, &committed, &produced));
            detail = Some("differs from the committed file".to_string());
        }
    }

    // `--fix` writes the produced content into the generated file — and only
    // into the generated files.  The in-place generator has already written
    // it; the temporary-path generator has not.
    //
    // The gate stays **failed**: the gate measures the *committed* artifact,
    // and the committed file is still stale until the fix is committed.  A
    // run with `--fix` in a pipeline is therefore never green against a stale
    // HEAD; that is deliberate, not a bug (SPEC 7).
    if options.fix {
        match status {
            DriftStatus::Drift => {
                if entry.output_arg.is_some() {
                    write_fixed(root, filesystem, paths.target, &entry.path, &produced)?;
                }
                detail = Some(
                    "the produced content was written to the working tree; commit it — the committed file is still stale"
                        .to_string(),
                );
            }
            DriftStatus::New => {
                let written = if entry.output_arg.is_some() {
                    let current = filesystem.read(paths.target).ok();
                    if current.as_deref() != Some(produced.as_slice()) {
                        write_fixed(root, filesystem, paths.target, &entry.path, &produced)?;
                        true
                    } else {
                        false
                    }
                } else {
                    // The in-place generator has written the file.
                    true
                };
                if written {
                    detail = Some(
                        "the produced content was written to the working tree; the file is not committed yet"
                            .to_string(),
                    );
                }
            }
            DriftStatus::Ok => {}
        }
    }

    Ok(DriftEntry {
        path: entry.path.clone(),
        status,
        diff,
        detail,
    })
}

/// True when the path is a symbolic link that points at nothing.
///
/// The generator would write through such a link and create the file behind
/// it — and the restore cannot take that file back: the tree would change,
/// and nobody was told about it.
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

fn is_dangling_link(filesystem: &RootDir, named: &Path, resolved: &Path) -> bool {
    let Ok(metadata) = filesystem.symlink_metadata(named) else {
        return false;
    };
    if !metadata.is_symlink() {
        return false;
    }
    // The link is followed: a link that points at nothing fails here.
    filesystem.metadata(resolved).is_err()
}

/// The path the comparison is made against.
///
/// The comparison looks at the file the operating system **resolves** the
/// named path to: git stores a linked directory as a blob, so the committed
/// tree holds the target of the link and not the link.  The path is relative
/// to the repository root; the report and the messages keep the **named**
/// path — the path the project named and the path the user knows.
fn committed_path(relative: &str, resolved: &Path) -> String {
    let resolved = resolved.to_string_lossy().replace('\\', "/");
    if resolved.is_empty() {
        relative.to_string()
    } else {
        resolved
    }
}

/// Compares the produced content with the committed file.
///
/// `lookup` is the path of the committed file, `named` the path the project
/// named: the messages name the named path.
///
/// * no repository → the comparison is skipped, the file counts as `new`;
/// * the file is not in the committed tree → `new`, informational and not a
///   failure;
/// * the committed path is a directory → an operational error;
/// * the committed file cannot be read → an operational error.  A broken
///   reference must not disable a gate (SPEC 8).
fn compare(
    git: &Git,
    lookup: &str,
    named: &str,
    produced: &[u8],
) -> Result<(DriftStatus, Option<Vec<u8>>), Error> {
    if !git.is_repo() {
        return Ok((DriftStatus::New, None));
    }
    match git.head_entry(lookup) {
        HeadEntry::Missing => Ok((DriftStatus::New, None)),
        HeadEntry::Directory => Err(Error::new(format!(
            "the committed path {named} is a directory, not a file"
        ))),
        HeadEntry::Unreadable => Err(Error::new(format!(
            "the committed file {named} cannot be read"
        ))),
        HeadEntry::Blob(object) => {
            let committed = git
                .show_object(&object)
                .ok_or_else(|| Error::new(format!("the committed file {named} cannot be read")))?;
            if committed == produced {
                Ok((DriftStatus::Ok, Some(committed)))
            } else {
                Ok((DriftStatus::Drift, Some(committed)))
            }
        }
    }
}

/// Runs a generator that writes the file **in place** and returns the produced
/// bytes.
///
/// The engine owns a verified restore: whatever the generator did, the tree is
/// byte- and mode-identical afterwards — unless `--fix` asked for the produced
/// content to stay.
fn produce_in_place(
    entry: &Generated,
    root: &Path,
    options: &Options,
    debug: &mut Vec<String>,
    paths: &TargetPaths<'_>,
    filesystem: &RootDir,
) -> Result<Vec<u8>, Error> {
    let display = entry.path.as_str();

    // 1. the snapshot: the original bytes, the file mode and the time the
    //    file was written last.
    let mut snapshot = Snapshot::take(filesystem, paths.target, paths.named)
        .map_err(|error| Error::new(format!("cannot read {display}: {error}")))?;

    // 1b. the marker: the file is touched with an old time, so that the
    //     engine can tell whether the generator wrote it — whatever the
    //     timestamp granularity of the filesystem is.
    let marker = fsutil::marker_time();
    let _ = filesystem.set_modified(paths.target, marker);

    // 1c. the directories the file needs are created if they do not exist: a
    //     generator that writes a file into a new directory would otherwise
    //     fail, and a project cannot commit an empty directory. Cleanup after
    //     restore checks identity and containment immediately before removing
    //     each pathname.
    //     A directory that cannot be created is an error of the engine, not
    //     of the generator: the user is not sent to the wrong place.
    //
    //     The engine creates the directories the **operating system** needs
    //     to walk the named path: the missing directories of the traversal —
    //     a `..` can only pop a directory that exists — and the directory
    //     the path resolves to, the place the file lands in when a link
    //     leads there.
    //
    //     The error names the directory that could not be created: it is
    //     named as it was created — the resolved one as the system resolves
    //     it, the named one as the project named it — and it is the place the
    //     user has to look.
    let created =
        create_directories(root, filesystem, &entry.path, paths.resolved).map_err(|error| {
            Error::new(format!(
                "cannot create the directory {} of {display}: {}{}",
                directory_name(root, &error.directory),
                error.cause,
                error.rollback_message(root)
            ))
        })?;
    snapshot.created = created;

    // 2. the generator.
    let run = run_generator(&entry.command, root, display, debug);

    // 3. the produced bytes.
    let produced = snapshot.read_current(filesystem).ok();

    // 3b. did the generator write the file?  A generator that leaves it alone
    //     produced nothing: the comparison below would compare the committed
    //     file with itself and report a green that says nothing.
    let wrote = snapshot.was_written(filesystem, produced.as_deref(), marker);

    // 4. restore and verify for an ordinary check; keep the produced content
    //    for --fix.
    let restored = if options.fix {
        snapshot.keep(root, paths.resolved, filesystem)
    } else {
        snapshot.restore(root, filesystem)
    };

    // A failed restore or keep is the more important error: the working tree
    // was modified and nobody was told about it.
    if let Err(error) = restored {
        let operation = if options.fix { "fix" } else { "restore" };
        return Err(Error::new(format!(
            "the {operation} of {display} failed — the working tree was modified: {error}"
        )));
    }

    if let Err(failure) = run {
        return Err(Error::with_output(failure.message, failure.transcript));
    }

    let produced = produced.ok_or_else(|| {
        Error::new(format!(
            "the generator for {display} did not write its output"
        ))
    })?;

    if !wrote {
        return Err(Error::new(format!(
            "the generator for {display} produced nothing — it did not write the file"
        )));
    }

    Ok(produced)
}

/// A directory the engine could not create or safely identify.
#[derive(Debug)]
struct DirectoryError {
    directory: PathBuf,
    cause: io::Error,
    rollback: Vec<(PathBuf, io::Error)>,
}

impl DirectoryError {
    fn rollback_message(&self, root: &Path) -> String {
        if self.rollback.is_empty() {
            return String::new();
        }
        let failures = self
            .rollback
            .iter()
            .map(|(directory, error)| format!("{}: {error}", directory_name(root, directory)))
            .collect::<Vec<_>>()
            .join(", ");
        format!("; rollback could not remove {failures} — the working tree was modified")
    }
}

/// Creates directories for both the resolved target and the named traversal.
/// Every filesystem operation is relative to the retained repository root.
fn create_directories(
    root: &Path,
    filesystem: &RootDir,
    relative: &str,
    resolved: &Path,
) -> Result<Vec<TrackedDir>, DirectoryError> {
    create_directories_with(
        root,
        filesystem,
        relative,
        resolved,
        |filesystem, candidate| filesystem.create_dir(candidate),
    )
}

fn create_directories_with<F>(
    root: &Path,
    filesystem: &RootDir,
    relative: &str,
    resolved: &Path,
    mut create: F,
) -> Result<Vec<TrackedDir>, DirectoryError>
where
    F: FnMut(&RootDir, &Path) -> io::Result<()>,
{
    let mut directories = Vec::new();
    if let Some(parent) = resolved.parent() {
        if path::inside_root_directory(root, parent) {
            if let Ok(parent) = filesystem.relative(parent) {
                directories.push((parent, None));
            }
        }
    }
    match traversal_directories(root, filesystem, relative) {
        Ok(traversal) => directories.extend(
            traversal
                .into_iter()
                .map(|(candidate, alias)| (candidate, Some(alias))),
        ),
        Err((directory, cause)) => {
            return Err(DirectoryError {
                directory,
                cause,
                rollback: Vec::new(),
            });
        }
    }

    let mut created = Vec::new();
    for (directory, alias) in directories {
        let missing = missing_directories(filesystem, &directory);
        let candidates: Vec<PathBuf> = if missing.is_empty() {
            vec![directory.clone()]
        } else {
            missing.into_iter().rev().collect()
        };
        for candidate in candidates {
            if let Err(cause) = validate_directory_candidate(root, filesystem, &candidate) {
                let rollback = rollback_directories(filesystem, &mut created);
                return Err(DirectoryError {
                    directory: candidate,
                    cause,
                    rollback,
                });
            }
            if candidate == directory {
                if let Some(alias) = &alias {
                    if let Err(cause) =
                        validate_traversal_alias(root, filesystem, alias, &candidate)
                    {
                        let rollback = rollback_directories(filesystem, &mut created);
                        return Err(DirectoryError {
                            directory: candidate,
                            cause,
                            rollback,
                        });
                    }
                }
            }
            match create(filesystem, &candidate) {
                Ok(()) => match filesystem.track_dir(&candidate) {
                    Ok(tracked) => match filesystem.verify_tracked_dir(&tracked).and_then(|valid| {
                        if !valid {
                            return Ok(false);
                        }
                        if candidate == directory {
                            if let Some(alias) = &alias {
                                validate_traversal_alias(root, filesystem, alias, &candidate)?;
                            }
                        }
                        Ok(true)
                    }) {
                        Ok(true) => created.push(tracked),
                        Ok(false) => {
                            let rollback = rollback_directories(filesystem, &mut created);
                            return Err(DirectoryError {
                                directory: candidate,
                                cause: io::Error::other(
                                    "the created directory disappeared before validation",
                                ),
                                rollback,
                            });
                        }
                        Err(cause) => {
                            return Err(post_creation_directory_error(
                                filesystem,
                                &candidate,
                                &mut created,
                                cause,
                            ));
                        }
                    },
                    Err(cause) => {
                        return Err(post_creation_directory_error(
                            filesystem,
                            &candidate,
                            &mut created,
                            cause,
                        ));
                    }
                },
                Err(cause)
                    if cause.kind() == io::ErrorKind::AlreadyExists
                        && filesystem
                            .metadata(&candidate)
                            .is_ok_and(|metadata| metadata.is_dir()) =>
                {
                    if let Err(cause) = validate_directory_candidate(root, filesystem, &candidate) {
                        let rollback = rollback_directories(filesystem, &mut created);
                        return Err(DirectoryError {
                            directory: candidate,
                            cause,
                            rollback,
                        });
                    }
                }
                Err(cause) => {
                    let rollback = rollback_directories(filesystem, &mut created);
                    return Err(DirectoryError {
                        directory: candidate,
                        cause,
                        rollback,
                    });
                }
            }
        }
    }
    created.reverse();
    Ok(created)
}

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

fn validate_directory_candidate(
    root: &Path,
    filesystem: &RootDir,
    candidate: &Path,
) -> io::Result<()> {
    let resolved =
        path::inside_repository(root, &candidate.to_string_lossy()).map_err(|refusal| {
            io::Error::other(refusal.message(
                "the directory candidate",
                "must lie inside the repository root",
            ))
        })?;
    if filesystem.relative(&resolved)? != candidate {
        return Err(io::Error::other(
            "the directory candidate changed or resolves through a symbolic link",
        ));
    }
    Ok(())
}

fn validate_traversal_alias(
    root: &Path,
    filesystem: &RootDir,
    alias: &str,
    candidate: &Path,
) -> io::Result<()> {
    let resolved = path::inside_repository(root, alias).map_err(|refusal| {
        io::Error::other(refusal.message(
            "the directory candidate",
            "must lie inside the repository root",
        ))
    })?;
    if filesystem.relative(&resolved)? != candidate {
        return Err(io::Error::other(
            "the named traversal changed or resolves outside the repository root",
        ));
    }
    Ok(())
}

fn verify_created_directory_identity(
    filesystem: &RootDir,
    directory: &TrackedDir,
) -> io::Result<bool> {
    filesystem.verify_tracked_dir(directory)
}

fn remove_created_directory(filesystem: &RootDir, directory: &mut TrackedDir) -> io::Result<()> {
    if !filesystem.verify_tracked_dir(directory)? {
        return Ok(());
    }
    if !filesystem.remove_tracked_dir(directory)? {
        return Err(io::Error::new(
            io::ErrorKind::DirectoryNotEmpty,
            "the created directory is not empty",
        ));
    }
    Ok(())
}

#[cfg(all(test, unix))]
fn remove_created_directory_with<F>(
    filesystem: &RootDir,
    directory: &mut TrackedDir,
    before_final_validation: F,
) -> io::Result<()>
where
    F: FnOnce(),
{
    if !filesystem.verify_tracked_dir(directory)? {
        return Ok(());
    }
    before_final_validation();
    remove_created_directory(filesystem, directory)
}

fn remove_created_empty_directories(
    filesystem: &RootDir,
    directories: &mut [TrackedDir],
) -> Vec<(PathBuf, io::Error)> {
    let mut failures = Vec::new();
    for directory in directories {
        match verify_created_directory_identity(filesystem, directory) {
            Ok(false) => continue,
            Err(error) => {
                failures.push((directory.path().to_path_buf(), error));
                continue;
            }
            Ok(true) => {}
        }
        match filesystem.remove_tracked_dir(directory) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => failures.push((directory.path().to_path_buf(), error)),
        }
    }
    failures
}

fn rollback_directories(
    filesystem: &RootDir,
    created: &mut [TrackedDir],
) -> Vec<(PathBuf, io::Error)> {
    let mut failures = Vec::new();
    for directory in created.iter_mut().rev() {
        if let Err(error) = remove_created_directory(filesystem, directory) {
            if error.kind() != io::ErrorKind::NotFound {
                failures.push((directory.path().to_path_buf(), error));
            }
        }
    }
    failures
}

fn post_creation_directory_error(
    filesystem: &RootDir,
    candidate: &Path,
    created: &mut [TrackedDir],
    cause: io::Error,
) -> DirectoryError {
    let mut rollback = rollback_directories(filesystem, created);
    match filesystem.symlink_metadata(candidate) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Ok(_) => rollback.push((
            candidate.to_path_buf(),
            io::Error::other(format!(
                "post-creation validation failed; candidate left in place: {cause}"
            )),
        )),
        Err(error) => rollback.push((
            candidate.to_path_buf(),
            io::Error::other(format!(
                "post-creation validation failed; candidate may remain and was left untouched: \
                 {cause}; cannot inspect it: {error}"
            )),
        )),
    }
    DirectoryError {
        directory: candidate.to_path_buf(),
        cause: io::Error::other(format!("post-creation validation failed: {cause}")),
        rollback,
    }
}

fn traversal_directories(
    root: &Path,
    filesystem: &RootDir,
    relative: &str,
) -> Result<Vec<(PathBuf, String)>, (PathBuf, io::Error)> {
    let components: Vec<Component<'_>> = Path::new(relative).components().collect();
    let mut prefix = PathBuf::new();
    let mut directories = Vec::new();
    for (index, component) in components.iter().enumerate() {
        match component {
            Component::Normal(name) => prefix.push(name),
            Component::ParentDir => {
                prefix.pop();
            }
            Component::CurDir => {}
            _ => {}
        }
        if index + 1 < components.len() {
            let alias = prefix.to_string_lossy().into_owned();
            let resolved = path::inside_repository(root, &alias).map_err(|refusal| {
                (
                    PathBuf::from(&alias),
                    io::Error::other(refusal.message(
                        "the directory candidate",
                        "must lie inside the repository root",
                    )),
                )
            })?;
            let relative = filesystem
                .relative(&resolved)
                .map_err(|error| (PathBuf::from(&alias), error))?;
            if relative != Path::new(".") {
                directories.push((relative, alias));
            }
        }
    }
    Ok(directories)
}

fn directory_name(root: &Path, directory: &Path) -> String {
    if directory.is_absolute() {
        path::relative_to(root, directory).unwrap_or_else(|| path::file_name(directory))
    } else {
        directory.to_string_lossy().replace('\\', "/")
    }
}

/// Writes the produced content into the generated file.
///
/// The mode of an existing file is kept: `--fix` changes the content of the
/// generated files, nothing else.
fn write_fixed(
    root: &Path,
    filesystem: &RootDir,
    target: &Path,
    relative: &str,
    produced: &[u8],
) -> Result<(), Error> {
    let display = path::display(Some(root), &root.join(relative));
    if filesystem.metadata(target).is_ok() {
        // Writing in place keeps the mode of the file.
        filesystem
            .write(target, produced)
            .map_err(|error| Error::new(format!("cannot write {display}: {error}")))?;
        return Ok(());
    }
    // A temporary-output generator never needs to traverse the configured
    // spelling; it writes the already-resolved in-root target directly.
    if let Some(parent) = target.parent() {
        filesystem.create_dir_all(parent).map_err(|error| {
            let directory = path::display(Some(root), &root.join(parent));
            Error::new(format!(
                "cannot write {display}: cannot create the directory {directory}: {error}"
            ))
        })?;
    }
    filesystem
        .write(target, produced)
        .map_err(|error| Error::new(format!("cannot write {display}: {error}")))
}

/// Attempts cleanup of empty directories created only to walk the named path.
///
/// The resolved target's parent and its ancestors are required for the file
/// to remain accessible. Canonicalizing each created directory also handles a
/// traversal that reaches one of those parents through an in-root link.
fn cleanup_fix_directories(
    root: &Path,
    filesystem: &RootDir,
    created: &mut [TrackedDir],
    resolved: &Path,
) -> Result<(), String> {
    let resolved_parent = resolved
        .parent()
        .and_then(|parent| filesystem.relative(parent).ok())
        .unwrap_or_else(|| PathBuf::from("."));
    let mut failures = Vec::new();
    for directory in created {
        if resolved_parent.starts_with(directory.path()) {
            // The target's parent chain is required to keep the generated file.
            continue;
        }
        match verify_created_directory_identity(filesystem, directory) {
            Ok(false) => continue,
            Err(error) => {
                failures.push((directory.path().to_path_buf(), error));
                continue;
            }
            Ok(true) => {}
        }
        match filesystem.remove_tracked_dir(directory) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => failures.push((directory.path().to_path_buf(), error)),
        }
    }
    if failures.is_empty() {
        return Ok(());
    }
    let failures = failures
        .iter()
        .map(|(directory, error)| format!("{}: {error}", directory_name(root, directory)))
        .collect::<Vec<_>>()
        .join(", ");
    Err(format!(
        "could not remove created traversal directories: {failures}"
    ))
}

/// A generator that did not succeed.
struct Failure {
    /// What happened, in ratchet's own words.
    message: String,
    /// The raw output of the generator; the diagnostic channel carries it.
    transcript: String,
}

/// Runs a generator in the repository root.
fn run_generator(
    command: &[String],
    root: &Path,
    display: &str,
    debug: &mut Vec<String>,
) -> Result<(), Failure> {
    let (program, arguments) = command.split_first().ok_or_else(|| Failure {
        message: format!("the generator for {display} could not be started: the command is empty"),
        transcript: "the generator command is empty".to_string(),
    })?;

    let output = Command::new(program)
        .args(arguments)
        .current_dir(root)
        .output()
        .map_err(|error| Failure {
            message: format!("the generator for {display} could not be started: {error}"),
            transcript: format!("{}: could not be started\n{error}", command.join(" ")),
        })?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let status = match output.status.code() {
        Some(code) => format!("status {code}"),
        None => "an unknown status".to_string(),
    };
    // The transcript shows the argv that was really executed — it belongs to
    // the diagnostic channel, never to the report (SPEC 6.2).
    let transcript = format!(
        "{}: {status}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        command.join(" ")
    );
    debug.push(format!("generator {display}: {transcript}"));

    if !output.status.success() {
        return Err(Failure {
            message: format!("the generator for {display} exited with {status}"),
            transcript,
        });
    }

    Ok(())
}

/// The snapshot of a generated file: its bytes, mode, time, and directories.
struct Snapshot {
    /// The resolved repository-relative target written by the generator.
    target: PathBuf,
    content: Option<Vec<u8>>,
    modified: Option<SystemTime>,
    inode: Option<u64>,
    mode: Option<u32>,
    link: Option<Link>,
    /// Engine-created directories, in child-before-parent cleanup order.
    created: Vec<TrackedDir>,
}

/// The symbolic link at the configured path, and the in-root path it resolves to.
struct Link {
    path: PathBuf,
    raw: PathBuf,
    resolved: PathBuf,
    content: Option<Vec<u8>>,
    modified: Option<SystemTime>,
    inode: Option<u64>,
    mode: Option<u32>,
}

impl Snapshot {
    fn take(filesystem: &RootDir, target: &Path, named_target: &Path) -> io::Result<Self> {
        match filesystem.symlink_metadata(named_target) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Snapshot {
                target: target.to_path_buf(),
                content: None,
                modified: None,
                inode: None,
                mode: None,
                link: None,
                created: Vec::new(),
            }),
            Err(error) => Err(error),
            Ok(metadata) if metadata.is_symlink() => {
                let raw = filesystem.read_link(named_target)?;
                let state = read_file_state(filesystem, target)?;
                let (content, modified, inode, mode) = match state {
                    Some((content, modified, inode, mode)) => {
                        (Some(content), modified, inode, mode)
                    }
                    None => (None, None, None, None),
                };
                Ok(Snapshot {
                    target: target.to_path_buf(),
                    content: None,
                    modified: None,
                    inode: None,
                    mode: None,
                    link: Some(Link {
                        path: named_target.to_path_buf(),
                        raw,
                        resolved: target.to_path_buf(),
                        content,
                        modified,
                        inode,
                        mode,
                    }),
                    created: Vec::new(),
                })
            }
            Ok(_) => {
                let state = read_file_state(filesystem, target)?.ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "the generated file is missing")
                })?;
                let (content, modified, inode, mode) = state;
                Ok(Snapshot {
                    target: target.to_path_buf(),
                    content: Some(content),
                    modified,
                    inode,
                    mode,
                    link: None,
                    created: Vec::new(),
                })
            }
        }
    }

    /// True when the generator wrote the file after the marker was set.
    fn was_written(
        &self,
        filesystem: &RootDir,
        produced: Option<&[u8]>,
        marker: SystemTime,
    ) -> bool {
        if self
            .link
            .as_ref()
            .is_some_and(|link| !link_is_unchanged(filesystem, link))
        {
            return true;
        }
        let current = self
            .link
            .as_ref()
            .map(|link| link.resolved.as_path())
            .unwrap_or(&self.target);
        let Ok(file) = filesystem.open_file(current) else {
            return false;
        };
        let Ok(metadata) = file.metadata() else {
            return false;
        };
        let Some(before) = self.modified() else {
            return true;
        };
        let now = metadata.modified().ok();
        if now != Some(before) && now != Some(marker) {
            return true;
        }
        if fsutil::inode_of(&metadata) != self.inode() {
            return true;
        }
        produced != self.bytes()
    }

    fn read_current(&self, filesystem: &RootDir) -> io::Result<Vec<u8>> {
        match &self.link {
            Some(link) if link_is_unchanged(filesystem, link) => filesystem.read(&link.resolved),
            Some(link) => filesystem.read(&link.path),
            None => filesystem.read(&self.target),
        }
    }

    fn bytes(&self) -> Option<&[u8]> {
        match &self.link {
            Some(link) => link.content.as_deref(),
            None => self.content.as_deref(),
        }
    }

    fn modified(&self) -> Option<SystemTime> {
        match &self.link {
            Some(link) => link.modified,
            None => self.modified,
        }
    }

    fn inode(&self) -> Option<u64> {
        match &self.link {
            Some(link) => link.inode,
            None => self.inode,
        }
    }

    fn restore(&mut self, root: &Path, filesystem: &RootDir) -> Result<(), String> {
        let restored = self.restore_file(filesystem);
        self.restore_with(root, filesystem, || restored)?;
        self.verify(filesystem)
    }

    fn restore_with<F>(
        &mut self,
        root: &Path,
        filesystem: &RootDir,
        restore_file: F,
    ) -> Result<(), String>
    where
        F: FnOnce() -> Result<(), String>,
    {
        let restored = restore_file();
        let cleaned = self.remove_created_directories(root, filesystem);
        match (restored, cleaned) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(restore), Ok(())) => Err(format!("file restoration failed: {restore}")),
            (Ok(()), Err(cleanup)) => Err(cleanup),
            (Err(restore), Err(cleanup)) => {
                Err(format!("file restoration failed: {restore}; {cleanup}"))
            }
        }
    }

    fn restore_file(&self, filesystem: &RootDir) -> Result<(), String> {
        match &self.link {
            Some(link) => {
                if let Some(content) = &link.content {
                    filesystem
                        .write_atomic(&link.resolved, content, link.mode, link.modified)
                        .map_err(|error| error.to_string())?;
                }
                if !link_is_unchanged(filesystem, link) {
                    filesystem
                        .remove_file(&link.path)
                        .map_err(|error| error.to_string())?;
                    filesystem
                        .create_symlink(&link.raw, &link.path)
                        .map_err(|error| error.to_string())?;
                }
            }
            None => match &self.content {
                Some(content) => filesystem
                    .write_atomic(&self.target, content, self.mode, self.modified)
                    .map_err(|error| error.to_string())?,
                None => {
                    if filesystem.symlink_metadata(&self.target).is_ok() {
                        filesystem
                            .remove_file(&self.target)
                            .map_err(|error| error.to_string())?;
                    }
                }
            },
        }
        Ok(())
    }

    fn keep(&mut self, root: &Path, resolved: &Path, filesystem: &RootDir) -> Result<(), String> {
        let kept = self.keep_mode(filesystem);
        let cleaned = cleanup_fix_directories(root, filesystem, &mut self.created, resolved);
        match (kept, cleaned) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(keep), Ok(())) => Err(keep),
            (Ok(()), Err(cleanup)) => Err(cleanup),
            (Err(keep), Err(cleanup)) => Err(format!("{keep}; {cleanup}")),
        }
    }

    fn keep_mode(&self, filesystem: &RootDir) -> Result<(), String> {
        if let (Some(mode), None) = (self.mode, &self.link) {
            let file = filesystem
                .open_file(&self.target)
                .map_err(|error| error.to_string())?;
            if fsutil::mode_of(&file.metadata().map_err(|error| error.to_string())?) != Some(mode) {
                filesystem
                    .set_mode(&self.target, mode)
                    .map_err(|error| error.to_string())?;
            }
        }
        Ok(())
    }

    fn remove_created_directories(
        &mut self,
        root: &Path,
        filesystem: &RootDir,
    ) -> Result<(), String> {
        let failures = remove_created_empty_directories(filesystem, &mut self.created);
        if failures.is_empty() {
            return Ok(());
        }
        let failures = failures
            .iter()
            .map(|(directory, error)| format!("{}: {error}", directory_name(root, directory)))
            .collect::<Vec<_>>()
            .join(", ");
        Err(format!("could not remove created directories: {failures}"))
    }

    fn verify(&self, filesystem: &RootDir) -> Result<(), String> {
        match &self.link {
            Some(link) => {
                let metadata = filesystem
                    .symlink_metadata(&link.path)
                    .map_err(|error| error.to_string())?;
                if !metadata.is_symlink() {
                    return Err("the target is no longer a symbolic link".to_string());
                }
                let raw = filesystem
                    .read_link(&link.path)
                    .map_err(|error| error.to_string())?;
                if raw != link.raw {
                    return Err("the symbolic link points somewhere else".to_string());
                }
                if let Some(content) = &link.content {
                    let read = filesystem
                        .read(&link.resolved)
                        .map_err(|error| error.to_string())?;
                    if &read != content {
                        return Err("the bytes differ from the snapshot".to_string());
                    }
                }
                Ok(())
            }
            None => match &self.content {
                Some(content) => {
                    let read = filesystem
                        .read(&self.target)
                        .map_err(|error| error.to_string())?;
                    if &read != content {
                        return Err("the bytes differ from the snapshot".to_string());
                    }
                    if let Some(mode) = self.mode {
                        let file = filesystem
                            .open_file(&self.target)
                            .map_err(|error| error.to_string())?;
                        if fsutil::mode_of(&file.metadata().map_err(|error| error.to_string())?)
                            != Some(mode)
                        {
                            return Err("the mode differs from the snapshot".to_string());
                        }
                    }
                    Ok(())
                }
                None => {
                    if filesystem.symlink_metadata(&self.target).is_ok() {
                        Err("the file was not removed".to_string())
                    } else {
                        Ok(())
                    }
                }
            },
        }
    }
}

fn link_is_unchanged(filesystem: &RootDir, link: &Link) -> bool {
    filesystem
        .symlink_metadata(&link.path)
        .is_ok_and(|metadata| metadata.is_symlink())
        && filesystem
            .read_link(&link.path)
            .is_ok_and(|raw| raw == link.raw)
}

type FileState = (Vec<u8>, Option<SystemTime>, Option<u64>, Option<u32>);

fn read_file_state(filesystem: &RootDir, path: &Path) -> io::Result<Option<FileState>> {
    let mut file = match filesystem.open_file(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut content = Vec::new();
    file.read_to_end(&mut content)?;
    let metadata = file.metadata()?;
    Ok(Some((
        content,
        metadata.modified().ok(),
        fsutil::inode_of(&metadata),
        fsutil::mode_of(&metadata),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::fs;

    fn filesystem(root: &crate::test_tempdir::TempDir) -> RootDir {
        RootDir::open(root.path()).expect("the repository capability opens")
    }

    #[cfg(unix)]
    #[test]
    fn restore_failure_still_removes_created_empty_directories() {
        let root = crate::test_tempdir::TempDir::new().expect("a temporary root");
        let filesystem = filesystem(&root);
        filesystem
            .create_dir(Path::new("new"))
            .expect("the parent is created");
        filesystem
            .create_dir(Path::new("new/nested"))
            .expect("the nested directory is created");
        let snapshot = Snapshot {
            target: PathBuf::from("new/nested/generated.md"),
            content: None,
            modified: None,
            inode: None,
            mode: None,
            link: None,
            created: vec![
                filesystem
                    .track_dir(Path::new("new/nested"))
                    .expect("the nested identity is captured"),
                filesystem
                    .track_dir(Path::new("new"))
                    .expect("the parent identity is captured"),
            ],
        };
        let mut snapshot = snapshot;

        let error = snapshot
            .restore_with(root.path(), &filesystem, || {
                Err("injected file restore failure".to_string())
            })
            .expect_err("the injected restoration fails");

        assert!(error.contains("injected file restore failure"), "{error}");
        assert!(
            !root.path().join("new").exists(),
            "the parent was left behind"
        );
        assert!(
            !root.path().join("new/nested").exists(),
            "the child was left behind"
        );
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_failure_names_repository_relative_paths() {
        let root = crate::test_tempdir::TempDir::new().expect("a temporary root");
        let filesystem = filesystem(&root);
        filesystem
            .create_dir(Path::new("not-a-directory"))
            .expect("the directory is created");
        let created = filesystem
            .track_dir(Path::new("not-a-directory"))
            .expect("the identity is captured");
        fs::remove_dir(root.path().join("not-a-directory")).expect("the directory is removed");
        fs::write(root.path().join("not-a-directory"), "content")
            .expect("the replacement file is created");
        let mut snapshot = Snapshot {
            target: PathBuf::from("not-a-directory"),
            content: None,
            modified: None,
            inode: None,
            mode: None,
            link: None,
            created: vec![created],
        };

        let error = snapshot
            .remove_created_directories(root.path(), &filesystem)
            .expect_err("the replaced path is not a directory");

        assert!(error.contains("not-a-directory"), "{error}");
        assert!(error.contains("no longer a directory"), "{error}");
        assert!(!error.contains(root.path().to_str().expect("the root is UTF-8")));
    }

    #[cfg(unix)]
    #[test]
    fn directory_creation_refuses_an_outside_traversal_before_creating_it() {
        use std::os::unix::fs::symlink;

        let root = crate::test_tempdir::TempDir::new().expect("a temporary root");
        let outside = crate::test_tempdir::TempDir::new().expect("an outside directory");
        let filesystem = filesystem(&root);
        let outside_path = fs::canonicalize(outside.path()).expect("outside resolves");
        symlink(&outside_path, root.path().join("link")).expect("the outside link is made");
        let root_name = root
            .path()
            .file_name()
            .expect("the temp root has a name")
            .to_string_lossy();
        let relative = format!("link/sub/../../{root_name}/generated.md");
        let resolved = fs::canonicalize(root.path())
            .expect("root resolves")
            .join("generated.md");
        assert_eq!(
            path::inside_repository(root.path(), &relative).unwrap(),
            resolved
        );
        let mut creates = 0;

        let error = create_directories_with(
            root.path(),
            &filesystem,
            &relative,
            &resolved,
            |filesystem, directory| {
                creates += 1;
                filesystem.create_dir(directory)
            },
        )
        .expect_err("the outside traversal is refused");

        assert_eq!(creates, 0);
        assert!(
            error
                .cause
                .to_string()
                .contains("must lie inside the repository root")
        );
        assert!(!outside.path().join("sub").exists());
        assert!(error.rollback.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn root_capability_blocks_creation_after_a_traversal_link_is_redirected() {
        use std::os::unix::fs::symlink;

        let root = crate::test_tempdir::TempDir::new().expect("a temporary root");
        let outside = crate::test_tempdir::TempDir::new().expect("an outside directory");
        let filesystem = filesystem(&root);
        let internal = root.path().join("internal");
        fs::create_dir(&internal).expect("the internal directory is made");
        let link = root.path().join("link");
        symlink(&internal, &link).expect("the internal link is made");
        let outside_keep = outside.path().join("keep.txt");
        fs::write(&outside_keep, "keep").expect("the unrelated file is written");
        let relative = "link/sub/../generated.md";
        let resolved = path::inside_repository(root.path(), relative).unwrap();

        let error = create_directories_with(
            root.path(),
            &filesystem,
            relative,
            &resolved,
            |filesystem, candidate| {
                if candidate == Path::new("internal/sub") {
                    fs::remove_file(&link)?;
                    symlink(outside.path(), &link)?;
                }
                filesystem.create_dir(candidate)
            },
        )
        .expect_err("post-creation alias validation detects the redirection");

        assert!(!outside.path().join("sub").exists());
        assert_eq!(fs::read_to_string(outside_keep).unwrap(), "keep");
        let rollback = error.rollback_message(root.path());
        assert!(rollback.contains("internal/sub"), "{rollback}");
        assert!(rollback.contains("candidate left in place"), "{rollback}");
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_cannot_follow_a_redirected_ancestor_outside_the_root() {
        use std::os::unix::fs::symlink;

        let root = crate::test_tempdir::TempDir::new().expect("a temporary root");
        let outside = crate::test_tempdir::TempDir::new().expect("an outside directory");
        let filesystem = filesystem(&root);
        filesystem.create_dir(Path::new("sub")).unwrap();
        filesystem.create_dir(Path::new("sub/x")).unwrap();
        let mut created = filesystem.track_dir(Path::new("sub/x")).unwrap();
        let outside_keep = outside.path().join("keep");
        fs::create_dir(&outside_keep).expect("the outside empty directory is made");
        fs::rename(root.path().join("sub"), outside.path().join("moved"))
            .expect("the tracked ancestor is moved outside");
        symlink(outside.path().join("moved"), root.path().join("sub"))
            .expect("the ancestor is redirected");

        let error = remove_created_directory_with(&filesystem, &mut created, || {})
            .expect_err("root-relative cleanup refuses the outside path");

        assert!(
            outside.path().join("moved/x").is_dir(),
            "cleanup removed the moved directory"
        );
        assert!(
            outside_keep.is_dir(),
            "the outside empty directory was removed"
        );
        assert!(
            error.to_string().contains("outside")
                || error.kind() == io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn rollback_reports_nonempty_directories_and_preserves_their_contents() {
        let root = crate::test_tempdir::TempDir::new().expect("a temporary root");
        let filesystem = filesystem(&root);
        let resolved = root.path().join("a/b/c/generated.md");
        let mut calls = 0;

        let error = create_directories_with(
            root.path(),
            &filesystem,
            "a/b/c/generated.md",
            &resolved,
            |filesystem, directory| {
                calls += 1;
                if calls == 3 {
                    return Err(io::Error::other("injected creation failure"));
                }
                filesystem.create_dir(directory)?;
                if calls == 2 {
                    filesystem.write(&directory.join("external.txt"), b"keep")?;
                }
                Ok(())
            },
        )
        .expect_err("the third directory creation fails");

        assert_eq!(error.rollback.len(), 2);
        assert_eq!(error.rollback[0].0, PathBuf::from("a/b"));
        assert_eq!(error.rollback[1].0, PathBuf::from("a"));
        let message = error.rollback_message(root.path());
        assert!(message.contains("a/b"), "{message}");
        assert!(message.contains("working tree was modified"), "{message}");
        assert!(root.path().join("a/b/external.txt").exists());
    }

    #[cfg(windows)]
    #[test]
    fn windows_directory_cleanup_releases_delete_sharing_pins() {
        let root = crate::test_tempdir::TempDir::new().expect("a temporary root");
        let filesystem = filesystem(&root);
        let mut created = vec![
            filesystem
                .create_tracked_dir(Path::new("new"))
                .expect("the directory is created and pinned"),
        ];

        assert!(remove_created_empty_directories(&filesystem, &mut created).is_empty());
        assert!(filesystem.symlink_metadata(Path::new("new")).is_err());
    }
}
