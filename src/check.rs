//! The check: count against baseline, plus the monotonicity of the baseline.

use globset::GlobMatcher;

use crate::baseline::Baseline;
use crate::config::{Config, Direction, Rule, Tier};
use crate::counter;
use crate::drift::DriftEntry;
use crate::git::Git;
use crate::registry::Finding;
use crate::stale;

/// The status of one rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleStatus {
    /// The count respects the baseline.
    Ok,
    /// The count is above a ceiling baseline.
    OverBaseline,
    /// The count is below a floor baseline.
    UnderFloor,
    /// A ceiling baseline was increased against the committed one.
    BaselineRaised,
    /// A floor baseline was decreased against the committed one.
    BaselineLowered,
    /// The rule has no baseline.
    MissingBaseline,
    /// The counter did not deliver a usable count.
    CounterError,
}

impl RuleStatus {
    /// The name used in the JSON report.
    pub fn as_str(self) -> &'static str {
        match self {
            RuleStatus::Ok => "ok",
            RuleStatus::OverBaseline => "over-baseline",
            RuleStatus::UnderFloor => "under-floor",
            RuleStatus::BaselineRaised => "baseline-raised",
            RuleStatus::BaselineLowered => "baseline-lowered",
            RuleStatus::MissingBaseline => "missing-baseline",
            RuleStatus::CounterError => "counter-error",
        }
    }

    /// True when the rule failed.
    pub fn is_failure(self) -> bool {
        !matches!(self, RuleStatus::Ok)
    }
}

/// The outcome of one rule.
#[derive(Debug, Clone)]
pub struct RuleResult {
    /// The rule id.
    pub id: String,
    /// Named path scope, when this result is one scoped measurement.
    pub scope_id: Option<String>,
    /// Human description of the global or named scope.
    pub scope_description: Option<String>,
    /// The status.
    pub status: RuleStatus,
    /// The current count, if the counter delivered one.
    pub count: Option<i64>,
    /// The baseline the count was compared against.
    pub baseline: Option<i64>,
    /// The direction of the rule.
    pub direction: Option<Direction>,
    /// The count unit.
    pub unit: Option<String>,
    /// The defect class.
    pub class: Option<String>,
    /// The tier of the rule.
    pub tier: Option<Tier>,
    /// A human readable explanation of the status.
    pub detail: Option<String>,
    /// The committed baseline, for the monotonicity statuses.
    pub committed: Option<i64>,
    /// The findings that count, when the rule counts a findings backend.
    ///
    /// They are the findings the count was made of — the report says *which*
    /// findings are open, not only how many (SPEC 14).  They are `None` for
    /// every rule that does not count findings.
    pub findings: Option<Vec<Finding>>,
}

impl RuleResult {
    /// A result for a rule that could not be counted.
    ///
    /// The metadata of the rule is known even when the counter fails, so it is
    /// reported (SPEC §6.3).
    fn counter_error(id: &str, rule: &Rule, message: String) -> Self {
        Self {
            id: id.to_string(),
            scope_id: None,
            scope_description: if rule.aggregate.is_some() {
                rule.description.clone()
            } else {
                None
            },
            status: RuleStatus::CounterError,
            count: None,
            baseline: None,
            direction: Some(rule.direction),
            unit: Some(rule.unit.clone()),
            class: Some(rule.class.clone()),
            tier: Some(rule.tier),
            detail: Some(message),
            committed: None,
            findings: None,
        }
    }
}

/// The result of a run.
#[derive(Debug, Clone)]
pub struct Report {
    /// One entry per evaluated rule, sorted by rule id.
    pub rules: Vec<RuleResult>,
    /// One entry per generated file, sorted by path.
    pub generated: Vec<DriftEntry>,
    /// The number of rules that were executed.
    pub executed: usize,
    /// The number of rules whose counter failed.
    pub counter_errors: usize,
    /// The part of `counter_errors` that is a configured scope selecting no
    /// files.
    ///
    /// It is incremented by `check` under `--strict` and by `count`, which has
    /// no strict mode: an empty scope is an error there too (SPEC 5.2).
    ///
    /// Such a result is a `CounterError`, but the counter itself ran fine: its
    /// output cannot explain the empty scope, so the debug hint is suppressed
    /// when every counter error has this cause.
    pub empty_scope_errors: usize,
    /// The number of stale artifacts under `--strict`.
    ///
    /// Without `--strict` the artifacts are notes, not errors: the run is
    /// tolerant of records the config no longer configures (SPEC 5.2).
    pub stale_errors: usize,
    /// Notes about what was skipped and why.
    pub notes: Vec<String>,
    /// The debug log (`--debug`).
    pub debug: Vec<String>,
    /// True when the run is green: no gate failed and no counter failed.
    pub ok: bool,
}

