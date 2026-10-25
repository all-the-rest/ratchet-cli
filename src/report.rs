//! The output formats: `stylish` (human readable), `json` (machine readable)
//! and `md` (the Markdown report).

use std::cmp::Ordering;

use serde::Serialize;

use crate::check::{BaselineAction, Report, RuleResult, RuleStatus};
use crate::config::Direction;
use crate::drift::DriftEntry;
use crate::registry::Finding;
use crate::release::Outcome as ReleaseOutcome;
use crate::verify::VerifyEntry;

/// The JSON schema version of the report.
pub const SCHEMA: u32 = 1;

/// Renders the human readable report.
pub fn render_stylish(report: &Report, quiet: bool) -> String {
    let mut out = String::new();
    let failed = report.failed();
    let drifted = report.drifted();

    if !quiet {
        out.push_str(&format!(
            "ratchet — {}{}, {} failed\n\n",
            plural(report.rules.len(), "rule"),
            generated_suffix(report),
            failed + drifted
        ));
    }

    let width = label_width(report);
    for rule in &report.rules {
        if quiet && !rule.status.is_failure() {
            continue;
        }
        let status = if rule.status.is_failure() {
            "FAIL"
        } else {
            "PASS"
        };
        let detail = match rule.scope_description.as_deref() {
            Some(description) => format!("{} — {description}", detail(rule)),
            None => detail(rule),
        };
        out.push_str(&format!(
            "  {status}  {:<width$}  {}\n",
            label(rule),
            detail,
            width = width
        ));
    }

    let generated: Vec<&DriftEntry> = report
        .generated
        .iter()
        .filter(|entry| !quiet || entry.status.is_failure())
        .collect();
    if !generated.is_empty() {
        out.push_str("\ngenerated files:\n\n");
        for entry in generated {
            let status = if entry.status.is_failure() {
                "FAIL"
            } else {
                "PASS"
            };
            out.push_str(&line(&format!(
                "  {status}  {:<width$}  {}",
                entry.path,
                entry.detail.clone().unwrap_or_default(),
                width = width
            )));
            if let Some(diff) = &entry.diff {
                // The diff is a unified diff of its own: its lines start with
                // a sign, and indenting them would change their meaning.
                out.push('\n');
                out.push_str(diff);
            }
        }
    }

    if !quiet {
        out.push('\n');
        if report.ok {
            out.push_str(&format!(
                "COHERENT — {} of {} gates passed\n",
                report.gates(),
                report.gates()
            ));
        } else {
            // The operational errors are not gates: a stale artifact makes the
            // run NOT COHERENT (exit 2) without failing a gate, and the
            // headline has to say why (SPEC 5.2, 6.1).
            let stale = if report.stale_errors == 0 {
                String::new()
            } else {
                format!(", {}", plural(report.stale_errors, "stale artifact"))
            };
            out.push_str(&format!(
                "NOT COHERENT — {} of {} gates failed{stale}\n",
                failed + drifted,
                report.gates()
            ));
        }
    }

    out
}

/// The part of the headline that names the generated files.
fn generated_suffix(report: &Report) -> String {
    if report.generated.is_empty() {
        return String::new();
    }
    format!(", {}", plural(report.generated.len(), "generated file"))
}

/// One line of the report, without the padding of an empty detail.
///
/// The label column is padded, and a row without a detail would end in
/// spaces — the report carries words, not padding.
fn line(text: &str) -> String {
    format!("{}\n", text.trim_end())
}

