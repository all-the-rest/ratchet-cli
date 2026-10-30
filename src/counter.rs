//! Counters: how the count of a rule is produced and how it is interpreted.
//!
//! The report carries ratchet's own words only: a counter that fails is
//! reported as *what* it was (which rule, which exit status), never as *what
//! it printed*.  The raw output of a counter belongs to the diagnostic
//! channel (`--debug`), which is exempt from the determinism rule (SPEC 6.2).

use std::fs;
use std::path::Path;
use std::process::Command;

use globset::GlobMatcher;
use serde_json::Value;

use crate::config::{AgentRule, Counter, FileMetric};
use crate::generate::Generator;
use crate::issues;
use crate::registry::{Finding, Registry, Severity, Status};

/// The value a counter produced.
#[derive(Debug, Clone, Default)]
pub struct CounterValue {
    /// An explicit count, if the counter reported one.
    pub count: Option<i64>,
    /// The counted items, if the counter reported any.
    pub items: Option<Vec<String>>,
    /// Values emitted for individual repository-relative paths.
    pub measurements: Option<Vec<Measurement>>,
}

/// One nonnegative metric attached to a repository-relative path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Measurement {
    pub path: String,
    pub value: i64,
}

/// A counter that did not deliver a usable count.
#[derive(Debug, Clone)]
pub struct CounterError {
    /// Ratchet's own sentence; the report carries it.
    pub message: String,
    /// The raw output of the subprocess; the diagnostic channel carries it.
    pub output: Option<String>,
    /// True when the counter itself is misconfigured (not a runtime failure).
    pub is_config: bool,
}

impl CounterError {
    /// A failure of the counter at runtime.
    pub(crate) fn runtime(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            output: None,
            is_config: false,
        }
    }

    /// A failure of the counter at runtime, with the raw output.
    fn runtime_with(message: impl Into<String>, output: String) -> Self {
        Self {
            message: message.into(),
            output: Some(output),
            is_config: false,
        }
    }

    /// A misconfigured counter.
    pub(crate) fn config(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            output: None,
            is_config: true,
        }
    }
}

impl std::fmt::Display for CounterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

/// The result of a counter: the value and the raw output of the subprocess.
#[derive(Debug, Clone)]
pub struct CounterRun {
    /// The value the counter produced.
    pub value: CounterValue,
    /// The raw output of the subprocess; the diagnostic channel carries it.
    pub output: String,
    /// The findings that count, when the counter is a findings backend.
    ///
    /// The list is the **same** selection the count was made of: the report
    /// shows the findings that produced the number, not a second reading of
    /// the source (SPEC 14). It is `None` for every other counter.
    pub findings: Option<Vec<Finding>>,
}

/// Runs the counter and returns its value and the raw output.
///
/// `path` is the path of the counter in the config (`rules.a.count`): a
/// counter that is misconfigured names it.
pub fn run_counter(counter: &Counter, root: &Path, path: &str) -> Result<CounterRun, CounterError> {
    let run = match counter {
        Counter::Command { command } => run_command(command, root),
        Counter::FileMetrics { file_metrics } => {
            run_file_metrics(&file_metrics.files, file_metrics.metric, root, path)
        }
        Counter::AgentRule { agent_rule, since } => {
            run_agent_rule(*agent_rule, since.as_deref(), root, path)
        }
        Counter::File {
            file,
            path: json_path,
            generate,
            output_arg,
        } => {
            // A generator writes to a temporary path: `output_arg` is
            // mandatory (the config rejects a counter without it).
            let generator = match (generate, output_arg) {
                (Some(command), Some(output_arg)) => Some(Generator {
                    command: command.clone(),
                    output_arg: output_arg.clone(),
                }),
                (Some(_), None) => {
                    return Err(CounterError::config(
                        "config error at count.output_arg: is required by \"generate\"",
                    ));
                }
                (None, _) => None,
            };
            run_file(file, json_path.as_deref(), generator, root, path)
        }
        Counter::Registry {
            registry,
            severities,
            statuses,
        } => run_registry(
            registry,
            severities.as_deref(),
            statuses.as_deref(),
            root,
            path,
        ),
        Counter::Issues {
            repository,
            label,
            severity_prefix,
            default_severity,
            severities,
            statuses,
        } => run_issues(&issues::Options {
            repository,
            label,
            severity_prefix,
            default_severity: *default_severity,
            severities: severities.as_deref(),
            statuses: statuses.as_deref(),
        }),
    }?;
    validate_measurement_files(&run.value, root)?;
    Ok(run)
}

fn validate_measurement_files(value: &CounterValue, root: &Path) -> Result<(), CounterError> {
    for measurement in value.measurements.iter().flatten() {
        let resolved =
            crate::path::inside_repository(root, &measurement.path).map_err(|refusal| {
                CounterError::runtime(format!(
                    "measurement path {} is outside the repository root: {}",
                    measurement.path,
                    refusal.message(&measurement.path, "is outside the repository root")
                ))
            })?;
        let metadata = fs::metadata(&resolved).map_err(|error| {
            CounterError::runtime(format!(
                "cannot inspect measurement path {}: {error}",
                measurement.path
            ))
        })?;
        if !metadata.is_file() {
            return Err(CounterError::runtime(format!(
                "measurement path {} is not a regular file",
                measurement.path
            )));
        }
    }
    Ok(())
}