impl Default for Report {
    fn default() -> Self {
        Self {
            rules: Vec::new(),
            generated: Vec::new(),
            executed: 0,
            counter_errors: 0,
            empty_scope_errors: 0,
            stale_errors: 0,
            notes: Vec::new(),
            debug: Vec::new(),
            ok: true,
        }
    }
}

impl Report {
    /// The number of failed rules.
    pub fn failed(&self) -> usize {
        self.rules
            .iter()
            .filter(|rule| rule.status.is_failure())
            .count()
    }

    /// The number of generated files that drifted.
    pub fn drifted(&self) -> usize {
        self.generated
            .iter()
            .filter(|entry| entry.status.is_failure())
            .count()
    }

    /// The number of gates of the run: the rules and the generated files.
    pub fn gates(&self) -> usize {
        self.rules.len() + self.generated.len()
    }

    /// The number of checks that ran: the rules.
    ///
    /// A **check** is a **rule** (SPEC 3.3): the drift gates are not
    /// counted.  They are gates over the *generated* files, and the report —
    /// which is a generated file — cannot be part of the count that decides
    /// whether the report is coherent.  The check and the report therefore
    /// evaluate the floor over the same set and cannot disagree about whether
    /// the run is coherent.
    pub fn checks(&self) -> usize {
        self.executed
    }
}

/// What a run selects: the rules that are part of it.
#[derive(Debug, Default)]
pub struct Selection<'a> {
    /// The one rule to run.
    pub rule: Option<&'a str>,
    /// The one tier to run.
    pub tier: Option<Tier>,
    /// The paths the run is scoped to; empty means "every path".
    pub paths: &'a [String],
    /// The paths to exclude from the selection.
    pub ignore: &'a [GlobMatcher],
    /// Fail closed on stale artifacts instead of noting them.
    ///
    /// The default is tolerant: a record the config no longer configures and
    /// a configured scope that selects no files are notes (SPEC 5.2).
    pub strict: bool,
}

impl Selection<'_> {
    /// True when the rule is part of the selection.
    pub fn selects(&self, rule: &Rule) -> bool {
        if let Some(tier) = self.tier {
            if rule.tier != tier {
                return false;
            }
        }
        self.in_paths(rule)
    }

    /// True when the scope of the rule intersects the selected paths.
    ///
    /// A rule without a scope always runs.  A rule whose scope is covered by
    /// the ignored paths is skipped: the paths it would have run on are gone.
    ///
    /// Ignoring a path never turns a failing gate green: a rule that would
    /// have run on the ignored path still runs on the rest (SPEC 5.1).
    fn in_paths(&self, rule: &Rule) -> bool {
        if self.paths.is_empty() {
            // The selection is unrestricted: only a scope that is covered by
            // the ignored paths empties it.
            if rule.files.is_empty() {
                return true;
            }
            return !self.ignored_scope(rule);
        }

        let remaining: Vec<&String> = self
            .paths
            .iter()
            .filter(|path| !self.ignored(path))
            .collect();
        if remaining.is_empty() {
            // Nothing is left to run on: an empty selection is never a pass.
            return false;
        }
        remaining.iter().any(|path| rule.in_scope(path))
    }

    /// True when the path matches one of the ignored patterns.
    fn ignored(&self, path: &str) -> bool {
        self.ignore.iter().any(|glob| glob.is_match(path))
    }

    /// True when every pattern of the scope of the rule is ignored.
    ///
    /// The patterns of the scope are the probe: a pattern that is covered by
    /// an ignored pattern covers every path below it.  A pattern that is not
    /// covered keeps its rule — a rule that runs when it should not is a
    /// false red, a rule that does not run when it should is a false green.
    fn ignored_scope(&self, rule: &Rule) -> bool {
        rule.files.iter().all(|pattern| self.ignored(pattern))
    }
}