/// Renders the human readable output of the drift command.
///
/// The drift command runs the generated files only: there are no rules in its
/// report, and the headline names the generated files alone.
///
/// The verdict is the one the run decided on — the same one the exit code and
/// the JSON report carry.  The line must never say COHERENT while the run
/// fails.
pub fn render_drift(entries: &[DriftEntry], ok: bool, quiet: bool) -> String {
    let mut out = String::new();
    let drifted = entries
        .iter()
        .filter(|entry| entry.status.is_failure())
        .count();

    if !quiet {
        out.push_str(&format!(
            "ratchet — {}, {drifted} failed\n\n",
            plural(entries.len(), "generated file")
        ));
    }

    let width = entries
        .iter()
        .map(|entry| entry.path.chars().count())
        .max()
        .unwrap_or(0)
        + 6;
    let shown: Vec<&DriftEntry> = entries
        .iter()
        .filter(|entry| !quiet || entry.status.is_failure())
        .collect();
    for entry in shown {
        let status = if entry.status.is_failure() {
            "FAIL"
        } else {
            "PASS"
        };
        out.push_str(&line(&format!(
            "  {status}  {:<width$}  {}",
            entry.path,
            entry.detail.clone().unwrap_or_default(),
            width = width
        )));
        if let Some(diff) = &entry.diff {
            out.push('\n');
            out.push_str(diff);
        }
    }

    if !quiet {
        out.push('\n');
        if ok {
            out.push_str(&format!(
                "COHERENT — {} of {} gates passed\n",
                entries.len(),
                entries.len()
            ));
        } else {
            out.push_str(&format!(
                "NOT COHERENT — {drifted} of {} gates failed\n",
                entries.len()
            ));
        }
    }

    out
}

/// Renders the human readable output of the pre-release check.
///
/// The verify command runs the pre-release checks alone: there are no rules
/// in its report, and the headline names the checks alone.
///
/// The verdict is the one the run decided on — the same one the exit code and
/// the JSON report carry.  The line must never say COHERENT while the run
/// fails.
pub fn render_verify(entries: &[VerifyEntry], ok: bool, quiet: bool) -> String {
    let mut out = String::new();
    let drifted = entries
        .iter()
        .filter(|entry| entry.status.is_failure())
        .count();

    if !quiet {
        out.push_str(&format!(
            "ratchet — {}, {drifted} failed\n\n",
            plural(entries.len(), "check")
        ));
    }

    let width = entries
        .iter()
        .map(|entry| entry.name.chars().count())
        .max()
        .unwrap_or(0)
        + 6;
    let shown: Vec<&VerifyEntry> = entries
        .iter()
        .filter(|entry| !quiet || entry.status.is_failure())
        .collect();
    for entry in shown {
        let status = if entry.status.is_failure() {
            "FAIL"
        } else {
            "PASS"
        };
        out.push_str(&line(&format!(
            "  {status}  {:<width$}  {}",
            entry.name,
            entry.detail.clone().unwrap_or_default(),
            width = width
        )));
        if let Some(diff) = &entry.diff {
            // The diff is a unified diff of its own: its lines start with
            // a sign, and indenting them would change their meaning.
            out.push('\n');
            out.push_str(diff);
        }
    }

    if !quiet {
        out.push('\n');
        if ok {
            out.push_str(&format!(
                "COHERENT — {} of {} checks passed\n",
                entries.len(),
                entries.len()
            ));
        } else {
            out.push_str(&format!(
                "NOT COHERENT — {drifted} of {} checks failed\n",
                entries.len()
            ));
        }
    }

    out
}

/// Renders the machine readable output of the pre-release check.
///
/// The document **reuses the check document's envelope** (§6.3): the same
/// `schema`, `ok`, `summary` and `notes`, the same empty `rules` array — the
/// verify command runs no rule — and the `generated` array replaced by a
/// `verify` array.  The summary counts the checks where the check document
/// counts the generated files: `generated` is the number of checks, `drifted`
/// the number of checks whose snapshot differs.
pub fn render_verify_json(entries: &[VerifyEntry], ok: bool, notes: &[String]) -> String {
    let json = JsonVerifyReport {
        schema: SCHEMA,
        ok,
        summary: JsonSummary {
            rules: 0,
            failed: 0,
            executed: 0,
            generated: entries.len(),
            drifted: entries
                .iter()
                .filter(|entry| entry.status.is_failure())
                .count(),
        },
        rules: Vec::new(),
        verify: entries.iter().map(JsonVerify::from).collect(),
        notes,
    };
    serde_json::to_string_pretty(&json).unwrap_or_else(|_| "{}".to_string())
}

