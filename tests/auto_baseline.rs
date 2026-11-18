//! Auto-advancing baselines: the check tightens gates instead of merely
//! reporting a count that happens to be lower.
//!
//! The tests use the built-in `file_metrics` counter, so they need neither a
//! shell nor an external program and run on every platform the suite supports.
//! The counters are controlled by the number of lines in a file under `src/`.

mod common;

use std::fs;
use std::process::{Command, Output};

use common::TempDir;
use serde_json::Value;

/// The command that runs the binary.
fn binary() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ratchet"));
    command.env("RATCHET_NO_UPDATE_CHECK", "1");
    command
}

/// A project with a config and, optionally, a baseline.
fn project(config: &str, baseline: Option<&str>) -> TempDir {
    let directory = TempDir::new().expect("a temporary directory");
    fs::write(directory.path().join("ratchet.config.jsonc"), config)
        .expect("the config is written");
    if let Some(baseline) = baseline {
        fs::write(directory.path().join("ratchet.baseline.json"), baseline)
            .expect("the baseline is written");
    }
    directory
}

/// Writes `count` lines to `src/a.txt`; the `line_count` metric reads it back.
fn lines(directory: &TempDir, count: usize) {
    let source = directory.path().join("src");
    fs::create_dir_all(&source).expect("the source directory is created");
    let mut text = String::new();
    for number in 0..count {
        text.push_str(&format!("line {number}\n"));
    }
    fs::write(source.join("a.txt"), text).expect("the source file is written");
}

/// A rule with one flat baseline that counts lines under `src/`.
fn flat_config(direction: &str, mode: &str) -> String {
    format!(
        r#"{{"rules":{{"r":{{"class":"growth","incident":"test","unit":"lines","direction":"{direction}",{mode}"aggregate":"max","count":{{"file_metrics":{{"files":["src/*.txt"],"metric":"line_count"}}}}}}}}}}"#
    )
}

/// The baseline of the flat rule.
fn flat_baseline(value: i64) -> String {
    format!(r#"{{"version":1,"rules":{{"r":{{"value":{value}}}}}}}"#)
}

/// Runs the binary in the project with the given arguments.
fn run(directory: &TempDir, arguments: &[&str]) -> Output {
    binary()
        .args(arguments)
        .current_dir(directory.path())
        .output()
        .expect("the binary runs")
}

/// The stdout of a run.
fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The baseline document of a project.
fn baseline_of(directory: &TempDir) -> Value {
    let text = fs::read_to_string(directory.path().join("ratchet.baseline.json"))
        .expect("the baseline is readable");
    serde_json::from_str(&text).expect("the baseline is JSON")
}

#[test]
fn check_tightens_an_auto_ceiling_and_writes_the_baseline() {
    let project = project(&flat_config("ceiling", ""), Some(&flat_baseline(5)));
    lines(&project, 3);

    let output = run(&project, &["check"]);

    assert!(output.status.success(), "{}", stdout(&output));
    // The PASS line shows the in-memory tightened value.
    assert!(stdout(&output).contains("3 ≤ 3"), "{}", stdout(&output));
    assert_eq!(baseline_of(&project)["rules"]["r"]["value"], 3);
}

#[test]
fn check_tightens_an_auto_floor_and_writes_the_baseline() {
    let project = project(&flat_config("floor", ""), Some(&flat_baseline(3)));
    lines(&project, 7);

    let output = run(&project, &["check"]);

    assert!(output.status.success(), "{}", stdout(&output));
    assert!(stdout(&output).contains("7 ≥ 7"), "{}", stdout(&output));
    assert_eq!(baseline_of(&project)["rules"]["r"]["value"], 7);
}

#[test]
fn a_fixed_rule_does_not_move_on_check() {
    let project = project(
        &flat_config("ceiling", "\"baseline\":\"fixed\","),
        Some(&flat_baseline(5)),
    );
    lines(&project, 3);

    let output = run(&project, &["check"]);

    assert!(output.status.success(), "{}", stdout(&output));
    assert!(stdout(&output).contains("3 ≤ 5"), "{}", stdout(&output));
    assert_eq!(baseline_of(&project)["rules"]["r"]["value"], 5);
}

#[test]
fn check_no_ratchet_leaves_the_baseline_untouched() {
    let project = project(&flat_config("ceiling", ""), Some(&flat_baseline(5)));
    lines(&project, 3);
    let before = fs::read_to_string(project.path().join("ratchet.baseline.json"))
        .expect("the baseline is readable");

    let dry = run(&project, &["check", "--no-ratchet"]);

    assert!(dry.status.success(), "{}", stdout(&dry));
    assert!(stdout(&dry).contains("3 ≤ 5"), "{}", stdout(&dry));
    assert_eq!(
        fs::read_to_string(project.path().join("ratchet.baseline.json"))
            .expect("the baseline is readable"),
        before,
        "a dry run must not mutate the tree"
    );

    let real = run(&project, &["check"]);
    assert!(real.status.success(), "{}", stdout(&real));
    assert_eq!(baseline_of(&project)["rules"]["r"]["value"], 3);
}

#[test]
fn a_failed_run_does_not_write_a_tightened_baseline() {
    let project = project(&flat_config("ceiling", ""), Some(&flat_baseline(2)));
    lines(&project, 3);
    let before = fs::read_to_string(project.path().join("ratchet.baseline.json"))
        .expect("the baseline is readable");

    let output = run(&project, &["check"]);

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        fs::read_to_string(project.path().join("ratchet.baseline.json"))
            .expect("the baseline is readable"),
        before,
        "a red run must not move a baseline"
    );
}

