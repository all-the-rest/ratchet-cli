//! Config discovery, parsing and validation.
//!
//! The config is a *flat* config in the ESLint shape: an array of config
//! objects that are merged.  Parsing is hand written on purpose: every error
//! has to name the offending path (`rules.my-rule.direction`), which a
//! derive-based deserializer cannot express.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

use globset::{GlobBuilder, GlobMatcher};
use serde::Serialize;
use serde_json::{Map, Value};

use crate::registry::{SEVERITIES, STATUSES, Severity, Status};

/// Config file names, in discovery order.
pub const CONFIG_FILE_NAMES: [&str; 4] = [
    "ratchet.config.jsonc",
    "ratchet.config.json",
    "ratchet.config.mjs",
    "ratchet.config.cjs",
];

/// Default baseline file name; resolved next to the config.
pub const DEFAULT_BASELINE_FILE: &str = "ratchet.baseline.json";

/// The names a `count.agent_rule` counter accepts, for error messages.
pub const AGENT_RULE_NAMES: &str = "\"rule-file-name\", \"rule-file-present\", \"plan-status-line\", \
     \"gates-untouched\", \"unverified-commits\"";

/// The presets a config object may name in `preset`.
pub const PRESET_NAMES: &str = "\"agent-rules\"";

/// The `agent-rules` preset: the canonical rule set a project adopts.
///
/// It lives in the binary on purpose. `ratchet` has no `extends` across
/// repositories — a path may not leave the repository root — so a rule set
/// that is shared between projects cannot be a file. A preset is versioned by
/// the binary that ships it, which makes "every project runs the same rules"
/// a fact about the release rather than a convention about copies.
///
/// `unverified-commits` carries no epoch here: the count over the whole
/// history still catches every new commit without a footer, because the
/// number only grows. A project that releases tags can narrow the window by
/// overriding the rule with a `since` date.
const AGENT_RULES_PRESET: &str = r#"
{
  "name": "agent-rules (preset)",
  "rules": {
    "rule-file-name": {
      "class": "a rule file the harness cannot see",
      "incident": "LuminaRust, jku-itpj2627-webshop-base and all-the-rest-skills carried `Agents.md`; tools that look up `AGENTS.md` by exact name never loaded it",
      "unit": "rule files with the wrong file name",
      "direction": "ceiling",
      "description": "counts every tracked `AGENTS.md` whose spelling differs — the comparison is case-sensitive by design",
      "count": { "agent_rule": "rule-file-name" }
    },
    "rule-file-present": {
      "class": "a project without rules",
      "incident": "noema had three workflows and a clean verification history but no project rules at all — the practice lived only in the global config",
      "unit": "root rule files",
      "direction": "floor",
      "description": "1 when `AGENTS.md` exists at the repository root, 0 when it does not",
      "count": { "agent_rule": "rule-file-present" }
    },
    "rule-file-max-lines": {
      "class": "a rule file drifting into a specification",
      "incident": "LuminaRust/Agents.md reached 623 lines and open-accreditation 380: architecture, stack and release runbook in the file every session loads",
      "unit": "lines in the longest rule file",
      "direction": "ceiling",
      "aggregate": "max",
      "description": "the largest `AGENTS.md` in the tree, measured in lines; the paydown is a module-local `AGENTS.md` next to the code it describes",
      "count": { "file_metrics": { "files": ["**/AGENTS.md"], "metric": "line_count" } }
    },
    "plan-status-line": {
      "class": "a plan without a state",
      "incident": "LuminaRust had six plan documents and no status line in two of them — 'done', 'planned' and 'recommendation' were indistinguishable",
      "unit": "plans without a status line",
      "direction": "ceiling",
      "description": "counts `docs/plans/*.md` that do not carry `**Status:**` within their first 15 lines; without a `docs/plans/` directory the count is 0",
      "count": { "agent_rule": "plan-status-line" }
    },
    "gates-untouched": {
      "class": "a gate disarmed by the change it should catch",
      "incident": "the gates run from the working tree, so one commit can weaken the check scripts and the code they guard together",
      "unit": "commits that change a gate and product code at once",
      "direction": "ceiling",
      "description": "counts the commits not yet on `origin/main` that touch a gate path and product code in the same commit; documentation and rule files do not count as product code",
      "count": { "agent_rule": "gates-untouched" }
    },
    "unverified-commits": {
      "class": "a commit with no verification evidence",
      "incident": "eight repositories carry a `Verify:` footer in 20 of 20 commits while ai-10-usd carries it in 3 of 20 — the convention exists, the enforcement does not",
      "unit": "commits without a `Verify:` footer",
      "direction": "ceiling",
      "description": "counts the commits of the history whose message has no `Verify:` line; a project with tagged releases narrows the window with `since`",
      "count": { "agent_rule": "unverified-commits" }
    }
  }
}
"#;

/// Resolves a preset name into the config object it stands for.
fn preset_value(name: &str) -> Result<Value, ConfigError> {
    match name {
        "agent-rules" => json5::from_str(AGENT_RULES_PRESET).map_err(|error| {
            ConfigError::new("preset", format!("the built-in preset is invalid: {error}"))
        }),
        other => Err(ConfigError::new(
            "preset",
            format!("unknown preset \"{other}\" — expected {PRESET_NAMES}"),
        )),
    }
}

/// Whether `text` is a calendar date in the one shape the counters accept.
fn is_iso_date(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| index == 4 || index == 7 || byte.is_ascii_digit())
}

/// A configuration error. `path` points at the offending field.
#[derive(Debug, Clone)]
pub struct ConfigError {
    /// Path of the offending field, e.g. `rules.my-rule.direction`.
    pub path: String,
    /// What is wrong with it.
    pub message: String,
    /// The raw output of a subprocess, if one was involved; the diagnostic
    /// channel carries it (SPEC 6.2).
    pub output: Option<String>,
}

impl ConfigError {
    fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
            output: None,
        }
    }

    /// A configuration error with the raw output of a subprocess.
    fn with_output(path: impl Into<String>, message: impl Into<String>, output: String) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
            output: Some(output),
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.path.is_empty() {
            write!(f, "{}", self.message)
        } else {
            write!(f, "config error at {}: {}", self.path, self.message)
        }
    }
}

impl std::error::Error for ConfigError {}

/// The direction a baseline is allowed to move in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// The count must not exceed the baseline.
    Ceiling,
    /// The count must not fall below the baseline.
    Floor,
}

impl Direction {
    /// The name used in configs and reports.
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::Ceiling => "ceiling",
            Direction::Floor => "floor",
        }
    }
}

/// The tier a rule belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    /// Seconds, no infrastructure.
    Fast,
    /// Real infrastructure.
    Slow,
}

impl Tier {
    /// The name used in configs and reports.
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Fast => "fast",
            Tier::Slow => "slow",
        }
    }
}

/// How the count of a rule is produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum Counter {
    /// An argv array; no shell is involved.
    Command {
        /// The program and its arguments.
        command: Vec<String>,
    },
    /// Built-in, language-agnostic metrics over selected repository files.
    FileMetrics { file_metrics: FileMetrics },
    /// A JSON file, optionally with a dotted path into it.
    File {
        /// The file, relative to the repository root.
        file: String,
        /// A dotted path into the JSON document.
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        /// The generator that refreshes the artifact before it is counted.
        #[serde(skip_serializing_if = "Option::is_none")]
        generate: Option<Vec<String>>,
        /// The argument the generator writes its output to.
        #[serde(skip_serializing_if = "Option::is_none")]
        output_arg: Option<String>,
    },
    /// A findings registry; the count is the number of the findings that
    /// count (SPEC 13).
    Registry {
        /// The registry file, relative to the repository root.
        registry: String,
        /// The levels that count; every level counts without the list.
        #[serde(skip_serializing_if = "Option::is_none")]
        severities: Option<Vec<Severity>>,
        /// The statuses that count; the counting statuses count without the
        /// list.
        #[serde(skip_serializing_if = "Option::is_none")]
        statuses: Option<Vec<Status>>,
    },
    /// A built-in repository check (SPEC 3.8). The processes are shipped with
    /// the binary, so a rule set that uses them is portable across
    /// repositories without carrying a script.
    AgentRule {
        /// Which check to run.
        agent_rule: AgentRule,
        /// The epoch of a counting window, `YYYY-MM-DD`; only read by the
        /// rules that count history.
        #[serde(skip_serializing_if = "Option::is_none")]
        since: Option<String>,
    },
    /// The findings of a GitHub repository; the count is the number of the
    /// issues that count (SPEC 13).
    Issues {
        /// The repository, in the form `owner/repo`.
        repository: String,
        /// The label that makes an issue a finding of the gate.
        label: String,
        /// The prefix of the label that carries the severity.
        severity_prefix: String,
        /// The severity of an issue without a severity label.
        default_severity: Severity,
        /// The levels that count; every level counts without the list.
        #[serde(skip_serializing_if = "Option::is_none")]
        severities: Option<Vec<Severity>>,
        /// The statuses that count; the counting statuses count without the
        /// list.
        #[serde(skip_serializing_if = "Option::is_none")]
        statuses: Option<Vec<Status>>,
    },
}

/// A built-in metric measured independently for each file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileMetric {
    SizeBytes,
    LineCount,
    MaxLineLength,
}

