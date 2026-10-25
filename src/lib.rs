//! `ratchet` — quality gates that compare a **count** against a **baseline**
//! that may only move in one declared direction.
//!
//! The engine owns the baseline, the direction, the monotonicity and the
//! reporting; the project owns the counting.  See `SPEC.md` for the
//! normative description of the behaviour.

pub mod baseline;
pub mod check;
pub mod config;
pub mod counter;
pub mod diff;
pub mod drift;
pub mod fsutil;
pub mod generate;
pub mod git;
pub mod issues;
pub mod path;
pub mod registry;
pub mod release;
pub mod report;
pub mod stale;
pub mod update;
pub mod update_check;
pub mod verify;
pub mod version;

#[cfg(test)]
#[path = "../tests/common/tempdir.rs"]
pub(crate) mod test_tempdir;

use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::baseline::{Baseline, BaselineRecord};
use crate::check::{ActionKind, BaselineAction, Report};
use crate::config::{CONFIG_FILE_NAMES, Config, DEFAULT_BASELINE_FILE, Direction, Rule, Tier};
use crate::git::Git;

/// The name of the config `ratchet init` writes.
const CONFIG_FILE_STARTER: &str = "ratchet.config.jsonc";

/// The starter config `ratchet init` writes.
const STARTER_CONFIG: &str = r#"// ratchet — starter config.
//
// One rule per defect class.  The example counts TODO markers in the
// repository: the baseline may only decrease, never grow.
//
// The incident of a rule names the instance that motivated it.  A rule for a
// class that has not occurred yet is a prevention rule, and its incident says
// so — a gate must not claim a history it does not have.
[
  {
    "name": "starter",
    "rules": {
      "todo-markers": {
        "class": "unfinished work",
        "incident": "prevention: no instance in this project yet — replace this with the incident that motivated the rule",
        "unit": "markers",
        "direction": "ceiling",
        "tier": "fast",
        "count": {
          "command": [
            "sh",
            "-c",
            "grep -rI --include='*.ts' --include='*.tsx' -o 'TODO' . 2>/dev/null | wc -l | tr -d '[:space:]'"
          ]
        }
      }
    }
  }
]
"#;

/// An operational error; every one of them is exit code 2.
#[derive(Debug)]
pub struct Error {
    message: String,
    /// The raw output of a subprocess, if one was involved; the diagnostic
    /// channel carries it (SPEC 6.2).
    output: Option<String>,
}

impl Error {
    /// Creates an error with the given message.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            output: None,
        }
    }

    /// Creates an error with the raw output of a subprocess.
    ///
    /// The output belongs to the diagnostic channel (SPEC 6.2): the report
    /// carries ratchet's own sentence, never what a subprocess printed.
    pub fn with_output(message: impl Into<String>, output: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            output: Some(output.into()),
        }
    }

    /// The message of the error.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The raw output of a subprocess, if one was involved.
    pub fn output(&self) -> Option<&str> {
        self.output.as_deref()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for Error {}

impl From<config::ConfigError> for Error {
    fn from(error: config::ConfigError) -> Self {
        Self {
            message: error.to_string(),
            output: error.output,
        }
    }
}

impl From<baseline::BaselineError> for Error {
    fn from(error: baseline::BaselineError) -> Self {
        Self::new(error.to_string())
    }
}

impl From<update::Error> for Error {
    fn from(error: update::Error) -> Self {
        Self::new(error.message())
    }
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::new(error.to_string())
    }
}

/// Everything a run needs: the config, the repository root and the baseline.
pub struct Context {
    /// The merged config.
    pub config: Config,
    /// The config file that was loaded.
    pub config_path: PathBuf,
    /// The repository root (the git root, or the config directory).
    pub root: PathBuf,
    /// The absolute path of the baseline file.
    pub baseline_path: PathBuf,
    /// The baseline path relative to the repository root, for `git show`.
    pub baseline_rel: Option<String>,
    /// The git repository, if there is one.
    pub git: Git,
}

/// Runs the CLI and returns the exit code.
pub fn run(args: impl IntoIterator<Item = OsString>) -> i32 {
    let args: Vec<OsString> = args.into_iter().collect();
    let cli = Cli::parse_from(normalize_args(args));
    let debug = cli.debug;
    match run_inner(cli) {
        Ok(code) => code,
        Err(error) => {
            // The error comes first; the transcript explains it afterwards.
            eprintln!("ratchet: error: {error}");
            // The raw output of a subprocess belongs to the diagnostic channel
            // (SPEC 6.2): it is printed only with --debug.
            if debug {
                if let Some(output) = error.output() {
                    eprintln!("ratchet: debug: {output}");
                }
            }
            2
        }
    }
}

fn run_inner(cli: Cli) -> Result<i32, Error> {
    let cwd = std::env::current_dir()
        .map_err(|e| Error::new(format!("cannot read the working directory: {e}")))?;

    match &cli.command {
        Command::Check(args) => cmd_check(&cli, args, &cwd),
        Command::Count(_) => cmd_count(&cli, &cwd),
        Command::Baseline(args) => match &args.command {
            BaselineCommand::Init(args) => cmd_baseline(&cli, &cwd, false, args.reason.as_deref()),
            BaselineCommand::Update(args) => cmd_baseline(&cli, &cwd, true, args.reason.as_deref()),
            BaselineCommand::Path(_) => cmd_baseline_path(&cli, &cwd),
            BaselineCommand::Raise(args) => cmd_baseline_raise(&cli, args, &cwd),
        },
        Command::Drift(args) => cmd_drift(&cli, args, &cwd),
        Command::Verify(args) => cmd_verify(&cli, args, &cwd),
        Command::Report(args) => cmd_report(&cli, args, &cwd),
        Command::Release(args) => cmd_release(&cli, args, &cwd),
        Command::Init(_) => cmd_init(&cli, &cwd),
        Command::Update(args) => cmd_update(&cli, args),
    }
}

/// Compiles the `--ignore` patterns.
///
/// An invalid pattern is an error: a pattern that cannot be compiled would
/// silently ignore nothing.
fn compile_ignore(patterns: &[String]) -> Result<Vec<globset::GlobMatcher>, Error> {
    config::compile_globs(patterns, "ignore")
        .map_err(|error| Error::new(format!("--ignore: {}", error.message)))
}

fn cmd_check(cli: &Cli, args: &CheckArgs, cwd: &Path) -> Result<i32, Error> {
    let context = load_context(cli, cwd)?;
    ensure_rule_exists(&context.config, cli.rule.as_deref())?;
    ensure_paths_exist(&args.paths, cwd, &context.root)?;

    let paths = check::selected_paths(&args.paths, cwd, &context.root);
    let report = run_gates(
        cli,
        &context,
        &Run {
            paths: &paths,
            tier: args.tier,
            fix: args.fix,
            drift: true,
        },
    )?;
    match format_of(cli)? {
        Output::Json => println!("{}", report::render_json(&report)),
        Output::Stylish => print!("{}", report::render_stylish(&report, cli.quiet)),
    }
    print_notes(&report.notes, &report.debug, cli);
    check_for_a_new_version(cli);

    Ok(exit_code(&report))
}

/// What a run selects and what it leaves out.
#[derive(Default)]
struct Run<'a> {
    /// The paths the run is scoped to.
    paths: &'a [String],
    /// The tier the run executes.
    tier: Option<Tier>,
    /// The produced content is written into the generated files.
    fix: bool,
    /// The run executes the drift gates.
    ///
    /// The report does not: the drift gates are gates over the *generated*
    /// files, and the report — which is a generated file — cannot be part of
    /// a comparison with itself (SPEC 14).
    drift: bool,
}