/// Renders the human readable output of the release command.
///
/// The **block** is part of the output: it is the artifact the command
/// produces, and a person who asks for a release wants to read it — before
/// the run writes it (a dry run) and after.
pub fn render_release(outcome: &ReleaseOutcome, dry_run: bool) -> String {
    let mut out = String::new();
    let version = outcome
        .version
        .map(|version| version.to_string())
        .unwrap_or_default();
    out.push_str(&format!("ratchet — the release {version}\n\n"));
    if let Some(tag) = &outcome.tag {
        out.push_str(&format!("  {:<14}{tag}\n", "the tag"));
    }
    if let Some(base) = &outcome.base {
        out.push_str(&format!("  {:<14}{base}\n", "since"));
    }
    out.push_str(&format!("  {:<14}{}\n", "the step", outcome.step.as_str()));
    out.push_str(&format!("  {:<14}{}\n", "the entries", outcome.announced()));
    if !outcome.written.is_empty() {
        out.push('\n');
        for file in &outcome.written {
            out.push_str(&format!("  wrote {file}\n"));
        }
    }
    if !outcome.block.is_empty() {
        out.push('\n');
        out.push_str(&outcome.block);
    }
    if dry_run {
        out.push_str("\n  the run was a dry one: nothing was written\n");
    }
    out
}

/// The JSON document of the release command.
#[derive(Serialize)]
struct JsonRelease<'a> {
    schema: u32,
    /// The version of the release.
    version: Option<String>,
    /// The tag of the release.
    tag: Option<&'a str>,
    /// The tag the release was computed from.
    #[serde(skip_serializing_if = "Option::is_none")]
    base: Option<&'a str>,
    /// The step the version moves by.
    step: &'a str,
    /// True when there is nothing to release.
    nothing: bool,
    /// True when the run was a dry one: nothing was written.
    dry_run: bool,
    /// The entries of the changelog block.
    entries: Vec<JsonEntry<'a>>,
    /// The changelog block.
    block: &'a str,
    /// The files that were written.
    written: &'a [String],
    /// The notes of the run.
    notes: &'a [String],
}

/// One commit of the release.
#[derive(Serialize)]
struct JsonEntry<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<&'a str>,
    subject: &'a str,
    breaking: bool,
    date: &'a str,
}

/// Renders the JSON document of the release command.
pub fn render_release_json(outcome: &ReleaseOutcome, dry_run: bool) -> String {
    let json = JsonRelease {
        schema: SCHEMA,
        version: outcome.version.map(|version| version.to_string()),
        tag: outcome.tag.as_deref(),
        base: outcome.base.as_deref(),
        step: outcome.step.as_str(),
        nothing: outcome.nothing,
        dry_run,
        entries: outcome
            .entries
            .iter()
            .map(|entry| JsonEntry {
                kind: entry.kind.as_deref(),
                scope: entry.scope.as_deref(),
                subject: &entry.subject,
                breaking: entry.breaking,
                date: &entry.date,
            })
            .collect(),
        block: &outcome.block,
        written: &outcome.written,
        notes: &outcome.notes,
    };
    // The document ends like every other JSON document of the tool: the
    // newline comes from the printing, not from the renderer.
    serde_json::to_string_pretty(&json).unwrap_or_else(|_| "{}".to_string())
}
/// Renders the human readable `count` output.
pub fn render_counts(report: &Report) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "ratchet — {} counted\n\n",
        plural(report.rules.len(), "rule")
    ));

    let width = label_width(report);
    for rule in &report.rules {
        let count = match (rule.count, &rule.detail) {
            (Some(count), _) => count.to_string(),
            (None, Some(detail)) => format!("failed — {detail}"),
            (None, None) => "failed".to_string(),
        };
        let unit = rule
            .unit
            .as_ref()
            .map(|unit| format!("  ({unit})"))
            .unwrap_or_default();
        let label = rule
            .scope_id
            .as_ref()
            .map(|scope| format!("{}#{scope}", rule.id))
            .unwrap_or_else(|| rule.id.clone());
        let description = rule
            .scope_description
            .as_ref()
            .map(|description| format!(" — {description}"))
            .unwrap_or_default();
        out.push_str(&format!(
            "  {:<width$}  {count}{unit}{description}\n",
            label,
            width = width
        ));
    }

    out
}