/// A check that ships with the binary (SPEC 3.8).
///
/// Every variant answers a question about the repository that git and the
/// working tree can answer without a project-owned script: no script to copy,
/// no path to resolve, no drift between a rule and its counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentRule {
    /// Tracked files named like `AGENTS.md` but spelled differently.
    RuleFileName,
    /// Whether the repository has a root `AGENTS.md`.
    RuleFilePresent,
    /// Plans under `docs/plans/` without a `**Status:**` line near the top.
    PlanStatusLine,
    /// Commits that change a gate and product code at once.
    GatesUntouched,
    /// Commits whose message carries no `Verify:` footer.
    UnverifiedCommits,
}

impl AgentRule {
    /// The name used in configs and reports.
    pub fn as_str(self) -> &'static str {
        match self {
            AgentRule::RuleFileName => "rule-file-name",
            AgentRule::RuleFilePresent => "rule-file-present",
            AgentRule::PlanStatusLine => "plan-status-line",
            AgentRule::GatesUntouched => "gates-untouched",
            AgentRule::UnverifiedCommits => "unverified-commits",
        }
    }

    /// Whether the check reads the commit history — and therefore accepts a
    /// `since` epoch.
    pub fn counts_history(self) -> bool {
        matches!(self, AgentRule::UnverifiedCommits)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileMetrics {
    pub files: Vec<String>,
    pub metric: FileMetric,
}

impl Counter {
    /// A short human readable description, used by `--debug`.
    pub fn describe(&self) -> String {
        match self {
            Counter::Command { command } => command.join(" "),
            Counter::FileMetrics { file_metrics } => {
                format!(
                    "file_metrics {:?} ({:?})",
                    file_metrics.files, file_metrics.metric
                )
            }
            Counter::AgentRule { agent_rule, since } => match since {
                Some(since) => format!("agent_rule {} since {since}", agent_rule.as_str()),
                None => format!("agent_rule {}", agent_rule.as_str()),
            },
            Counter::File {
                file,
                path,
                generate,
                ..
            } => {
                let mut text = file.clone();
                if let Some(path) = path {
                    text.push('#');
                    text.push_str(path);
                }
                if let Some(generate) = generate {
                    text.push_str(" (generated by ");
                    text.push_str(&generate.join(" "));
                    text.push(')');
                }
                text
            }
            Counter::Registry {
                registry,
                severities,
                statuses,
            } => {
                let mut text = registry.clone();
                let mut filters: Vec<String> = Vec::new();
                if let Some(severities) = severities {
                    filters.push(format!(
                        "severities: {}",
                        severities
                            .iter()
                            .map(|severity| severity.as_str())
                            .collect::<Vec<&str>>()
                            .join(", ")
                    ));
                }
                if let Some(statuses) = statuses {
                    filters.push(format!(
                        "statuses: {}",
                        statuses
                            .iter()
                            .map(|status| status.as_str())
                            .collect::<Vec<&str>>()
                            .join(", ")
                    ));
                }
                if !filters.is_empty() {
                    text.push_str(" (");
                    text.push_str(&filters.join("; "));
                    text.push(')');
                }
                text
            }
            Counter::Issues {
                repository,
                label,
                severities,
                statuses,
                ..
            } => {
                let mut text = format!("{repository} (label: {label}");
                if let Some(severities) = severities {
                    text.push_str(&format!(
                        "; severities: {}",
                        severities
                            .iter()
                            .map(|severity| severity.as_str())
                            .collect::<Vec<&str>>()
                            .join(", ")
                    ));
                }
                if let Some(statuses) = statuses {
                    text.push_str(&format!(
                        "; statuses: {}",
                        statuses
                            .iter()
                            .map(|status| status.as_str())
                            .collect::<Vec<&str>>()
                            .join(", ")
                    ));
                }
                text.push(')');
                text
            }
        }
    }
}

/// One gate, one defect class.
#[derive(Debug, Clone, Serialize)]
pub struct Rule {
    /// The rule id (the key in the `rules` object).
    #[serde(skip)]
    pub id: String,
    /// The defect class.
    pub class: String,
    /// The incident that motivated the rule.
    ///
    /// For a class that has already occurred, the incident names that
    /// concrete instance.  For a prevention rule — a class whose count is zero
    /// today — it says so plainly (SPEC §3.2).
    pub incident: String,
    /// What one count unit is.
    pub unit: String,
    /// The allowed direction of the baseline.
    pub direction: Direction,
    /// The tier of the rule.
    pub tier: Tier,
    /// How the count is produced.
    pub count: Counter,
    /// Glob patterns removed from the counted items.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    /// Glob scope of the rule; without it the rule always runs.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    /// Free text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// How per-file measurements reduce to a scoped value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aggregate: Option<Aggregate>,
    /// Named path scopes. Exact-file scopes take precedence over glob scopes.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<PathScope>,
    /// The compiled `exclude` patterns, one group per configured pattern.
    ///
    /// A pattern can expand into more than one matcher (`src/**` also matches
    /// the directory `src`), so a flat vector would not be aligned with
    /// `exclude`.  One inner vector per original pattern keeps the two in
    /// step: the index of a group is the index of the pattern.
    #[serde(skip)]
    pub exclude_globs: Vec<Vec<GlobMatcher>>,
    /// Compiled `files` patterns.
    #[serde(skip)]
    pub file_globs: Vec<GlobMatcher>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Aggregate {
    Count,
    Max,
}

#[derive(Debug, Clone, Serialize)]
pub struct PathScope {
    pub id: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    #[serde(skip)]
    pub globs: Vec<GlobMatcher>,
}

impl Rule {
    /// The compiled scope of the rule: the paths it covers.
    ///
    /// A rule without a `files` scope covers every path.
    pub fn in_scope(&self, path: &str) -> bool {
        self.file_globs.is_empty() || self.file_globs.iter().any(|glob| glob.is_match(path))
    }
}

/// Per tier settings.
#[derive(Debug, Clone, Default, Serialize)]
pub struct TierSettings {
    /// The minimum number of checks that must execute in the tier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub floor: Option<i64>,
}

/// A drift gate entry: a generated file and the generator that produces it.
#[derive(Debug, Clone, Serialize)]
pub struct Generated {
    /// The generated file, relative to the repository root.
    pub path: String,
    /// The generator command.
    pub command: Vec<String>,
    /// The argument the generator writes its output to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_arg: Option<String>,
    /// The tier of the drift gate; `fast` by default.
    ///
    /// A generator that needs a database, a browser or the network belongs to
    /// the slow tier: a fast tier that waits for it is a fast tier that is
    /// not fast (SPEC §7).
    pub tier: Tier,
}

/// A pre-release check: a command whose output is compared with a committed
/// snapshot (SPEC §15).
///
/// A benchmark is a measurement, not a defect class: it has no meaningful
/// "baseline that may only sink".  The value moves in both directions and
/// only matters at the moment of a release, so the check has **no direction**.
#[derive(Debug, Clone, Serialize)]
pub struct Verify {
    /// The name of the check; it identifies the entry in the report.
    pub name: String,
    /// The command that produces the output.
    pub command: Vec<String>,
    /// The committed snapshot, relative to the repository root.
    pub snapshot: String,
    /// The argument the command writes its output to.
    ///
    /// The argv gets the temporary output path appended — the flag, then the
    /// path — so the command writes where the tool tells it and the tree
    /// stays untouched (SPEC §15).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_arg: Option<String>,
}

/// The release step and the changelog (SPEC §16).
///
/// The version and the changelog are derived facts: both are determined by
/// the commit history.  The project owns the **paths** — where the version
/// lives, where the changelog lives — and the engine owns the mechanic.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Release {
    /// The path of the changelog file, relative to the repository root.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changelog: Option<String>,
    /// The first version the tool manages.
    ///
    /// Below that version the changelog is written by hand: the tool refuses
    /// to write a block for a version below it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changelog_from: Option<String>,
    /// The prefix of the tag of a release.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag_prefix: Option<String>,
    /// Where the version of the project lives.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<VersionFile>,
    /// The mapping from the commit type to the version step.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub steps: BTreeMap<String, String>,
}

/// Where the version of a project lives.
///
/// The format is the shape of the file, not the language of the project: the
/// same tool writes the version of a Rust, a Node, a Python or a Go project
/// — the project owns the file, the engine owns the edit.
#[derive(Debug, Clone, Serialize)]
pub struct VersionFile {
    /// The path of the file, relative to the repository root.
    pub path: String,
    /// The shape of the file.
    pub format: VersionFormat,
    /// The section of the key (`toml`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub section: Option<String>,
    /// The key of the version (`toml`, `json`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// The prefix of the line (`line`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    /// The suffix of the line (`line`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suffix: Option<String>,
}

/// The shape of the file that carries the version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VersionFormat {
    /// A TOML file: a section and a key (`Cargo.toml`, `pyproject.toml`).
    Toml,
    /// A JSON file: a key of the root object (`package.json`).
    Json,
    /// A line of a text file: a prefix and a suffix (`Makefile`).
    Line,
}

/// A merged config.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Config {
    /// Optional label of the config.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Per tier settings.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub tiers: BTreeMap<Tier, TierSettings>,
    /// Drift gate entries.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub generated: Vec<Generated>,
    /// The pre-release checks.
    ///
    /// They run in the release pipeline, not in the commit gate: a measurement
    /// is not a defect class (SPEC §15).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub verify: Vec<Verify>,
    /// The rules, sorted by id.
    pub rules: BTreeMap<String, Rule>,
    /// The baseline path, relative to the config.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline: Option<String>,
    /// The release step and the changelog.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release: Option<Release>,
}

