//! End to end tests of the Markdown report: a temporary git repository, the
//! real binary.
//!
//! The report is a **rendering of a run**: the numbers are the numbers of the
//! run, and the findings are the findings the count was made of (SPEC 14).
//! The report runs the **rules** only — the drift gates are gates over the
//! generated files, and the report is one of them.

mod common;

use std::path::Path;
use std::process::{Command, Output};

use common::TempDir;
use serde_json::Value;

/// The command that runs the binary.
///
/// The update check needs the network and the tests do not: the variable
/// keeps the tests hermetic.
fn binary() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ratchet"));
    command.env("RATCHET_NO_UPDATE_CHECK", "1");
    command
}

/// A temporary git repository with a config and a baseline.
struct Repo {
    dir: TempDir,
}

impl Repo {
    /// Creates a repository with a config, both committed.
    fn new(config: &str) -> Self {
        let repo = Self::plain(config);
        repo.git(&["init", "-q", "."]);
        repo.git(&["add", "-A"]);
        repo.git(&["commit", "-q", "-m", "init"]);
        repo
    }

    /// Creates a directory with a config, but no repository.
    fn plain(config: &str) -> Self {
        let dir = TempDir::new().expect("a temporary directory");
        let repo = Self { dir };
        repo.write("ratchet.config.jsonc", config);
        repo
    }

    /// The root of the repository.
    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Writes a file into the repository.
    fn write(&self, name: &str, content: &str) {
        let path = self.dir.path().join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("the parent directory is created");
        }
        std::fs::write(path, content).expect("the file is written");
    }

    /// Reads a file from the repository.
    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.dir.path().join(name)).expect("the file is readable")
    }

    /// The state of the working tree.
    fn status(&self) -> String {
        let output = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(self.path())
            .output()
            .expect("git runs");
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// Runs a git command in the repository.
    fn git(&self, arguments: &[&str]) {
        let status = Command::new("git")
            .args(arguments)
            .current_dir(self.path())
            .env("GIT_AUTHOR_NAME", "ratchet test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "ratchet test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .status()
            .expect("git runs");
        assert!(status.success(), "git {arguments:?} failed");
    }

    /// Commits everything that is in the tree.
    fn commit_all(&self) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", "the files"]);
    }

    /// Runs the binary in the repository.
    fn run(&self, arguments: &[&str]) -> Output {
        binary()
            .args(arguments)
            .current_dir(self.path())
            .output()
            .expect("the binary runs")
    }

    /// Runs the binary with the environment of a caller.
    fn run_with(&self, environment: &[(&str, &str)], arguments: &[&str]) -> Output {
        let mut command = binary();
        command.args(arguments).current_dir(self.path());
        for (name, value) in environment {
            command.env(name, value);
        }
        command.output().expect("the binary runs")
    }
}

/// The stdout of a run.
fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The stderr of a run.
fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Everything a run said.
fn said(output: &Output) -> String {
    format!("{}{}", stdout(output), stderr(output))
}

/// The code of a run.
fn code(output: &Output) -> i32 {
    output.status.code().expect("the process exited normally")
}

/// The findings of the fixture: two of them count.
const FINDINGS: &str = r#"{
  "version": 1,
  "findings": [
    { "id": "SEC-01", "title": "org admins reassign foreign users", "severity": "critical",
      "status": "open", "owner": "alice", "ticket": "D-12" },
    { "id": "SEC-02", "title": "a high one", "severity": "high", "status": "in-progress",
      "owner": "bob", "ticket": "D-13" },
    { "id": "SEC-03", "title": "a medium one", "severity": "medium", "status": "open",
      "owner": "alice", "ticket": "D-14" }
  ]
}
"#;

/// The drift gate of the fixture: a file that a script writes in place.
const GENERATED: &str = r#"{ "path": "docs/generated.md", "command": ["sh", "generate.sh"] }"#;

