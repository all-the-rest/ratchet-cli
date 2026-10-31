//! Git integration.
//!
//! The engine only ever *reads* from git: the repository root and the
//! committed version of a file.  Every git error degrades to "there is no
//! committed reference" — a missing git must never panic.

use std::path::{Path, PathBuf};
use std::process::Command;

/// What the committed tree holds at a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadEntry {
    /// The path is not in the committed tree.
    Missing,
    /// The path is a file; the blob is the object with this name.
    Blob(String),
    /// The path is a directory.
    Directory,
    /// The committed tree cannot be read.
    Unreadable,
}

/// A git repository (or the absence of one).
#[derive(Debug, Default)]
pub struct Git {
    root: Option<PathBuf>,
}

impl Git {
    /// Looks for the repository that contains `cwd`.
    pub fn discover(cwd: &Path) -> Self {
        let root = run(cwd, &["rev-parse", "--show-toplevel"])
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty())
            .map(PathBuf::from);
        Self { root }
    }

    /// The repository root, if there is one.
    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// True when `cwd` is inside a git repository.
    pub fn is_repo(&self) -> bool {
        self.root.is_some()
    }

    /// The committed content of `path` (relative to the repository root).
    ///
    /// Returns `None` when there is no repository, when the file is not
    /// committed, or when git fails.
    pub fn show_head(&self, path: &str) -> Option<String> {
        let root = self.root.as_ref()?;
        let spec = format!("HEAD:{path}");
        run(root, &["show", &spec])
    }

    /// The tree entry of `path` in the committed tree.
    ///
    /// The entry is resolved **without** the blob: a tree that names a file
    /// whose blob cannot be read still names the file, and the file cannot be
    /// read — a broken reference must not disable a gate (SPEC 8).  The
    /// distinction matters: "not in the tree" is a file that is not committed
    /// yet, "in the tree but unreadable" is an operational error.
    pub fn head_entry(&self, path: &str) -> HeadEntry {
        let Some(root) = self.root.as_ref() else {
            return HeadEntry::Missing;
        };
        match head_state(root) {
            // The repository has no commit: nothing is committed yet.
            HeadState::NoCommit => return HeadEntry::Missing,
            // The repository is damaged: its tree cannot be read, and reading
            // its files as "not committed yet" would turn a broken repository
            // into a green gate.
            HeadState::Damaged => return HeadEntry::Unreadable,
            HeadState::Ready => {}
        }
        let Some(output) = run(root, &["ls-tree", "HEAD", "--", path]) else {
            return HeadEntry::Unreadable;
        };

        let mut entries = output.lines().filter(|line| !line.trim().is_empty());
        let Some(first) = entries.next() else {
            return HeadEntry::Missing;
        };
        if entries.next().is_some() {
            // More than one entry: the path is a directory and its contents
            // are listed.
            return HeadEntry::Directory;
        }

        // The entry is "<mode> <type> <object>\t<path>".
        let mut fields = first.split_whitespace();
        let _mode = fields.next();
        match (fields.next(), fields.next()) {
            (Some("blob"), Some(object)) => HeadEntry::Blob(object.to_string()),
            (Some("tree"), _) => HeadEntry::Directory,
            _ => HeadEntry::Unreadable,
        }
    }

    /// The bytes of a committed object.
    ///
    /// The object is read by its name: the tree entry told us which blob the
    /// committed file is, and reading it by name keeps the two steps apart.
    pub fn show_object(&self, object: &str) -> Option<Vec<u8>> {
        let root = self.root.as_ref()?;
        run_bytes(root, &["cat-file", "blob", object])
    }

    /// The last release tag: the most recent tag that names a release.
    ///
    /// The tag of the release is the reference of the release step (SPEC
    /// §16) — never a version string somebody typed.  A tag that is not a
    /// release tag (the moving major tag of the action, a pre-release) is
    /// **not** a release and does not count: it is skipped, and the search
    /// continues with the tag below it.
    ///
    /// The search walks the history of `HEAD`, so a tag that is not an
    /// ancestor of `HEAD` is not a release of this branch.
    pub fn last_release_tag(&self, prefix: &str) -> Option<String> {
        let root = self.root.as_ref()?;
        // The commits that carry a tag, newest first.  Every tag of a commit
        // is on its line.
        let output = run(
            root,
            &[
                "log",
                "--tags",
                "--simplify-by-decoration",
                "--pretty=format:%D",
            ],
        )?;
        for line in output.lines() {
            for part in line.split(',') {
                let part = part.trim();
                let Some(name) = part.strip_prefix("tag: ") else {
                    continue;
                };
                if crate::version::parse_tag(name, prefix).is_ok() {
                    return Some(name.to_string());
                }
            }
        }
        None
    }

    /// True when `reference` resolves to a commit.
    pub fn rev_exists(&self, reference: &str) -> bool {
        let Some(root) = self.root.as_ref() else {
            return false;
        };
        run(
            root,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{reference}^{{commit}}"),
            ],
        )
        .is_some()
    }

    /// The tracked files, relative to the repository root.
    pub fn tracked_files(&self) -> Vec<String> {
        let Some(root) = self.root.as_ref() else {
            return Vec::new();
        };
        run(root, &["ls-files"])
            .map(|text| text.lines().map(str::to_string).collect())
            .unwrap_or_default()
    }

    /// The commits of `range` and the files each one changed.
    ///
    /// The record separator keeps a commit apart from its file names; a file
    /// name is never interpolated into anything.
    pub fn commits_with_files(&self, range: &str) -> Vec<(String, Vec<String>)> {
        let Some(root) = self.root.as_ref() else {
            return Vec::new();
        };
        let output = match run(root, &["log", "--format=%x1e%H", "--name-only", range]) {
            Some(output) => output,
            None => return Vec::new(),
        };
        let mut commits = Vec::new();
        for record in output.split('\u{1e}') {
            let mut lines = record.lines().filter(|line| !line.trim().is_empty());
            let Some(commit) = lines.next() else {
                continue;
            };
            let files = lines.map(str::to_string).collect();
            commits.push((commit.to_string(), files));
        }
        commits
    }

    /// The commits since a tag, oldest first.
    ///
    /// Without a tag the whole history is returned.  The subject, the body
    /// and the date of every commit are read: the release step needs the
    /// type of the commit, its breaking marker and the date of the release.
    pub fn commits_since(&self, since: Option<&str>) -> Option<Vec<Commit>> {
        let root = self.root.as_ref()?;
        // The fields are separated by a unit separator, the records by a
        // record separator: a commit message may hold anything else.
        let format = "--format=%s%x1f%cs%x1f%an%x1f%P%x1f%b%x1e";
        let range = match since {
            Some(tag) => format!("{tag}..HEAD"),
            None => "HEAD".to_string(),
        };
        let output = run(root, &["log", "--reverse", format, &range])?;
        Some(parse_commits(&output))
    }
}