/// The final count and the `exclude` patterns that matched nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finalized {
    /// The count after `exclude`.
    pub count: i64,
    /// The indices of the `exclude` patterns that matched no item.
    ///
    /// A dead pattern is a note, never a failure: the engine never mutates the
    /// config, so there is nothing to prune (SPEC 5.2).
    pub unmatched_excludes: Vec<usize>,
}

/// Applies `exclude` to the items and turns the value into the final count.
///
/// `exclude` is grouped: one inner slice per configured pattern, because a
/// pattern can expand into more than one matcher (SPEC 3.7).  The index of a
/// group is therefore the index of the pattern.
pub fn finalize(
    value: CounterValue,
    exclude: &[Vec<GlobMatcher>],
    exclude_path: &str,
) -> Result<Finalized, CounterError> {
    let CounterValue {
        count,
        items,
        measurements,
    } = value;
    if measurements.is_some() {
        return Err(CounterError::config(format!(
            "config error at {exclude_path}: per-file measurements require scoped aggregation"
        )));
    }

    if let Some(items) = items {
        let mut items = items;
        items.sort();
        if exclude.is_empty() {
            let count = count.unwrap_or(items.len() as i64);
            return Ok(Finalized {
                count: non_negative(count)?,
                unmatched_excludes: Vec::new(),
            });
        }
        let unmatched_excludes = exclude
            .iter()
            .enumerate()
            .filter(|(_, group)| {
                !group
                    .iter()
                    .any(|glob| items.iter().any(|item| glob.is_match(item)))
            })
            .map(|(index, _)| index)
            .collect();
        let kept = items
            .iter()
            .filter(|item| !exclude.iter().flatten().any(|glob| glob.is_match(item)))
            .count() as i64;
        return Ok(Finalized {
            count: non_negative(kept)?,
            unmatched_excludes,
        });
    }

    if !exclude.is_empty() {
        return Err(CounterError::config(format!(
            "config error at {exclude_path}: exclude requires a counter that yields items"
        )));
    }

    let count = count.ok_or_else(|| CounterError::runtime("the counter did not print a count"))?;
    Ok(Finalized {
        count: non_negative(count)?,
        unmatched_excludes: Vec::new(),
    })
}

/// Reduces file measurements using a configured aggregate.
pub fn finalize_measurements(
    value: CounterValue,
    aggregate: crate::config::Aggregate,
) -> Result<i64, CounterError> {
    let measurements = value
        .measurements
        .ok_or_else(|| CounterError::runtime("the counter did not emit per-file measurements"))?;
    if measurements.is_empty() {
        return Err(CounterError::runtime("the counter selected no files"));
    }
    match aggregate {
        crate::config::Aggregate::Count => i64::try_from(measurements.len())
            .map_err(|_| CounterError::runtime("measurement count is too large")),
        crate::config::Aggregate::Max => measurements
            .iter()
            .map(|measurement| measurement.value)
            .max()
            .ok_or_else(|| CounterError::runtime("the counter selected no files")),
    }
}

/// Parses the stdout of a command counter.
pub fn parse_counter_output(raw: &str) -> Result<CounterValue, CounterError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(CounterError::runtime("the counter did not print a count"));
    }
    if let Ok(count) = trimmed.parse::<i64>() {
        return Ok(CounterValue {
            count: Some(count),
            items: None,
            measurements: None,
        });
    }
    let value: Value = serde_json::from_str(trimmed)
        .map_err(|_| CounterError::runtime("the counter did not print a count"))?;
    value_to_counter_value(&value)
}