/// Renders the human readable output of the baseline commands.
pub fn render_baseline(command: &str, path: &str, actions: &[BaselineAction]) -> String {
    let mut out = String::new();
    out.push_str(&format!("ratchet — baseline {command}\n\n"));
    for action in actions {
        out.push_str(&format!("  {}: {}\n", action.id, action.message));
    }
    out.push_str(&format!("\n{path}\n"));
    out
}

/// Renders the machine readable report.
pub fn render_json(report: &Report) -> String {
    let json = JsonReport {
        schema: SCHEMA,
        ok: report.ok,
        summary: JsonSummary {
            rules: report.rules.len(),
            failed: report.failed(),
            executed: report.checks(),
            generated: report.generated.len(),
            drifted: report.drifted(),
        },
        rules: report.rules.iter().map(JsonRule::from).collect(),
        generated: report.generated.iter().map(JsonGenerated::from).collect(),
        notes: &report.notes,
    };
    serde_json::to_string_pretty(&json).unwrap_or_else(|_| "{}".to_string())
}

/// Renders the machine readable output of the baseline commands.
pub fn render_baseline_json(command: &str, actions: &[BaselineAction], failed: usize) -> String {
    let json = JsonBaselineReport {
        schema: SCHEMA,
        ok: failed == 0,
        summary: JsonSummary {
            rules: actions.len(),
            failed,
            executed: actions.len(),
            generated: 0,
            drifted: 0,
        },
        rules: actions.iter().map(JsonBaselineAction::from).collect(),
        generated: Vec::new(),
        notes: &[],
        baseline: JsonBaseline {
            command: command.to_string(),
        },
    };
    serde_json::to_string_pretty(&json).unwrap_or_else(|_| "{}".to_string())
}

/// Renders the machine readable output of `ratchet init`.
pub fn render_init_json(config: &str) -> String {
    let json = JsonInitReport {
        schema: SCHEMA,
        ok: true,
        summary: JsonSummary {
            rules: 0,
            failed: 0,
            executed: 0,
            generated: 0,
            drifted: 0,
        },
        rules: Vec::new(),
        generated: Vec::new(),
        notes: &[],
        config: config.to_string(),
    };
    serde_json::to_string_pretty(&json).unwrap_or_else(|_| "{}".to_string())
}