/// A config with two rules — one of them counting a registry — and one
/// generated file.
fn config(generated: &str) -> String {
    format!(
        r#"[
      {{
        "name": "test",
        "rules": {{
          "todo-markers": {{
            "class": "unfinished work",
            "incident": "a TODO marker was merged without a tracking issue",
            "unit": "markers",
            "direction": "ceiling",
            // Constant baselines keep the documented report numbers: a
            // `report` run would otherwise tighten auto ceilings to 2 and 3.
            "baseline": "fixed",
            "tier": "fast",
            "count": {{ "command": ["echo", "3"] }}
          }},
          "open-findings": {{
            "class": "open security findings",
            "incident": "a finding was closed without a regression test",
            "unit": "findings",
            "direction": "ceiling",
            "baseline": "fixed",
            "tier": "fast",
            "count": {{ "registry": "ratchet.findings.json", "severities": ["critical", "high"],
                       "statuses": ["open", "in-progress"] }}
          }}
        }},
        "generated": [{generated}]
      }}
    ]"#
    )
}

/// The baseline of the fixture.
const BASELINE: &str = r#"{
  "version": 1,
  "rules": {
    "todo-markers": { "value": 5 },
    "open-findings": { "value": 3 }
  }
}
"#;

/// The script that writes the generated file.
const GENERATOR: &str = "#!/bin/sh\nprintf 'generated\\n' > docs/generated.md\n";

/// A repository with the fixture: two rules, a registry and a generated file.
fn fixture() -> Repo {
    let repo = Repo::new(&config(GENERATED));
    repo.write("generate.sh", GENERATOR);
    repo.write("docs/generated.md", "generated\n");
    repo.write("ratchet.findings.json", FINDINGS);
    repo.write("ratchet.baseline.json", BASELINE);
    repo.commit_all();
    repo
}

/// The report of the fixture — the exact text the binary writes.
///
/// The report holds the **rules** and nothing else: no drift section, and no
/// generated file in the summary.
const EXPECTED: &str = r#"# Ratchet report

**Generated — do not edit by hand.** Regenerate with `ratchet report --format md --output <path>`.

## Summary

**Verdict: COHERENT**

| Rules | Failed | Executed |
|---|---|---|
| 2 | 0 | 2 |

## Rules

| Rule | Class | Direction | Tier | Count | Baseline | Status | Detail |
|---|---|---|---|---|---|---|---|
| open-findings | open security findings | ceiling | fast | 2 | 3 | ok | — |
| todo-markers | unfinished work | ceiling | fast | 3 | 5 | ok | — |

## Findings

### open-findings

| Id | Severity | Status | Title | Owner | Ticket |
|---|---|---|---|---|---|
| SEC-01 | critical | open | org admins reassign foreign users | alice | D-12 |
| SEC-02 | high | in-progress | a high one | bob | D-13 |
"#;

/// True when the text carries a date of the form `2026-10-21`.
fn has_a_date(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.windows(10).any(|window| {
        window[0..4].iter().all(u8::is_ascii_digit)
            && window[4] == b'-'
            && window[5..7].iter().all(u8::is_ascii_digit)
            && window[7] == b'-'
            && window[8..10].iter().all(u8::is_ascii_digit)
    })
}

#[test]
fn the_report_is_the_run_rendered_in_markdown() {
    let repo = fixture();

    let output = repo.run(&["report", "--format", "md"]);

    assert_eq!(code(&output), 0, "{}", said(&output));
    // The report is the only thing on stdout.
    assert_eq!(stdout(&output), EXPECTED);
}

#[test]
fn the_numbers_of_the_report_are_the_numbers_of_the_run() {
    let repo = fixture();

    let output = repo.run(&["report", "--format", "md"]);
    let report = stdout(&output);
    let json = repo.run(&["check", "--format", "json"]);
    let machine: Value = serde_json::from_str(&stdout(&json)).expect("the stdout is JSON");

    // The summary of the report is the summary of the run.
    assert!(
        report.contains(&format!(
            "| {} | {} | {} |",
            machine["summary"]["rules"],
            machine["summary"]["failed"],
            machine["summary"]["executed"],
        )),
        "{report}"
    );
    // One row per rule, with the numbers of that rule.
    for rule in machine["rules"].as_array().expect("an array") {
        let row = format!(
            "| {} | {} | {} | {} | {} | {} | {} |",
            rule["id"].as_str().expect("an id"),
            rule["class"].as_str().expect("a class"),
            rule["direction"].as_str().expect("a direction"),
            rule["tier"].as_str().expect("a tier"),
            rule["count"].as_i64().expect("a count"),
            rule["baseline"].as_i64().expect("a baseline"),
            rule["status"].as_str().expect("a status"),
        );
        assert!(report.contains(&row), "{row} is missing in:\n{report}");
    }
}