/// Interprets a JSON value as a counter value.
pub fn value_to_counter_value(value: &Value) -> Result<CounterValue, CounterError> {
    match value {
        Value::Number(number) => {
            let count = number
                .as_i64()
                .ok_or_else(|| CounterError::runtime("the counter did not print a count"))?;
            Ok(CounterValue {
                count: Some(count),
                items: None,
                measurements: None,
            })
        }
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                let text = item
                    .as_str()
                    .ok_or_else(|| CounterError::runtime("the counter did not print a count"))?;
                out.push(text.to_string());
            }
            Ok(CounterValue {
                count: None,
                items: Some(out),
                measurements: None,
            })
        }
        Value::Object(map) => {
            let count =
                match map.get("count") {
                    None => None,
                    Some(count) => Some(count.as_i64().ok_or_else(|| {
                        CounterError::runtime("the counter did not print a count")
                    })?),
                };
            let items = match map.get("items") {
                None => None,
                Some(items) => {
                    let items = items.as_array().ok_or_else(|| {
                        CounterError::runtime("the counter did not print a count")
                    })?;
                    let mut out = Vec::with_capacity(items.len());
                    for item in items {
                        let text = item.as_str().ok_or_else(|| {
                            CounterError::runtime("the counter did not print a count")
                        })?;
                        out.push(text.to_string());
                    }
                    Some(out)
                }
            };
            let measurements = match map.get("measurements") {
                None => None,
                Some(value) => {
                    let entries = value.as_array().ok_or_else(|| {
                        CounterError::runtime("the counter printed malformed measurements")
                    })?;
                    let mut parsed = Vec::with_capacity(entries.len());
                    let mut seen = std::collections::BTreeSet::new();
                    for entry in entries {
                        let item = entry.as_object().ok_or_else(|| {
                            CounterError::runtime("the counter printed malformed measurements")
                        })?;
                        if item.len() != 2 {
                            return Err(CounterError::runtime(
                                "the counter printed malformed measurements",
                            ));
                        }
                        let path = item.get("path").and_then(Value::as_str).ok_or_else(|| {
                            CounterError::runtime("the counter printed malformed measurements")
                        })?;
                        let candidate = Path::new(path);
                        if path.is_empty()
                            || candidate.is_absolute()
                            || path.contains('\\')
                            || candidate
                                .components()
                                .any(|part| !matches!(part, std::path::Component::Normal(_)))
                        {
                            return Err(CounterError::runtime(
                                "the counter printed a path that is not repository-relative",
                            ));
                        }
                        if !seen.insert(path.to_string()) {
                            return Err(CounterError::runtime(
                                "the counter printed duplicate measurement paths",
                            ));
                        }
                        let number = item
                            .get("value")
                            .and_then(Value::as_i64)
                            .filter(|n| *n >= 0)
                            .ok_or_else(|| {
                                CounterError::runtime(
                                    "the counter printed an invalid measurement value",
                                )
                            })?;
                        parsed.push(Measurement {
                            path: path.to_string(),
                            value: number,
                        });
                    }
                    Some(parsed)
                }
            };
            if measurements.is_some() && (count.is_some() || items.is_some()) {
                return Err(CounterError::runtime(
                    "the counter mixed scalar values with per-file measurements",
                ));
            }
            if count.is_none() && items.is_none() && measurements.is_none() {
                return Err(CounterError::runtime("the counter did not print a count"));
            }
            Ok(CounterValue {
                count,
                items,
                measurements,
            })
        }
        _ => Err(CounterError::runtime("the counter did not print a count")),
    }
}

fn non_negative(count: i64) -> Result<i64, CounterError> {
    if count < 0 {
        return Err(CounterError::runtime(
            "the counter printed a negative count",
        ));
    }
    Ok(count)
}

fn run_file_metrics(
    patterns: &[String],
    metric: FileMetric,
    root: &Path,
    path: &str,
) -> Result<CounterRun, CounterError> {
    let globs = crate::config::compile_globs(patterns, &format!("{path}.file_metrics.files"))
        .map_err(|error| CounterError::config(error.to_string()))?;
    let mut paths = Vec::new();
    collect_files(root, root, &globs, &mut paths)?;
    paths.sort();
    let mut measurements = Vec::new();
    for (relative, full) in paths {
        if !globs.iter().any(|glob| glob.is_match(&relative)) {
            continue;
        }
        let bytes = fs::read(&full)
            .map_err(|error| CounterError::runtime(format!("cannot read {relative}: {error}")))?;
        let value = match metric {
            FileMetric::SizeBytes => i64::try_from(bytes.len())
                .map_err(|_| CounterError::runtime("file is too large to measure"))?,
            FileMetric::LineCount | FileMetric::MaxLineLength => {
                let text = std::str::from_utf8(&bytes)
                    .map_err(|_| CounterError::runtime(format!("{relative} is not UTF-8")))?;
                let (line_count, max_line_length) = line_metrics(text);
                let count = if metric == FileMetric::LineCount {
                    line_count
                } else {
                    max_line_length
                };
                i64::try_from(count)
                    .map_err(|_| CounterError::runtime("file metric is too large"))?
            }
        };
        measurements.push(Measurement {
            path: relative,
            value,
        });
    }
    Ok(CounterRun {
        value: CounterValue {
            count: None,
            items: None,
            measurements: Some(measurements),
        },
        output: String::new(),
        findings: None,
    })
}

fn line_metrics(text: &str) -> (usize, usize) {
    if text.is_empty() {
        return (0, 0);
    }
    let mut lines = 0usize;
    let mut current = 0usize;
    let mut longest = 0usize;
    let mut chars = text.chars().peekable();
    let mut ended_with_terminator = false;
    while let Some(character) = chars.next() {
        match character {
            '\n' => {
                lines += 1;
                longest = longest.max(current);
                current = 0;
                ended_with_terminator = true;
            }
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                lines += 1;
                longest = longest.max(current);
                current = 0;
                ended_with_terminator = true;
            }
            _ => {
                current += 1;
                ended_with_terminator = false;
            }
        }
    }
    if !ended_with_terminator {
        lines += 1;
        longest = longest.max(current);
    }
    (lines, longest)
}