/// Runs the gates: count every rule, compare with the baseline and check that
/// the baseline only moved in the declared direction.
pub fn check(
    config: &Config,
    baseline: &Baseline,
    committed: Option<&Baseline>,
    root: &std::path::Path,
    selection: &Selection<'_>,
) -> Report {
    let mut report = Report::default();
    // Stale artifacts are accumulated as they are found: every one is
    // reported, not only the first (SPEC 5.2).  Without `--strict` each is a
    // note; with it each is an operational error.
    let mut stale: Vec<stale::Stale> = Vec::new();
    // A rule record that the config no longer has is checked for every run
    // that is not restricted to a rule: naming a removed rule already fails
    // in `ensure_rule_exists`.
    if selection.rule.is_none() {
        stale.extend(stale::unknown_rules(config, baseline));
    }

    for (id, rule) in &config.rules {
        if let Some(filter) = selection.rule {
            if filter != id {
                continue;
            }
        }
        if !selection.selects(rule) {
            report.debug.push(format!(
                "rule {id} is out of scope for the selection — skipped"
            ));
            continue;
        }
        // The scope records of the rules this run processes: a scope the rule
        // no longer configures is stale (SPEC 5.2).
        stale.extend(stale::unknown_scopes(rule, baseline));
        report.executed += 1;

        let run = match counter::run_counter(&rule.count, root, &format!("rules.{id}.count")) {
            Ok(run) => run,
            Err(error) => {
                push_counter_error(&mut report, id, rule, error);
                continue;
            }
        };
        report.debug.push(format!(
            "counter {id}: {}\n{}\n--- count ---",
            rule.count.describe(),
            run.output
        ));

        if let Some(measurements) = run.value.measurements.as_ref() {
            let scoped = match scoped_counts(rule, measurements) {
                Ok(scoped) => scoped,
                Err(error) => {
                    push_counter_error(&mut report, id, rule, error);
                    continue;
                }
            };
            // A dead `exclude` pattern is always a note: the engine never
            // mutates the config, so there is nothing to prune and `--strict`
            // does not fail on it (SPEC 5.2).
            push_unmatched_excludes(&mut report, id, rule, &scoped.unmatched_excludes);
            // A configured scope that selects no file is stale: without
            // `--strict` it is skipped with a note, with `--strict` it is
            // today's error (SPEC 5.2).
            if selection.strict && !scoped.empty.is_empty() {
                for (scope_id, description) in &scoped.empty {
                    push_counter_error(
                        &mut report,
                        id,
                        rule,
                        counter::CounterError::runtime(format!(
                            "scope {scope_id} ({description}) selected no files"
                        )),
                    );
                    // The counter ran and delivered its measurements; only
                    // the scope is empty, so the output explains nothing.
                    report.empty_scope_errors += 1;
                }
                continue;
            }
            // Only the tolerant path records the empty scope as a stale note:
            // under `--strict` the counter error above already names it.
            for (scope_id, description) in &scoped.empty {
                stale.push(stale::Stale::EmptyScope {
                    rule: id.clone(),
                    scope: scope_id.clone(),
                    description: description.clone(),
                });
            }
            report.executed = report.executed.saturating_sub(1) + scoped.entries.len();
            for (scope_id, description, count) in scoped.entries {
                let scope_record = scope_id
                    .as_deref()
                    .and_then(|scope| baseline.get(id).and_then(|record| record.scopes.get(scope)));
                let global_record = baseline.get(id);
                let (record_value, reason, committed_value) = match scope_id.as_deref() {
                    Some(scope) => (
                        scope_record.map(|record| record.value),
                        scope_record.and_then(|record| record.reason.as_deref()),
                        committed
                            .and_then(|base| base.get(id))
                            .and_then(|record| record.scopes.get(scope))
                            .map(|record| record.value),
                    ),
                    None => (
                        global_record.and_then(|record| record.value),
                        global_record.and_then(|record| record.reason.as_deref()),
                        committed
                            .and_then(|base| base.get(id))
                            .and_then(|record| record.value),
                    ),
                };
                let current = count_status(rule.direction, count, record_value);
                let mut status = current;
                let mut detail =
                    status_detail(id, scope_id.as_deref(), count, record_value, status);
                if let (Some(value), Some(committed_value)) = (record_value, committed_value) {
                    let violation = match rule.direction {
                        Direction::Ceiling if value > committed_value => {
                            Some(RuleStatus::BaselineRaised)
                        }
                        Direction::Floor if value < committed_value => {
                            Some(RuleStatus::BaselineLowered)
                        }
                        _ => None,
                    };
                    if let Some(violation) = violation {
                        if let Some(reason) =
                            reason.map(str::trim).filter(|reason| !reason.is_empty())
                        {
                            let scope = scope_id
                                .as_deref()
                                .map(|scope| format!(" scope {scope}"))
                                .unwrap_or_default();
                            report.notes.push(format!("{id}{scope}: the baseline moved deliberately, {committed_value} → {value} — {reason}"));
                        } else {
                            status = violation;
                            detail = Some(format!(
                                "baseline {committed_value} moved to {value} without a reason"
                            ));
                        }
                    }
                }
                report.rules.push(RuleResult {
                    id: id.clone(),
                    scope_id,
                    scope_description: Some(description),
                    status,
                    count: Some(count),
                    baseline: record_value,
                    direction: Some(rule.direction),
                    unit: Some(rule.unit.clone()),
                    class: Some(rule.class.clone()),
                    tier: Some(rule.tier),
                    detail,
                    committed: committed_value,
                    findings: None,
                });
            }
            continue;
        }
        if !rule.scopes.is_empty() || rule.aggregate.is_some() {
            push_counter_error(
                &mut report,
                id,
                rule,
                counter::CounterError::config(
                    "config error: a rule with aggregate or scopes must emit per-file measurements"
                        .to_string(),
                ),
            );
            continue;
        }
        let finalized = match counter::finalize(
            run.value,
            &rule.exclude_globs,
            &format!("rules.{id}.exclude"),
        ) {
            Ok(finalized) => finalized,
            Err(error) => {
                push_counter_error(&mut report, id, rule, error);
                continue;
            }
        };
        push_unmatched_excludes(&mut report, id, rule, &finalized.unmatched_excludes);
        let count = finalized.count;
        report.debug.push(format!("counter {id}: count {count}"));

        let record = baseline.get(id);
        let record_value = record.and_then(|record| record.value);
        let count_status = count_status(rule.direction, count, record_value);
        let mut status = count_status;
        let mut detail = match (count_status, record_value) {
            (RuleStatus::MissingBaseline, _) => Some(format!("no baseline for {id}")),
            (RuleStatus::OverBaseline, Some(value)) => {
                Some(format!("count {count} > baseline {value}"))
            }
            (RuleStatus::UnderFloor, Some(value)) => {
                Some(format!("count {count} < baseline {value}"))
            }
            _ => None,
        };
        let mut committed_value = None;

        // Monotonicity: the baseline may only move in the declared direction.
        if let (Some(value), Some(committed_record)) = (
            record_value,
            committed
                .and_then(|base| base.get(id))
                .and_then(|record| record.value),
        ) {
            committed_value = Some(committed_record);
            let violation = match rule.direction {
                Direction::Ceiling if value > committed_record => Some((
                    RuleStatus::BaselineRaised,
                    format!(
                        "baseline {} raised to {} — a ceiling may only decrease",
                        committed_record, value
                    ),
                )),
                Direction::Floor if value < committed_record => Some((
                    RuleStatus::BaselineLowered,
                    format!(
                        "baseline {} lowered to {} — a floor may only increase",
                        committed_record, value
                    ),
                )),
                _ => None,
            };
            if let Some((violation, explanation)) = violation {
                // A move against the declared direction is allowed when
                // the record carries a reason: the reason is in the file,
                // so it is in the diff — the change is visible, deliberate
                // and reviewable on its own (SPEC 5.2).
                let reason = record
                    .and_then(|record| record.reason.as_deref())
                    .map(str::trim)
                    .filter(|reason| !reason.is_empty());
                match reason {
                    Some(reason) => {
                        let verb = match rule.direction {
                            Direction::Ceiling => "raised",
                            Direction::Floor => "lowered",
                        };
                        report.notes.push(format!(
                            "{id}: the baseline was {verb} deliberately, {} → {} — {reason}",
                            committed_record, value
                        ));
                    }
                    None => {
                        // The monotonicity check takes precedence; the count
                        // comparison is reported as an additional note (SPEC 5.2).
                        if count_status.is_failure() && count_status != RuleStatus::MissingBaseline
                        {
                            report.notes.push(format!(
                                "{id}: the count {count} also fails against the baseline {} ({})",
                                value,
                                count_status.as_str()
                            ));
                        }
                        status = violation;
                        detail = Some(explanation);
                    }
                }
            }
        }

        report.rules.push(RuleResult {
            id: id.clone(),
            scope_id: None,
            scope_description: None,
            status,
            count: Some(count),
            baseline: record_value,
            direction: Some(rule.direction),
            unit: Some(rule.unit.clone()),
            class: Some(rule.class.clone()),
            tier: Some(rule.tier),
            detail,
            committed: committed_value,
            // The findings the count was made of: the report shows which
            // findings are open, not only how many (SPEC 14).
            findings: run.findings,
        });
    }

    // The stale artifacts are named in every mode: a note by default, an
    // operational error under `--strict` (SPEC 5.2).
    for artifact in &stale {
        report.notes.push(artifact.check_note(selection.strict));
        if selection.strict {
            report.stale_errors += 1;
        }
    }

    // An empty selection is never a pass.
    report.ok = report.failed() == 0
        && report.counter_errors == 0
        && report.stale_errors == 0
        && report.executed > 0;
    report
}