/// A commit of the history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    /// The subject of the commit.
    pub subject: String,
    /// The date of the commit (`YYYY-MM-DD`, the date of the commit itself).
    pub date: String,
    /// The body of the commit.
    pub body: String,
    /// The author name (`%an`).
    pub author: String,
    /// The number of parents: more than one means a merge commit.
    pub parents: usize,
}

/// Parses the commits of the output of `git log`.
fn parse_commits(output: &str) -> Vec<Commit> {
    let mut commits = Vec::new();
    for record in output.split('\u{1e}') {
        let record = record.trim_start_matches('\n');
        if record.trim().is_empty() {
            continue;
        }
        let mut fields = record.split('\u{1f}');
        let subject = fields.next().unwrap_or_default().to_string();
        let date = fields.next().unwrap_or_default().to_string();
        let author = fields.next().unwrap_or_default().to_string();
        let parents = fields.next().unwrap_or_default().split_whitespace().count();
        let body = fields.next().unwrap_or_default().to_string();
        commits.push(Commit {
            subject,
            date,
            author,
            parents,
            body,
        });
    }
    commits
}

/// The state of the committed tree of a repository.
enum HeadState {
    /// The repository has no commit: nothing is committed yet.
    NoCommit,
    /// The repository is damaged: the commit it points at cannot be read.
    Damaged,
    /// The committed tree can be read.
    Ready,
}

/// The state of the committed tree.
///
/// The decision is made on **the ref that `HEAD` points to**, not on the set
/// of all refs:
///
/// * a `HEAD` that names a ref which does not exist is an **unborn** `HEAD`
///   — a fresh branch — and nothing is committed on it;
/// * a `HEAD` that names a ref which exists but whose commit object cannot
///   be read is **damaged**;
/// * a **detached** `HEAD` points at a commit: the repository has one even
///   when it has no ref at all, and a detached `HEAD` that cannot be resolved
///   is damaged.
fn head_state(root: &Path) -> HeadState {
    if let Some(reference) = run(root, &["symbolic-ref", "-q", "HEAD"]) {
        let reference = reference.trim();
        // The ref decides: a ref that does not exist is an unborn HEAD.
        if !succeeds(root, &["rev-parse", "--verify", "--quiet", reference]) {
            return HeadState::NoCommit;
        }
        // The ref exists: its commit object has to be readable.
        let commit = format!("{reference}^{{commit}}");
        if !succeeds(root, &["rev-parse", "--verify", "--quiet", &commit]) {
            return HeadState::Damaged;
        }
        return HeadState::Ready;
    }

    // A detached HEAD points at a commit.
    if !succeeds(root, &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"]) {
        return HeadState::Damaged;
    }
    HeadState::Ready
}

/// Runs a git command and reports whether it succeeded.
fn succeeds(cwd: &Path, arguments: &[&str]) -> bool {
    Command::new("git")
        .args(arguments)
        .current_dir(cwd)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Runs a git command and returns its stdout.
fn run(cwd: &Path, arguments: &[&str]) -> Option<String> {
    let output = run_bytes(cwd, arguments)?;
    Some(String::from_utf8_lossy(&output).into_owned())
}

/// Runs a git command and returns its stdout as bytes.
fn run_bytes(cwd: &Path, arguments: &[&str]) -> Option<Vec<u8>> {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(output.stdout)
}