fn collect_files(
    root: &Path,
    directory: &Path,
    globs: &[GlobMatcher],
    out: &mut Vec<(String, std::path::PathBuf)>,
) -> Result<(), CounterError> {
    let entries = fs::read_dir(directory).map_err(|error| {
        CounterError::runtime(format!("cannot read repository directory: {error}"))
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            CounterError::runtime(format!("cannot read repository entry: {error}"))
        })?;
        let kind = entry.file_type().map_err(|error| {
            CounterError::runtime(format!("cannot inspect repository entry: {error}"))
        })?;
        let relative = entry
            .path()
            .strip_prefix(root)
            .map_err(|_| CounterError::runtime("file path is outside repository root"))?
            .to_string_lossy()
            .replace('\\', "/");
        if kind.is_symlink() {
            // A directory link could contain selected descendants, and walking
            // through it would weaken the repository boundary. Fail closed.
            if globs.iter().any(|glob| glob.is_match(&relative)) {
                return Err(CounterError::runtime("file_metrics refuses symbolic links"));
            }
            match fs::metadata(entry.path()) {
                Ok(metadata) if metadata.is_dir() => {
                    return Err(CounterError::runtime("file_metrics refuses symbolic links"));
                }
                Ok(_) => {}
                // A dangling, unselected link has no descendants to collect.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(CounterError::runtime(format!(
                        "cannot inspect symbolic link {relative}: {error}"
                    )));
                }
            }
            continue;
        }
        if kind.is_dir() {
            collect_files(root, &entry.path(), globs, out)?;
        } else if kind.is_file() {
            out.push((relative, entry.path()));
        }
    }
    Ok(())
}