/// Runs the gates: the drift gates, the counters, the comparison with the
/// baseline and the floors of the tiers.
///
/// The check and the report use the **same** function: the numbers of the
/// report are the numbers of the run, and a report that counted the gates
/// again could contradict them (SPEC 14).
fn run_gates(cli: &Cli, context: &Context, run: &Run<'_>) -> Result<check::Report, Error> {
    let baseline = Baseline::load_if_exists(Some(&context.root), &context.baseline_path)?;
    let ignore = compile_ignore(&cli.ignore)?;

    // The drift gates run first: a generator that cannot be run is an
    // operational error, and the tree has to be whole before a counter reads
    // it (SPEC 5.2).
    let drift = if run.drift {
        drift::run(
            &context.config,
            &context.git,
            &drift::Options {
                root: &context.root,
                fix: run.fix,
                path: None,
                // The drift gates are tiered like the rules: a slow generator
                // does not run in the fast tier (SPEC 7).
                tier: run.tier,
            },
        )?
    } else {
        // The report runs no drift gate: the drift gates' verdicts belong to
        // the check (SPEC 14).
        drift::Outcome::default()
    };

    let mut notes = Vec::new();
    // A committed baseline that exists but cannot be parsed is an operational
    // error: a broken reference must not disable the gate (SPEC 5.2, 8).
    let committed = read_committed_baseline(context)?;
    if let Some(note) = check::monotonicity_note(
        &context.git,
        context
            .baseline_rel
            .as_deref()
            .unwrap_or(DEFAULT_BASELINE_FILE),
        committed.as_ref(),
    ) {
        notes.push(note);
    }

    let selection = check::Selection {
        rule: cli.rule.as_deref(),
        tier: run.tier,
        paths: run.paths,
        ignore: &ignore,
        strict: cli.strict,
    };
    let mut report = check::check(
        &context.config,
        &baseline,
        committed.as_ref(),
        &context.root,
        &selection,
    );
    report.generated = drift.entries;
    report.notes.extend(notes);
    report.notes.extend(drift.notes);
    report.debug.extend(drift.debug);

    // The floor of every tier that runs is enforced (SPEC 3.3).
    let tier_notes = check::tier_notes(&context.config, &selection, &report);
    report.notes.extend(tier_notes.iter().cloned());

    if report.checks() == 0 && tier_notes.is_empty() {
        report
            .notes
            .push("no check was executed — an empty selection is never a pass".to_string());
    }
    if report
        .rules
        .iter()
        .any(|rule| rule.status == check::RuleStatus::MissingBaseline)
    {
        report
            .notes
            .push("hint: run `ratchet baseline init` to write the missing baseline".to_string());
    }
    if report
        .rules
        .iter()
        .any(|rule| rule.status == check::RuleStatus::CounterError)
        && report.counter_errors > report.empty_scope_errors
    {
        report
            .notes
            .push("hint: run with --debug to see the output of the counter".to_string());
    }

    report.ok = report.failed() == 0
        && report.counter_errors == 0
        && report.stale_errors == 0
        && report.drifted() == 0
        && report.checks() > 0
        && tier_notes.is_empty();

    if cli.debug {
        report.debug.insert(0, resolved_config(&context.config));
    }

    Ok(report)
}

/// Renders the run into a Markdown report (SPEC 14).
///
/// The report is a **rendering of a run**: the same counters and the same
/// numbers.  It runs the **rules** only: the drift gates are gates over the
/// *generated* files, and the report — which is a generated file — cannot be
/// part of a comparison with itself.
///
/// The exit code is the exit code of the **command**: 0 when the report was
/// produced, 2 when it could not be produced.  The verdict of the run is in
/// the **content**: the report is a generator, and a generator that failed
/// because the run is red would fail the drift gate that invoked it.
fn cmd_report(cli: &Cli, args: &ReportArgs, cwd: &Path) -> Result<i32, Error> {
    let context = load_context(cli, cwd)?;
    ensure_rule_exists(&context.config, cli.rule.as_deref())?;
    report_format(cli)?;

    // The path of the report, relative to the repository root.
    let output = match &args.output {
        Some(path) => Some(report_path(&context.root, path)?),
        None => None,
    };

    // The report runs the rules only: the drift gates are gates over the
    // generated files, and the report is one of them (SPEC 14).
    let report = run_gates(
        cli,
        &context,
        &Run {
            paths: &[],
            tier: None,
            // The report writes one file and changes nothing else.
            fix: false,
            // The report runs no drift gate.
            drift: false,
        },
    )?;
    let markdown = report::render_markdown(&report);

    match output {
        None => {
            // The report is the only thing on stdout.
            print!("{markdown}");
        }
        Some((full, relative)) => {
            // The report writes one file and nothing else: the directory of
            // the report has to exist, the command does not create it.
            if let Some(parent) = full.parent() {
                if !parent.is_dir() {
                    return Err(Error::new(format!(
                        "cannot write {}: the directory {} does not exist",
                        relative,
                        path::display(Some(&context.root), parent)
                    )));
                }
            }
            fs::write(&full, &markdown).map_err(|e| {
                Error::new(format!(
                    "cannot write {}: {e}",
                    path::display(Some(&context.root), &full)
                ))
            })?;
            println!("ratchet: wrote {relative}");
        }
    }
    print_notes(&report.notes, &report.debug, cli);
    check_for_a_new_version(cli);

    // The report was produced: the command succeeded.  The verdict of the
    // run is in the report, not in the exit code — except under `--strict`,
    // where stale artifacts are an operational error (SPEC 5.2).
    Ok(if report.stale_errors > 0 { 2 } else { 0 })
}

/// Computes the next version and the changelog of the release (SPEC §16).
fn cmd_release(cli: &Cli, args: &ReleaseArgs, cwd: &Path) -> Result<i32, Error> {
    let context = load_context(cli, cwd)?;
    // The release command runs no rule: the option has no effect here.  It is
    // still compiled: an invalid pattern is an error everywhere.
    let _ignore = compile_ignore(&cli.ignore)?;

    let outcome = release::run(
        &context.config,
        &context.git,
        &release::Options {
            root: &context.root,
            from: args.from.as_deref(),
            dry_run: args.dry_run,
        },
    )?;

    let mut debug = Vec::new();
    if cli.debug {
        debug.push(resolved_config(&context.config));
    }

    match format_of(cli)? {
        Output::Json => {
            println!("{}", report::render_release_json(&outcome, args.dry_run));
        }
        Output::Stylish => print!("{}", report::render_release(&outcome, args.dry_run)),
    }
    print_notes(&outcome.notes, &debug, cli);

    // Nothing to release is not a failure of the tool — but it is a failure
    // of the request: the release that was asked for does not exist.
    Ok(if outcome.nothing { 1 } else { 0 })
}

/// The path of the report: the absolute path and the repository-relative one.
///
/// The report is written **inside** the repository root: a path that points
/// outside it would let the engine write outside the tree it owns, and the
/// report is a file of the repository — the drift gate reads it there.
///
/// The path is **relative** — the paths of a project are relative (SPEC §6)
/// — and the check resolves it: a symbolic link inside the root can point
/// outside it, and the report would follow the link.
fn report_path(root: &Path, raw: &str) -> Result<(PathBuf, String), Error> {
    if let Err(refusal) = path::inside_repository(root, raw) {
        return Err(Error::new(format!(
            "config error at --output: {}",
            refusal.message("the report", "must be written inside the repository root")
        )));
    }
    let full = path::normalize(&root.join(Path::new(raw)));
    let relative = path::relative_to(root, &full).unwrap_or_default();
    if relative.is_empty() {
        return Err(Error::new(
            "config error at --output: the report is the repository root — it is not a file",
        ));
    }
    Ok((full, relative))
}

/// The format of a command that prints a report.
///
/// The Markdown report belongs to the `report` command: the other commands
/// print the stylish output or the JSON document.
fn format_of(cli: &Cli) -> Result<Output, Error> {
    match cli.format {
        None | Some(Format::Stylish) => Ok(Output::Stylish),
        Some(Format::Json) => Ok(Output::Json),
        Some(Format::Md) => Err(Error::new(
            "the Markdown report is written by `ratchet report --format md`",
        )),
    }
}

/// The format of the `report` command: the Markdown report and nothing else.
fn report_format(cli: &Cli) -> Result<(), Error> {
    match cli.format {
        None | Some(Format::Md) => Ok(()),
        Some(_) => Err(Error::new(
            "the report command writes the Markdown report — `ratchet report --format md`",
        )),
    }
}

/// The format of a command that prints one line and nothing else.
///
/// The `baseline path` and the `update` have no JSON document and no Markdown
/// report: a format they cannot honour is **refused**, not ignored — a
/// silently ignored argument is an argument that does not do what it says.
fn plain_format(cli: &Cli) -> Result<(), Error> {
    match cli.format {
        None | Some(Format::Stylish) => Ok(()),
        Some(Format::Json) => Err(Error::new(
            "this command prints one line — it has no JSON output",
        )),
        Some(Format::Md) => Err(Error::new(
            "the Markdown report is written by `ratchet report --format md`",
        )),
    }
}