/// Parses a JSONC (or plain JSON) config.
pub fn parse_config_str(text: &str) -> Result<Config, ConfigError> {
    let value: Value =
        json5::from_str(text).map_err(|e| ConfigError::new("", format!("invalid JSON: {e}")))?;
    parse_config_value(&value)
}

/// Validates and merges a config document.
pub fn parse_config_value(value: &Value) -> Result<Config, ConfigError> {
    let objects: Vec<&Value> = match value {
        Value::Array(items) => items.iter().collect(),
        Value::Object(_) => vec![value],
        other => {
            return Err(ConfigError::new(
                "",
                format!(
                    "expected an array of config objects, found {}",
                    type_name(other)
                ),
            ));
        }
    };

    let mut config = Config::default();
    for object in objects {
        merge_object(&mut config, object)?;
    }
    Ok(config)
}

/// Evaluates a JavaScript config with `node` and parses its default export.
pub fn load_js_config(path: &Path) -> Result<Config, ConfigError> {
    let absolute = absolute_path(path);
    let name = file_name(path);

    // On Unix, backslash is a legal filename character, but Node's file-URL
    // module loader rejects its percent-encoded form. Windows paths use
    // backslashes as separators and are supported by `pathToFileURL`.
    #[cfg(unix)]
    if absolute.to_string_lossy().contains('\\') {
        return Err(ConfigError::new(
            "",
            format!(
                "{name} cannot be evaluated: the path contains a backslash, \
                 which the JavaScript bridge cannot load — use ratchet.config.jsonc instead"
            ),
        ));
    }

    // The path is resolved as a file URL: a path that contains `#`, `?` or `%`
    // is not a valid URL specifier.  The path itself is passed as one argv
    // element, it is never interpolated into a shell.
    let script = format!(
        "const {{ pathToFileURL }} = await import('node:url'); \
         const module = await import(pathToFileURL({}).href); \
         process.stdout.write(JSON.stringify(module.default ?? module));",
        serde_json::to_string(&absolute.to_string_lossy()).unwrap_or_else(|_| "\"\"".to_string())
    );

    let output = Command::new("node")
        .arg("--input-type=module")
        .arg("-e")
        .arg(&script)
        .current_dir(absolute.parent().unwrap_or(Path::new(".")))
        .output();

    let output = match output {
        Ok(output) => output,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ConfigError::new(
                "",
                format!(
                    "node was not found on PATH — {name} cannot be evaluated, use ratchet.config.jsonc instead"
                ),
            ));
        }
        Err(e) => {
            return Err(ConfigError::new("", format!("cannot run node: {e}")));
        }
    };

    // The transcript of node: the diagnostic channel carries the raw output of
    // the subprocess — both streams, in one shape (SPEC 6.2).
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let status = match output.status.code() {
        Some(code) => format!("status {code}"),
        None => "an unknown status".to_string(),
    };
    let transcript = format!("node: {status}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}");

    if !output.status.success() {
        // Node prints the absolute path of the config and the working
        // directory.  That belongs to the diagnostic channel, never to the
        // report (SPEC 6.2).
        return Err(ConfigError::with_output(
            "",
            format!("{name} could not be evaluated (node exited with {status})"),
            transcript,
        ));
    }

    if stdout.trim().is_empty() {
        return Err(ConfigError::with_output(
            "",
            format!("{name} must export a config object"),
            transcript,
        ));
    }

    // The export is not valid JSON: node's output belongs to the diagnostic
    // channel (SPEC 6.2), the report says what happened (SPEC 6.2).
    let value: Value = serde_json::from_str(stdout.trim()).map_err(|e| {
        ConfigError::with_output(
            "",
            format!("{name} did not export valid JSON: {e}"),
            transcript,
        )
    })?;
    parse_config_value(&value)
}

/// The file name of a path; error messages stay free of absolute paths.
fn file_name(path: &Path) -> String {
    crate::path::file_name(path)
}

/// The absolute path of `path`, without resolving symlinks.
fn absolute_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

fn merge_object(config: &mut Config, value: &Value) -> Result<(), ConfigError> {
    let map = expect_object(value, "")?;
    check_unknown(
        map,
        &[
            "name",
            "preset",
            "tiers",
            "generated",
            "verify",
            "rules",
            "baseline",
            "release",
        ],
        "",
    )?;

    // The preset is the base of this object: it is merged first, so every
    // field the object carries itself wins over the preset.
    if let Some(preset) = map.get("preset") {
        let name = expect_string(preset, "preset")?;
        merge_object(config, &preset_value(&name)?)?;
    }

    if let Some(name) = map.get("name") {
        config.name = Some(expect_string(name, "name")?);
    }
    if let Some(baseline) = map.get("baseline") {
        config.baseline = Some(expect_string(baseline, "baseline")?);
    }
    if let Some(tiers) = map.get("tiers") {
        merge_tiers(config, tiers)?;
    }
    if let Some(generated) = map.get("generated") {
        merge_generated(config, generated)?;
    }
    if let Some(verify) = map.get("verify") {
        merge_verify(config, verify)?;
    }
    if let Some(rules) = map.get("rules") {
        merge_rules(config, rules)?;
    }
    if let Some(release) = map.get("release") {
        merge_release(config, release)?;
    }
    Ok(())
}

/// Merges the release settings (SPEC §16).
fn merge_release(config: &mut Config, value: &Value) -> Result<(), ConfigError> {
    let map = expect_object(value, "release")?;
    check_unknown(
        map,
        &[
            "changelog",
            "changelogFrom",
            "tagPrefix",
            "version",
            "steps",
        ],
        "release",
    )?;

    let mut release = config.release.clone().unwrap_or_default();

    if let Some(changelog) = map.get("changelog") {
        let path = expect_string(changelog, "release.changelog")?;
        if path.trim().is_empty() {
            return Err(ConfigError::new("release.changelog", "must not be empty"));
        }
        release.changelog = Some(path);
    }
    if let Some(from) = map.get("changelogFrom") {
        let version = expect_string(from, "release.changelogFrom")?;
        if version.trim().is_empty() {
            return Err(ConfigError::new(
                "release.changelogFrom",
                "must not be empty",
            ));
        }
        release.changelog_from = Some(version);
    }
    if let Some(prefix) = map.get("tagPrefix") {
        release.tag_prefix = Some(expect_string(prefix, "release.tagPrefix")?);
    }
    if let Some(steps) = map.get("steps") {
        let step_map = expect_object(steps, "release.steps")?;
        for (kind, step) in step_map {
            let path = format!("release.steps.{kind}");
            let text = expect_string(step, &path)?;
            // The step has to be a step: a typo in the configuration would
            // otherwise be a version that moves nowhere.
            crate::version::Step::parse(&text)
                .map_err(|message| ConfigError::new(&path, message))?;
            release.steps.insert(kind.clone(), text);
        }
    }
    if let Some(version) = map.get("version") {
        release.version = Some(parse_version_file(version)?);
    }

    config.release = Some(release);
    Ok(())
}

/// Parses the place the version of the project lives.
fn parse_version_file(value: &Value) -> Result<VersionFile, ConfigError> {
    let path = "release.version";
    let map = expect_object(value, path)?;
    check_unknown(
        map,
        &["path", "format", "section", "key", "prefix", "suffix"],
        path,
    )?;

    let file = require_string(map, "path", path)?;
    if file.trim().is_empty() {
        return Err(ConfigError::new(
            format!("{path}.path"),
            "must not be empty",
        ));
    }
    let format = match map.get("format") {
        None => {
            return Err(ConfigError::new(
                format!("{path}.format"),
                "must not be empty — expected \"toml\", \"json\" or \"line\"",
            ));
        }
        Some(value) => match expect_string(value, &format!("{path}.format"))?.as_str() {
            "toml" => VersionFormat::Toml,
            "json" => VersionFormat::Json,
            "line" => VersionFormat::Line,
            other => {
                return Err(ConfigError::new(
                    format!("{path}.format"),
                    format!("expected \"toml\", \"json\" or \"line\", found \"{other}\""),
                ));
            }
        },
    };

    let section = optional_string(map, "section", path)?;
    let key = optional_string(map, "key", path)?;
    let prefix = optional_string(map, "prefix", path)?;
    let suffix = optional_string(map, "suffix", path)?;

    // The fields of the format are required: a version file without them
    // cannot be read, and a tool that guessed would write the version into a
    // file that does not carry one.
    let missing = |field: &str| {
        Err(ConfigError::new(
            format!("{path}.{field}"),
            format!("the format \"{}\" needs it", format_name(format)),
        ))
    };
    match format {
        VersionFormat::Toml => {
            if section.is_none() {
                return missing("section");
            }
            if key.is_none() {
                return missing("key");
            }
        }
        VersionFormat::Json => {
            if key.is_none() {
                return missing("key");
            }
        }
        VersionFormat::Line => {
            if prefix.is_none() {
                return missing("prefix");
            }
            if suffix.is_none() {
                return missing("suffix");
            }
        }
    }

    Ok(VersionFile {
        path: file,
        format,
        section,
        key,
        prefix,
        suffix,
    })
}

/// The name of a format, for the messages.
fn format_name(format: VersionFormat) -> &'static str {
    match format {
        VersionFormat::Toml => "toml",
        VersionFormat::Json => "json",
        VersionFormat::Line => "line",
    }
}