/// The notes about the tiers that did not reach their floor.
///
/// The floor is the minimum number of checks that must **execute** in the
/// tier.  A check is a rule **or a drift gate** that runs in the tier: a
/// check that ran and failed counts as executed — it fails anyway; a check
/// that was never selected does not (SPEC 3.3).
///
/// The floor of **every tier that runs** is enforced: with `--tier` that is
/// the selected tier, without it every tier of the config — a tier that
/// executes no check at all is the empty selection and fails.
pub fn tier_notes(config: &Config, selection: &Selection<'_>, report: &Report) -> Vec<String> {
    let mut notes = Vec::new();
    for tier in tiers_that_run(config, selection) {
        let executed = executed_checks(report, tier);
        let floor = config
            .tiers
            .get(&tier)
            .and_then(|settings| settings.floor)
            .unwrap_or(0);

        if executed == 0 {
            notes.push(format!(
                "the tier `{}` executed no check — an empty selection is never a pass",
                tier.as_str()
            ));
            continue;
        }
        if (executed as i64) < floor {
            notes.push(format!(
                "the tier `{}` executed {}, the floor is {floor}",
                tier.as_str(),
                plural(executed, "check")
            ));
        }
    }
    notes
}

/// The checks that executed in a tier: its rules.
///
/// A drift gate is **not** a check (SPEC 3.3): the floor counts the rules,
/// so that the check and the report — which runs no drift gate — evaluate
/// the same set.
fn executed_checks(report: &Report, tier: Tier) -> usize {
    report
        .rules
        .iter()
        .filter(|rule| rule.tier == Some(tier))
        .count()
}