/// Asks the release API whether there is a newer version.
///
/// The check runs at most once a day, it never blocks the gate for longer
/// than a moment and it never fails it: every error is swallowed and the
/// gate keeps its verdict (SPEC 9).
fn check_for_a_new_version(cli: &Cli) {
    if cli.no_update_check || cli.quiet || update_check::disabled() {
        return;
    }
    let Ok(current) = update::current_version() else {
        return;
    };
    if let Some(message) = update_check::run(&update::api_base(), current) {
        eprintln!("ratchet: note: {message}");
    }
}

/// Runs the drift gates: the generated files and nothing else.
fn cmd_drift(cli: &Cli, args: &DriftArgs, cwd: &Path) -> Result<i32, Error> {
    let context = load_context(cli, cwd)?;
    // `--ignore` applies to the rules' path scope; the drift command has no
    // rules, so the option has no effect here.  It is still compiled: an
    // invalid pattern is an error everywhere.
    let _ignore = compile_ignore(&cli.ignore)?;

    // The selected gate, relative to the repository root.
    let selected = match &args.path {
        Some(path) => check::selected_paths(std::slice::from_ref(path), cwd, &context.root)
            .into_iter()
            .next(),
        None => None,
    };

    let outcome = drift::run(
        &context.config,
        &context.git,
        &drift::Options {
            root: &context.root,
            fix: args.fix,
            path: selected.as_deref(),
            // The drift command runs every tier: it has no `--tier`.
            tier: None,
        },
    )?;

    let drifted = outcome.drifted();
    // An empty selection is never a pass: a drift command without a
    // generated entry runs no check at all.
    let ran = !outcome.entries.is_empty();
    let ok = drifted == 0 && ran;

    let mut debug = outcome.debug;
    if cli.debug {
        debug.insert(0, resolved_config(&context.config));
    }
    let mut notes = outcome.notes;
    if !ran {
        notes.push("no check was executed — an empty selection is never a pass".to_string());
    }

    match format_of(cli)? {
        Output::Json => {
            let report = Report {
                generated: outcome.entries,
                notes: notes.clone(),
                ok,
                ..Report::default()
            };
            println!("{}", report::render_json(&report));
        }
        Output::Stylish => print!("{}", report::render_drift(&outcome.entries, ok, cli.quiet)),
    }
    print_notes(&notes, &debug, cli);

    Ok(if ok { 0 } else { 1 })
}

/// Runs the pre-release checks: the commands whose output is compared with a
/// committed snapshot (SPEC §15).
///
/// The checks are **not** part of the commit gate: they run in the release
/// pipeline.  A measurement is not a defect class, and a permanent gate over
/// it would be noise.
fn cmd_verify(cli: &Cli, args: &VerifyArgs, cwd: &Path) -> Result<i32, Error> {
    let context = load_context(cli, cwd)?;
    // `--ignore` applies to the rules' path scope; the verify command has no
    // rules, so the option has no effect here.  It is still compiled: an
    // invalid pattern is an error everywhere.
    let _ignore = compile_ignore(&cli.ignore)?;

    let outcome = verify::run(
        &context.config,
        &context.git,
        &verify::Options {
            root: &context.root,
            update: args.update,
            name: args.name.as_deref(),
        },
    )?;

    let drifted = outcome.drifted();
    // An empty selection is never a pass: a verify command without a check
    // runs no check at all.
    let ran = !outcome.entries.is_empty();
    let ok = drifted == 0 && ran;

    let mut debug = outcome.debug;
    if cli.debug {
        debug.insert(0, resolved_config(&context.config));
    }
    let mut notes = outcome.notes;
    if !ran {
        notes.push("no check was executed — an empty selection is never a pass".to_string());
    }

    match format_of(cli)? {
        Output::Json => {
            println!(
                "{}",
                report::render_verify_json(&outcome.entries, ok, &notes)
            );
        }
        Output::Stylish => print!("{}", report::render_verify(&outcome.entries, ok, cli.quiet)),
    }
    print_notes(&notes, &debug, cli);

    Ok(if ok { 0 } else { 1 })
}

/// The resolved config: the first line of the diagnostic channel.
fn resolved_config(config: &Config) -> String {
    format!(
        "resolved config: {}",
        serde_json::to_string_pretty(config).unwrap_or_default()
    )
}

fn cmd_count(cli: &Cli, cwd: &Path) -> Result<i32, Error> {
    let context = load_context(cli, cwd)?;
    ensure_rule_exists(&context.config, cli.rule.as_deref())?;
    let mut report = check::counts(&context.config, &context.root, cli.rule.as_deref());
    if cli.debug {
        report.debug.insert(0, resolved_config(&context.config));
    }
    if report
        .rules
        .iter()
        .any(|rule| rule.status == check::RuleStatus::CounterError)
        && report.counter_errors > report.empty_scope_errors
    {
        report
            .notes
            .push("hint: run with --debug to see the output of the counter".to_string());
    }

    match format_of(cli)? {
        Output::Json => println!("{}", report::render_json(&report)),
        Output::Stylish => print!("{}", report::render_counts(&report)),
    }
    print_notes(&report.notes, &report.debug, cli);

    if report.counter_errors > 0 {
        Ok(2)
    } else {
        Ok(0)
    }
}

/// The stale artifacts that a `baseline init --strict` refuses to document.
///
/// The pre-flight detects the baseline records the config no longer has and
/// runs the counters of the per-file measurement rules: a configured scope
/// that selects no files is only visible after the measurement ran
/// (SPEC 5.4).  The successful runs are handed back so that the init does not
/// count a rule twice.  A counter failure is an operational error: the
/// baseline is not documented on a tree whose count is unknown.
fn preflight_stale(
    context: &Context,
    baseline: &Baseline,
    rule_filter: Option<&str>,
    runs: &mut std::collections::BTreeMap<String, counter::CounterRun>,
) -> Result<Vec<stale::Stale>, Error> {
    let mut artifacts = Vec::new();
    if rule_filter.is_none() {
        artifacts.extend(stale::unknown_rules(&context.config, baseline));
    }
    for (id, rule) in &context.config.rules {
        if rule_filter.is_some_and(|filter| filter != id) {
            continue;
        }
        artifacts.extend(stale::unknown_scopes(rule, baseline));
        if rule.scopes.is_empty() && rule.aggregate.is_none() {
            continue;
        }
        let run = counter::run_counter(&rule.count, &context.root, &format!("rules.{id}.count"))
            .map_err(|error| Error::new(error.message))?;
        let measurements = run.value.measurements.as_ref().ok_or_else(|| {
            Error::new(format!(
                "config error: a rule with aggregate or scopes must emit per-file measurements \
                 (rules.{id}.count)"
            ))
        })?;
        let scoped =
            check::scoped_counts(rule, measurements).map_err(|error| Error::new(error.message))?;
        for (scope, description) in scoped.empty {
            artifacts.push(stale::Stale::EmptyScope {
                rule: id.clone(),
                scope,
                description,
            });
        }
        runs.insert(id.clone(), run);
    }
    Ok(artifacts)
}