/// Renders the Markdown report (SPEC 14).
///
/// The report is a **rendering of a run**: every number is a number of that
/// run, and the findings are the findings the count was made of.  A report
/// that counted the findings again could contradict the number it claims to
/// report.
///
/// The report renders the gates that do not depend on the report: the drift
/// gates are not in it — a report that contained the result of the comparison
/// with itself would be a regress, and the report is exactly what the drift
/// gate compares.  The drift gates' verdicts belong to the `check`.
///
/// The generator is deterministic: the rows are sorted, there is no timestamp,
/// no absolute path and no machine name.  The same tree produces the same
/// bytes — otherwise the drift gate would report a difference that does not
/// exist.
pub fn render_markdown(report: &Report) -> String {
    let mut out = String::new();

    out.push_str("# Ratchet report\n\n");
    // 0. the header: the file says that it is generated, and how.
    out.push_str(
        "**Generated — do not edit by hand.** Regenerate with `ratchet report --format md \
         --output <path>`.\n\n",
    );

    // 1. the summary: the verdict of the run and its numbers.
    out.push_str("## Summary\n\n");
    if report.ok {
        out.push_str("**Verdict: COHERENT**\n\n");
    } else if report.notes.is_empty() {
        out.push_str("**Verdict: NOT COHERENT**\n\n");
    } else {
        out.push_str("**Verdict: NOT COHERENT** — the notes below say why.\n\n");
    }
    out.push_str("| Rules | Failed | Executed |\n");
    out.push_str("|---|---|---|\n");
    out.push_str(&format!(
        "| {} | {} | {} |\n",
        report.rules.len(),
        report.failed(),
        report.checks()
    ));
    out.push('\n');

    // 2. one row per rule.  The detail says why a rule is not green: the
    //    file alone has to say why.
    out.push_str("## Rules\n\n");
    let scoped = report
        .rules
        .iter()
        .any(|rule| rule.scope_id.is_some() || rule.scope_description.is_some());
    if scoped {
        out.push_str("| Rule | Scope | Description | Class | Direction | Tier | Count | Baseline | Status | Detail |\n");
        out.push_str("|---|---|---|---|---|---|---|---|---|---|\n");
        for rule in &report.rules {
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
                cell(&rule.id),
                value(rule.scope_id.as_deref()),
                value(rule.scope_description.as_deref()),
                value(rule.class.as_deref()),
                value(rule.direction.map(|direction| direction.as_str())),
                value(rule.tier.map(|tier| tier.as_str())),
                number(rule.count),
                number(rule.baseline),
                rule.status.as_str(),
                value(rule.detail.as_deref()),
            ));
        }
    } else {
        out.push_str("| Rule | Class | Direction | Tier | Count | Baseline | Status | Detail |\n");
        out.push_str("|---|---|---|---|---|---|---|---|\n");
        for rule in &report.rules {
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} | {} |\n",
                cell(&rule.id),
                value(rule.class.as_deref()),
                value(rule.direction.map(|direction| direction.as_str())),
                value(rule.tier.map(|tier| tier.as_str())),
                number(rule.count),
                number(rule.baseline),
                rule.status.as_str(),
                value(rule.detail.as_deref()),
            ));
        }
    }
    out.push('\n');

    // 3. the findings that count, for every rule that counts a findings backend.
    let findings_rules: Vec<&RuleResult> = report
        .rules
        .iter()
        .filter(|rule| rule.findings.is_some())
        .collect();
    if !findings_rules.is_empty() {
        out.push_str("## Findings\n\n");
        for rule in findings_rules {
            out.push_str(&format!("### {}\n\n", cell(&rule.id)));
            out.push_str("| Id | Severity | Status | Title | Owner | Ticket |\n");
            out.push_str("|---|---|---|---|---|---|\n");
            let mut findings: Vec<&Finding> = rule.findings.iter().flatten().collect();
            // The findings are ordered like their ids: the report is a stable
            // diff.
            findings.sort_by(|left, right| finding_order(left, right));
            for finding in findings {
                out.push_str(&format!(
                    "| {} | {} | {} | {} | {} | {} |\n",
                    cell(&finding.id),
                    finding.severity.as_str(),
                    finding.status.as_str(),
                    cell(&finding.title),
                    value(finding.owner.as_deref()),
                    value(finding.ticket.as_deref()),
                ));
            }
            out.push('\n');
        }
    }

    // 4. the notes of the run: the tier floor, the skipped comparisons.
    if !report.notes.is_empty() {
        out.push_str("## Notes\n\n");
        for note in &report.notes {
            out.push_str(&format!("- {note}\n"));
        }
        out.push('\n');
    }

    // The file ends with exactly one newline.
    let mut out = out.trim_end().to_string();
    out.push('\n');
    out
}

/// The order of two findings: by issue number when the id is one, by id
/// otherwise.
///
/// A registry id (`SEC-01`) is compared as it is written; an issue id is a
/// number (`#12`), and `#2` comes before `#10` — the report is a stable diff,
/// and the numbers of the tracker are a sequence a reader walks upwards.
fn finding_order(left: &Finding, right: &Finding) -> Ordering {
    match (issue_number(&left.id), issue_number(&right.id)) {
        (Some(left), Some(right)) => left.cmp(&right),
        _ => left.id.cmp(&right.id),
    }
}