#[test]
fn two_runs_produce_the_same_bytes() {
    let repo = fixture();

    let first = repo.run(&["report", "--format", "md"]);
    let second = repo.run(&["report", "--format", "md"]);

    assert_eq!(code(&first), 0, "{}", said(&first));
    assert_eq!(stdout(&first), stdout(&second));
}

#[test]
fn the_report_carries_no_absolute_path_no_timestamp_and_no_machine_name() {
    let repo = fixture();

    let output = repo.run(&["report", "--format", "md"]);
    let report = stdout(&output);

    // The path of the repository itself is machine-specific.
    let root = repo.path().to_string_lossy().into_owned();
    assert!(!report.contains(&root), "{report}");
    // A date would make the same tree produce different bytes.
    assert!(!has_a_date(&report), "{report}");
    // The name of the machine, when the environment names it.
    if let Ok(host) = std::env::var("HOSTNAME") {
        assert!(!report.contains(&host), "{report}");
    }
}

#[test]
fn the_report_does_not_depend_on_the_environment() {
    let repo = fixture();

    // The variables of an earlier design are ignored: nothing about the
    // report is read from the environment.
    let output = repo.run_with(
        &[
            ("RATCHET_GENERATED_PATH", "docs/generated.md"),
            ("RATCHET_REPORT_CHILD", "1"),
        ],
        &["report", "--format", "md"],
    );

    assert_eq!(code(&output), 0, "{}", said(&output));
    assert_eq!(stdout(&output), EXPECTED);
}

#[test]
fn the_findings_that_count_are_the_findings_the_number_was_made_of() {
    let repo = fixture();

    let output = repo.run(&["report", "--format", "md"]);
    let report = stdout(&output);

    // SEC-01 and SEC-02 count: SEC-03 is a medium one, and the rule names
    // the critical and the high findings.
    assert!(report.contains("| SEC-01 | critical | open |"), "{report}");
    assert!(
        report.contains("| SEC-02 | high | in-progress |"),
        "{report}"
    );
    assert!(!report.contains("SEC-03"), "{report}");
}

#[test]
fn a_pipe_in_a_finding_does_not_open_a_column() {
    let repo = fixture();
    repo.write(
        "ratchet.findings.json",
        &FINDINGS.replace("org admins reassign foreign users", "a | b"),
    );
    repo.commit_all();

    let output = repo.run(&["report", "--format", "md"]);
    let report = stdout(&output);

    // The pipe is escaped: the row keeps its columns.
    assert!(
        report.contains("| SEC-01 | critical | open | a \\| b | alice | D-12 |"),
        "{report}"
    );
    // The row has the six cells of the table: the escaped pipe opens none.
    let row = report
        .lines()
        .find(|line| line.starts_with("| SEC-01 |"))
        .expect("the row of the finding");
    let cells = row.split(" | ").count();
    assert_eq!(cells, 6, "{row}");
}

// --- the red runs -------------------------------------------------------------

#[test]
fn a_failing_rule_is_a_red_report_and_a_green_command() {
    let repo = fixture();
    repo.write("ratchet.baseline.json", &baseline("todo-markers", 2));
    repo.commit_all();

    let output = repo.run(&["report", "--format", "md"]);

    // The report was produced: the command succeeded.
    assert_eq!(code(&output), 0, "{}", said(&output));
    let report = stdout(&output);
    // The verdict of the run.
    assert!(report.contains("**Verdict: NOT COHERENT**"), "{report}");
    // The reason is in the file: the row names it.
    assert!(report.contains("| over-baseline |"), "{report}");
    assert!(report.contains("count 3 > baseline 2"), "{report}");

    // The check on the same tree is red.
    let check = repo.run(&["check"]);
    assert_eq!(code(&check), 1, "{}", said(&check));
}