fn cmd_baseline(cli: &Cli, cwd: &Path, update: bool, reason: Option<&str>) -> Result<i32, Error> {
    let context = load_context(cli, cwd)?;
    let mut baseline = Baseline::load_if_exists(Some(&context.root), &context.baseline_path)?;

    ensure_rule_exists(&context.config, cli.rule.as_deref())?;
    let mut actions: Vec<BaselineAction> = Vec::new();
    let mut failed = 0usize;
    let mut counter_errors = 0usize;
    let mut changed = false;
    let mut debug: Vec<String> = Vec::new();

    // `baseline init --strict` validates before it writes: the baseline is
    // only documented on a tree that carries no leftovers (SPEC 5.4).  The
    // counters of the per-file measurement rules run once; a successful run
    // is handed to the initialization below.
    let mut preflight: std::collections::BTreeMap<String, counter::CounterRun> =
        std::collections::BTreeMap::new();
    if !update && cli.strict {
        let artifacts = preflight_stale(&context, &baseline, cli.rule.as_deref(), &mut preflight)?;
        if !artifacts.is_empty() {
            let messages: Vec<String> = artifacts
                .iter()
                .map(|artifact| artifact.check_note(true))
                .collect();
            return Err(Error::new(messages.join("\n")));
        }
    }

    // A baseline rule record for a rule the config no longer has is stale.
    // The update prunes it; the init documents it and leaves it be; the
    // strict init refused above.
    if cli.rule.is_none() {
        for artifact in stale::unknown_rules(&context.config, &baseline) {
            let stale::Stale::UnknownRule { rule } = &artifact else {
                continue;
            };
            if update {
                baseline.rules.remove(rule);
                changed = true;
                actions.push(BaselineAction {
                    id: rule.clone(),
                    kind: ActionKind::Pruned,
                    value: None,
                    message: artifact.update_pruned(),
                });
            } else if !cli.strict {
                actions.push(BaselineAction {
                    id: rule.clone(),
                    kind: ActionKind::Ignored,
                    value: None,
                    message: artifact.init_ignored(),
                });
            }
        }
    }

    for (id, rule) in &context.config.rules {
        if let Some(filter) = &cli.rule {
            if filter != id {
                continue;
            }
        }

        // A baseline scope record for a scope this rule no longer configures
        // is stale.  It is config-level: it is pruned (or ignored) even when
        // the counter fails below.
        let unknown_scopes = stale::unknown_scopes(rule, &baseline);
        if update {
            for artifact in unknown_scopes {
                let stale::Stale::UnknownScope { scope, .. } = &artifact else {
                    continue;
                };
                let removed = baseline
                    .rules
                    .get_mut(id)
                    .and_then(|record| record.scopes.remove(scope));
                if let Some(record) = removed {
                    changed = true;
                    actions.push(BaselineAction {
                        id: artifact.action_id(),
                        kind: ActionKind::Pruned,
                        value: Some(record.value),
                        message: artifact.update_pruned(),
                    });
                }
            }
        } else if !cli.strict {
            for artifact in unknown_scopes {
                let value = match &artifact {
                    stale::Stale::UnknownScope { scope, .. } => baseline
                        .get(id)
                        .and_then(|record| record.scopes.get(scope))
                        .map(|record| record.value),
                    _ => None,
                };
                actions.push(BaselineAction {
                    id: artifact.action_id(),
                    kind: ActionKind::Ignored,
                    value,
                    message: artifact.init_ignored(),
                });
            }
        }

        let existing = baseline.get(id).and_then(|record| record.value);

        // `baseline init` only writes the rules that have no baseline: the
        // counters of the other rules are not needed and a broken counter
        // must not fail the command (SPEC 5.4).
        if !update && rule.scopes.is_empty() && rule.aggregate.is_none() {
            if let Some(value) = existing {
                actions.push(BaselineAction {
                    id: id.clone(),
                    kind: ActionKind::Kept,
                    value: Some(value),
                    message: format!("kept baseline {value}"),
                });
                continue;
            }
        }

        let counter_path = format!("rules.{id}.count");
        let run = match preflight.remove(id) {
            Some(run) => run,
            None => match counter::run_counter(&rule.count, &context.root, &counter_path) {
                Ok(run) => run,
                Err(error) => {
                    counter_errors += 1;
                    failed += 1;
                    debug.push(format!(
                        "counter {id}: {}\n{}",
                        rule.count.describe(),
                        error.output.clone().unwrap_or_default()
                    ));
                    actions.push(BaselineAction {
                        id: id.clone(),
                        kind: ActionKind::Refused,
                        value: None,
                        message: error.message.clone(),
                    });
                    continue;
                }
            },
        };
        debug.push(format!(
            "counter {id}: {}\n{}\n--- count ---",
            rule.count.describe(),
            run.output
        ));
        if let Some(measurements) = run.value.measurements.as_ref() {
            let scoped = match check::scoped_counts(rule, measurements) {
                Ok(scoped) => scoped,
                Err(error) => {
                    counter_errors += 1;
                    failed += 1;
                    actions.push(BaselineAction {
                        id: id.clone(),
                        kind: ActionKind::Refused,
                        value: None,
                        message: error.message,
                    });
                    continue;
                }
            };
            // A configured scope that selects no file is stale: the update
            // prunes its record and never pays it down; the init writes no
            // record for it (SPEC 5.5, 5.4).
            for (scope_id, description) in &scoped.empty {
                let artifact = stale::Stale::EmptyScope {
                    rule: id.clone(),
                    scope: scope_id.clone(),
                    description: description.clone(),
                };
                if update {
                    let removed = baseline
                        .rules
                        .get_mut(id)
                        .and_then(|record| record.scopes.remove(scope_id));
                    if let Some(record) = removed {
                        changed = true;
                        actions.push(BaselineAction {
                            id: artifact.action_id(),
                            kind: ActionKind::Pruned,
                            value: Some(record.value),
                            message: artifact.update_pruned(),
                        });
                    }
                } else {
                    actions.push(BaselineAction {
                        id: artifact.action_id(),
                        kind: ActionKind::Ignored,
                        value: None,
                        message: artifact.init_ignored(),
                    });
                }
            }
            let global_value = scoped
                .entries
                .iter()
                .find(|(scope, _, _)| scope.is_none())
                .map(|(_, _, value)| *value);
            if baseline.get(id).is_none() {
                baseline.rules.insert(
                    id.clone(),
                    BaselineRecord {
                        value: global_value,
                        reason: reason.map(str::to_string),
                        scopes: Default::default(),
                    },
                );
                changed = true;
            }
            for (scope_id, description, value) in scoped.entries {
                if let Some(scope_id) = scope_id {
                    let current = baseline
                        .get(id)
                        .and_then(|record| record.scopes.get(&scope_id))
                        .cloned();
                    let label = format!("{id}#{scope_id}");
                    if !update && current.is_some() {
                        actions.push(BaselineAction {
                            id: label,
                            kind: ActionKind::Kept,
                            value: current.map(|record| record.value),
                            message: format!("kept scope {description}"),
                        });
                        continue;
                    }
                    if update && current.is_none() {
                        failed += 1;
                        actions.push(BaselineAction { id: label, kind: ActionKind::Refused, value: None, message: format!("refusing: no baseline for scope {scope_id} — run ratchet baseline init") });
                        continue;
                    }
                    let allowed = current.as_ref().is_some_and(|record| match rule.direction {
                        Direction::Ceiling => value <= record.value,
                        Direction::Floor => value >= record.value,
                    });
                    if current.is_some() && !allowed {
                        failed += 1;
                        actions.push(BaselineAction {
                            id: label,
                            kind: ActionKind::Refused,
                            value: current.map(|record| record.value),
                            message: format!(
                                "refusing: scope count {value} moves against the rule direction"
                            ),
                        });
                        continue;
                    }
                    let record = baseline
                        .rules
                        .get_mut(id)
                        .ok_or_else(|| Error::new(format!("missing baseline record for {id}")))?;
                    let scope_reason = reason
                        .map(str::to_string)
                        .or_else(|| current.as_ref().and_then(|record| record.reason.clone()));
                    record.scopes.insert(
                        scope_id,
                        baseline::ScopeBaselineRecord {
                            value,
                            reason: scope_reason,
                        },
                    );
                    changed = true;
                    actions.push(BaselineAction {
                        id: label,
                        kind: if current.is_some() {
                            ActionKind::Updated
                        } else {
                            ActionKind::Added
                        },
                        value: Some(value),
                        message: format!(
                            "scope {description}: {}",
                            current
                                .map(|record| format!("{} → {value}", record.value))
                                .unwrap_or_else(|| format!("added {value}"))
                        ),
                    });
                } else {
                    let current = baseline.get(id).and_then(|record| record.value);
                    if !update && existing.is_some() {
                        continue;
                    }
                    if update && current.is_none() {
                        failed += 1;
                        actions.push(BaselineAction {
                            id: id.clone(),
                            kind: ActionKind::Refused,
                            value: None,
                            message: format!(
                                "refusing: no baseline for {id} — run ratchet baseline init"
                            ),
                        });
                        continue;
                    }
                    let allowed = current.is_some_and(|old| match rule.direction {
                        Direction::Ceiling => value <= old,
                        Direction::Floor => value >= old,
                    });
                    if current.is_some() && !allowed {
                        failed += 1;
                        actions.push(BaselineAction {
                            id: id.clone(),
                            kind: ActionKind::Refused,
                            value: current,
                            message: format!(
                                "refusing: count {value} moves against the rule direction"
                            ),
                        });
                        continue;
                    }
                    let record = baseline
                        .rules
                        .get_mut(id)
                        .ok_or_else(|| Error::new(format!("missing baseline record for {id}")))?;
                    record.value = Some(value);
                    if reason.is_some() {
                        record.reason = reason.map(str::to_string);
                    }
                    changed = true;
                    actions.push(BaselineAction {
                        id: id.clone(),
                        kind: if current.is_some() {
                            ActionKind::Updated
                        } else {
                            ActionKind::Added
                        },
                        value: Some(value),
                        message: current
                            .map(|old| format!("{old} → {value}"))
                            .unwrap_or_else(|| format!("added baseline {value}")),
                    });
                }
            }
            continue;
        }
        let count = match counter::finalize(
            run.value,
            &rule.exclude_globs,
            &format!("rules.{id}.exclude"),
        ) {
            Ok(finalized) => finalized.count,
            Err(error) => {
                counter_errors += 1;
                failed += 1;
                actions.push(BaselineAction {
                    id: id.clone(),
                    kind: ActionKind::Refused,
                    value: None,
                    message: error.message.clone(),
                });
                continue;
            }
        };
        debug.push(format!("counter {id}: count {count}"));
        match existing {
            None if update => {
                failed += 1;
                actions.push(BaselineAction {
                    id: id.clone(),
                    kind: ActionKind::Refused,
                    value: None,
                    message: format!("refusing: no baseline for {id} — run ratchet baseline init"),
                });
            }
            None => {
                baseline.rules.insert(
                    id.clone(),
                    BaselineRecord {
                        value: Some(count),
                        reason: reason.map(str::to_string),
                        scopes: Default::default(),
                    },
                );
                changed = true;
                actions.push(BaselineAction {
                    id: id.clone(),
                    kind: ActionKind::Added,
                    value: Some(count),
                    message: format!("added baseline {count}"),
                });
            }
            Some(value) => {
                let allowed = match rule.direction {
                    Direction::Ceiling => count <= value,
                    Direction::Floor => count >= value,
                };
                if allowed {
                    let reason = reason
                        .map(str::to_string)
                        .or_else(|| baseline.get(id).and_then(|record| record.reason.clone()));
                    baseline.rules.insert(
                        id.clone(),
                        BaselineRecord {
                            value: Some(count),
                            reason,
                            scopes: baseline
                                .get(id)
                                .map(|record| record.scopes.clone())
                                .unwrap_or_default(),
                        },
                    );
                    changed = true;
                    actions.push(BaselineAction {
                        id: id.clone(),
                        kind: ActionKind::Updated,
                        value: Some(count),
                        message: format!("{value} → {count}"),
                    });
                } else {
                    failed += 1;
                    let message = match rule.direction {
                        Direction::Ceiling => format!(
                            "refusing: count {count} > baseline {value} — a ceiling may only decrease"
                        ),
                        Direction::Floor => format!(
                            "refusing: count {count} < baseline {value} — a floor may only increase"
                        ),
                    };
                    actions.push(BaselineAction {
                        id: id.clone(),
                        kind: ActionKind::Refused,
                        value: Some(value),
                        message,
                    });
                }
            }
        }
    }

    if changed {
        baseline.save(Some(&context.root), &context.baseline_path)?;
    }

    if cli.debug {
        debug.insert(0, resolved_config(&context.config));
        for line in &debug {
            eprintln!("ratchet: debug: {line}");
        }
    }

    let command = if update { "update" } else { "init" };
    print_baseline(cli, &context, command, &actions, failed)?;

    if counter_errors > 0 {
        Ok(2)
    } else if failed > 0 {
        Ok(1)
    } else {
        Ok(0)
    }
}