/// The tiers that a run executes.
///
/// * with `--tier` — the selected tier and nothing else;
/// * without it — every declared tier and the tiers of the rules.  A tier
///   that runs and has no check is the empty selection and fails.
///
/// The tiers of the drift gates are not tiers that run: a drift gate is not
/// a check (SPEC 3.3), and a tier that holds nothing but a generator has
/// nothing to count.
fn tiers_that_run(config: &Config, selection: &Selection<'_>) -> Vec<Tier> {
    if let Some(tier) = selection.tier {
        return vec![tier];
    }
    let mut tiers: Vec<Tier> = config.tiers.keys().copied().collect();
    for rule in config.rules.values() {
        if !tiers.contains(&rule.tier) {
            tiers.push(rule.tier);
        }
    }
    tiers.sort();
    tiers
}

/// The count with its unit, in the singular and in the plural.
fn plural(count: usize, word: &str) -> String {
    if count == 1 {
        format!("{count} {word}")
    } else {
        format!("{count} {word}s")
    }
}

/// Records the `exclude` patterns that matched nothing.
///
/// A dead pattern is always a note, in every mode: the engine never mutates
/// the config, so there is nothing to prune and `--strict` does not fail on
/// it (SPEC 5.2).
fn push_unmatched_excludes(report: &mut Report, id: &str, rule: &Rule, indices: &[usize]) {
    for index in indices {
        if let Some(pattern) = rule.exclude.get(*index) {
            report.notes.push(format!(
                "exclude pattern \"{pattern}\" for rule \"{id}\" matches nothing — ignored"
            ));
        }
    }
}

/// Records a counter that did not deliver a usable count.
fn push_counter_error(report: &mut Report, id: &str, rule: &Rule, error: counter::CounterError) {
    report.counter_errors += 1;
    // The raw output of the subprocess belongs to the diagnostic channel
    // (SPEC 6.2); the report carries ratchet's own sentence.
    report.debug.push(format!(
        "counter {id}: {}\n{}",
        rule.count.describe(),
        error.output.clone().unwrap_or_default()
    ));
    report
        .rules
        .push(RuleResult::counter_error(id, rule, error.message));
}

/// The note about the skipped monotonicity check.
pub fn monotonicity_note(
    git: &Git,
    baseline_rel: &str,
    committed: Option<&Baseline>,
) -> Option<String> {
    if committed.is_some() {
        return None;
    }
    if git.is_repo() {
        Some(format!(
            "monotonicity skipped: {baseline_rel} is not committed yet"
        ))
    } else {
        Some("monotonicity skipped: not a git repository".to_string())
    }
}

#[cfg(test)]
mod scoped_tests {
    use super::*;

    fn config(scopes: &str) -> Config {
        let text = r#"{"rules":{"r":{"class":"growth","incident":"test","unit":"files","direction":"ceiling","aggregate":"count","count":{"command":["echo","{}"]},"scopes":SCOPES}}}"#.replace("SCOPES", scopes);
        crate::config::parse_config_str(&text).expect("valid config")
    }

    fn config_with_exclude(scopes: &str, exclude: &str) -> Config {
        let text = r#"{"rules":{"r":{"class":"growth","incident":"test","unit":"files","direction":"ceiling","aggregate":"count","count":{"command":["echo","{}"]},"exclude":EXCLUDE,"scopes":SCOPES}}}"#
            .replace("SCOPES", scopes)
            .replace("EXCLUDE", exclude);
        crate::config::parse_config_str(&text).expect("valid config")
    }