#[test]
fn the_tier_floor_is_a_red_report_and_a_green_command() {
    // The fast tier has a floor of 5 and the run executes 2 checks: the run
    // is red, and every rule is green.
    let repo = Repo::new(
        r#"[
      {
        "tiers": { "fast": { "floor": 5 } },
        "rules": {
          "todo-markers": {
            "class": "unfinished work",
            "incident": "a TODO marker was merged without a tracking issue",
            "unit": "markers",
            "direction": "ceiling",
            "count": { "command": ["echo", "3"] }
          }
        }
      }
    ]"#,
    );
    repo.write("ratchet.baseline.json", &baseline("todo-markers", 5));
    repo.commit_all();

    let output = repo.run(&["report", "--format", "md"]);

    assert_eq!(code(&output), 0, "{}", said(&output));
    let report = stdout(&output);
    assert!(report.contains("**Verdict: NOT COHERENT**"), "{report}");
    // The notes of the run: the tier floor.
    assert!(report.contains("## Notes"), "{report}");
    assert!(
        report.contains("the tier `fast` executed 1 check, the floor is 5"),
        "{report}"
    );
    // The numbers of the run: no rule failed, and the run is red all the same.
    assert!(report.contains("| 1 | 0 | 1 |"), "{report}");

    // The check on the same tree is red.
    let check = repo.run(&["check"]);
    assert_eq!(code(&check), 1, "{}", said(&check));
}

#[test]
fn the_empty_selection_is_a_red_report_and_a_green_command() {
    let repo = Repo::new("[]");

    let output = repo.run(&["report", "--format", "md"]);

    assert_eq!(code(&output), 0, "{}", said(&output));
    let report = stdout(&output);
    assert!(report.contains("**Verdict: NOT COHERENT**"), "{report}");
    assert!(
        report.contains("no check was executed — an empty selection is never a pass"),
        "{report}"
    );

    // The check on the same tree is red.
    let check = repo.run(&["check"]);
    assert_eq!(code(&check), 1, "{}", said(&check));
}

#[test]
fn a_counter_error_is_a_red_report_and_a_green_command() {
    let repo = fixture();
    let broken = repo
        .read("ratchet.config.jsonc")
        .replace("[\"echo\", \"3\"]", "[\"false\"]");
    repo.write("ratchet.config.jsonc", &broken);
    repo.commit_all();

    let output = repo.run(&["report", "--format", "md"]);

    // The report was produced: the command succeeded.
    assert_eq!(code(&output), 0, "{}", said(&output));
    let report = stdout(&output);
    assert!(report.contains("**Verdict: NOT COHERENT**"), "{report}");
    // The reason is in the file: the row names it.
    assert!(report.contains("| counter-error |"), "{report}");
    assert!(
        report.contains("the counter exited with status 1"),
        "{report}"
    );

    // The check on the same tree is an operational error.
    let check = repo.run(&["check"]);
    assert_eq!(code(&check), 2, "{}", said(&check));
}

#[test]
fn the_check_and_the_report_agree_about_the_floor() {
    // The fast tier has a floor of two and the run has one rule: the check
    // and the report are both red, and they count the same rules.
    let repo = Repo::new(
        r#"[
      {
        "tiers": { "fast": { "floor": 2 } },
        "rules": {
          "todo-markers": {
            "class": "unfinished work",
            "incident": "a TODO marker was merged without a tracking issue",
            "unit": "markers",
            "direction": "ceiling",
            "count": { "command": ["echo", "3"] }
          }
        }
      }
    ]"#,
    );
    repo.write("ratchet.baseline.json", &baseline("todo-markers", 5));
    repo.commit_all();

    let report = repo.run(&["report", "--format", "md"]);
    let check = repo.run(&["check", "--format", "json"]);

    assert_eq!(code(&report), 0, "{}", said(&report));
    assert!(stdout(&report).contains("**Verdict: NOT COHERENT**"));
    assert!(stdout(&report).contains("the tier `fast` executed 1 check, the floor is 2"));

    assert_eq!(code(&check), 1, "{}", said(&check));
    let machine: Value = serde_json::from_str(&stdout(&check)).expect("the stdout is JSON");
    assert_eq!(machine["ok"], false);
    let notes = machine["notes"].as_array().expect("the notes are an array");
    assert!(
        notes.iter().any(|note| note
            .as_str()
            .unwrap_or_default()
            .contains("the tier `fast` executed 1 check, the floor is 2")),
        "{notes:?}"
    );
}