/// The decision of a raise: the move, or the refusal.
enum Raise {
    /// The baseline moves against its declared direction.
    Move {
        /// The new value of the baseline.
        value: i64,
        /// The reason, which travels with the record.
        reason: String,
        /// A human readable message.
        message: String,
    },
    /// The move is refused; the baseline file stays untouched.
    Refuse {
        /// A human readable message.
        message: String,
    },
}

/// Decides what a raise of a rule does.
///
/// The command moves the baseline **against** the declared direction: a
/// ceiling goes up, a floor goes down.  Every other move is the paydown's
/// job (`baseline update`), and a raise without a reason is refused — the
/// reason is what makes the move reviewable (SPEC 4, 5.2).
///
/// `current` is the value the baseline has today, `None` when the rule has no
/// baseline yet.
fn decide_raise(rule: &Rule, current: Option<i64>, value: i64, reason: Option<&str>) -> Raise {
    let Some(reason) = reason.map(str::trim).filter(|reason| !reason.is_empty()) else {
        return Raise::Refuse {
            message:
                "refusing: a raise needs a reason — --reason \"<text>\" says why the baseline \
                 moves against its direction"
                    .to_string(),
        };
    };
    let Some(current) = current else {
        return Raise::Refuse {
            message: format!(
                "refusing: no baseline for {} — run ratchet baseline init",
                rule.id
            ),
        };
    };
    if value < 0 {
        return Raise::Refuse {
            message: format!("refusing: a baseline cannot be negative — {value} is not a count"),
        };
    }

    let against = match rule.direction {
        Direction::Ceiling => value > current,
        Direction::Floor => value < current,
    };
    if !against {
        let message = if value == current {
            format!("refusing: the baseline is already {current} — there is nothing to move")
        } else {
            match rule.direction {
                Direction::Ceiling => format!(
                    "refusing: value {value} does not raise the baseline {current} — a ceiling may \
                     only decrease; that is the paydown — use `ratchet baseline update`"
                ),
                Direction::Floor => format!(
                    "refusing: value {value} does not lower the baseline {current} — a floor may \
                     only increase; that is the paydown — use `ratchet baseline update`"
                ),
            }
        };
        return Raise::Refuse { message };
    }

    let message = match rule.direction {
        Direction::Ceiling => format!("{current} → {value} — raised deliberately"),
        Direction::Floor => format!("{current} → {value} — lowered deliberately"),
    };
    Raise::Move {
        value,
        reason: reason.to_string(),
        message,
    }
}

/// The deliberate move against the declared direction (SPEC 5.5).
///
/// The paydown (`baseline update`) moves a baseline in the allowed direction;
/// this command moves it the other way, on purpose.  The reason is required:
/// it travels with the record, so the diff on the baseline file carries it
/// (SPEC 4) and the monotonicity check can accept the move (SPEC 5.2).
fn cmd_baseline_raise(cli: &Cli, args: &BaselineRaiseArgs, cwd: &Path) -> Result<i32, Error> {
    let context = load_context(cli, cwd)?;

    // The command moves exactly one baseline: without a rule there is nothing
    // to raise.
    let Some(id) = cli.rule.as_deref() else {
        return Err(Error::new(
            "ratchet baseline raise needs a rule: --rule <id>",
        ));
    };
    ensure_rule_exists(&context.config, Some(id))?;
    let rule = &context.config.rules[id];

    let mut baseline = Baseline::load_if_exists(Some(&context.root), &context.baseline_path)?;
    if let Some(scope_id) = args.scope.as_deref() {
        let scope = rule
            .scopes
            .iter()
            .find(|scope| scope.id == scope_id)
            .ok_or_else(|| Error::new(format!("unknown scope for rule {id}: {scope_id}")))?;
        let current = baseline
            .get(id)
            .and_then(|record| record.scopes.get(scope_id))
            .map(|record| record.value);
        let mut actions = Vec::new();
        let mut failed = 0usize;
        match decide_raise(rule, current, args.value, args.reason.as_deref()) {
            Raise::Refuse { message } => {
                failed += 1;
                actions.push(BaselineAction {
                    id: format!("{id}#{}", scope.id),
                    kind: ActionKind::Refused,
                    value: current,
                    message,
                });
            }
            Raise::Move {
                value,
                reason,
                message,
            } => {
                let record = baseline.rules.get_mut(id).ok_or_else(|| {
                    Error::new(format!(
                        "refusing: no baseline for {id} — run ratchet baseline init"
                    ))
                })?;
                record.scopes.insert(
                    scope_id.to_string(),
                    baseline::ScopeBaselineRecord {
                        value,
                        reason: Some(reason),
                    },
                );
                baseline.save(Some(&context.root), &context.baseline_path)?;
                actions.push(BaselineAction {
                    id: format!("{id}#{}", scope.id),
                    kind: ActionKind::Updated,
                    value: Some(value),
                    message,
                });
            }
        }
        print_baseline(cli, &context, "raise", &actions, failed)?;
        return Ok(if failed > 0 { 1 } else { 0 });
    }
    let current = baseline.get(id).and_then(|record| record.value);

    let mut actions: Vec<BaselineAction> = Vec::new();
    let mut failed = 0usize;

    match decide_raise(rule, current, args.value, args.reason.as_deref()) {
        Raise::Refuse { message } => {
            failed += 1;
            actions.push(BaselineAction {
                id: id.to_string(),
                kind: ActionKind::Refused,
                value: current,
                message,
            });
        }
        Raise::Move {
            value,
            reason,
            message,
        } => {
            // The record carries the new value **and** the reason: the reason
            // travels with the record, so the diff on the baseline file
            // carries it.
            baseline.rules.insert(
                id.to_string(),
                BaselineRecord {
                    value: Some(value),
                    reason: Some(reason),
                    scopes: baseline
                        .get(id)
                        .map(|record| record.scopes.clone())
                        .unwrap_or_default(),
                },
            );
            baseline.save(Some(&context.root), &context.baseline_path)?;
            actions.push(BaselineAction {
                id: id.to_string(),
                kind: ActionKind::Updated,
                value: Some(value),
                message,
            });
        }
    }

    print_baseline(cli, &context, "raise", &actions, failed)?;
    Ok(if failed > 0 { 1 } else { 0 })
}