    #[test]
    fn exact_file_beats_nested_module_and_nested_module_beats_broad_module() {
        let config = config(
            r#"[
          {"id":"broad","description":"Broad","files":["src/**"]},
          {"id":"nested","description":"Nested","files":["src/auth/**"]},
          {"id":"exact","description":"Exact","file":"src/auth/Login.java"}
        ]"#,
        );
        let values = vec![
            counter::Measurement {
                path: "src/Other.java".into(),
                value: 1,
            },
            counter::Measurement {
                path: "src/auth/Service.java".into(),
                value: 1,
            },
            counter::Measurement {
                path: "src/auth/Login.java".into(),
                value: 1,
            },
        ];
        let groups =
            scoped_counts(config.rules.get("r").expect("rule"), &values).expect("scopes resolve");
        assert!(groups.empty.is_empty());
        let grouped: std::collections::BTreeMap<_, _> = groups
            .entries
            .into_iter()
            .map(|(id, _, value)| (id, value))
            .collect();
        assert_eq!(grouped.get(&Some("broad".into())), Some(&1));
        assert_eq!(grouped.get(&Some("nested".into())), Some(&1));
        assert_eq!(grouped.get(&Some("exact".into())), Some(&1));
    }

    #[test]
    fn equal_specificity_fails_closed_and_an_empty_scope_is_reported() {
        let tied = config(
            r#"[
          {"id":"one","description":"One","files":["src/**"]},
          {"id":"two","description":"Two","files":["src/**"]}
        ]"#,
        );
        let error = scoped_counts(
            tied.rules.get("r").expect("rule"),
            &[counter::Measurement {
                path: "src/a.java".into(),
                value: 1,
            }],
        )
        .expect_err("ambiguous scopes fail");
        assert!(error.message.contains("ambiguous"));

        let empty = config(r#"[{"id":"empty","description":"Empty","files":["other/**"]}]"#);
        let scoped = scoped_counts(
            empty.rules.get("r").expect("rule"),
            &[counter::Measurement {
                path: "src/a.java".into(),
                value: 1,
            }],
        )
        .expect("an empty scope is reported, not an error");
        assert_eq!(
            scoped.empty,
            vec![("empty".to_string(), "Empty".to_string())]
        );
        assert!(
            scoped
                .entries
                .iter()
                .any(|(id, _, value)| id.is_none() && *value == 1),
            "the global fallback still counts"
        );
    }

    #[test]
    fn ambiguity_is_decided_before_exclude() {
        // Two scopes of equal specificity tie on the path; the `exclude`
        // pattern would remove the measurement afterwards.  The assignment is
        // decided first, so the tie still fails closed instead of being
        // hidden by the exclusion.
        let tied = config_with_exclude(
            r#"[
          {"id":"one","description":"One","files":["src/**"]},
          {"id":"two","description":"Two","files":["src/**"]}
        ]"#,
            r#"["src/a.java"]"#,
        );
        let error = scoped_counts(
            tied.rules.get("r").expect("rule"),
            &[counter::Measurement {
                path: "src/a.java".into(),
                value: 1,
            }],
        )
        .expect_err("an excluded path does not resolve an ambiguous scope");
        assert_eq!(error.message, "ambiguous path scopes for src/a.java");
    }
}

/// The selected paths, relative to the repository root.
pub fn selected_paths(
    paths: &[String],
    cwd: &std::path::Path,
    root: &std::path::Path,
) -> Vec<String> {
    paths
        .iter()
        .map(|path| {
            let path = std::path::Path::new(path);
            let absolute = if path.is_absolute() {
                path.to_path_buf()
            } else {
                cwd.join(path)
            };
            match crate::path::relative_to(root, &absolute) {
                Some(relative) => relative,
                None => path.to_string_lossy().replace('\\', "/"),
            }
        })
        .collect()
}

/// Builds the report of a `count` run: the counts without a comparison.
pub fn counts(config: &Config, root: &std::path::Path, rule_filter: Option<&str>) -> Report {
    let mut report = Report::default();
    for (id, rule) in &config.rules {
        if let Some(filter) = rule_filter {
            if filter != id {
                continue;
            }
        }
        report.executed += 1;
        let run = match counter::run_counter(&rule.count, root, &format!("rules.{id}.count")) {
            Ok(run) => run,
            Err(error) => {
                push_counter_error(&mut report, id, rule, error);
                continue;
            }
        };
        report.debug.push(format!(
            "counter {id}: {}\n{}\n--- count ---",
            rule.count.describe(),
            run.output
        ));
        if let Some(measurements) = run.value.measurements.as_ref() {
            let scoped = match scoped_counts(rule, measurements) {
                Ok(scoped) => scoped,
                Err(error) => {
                    push_counter_error(&mut report, id, rule, error);
                    continue;
                }
            };
            // A dead `exclude` pattern is a note here too: `count` reports
            // what it counted, and a pattern that never applied is worth
            // naming (SPEC 5.2).
            push_unmatched_excludes(&mut report, id, rule, &scoped.unmatched_excludes);
            // `count` is not a gate: the tolerance of `check` has no meaning
            // here, and an empty scope stays the error it has always been.
            if let Some((scope_id, description)) = scoped.empty.first() {
                push_counter_error(
                    &mut report,
                    id,
                    rule,
                    counter::CounterError::runtime(format!(
                        "scope {scope_id} ({description}) selected no files"
                    )),
                );
                // The counter ran and delivered its measurements; only the
                // scope is empty, so its output explains nothing.
                report.empty_scope_errors += 1;
                continue;
            }
            report.executed = report.executed.saturating_sub(1) + scoped.entries.len();
            for (scope_id, description, count) in scoped.entries {
                report.rules.push(RuleResult {
                    id: id.clone(),
                    scope_id,
                    scope_description: Some(description),
                    status: RuleStatus::Ok,
                    count: Some(count),
                    baseline: None,
                    direction: Some(rule.direction),
                    unit: Some(rule.unit.clone()),
                    class: Some(rule.class.clone()),
                    tier: Some(rule.tier),
                    detail: None,
                    committed: None,
                    findings: None,
                });
            }
            continue;
        }
        let finalized = match counter::finalize(
            run.value,
            &rule.exclude_globs,
            &format!("rules.{id}.exclude"),
        ) {
            Ok(finalized) => finalized,
            Err(error) => {
                push_counter_error(&mut report, id, rule, error);
                continue;
            }
        };
        push_unmatched_excludes(&mut report, id, rule, &finalized.unmatched_excludes);
        let count = finalized.count;
        report.debug.push(format!("counter {id}: count {count}"));
        report.rules.push(RuleResult {
            id: id.clone(),
            scope_id: None,
            scope_description: None,
            status: RuleStatus::Ok,
            count: Some(count),
            baseline: None,
            direction: Some(rule.direction),
            unit: Some(rule.unit.clone()),
            class: Some(rule.class.clone()),
            tier: Some(rule.tier),
            detail: None,
            committed: None,
            findings: run.findings,
        });
    }
    report.ok = report.failed() == 0 && report.counter_errors == 0;
    report
}