// --- the output ---------------------------------------------------------------

#[test]
fn the_output_writes_the_file_and_nothing_else() {
    let repo = fixture();

    let output = repo.run(&["report", "--format", "md", "--output", "docs/report.md"]);

    assert_eq!(code(&output), 0, "{}", said(&output));
    // The report is in the file.
    assert_eq!(repo.read("docs/report.md"), EXPECTED);
    // The command says what it wrote, and nothing else is on stdout.
    assert_eq!(stdout(&output), "ratchet: wrote docs/report.md\n");
    // The tree carries the one file and nothing else.
    assert_eq!(repo.status(), "?? docs/report.md\n");
}

#[test]
fn a_missing_directory_is_an_error() {
    let repo = fixture();

    let output = repo.run(&["report", "--format", "md", "--output", "nowhere/report.md"]);

    // The command could not produce the report: an operational error.
    assert_eq!(code(&output), 2, "{}", said(&output));
    assert!(
        stderr(&output).contains("the directory nowhere does not exist"),
        "{}",
        said(&output)
    );
    assert_eq!(repo.status(), "");
}

#[test]
fn a_path_outside_the_repository_root_is_a_configuration_error() {
    let repo = fixture();

    let output = repo.run(&["report", "--format", "md", "--output", "../outside.md"]);

    assert_eq!(code(&output), 2, "{}", said(&output));
    assert!(
        stderr(&output).contains("must be written inside the repository root"),
        "{}",
        said(&output)
    );
}

#[test]
fn an_absolute_output_inside_the_repository_root_is_refused() {
    let repo = fixture();
    // The path is inside the repository — but it is absolute, and the paths
    // of a project are relative.
    let absolute = repo.path().join("report.md");

    let output = repo.run(&[
        "report",
        "--format",
        "md",
        "--output",
        absolute.to_str().expect("a path"),
    ]);

    assert_eq!(code(&output), 2, "{}", said(&output));
    let text = stderr(&output);
    // The message says what is wrong — and it does not blame a link.
    assert!(text.contains("has to be a relative path"), "{text}");
    assert!(!text.contains("symbolic link"), "{text}");
    // And nothing was written.
    assert_eq!(repo.status(), "");
}

#[cfg(unix)]
#[test]
fn the_output_does_not_escape_the_repository_root_through_a_symlink() {
    let repo = fixture();
    // The link points outside the repository: the report must not follow it.
    let outside = repo
        .path()
        .parent()
        .expect("a parent directory")
        .join(format!(
            "{}-outside.md",
            repo.path()
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default()
        ));
    std::os::unix::fs::symlink(&outside, repo.path().join("link.md")).expect("the link is created");

    let output = repo.run(&["report", "--format", "md", "--output", "link.md"]);

    assert_eq!(code(&output), 2, "{}", said(&output));
    assert!(
        stderr(&output).contains("must be written inside the repository root"),
        "{}",
        said(&output)
    );
    // Nothing was written outside the repository.
    assert!(
        !outside.exists(),
        "the report was written outside the repository"
    );
    // And nothing inside either: only the link the test created.
    assert_eq!(repo.status(), "?? link.md\n");
}

#[cfg(unix)]
#[test]
fn a_chain_of_links_does_not_carry_the_report_out_of_the_repository_root() {
    let repo = fixture();
    // The chain is what carries the path out: `report.md` → `real/report.md`
    // → a file outside the repository that is not there.
    let outside = repo
        .path()
        .parent()
        .expect("a parent directory")
        .join(format!(
            "{}-chain.md",
            repo.path()
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default()
        ));
    std::fs::create_dir_all(repo.path().join("real")).expect("the directory is created");
    std::os::unix::fs::symlink(&outside, repo.path().join("real/report.md"))
        .expect("the second link is made");
    std::os::unix::fs::symlink(
        repo.path().join("real/report.md"),
        repo.path().join("report.md"),
    )
    .expect("the first link is made");

    let output = repo.run(&["report", "--format", "md", "--output", "report.md"]);

    assert_eq!(code(&output), 2, "{}", said(&output));
    assert!(
        stderr(&output).contains("must be written inside the repository root"),
        "{}",
        said(&output)
    );
    // Nothing was written outside the repository.
    assert!(
        !outside.exists(),
        "the report was written outside the repository"
    );
}