/// Prints the output of a baseline command.
fn print_baseline(
    cli: &Cli,
    context: &Context,
    command: &str,
    actions: &[BaselineAction],
    failed: usize,
) -> Result<(), Error> {
    // The path that was written — relative to the repository root.
    let path = path::display(Some(&context.root), &context.baseline_path);
    match format_of(cli)? {
        Output::Json => println!("{}", report::render_baseline_json(command, actions, failed)),
        Output::Stylish => print!("{}", report::render_baseline(command, &path, actions)),
    }
    Ok(())
}

/// The path of the baseline file: the answer to "which file is my baseline?".
///
/// The path is the one every other command uses — the config's `baseline`
/// key, the default next to the config.  The resolution lives in one place
/// (`load_context`), so a script outside the binary asks instead of
/// re-implementing it: a second implementation answers a different question
/// than the binary, and a wrong answer is a commit of the wrong file
/// (SPEC 5.5).
///
/// The path is repository-relative and nothing else is printed: the output is
/// meant for a shell.
fn cmd_baseline_path(cli: &Cli, cwd: &Path) -> Result<i32, Error> {
    // The command prints one line: a format it cannot honour is refused
    // before anything else happens.
    plain_format(cli)?;
    let context = load_context(cli, cwd)?;
    let Some(relative) = context.baseline_rel.as_deref() else {
        return Err(Error::new(
            "the baseline is not inside the repository root — its path cannot be named",
        ));
    };
    if relative.is_empty() {
        return Err(Error::new(
            "the baseline is the repository root itself — its path cannot be named",
        ));
    }
    println!("{relative}");
    Ok(0)
}

fn cmd_init(cli: &Cli, cwd: &Path) -> Result<i32, Error> {
    // The same discovery as `check`: a config in a parent directory of the
    // repository must not be shadowed by a second one.
    let git = Git::discover(cwd);
    if let Some(existing) = discover_config(cli, cwd, git.root())? {
        let name = path::display(git.root(), &existing);
        if cli.debug {
            // The resolved config of the config that exists.
            match load_config_file(&existing) {
                Ok(config) => eprintln!("ratchet: debug: {}", resolved_config(&config)),
                Err(error) => eprintln!("ratchet: debug: {error}"),
            }
        }
        match format_of(cli)? {
            Output::Json => println!("{}", report::render_init_json(&format!("{name} (exists)"))),
            Output::Stylish => println!("ratchet: {name} already exists — nothing to do"),
        }
        return Ok(0);
    }

    if cli.debug {
        eprintln!("ratchet: debug: no config found — nothing to resolve");
    }

    // The config is written where the discovery finds it: the root of the
    // repository, or the working directory when there is no repository.
    let directory = git
        .root()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| cwd.to_path_buf());
    // The config is a path the project names: the rule applies to it as well.
    // A link at that position that leads out of the tree is refused — the
    // starter config would be written outside it.
    if let Err(refusal) = path::inside_repository(&directory, CONFIG_FILE_STARTER) {
        return Err(Error::new(refusal.message(
            &format!("the config file {CONFIG_FILE_STARTER}"),
            "must lie inside the repository root",
        )));
    }
    let path = directory.join(CONFIG_FILE_STARTER);
    fs::write(&path, STARTER_CONFIG).map_err(|e| {
        Error::new(format!(
            "cannot write {}: {e}",
            path::display(git.root(), &path)
        ))
    })?;
    if cli.debug {
        eprintln!("ratchet: debug: wrote {}", path::display(git.root(), &path));
    }
    match format_of(cli)? {
        Output::Json => println!("{}", report::render_init_json("ratchet.config.jsonc")),
        Output::Stylish => println!("ratchet: wrote ratchet.config.jsonc"),
    }
    Ok(0)
}

/// The self-update: the newest release, verified and put in place.
///
/// The command needs no config and no repository: it replaces the binary and
/// touches nothing else (SPEC 9).
fn cmd_update(cli: &Cli, _args: &UpdateArgs) -> Result<i32, Error> {
    // The command prints a few lines: a format it cannot honour is refused
    // before the network is touched.
    plain_format(cli)?;
    let options = update::Options {
        current: update::current_version()?,
        api_base: update::api_base(),
        download_base: update::download_base(),
        target: update::target_path()?,
    };
    let outcome = update::run(&options)?;
    match outcome {
        update::Outcome::Current { release } => {
            println!(
                "ratchet {} is up to date — the newest release is {release}",
                options.current
            );
        }
        update::Outcome::Updated { version } => {
            println!(
                "ratchet {} → {version} — the binary was replaced",
                options.current
            );
        }
        update::Outcome::Deferred {
            version,
            path,
            file,
        } => {
            println!(
                "ratchet {} → {version} — a running executable cannot be replaced on windows",
                options.current
            );
            println!("the new binary was written to {}", path.display());
            println!("move it over {} and start ratchet again", file.display());
        }
    }
    // The binary is up to date now: the check must not announce the version
    // that was just installed.
    update_check::mark_checked();
    Ok(0)
}

/// Fails when `--rule` names a rule that does not exist.
fn ensure_rule_exists(config: &Config, rule: Option<&str>) -> Result<(), Error> {
    if let Some(rule) = rule {
        if !config.rules.contains_key(rule) {
            return Err(Error::new(format!("unknown rule: {rule}")));
        }
    }
    Ok(())
}

/// Fails when a selected path does not exist.
///
/// A typo must not silently narrow the run into a green one (SPEC 5.2).
fn ensure_paths_exist(paths: &[String], cwd: &Path, root: &Path) -> Result<(), Error> {
    for path in paths {
        let candidate = Path::new(path);
        let absolute = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            cwd.join(candidate)
        };
        if !absolute.exists() {
            return Err(Error::new(format!(
                "unknown command or path: {} — it is neither a ratchet command nor an existing path",
                path::display(Some(root), &absolute)
            )));
        }
    }
    Ok(())
}

/// Loads the config, the repository root and the baseline path.
fn load_context(cli: &Cli, cwd: &Path) -> Result<Context, Error> {
    let git = Git::discover(cwd);
    let config_path = find_config(cli, cwd, git.root())?;
    let config = load_config_file(&config_path)?;
    let config_dir = parent_dir(&config_path);
    let root = git
        .root()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| config_dir.clone());

    let baseline_rel = config
        .baseline
        .clone()
        .unwrap_or_else(|| DEFAULT_BASELINE_FILE.to_string());

    // The baseline has to be comparable with its committed version, and that
    // only works inside the repository root (SPEC 2).  The check resolves the
    // path — a link cannot carry it out of the root.
    let baseline_path =
        path::inside_root(&root, &config_dir, &baseline_rel).map_err(|refusal| {
            Error::new(format!(
                "config error at baseline: {}",
                refusal.message("the baseline", "must be inside the repository root")
            ))
        })?;

    // `baseline_path` has been resolved through the filesystem above. On
    // Windows that path has the extended `\\?\` prefix, while Git's root
    // spelling does not; compare against the same canonical root spelling.
    let baseline_rel = path::relative_to(&path::canonical_root(&root), &baseline_path);

    Ok(Context {
        config,
        config_path,
        root,
        baseline_path,
        baseline_rel,
        git,
    })
}