fn merge_tiers(config: &mut Config, value: &Value) -> Result<(), ConfigError> {
    let map = expect_object(value, "tiers")?;
    for (name, settings) in map {
        let tier = match name.as_str() {
            "fast" => Tier::Fast,
            "slow" => Tier::Slow,
            other => {
                return Err(ConfigError::new(
                    format!("tiers.{other}"),
                    "expected \"fast\" or \"slow\"",
                ));
            }
        };
        let path = format!("tiers.{name}");
        let settings_map = expect_object(settings, &path)?;
        check_unknown(settings_map, &["floor"], &path)?;
        let floor = match settings_map.get("floor") {
            None => None,
            Some(floor) => Some(expect_integer(floor, &format!("{path}.floor"))?),
        };
        config.tiers.insert(tier, TierSettings { floor });
    }
    Ok(())
}

fn merge_generated(config: &mut Config, value: &Value) -> Result<(), ConfigError> {
    let items = expect_array(value, "generated")?;
    for (index, item) in items.iter().enumerate() {
        let path = format!("generated[{index}]");
        let map = expect_object(item, &path)?;
        check_unknown(map, &["path", "command", "output_arg", "tier"], &path)?;
        let file = require_string(map, "path", &path)?;
        let command = require_string_array(map, "command", &path)?;
        if command.is_empty() {
            return Err(ConfigError::new(
                format!("{path}.command"),
                "must not be empty",
            ));
        }
        let output_arg = match map.get("output_arg") {
            None => None,
            Some(value) => Some(expect_string(value, &format!("{path}.output_arg"))?),
        };
        let tier = match map.get("tier") {
            None => Tier::Fast,
            Some(value) => match expect_string(value, &format!("{path}.tier"))?.as_str() {
                "fast" => Tier::Fast,
                "slow" => Tier::Slow,
                other => {
                    return Err(ConfigError::new(
                        format!("{path}.tier"),
                        format!("expected \"fast\" or \"slow\", found \"{other}\""),
                    ));
                }
            },
        };
        let entry = Generated {
            path: file,
            command,
            output_arg,
            tier,
        };
        // A later entry for the same path overrides the earlier one.
        match config.generated.iter_mut().find(|g| g.path == entry.path) {
            Some(existing) => *existing = entry,
            None => config.generated.push(entry),
        }
    }
    Ok(())
}

/// Merges the pre-release checks (SPEC §15).
fn merge_verify(config: &mut Config, value: &Value) -> Result<(), ConfigError> {
    let items = expect_array(value, "verify")?;
    // The names of this array: a name twice in one array is a mistake, and a
    // mistake that silently drops a check is a false green.
    let mut names: Vec<String> = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let path = format!("verify[{index}]");
        let map = expect_object(item, &path)?;
        check_unknown(map, &["name", "command", "snapshot", "output_arg"], &path)?;

        let name = require_string(map, "name", &path)?;
        if name.trim().is_empty() {
            return Err(ConfigError::new(
                format!("{path}.name"),
                "must not be empty",
            ));
        }
        if names.contains(&name) {
            return Err(ConfigError::new(
                format!("{path}.name"),
                format!("the name \"{name}\" is used twice — every check has its own name"),
            ));
        }
        names.push(name.clone());
        let command = require_string_array(map, "command", &path)?;
        if command.is_empty() {
            return Err(ConfigError::new(
                format!("{path}.command"),
                "must not be empty",
            ));
        }
        let snapshot = require_string(map, "snapshot", &path)?;
        if snapshot.trim().is_empty() {
            return Err(ConfigError::new(
                format!("{path}.snapshot"),
                "must not be empty",
            ));
        }
        let output_arg = match map.get("output_arg") {
            None => None,
            Some(value) => Some(expect_string(value, &format!("{path}.output_arg"))?),
        };

        // The placeholder in the argv and the `output_arg` are two ways to
        // name the output path.  A command that uses both does not say where
        // its output goes, and the tool would tell it twice.
        if output_arg.is_some()
            && command
                .iter()
                .any(|part| part.contains(crate::verify::PLACEHOLDER))
        {
            return Err(ConfigError::new(
                format!("{path}.output_arg"),
                format!(
                    "the command carries the {} placeholder and the entry declares an \
                     `output_arg` — one of them names the output path",
                    crate::verify::PLACEHOLDER
                ),
            ));
        }

        let entry = Verify {
            name,
            command,
            snapshot,
            output_arg,
        };
        // A later config object overrides an earlier entry with the same name.
        match config.verify.iter_mut().find(|v| v.name == entry.name) {
            Some(existing) => *existing = entry,
            None => config.verify.push(entry),
        }
    }
    Ok(())
}

fn merge_rules(config: &mut Config, value: &Value) -> Result<(), ConfigError> {
    let map = expect_object(value, "rules")?;
    for (id, rule) in map {
        // A later config object overrides an earlier one for the same rule id.
        config.rules.insert(id.clone(), parse_rule(id, rule)?);
    }
    Ok(())
}

fn parse_rule(id: &str, value: &Value) -> Result<Rule, ConfigError> {
    let path = format!("rules.{id}");
    let map = expect_object(value, &path)?;
    check_unknown(
        map,
        &[
            "class",
            "incident",
            "unit",
            "direction",
            "tier",
            "count",
            "exclude",
            "files",
            "description",
            "aggregate",
            "scopes",
        ],
        &path,
    )?;

    let class = require_string(map, "class", &path)?;
    let incident = require_string(map, "incident", &path)?;
    let unit = require_string(map, "unit", &path)?;
    // An empty string is a missing value that happens to pass the type check.
    for (field, value) in [("class", &class), ("incident", &incident), ("unit", &unit)] {
        if value.trim().is_empty() {
            return Err(ConfigError::new(
                format!("{path}.{field}"),
                "must not be empty",
            ));
        }
    }
    let direction = match require_string(map, "direction", &path)?.as_str() {
        "ceiling" => Direction::Ceiling,
        "floor" => Direction::Floor,
        other => {
            return Err(ConfigError::new(
                format!("{path}.direction"),
                format!("expected \"ceiling\" or \"floor\", found \"{other}\""),
            ));
        }
    };
    let tier = match map.get("tier") {
        None => Tier::Fast,
        Some(value) => match expect_string(value, &format!("{path}.tier"))?.as_str() {
            "fast" => Tier::Fast,
            "slow" => Tier::Slow,
            other => {
                return Err(ConfigError::new(
                    format!("{path}.tier"),
                    format!("expected \"fast\" or \"slow\", found \"{other}\""),
                ));
            }
        },
    };
    let count = match map.get("count") {
        None => {
            return Err(ConfigError::new(
                format!("{path}.count"),
                "missing required field",
            ));
        }
        Some(value) => parse_counter(value, &format!("{path}.count"))?,
    };
    if matches!(&count, Counter::Issues { .. }) && tier != Tier::Slow {
        return Err(ConfigError::new(
            format!("{path}.tier"),
            "issues counters require tier \"slow\" because they make network requests",
        ));
    }
    let exclude = optional_string_array(map, "exclude", &path)?;
    let files = optional_string_array(map, "files", &path)?;
    let description = match map.get("description") {
        None => None,
        Some(value) => Some(expect_string(value, &format!("{path}.description"))?),
    };
    if description
        .as_deref()
        .is_some_and(|text| text.trim().is_empty())
    {
        return Err(ConfigError::new(
            format!("{path}.description"),
            "must not be empty",
        ));
    }
    let aggregate = match map.get("aggregate") {
        None => None,
        Some(value) => Some(
            match expect_string(value, &format!("{path}.aggregate"))?.as_str() {
                "count" => Aggregate::Count,
                "max" => Aggregate::Max,
                other => {
                    return Err(ConfigError::new(
                        format!("{path}.aggregate"),
                        format!("expected \"count\" or \"max\", found \"{other}\""),
                    ));
                }
            },
        ),
    };
    let scopes = parse_scopes(map.get("scopes"), &format!("{path}.scopes"))?;
    if matches!(count, Counter::FileMetrics { .. }) && aggregate.is_none() {
        return Err(ConfigError::new(
            format!("{path}.aggregate"),
            "is required for file_metrics counters",
        ));
    }
    if !scopes.is_empty() && aggregate.is_none() {
        return Err(ConfigError::new(
            format!("{path}.aggregate"),
            "is required when scopes are configured",
        ));
    }
    if (!scopes.is_empty() || aggregate.is_some())
        && !matches!(count, Counter::Command { .. } | Counter::FileMetrics { .. })
    {
        return Err(ConfigError::new(
            format!("{path}.count"),
            "scoped metrics require per-file measurements",
        ));
    }
    let exclude_globs = compile_globs_grouped(&exclude, &format!("{path}.exclude"))?;
    let file_globs = compile_globs(&files, &format!("{path}.files"))?;

    Ok(Rule {
        id: id.to_string(),
        class,
        incident,
        unit,
        direction,
        tier,
        count,
        exclude,
        files,
        description,
        aggregate,
        scopes,
        exclude_globs,
        file_globs,
    })
}