/// The number of an issue id (`#12`), if the id is one.
fn issue_number(id: &str) -> Option<u64> {
    id.strip_prefix('#')?.parse().ok()
}

/// The content of a table cell: one line, with the pipe escaped.
///
/// A cell that carries a newline would end the row, and a cell that carries a
/// pipe would open a column.  Neither belongs in a report that has to be
/// read by a machine as well.
fn cell(text: &str) -> String {
    text.replace(['\r', '\n'], " ")
        .replace('|', "\\|")
        .trim()
        .to_string()
}

/// A value that may be missing: an em dash says that there is none.
fn value(text: Option<&str>) -> String {
    match text {
        Some(text) => cell(text),
        None => "—".to_string(),
    }
}

/// A number that may be missing.
fn number(value: Option<i64>) -> String {
    match value {
        Some(value) => value.to_string(),
        None => "—".to_string(),
    }
}

/// The label of a rule in the human readable output.
fn label(rule: &RuleResult) -> String {
    let id = rule
        .scope_id
        .as_ref()
        .map(|scope| format!("{}#{scope}", rule.id))
        .unwrap_or_else(|| rule.id.clone());
    if rule.status == RuleStatus::MissingBaseline && rule.scope_id.is_none() {
        "missing-baseline".to_string()
    } else if rule.status == RuleStatus::MissingBaseline {
        format!("missing-baseline {id}")
    } else {
        id
    }
}

/// The width of the label column.
fn label_width(report: &Report) -> usize {
    let rules = report
        .rules
        .iter()
        .map(|rule| label(rule).chars().count())
        .max()
        .unwrap_or(0);
    let generated = report
        .generated
        .iter()
        .map(|entry| entry.path.chars().count())
        .max()
        .unwrap_or(0);
    rules.max(generated) + 6
}

/// The detail of a rule in the human readable output.
fn detail(rule: &RuleResult) -> String {
    match rule.status {
        RuleStatus::Ok | RuleStatus::OverBaseline | RuleStatus::UnderFloor => {
            let operator = match (rule.direction, rule.status) {
                (Some(Direction::Ceiling), RuleStatus::Ok) => "≤",
                (Some(Direction::Floor), RuleStatus::Ok) => "≥",
                (Some(Direction::Floor), _) => "<",
                _ => ">",
            };
            let unit = rule
                .unit
                .as_ref()
                .map(|unit| format!("  ({unit})"))
                .unwrap_or_default();
            format!(
                "{} {operator} {}{unit}",
                rule.count.unwrap_or_default(),
                rule.baseline.unwrap_or_default()
            )
        }
        RuleStatus::MissingBaseline => format!("no baseline for {}", rule.id),
        RuleStatus::BaselineRaised => format!(
            "baseline {} raised to {} — a ceiling may only decrease",
            rule.committed.unwrap_or_default(),
            rule.baseline.unwrap_or_default()
        ),
        RuleStatus::BaselineLowered => format!(
            "baseline {} lowered to {} — a floor may only increase",
            rule.committed.unwrap_or_default(),
            rule.baseline.unwrap_or_default()
        ),
        RuleStatus::CounterError => rule.detail.clone().unwrap_or_default(),
    }
}

fn plural(count: usize, word: &str) -> String {
    if count == 1 {
        format!("{count} {word}")
    } else {
        format!("{count} {word}s")
    }
}

#[derive(Serialize)]
struct JsonReport<'a> {
    schema: u32,
    ok: bool,
    summary: JsonSummary,
    rules: Vec<JsonRule<'a>>,
    generated: Vec<JsonGenerated<'a>>,
    /// The notes the human output prints on stderr: the tier floor, the
    /// skipped monotonicity comparison, a skipped drift gate.  A machine
    /// consumer must be able to learn why the run is not green without the
    /// note being only on stderr (SPEC 6.3).
    notes: &'a [String],
}