#[test]
fn baseline_raise_is_refused_for_an_auto_rule_even_with_a_reason() {
    let project = project(&flat_config("ceiling", ""), Some(&flat_baseline(5)));
    lines(&project, 3);
    let before = fs::read_to_string(project.path().join("ratchet.baseline.json"))
        .expect("the baseline is readable");

    let output = run(
        &project,
        &[
            "baseline",
            "raise",
            "--rule",
            "r",
            "--value",
            "7",
            "--reason",
            "planned growth",
        ],
    );

    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    let text = stdout(&output);
    assert!(text.contains("\"baseline\": \"fixed\""), "{text}");
    assert_eq!(
        fs::read_to_string(project.path().join("ratchet.baseline.json"))
            .expect("the baseline is readable"),
        before
    );
}

#[test]
fn baseline_raise_still_works_for_a_fixed_rule() {
    let project = project(
        &flat_config("ceiling", "\"baseline\":\"fixed\","),
        Some(&flat_baseline(5)),
    );
    lines(&project, 3);

    let output = run(
        &project,
        &[
            "baseline",
            "raise",
            "--rule",
            "r",
            "--value",
            "7",
            "--reason",
            "planned growth",
        ],
    );

    assert!(output.status.success(), "{}", stdout(&output));
    let baseline = baseline_of(&project);
    assert_eq!(baseline["rules"]["r"]["value"], 7);
    assert_eq!(baseline["rules"]["r"]["reason"], "planned growth");
}

#[test]
fn every_scope_and_the_global_fallback_tighten_in_one_write() {
    let config = r#"{"rules":{"r":{"class":"growth","incident":"test","unit":"lines","direction":"ceiling","aggregate":"max","count":{"file_metrics":{"files":["src/*.txt","src/auth/*.txt"],"metric":"line_count"}},"scopes":[{"id":"auth","description":"Auth","files":["src/auth/*.txt"]}]}}}"#;
    let baseline = r#"{"version":1,"rules":{"r":{"value":10,"scopes":{"auth":{"value":5}}}}}"#;
    let project = project(config, Some(baseline));
    lines(&project, 7);
    let auth = project.path().join("src/auth");
    fs::create_dir_all(&auth).expect("the scope directory is created");
    fs::write(auth.join("a.txt"), "x\nx\nx\n").expect("the scoped file is written");

    let output = run(&project, &["check"]);

    assert!(output.status.success(), "{}", stdout(&output));
    let baseline = baseline_of(&project);
    assert_eq!(
        baseline["rules"]["r"]["value"], 7,
        "the global fallback tightens"
    );
    assert_eq!(
        baseline["rules"]["r"]["scopes"]["auth"]["value"], 3,
        "the named scope tightens independently"
    );
    let text = stdout(&output);
    assert!(text.contains("7 ≤ 7"), "{text}");
    assert!(text.contains("3 ≤ 3"), "{text}");
}