fn parse_counter(value: &Value, path: &str) -> Result<Counter, ConfigError> {
    let map = expect_object(value, path)?;

    // Exactly one form per counter: a counter that says two things at once
    // does not say which one counts.
    let forms = [
        "command",
        "file",
        "registry",
        "issues",
        "file_metrics",
        "agent_rule",
    ];
    let present: Vec<&str> = forms
        .iter()
        .copied()
        .filter(|key| map.contains_key(*key))
        .collect();
    match present.as_slice() {
        ["command"] => {
            check_unknown(map, &["command"], path)?;
            let command = require_string_array(map, "command", path)?;
            if command.is_empty() {
                return Err(ConfigError::new(
                    format!("{path}.command"),
                    "must not be empty",
                ));
            }
            Ok(Counter::Command { command })
        }
        ["file_metrics"] => {
            check_unknown(map, &["file_metrics"], path)?;
            let metrics = expect_object(&map["file_metrics"], &format!("{path}.file_metrics"))?;
            check_unknown(
                metrics,
                &["files", "metric"],
                &format!("{path}.file_metrics"),
            )?;
            let files = require_string_array(metrics, "files", &format!("{path}.file_metrics"))?;
            if files.is_empty() {
                return Err(ConfigError::new(
                    format!("{path}.file_metrics.files"),
                    "must not be empty",
                ));
            }
            compile_globs(&files, &format!("{path}.file_metrics.files"))?;
            let metric = match require_string(metrics, "metric", &format!("{path}.file_metrics"))?
                .as_str()
            {
                "size_bytes" => FileMetric::SizeBytes,
                "line_count" => FileMetric::LineCount,
                "max_line_length" => FileMetric::MaxLineLength,
                other => {
                    return Err(ConfigError::new(
                        format!("{path}.file_metrics.metric"),
                        format!("unknown file metric \"{other}\""),
                    ));
                }
            };
            Ok(Counter::FileMetrics {
                file_metrics: FileMetrics { files, metric },
            })
        }
        ["agent_rule"] => {
            check_unknown(map, &["agent_rule", "since"], path)?;
            let name = require_string(map, "agent_rule", path)?;
            let agent_rule = match name.as_str() {
                "rule-file-name" => AgentRule::RuleFileName,
                "rule-file-present" => AgentRule::RuleFilePresent,
                "plan-status-line" => AgentRule::PlanStatusLine,
                "gates-untouched" => AgentRule::GatesUntouched,
                "unverified-commits" => AgentRule::UnverifiedCommits,
                other => {
                    return Err(ConfigError::new(
                        format!("{path}.agent_rule"),
                        format!(
                            "unknown agent rule \"{other}\" — expected one of {}",
                            AGENT_RULE_NAMES
                        ),
                    ));
                }
            };
            let since = match map.get("since") {
                None => None,
                Some(value) => {
                    let since = expect_string(value, &format!("{path}.since"))?;
                    if !agent_rule.counts_history() {
                        return Err(ConfigError::new(
                            format!("{path}.since"),
                            format!(
                                "\"{}\" does not read the commit history, so it takes no epoch",
                                agent_rule.as_str()
                            ),
                        ));
                    }
                    if !is_iso_date(&since) {
                        return Err(ConfigError::new(
                            format!("{path}.since"),
                            format!("expected a date as YYYY-MM-DD, found \"{since}\""),
                        ));
                    }
                    Some(since)
                }
            };
            Ok(Counter::AgentRule { agent_rule, since })
        }
        ["file"] => {
            check_unknown(map, &["file", "path", "generate", "output_arg"], path)?;
            let file = require_string(map, "file", path)?;
            let json_path = match map.get("path") {
                None => None,
                Some(value) => Some(expect_string(value, &format!("{path}.path"))?),
            };
            let generate = match map.get("generate") {
                None => None,
                Some(value) => {
                    let items = expect_array(value, &format!("{path}.generate"))?;
                    let mut command = Vec::with_capacity(items.len());
                    for (index, item) in items.iter().enumerate() {
                        command.push(expect_string(item, &format!("{path}.generate[{index}]"))?);
                    }
                    if command.is_empty() {
                        return Err(ConfigError::new(
                            format!("{path}.generate"),
                            "must not be empty",
                        ));
                    }
                    Some(command)
                }
            };
            let output_arg = match map.get("output_arg") {
                None => None,
                Some(value) => Some(expect_string(value, &format!("{path}.output_arg"))?),
            };
            if output_arg.is_some() && generate.is_none() {
                return Err(ConfigError::new(
                    format!("{path}.output_arg"),
                    "requires \"generate\"",
                ));
            }
            if generate.is_some() && output_arg.is_none() {
                // A generator that cannot write to a given path would have to
                // write into the tree — and the tree is not the engine's to
                // write (SPEC 3.4).
                return Err(ConfigError::new(
                    format!("{path}.output_arg"),
                    "is required by \"generate\" — the generator writes to a temporary path",
                ));
            }
            Ok(Counter::File {
                file,
                path: json_path,
                generate,
                output_arg,
            })
        }
        ["registry"] => {
            check_unknown(map, &["registry", "severities", "statuses"], path)?;
            let registry = require_string(map, "registry", path)?;
            if registry.trim().is_empty() {
                return Err(ConfigError::new(
                    format!("{path}.registry"),
                    "must not be empty",
                ));
            }
            let severities = optional_severities(map, "severities", path)?;
            let statuses = optional_statuses(map, "statuses", path)?;
            Ok(Counter::Registry {
                registry,
                severities,
                statuses,
            })
        }
        ["issues"] => parse_issues_counter(map, path),
        _ => Err(ConfigError::new(
            path,
            "expected exactly one of \"command\", \"file\", \"registry\", \"issues\" or \"file_metrics\"",
        )),
    }
}

fn parse_scopes(value: Option<&Value>, path: &str) -> Result<Vec<PathScope>, ConfigError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let entries = expect_array(value, path)?;
    let mut scopes = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let at = format!("{path}[{index}]");
        let map = expect_object(entry, &at)?;
        check_unknown(map, &["id", "description", "file", "files"], &at)?;
        let id = require_string(map, "id", &at)?;
        let description = require_string(map, "description", &at)?;
        if id.trim().is_empty() || description.trim().is_empty() {
            return Err(ConfigError::new(
                &at,
                "id and description must not be empty",
            ));
        }
        if scopes.iter().any(|scope: &PathScope| scope.id == id) {
            return Err(ConfigError::new(
                format!("{at}.id"),
                format!("duplicate scope id \"{id}\""),
            ));
        }
        let file = optional_string(map, "file", &at)?;
        let files = optional_string_array(map, "files", &at)?;
        if file.is_some() == !files.is_empty() {
            return Err(ConfigError::new(
                &at,
                "specify exactly one of file or files",
            ));
        }
        let selectors = file.iter().cloned().chain(files.iter().cloned());
        for selector in selectors {
            let path = Path::new(&selector);
            if path.is_absolute()
                || selector.contains('\\')
                || path.components().any(|part| {
                    matches!(
                        part,
                        std::path::Component::ParentDir
                            | std::path::Component::RootDir
                            | std::path::Component::Prefix(_)
                    )
                })
            {
                return Err(ConfigError::new(
                    &at,
                    "scope paths must be repository-relative",
                ));
            }
        }
        if let Some(exact) = file.as_deref() {
            if scopes
                .iter()
                .any(|scope: &PathScope| scope.file.as_deref() == Some(exact))
            {
                return Err(ConfigError::new(
                    format!("{at}.file"),
                    format!("duplicate exact-file scope \"{exact}\""),
                ));
            }
        }
        let globs = compile_globs(&files, &format!("{at}.files"))?;
        scopes.push(PathScope {
            id,
            description,
            file,
            files,
            globs,
        });
    }
    Ok(scopes)
}

/// The `issues` counter: the findings of a GitHub repository (SPEC 13).
fn parse_issues_counter(map: &Map<String, Value>, path: &str) -> Result<Counter, ConfigError> {
    check_unknown(map, &["issues", "severities", "statuses"], path)?;
    let issues_path = format!("{path}.issues");
    let value = map
        .get("issues")
        .ok_or_else(|| ConfigError::new(&issues_path, "missing required field"))?;
    let issues = expect_object(value, &issues_path)?;
    check_unknown(
        issues,
        &["repository", "label", "severity_prefix", "default_severity"],
        &issues_path,
    )?;

    let repository = require_string(issues, "repository", &issues_path)?;
    if !is_repository(&repository) {
        return Err(ConfigError::new(
            format!("{issues_path}.repository"),
            format!("expected \"owner/repo\", found \"{repository}\""),
        ));
    }

    let label = optional_string(issues, "label", &issues_path)?
        .unwrap_or_else(|| crate::issues::DEFAULT_LABEL.to_string());
    if label.trim().is_empty() {
        return Err(ConfigError::new(
            format!("{issues_path}.label"),
            "must not be empty",
        ));
    }
    // The API takes the labels of the query comma separated: a comma inside
    // the label would select two labels and the count would be of another
    // set than the one the config names.
    if label.contains(',') {
        return Err(ConfigError::new(
            format!("{issues_path}.label"),
            "must not contain a comma — the API separates labels with it",
        ));
    }

    let severity_prefix = optional_string(issues, "severity_prefix", &issues_path)?
        .unwrap_or_else(|| crate::issues::DEFAULT_SEVERITY_PREFIX.to_string());
    if severity_prefix.is_empty() {
        return Err(ConfigError::new(
            format!("{issues_path}.severity_prefix"),
            "must not be empty",
        ));
    }

    let default_severity = match issues.get("default_severity") {
        None => crate::issues::DEFAULT_SEVERITY,
        Some(value) => {
            let text = expect_string(value, &format!("{issues_path}.default_severity"))?;
            Severity::parse(&text).ok_or_else(|| {
                ConfigError::new(
                    format!("{issues_path}.default_severity"),
                    format!(
                        "unknown severity \"{text}\" — expected one of {}",
                        SEVERITIES.join(", ")
                    ),
                )
            })?
        }
    };

    let severities = optional_severities(map, "severities", path)?;
    let statuses = optional_statuses(map, "statuses", path)?;
    Ok(Counter::Issues {
        repository,
        label,
        severity_prefix,
        default_severity,
        severities,
        statuses,
    })
}