/// The scoped values of a per-file measurement rule.
///
/// `empty` names the configured scopes that no measurement reached: they are
/// stale artifacts, reported as notes by default and as errors under `--strict`
/// (SPEC 5.2).  They are not part of `entries`: a scope that selects nothing
/// is never compared.  A scope whose files exist but are all removed by
/// `exclude` is neither empty nor measured: it gets no entry and is not stale.
#[derive(Debug)]
pub(crate) struct ScopedCounts {
    /// One entry per scope that selected files, plus the global fallback.
    pub entries: Vec<(Option<String>, String, i64)>,
    /// The configured scopes that selected no file: `(id, description)`.
    pub empty: Vec<(String, String)>,
    /// The indices of the `exclude` patterns that matched no measurement.
    pub unmatched_excludes: Vec<usize>,
}

/// Splits measurements into the one effective scope for each path, then reduces each selection.
pub(crate) fn scoped_counts(
    rule: &Rule,
    measurements: &[counter::Measurement],
) -> Result<ScopedCounts, counter::CounterError> {
    let aggregate = rule.aggregate.ok_or_else(|| {
        counter::CounterError::config("config error: per-file measurements require an aggregate")
    })?;
    if measurements.is_empty() {
        return Err(counter::CounterError::runtime(
            "the counter selected no files",
        ));
    }
    let mut groups: std::collections::BTreeMap<Option<String>, Vec<i64>> =
        std::collections::BTreeMap::new();
    let mut descriptions = std::collections::BTreeMap::new();
    let mut exact_files = std::collections::BTreeSet::new();
    for scope in &rule.scopes {
        if let Some(file) = &scope.file {
            if !exact_files.insert(file) {
                return Err(counter::CounterError::config(format!(
                    "config error: more than one exact scope names {file}"
                )));
            }
        }
        descriptions.insert(Some(scope.id.clone()), scope.description.clone());
    }
    // The scopes are assigned **before** `exclude` decides whether a
    // measurement contributes: a scope whose files still exist but are all
    // excluded is not empty, it is simply not measured.
    let mut matched_scopes = std::collections::BTreeSet::new();
    for measurement in measurements {
        let exact: Vec<_> = rule
            .scopes
            .iter()
            .filter(|scope| scope.file.as_deref() == Some(&measurement.path))
            .collect();
        let selected = if exact.len() > 1 {
            return Err(counter::CounterError::runtime(format!(
                "ambiguous exact scopes for {}",
                measurement.path
            )));
        } else if let Some(scope) = exact.first() {
            Some(*scope)
        } else {
            let matched: Vec<_> = rule
                .scopes
                .iter()
                .filter(|scope| {
                    scope.file.is_none()
                        && scope
                            .globs
                            .iter()
                            .any(|glob| glob.is_match(&measurement.path))
                })
                .collect();
            let mut ranked: Vec<_> = matched
                .iter()
                .map(|scope| (glob_specificity(&scope.files, &measurement.path), *scope))
                .collect();
            ranked.sort_by(|left, right| {
                right
                    .0
                    .cmp(&left.0)
                    .then_with(|| left.1.id.cmp(&right.1.id))
            });
            if ranked.len() > 1 && ranked[0].0 == ranked[1].0 {
                return Err(counter::CounterError::runtime(format!(
                    "ambiguous path scopes for {}",
                    measurement.path
                )));
            }
            ranked.first().map(|(_, scope)| *scope)
        };
        let key = selected.map(|scope| scope.id.clone());
        if let Some(scope_id) = &key {
            matched_scopes.insert(scope_id.clone());
        }
        if rule
            .exclude_globs
            .iter()
            .flatten()
            .any(|glob| glob.is_match(&measurement.path))
        {
            continue;
        }
        groups.entry(key).or_default().push(measurement.value);
    }
    let unmatched_excludes = rule
        .exclude_globs
        .iter()
        .enumerate()
        .filter(|(_, group)| {
            !group
                .iter()
                .any(|glob| measurements.iter().any(|m| glob.is_match(&m.path)))
        })
        .map(|(index, _)| index)
        .collect();
    let mut empty = Vec::new();
    for scope in &rule.scopes {
        if !matched_scopes.contains(&scope.id) {
            empty.push((scope.id.clone(), scope.description.clone()));
        }
    }
    if groups.is_empty() {
        return Err(counter::CounterError::runtime(
            "the counter selected no files",
        ));
    }
    let mut entries = Vec::new();
    for (id, values) in groups {
        if values.is_empty() {
            continue;
        }
        let value = match aggregate {
            crate::config::Aggregate::Count => i64::try_from(values.len())
                .map_err(|_| counter::CounterError::runtime("measurement count is too large"))?,
            crate::config::Aggregate::Max => values
                .into_iter()
                .max()
                .ok_or_else(|| counter::CounterError::runtime("scope selected no files"))?,
        };
        let description = id
            .as_ref()
            .and_then(|id| descriptions.get(&Some(id.clone())).cloned())
            .or_else(|| rule.description.clone())
            .unwrap_or_else(|| "Global fallback".to_string());
        entries.push((id, description, value));
    }
    Ok(ScopedCounts {
        entries,
        empty,
        unmatched_excludes,
    })
}