#[cfg(unix)]
#[test]
fn a_symlink_inside_the_repository_root_is_honoured() {
    let repo = fixture();
    std::os::unix::fs::symlink("docs/real.md", repo.path().join("link.md"))
        .expect("the link is created");

    let output = repo.run(&["report", "--format", "md", "--output", "link.md"]);

    assert_eq!(code(&output), 0, "{}", said(&output));
    // The report is written through the link — inside the repository.
    assert_eq!(repo.read("docs/real.md"), EXPECTED);
}

// --- the options --------------------------------------------------------------

#[test]
fn the_commands_that_print_one_line_refuse_a_format_they_cannot_honour() {
    let repo = fixture();

    for command in [vec!["baseline", "path"], vec!["update"]] {
        for format in ["md", "json"] {
            let mut arguments = command.clone();
            arguments.extend(["--format", format]);
            let output = repo.run(&arguments);

            assert_eq!(
                code(&output),
                2,
                "{} --format {format}: {}",
                command.join(" "),
                said(&output)
            );
            assert!(
                stderr(&output).contains(if format == "md" {
                    "the Markdown report is written by `ratchet report --format md`"
                } else {
                    "this command prints one line — it has no JSON output"
                }),
                "{} --format {format}: {}",
                command.join(" "),
                said(&output)
            );
        }
    }
}

// --- the options --------------------------------------------------------------

#[test]
fn the_markdown_format_belongs_to_the_report_command() {
    let repo = fixture();

    for command in ["check", "count", "drift"] {
        let output = repo.run(&[command, "--format", "md"]);

        assert_eq!(code(&output), 2, "{command}: {}", said(&output));
        assert!(
            stderr(&output)
                .contains("the Markdown report is written by `ratchet report --format md`"),
            "{command}: {}",
            said(&output)
        );
    }
}

#[test]
fn the_report_command_writes_the_markdown_report() {
    let repo = fixture();

    for format in ["stylish", "json"] {
        let output = repo.run(&["report", "--format", format]);

        assert_eq!(code(&output), 2, "{format}: {}", said(&output));
        assert!(
            stderr(&output).contains("the report command writes the Markdown report"),
            "{format}: {}",
            said(&output)
        );
    }
}

#[test]
fn the_report_command_has_no_tier_and_no_fix() {
    let repo = fixture();

    for option in ["--tier", "--fix"] {
        let output = repo.run(&["report", option, "fast"]);

        assert_eq!(code(&output), 2, "{option}: {}", said(&output));
        assert!(
            stderr(&output).contains("unexpected argument"),
            "{option}: {}",
            said(&output)
        );
    }
}

#[test]
fn the_help_of_the_report_command_offers_no_fix() {
    let repo = fixture();

    let output = repo.run(&["report", "--help"]);

    assert_eq!(code(&output), 0, "{}", said(&output));
    let help = stdout(&output);
    assert!(help.contains("--output"), "{help}");
    assert!(!help.contains("--fix"), "{help}");
}

// --- the drift gate over the report -------------------------------------------

/// The drift gate of the report itself: the binary writes the report.
fn report_gate(path: &str) -> String {
    serde_json::json!({
        "path": path,
        "command": [
            env!("CARGO_BIN_EXE_ratchet"),
            "report",
            "--format",
            "md",
            "--output",
            path
        ]
    })
    .to_string()
}

/// A repository whose only generated file is the report.
fn report_repo(generated: &str) -> Repo {
    let repo = Repo::new(
        &r#"[
      {
        "rules": {
          "todo-markers": {
            "class": "unfinished work",
            "incident": "a TODO marker was merged without a tracking issue",
            "unit": "markers",
            "direction": "ceiling",
            // A fixed baseline keeps the check from writing during the
            // drift-gate test: that test asserts the tree is untouched.
            "baseline": "fixed",
            "count": { "command": ["echo", "3"] }
          }
        },
        "generated": [GENERATED]
      }
    ]"#
        .replace("GENERATED", generated),
    );
    repo.write("ratchet.baseline.json", &baseline("todo-markers", 5));
    // The report is written into `docs/`: the directory has to exist — the
    // command writes one file and creates nothing.
    std::fs::create_dir_all(repo.path().join("docs")).expect("the directory is created");
    repo.commit_all();
    repo
}