#[test]
fn report_does_not_write_a_baseline() {
    let project = project(&flat_config("ceiling", ""), Some(&flat_baseline(5)));
    lines(&project, 3);
    let before = fs::read_to_string(project.path().join("ratchet.baseline.json"))
        .expect("the baseline is readable");

    let output = run(&project, &["report", "--format", "md"]);

    assert!(output.status.success(), "{}", stdout(&output));
    assert_eq!(
        fs::read_to_string(project.path().join("ratchet.baseline.json"))
            .expect("the baseline is readable"),
        before,
        "a report renders a run; it never owns the baseline"
    );
}

#[test]
fn a_red_run_shows_the_recorded_baseline() {
    // One rule would improve; another fails. The run is red, so nothing is
    // written — and the PASS line must not claim a baseline nobody wrote.
    let config = r#"{"rules":{
        "improve":{"class":"growth","incident":"test","unit":"lines","direction":"ceiling","aggregate":"max","count":{"file_metrics":{"files":["src/*.txt"],"metric":"line_count"}}},
        "fail":{"class":"growth","incident":"test","unit":"lines","direction":"ceiling","aggregate":"max","count":{"file_metrics":{"files":["src/*.txt"],"metric":"line_count"}}}
    }}"#;
    let baseline = r#"{"version":1,"rules":{"improve":{"value":5},"fail":{"value":1}}}"#;
    let project = project(config, Some(baseline));
    lines(&project, 3);
    let before = fs::read_to_string(project.path().join("ratchet.baseline.json"))
        .expect("the baseline is readable");

    let output = run(&project, &["check"]);

    assert_eq!(output.status.code(), Some(1));
    let text = stdout(&output);
    assert!(text.contains("3 ≤ 5"), "{text}");
    assert_eq!(
        fs::read_to_string(project.path().join("ratchet.baseline.json"))
            .expect("the baseline is readable"),
        before,
        "a red run must not move a baseline"
    );
}

#[test]
fn report_renders_the_recorded_baseline_without_writing() {
    let project = project(&flat_config("ceiling", ""), Some(&flat_baseline(5)));
    lines(&project, 3);
    let before = fs::read_to_string(project.path().join("ratchet.baseline.json"))
        .expect("the baseline is readable");

    let output = run(&project, &["report", "--format", "md"]);

    assert!(output.status.success(), "{}", stdout(&output));
    let markdown = stdout(&output);
    assert!(markdown.contains("| 3 | 5 | ok |"), "{markdown}");
    assert_eq!(
        fs::read_to_string(project.path().join("ratchet.baseline.json"))
            .expect("the baseline is readable"),
        before,
        "a report renders a run; it never writes the baseline"
    );
}

#[test]
fn a_red_run_shows_the_recorded_baseline_of_a_passing_rule() {
    let config = r#"{"rules":{
      "r":{"class":"growth","incident":"test","unit":"lines","direction":"ceiling","aggregate":"max","count":{"file_metrics":{"files":["src/a.txt"],"metric":"line_count"}}},
      "bad":{"class":"growth","incident":"test","unit":"lines","direction":"ceiling","aggregate":"max","count":{"file_metrics":{"files":["src/bad.txt"],"metric":"line_count"}}}
    }}"#;
    let baseline = r#"{"version":1,"rules":{"r":{"value":5},"bad":{"value":1}}}"#;
    let project = project(config, Some(baseline));
    lines(&project, 3);
    let source = project.path().join("src");
    fs::write(source.join("bad.txt"), "x\nx\nx\n").expect("the failing file is written");
    let before = fs::read_to_string(project.path().join("ratchet.baseline.json"))
        .expect("the baseline is readable");

    let output = run(&project, &["check"]);

    assert_eq!(output.status.code(), Some(1));
    let text = stdout(&output);
    assert!(text.contains("3 ≤ 5"), "{text}");
    assert!(!text.contains("3 ≤ 3"), "{text}");
    assert_eq!(
        fs::read_to_string(project.path().join("ratchet.baseline.json"))
            .expect("the baseline is readable"),
        before,
        "a red run writes nothing"
    );
}

#[test]
fn count_does_not_write_a_baseline() {
    let project = project(&flat_config("ceiling", ""), Some(&flat_baseline(5)));
    lines(&project, 3);
    let before = fs::read_to_string(project.path().join("ratchet.baseline.json"))
        .expect("the baseline is readable");

    let output = run(&project, &["count"]);

    assert!(output.status.success(), "{}", stdout(&output));
    assert_eq!(
        fs::read_to_string(project.path().join("ratchet.baseline.json"))
            .expect("the baseline is readable"),
        before,
        "counting is not a gate and never writes"
    );
}