fn glob_specificity(patterns: &[String], path: &str) -> (usize, usize) {
    patterns
        .iter()
        .filter_map(|pattern| {
            let matches = crate::config::compile_globs(std::slice::from_ref(pattern), "scope")
                .ok()
                .is_some_and(|globs| globs.iter().any(|glob| glob.is_match(path)));
            if !matches {
                return None;
            }
            let literal = pattern
                .chars()
                .filter(|ch| !matches!(ch, '*' | '?' | '[' | ']' | '{' | '}' | '!'))
                .count();
            let components = pattern
                .split('/')
                .filter(|part| !part.contains('*') && !part.contains('?'))
                .count();
            Some((literal, components))
        })
        .max()
        .unwrap_or_default()
}

fn count_status(direction: Direction, count: i64, baseline: Option<i64>) -> RuleStatus {
    match baseline {
        None => RuleStatus::MissingBaseline,
        Some(value) if direction == Direction::Ceiling && count > value => RuleStatus::OverBaseline,
        Some(value) if direction == Direction::Floor && count < value => RuleStatus::UnderFloor,
        Some(_) => RuleStatus::Ok,
    }
}

fn status_detail(
    id: &str,
    scope: Option<&str>,
    count: i64,
    baseline: Option<i64>,
    status: RuleStatus,
) -> Option<String> {
    let name = scope
        .map(|scope| format!("{id} scope {scope}"))
        .unwrap_or_else(|| id.to_string());
    match status {
        RuleStatus::MissingBaseline => Some(format!("no baseline for {name}")),
        RuleStatus::OverBaseline => Some(format!(
            "count {count} > baseline {}",
            baseline.unwrap_or_default()
        )),
        RuleStatus::UnderFloor => Some(format!(
            "count {count} < baseline {}",
            baseline.unwrap_or_default()
        )),
        _ => None,
    }
}

/// The result of a `baseline init` or `baseline update` run.
#[derive(Debug, Clone)]
pub struct BaselineAction {
    /// The rule id.
    pub id: String,
    /// What happened.
    pub kind: ActionKind,
    /// The baseline value, if the rule has one.
    pub value: Option<i64>,
    /// A human readable message.
    pub message: String,
}

/// The kind of a baseline action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    /// A baseline was written for a rule that had none.
    Added,
    /// An existing baseline was left untouched.
    Kept,
    /// An existing baseline was moved towards the current count.
    Updated,
    /// The baseline was not allowed to move.
    Refused,
    /// A stale record was left untouched (the tolerant `baseline init`).
    Ignored,
    /// A stale record was removed (`baseline update`).
    Pruned,
}

impl ActionKind {
    /// The name used in the JSON report.
    pub fn as_str(self) -> &'static str {
        match self {
            ActionKind::Added => "added",
            ActionKind::Kept => "kept",
            ActionKind::Updated => "updated",
            ActionKind::Refused => "refused",
            ActionKind::Ignored => "ignored",
            ActionKind::Pruned => "pruned",
        }
    }
}