#[test]
fn the_drift_gate_accepts_the_report() {
    let repo = report_repo(&report_gate("docs/a.md"));

    // The report is written and committed.
    let output = repo.run(&["report", "--format", "md", "--output", "docs/a.md"]);
    assert_eq!(code(&output), 0, "{}", said(&output));
    repo.commit_all();

    // The drift gate compares the committed report with the produced one.
    let check = repo.run(&["check", "--format", "json"]);
    assert_eq!(code(&check), 0, "{}", said(&check));
    let report: Value = serde_json::from_str(&stdout(&check)).expect("the stdout is JSON");
    assert_eq!(report["summary"]["generated"], 1);
    assert_eq!(report["summary"]["drifted"], 0);
    assert_eq!(report["generated"][0]["path"], "docs/a.md");
    assert_eq!(report["generated"][0]["status"], "ok");
    // The check leaves the tree alone.
    assert_eq!(repo.status(), "");
}

#[test]
fn the_drift_gate_runs_the_report_even_when_the_run_is_red() {
    let repo = report_repo(&report_gate("docs/a.md"));
    // The rule is over its baseline: the run is red.
    repo.write("ratchet.baseline.json", &baseline("todo-markers", 2));
    repo.commit_all();

    // The report is produced — and the command succeeds: the report is a
    // generator, and a generator that failed would fail the gate.
    let output = repo.run(&["report", "--format", "md", "--output", "docs/a.md"]);
    assert_eq!(code(&output), 0, "{}", said(&output));
    assert!(repo.read("docs/a.md").contains("**Verdict: NOT COHERENT**"));
    repo.commit_all();

    // The gate runs the report and compares it with the committed one.
    let check = repo.run(&["check", "--format", "json"]);
    assert_eq!(code(&check), 1, "{}", said(&check));
    let report: Value = serde_json::from_str(&stdout(&check)).expect("the stdout is JSON");
    assert_eq!(report["summary"]["drifted"], 0);
    assert_eq!(report["generated"][0]["status"], "ok");
}

#[test]
fn a_hand_edited_report_is_drift() {
    let repo = report_repo(&report_gate("docs/a.md"));
    let output = repo.run(&["report", "--format", "md", "--output", "docs/a.md"]);
    assert_eq!(code(&output), 0, "{}", said(&output));
    repo.commit_all();

    // Somebody edits the report by hand.
    let committed = repo.read("docs/a.md");
    let edited = committed.replacen("| 1 | 0 | 1 |", "| 1 | 1 | 1 |", 1);
    assert_ne!(edited, committed, "the summary was not edited");
    repo.write("docs/a.md", &edited);
    repo.commit_all();

    // The gate sees the difference between the committed file and the
    // produced one.
    let check = repo.run(&["check", "--format", "json"]);
    assert_eq!(code(&check), 1, "{}", said(&check));
    let report: Value = serde_json::from_str(&stdout(&check)).expect("the stdout is JSON");
    assert_eq!(report["summary"]["drifted"], 1);
    assert_eq!(report["generated"][0]["status"], "drift");
}

#[test]
fn the_report_does_not_change_when_a_generated_file_drifts() {
    let repo = fixture();

    let before = repo.run(&["report", "--format", "md"]);
    assert_eq!(code(&before), 0, "{}", said(&before));

    // The generated file drifts: the check is red.
    repo.write("docs/generated.md", "stale\n");
    repo.commit_all();
    let check = repo.run(&["check"]);
    assert_eq!(code(&check), 1, "{}", said(&check));

    // The report is a function of the rules alone: it does not change.
    let after = repo.run(&["report", "--format", "md"]);
    assert_eq!(code(&after), 0, "{}", said(&after));
    assert_eq!(stdout(&before), stdout(&after));
    // And it holds no drift state at all.
    assert!(
        !stdout(&after).contains("docs/generated.md"),
        "{}",
        stdout(&after)
    );
}

/// A baseline with one record.
fn baseline(rule: &str, value: i64) -> String {
    format!("{{ \"version\": 1, \"rules\": {{ \"{rule}\": {{ \"value\": {value} }} }} }}\n")
}
