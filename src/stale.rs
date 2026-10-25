//! Stale artifacts: things the config no longer configures.
//!
//! The mechanism is **"no fail on old things"**: an artifact that was an
//! exception once and is not one any more is a note by default and a failure
//! only under `--strict` (SPEC 5.2).  `baseline update` prunes it (SPEC 5.5).
//!
//! The messages live here so that `check`, `baseline init` and
//! `baseline update` name the same artifact with the same words.

use crate::baseline::Baseline;
use crate::config::{Config, Rule};

/// One stale artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stale {
    /// A baseline record for a rule that is not configured.
    UnknownRule {
        /// The rule id of the baseline record.
        rule: String,
    },
    /// A baseline scope record for a scope the rule does not configure.
    UnknownScope {
        /// The rule id of the baseline record.
        rule: String,
        /// The scope id of the baseline record.
        scope: String,
    },
    /// A configured scope that selects no files.
    EmptyScope {
        /// The rule that configures the scope.
        rule: String,
        /// The scope id.
        scope: String,
        /// The human description of the scope.
        description: String,
    },
}

impl Stale {
    /// The id the baseline command reports the artifact under.
    pub fn action_id(&self) -> String {
        match self {
            Stale::UnknownRule { rule } => rule.clone(),
            Stale::UnknownScope { rule, scope } | Stale::EmptyScope { rule, scope, .. } => {
                format!("{rule}#{scope}")
            }
        }
    }

    /// The note of a `check` (or `report`) run.
    ///
    /// Without `--strict` the artifact is ignored with a hint; with it the
    /// run fails and the note says what to remove (SPEC 5.2).
    pub fn check_note(&self, strict: bool) -> String {
        match self {
            Stale::UnknownRule { rule } if strict => format!(
                "baseline record for unknown rule \"{rule}\" — remove it or run ratchet \
                 baseline update"
            ),
            Stale::UnknownRule { rule } => format!(
                "baseline record for unknown rule \"{rule}\" — ignored (run ratchet baseline \
                 update to prune it)"
            ),
            Stale::UnknownScope { rule, scope } if strict => format!(
                "baseline scope \"{scope}\" for rule \"{rule}\" is not configured — remove it or \
                 run ratchet baseline update"
            ),
            Stale::UnknownScope { rule, scope } => format!(
                "baseline scope \"{scope}\" for rule \"{rule}\" is not configured — ignored (run \
                 ratchet baseline update to prune it)"
            ),
            Stale::EmptyScope {
                scope, description, ..
            } if strict => format!("scope {scope} ({description}) selected no files"),
            Stale::EmptyScope { rule, scope, .. } => format!(
                "rule {rule} scope {scope} selects no files — ignored (remove the scope from the \
                 config)"
            ),
        }
    }

    /// The action of a `baseline init` that documents and leaves the record be.
    pub fn init_ignored(&self) -> String {
        match self {
            Stale::UnknownRule { rule } => format!(
                "baseline record for unknown rule \"{rule}\" — left untouched (run ratchet \
                 baseline update to prune)"
            ),
            Stale::UnknownScope { rule, scope } => format!(
                "baseline scope \"{scope}\" for rule \"{rule}\" is not configured — left \
                 untouched (run ratchet baseline update to prune)"
            ),
            Stale::EmptyScope { scope, .. } => {
                format!("scope {scope} selects no files — no baseline written")
            }
        }
    }

    /// The action of a `baseline update` that removes the record.
    pub fn update_pruned(&self) -> String {
        match self {
            Stale::UnknownRule { rule } => {
                format!("rule \"{rule}\" is no longer configured — pruned")
            }
            Stale::UnknownScope { scope, .. } => {
                format!("scope \"{scope}\" is no longer configured — pruned")
            }
            Stale::EmptyScope { scope, .. } => {
                format!("scope \"{scope}\" selects no files — pruned")
            }
        }
    }
}

/// The baseline rule records for rules the config no longer configures.
pub fn unknown_rules(config: &Config, baseline: &Baseline) -> Vec<Stale> {
    baseline
        .rules
        .keys()
        .filter(|id| !config.rules.contains_key(*id))
        .map(|rule| Stale::UnknownRule { rule: rule.clone() })
        .collect()
}

/// The baseline scope records of `rule` for scopes it does not configure.
pub fn unknown_scopes(rule: &Rule, baseline: &Baseline) -> Vec<Stale> {
    let Some(record) = baseline.get(&rule.id) else {
        return Vec::new();
    };
    record
        .scopes
        .keys()
        .filter(|scope| {
            !rule
                .scopes
                .iter()
                .any(|configured| &configured.id == *scope)
        })
        .map(|scope| Stale::UnknownScope {
            rule: rule.id.clone(),
            scope: scope.clone(),
        })
        .collect()
}