fn run_command(command: &[String], root: &Path) -> Result<CounterRun, CounterError> {
    let (program, arguments) = command
        .split_first()
        .ok_or_else(|| CounterError::runtime("the counter command is empty"))?;

    let output = Command::new(program)
        .args(arguments)
        .current_dir(root)
        .output()
        .map_err(|e| CounterError::runtime(format!("the counter could not be started: {e}")))?;

    let transcript = transcript(command, &output);

    if !output.status.success() {
        let status = match output.status.code() {
            Some(code) => format!("status {code}"),
            None => "an unknown status".to_string(),
        };
        return Err(CounterError::runtime_with(
            format!("the counter exited with {status}"),
            transcript,
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let value = parse_counter_output(&stdout)
        .map_err(|error| CounterError::runtime_with(error.message, transcript.clone()))?;

    Ok(CounterRun {
        value,
        output: transcript,
        findings: None,
    })
}

fn run_file(
    file: &str,
    json_path: Option<&str>,
    generate: Option<Generator>,
    root: &Path,
    path: &str,
) -> Result<CounterRun, CounterError> {
    // Every path in the report is relative to the repository root.
    let display = crate::path::display(Some(root), &root.join(file));

    // The artifact is a file of the repository: a path that leaves the root
    // would name a file the repository does not own.  The rule is about the
    // path the project **names**, not about what the run reads from it: with
    // `generate` the count comes from a temporary path, and the configured
    // path is still the project's.  The message names the **configured**
    // path — the user has to see which path of the config was refused.
    if let Err(refusal) = crate::path::inside_repository(root, file) {
        return Err(CounterError::config(format!(
            "config error at {path}.file: {}",
            refusal.message(file, "is outside the repository root")
        )));
    }

    let (text, output) = match generate {
        Some(generator) => {
            // The artifact is regenerated first: a count read from a stale
            // artifact is a gate that lies.  The generator writes to a
            // temporary path; the tree is never touched.
            let generated = generator
                .run(Path::new(file), root)
                .map_err(|error| CounterError::runtime_with(error.message, error.output))?;
            let text = std::str::from_utf8(&generated.bytes)
                .map_err(|_| {
                    CounterError::runtime_with(
                        format!("the generator for {display} did not write UTF-8"),
                        generated.output.clone(),
                    )
                })?
                .to_string();
            (text, generated.output.clone())
        }
        None => {
            let full = root.join(file);
            let text = fs::read_to_string(&full)
                .map_err(|e| CounterError::runtime(format!("cannot read {display}: {e}")))?;
            (text, String::new())
        }
    };

    // The output of the generator belongs to the diagnostic channel; it is
    // attached to every error that happens after it ran (SPEC 6.2).
    let value: Value = serde_json::from_str(&text).map_err(|e| {
        CounterError::runtime_with(format!("{display} is not valid JSON: {e}"), output.clone())
    })?;
    let value: &Value = match json_path {
        None => &value,
        Some(path) => navigate(&value, path)
            .map_err(|e| CounterError::runtime_with(format!("{display}: {e}"), output.clone()))?,
    };
    let value = value_to_counter_value(value).map_err(|_| {
        CounterError::runtime_with(
            format!("{display} does not contain a count"),
            output.clone(),
        )
    })?;

    Ok(CounterRun {
        value,
        output,
        findings: None,
    })
}

/// Reads a findings registry and counts the findings that count (SPEC 13).
///
/// The registry is a counter source like every other artifact: it is read
/// when the rule runs, and a registry that cannot be read or does not hold
/// what it must hold is a configuration error — never a gate that reports
/// zero findings.
///
/// The findings that count travel with the count: the report shows **the
/// findings the number was made of** (SPEC 14).
fn run_registry(
    registry: &str,
    severities: Option<&[Severity]>,
    statuses: Option<&[Status]>,
    root: &Path,
    path: &str,
) -> Result<CounterRun, CounterError> {
    // The registry lives inside the repository root: a path that escapes it
    // is a configuration error, not a file to read.  The check resolves the
    // whole path — a link cannot carry it out.
    if let Err(refusal) = crate::path::inside_repository(root, registry) {
        return Err(CounterError::config(format!(
            "config error at {path}.registry: {}",
            refusal.message(registry, "is outside the repository root")
        )));
    }
    let full = root.join(registry);

    // Every path in the report is relative to the repository root.
    let display = crate::path::display(Some(root), &full);
    // The root itself has no relative name: the configured path names it.
    let display = if display.is_empty() {
        registry.to_string()
    } else {
        display
    };
    let text = fs::read_to_string(&full).map_err(|e| {
        CounterError::config(format!(
            "config error at {path}.registry: cannot read {display}: {e}"
        ))
    })?;

    let registry = Registry::parse(&text, &display).map_err(|error| {
        CounterError::config(format!(
            "config error at {path}.registry: {}",
            error.message
        ))
    })?;

    // The count is the number of the findings that count — and the findings
    // themselves travel with it: the report shows the ones the number was
    // made of.
    let findings: Vec<Finding> = registry
        .selecting(severities, statuses)
        .into_iter()
        .cloned()
        .collect();
    let count = findings.len() as i64;
    Ok(CounterRun {
        value: CounterValue {
            count: Some(count),
            items: None,
            measurements: None,
        },
        output: String::new(),
        findings: Some(findings),
    })
}

/// Reads the findings of a GitHub repository (SPEC 13).
///
/// The token, the endpoint and the requests live in [`crate::issues`]; the
/// counter turns the answer into a count, and the findings travel with it —
/// exactly as the registry's do.  A failure never becomes a zero: a missing
/// token, a failing request or a label outside the vocabulary leaves the rule
/// without a count, and the run exits with 2.
fn run_issues(options: &issues::Options<'_>) -> Result<CounterRun, CounterError> {
    let findings =
        issues::findings(options).map_err(|error| CounterError::runtime(error.message()))?;
    Ok(CounterRun {
        value: CounterValue {
            count: Some(findings.len() as i64),
            items: None,
            measurements: None,
        },
        output: String::new(),
        findings: Some(findings),
    })
}

fn navigate<'a>(value: &'a Value, path: &str) -> Result<&'a Value, String> {
    let mut current = value;
    for segment in path.split('.') {
        let map = current
            .as_object()
            .ok_or_else(|| format!("\"{path}\" not found: {segment} is not an object"))?;
        current = map
            .get(segment)
            .ok_or_else(|| format!("\"{path}\" not found: no \"{segment}\""))?;
    }
    Ok(current)
}

/// The transcript of a subprocess: the diagnostic channel (SPEC 6.2).
///
/// It shows the argv that was really executed.
fn transcript(command: &[String], output: &std::process::Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let status = match output.status.code() {
        Some(code) => format!("status {code}"),
        None => "an unknown status".to_string(),
    };
    format!(
        "{}: {status}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        command.join(" ")
    )
}

/// Runs a built-in agent rule (SPEC 3.8).
///
/// A built-in rule reads git and the working tree directly. It has no script,
/// no path and no drift: the counter that ships with the binary is the one the
/// rule names.
fn run_agent_rule(
    rule: AgentRule,
    since: Option<&str>,
    root: &Path,
    _path: &str,
) -> Result<CounterRun, CounterError> {
    let git = crate::git::Git::discover(root);
    let value = match rule {
        AgentRule::RuleFileName => {
            let items: Vec<String> = git
                .tracked_files()
                .into_iter()
                .filter(|file| is_misnamed_rule_file(file))
                .collect();
            CounterValue {
                count: Some(items.len() as i64),
                items: Some(items),
                measurements: None,
            }
        }
        AgentRule::RuleFilePresent => count_value(i64::from(root.join("AGENTS.md").is_file())),
        AgentRule::PlanStatusLine => {
            count_value(plans_without_status(&root.join("docs").join("plans")))
        }
        AgentRule::GatesUntouched => {
            let range = if git.rev_exists("origin/main") {
                "origin/main..HEAD".to_string()
            } else if git.rev_exists("@{u}") {
                "@{u}..HEAD".to_string()
            } else {
                String::new()
            };
            if range.is_empty() {
                count_value(0)
            } else {
                count_value(
                    git.commits_with_files(&range)
                        .iter()
                        .filter(|(_, files)| disarms_a_gate(files))
                        .count() as i64,
                )
            }
        }
        AgentRule::UnverifiedCommits => {
            let commits = git.commits_since(since).unwrap_or_default();
            count_value(
                commits
                    .iter()
                    .filter(|commit| !has_verify_footer(commit))
                    .count() as i64,
            )
        }
    };
    Ok(CounterRun {
        value,
        output: String::new(),
        findings: None,
    })
}

/// A scalar count without items.
fn count_value(count: i64) -> CounterValue {
    CounterValue {
        count: Some(count),
        items: None,
        measurements: None,
    }
}

/// A tracked rule file whose name is not exactly `AGENTS.md`.
///
/// The comparison is case-sensitive on purpose: a harness looks the name up
/// exactly, so `Agents.md` is the defect, not a style question.
fn is_misnamed_rule_file(file: &str) -> bool {
    let name = file.rsplit('/').next().unwrap_or(file);
    name.eq_ignore_ascii_case("agents.md") && name != "AGENTS.md"
}

/// Plans in `directory` without a `**Status:**` line in the first 15 lines.
fn plans_without_status(directory: &Path) -> i64 {
    let Ok(entries) = fs::read_dir(directory) else {
        return 0;
    };
    let mut missing = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let head: String = text.lines().take(15).collect::<Vec<_>>().join("\n");
        if !head.contains("**Status:**") {
            missing += 1;
        }
    }
    missing
}