#[derive(Serialize)]
struct JsonSummary {
    rules: usize,
    failed: usize,
    executed: usize,
    generated: usize,
    drifted: usize,
}

/// One generated file of the report.
#[derive(Serialize)]
struct JsonGenerated<'a> {
    path: &'a str,
    status: &'a str,
    /// The unified diff (committed → produced); absent when there is none.
    diff: Option<&'a str>,
    /// The reason for a status that is not `ok` (SPEC 6.3).
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<&'a str>,
}

impl<'a> From<&'a DriftEntry> for JsonGenerated<'a> {
    fn from(entry: &'a DriftEntry) -> Self {
        Self {
            path: &entry.path,
            status: entry.status.as_str(),
            diff: entry.diff.as_deref(),
            detail: entry.detail.as_deref(),
        }
    }
}

/// The JSON document of the pre-release check.
///
/// It is the check document's envelope (§6.3) with the `generated` array
/// replaced by a `verify` array.
#[derive(Serialize)]
struct JsonVerifyReport<'a> {
    schema: u32,
    ok: bool,
    summary: JsonSummary,
    /// The verify command runs no rule: the array is empty.
    rules: Vec<JsonRule<'a>>,
    verify: Vec<JsonVerify<'a>>,
    notes: &'a [String],
}

/// One check of the pre-release check.
#[derive(Serialize)]
struct JsonVerify<'a> {
    name: &'a str,
    status: &'a str,
    /// The unified diff (committed → produced); absent when there is none.
    diff: Option<&'a str>,
    /// The reason for a status that is not `ok` (SPEC 6.3).
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<&'a str>,
}

impl<'a> From<&'a VerifyEntry> for JsonVerify<'a> {
    fn from(entry: &'a VerifyEntry) -> Self {
        Self {
            name: &entry.name,
            status: entry.status.as_str(),
            diff: entry.diff.as_deref(),
            detail: entry.detail.as_deref(),
        }
    }
}

#[derive(Serialize)]
struct JsonRule<'a> {
    id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<&'a str>,
    status: &'a str,
    count: Option<i64>,
    baseline: Option<i64>,
    direction: Option<&'a str>,
    unit: Option<&'a str>,
    class: Option<&'a str>,
    tier: Option<&'a str>,
    /// The reason for a status that is not `ok` (SPEC 6.3).
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<&'a str>,
}

impl<'a> From<&'a RuleResult> for JsonRule<'a> {
    fn from(rule: &'a RuleResult) -> Self {
        Self {
            id: &rule.id,
            scope: rule.scope_id.as_deref(),
            description: rule.scope_description.as_deref(),
            status: rule.status.as_str(),
            count: rule.count,
            baseline: rule.baseline,
            direction: rule.direction.map(|direction| direction.as_str()),
            unit: rule.unit.as_deref(),
            class: rule.class.as_deref(),
            tier: rule.tier.map(|tier| tier.as_str()),
            detail: rule.detail.as_deref(),
        }
    }
}

#[derive(Serialize)]
struct JsonBaselineReport {
    schema: u32,
    ok: bool,
    summary: JsonSummary,
    rules: Vec<JsonBaselineAction>,
    generated: Vec<serde_json::Value>,
    notes: &'static [String],
    baseline: JsonBaseline,
}

#[derive(Serialize)]
struct JsonBaseline {
    command: String,
}

#[derive(Serialize)]
struct JsonInitReport {
    schema: u32,
    ok: bool,
    summary: JsonSummary,
    rules: Vec<serde_json::Value>,
    generated: Vec<serde_json::Value>,
    notes: &'static [String],
    config: String,
}

#[derive(Serialize)]
struct JsonBaselineAction {
    id: String,
    action: &'static str,
    value: Option<i64>,
    message: String,
}

impl From<&BaselineAction> for JsonBaselineAction {
    fn from(action: &BaselineAction) -> Self {
        Self {
            id: action.id.clone(),
            action: action.kind.as_str(),
            value: action.value,
            message: action.message.clone(),
        }
    }
}