/// Finds the config file: the explicit one, or the first one in the search path.
fn find_config(cli: &Cli, cwd: &Path, git_root: Option<&Path>) -> Result<PathBuf, Error> {
    discover_config(cli, cwd, git_root)?.ok_or_else(|| {
        Error::new("no config found — expected ratchet.config.jsonc (run `ratchet init`)")
    })
}

/// The discovery of the config file; `None` when there is none.
///
/// The lookup walks up from the working directory to the git root.
fn discover_config(
    cli: &Cli,
    cwd: &Path,
    git_root: Option<&Path>,
) -> Result<Option<PathBuf>, Error> {
    if let Some(path) = &cli.config {
        if !path.is_file() {
            return Err(Error::new(format!(
                "config file not found: {}",
                path::display(git_root, path)
            )));
        }
        return Ok(Some(absolute(path, cwd)));
    }
    if cli.no_config {
        return Err(Error::new("--no-config requires --config <path>"));
    }

    let mut directory = Some(cwd.to_path_buf());
    while let Some(current) = directory {
        for name in CONFIG_FILE_NAMES {
            let candidate = current.join(name);
            if candidate.is_file() {
                return Ok(Some(candidate));
            }
        }
        if let Some(root) = git_root {
            if current == root {
                break;
            }
        }
        directory = current.parent().map(Path::to_path_buf);
    }

    Ok(None)
}

/// Loads a config file; the extension decides how it is evaluated.
fn load_config_file(path: &Path) -> Result<Config, Error> {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("mjs") | Some("cjs") => Ok(config::load_js_config(path)?),
        _ => {
            let text = fs::read_to_string(path).map_err(|e| {
                Error::new(format!(
                    "cannot read {}: {e}",
                    path.file_name()
                        .unwrap_or(path.as_os_str())
                        .to_string_lossy()
                ))
            })?;
            Ok(config::parse_config_str(&text)?)
        }
    }
}

/// Reads the committed baseline.
///
/// * no repository, or the baseline is not in `HEAD` → `None`: the
///   monotonicity check is skipped with a note.
/// * the committed baseline exists but cannot be parsed → an operational
///   error: a broken reference must not disable the gate (SPEC 5.2, 8).
fn read_committed_baseline(context: &Context) -> Result<Option<Baseline>, Error> {
    let Some(relative) = context.baseline_rel.as_ref() else {
        return Ok(None);
    };
    let Some(text) = context.git.show_head(relative) else {
        return Ok(None);
    };
    match Baseline::parse(&text, relative) {
        Ok(baseline) => Ok(Some(baseline)),
        Err(error) => Err(Error::new(format!(
            "the committed baseline is unreadable: {error}"
        ))),
    }
}

/// Prints the notes and the debug log to stderr.
fn print_notes(notes: &[String], debug: &[String], cli: &Cli) {
    for note in notes {
        eprintln!("ratchet: note: {note}");
    }
    if cli.debug {
        for line in debug {
            eprintln!("ratchet: debug: {line}");
        }
    }
}

/// The exit code of a check: 2 for a counter error or a stale artifact under
/// `--strict`, 1 for a failed gate.
fn exit_code(report: &Report) -> i32 {
    if report.counter_errors > 0 || report.stale_errors > 0 {
        2
    } else if report.ok {
        0
    } else {
        1
    }
}

/// The absolute path of `path`.
fn absolute(path: &Path, cwd: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

/// The directory that contains `path`.
fn parent_dir(path: &Path) -> PathBuf {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => path
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(".")),
    }
}

/// The subcommands of the CLI.
const SUBCOMMANDS: [&str; 9] = [
    "check", "count", "baseline", "drift", "verify", "report", "release", "init", "update",
];

/// The options that take a value; the value is not a positional argument.
const OPTIONS_WITH_VALUE: [&str; 11] = [
    "config", "format", "rule", "tier", "ignore", "path", "reason", "value", "output", "name",
    "scope",
];

/// Inserts the default command when the first argument is not a subcommand:
/// `ratchet src/main.rs` means `ratchet check src/main.rs`.
fn normalize_args(args: Vec<OsString>) -> Vec<OsString> {
    if args.is_empty() {
        return vec![OsString::from("ratchet")];
    }

    let wants_help = args
        .iter()
        .any(|arg| arg == "--help" || arg == "-h" || arg == "--version" || arg == "-V");
    if wants_help {
        return args;
    }

    if let Some(index) = first_positional(&args) {
        let name = args[index].to_string_lossy().to_string();
        if SUBCOMMANDS.contains(&name.as_str()) {
            return args;
        }
    }

    let mut normalized = Vec::with_capacity(args.len() + 1);
    normalized.push(args[0].clone());
    normalized.push(OsString::from("check"));
    normalized.extend(args[1..].iter().cloned());
    normalized
}

/// The index of the first argument that is not an option or an option value.
fn first_positional(args: &[OsString]) -> Option<usize> {
    let mut index = 1;
    while index < args.len() {
        let argument = args[index].to_string_lossy().to_string();
        if argument == "--" {
            return if index + 1 < args.len() {
                Some(index + 1)
            } else {
                None
            };
        }
        if let Some(name) = argument.strip_prefix("--") {
            if name.contains('=') {
                index += 1;
                continue;
            }
            if OPTIONS_WITH_VALUE.contains(&name) {
                index += 2;
                continue;
            }
            index += 1;
            continue;
        }
        if argument.starts_with('-') && argument.len() > 1 {
            index += 1;
            continue;
        }
        return Some(index);
    }
    None
}

/// The command line interface.
#[derive(Parser, Debug)]
#[command(
    name = "ratchet",
    version,
    about = "Ratchet gates: a count against a baseline that may only move in one direction.",
    long_about = None
)]
struct Cli {
    /// The command to run; `check` is the default.
    #[command(subcommand)]
    command: Command,

    /// Use this config file.
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Do not look for a config; only valid together with --config.
    #[arg(long, global = true)]
    no_config: bool,

    /// The output format.
    ///
    /// The format is optional: a command that prints a report uses the
    /// stylish output, the report command writes the Markdown report.
    #[arg(long, global = true, value_enum, value_name = "FORMAT")]
    format: Option<Format>,

    /// Restrict the run to one rule.
    #[arg(long, global = true, value_name = "ID")]
    rule: Option<String>,

    /// Exclude the paths that match this pattern from the run (repeatable).
    #[arg(long, global = true, value_name = "PATTERN")]
    ignore: Vec<String>,

    /// Print the resolved config and every counter invocation.
    #[arg(long, global = true)]
    debug: bool,

    /// Only print failures.
    #[arg(long, global = true, short = 'q')]
    quiet: bool,

    /// Fail closed on stale baseline records and scopes that select no files.
    #[arg(long, global = true)]
    strict: bool,

    /// Do not check for a new version.
    #[arg(long, global = true)]
    no_update_check: bool,
}

/// The commands.
#[derive(Subcommand, Debug)]
enum Command {
    /// Run the gates (the default command).
    Check(CheckArgs),
    /// Print the current count per rule.
    Count(CountArgs),
    /// Read and write the baseline file.
    Baseline(BaselineArgs),
    /// Run the drift gates: the generated files.
    Drift(DriftArgs),
    /// Run the pre-release checks: the commands against their snapshots.
    Verify(VerifyArgs),
    /// Render the run into a Markdown report.
    Report(ReportArgs),
    /// Compute the next version and the changelog of the release.
    Release(ReleaseArgs),
    /// Write a starter config.
    Init(InitArgs),
    /// Update the binary to the newest release.
    Update(UpdateArgs),
}

/// The arguments of `report`.
#[derive(Args, Debug)]
struct ReportArgs {
    /// Write the report to this file instead of stdout.
    ///
    /// The path is relative to the repository root; a path outside it is a
    /// configuration error.
    #[arg(long, value_name = "PATH")]
    output: Option<String>,
}

/// The arguments of `release`.
#[derive(Args, Debug)]
struct ReleaseArgs {
    /// Compute the release from this tag instead of the last release tag.
    #[arg(long, value_name = "TAG")]
    from: Option<String>,

    /// Print the release, change nothing.
    #[arg(long)]
    dry_run: bool,
}