/// Paths whose change can disarm the check that would catch the mistake.
fn is_gate_path(file: &str) -> bool {
    if file.starts_with(".github/workflows/")
        || file.starts_with(".github/actions/")
        || file.starts_with(".githooks/")
        || file.starts_with(".agents/gates/")
    {
        return true;
    }
    if file == "scripts/gates.sh"
        || file == "ratchet.config.jsonc"
        || file == "ratchet.config.json"
        || file == "ratchet.baseline.json"
    {
        return true;
    }
    let name = file.rsplit('/').next().unwrap_or(file);
    name.starts_with("check_") && name.ends_with(".sh")
}

/// Whether a file is product code rather than documentation or a rule file.
///
/// A fix that also documents itself is the normal case; the rule guards the
/// one commit that changes what is checked and what runs.
fn is_product_path(file: &str) -> bool {
    if is_gate_path(file) {
        return false;
    }
    if file.starts_with("docs/") || file.contains("/docs/") {
        return false;
    }
    let name = file.rsplit('/').next().unwrap_or(file);
    if name == "AGENTS.md" {
        return false;
    }
    !matches!(
        file.rsplit('.').next().unwrap_or(""),
        "md" | "txt" | "json" | "yaml" | "yml"
    )
}

/// A commit that changes a gate and product code at once.
fn disarms_a_gate(files: &[String]) -> bool {
    files.iter().any(|f| is_gate_path(f)) && files.iter().any(|f| is_product_path(f))
}