/// True when a repository is in the form `owner/repo`.
///
/// The form is checked strictly: the name is part of the request URL, and a
/// name that carries a path separator, a query or a fragment would address a
/// different resource than the one the config names.
fn is_repository(text: &str) -> bool {
    let mut parts = text.split('/');
    let (Some(owner), Some(repository), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    let valid = |part: &str| {
        !part.is_empty()
            && part != "."
            && part != ".."
            && part
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    };
    valid(owner) && valid(repository)
}

/// The levels of a findings counter; every level counts without the list.
fn optional_severities(
    map: &Map<String, Value>,
    key: &str,
    path: &str,
) -> Result<Option<Vec<Severity>>, ConfigError> {
    match map.get(key) {
        None => Ok(None),
        Some(value) => {
            let items = expect_array(value, &format!("{path}.{key}"))?;
            if items.is_empty() {
                // A list that selects nothing is a gate that counts nothing:
                // it would be green forever.
                return Err(ConfigError::new(
                    format!("{path}.{key}"),
                    "must not be empty",
                ));
            }
            let mut out = Vec::with_capacity(items.len());
            for (index, item) in items.iter().enumerate() {
                let text = expect_string(item, &format!("{path}.{key}[{index}]"))?;
                let severity = Severity::parse(&text).ok_or_else(|| {
                    ConfigError::new(
                        format!("{path}.{key}[{index}]"),
                        format!(
                            "unknown severity \"{text}\" — expected one of {}",
                            SEVERITIES.join(", ")
                        ),
                    )
                })?;
                out.push(severity);
            }
            Ok(Some(out))
        }
    }
}

/// The statuses of a findings counter; the counting statuses count without
/// the list.
fn optional_statuses(
    map: &Map<String, Value>,
    key: &str,
    path: &str,
) -> Result<Option<Vec<Status>>, ConfigError> {
    match map.get(key) {
        None => Ok(None),
        Some(value) => {
            let items = expect_array(value, &format!("{path}.{key}"))?;
            if items.is_empty() {
                return Err(ConfigError::new(
                    format!("{path}.{key}"),
                    "must not be empty",
                ));
            }
            let mut out = Vec::with_capacity(items.len());
            for (index, item) in items.iter().enumerate() {
                let text = expect_string(item, &format!("{path}.{key}[{index}]"))?;
                let status = Status::parse(&text).ok_or_else(|| {
                    ConfigError::new(
                        format!("{path}.{key}[{index}]"),
                        format!(
                            "unknown status \"{text}\" — expected one of {}",
                            STATUSES.join(", ")
                        ),
                    )
                })?;
                out.push(status);
            }
            Ok(Some(out))
        }
    }
}

/// Compiles glob patterns into matchers.
///
/// The patterns are the ones of a config (`files`, `file_metrics.files`, the
/// scopes) and the ones of the command line (`--ignore`); the same rules apply
/// to all of them.
pub fn compile_globs(patterns: &[String], path: &str) -> Result<Vec<GlobMatcher>, ConfigError> {
    Ok(compile_globs_grouped(patterns, path)?
        .into_iter()
        .flatten()
        .collect())
}

/// Compiles glob patterns into matchers, one group per original pattern.
///
/// A pattern can expand into more than one matcher (`src/**` also matches the
/// directory `src`, SPEC 3.7), so the expansion has to stay attached to the
/// pattern it came from.  The index of a group is the index of the pattern in
/// `patterns`.
pub fn compile_globs_grouped(
    patterns: &[String],
    path: &str,
) -> Result<Vec<Vec<GlobMatcher>>, ConfigError> {
    let mut compiled = Vec::with_capacity(patterns.len());
    for original in patterns {
        let mut group = Vec::new();
        for pattern in glob_patterns(original) {
            let glob = GlobBuilder::new(&pattern)
                .literal_separator(true)
                .build()
                .map_err(|e| {
                    ConfigError::new(path, format!("invalid glob pattern \"{original}\": {e}"))
                })?;
            group.push(glob.compile_matcher());
        }
        compiled.push(group);
    }
    Ok(compiled)
}

/// The patterns a glob stands for.
///
/// `**` matches zero or more path segments (SPEC 3.7).  Globset already
/// handles that in the middle of a pattern (`a/**/b` matches `a/b`); at the end
/// of a pattern it does not: `src/**` has to match the directory `src` as
/// well, so the directory is added as a pattern of its own.
fn glob_patterns(pattern: &str) -> Vec<String> {
    let mut patterns = vec![pattern.to_string()];
    if let Some(prefix) = pattern.strip_suffix("/**") {
        if !prefix.is_empty() {
            patterns.push(prefix.to_string());
        }
    }
    patterns
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

fn expect_object<'a>(value: &'a Value, path: &str) -> Result<&'a Map<String, Value>, ConfigError> {
    value.as_object().ok_or_else(|| {
        ConfigError::new(
            path,
            format!("expected an object, found {}", type_name(value)),
        )
    })
}

fn expect_array<'a>(value: &'a Value, path: &str) -> Result<&'a Vec<Value>, ConfigError> {
    value.as_array().ok_or_else(|| {
        ConfigError::new(
            path,
            format!("expected an array, found {}", type_name(value)),
        )
    })
}

fn expect_string(value: &Value, path: &str) -> Result<String, ConfigError> {
    value.as_str().map(str::to_string).ok_or_else(|| {
        ConfigError::new(
            path,
            format!("expected a string, found {}", type_name(value)),
        )
    })
}

fn expect_integer(value: &Value, path: &str) -> Result<i64, ConfigError> {
    value.as_i64().ok_or_else(|| {
        ConfigError::new(
            path,
            format!("expected an integer, found {}", type_name(value)),
        )
    })
}

fn require_string_array(
    map: &Map<String, Value>,
    key: &str,
    path: &str,
) -> Result<Vec<String>, ConfigError> {
    let value = map
        .get(key)
        .ok_or_else(|| ConfigError::new(format!("{path}.{key}"), "missing required field"))?;
    let items = expect_array(value, &format!("{path}.{key}"))?;
    let mut out = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        out.push(expect_string(item, &format!("{path}.{key}[{index}]"))?);
    }
    Ok(out)
}

fn optional_string_array(
    map: &Map<String, Value>,
    key: &str,
    path: &str,
) -> Result<Vec<String>, ConfigError> {
    match map.get(key) {
        None => Ok(Vec::new()),
        Some(value) => {
            let items = expect_array(value, &format!("{path}.{key}"))?;
            let mut out = Vec::with_capacity(items.len());
            for (index, item) in items.iter().enumerate() {
                out.push(expect_string(item, &format!("{path}.{key}[{index}]"))?);
            }
            Ok(out)
        }
    }
}

fn require_string(map: &Map<String, Value>, key: &str, path: &str) -> Result<String, ConfigError> {
    let value = map
        .get(key)
        .ok_or_else(|| ConfigError::new(format!("{path}.{key}"), "missing required field"))?;
    expect_string(value, &format!("{path}.{key}"))
}

/// An optional string field.
fn optional_string(
    map: &Map<String, Value>,
    key: &str,
    path: &str,
) -> Result<Option<String>, ConfigError> {
    match map.get(key) {
        None => Ok(None),
        Some(value) => Ok(Some(expect_string(value, &format!("{path}.{key}"))?)),
    }
}