/// The arguments of `check`.
#[derive(Args, Debug)]
struct CheckArgs {
    /// Restrict the run to the rules whose scope intersects these paths.
    #[arg(value_name = "PATH")]
    paths: Vec<String>,

    /// Run only the rules of this tier.
    #[arg(long, value_name = "TIER")]
    tier: Option<Tier>,

    /// Apply the fixable: regenerate the generated files.
    #[arg(long)]
    fix: bool,
}

/// The arguments of `drift`.
#[derive(Args, Debug)]
struct DriftArgs {
    /// Run only the drift gate of this generated file.
    #[arg(long, value_name = "PATH")]
    path: Option<String>,

    /// Apply the fixable: write the produced content into the generated files.
    #[arg(long)]
    fix: bool,
}

/// The arguments of `verify`.
#[derive(Args, Debug)]
struct VerifyArgs {
    /// Run only the check of this name.
    #[arg(long, value_name = "NAME")]
    name: Option<String>,

    /// Write the produced output into the snapshots.
    ///
    /// The snapshot is machine-written, never hand-edited (SPEC §15).  The
    /// check stays failed: the committed snapshot is still stale until the
    /// file is committed.
    #[arg(long)]
    update: bool,
}

/// The arguments of `count`.
#[derive(Args, Debug)]
struct CountArgs {}

/// The arguments of `init`.
#[derive(Args, Debug)]
struct InitArgs {}

/// The arguments of `update`.
#[derive(Args, Debug)]
struct UpdateArgs {}

/// The arguments of `baseline`.
#[derive(Args, Debug)]
struct BaselineArgs {
    /// The baseline command.
    #[command(subcommand)]
    command: BaselineCommand,
}

/// The baseline commands.
#[derive(Subcommand, Debug)]
enum BaselineCommand {
    /// Write the current count as the baseline of every rule without one.
    Init(BaselineReasonArgs),
    /// Move the baseline towards the current count, in the allowed direction only.
    Update(BaselineReasonArgs),
    /// Print the path of the baseline file.
    Path(BaselinePathArgs),
    /// Move the baseline against its direction, with a reason.
    Raise(BaselineRaiseArgs),
}

/// The arguments of the baseline commands.
#[derive(Args, Debug)]
struct BaselineReasonArgs {
    /// The reason stored in the baseline record.
    #[arg(long, value_name = "TEXT")]
    reason: Option<String>,
}

/// The arguments of `baseline path`.
#[derive(Args, Debug)]
struct BaselinePathArgs {}

/// The arguments of `baseline raise`.
#[derive(Args, Debug)]
struct BaselineRaiseArgs {
    /// The new value of the baseline.
    #[arg(long, value_name = "N", allow_negative_numbers = true)]
    value: i64,

    /// The reason the baseline moves against its direction; it is required.
    #[arg(long, value_name = "TEXT")]
    reason: Option<String>,

    /// The named scope whose baseline is raised instead of the global fallback.
    #[arg(long, value_name = "ID")]
    scope: Option<String>,
}

/// The output format.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    /// Human readable.
    Stylish,
    /// Machine readable.
    Json,
    /// The Markdown report.
    Md,
}

/// The output of a command that prints a report.
///
/// The Markdown format is not one of them: the `report` command writes the
/// Markdown report, the other commands print the stylish output or the JSON
/// document.
#[derive(Clone, Copy)]
enum Output {
    /// The human readable output.
    Stylish,
    /// The JSON document.
    Json,
}

/// The tier of a rule, on the command line.
///
/// The names are the ones of the config: `fast` and `slow`.
impl ValueEnum for Tier {
    fn value_variants<'a>() -> &'a [Self] {
        &[Tier::Fast, Tier::Slow]
    }

    fn to_possible_value(&self) -> Option<clap::builder::PossibleValue> {
        Some(match self {
            Tier::Fast => clap::builder::PossibleValue::new("fast"),
            Tier::Slow => clap::builder::PossibleValue::new("slow"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn inserts_the_default_command() {
        let normalized = normalize_args(args(&["ratchet", "src/main.rs"]));
        assert_eq!(normalized[1], OsString::from("check"));
        assert_eq!(normalized[2], OsString::from("src/main.rs"));
    }

    #[test]
    fn keeps_a_known_subcommand() {
        let normalized = normalize_args(args(&["ratchet", "count"]));
        assert_eq!(normalized.len(), 2);
        assert_eq!(normalized[1], OsString::from("count"));
    }

    #[test]
    fn inserts_the_default_command_before_an_option() {
        let normalized = normalize_args(args(&["ratchet", "--config", "a.jsonc"]));
        assert_eq!(normalized[1], OsString::from("check"));
    }

    #[test]
    fn leaves_the_help_alone() {
        let normalized = normalize_args(args(&["ratchet", "--help"]));
        assert_eq!(normalized.len(), 2);
    }

    /// A rule with a direction, for the decision of a raise.
    fn rule(direction: Direction) -> Rule {
        Rule {
            id: "a".to_string(),
            class: "c".to_string(),
            incident: "i".to_string(),
            unit: "u".to_string(),
            direction,
            tier: Tier::Fast,
            count: config::Counter::Command {
                command: vec!["echo".to_string(), "1".to_string()],
            },
            exclude: Vec::new(),
            files: Vec::new(),
            description: None,
            aggregate: None,
            scopes: Vec::new(),
            exclude_globs: Vec::new(),
            file_globs: Vec::new(),
        }
    }

    /// The message of a refusal; `None` when the raise was allowed.
    fn refusal(
        direction: Direction,
        current: Option<i64>,
        value: i64,
        reason: Option<&str>,
    ) -> Option<String> {
        match decide_raise(&rule(direction), current, value, reason) {
            Raise::Refuse { message } => Some(message),
            Raise::Move { .. } => None,
        }
    }

    /// The move of a raise; `None` when it was refused.
    fn movement(
        direction: Direction,
        current: Option<i64>,
        value: i64,
        reason: Option<&str>,
    ) -> Option<(i64, String, String)> {
        match decide_raise(&rule(direction), current, value, reason) {
            Raise::Move {
                value,
                reason,
                message,
            } => Some((value, reason, message)),
            Raise::Refuse { .. } => None,
        }
    }

    #[test]
    fn a_raise_without_a_reason_is_refused() {
        let message = refusal(Direction::Ceiling, Some(5), 7, None).expect("the raise is refused");
        assert!(message.contains("a raise needs a reason"), "{message}");
        // An empty reason is no reason.
        let message =
            refusal(Direction::Ceiling, Some(5), 7, Some("   ")).expect("the raise is refused");
        assert!(message.contains("a raise needs a reason"), "{message}");
    }

    #[test]
    fn a_raise_against_the_direction_moves_the_baseline() {
        // A ceiling goes up.
        let (value, reason, message) =
            movement(Direction::Ceiling, Some(5), 7, Some("because")).expect("the raise moves");
        assert_eq!(value, 7);
        assert_eq!(reason, "because");
        assert!(message.contains("raised deliberately"), "{message}");
        // A floor goes down.
        let (value, reason, message) =
            movement(Direction::Floor, Some(5), 3, Some("because")).expect("the raise moves");
        assert_eq!(value, 3);
        assert_eq!(reason, "because");
        assert!(message.contains("lowered deliberately"), "{message}");
    }

    #[test]
    fn a_raise_in_the_allowed_direction_is_the_paydown() {
        // A ceiling that goes down.
        let message =
            refusal(Direction::Ceiling, Some(5), 3, Some("because")).expect("the raise is refused");
        assert!(
            message.contains("that is the paydown — use `ratchet baseline update`"),
            "{message}"
        );
        // A floor that goes up.
        let message =
            refusal(Direction::Floor, Some(5), 7, Some("because")).expect("the raise is refused");
        assert!(
            message.contains("that is the paydown — use `ratchet baseline update`"),
            "{message}"
        );
    }

    #[test]
    fn a_raise_that_moves_nothing_is_refused() {
        let message =
            refusal(Direction::Ceiling, Some(5), 5, Some("because")).expect("the raise is refused");
        assert!(message.contains("there is nothing to move"), "{message}");
    }

    #[test]
    fn a_raise_of_a_rule_without_a_baseline_is_refused() {
        let message =
            refusal(Direction::Ceiling, None, 7, Some("because")).expect("the raise is refused");
        assert!(message.contains("no baseline for a"), "{message}");
    }
}