/// Whether a commit message carries a `Verify:` footer.
fn has_verify_footer(commit: &crate::git::Commit) -> bool {
    commit
        .body
        .lines()
        .chain(std::iter::once(commit.subject.as_str()))
        .any(|line| line.trim_start().starts_with("Verify:"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use globset::GlobBuilder;

    fn matcher(pattern: &str) -> GlobMatcher {
        GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .expect("valid glob")
            .compile_matcher()
    }

    #[test]
    fn parses_an_integer() {
        let value = parse_counter_output("42\n").expect("parses");
        assert_eq!(value.count, Some(42));
        assert!(value.items.is_none());
    }

    #[test]
    fn parses_an_array_of_strings() {
        let value = parse_counter_output(r#"["a.ts", "b.ts"]"#).expect("parses");
        assert_eq!(value.count, None);
        assert_eq!(value.items.unwrap(), vec!["a.ts", "b.ts"]);
    }

    #[test]
    fn parses_an_object_with_a_count() {
        let value = parse_counter_output(r#"{"count": 7}"#).expect("parses");
        assert_eq!(value.count, Some(7));
    }

    #[test]
    fn parses_an_object_with_a_count_and_items() {
        let value = parse_counter_output(r#"{"count": 2, "items": ["a", "b"]}"#).expect("parses");
        assert_eq!(value.count, Some(2));
        assert_eq!(value.items.unwrap().len(), 2);
    }

    #[test]
    fn rejects_unparseable_output() {
        assert!(parse_counter_output("not a number").is_err());
        assert!(parse_counter_output("").is_err());
        assert!(parse_counter_output(r#"{"nope": 1}"#).is_err());
    }

    #[test]
    fn parses_per_file_measurements_and_rejects_unsafe_or_duplicate_paths() {
        let value = parse_counter_output(r#"{"measurements":[{"path":"src/a.java","value":3}]}"#)
            .expect("measurement parses");
        assert_eq!(
            value.measurements.expect("measurement list")[0],
            Measurement {
                path: "src/a.java".to_string(),
                value: 3
            }
        );
        for input in [
            r#"{"measurements":[{"path":"/tmp/a","value":1}]}"#,
            r#"{"measurements":[{"path":"../a","value":1}]}"#,
            r#"{"measurements":[{"path":"a","value":-1}]}"#,
            r#"{"measurements":[{"path":"a","value":1},{"path":"a","value":2}]}"#,
        ] {
            assert!(parse_counter_output(input).is_err(), "{input}");
        }
    }

    #[test]
    fn line_metrics_follow_crlf_and_unicode_scalar_semantics() {
        assert_eq!(line_metrics(""), (0, 0));
        assert_eq!(line_metrics("a\r\nb\n"), (2, 1));
        assert_eq!(line_metrics("é🙂\n"), (1, 2));
        assert_eq!(line_metrics("a\r\rb"), (3, 1));
    }

    #[test]
    fn count_and_max_reduce_file_measurements() {
        let value = CounterValue {
            count: None,
            items: None,
            measurements: Some(vec![
                Measurement {
                    path: "a".to_string(),
                    value: 4,
                },
                Measurement {
                    path: "b".to_string(),
                    value: 9,
                },
            ]),
        };
        assert_eq!(
            finalize_measurements(value.clone(), crate::config::Aggregate::Count).expect("count"),
            2
        );
        assert_eq!(
            finalize_measurements(value, crate::config::Aggregate::Max).expect("max"),
            9
        );
    }

    #[test]
    fn file_metrics_filter_globs_and_measure_bytes_and_lines() {
        let root = crate::test_tempdir::TempDir::new().expect("temporary root");
        fs::create_dir_all(root.path().join("src")).expect("source directory");
        fs::write(root.path().join("src/A.java"), "é\r\n🙂🙂\n").expect("Java source");
        fs::write(root.path().join("src/B.txt"), "ignored").expect("nonmatching file");
        fs::write(root.path().join("Top.java"), "x").expect("root-level Java source");
        let patterns = vec!["**/*.java".to_string()];
        let size = run_file_metrics(&patterns, FileMetric::SizeBytes, root.path(), "test")
            .expect("size metric");
        assert_eq!(
            size.value
                .measurements
                .as_ref()
                .expect("measurements")
                .len(),
            2
        );
        let nested_size = size
            .value
            .measurements
            .as_ref()
            .expect("measurements")
            .iter()
            .find(|measurement| measurement.path == "src/A.java")
            .expect("nested Java file")
            .value;
        assert_eq!(nested_size, 13);
        let lines = run_file_metrics(&patterns, FileMetric::LineCount, root.path(), "test")
            .expect("line metric");
        assert_eq!(
            lines
                .value
                .measurements
                .as_ref()
                .expect("measurements")
                .iter()
                .find(|measurement| measurement.path == "src/A.java")
                .expect("nested Java file")
                .value,
            2
        );
        let longest = run_file_metrics(&patterns, FileMetric::MaxLineLength, root.path(), "test")
            .expect("line length metric");
        assert_eq!(
            longest
                .value
                .measurements
                .as_ref()
                .expect("measurements")
                .iter()
                .find(|measurement| measurement.path == "src/A.java")
                .expect("nested Java file")
                .value,
            2
        );
    }

    #[cfg(unix)]
    #[test]
    fn file_metrics_ignore_unselected_symlink_files() {
        let root = crate::test_tempdir::TempDir::new().expect("temporary root");
        let outside = crate::test_tempdir::TempDir::new().expect("outside root");
        fs::write(outside.path().join("A.java"), "x").expect("outside source");
        fs::create_dir_all(root.path().join("src")).expect("source directory");
        fs::write(root.path().join("src/A.rs"), "fn main() {}\n").expect("selected source");
        std::os::unix::fs::symlink(outside.path().join("A.java"), root.path().join("other.txt"))
            .expect("link");
        std::os::unix::fs::symlink(
            root.path().join("missing"),
            root.path().join("dangling.txt"),
        )
        .expect("dangling link");
        let result = run_file_metrics(
            &["src/**/*.rs".to_string()],
            FileMetric::LineCount,
            root.path(),
            "test",
        )
        .expect("unselected symlink is ignored");
        assert_eq!(result.value.measurements.expect("measurements").len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn file_metrics_refuse_selected_symlink_files() {
        let root = crate::test_tempdir::TempDir::new().expect("temporary root");
        let outside = crate::test_tempdir::TempDir::new().expect("outside root");
        fs::write(outside.path().join("A.rs"), "fn main() {}\n").expect("outside source");
        fs::create_dir_all(root.path().join("src")).expect("source directory");
        std::os::unix::fs::symlink(outside.path().join("A.rs"), root.path().join("src/A.rs"))
            .expect("link");
        let error = run_file_metrics(
            &["src/**/*.rs".to_string()],
            FileMetric::SizeBytes,
            root.path(),
            "test",
        )
        .expect_err("symlinks are refused");
        assert!(error.message.contains("symbolic links"));
    }

    #[cfg(unix)]
    #[test]
    fn file_metrics_refuse_unselected_symlink_directories() {
        let root = crate::test_tempdir::TempDir::new().expect("temporary root");
        let outside = crate::test_tempdir::TempDir::new().expect("outside root");
        fs::write(outside.path().join("A.rs"), "fn main() {}\n").expect("outside source");
        std::os::unix::fs::symlink(outside.path(), root.path().join("linked"))
            .expect("directory link");
        let error = run_file_metrics(
            &["src/**/*.rs".to_string()],
            FileMetric::SizeBytes,
            root.path(),
            "test",
        )
        .expect_err("symlink directories fail closed");
        assert!(error.message.contains("symbolic links"));
    }

    #[test]
    fn the_message_of_a_counter_error_is_ratchets_own() {
        let error = parse_counter_output("not a number").expect_err("fails");
        assert_eq!(error.message, "the counter did not print a count");
    }

    #[test]
    fn counts_the_items_of_an_array() {
        let value = parse_counter_output(r#"["a.ts", "b.ts"]"#).expect("parses");
        let finalized = finalize(value, &[], "rules.a.exclude").expect("counts");
        assert_eq!(finalized.count, 2);
        assert!(finalized.unmatched_excludes.is_empty());
    }

    #[test]
    fn exclude_removes_items_from_the_count_and_names_a_dead_pattern() {
        let value =
            parse_counter_output(r#"["src/a.ts", "src/b.ts", "docs/c.ts"]"#).expect("parses");
        let finalized = finalize(
            value,
            &[vec![matcher("src/**")], vec![matcher("vendor/**")]],
            "rules.a.exclude",
        )
        .expect("counts");
        assert_eq!(finalized.count, 1);
        assert_eq!(finalized.unmatched_excludes, vec![1]);
    }

    #[test]
    fn grouped_exclude_names_the_dead_pattern_after_an_expansion() {
        // `a/**` expands into two matchers; the group keeps them attached to
        // the pattern, so `dead/**` is the one that is reported.
        let exclude = crate::config::compile_globs_grouped(
            &[
                "a/**".to_string(),
                "b/**".to_string(),
                "dead/**".to_string(),
            ],
            "rules.a.exclude",
        )
        .expect("valid globs");
        assert_eq!(exclude.len(), 3, "one group per pattern");
        assert_eq!(exclude[0].len(), 2, "`a/**` expands to the directory too");
        let value = parse_counter_output(r#"["a/one.ts", "b/two.ts"]"#).expect("parses");
        let finalized = finalize(value, &exclude, "rules.a.exclude").expect("counts");
        assert_eq!(finalized.count, 0);
        assert_eq!(finalized.unmatched_excludes, vec![2]);
    }

    #[test]
    fn exclude_on_a_scalar_counter_is_a_config_error() {
        let value = parse_counter_output("5").expect("parses");
        let error =
            finalize(value, &[vec![matcher("src/**")]], "rules.a.exclude").expect_err("fails");
        assert!(error.is_config);
        assert!(error.message.contains("rules.a.exclude"));
    }

    #[test]
    fn a_negative_count_is_an_error() {
        let value = parse_counter_output("-3").expect("parses");
        let error = finalize(value, &[], "rules.a.exclude").expect_err("fails");
        assert_eq!(error.message, "the counter printed a negative count");
    }

    #[test]
    fn a_star_does_not_cross_a_path_separator() {
        assert!(matcher("src/*").is_match("src/a.ts"));
        assert!(!matcher("src/*").is_match("src/nested/a.ts"));
        assert!(matcher("src/**").is_match("src/nested/a.ts"));
    }

    #[test]
    fn a_rule_file_with_the_wrong_case_is_the_defect() {
        assert!(is_misnamed_rule_file("Agents.md"));
        assert!(is_misnamed_rule_file("pkg/agents.md"));
        assert!(!is_misnamed_rule_file("AGENTS.md"));
        assert!(!is_misnamed_rule_file("pkg/AGENTS.md"));
        assert!(!is_misnamed_rule_file("README.md"));
    }

    #[test]
    fn documentation_and_rule_files_are_not_product_code() {
        assert!(!is_product_path("docs/plans/a.md"));
        assert!(!is_product_path("src/AGENTS.md"));
        assert!(!is_product_path(".github/workflows/ci.yml"));
        assert!(is_product_path("src/main.rs"));
        assert!(is_product_path("scripts/deploy.sh"));
    }

    #[test]
    fn a_commit_touching_a_gate_and_code_is_flagged() {
        let both = vec![
            "scripts/check_sizes.sh".to_string(),
            "src/main.rs".to_string(),
        ];
        assert!(disarms_a_gate(&both));
        let gate_only = vec![".github/workflows/ci.yml".to_string()];
        assert!(!disarms_a_gate(&gate_only));
        let code_only = vec!["src/main.rs".to_string()];
        assert!(!disarms_a_gate(&code_only));
        let gate_and_docs = vec![
            ".githooks/pre-commit".to_string(),
            "docs/agents.md".to_string(),
        ];
        assert!(!disarms_a_gate(&gate_and_docs));
    }

    #[test]
    fn a_plan_without_a_status_line_is_counted() {
        let root = std::env::temp_dir().join(format!("ratchet-plans-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("mkdir");
        let root = root.as_path();
        fs::write(root.join("with.md"), "# T\n**Status:** Plan\n").expect("write");
        fs::write(root.join("without.md"), "# T\nkein Status\n").expect("write");
        fs::write(root.join("ignored.txt"), "kein Status\n").expect("write");
        assert_eq!(plans_without_status(root), 1);
        assert_eq!(plans_without_status(&root.join("missing")), 0);
        let _ = fs::remove_dir_all(root);
    }
}