fn check_unknown(
    map: &Map<String, Value>,
    allowed: &[&str],
    path: &str,
) -> Result<(), ConfigError> {
    for key in map.keys() {
        if !allowed.contains(&key.as_str()) {
            let field = if path.is_empty() {
                key.clone()
            } else {
                format!("{path}.{key}")
            };
            return Err(ConfigError::new(field, "unknown field"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scoped_metric_config(scopes: &str) -> String {
        r#"{"rules":{"size":{"class":"growth","incident":"i","unit":"bytes","direction":"ceiling","aggregate":"max","count":{"file_metrics":{"files":["**/*.java"],"metric":"size_bytes"}},"scopes":SCOPES}}}"#.replace("SCOPES", scopes)
    }

    #[test]
    fn parses_scoped_file_metrics_and_exact_overrides() {
        let config = parse_config_str(&scoped_metric_config(
            r#"[
            {"id":"auth","description":"Authentication","files":["src/auth/**"]},
            {"id":"login","description":"Legacy login","file":"src/auth/Login.java"}
        ]"#,
        ))
        .expect("valid scoped metric");
        let rule = config.rules.get("size").expect("rule");
        assert_eq!(rule.scopes.len(), 2);
        assert_eq!(rule.aggregate, Some(Aggregate::Max));
        assert!(matches!(
            rule.count,
            Counter::FileMetrics {
                file_metrics: FileMetrics {
                    metric: FileMetric::SizeBytes,
                    ..
                }
            }
        ));
    }

    #[test]
    fn rejects_scopes_without_aggregate_and_duplicate_scope_ids() {
        let no_aggregate = scoped_metric_config("[]").replace("\"aggregate\":\"max\",", "");
        assert!(parse_config_str(&no_aggregate).is_err());
        let duplicates = scoped_metric_config(
            r#"[
            {"id":"same","description":"A","files":["a/**"]},
            {"id":"same","description":"B","files":["b/**"]}
        ]"#,
        );
        assert!(
            parse_config_str(&duplicates)
                .expect_err("duplicate ids fail")
                .message
                .contains("duplicate scope id")
        );
    }

    #[test]
    fn parses_a_jsonc_config_with_comments() {
        let text = r#"
            // the only source of truth
            [
              {
                "name": "noema",
                /* a block comment */
                "rules": {
                  "unwrapped-action-calls": {
                    "class": "layer violation",
                    "incident": "F-09",
                    "unit": "call sites",
                    "direction": "ceiling",
                    "tier": "fast",
                    "count": { "command": ["node", "scripts/audit.mjs", "--print"] },
                  },
                },
              },
            ]
        "#;
        let config = parse_config_str(text).expect("config parses");
        assert_eq!(config.name.as_deref(), Some("noema"));
        let rule = config
            .rules
            .get("unwrapped-action-calls")
            .expect("rule exists");
        assert_eq!(rule.direction, Direction::Ceiling);
        assert_eq!(rule.tier, Tier::Fast);
        assert_eq!(
            rule.count,
            Counter::Command {
                command: vec!["node".into(), "scripts/audit.mjs".into(), "--print".into()]
            }
        );
    }

    #[test]
    fn merges_config_objects_and_overrides_rules_by_id() {
        let text = r#"[
            { "rules": { "a": { "class": "c", "incident": "i", "unit": "u",
                                "direction": "ceiling", "count": { "command": ["echo", "1"] } } } },
            { "rules": { "a": { "class": "c2", "incident": "i", "unit": "u",
                                "direction": "floor", "count": { "command": ["echo", "2"] } } } }
        ]"#;
        let config = parse_config_str(text).expect("config parses");
        assert_eq!(config.rules.len(), 1);
        let rule = config.rules.get("a").expect("rule exists");
        assert_eq!(rule.class, "c2");
        assert_eq!(rule.direction, Direction::Floor);
    }

    #[test]
    fn rejects_unknown_fields_with_a_path() {
        let text = r#"[{ "rules": { "a": { "class": "c", "incident": "i", "unit": "u",
            "direction": "ceiling", "count": { "command": ["echo", "1"] }, "surprise": 1 } } }]"#;
        let error = parse_config_str(text).expect_err("unknown field is an error");
        assert_eq!(error.path, "rules.a.surprise");
    }

    #[test]
    fn rejects_an_invalid_direction_with_a_path() {
        let text = r#"[{ "rules": { "a": { "class": "c", "incident": "i", "unit": "u",
            "direction": "sideways", "count": { "command": ["echo", "1"] } } } }]"#;
        let error = parse_config_str(text).expect_err("invalid direction is an error");
        assert_eq!(error.path, "rules.a.direction");
    }

    #[test]
    fn rejects_a_counter_with_both_command_and_file() {
        let text = r#"[{ "rules": { "a": { "class": "c", "incident": "i", "unit": "u",
            "direction": "ceiling", "count": { "command": ["echo", "1"], "file": "x.json" } } } }]"#;
        let error = parse_config_str(text).expect_err("both counters is an error");
        assert_eq!(error.path, "rules.a.count");
    }

    #[test]
    fn rejects_a_path_on_a_command_counter() {
        let text = r#"[{ "rules": { "a": { "class": "c", "incident": "i", "unit": "u",
            "direction": "ceiling", "count": { "command": ["echo", "1"], "path": "x" } } } }]"#;
        let error = parse_config_str(text).expect_err("a path on a command is an error");
        assert_eq!(error.path, "rules.a.count.path");
    }

    #[test]
    fn rejects_a_missing_required_field() {
        let text = r#"[{ "rules": { "a": { "class": "c", "unit": "u",
            "direction": "ceiling", "count": { "command": ["echo", "1"] } } } }]"#;
        let error = parse_config_str(text).expect_err("missing field is an error");
        assert_eq!(error.path, "rules.a.incident");
    }

    #[test]
    fn parses_the_file_counter_with_a_dotted_path() {
        let text = r#"[{ "rules": { "a": { "class": "c", "incident": "i", "unit": "u",
            "direction": "ceiling",
            "count": { "file": "docs/_audit-raw.json", "path": "coherence.innerHtml" } } } }]"#;
        let config = parse_config_str(text).expect("config parses");
        let rule = config.rules.get("a").expect("rule exists");
        assert_eq!(
            rule.count,
            Counter::File {
                file: "docs/_audit-raw.json".into(),
                path: Some("coherence.innerHtml".into()),
                generate: None,
                output_arg: None
            }
        );
    }

    #[test]
    fn parses_the_registry_counter() {
        let text = r#"[{ "rules": { "a": { "class": "c", "incident": "i", "unit": "u",
            "direction": "ceiling",
            "count": { "registry": "ratchet.findings.json",
                       "severities": ["critical", "high"],
                       "statuses": ["open", "in-progress"] } } } }]"#;
        let config = parse_config_str(text).expect("config parses");
        let rule = config.rules.get("a").expect("rule exists");
        assert_eq!(
            rule.count,
            Counter::Registry {
                registry: "ratchet.findings.json".into(),
                severities: Some(vec![Severity::Critical, Severity::High]),
                statuses: Some(vec![Status::Open, Status::InProgress]),
            }
        );
    }

    #[test]
    fn a_registry_counter_without_the_lists_counts_everything() {
        let text = r#"[{ "rules": { "a": { "class": "c", "incident": "i", "unit": "u",
            "direction": "ceiling", "count": { "registry": "ratchet.findings.json" } } } }]"#;
        let config = parse_config_str(text).expect("config parses");
        let rule = config.rules.get("a").expect("rule exists");
        assert_eq!(
            rule.count,
            Counter::Registry {
                registry: "ratchet.findings.json".into(),
                severities: None,
                statuses: None,
            }
        );
    }

    #[test]
    fn parses_the_issues_counter_with_its_defaults() {
        let text = r#"[{ "rules": { "a": { "class": "c", "incident": "i", "unit": "u",
            "tier": "slow", "direction": "ceiling",
            "count": { "issues": { "repository": "acme/widgets" } } } } }]"#;
        let config = parse_config_str(text).expect("config parses");
        let rule = config.rules.get("a").expect("rule exists");
        assert_eq!(
            rule.count,
            Counter::Issues {
                repository: "acme/widgets".into(),
                label: "ratchet".into(),
                severity_prefix: "severity:".into(),
                default_severity: Severity::Medium,
                severities: None,
                statuses: None,
            }
        );
    }

    #[test]
    fn parses_the_issues_counter_with_every_field() {
        let text = r#"[{ "rules": { "a": { "class": "c", "incident": "i", "unit": "u",
            "tier": "slow", "direction": "ceiling",
            "count": { "issues": { "repository": "acme/widgets", "label": "gate",
                                   "severity_prefix": "level:",
                                   "default_severity": "low" },
                       "severities": ["critical", "high"],
                       "statuses": ["open"] } } } }]"#;
        let config = parse_config_str(text).expect("config parses");
        let rule = config.rules.get("a").expect("rule exists");
        assert_eq!(
            rule.count,
            Counter::Issues {
                repository: "acme/widgets".into(),
                label: "gate".into(),
                severity_prefix: "level:".into(),
                default_severity: Severity::Low,
                severities: Some(vec![Severity::Critical, Severity::High]),
                statuses: Some(vec![Status::Open]),
            }
        );
    }

    #[test]
    fn rejects_issues_counters_in_the_fast_tier() {
        for tier in [None, Some("fast")] {
            let tier = tier
                .map(|tier| format!("\"tier\": \"{tier}\","))
                .unwrap_or_default();
            let text = format!(
                r#"[{{ "rules": {{ "a": {{ "class": "c", "incident": "i", "unit": "u",
                    {tier} "direction": "ceiling",
                    "count": {{ "issues": {{ "repository": "acme/widgets" }} }} }} }} }}]"#
            );
            let error = parse_config_str(&text).expect_err("network rules cannot be fast");
            assert_eq!(error.path, "rules.a.tier");
            assert!(error.message.contains("slow"), "{}", error.message);
        }
    }

    #[test]
    fn rejects_a_repository_that_is_not_owner_repo() {
        for repository in [
            "widgets",
            "owner/",
            "/widgets",
            "a/b/c",
            "own er/widgets",
            "../x",
        ] {
            let text = format!(
                r#"[{{ "rules": {{ "a": {{ "class": "c", "incident": "i", "unit": "u",
                    "direction": "ceiling",
                    "count": {{ "issues": {{ "repository": "{repository}" }} }} }} }} }}]"#
            );
            let error = parse_config_str(&text).expect_err("the repository is malformed");
            assert_eq!(
                error.path, "rules.a.count.issues.repository",
                "for {repository}"
            );
        }
    }

    #[test]
    fn rejects_an_unknown_field_in_the_issues_counter() {
        let text = r#"[{ "rules": { "a": { "class": "c", "incident": "i", "unit": "u",
            "direction": "ceiling",
            "count": { "issues": { "repository": "acme/widgets", "base": "https://example.com" } } } } }]"#;
        let error = parse_config_str(text).expect_err("an unknown field is an error");
        assert_eq!(error.path, "rules.a.count.issues.base");
    }

    #[test]
    fn rejects_an_unknown_default_severity() {
        let text = r#"[{ "rules": { "a": { "class": "c", "incident": "i", "unit": "u",
            "direction": "ceiling",
            "count": { "issues": { "repository": "acme/widgets", "default_severity": "blocker" } } } } }]"#;
        let error = parse_config_str(text).expect_err("an unknown level is an error");
        assert_eq!(error.path, "rules.a.count.issues.default_severity");
    }

    #[test]
    fn rejects_an_empty_label() {
        let text = r#"[{ "rules": { "a": { "class": "c", "incident": "i", "unit": "u",
            "direction": "ceiling",
            "count": { "issues": { "repository": "acme/widgets", "label": "  " } } } } }]"#;
        let error = parse_config_str(text).expect_err("an empty label is an error");
        assert_eq!(error.path, "rules.a.count.issues.label");
    }

    #[test]
    fn rejects_issues_together_with_a_registry() {
        let text = r#"[{ "rules": { "a": { "class": "c", "incident": "i", "unit": "u",
            "direction": "ceiling",
            "count": { "issues": { "repository": "acme/widgets" },
                       "registry": "ratchet.findings.json" } } } }]"#;
        let error = parse_config_str(text).expect_err("two counters is an error");
        assert_eq!(error.path, "rules.a.count");
        assert!(
            error.message.contains("\"issues\""),
            "the message names the form: {}",
            error.message
        );
    }

    #[test]
    fn rejects_an_unknown_severity_in_the_rule() {
        let text = r#"[{ "rules": { "a": { "class": "c", "incident": "i", "unit": "u",
            "direction": "ceiling",
            "count": { "registry": "ratchet.findings.json", "severities": ["blocker"] } } } }]"#;
        let error = parse_config_str(text).expect_err("an unknown level is an error");
        assert_eq!(error.path, "rules.a.count.severities[0]");
    }

    #[test]
    fn rejects_an_unknown_status_in_the_rule() {
        let text = r#"[{ "rules": { "a": { "class": "c", "incident": "i", "unit": "u",
            "direction": "ceiling",
            "count": { "registry": "ratchet.findings.json", "statuses": ["doing"] } } } }]"#;
        let error = parse_config_str(text).expect_err("an unknown status is an error");
        assert_eq!(error.path, "rules.a.count.statuses[0]");
    }

    #[test]
    fn rejects_a_counter_with_a_registry_and_a_file() {
        let text = r#"[{ "rules": { "a": { "class": "c", "incident": "i", "unit": "u",
            "direction": "ceiling",
            "count": { "registry": "ratchet.findings.json", "file": "x.json" } } } }]"#;
        let error = parse_config_str(text).expect_err("two counters is an error");
        assert_eq!(error.path, "rules.a.count");
    }

    #[test]
    fn a_rule_without_files_always_runs() {
        let text = r#"[{ "rules": { "a": { "class": "c", "incident": "i", "unit": "u",
            "direction": "ceiling", "count": { "command": ["echo", "1"] } } } }]"#;
        let config = parse_config_str(text).expect("config parses");
        let rule = config.rules.get("a").expect("rule exists");
        assert!(rule.in_scope("src/other.rs"));
    }

    #[test]
    fn a_scoped_rule_runs_only_for_paths_in_its_scope() {
        let text = r#"[{ "rules": { "a": { "class": "c", "incident": "i", "unit": "u",
            "direction": "ceiling", "count": { "command": ["echo", "1"] },
            "files": ["src/**"] } } }]"#;
        let config = parse_config_str(text).expect("config parses");
        let rule = config.rules.get("a").expect("rule exists");
        assert!(rule.in_scope("src/app/main.rs"));
        assert!(!rule.in_scope("docs/readme.md"));
    }

    #[test]
    fn a_double_star_matches_the_directory_itself() {
        let text = r#"[{ "rules": { "a": { "class": "c", "incident": "i", "unit": "u",
            "direction": "ceiling", "count": { "command": ["echo", "1"] },
            "files": ["src/**"] } } }]"#;
        let config = parse_config_str(text).expect("config parses");
        let rule = config.rules.get("a").expect("rule exists");
        // `src/**` matches `src` and everything below it (SPEC 3.7).
        assert!(rule.in_scope("src"));
        assert!(rule.in_scope("src/"));
        assert!(rule.in_scope("src/nested/deep/file.rs"));
        assert!(!rule.in_scope("srcs/file.rs"));
        assert!(!rule.in_scope("docs/readme.md"));
    }

    #[test]
    fn rejects_an_invalid_glob_pattern() {
        let text = r#"[{ "rules": { "a": { "class": "c", "incident": "i", "unit": "u",
            "direction": "ceiling", "count": { "command": ["echo", "1"] },
            "exclude": ["src/["] } } }]"#;
        let error = parse_config_str(text).expect_err("invalid glob is an error");
        assert_eq!(error.path, "rules.a.exclude");
    }

    #[test]
    fn loads_a_javascript_config_when_node_is_available() {
        if !node_available() {
            eprintln!(
                "*** SKIPPED: node is not on the PATH — this JavaScript test did NOT run. \
                 Install node (or run the CI, which installs it) to exercise it. ***"
            );
            return;
        }
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ratchet.config.mjs");
        let config = load_js_config(&path).expect("the js config loads");
        assert_eq!(config.name.as_deref(), Some("fixture"));
        // The rule of the fixture proves that the file was really evaluated.
        let rule = config
            .rules
            .get("js-rule")
            .expect("the rule of the fixture is loaded");
        assert_eq!(rule.direction, Direction::Ceiling);
        assert_eq!(rule.class, "layer violation");
    }

    #[test]
    fn a_javascript_config_in_a_path_with_url_characters_loads() {
        if !node_available() {
            eprintln!(
                "*** SKIPPED: node is not on the PATH — this JavaScript test did NOT run. \
                 Install node (or run the CI, which installs it) to exercise it. ***"
            );
            return;
        }
        // `#` and `%` (and `?` on Unix) are not valid in a URL specifier: the
        // bridge has to resolve the path as a file URL.
        let directory = crate::test_tempdir::TempDir::new().expect("a temporary directory");
        #[cfg(windows)]
        let path = directory.path().join("ratchet#config100%.mjs");
        #[cfg(unix)]
        let path = directory.path().join("ratchet#config?100%.mjs");
        std::fs::write(
            &path,
            "export default [{ rules: { a: { class: \"c\", incident: \"i\", unit: \"u\", \
             direction: \"ceiling\", count: { command: [\"echo\", \"1\"] } } } }];\n",
        )
        .expect("the file is written");

        let config = load_js_config(&path).expect("the js config loads");

        assert!(config.rules.contains_key("a"));
    }

    fn node_available() -> bool {
        Command::new("node")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn the_agent_rules_preset_merges_the_canonical_six() {
        let config = parse_config_str(r#"{ "preset": "agent-rules" }"#).expect("preset parses");
        assert_eq!(config.rules.len(), 6);
        for id in [
            "rule-file-name",
            "rule-file-present",
            "rule-file-max-lines",
            "plan-status-line",
            "gates-untouched",
            "unverified-commits",
        ] {
            assert!(config.rules.contains_key(id), "{id} missing");
        }
        let present = &config.rules["rule-file-present"];
        assert_eq!(present.direction, Direction::Floor);
    }

    #[test]
    fn an_object_extends_the_preset_with_its_own_rules() {
        let config = parse_config_str(
            r#"[
                { "preset": "agent-rules" },
                { "rules": { "own-rule": {
                    "class": "c", "incident": "i", "unit": "u", "direction": "ceiling",
                    "count": { "command": ["true"] } } } }
            ]"#,
        )
        .expect("config parses");
        assert_eq!(config.rules.len(), 7);
        assert!(config.rules.contains_key("own-rule"));
    }

    #[test]
    fn an_unknown_preset_is_an_error() {
        let error = parse_config_str(r#"{ "preset": "nope" }"#).expect_err("fails");
        assert!(
            error.message.contains("unknown preset"),
            "{}",
            error.message
        );
        assert!(error.message.contains("agent-rules"));
    }

    #[test]
    fn an_epoch_belongs_only_to_a_history_rule() {
        let error = parse_config_str(
            r#"{ "rules": { "a": {
                "class": "c", "incident": "i", "unit": "u", "direction": "ceiling",
                "count": { "agent_rule": "rule-file-name", "since": "2026-10-30" } } } }"#,
        )
        .expect_err("fails");
        assert!(error.message.contains("does not read the commit history"));
    }

    #[test]
    fn the_epoch_must_be_a_calendar_date() {
        let error = parse_config_str(
            r#"{ "rules": { "a": {
                "class": "c", "incident": "i", "unit": "u", "direction": "ceiling",
                "count": { "agent_rule": "unverified-commits", "since": "30.10.2026" } } } }"#,
        )
        .expect_err("fails");
        assert!(error.message.contains("YYYY-MM-DD"));
    }

    #[test]
    fn an_unknown_agent_rule_names_the_known_ones() {
        let error = parse_config_str(
            r#"{ "rules": { "a": {
                "class": "c", "incident": "i", "unit": "u", "direction": "ceiling",
                "count": { "agent_rule": "nope" } } } }"#,
        )
        .expect_err("fails");
        assert!(error.message.contains("rule-file-present"));
    }
}
