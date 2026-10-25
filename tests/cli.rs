//! End to end tests: a temporary git repository, the real binary.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use common::TempDir;
use serde_json::Value;

/// The command that runs the binary.
///
/// Every invocation of the binary goes through here: the update check needs
/// the network, and the tests do not — the variable keeps the tests
/// hermetic and keeps the cache of the machine that runs them untouched.
fn binary() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ratchet"));
    command.env("RATCHET_NO_UPDATE_CHECK", "1");
    command
}

#[test]
fn scoped_file_metrics_initialize_check_report_and_raise_only_one_scope() {
    let config = r#"{
      "rules": {"metric": {
        "class": "file growth", "incident": "test", "unit": "bytes",
        "direction": "ceiling", "description": "All Java sources", "aggregate": "max",
        "count": {"file_metrics": {"files": ["**/*.java"], "metric": "size_bytes"}},
        "scopes": [{"id":"auth", "description":"Authentication module", "files":["src/auth/**"]}]
      }}
    }"#;
    let repo = Repo::plain(config, None);
    repo.write("src/auth/Login.java", "abc");
    repo.write("src/util/Other.java", "123456");
    let missing = repo.run(&["check", "--format", "json"]);
    assert_eq!(missing.status.code(), Some(1));
    let missing: Value = serde_json::from_slice(&missing.stdout).expect("missing-baseline report");
    assert_eq!(missing["summary"]["failed"], 2);
    let initialized = repo.run(&["baseline", "init", "--rule", "metric"]);
    assert!(
        initialized.status.success(),
        "{}",
        String::from_utf8_lossy(&initialized.stderr)
    );
    let baseline: Value =
        serde_json::from_str(&repo.read("ratchet.baseline.json")).expect("baseline JSON");
    assert_eq!(baseline["rules"]["metric"]["value"], 6);
    assert_eq!(baseline["rules"]["metric"]["scopes"]["auth"]["value"], 3);
    repo.git(&["init", "-q", "."]);
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "baseline"]);

    let checked = repo.run(&["check", "--format", "json"]);
    assert!(
        checked.status.success(),
        "{}",
        String::from_utf8_lossy(&checked.stdout)
    );
    let report: Value = serde_json::from_slice(&checked.stdout).expect("JSON report");
    assert_eq!(report["rules"].as_array().expect("rule results").len(), 2);
    assert!(
        report["rules"]
            .as_array()
            .expect("rule results")
            .iter()
            .any(
                |entry| entry["scope"] == "auth" && entry["description"] == "Authentication module"
            )
    );
    let repeated = repo.run(&["check", "--format", "json"]);
    assert_eq!(
        checked.stdout, repeated.stdout,
        "scope reports are deterministic"
    );

    let counts = repo.run(&["count", "--rule", "metric", "--format", "json"]);
    assert!(
        counts.status.success(),
        "{}",
        String::from_utf8_lossy(&counts.stderr)
    );
    let counts: Value = serde_json::from_slice(&counts.stdout).expect("count JSON");
    assert_eq!(counts["rules"].as_array().expect("count results").len(), 2);
    let markdown = repo.run(&["report"]);
    assert!(
        markdown.status.success(),
        "{}",
        String::from_utf8_lossy(&markdown.stderr)
    );
    assert!(String::from_utf8_lossy(&markdown.stdout).contains("Authentication module"));

    let raised = repo.run(&[
        "baseline",
        "raise",
        "--rule",
        "metric",
        "--scope",
        "auth",
        "--value",
        "4",
        "--reason",
        "planned growth",
    ]);
    assert!(
        raised.status.success(),
        "{}",
        String::from_utf8_lossy(&raised.stderr)
    );
    let after: Value =
        serde_json::from_str(&repo.read("ratchet.baseline.json")).expect("baseline JSON");
    assert_eq!(after["rules"]["metric"]["value"], 6);
    assert_eq!(after["rules"]["metric"]["scopes"]["auth"]["value"], 4);
    let raised_check = repo.run(&["check", "--format", "json"]);
    assert!(
        raised_check.status.success(),
        "{}",
        String::from_utf8_lossy(&raised_check.stdout)
    );

    repo.write("src/auth/Login.java", "ab");
    let updated = repo.run(&["baseline", "update", "--rule", "metric"]);
    assert!(
        updated.status.success(),
        "{}",
        String::from_utf8_lossy(&updated.stdout)
    );
    let after_update: Value =
        serde_json::from_str(&repo.read("ratchet.baseline.json")).expect("baseline JSON");
    assert_eq!(after_update["rules"]["metric"]["value"], 6);
    assert_eq!(
        after_update["rules"]["metric"]["scopes"]["auth"]["value"],
        2
    );
}

#[test]
fn command_counter_measurements_are_aggregated_and_paths_are_validated() {
    let valid = r#"{"rules":{"r":{"class":"coverage","incident":"test","unit":"files","direction":"floor","aggregate":"count","count":{"command":["sh","-c","printf '%s' '{\"measurements\":[{\"path\":\"src/A.java\",\"value\":10},{\"path\":\"src/B.java\",\"value\":20}]}'"]}}}}"#;
    let repo = Repo::plain(valid, None);
    repo.write("src/A.java", "a");
    repo.write("src/B.java", "b");
    let count = repo.run(&["count", "--format", "json"]);
    assert!(
        count.status.success(),
        "{}",
        String::from_utf8_lossy(&count.stderr)
    );
    let report: Value = serde_json::from_slice(&count.stdout).expect("count JSON");
    assert_eq!(report["rules"][0]["count"], 2);

    let unsafe_config = r#"{"rules":{"r":{"class":"coverage","incident":"test","unit":"files","direction":"floor","aggregate":"count","count":{"command":["sh","-c","printf '%s' '{\"measurements\":[{\"path\":\"outside.java\",\"value\":10}]}'"]}}}}"#;
    let unsafe_repo = Repo::plain(unsafe_config, None);
    let _outside = TempDir::new().expect("outside root");
    unsafe_repo.write("outside.java", "inside");
    #[cfg(unix)]
    {
        std::fs::remove_file(unsafe_repo.path().join("outside.java")).expect("remove inside file");
        std::os::unix::fs::symlink(
            _outside.path().join("secret.java"),
            unsafe_repo.path().join("outside.java"),
        )
        .expect("outside link");
        std::fs::write(_outside.path().join("secret.java"), "secret").expect("outside file");
        let output = unsafe_repo.run(&["count", "--format", "json"]);
        assert_eq!(output.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&output.stdout).contains("outside the repository root"));
    }
}

#[test]
fn scoped_baseline_can_start_without_a_global_fallback_and_initialize_it_later() {
    let config = r#"{"rules":{"m":{"class":"growth","incident":"test","unit":"bytes","direction":"ceiling","aggregate":"max","count":{"file_metrics":{"files":["**/*.java"],"metric":"size_bytes"}},"scopes":[{"id":"auth","description":"Auth","files":["src/auth/**"]}]}}}"#;
    let repo = Repo::plain(config, None);
    repo.write("src/auth/Login.java", "abc");
    let first = repo.run(&["baseline", "init"]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stdout)
    );
    let initial: Value =
        serde_json::from_str(&repo.read("ratchet.baseline.json")).expect("baseline");
    assert!(initial["rules"]["m"].get("value").is_none());
    assert_eq!(initial["rules"]["m"]["scopes"]["auth"]["value"], 3);

    repo.write("src/Other.java", "12345");
    let second = repo.run(&["baseline", "init"]);
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stdout)
    );
    let updated: Value =
        serde_json::from_str(&repo.read("ratchet.baseline.json")).expect("baseline");
    assert_eq!(updated["rules"]["m"]["value"], 5);
    assert_eq!(updated["rules"]["m"]["scopes"]["auth"]["value"], 3);
}

/// A temporary git repository with a config and a baseline.
struct Repo {
    dir: TempDir,
}

impl Repo {
    /// Creates a repository with a config and a baseline, both committed.
    fn new(config: &str, baseline: Option<&str>) -> Self {
        let repo = Self::plain(config, baseline);
        repo.git(&["init", "-q", "."]);
        repo.git(&["add", "-A"]);
        repo.git(&["commit", "-q", "-m", "init"]);
        repo
    }

    /// Creates a directory with a config and a baseline, but no repository.
    fn plain(config: &str, baseline: Option<&str>) -> Self {
        let dir = TempDir::new().expect("a temporary directory");
        let repo = Self { dir };
        repo.write("ratchet.config.jsonc", config);
        if let Some(baseline) = baseline {
            repo.write("ratchet.baseline.json", baseline);
        }
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

    /// Runs the binary in the repository.
    fn run(&self, arguments: &[&str]) -> Output {
        binary()
            .args(arguments)
            .current_dir(self.path())
            .output()
            .expect("the binary runs")
    }

    /// Runs the binary without a reachable git repository.
    fn run_without_git(&self, arguments: &[&str]) -> Output {
        let ceiling = self.path().parent().expect("a parent directory");
        binary()
            .args(arguments)
            .current_dir(self.path())
            .env("GIT_CEILING_DIRECTORIES", ceiling)
            .output()
            .expect("the binary runs")
    }
}

/// True when `node` is on the PATH.
fn node_available() -> bool {
    Command::new("node")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// A directory outside the repository root.
///
/// The directory is removed again when it goes out of scope: a temporary is
/// removed, and a test that fails must not leave anything behind.  The name
/// carries the process id: a run that lets the engine write into it — a
/// defect, a mutation — must not make the next run fail.
struct Outside {
    path: PathBuf,
}

impl Outside {
    /// Creates the directory.
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("ratchet-outside-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("the directory is created");
        Self { path }
    }

    /// The path of the directory.
    fn path(&self) -> &Path {
        &self.path
    }

    /// The path of a file in the directory.
    fn file(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for Outside {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// A chain of two links: `name` → `middle` → `target`.
///
/// The chain is what carries a path out of the root: `canonicalize` follows a
/// chain whose final target **exists**, but a chain that ends at a missing
/// file has to be followed by hand — and that is the shape that makes a
/// *write* succeed outside the repository.
#[cfg(unix)]
fn link_chain(name: &Path, middle: &Path, target: &Path) {
    let directory = middle.parent().expect("a parent directory");
    std::fs::create_dir_all(directory).expect("the directory is created");
    std::os::unix::fs::symlink(target, middle).expect("the second link is made");
    std::os::unix::fs::symlink(middle, name).expect("the first link is made");
}

/// The stdout of a run.
fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The stderr of a run.
fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The code of a run.
fn code(output: &Output) -> i32 {
    output.status.code().expect("the process exited normally")
}

/// A config with one ceiling rule that counts three.
fn ceiling_config() -> &'static str {
    r#"[
      {
        "name": "test",
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
    ]"#
}

/// A config with one floor rule that counts three.
fn floor_config() -> &'static str {
    r#"[
      {
        "rules": {
          "tests": {
            "class": "coverage",
            "incident": "a test was deleted",
            "unit": "tests",
            "direction": "floor",
            "count": { "command": ["echo", "3"] }
          }
        }
      }
    ]"#
}

/// A config whose counter always fails.
fn counter_error_config() -> &'static str {
    r#"[
      {
        "rules": {
          "a": {
            "class": "layer violation",
            "incident": "the counter crashed",
            "unit": "call sites",
            "direction": "ceiling",
            "tier": "slow",
            "count": { "command": ["false"] }
          }
        }
      }
    ]"#
}

/// A baseline with one record.
fn baseline(rule: &str, value: i64) -> String {
    format!(
        "{{\n  \"version\": 1,\n  \"rules\": {{\n    \"{rule}\": {{ \"value\": {value} }}\n  }}\n}}\n"
    )
}

#[test]
fn a_green_run_exits_zero() {
    let repo = Repo::new(ceiling_config(), Some(&baseline("todo-markers", 5)));

    let output = repo.run(&["check"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("PASS"), "{text}");
    assert!(text.contains("COHERENT"), "{text}");
}

#[test]
fn a_count_above_the_baseline_fails() {
    let repo = Repo::new(ceiling_config(), Some(&baseline("todo-markers", 2)));

    let output = repo.run(&["check"]);

    assert_eq!(code(&output), 1, "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("FAIL"), "{text}");
    assert!(text.contains("3 > 2"), "{text}");
}

#[test]
fn a_floor_rule_fails_below_its_baseline() {
    let repo = Repo::new(floor_config(), Some(&baseline("tests", 5)));

    let output = repo.run(&["check"]);

    assert_eq!(code(&output), 1, "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("3 < 5"), "{text}");
}

#[test]
fn a_floor_rule_passes_at_its_baseline() {
    let repo = Repo::new(floor_config(), Some(&baseline("tests", 3)));

    let output = repo.run(&["check"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
}

#[test]
fn a_rule_without_a_baseline_fails() {
    let repo = Repo::new(ceiling_config(), None);

    let output = repo.run(&["check"]);

    assert_eq!(code(&output), 1, "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("missing-baseline"), "{text}");
    assert!(text.contains("no baseline for todo-markers"), "{text}");
}

#[test]
fn a_raised_baseline_fails_against_the_committed_one() {
    let repo = Repo::new(ceiling_config(), Some(&baseline("todo-markers", 5)));
    // The working tree raises the baseline, HEAD still has 5.
    repo.write("ratchet.baseline.json", &baseline("todo-markers", 7));

    let output = repo.run(&["check"]);

    assert_eq!(code(&output), 1, "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("baseline 5 raised to 7"), "{text}");
}

#[test]
fn a_lowered_floor_baseline_fails_against_the_committed_one() {
    let repo = Repo::new(floor_config(), Some(&baseline("tests", 5)));
    repo.write("ratchet.baseline.json", &baseline("tests", 3));

    let output = repo.run(&["check"]);

    assert_eq!(code(&output), 1, "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("baseline 5 lowered to 3"), "{text}");
}

#[test]
fn baseline_update_refuses_the_forbidden_direction() {
    let repo = Repo::new(ceiling_config(), Some(&baseline("todo-markers", 2)));

    let output = repo.run(&["baseline", "update"]);

    assert_eq!(code(&output), 1, "{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.contains("refusing: count 3 > baseline 2 — a ceiling may only decrease"),
        "{text}"
    );
    // The refused rule is not modified.
    assert!(repo.read("ratchet.baseline.json").contains("\"value\": 2"));
}

#[test]
fn baseline_update_accepts_the_allowed_direction() {
    let repo = Repo::new(ceiling_config(), Some(&baseline("todo-markers", 5)));

    let output = repo.run(&["baseline", "update", "--reason", "paid down"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let baseline = repo.read("ratchet.baseline.json");
    assert!(baseline.contains("\"value\": 3"), "{baseline}");
    assert!(baseline.contains("paid down"), "{baseline}");
}

#[test]
fn baseline_update_refuses_a_floor_rule_that_fell() {
    let repo = Repo::new(floor_config(), Some(&baseline("tests", 5)));

    let output = repo.run(&["baseline", "update"]);

    assert_eq!(code(&output), 1, "{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.contains("refusing: count 3 < baseline 5 — a floor may only increase"),
        "{text}"
    );
}

#[test]
fn baseline_update_moves_a_floor_rule_up() {
    let repo = Repo::new(floor_config(), Some(&baseline("tests", 3)));

    let output = repo.run(&["baseline", "update"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert!(repo.read("ratchet.baseline.json").contains("\"value\": 3"));
}

#[test]
fn baseline_init_writes_the_current_count() {
    let repo = Repo::new(ceiling_config(), None);

    let output = repo.run(&["baseline", "init", "--reason", "first measurement"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let baseline = repo.read("ratchet.baseline.json");
    assert!(baseline.contains("\"value\": 3"), "{baseline}");
    assert!(baseline.contains("first measurement"), "{baseline}");
}

#[test]
fn baseline_init_keeps_an_existing_baseline() {
    let repo = Repo::new(ceiling_config(), Some(&baseline("todo-markers", 5)));

    let output = repo.run(&["baseline", "init"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert!(repo.read("ratchet.baseline.json").contains("\"value\": 5"));
}

#[test]
fn the_json_format_prints_only_json() {
    let repo = Repo::new(ceiling_config(), Some(&baseline("todo-markers", 5)));

    let output = repo.run(&["check", "--format", "json"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let text = stdout(&output);
    let report: Value = serde_json::from_str(&text).expect("the stdout is a JSON object");
    assert_eq!(report["schema"], 1);
    assert_eq!(report["ok"], true);
    assert_eq!(report["summary"]["rules"], 1);
    assert_eq!(report["summary"]["failed"], 0);
    assert_eq!(report["summary"]["executed"], 1);
    assert_eq!(report["summary"]["generated"], 0);
    assert_eq!(report["summary"]["drifted"], 0);
    assert_eq!(report["rules"][0]["id"], "todo-markers");
    assert_eq!(report["rules"][0]["status"], "ok");
    assert_eq!(report["rules"][0]["count"], 3);
    assert_eq!(report["rules"][0]["baseline"], 5);
    assert_eq!(report["rules"][0]["direction"], "ceiling");
    assert_eq!(report["rules"][0]["unit"], "markers");
    assert_eq!(report["rules"][0]["class"], "unfinished work");
    assert_eq!(report["rules"][0]["tier"], "fast");
    assert_eq!(report["generated"].as_array().expect("an array").len(), 0);
}

#[test]
fn the_json_format_reports_a_failed_gate() {
    let repo = Repo::new(ceiling_config(), Some(&baseline("todo-markers", 2)));

    let output = repo.run(&["check", "--format", "json"]);

    assert_eq!(code(&output), 1, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    assert_eq!(report["ok"], false);
    assert_eq!(report["summary"]["failed"], 1);
    assert_eq!(report["rules"][0]["status"], "over-baseline");
}

#[test]
fn an_invalid_config_exits_two_and_names_the_path() {
    let repo = Repo::new(
        r#"[
          {
            "rules": {
              "a": {
                "class": "c",
                "incident": "i",
                "unit": "u",
                "direction": "sideways",
                "count": { "command": ["echo", "1"] }
              }
            }
          }
        ]"#,
        None,
    );

    let output = repo.run(&["check"]);

    assert_eq!(code(&output), 2);
    let text = stderr(&output);
    assert!(text.contains("rules.a.direction"), "{text}");
}

#[test]
fn a_missing_config_exits_two() {
    let repo = Repo::new(ceiling_config(), None);
    std::fs::remove_file(repo.path().join("ratchet.config.jsonc")).expect("the config is removed");

    let output = repo.run(&["check"]);

    assert_eq!(code(&output), 2);
    assert!(stderr(&output).contains("no config found"));
}

#[test]
fn a_crashed_counter_exits_two() {
    let repo = Repo::new(
        r#"[
          {
            "rules": {
              "a": {
                "class": "c",
                "incident": "i",
                "unit": "u",
                "direction": "ceiling",
                "count": { "command": ["false"] }
              }
            }
          }
        ]"#,
        Some(&baseline("a", 1)),
    );

    let output = repo.run(&["check", "--format", "json"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    assert_eq!(report["rules"][0]["status"], "counter-error");
}

#[test]
fn the_paths_scope_the_run() {
    let config = r#"[
      {
        "rules": {
          "scoped": {
            "class": "layer violation",
            "incident": "a call site was wrapped",
            "unit": "call sites",
            "direction": "ceiling",
            "count": { "command": ["echo", "1"] },
            "files": ["src/**"]
          }
        }
      }
    ]"#;
    let repo = Repo::new(config, Some(&baseline("scoped", 3)));
    repo.write("docs/readme.md", "hello");
    repo.write("src/main.rs", "fn main() {}");

    let output = repo.run(&["check", "docs/readme.md"]);

    // The rule is out of scope, nothing is executed: an empty selection fails.
    assert_eq!(code(&output), 1, "{}", stderr(&output));
    assert!(!stdout(&output).contains("PASS"), "{}", stdout(&output));

    let output = repo.run(&["check", "src/main.rs"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert!(stdout(&output).contains("PASS"), "{}", stdout(&output));
}

#[test]
fn the_count_command_prints_the_current_count() {
    let repo = Repo::new(ceiling_config(), Some(&baseline("todo-markers", 5)));

    let output = repo.run(&["count"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("3"), "{text}");
    assert!(text.contains("markers"), "{text}");
}

#[test]
fn the_default_command_is_check() {
    let repo = Repo::new(ceiling_config(), Some(&baseline("todo-markers", 5)));

    let output = repo.run(&[]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert!(stdout(&output).contains("PASS"));
}

#[test]
fn the_config_is_found_in_a_parent_directory() {
    let repo = Repo::new(ceiling_config(), Some(&baseline("todo-markers", 5)));
    let nested = repo.path().join("nested/deeper");
    std::fs::create_dir_all(&nested).expect("the directory is created");

    let output = binary()
        .arg("check")
        .current_dir(&nested)
        .output()
        .expect("the binary runs");

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
}

#[test]
fn init_writes_a_starter_config_and_never_overwrites() {
    let repo = Repo::new(ceiling_config(), None);
    std::fs::remove_file(repo.path().join("ratchet.config.jsonc")).expect("the config is removed");

    let output = repo.run(&["init"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let config = repo.read("ratchet.config.jsonc");
    assert!(config.contains("\"rules\""), "{config}");
    assert!(config.contains("\"direction\": \"ceiling\""), "{config}");

    // A second run does not overwrite the config.
    repo.write("ratchet.config.jsonc", "// mine\n");
    let output = repo.run(&["init"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert_eq!(repo.read("ratchet.config.jsonc"), "// mine\n");
}

#[test]
fn the_starter_config_is_valid() {
    let repo = Repo::new(ceiling_config(), None);
    std::fs::remove_file(repo.path().join("ratchet.config.jsonc")).expect("the config is removed");
    repo.run(&["init"]);

    let output = repo.run(&["check"]);

    // The starter config counts TODO markers; there are none in the repository.
    assert_eq!(code(&output), 1, "{}", stderr(&output));
    assert!(
        stdout(&output).contains("missing-baseline"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn init_writes_the_starter_config_in_a_fresh_directory() {
    // A fresh directory has no config yet: the path is inside it, and the
    // command writes the starter config.
    let repo = Repo::plain(ceiling_config(), None);
    std::fs::remove_file(repo.path().join("ratchet.config.jsonc")).expect("the config is removed");

    let output = repo.run(&["init"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert!(
        repo.read("ratchet.config.jsonc").contains("\"rules\""),
        "the starter config was not written"
    );
}

#[cfg(unix)]
#[test]
fn init_through_a_link_outside_the_repository_root_is_refused() {
    // The config is a path the project names: a link at that position that
    // leads out of the tree would take the starter config with it.
    let repo = Repo::new(ceiling_config(), None);
    std::fs::remove_file(repo.path().join("ratchet.config.jsonc")).expect("the config is removed");
    let outside = Outside::new("init");
    std::os::unix::fs::symlink(
        outside.file("ratchet.config.jsonc"),
        repo.path().join("ratchet.config.jsonc"),
    )
    .expect("the link is made");

    let output = repo.run(&["init"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let text = stderr(&output);
    assert!(
        text.contains("the config file ratchet.config.jsonc"),
        "{text}"
    );
    assert!(
        text.contains("must lie inside the repository root"),
        "{text}"
    );
    // And the starter config was not written outside the root.
    assert!(
        !outside.file("ratchet.config.jsonc").exists(),
        "the starter config was written outside the repository root"
    );
}

#[cfg(unix)]
#[test]
fn init_behind_a_chain_of_links_is_refused() {
    // The config is a chain: `ratchet.config.jsonc` → `real/config.jsonc` →
    // a file outside the root that is not there.
    let repo = Repo::new(ceiling_config(), None);
    std::fs::remove_file(repo.path().join("ratchet.config.jsonc")).expect("the config is removed");
    let outside = Outside::new("init-chain");
    link_chain(
        &repo.path().join("ratchet.config.jsonc"),
        &repo.path().join("real/config.jsonc"),
        &outside.file("shared-config.jsonc"),
    );

    let output = repo.run(&["init"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let text = stderr(&output);
    assert!(
        text.contains("the config file ratchet.config.jsonc"),
        "{text}"
    );
    assert!(
        text.contains("must lie inside the repository root"),
        "{text}"
    );
    // And the starter config was not written outside the root.
    assert!(
        !outside.file("shared-config.jsonc").exists(),
        "the starter config was written outside the repository root"
    );
}

#[test]
fn a_missing_baseline_file_is_an_empty_baseline() {
    let repo = Repo::new(ceiling_config(), None);

    let output = repo.run(&["check", "--format", "json"]);

    assert_eq!(code(&output), 1, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    assert_eq!(report["rules"][0]["status"], "missing-baseline");
    assert_eq!(report["rules"][0]["baseline"], Value::Null);
}

#[test]
fn a_file_counter_reads_a_json_document() {
    let repo = Repo::new(
        r#"[
          {
            "rules": {
              "a": {
                "class": "c",
                "incident": "i",
                "unit": "files",
                "direction": "ceiling",
                "count": { "file": "audit.json", "path": "coherence.innerHtml" }
              }
            }
          }
        ]"#,
        Some(&baseline("a", 1)),
    );
    repo.write(
        "audit.json",
        r#"{"coherence": {"innerHtml": ["src/a.ts", "src/b.ts"]}}"#,
    );

    let output = repo.run(&["check", "--format", "json"]);

    assert_eq!(code(&output), 1, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    assert_eq!(report["rules"][0]["count"], 2);
}

#[test]
fn a_file_counter_outside_the_repository_root_is_refused() {
    // The artifact is outside the root — and it is there: a counter that
    // reads it would count a file the repository does not own.
    let outside = Outside::new("file");
    std::fs::write(outside.file("audit.json"), r#"{"count": 1}"#).expect("the file is written");
    let relative = format!(
        "../{}/audit.json",
        outside
            .path()
            .file_name()
            .expect("a file name")
            .to_string_lossy()
    );
    let repo = Repo::new(
        &format!(
            r#"[
          {{
            "rules": {{
              "a": {{
                "class": "c",
                "incident": "i",
                "unit": "files",
                "direction": "ceiling",
                "count": {{ "file": "{relative}" }}
              }}
            }}
          }}
        ]"#
        ),
        Some(&baseline("a", 1)),
    );

    let output = repo.run(&["check", "--format", "json"]);

    // A counter that reads a file outside the root is an operational error.
    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    let detail = report["rules"][0]["detail"].as_str().unwrap_or_default();
    // The refusal is a **configuration** error, and it names the configured
    // path: the user has to see which path of the config was refused.
    assert!(
        detail.starts_with("config error at rules.a.count.file:"),
        "{detail}"
    );
    assert!(detail.contains(&relative), "{detail}");
    assert!(
        detail.contains("is outside the repository root"),
        "{detail}"
    );
}

#[cfg(unix)]
#[test]
fn a_file_counter_through_a_linked_directory_outside_the_root_is_refused() {
    let repo = Repo::new(
        r#"[
          {
            "rules": {
              "a": {
                "class": "c",
                "incident": "i",
                "unit": "files",
                "direction": "ceiling",
                "count": { "file": "docs/audit.json" }
              }
            }
          }
        ]"#,
        Some(&baseline("a", 1)),
    );
    // The directory inside the root is a link that points out of it: the file
    // behind the link is a file the repository does not own.
    let outside = Outside::new("file-link");
    std::fs::write(outside.file("audit.json"), r#"{"count": 1}"#).expect("the file is written");
    std::os::unix::fs::symlink(outside.path(), repo.path().join("docs")).expect("the link is made");

    let output = repo.run(&["check", "--format", "json"]);

    // The counter does not read through the link.
    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    let detail = report["rules"][0]["detail"].as_str().unwrap_or_default();
    assert!(
        detail.starts_with("config error at rules.a.count.file:"),
        "{detail}"
    );
    assert!(detail.contains("docs/audit.json"), "{detail}");
    assert!(
        detail.contains("is outside the repository root"),
        "{detail}"
    );
}

#[cfg(unix)]
#[test]
fn a_file_counter_that_is_a_link_to_a_file_outside_the_root_is_refused() {
    let repo = Repo::new(
        r#"[
          {
            "rules": {
              "a": {
                "class": "c",
                "incident": "i",
                "unit": "files",
                "direction": "ceiling",
                "count": { "file": "audit.json" }
              }
            }
          }
        ]"#,
        Some(&baseline("a", 1)),
    );
    // The artifact itself is a link to a file outside the root: the counter
    // would read a file the repository does not own.
    let outside = Outside::new("file-target");
    std::fs::write(outside.file("audit.json"), r#"{"count": 7}"#).expect("the file is written");
    std::os::unix::fs::symlink(outside.file("audit.json"), repo.path().join("audit.json"))
        .expect("the link is made");

    let output = repo.run(&["check", "--format", "json"]);

    // The counter does not read through the link.
    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    let detail = report["rules"][0]["detail"].as_str().unwrap_or_default();
    assert!(
        detail.starts_with("config error at rules.a.count.file:"),
        "{detail}"
    );
    assert!(detail.contains("audit.json"), "{detail}");
    assert!(
        detail.contains("is outside the repository root"),
        "{detail}"
    );
}

#[cfg(unix)]
#[test]
fn a_file_counter_through_a_link_and_a_parent_is_refused() {
    // "link/../audit.json" is ambiguous: the operating system reads it as
    // the parent of the link's **target**, a reader of the config as the
    // parent of the named directory.  The tool does not choose one.
    let repo = Repo::new(
        r#"[
          {
            "rules": {
              "a": {
                "class": "c",
                "incident": "i",
                "unit": "files",
                "direction": "ceiling",
                "count": { "file": "link/../audit.json" }
              }
            }
          }
        ]"#,
        Some(&baseline("a", 1)),
    );
    let outside = Outside::new("parent-file");
    std::fs::write(outside.file("audit.json"), r#"{"count": 42}"#).expect("the file is written");
    std::os::unix::fs::symlink(outside.path(), repo.path().join("link")).expect("the link is made");

    let output = repo.run(&["check", "--format", "json"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    let detail = report["rules"][0]["detail"].as_str().unwrap_or_default();
    assert!(
        detail.starts_with("config error at rules.a.count.file:"),
        "{detail}"
    );
    assert!(detail.contains("is ambiguous"), "{detail}");
    // And the file outside the root was not read.
    assert!(report["rules"][0]["count"].is_null(), "{report}");
}

#[test]
fn a_file_counter_with_an_absolute_path_is_refused() {
    let repo = Repo::new(
        r#"[
          {
            "rules": {
              "a": {
                "class": "c",
                "incident": "i",
                "unit": "files",
                "direction": "ceiling",
                "count": { "file": "/etc/audit.json" }
              }
            }
          }
        ]"#,
        Some(&baseline("a", 1)),
    );

    let output = repo.run(&["check", "--format", "json"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    let detail = report["rules"][0]["detail"].as_str().unwrap_or_default();
    assert!(
        detail.starts_with("config error at rules.a.count.file:"),
        "{detail}"
    );
    assert!(detail.contains("has to be a relative path"), "{detail}");
}

#[test]
fn a_file_counter_with_a_parent_that_follows_no_link_still_resolves() {
    // The carve-out stays: a ".." that follows no link is not ambiguous, and
    // the path resolves as it always did.
    let repo = Repo::new(
        r#"[
          {
            "rules": {
              "a": {
                "class": "c",
                "incident": "i",
                "unit": "files",
                "direction": "ceiling",
                "count": { "file": "docs/../audit.json" }
              }
            }
          }
        ]"#,
        Some(&baseline("a", 1)),
    );
    repo.write("docs/keep.md", "keep\n");
    repo.write("audit.json", r#"{"count": 1}"#);

    let output = repo.run(&["check", "--format", "json"]);

    // The counter reads the file the path names.
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    assert_eq!(report["rules"][0]["count"], 1);
}

#[cfg(unix)]
#[test]
fn a_file_counter_behind_a_chain_longer_than_the_bound_is_refused() {
    // Forty links: the operating system refuses to follow more than 32, and
    // the tool refuses the path with it.
    let repo = Repo::new(
        r#"[
          {
            "rules": {
              "a": {
                "class": "c",
                "incident": "i",
                "unit": "files",
                "direction": "ceiling",
                "count": { "file": "audit.json" }
              }
            }
          }
        ]"#,
        Some(&baseline("a", 1)),
    );
    let outside = Outside::new("long-chain");
    let mut previous = outside.file("shared-audit.json");
    for index in (1..=40).rev() {
        let link = repo.path().join(format!("l{index}"));
        std::os::unix::fs::symlink(&previous, &link).expect("the link is made");
        previous = link;
    }
    std::os::unix::fs::symlink(&previous, repo.path().join("audit.json"))
        .expect("the link is made");

    let output = repo.run(&["check", "--format", "json"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    let detail = report["rules"][0]["detail"].as_str().unwrap_or_default();
    assert!(detail.contains("could not be resolved"), "{detail}");
    // And the file outside the root was not read.
    assert!(report["rules"][0]["count"].is_null(), "{report}");
}

#[cfg(unix)]
#[test]
fn a_file_counter_that_is_a_link_to_a_file_inside_the_root_is_read() {
    // The carve-out stays: the file may be a link as long as its target
    // lies inside the root.
    let repo = Repo::new(
        r#"[
          {
            "rules": {
              "a": {
                "class": "c",
                "incident": "i",
                "unit": "files",
                "direction": "ceiling",
                "count": { "file": "audit.json" }
              }
            }
          }
        ]"#,
        Some(&baseline("a", 1)),
    );
    repo.write("real.json", r#"{"count": 1}"#);
    std::os::unix::fs::symlink("real.json", repo.path().join("audit.json"))
        .expect("the link is made");

    let output = repo.run(&["check", "--format", "json"]);

    // The counter reads the file behind the link.
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    assert_eq!(report["rules"][0]["count"], 1);
}

#[cfg(unix)]
#[test]
fn a_file_counter_through_a_chain_of_links_outside_the_root_is_refused() {
    let repo = Repo::new(
        r#"[
          {
            "rules": {
              "a": {
                "class": "c",
                "incident": "i",
                "unit": "files",
                "direction": "ceiling",
                "count": { "file": "audit.json" }
              }
            }
          }
        ]"#,
        Some(&baseline("a", 1)),
    );
    // The artifact is a chain: `audit.json` → `real/audit.json` → a file
    // outside the root that is not there.
    let outside = Outside::new("file-chain");
    link_chain(
        &repo.path().join("audit.json"),
        &repo.path().join("real/audit.json"),
        &outside.file("shared-audit.json"),
    );

    let output = repo.run(&["check", "--format", "json"]);

    // The counter does not read through the chain.
    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    let detail = report["rules"][0]["detail"].as_str().unwrap_or_default();
    assert!(
        detail.starts_with("config error at rules.a.count.file:"),
        "{detail}"
    );
    assert!(detail.contains("audit.json"), "{detail}");
    assert!(
        detail.contains("is outside the repository root"),
        "{detail}"
    );
}

#[test]
fn a_file_counter_with_a_generator_and_a_path_outside_the_root_is_refused() {
    // The rule is about the path the project **names**, not about what the
    // run reads from it: with `generate` the count comes from a temporary
    // path, and the configured path is still the project's.
    let repo = Repo::new(
        r#"[
          {
            "rules": {
              "a": {
                "class": "c",
                "incident": "i",
                "unit": "files",
                "direction": "ceiling",
                "count": {
                  "file": "../outside/audit.json",
                  "generate": ["sh", "generate.sh"],
                  "output_arg": "--output"
                }
              }
            }
          }
        ]"#,
        Some(&baseline("a", 1)),
    );
    repo.write(
        "generate.sh",
        "#!/bin/sh\nprintf '{\"count\": 1}' > \"$2\"\n",
    );

    let output = repo.run(&["check", "--format", "json"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    let detail = report["rules"][0]["detail"].as_str().unwrap_or_default();
    assert!(
        detail.starts_with("config error at rules.a.count.file:"),
        "{detail}"
    );
    assert!(detail.contains("../outside/audit.json"), "{detail}");
    assert!(
        detail.contains("is outside the repository root"),
        "{detail}"
    );
}

#[test]
fn a_file_counter_with_a_generator_and_a_path_inside_the_root_counts() {
    // The carve-out stays: a path inside the root is counted, generator or
    // not.
    let repo = Repo::new(
        r#"[
          {
            "rules": {
              "a": {
                "class": "c",
                "incident": "i",
                "unit": "files",
                "direction": "ceiling",
                "count": {
                  "file": "docs/audit.json",
                  "generate": ["sh", "generate.sh"],
                  "output_arg": "--output"
                }
              }
            }
          }
        ]"#,
        Some(&baseline("a", 1)),
    );
    repo.write(
        "generate.sh",
        "#!/bin/sh\nprintf '{\"count\": 1}' > \"$2\"\n",
    );

    let output = repo.run(&["check", "--format", "json"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    assert_eq!(report["rules"][0]["count"], 1);
}

#[test]
fn the_exclude_patterns_remove_items_from_the_count() {
    let repo = Repo::new(
        r#"[
          {
            "rules": {
              "a": {
                "class": "c",
                "incident": "i",
                "unit": "files",
                "direction": "ceiling",
                "count": { "file": "audit.json" },
                "exclude": ["src/**"]
              }
            }
          }
        ]"#,
        Some(&baseline("a", 1)),
    );
    repo.write("audit.json", r#"["src/a.ts", "src/b.ts", "docs/c.ts"]"#);

    let output = repo.run(&["check", "--format", "json"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    assert_eq!(report["rules"][0]["count"], 1);
}

#[test]
fn the_quiet_flag_prints_only_failures() {
    let repo = Repo::new(ceiling_config(), Some(&baseline("todo-markers", 2)));

    let output = repo.run(&["check", "--quiet"]);

    assert_eq!(code(&output), 1, "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("FAIL"), "{text}");
    assert!(!text.contains("PASS"), "{text}");
    assert!(!text.contains("ratchet —"), "{text}");
}

#[test]
fn an_unknown_rule_exits_two() {
    let repo = Repo::new(ceiling_config(), Some(&baseline("todo-markers", 5)));

    let output = repo.run(&["check", "--rule", "nope"]);

    assert_eq!(code(&output), 2);
    assert!(stderr(&output).contains("unknown rule: nope"));
}

#[test]
fn a_rule_filter_restricts_the_run() {
    let config = r#"[
      {
        "rules": {
          "a": {
            "class": "c",
            "incident": "i",
            "unit": "u",
            "direction": "ceiling",
            "count": { "command": ["echo", "1"] }
          },
          "b": {
            "class": "c",
            "incident": "i",
            "unit": "u",
            "direction": "ceiling",
            "count": { "command": ["echo", "2"] }
          }
        }
      }
    ]"#;
    let repo = Repo::new(
        config,
        Some(
            "{\n  \"version\": 1,\n  \"rules\": {\n    \"a\": { \"value\": 1 },\n    \"b\": { \"value\": 1 }\n  }\n}\n",
        ),
    );

    let output = repo.run(&["check", "--rule", "a", "--format", "json"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    assert_eq!(report["summary"]["rules"], 1);
    assert_eq!(report["rules"][0]["id"], "a");
    assert_eq!(report["rules"][0]["status"], "ok");
}

// --- the fixes of the verification round -------------------------------------

#[test]
fn without_git_the_committed_comparison_is_skipped_with_a_note() {
    let repo = Repo::plain(ceiling_config(), Some(&baseline("todo-markers", 5)));

    let output = repo.run_without_git(&["check"]);

    // The gate still runs: the count is compared with the baseline.
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let text = stderr(&output);
    assert!(
        text.contains("monotonicity skipped: not a git repository"),
        "{text}"
    );
    assert!(stdout(&output).contains("PASS"), "{}", stdout(&output));
}

#[test]
fn the_arguments_of_a_counter_arrive_verbatim() {
    let repo = Repo::new(ceiling_config(), Some(&baseline("todo-markers", 5)));
    // Use a native process rather than a POSIX shell: this checks that ratchet
    // preserves argv on Windows as well as Unix.
    repo.write(
        "argv.js",
        "const fs = require('node:fs');\nconst args = process.argv.slice(2);\nconst output = args.shift();\nfs.writeFileSync(output, Buffer.from(args.join('\\0') + '\\0'));\nprocess.stdout.write(String(args.length));\n",
    );
    let outside = TempDir::new().expect("a temporary directory");
    let output_file = outside.path().join("argv.bin");

    let arguments = vec![
        "a b".to_string(),
        "c\"d".to_string(),
        "*".to_string(),
        ";".to_string(),
        "|".to_string(),
        "$(echo x)".to_string(),
        "line1\nline2".to_string(),
    ];
    let mut command = vec!["node".to_string(), "argv.js".to_string()];
    command.push(output_file.to_string_lossy().into_owned());
    command.extend(arguments.iter().cloned());

    let config = serde_json::json!([
        {
            "rules": {
                "argv": {
                    "class": "c",
                    "incident": "i",
                    "unit": "u",
                    "direction": "ceiling",
                    "count": { "command": command }
                }
            }
        }
    ]);
    repo.write("ratchet.config.jsonc", &config.to_string());
    repo.write(
        "ratchet.baseline.json",
        &baseline("argv", arguments.len() as i64),
    );
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "argv"]);

    let output = repo.run(&["check", "--format", "json"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    // Every argument arrived: none was split, expanded or substituted.
    assert_eq!(report["rules"][0]["count"], arguments.len());
    let received = std::fs::read(&output_file).expect("the file is readable");
    let mut expected = Vec::new();
    for argument in &arguments {
        expected.extend_from_slice(argument.as_bytes());
        expected.push(0);
    }
    assert_eq!(received, expected);
}

#[test]
fn the_engine_does_not_mutate_the_tree() {
    let repo = Repo::new(ceiling_config(), Some(&baseline("todo-markers", 5)));

    let output = repo.run(&["check"]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert_eq!(repo.status(), "", "check changed the working tree");

    let output = repo.run(&["count"]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert_eq!(repo.status(), "", "count changed the working tree");
}

#[test]
fn the_baseline_commands_only_touch_the_baseline_file() {
    let repo = Repo::new(ceiling_config(), None);

    let output = repo.run(&["baseline", "init"]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert_eq!(repo.status(), "?? ratchet.baseline.json\n");

    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "baseline"]);

    let output = repo.run(&["baseline", "update", "--reason", "paid down"]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert_eq!(repo.status(), " M ratchet.baseline.json\n");
}

#[test]
fn a_counter_error_on_count_exits_two() {
    let repo = Repo::new(counter_error_config(), Some(&baseline("a", 1)));

    let output = repo.run(&["count"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    assert!(stdout(&output).contains("failed"), "{}", stdout(&output));
}

#[test]
fn a_counter_error_on_baseline_init_exits_two() {
    let repo = Repo::new(counter_error_config(), None);

    let output = repo.run(&["baseline", "init"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    assert!(
        stdout(&output).contains("the counter exited with status 1"),
        "{}",
        stdout(&output)
    );
    assert!(!repo.path().join("ratchet.baseline.json").exists());
}

#[test]
fn no_config_without_an_explicit_config_exits_two() {
    let repo = Repo::new(ceiling_config(), Some(&baseline("todo-markers", 5)));

    let output = repo.run(&["check", "--no-config"]);

    assert_eq!(code(&output), 2);
    assert!(stderr(&output).contains("--no-config requires --config"));
}

#[test]
fn the_javascript_config_is_used_end_to_end() {
    if !node_available() {
        eprintln!(
            "*** SKIPPED: node is not on the PATH — this JavaScript test did NOT run. \
             Install node (or run the CI, which installs it) to exercise it. ***"
        );
        return;
    }
    let repo = Repo::plain("", None);
    std::fs::remove_file(repo.path().join("ratchet.config.jsonc")).expect("the config is removed");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ratchet.config.mjs");
    std::fs::copy(fixture, repo.path().join("ratchet.config.mjs")).expect("the fixture is copied");
    repo.write(
        "ratchet.baseline.json",
        "{\n  \"version\": 1,\n  \"rules\": {\n    \"js-rule\": { \"value\": 1 }\n  }\n}\n",
    );

    let output = repo.run(&["check", "--format", "json"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    // The rule of the fixture proves that the JavaScript config was really
    // evaluated: there is no other config in the repository.
    assert_eq!(report["rules"][0]["id"], "js-rule");
    assert_eq!(report["rules"][0]["status"], "ok");
    assert_eq!(report["rules"][0]["count"], 1);
}

#[test]
fn two_runs_produce_the_same_bytes() {
    let repo = Repo::new(ceiling_config(), Some(&baseline("todo-markers", 5)));

    let first = repo.run(&["check", "--format", "json"]);
    let second = repo.run(&["check", "--format", "json"]);

    assert_eq!(code(&first), 0, "{}", stderr(&first));
    assert_eq!(stdout(&first), stdout(&second));
    assert_eq!(stderr(&first), stderr(&second));
}

#[test]
fn a_committed_baseline_that_does_not_parse_exits_two() {
    let repo = Repo::new(ceiling_config(), Some(&baseline("todo-markers", 5)));
    // The committed baseline is broken, the working tree is fine.
    repo.write("ratchet.baseline.json", "{ this is not a baseline");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "broken"]);
    repo.write("ratchet.baseline.json", &baseline("todo-markers", 5));

    let output = repo.run(&["check"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let text = stderr(&output);
    assert!(
        text.contains("the committed baseline is unreadable"),
        "{text}"
    );
    // The gate is not silently disabled.
    assert!(!text.contains("monotonicity skipped"), "{text}");
}

#[test]
fn an_unknown_command_or_path_exits_two() {
    let repo = Repo::new(ceiling_config(), Some(&baseline("todo-markers", 5)));

    for argument in ["chek", "driftt"] {
        let output = repo.run(&[argument]);
        assert_eq!(code(&output), 2, "{argument} must not be green");
        let text = stderr(&output);
        assert!(
            text.contains("unknown command or path"),
            "{argument}: {text}"
        );
    }
}

#[test]
fn a_path_that_does_not_exist_exits_two() {
    let repo = Repo::new(ceiling_config(), Some(&baseline("todo-markers", 5)));

    let output = repo.run(&["check", "src/does-not-exist.rs"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    assert!(
        stderr(&output).contains("unknown command or path"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn init_does_not_shadow_a_config_of_a_parent_directory() {
    let repo = Repo::new(ceiling_config(), Some(&baseline("todo-markers", 5)));
    let nested = repo.path().join("nested/deeper");
    std::fs::create_dir_all(&nested).expect("the directory is created");

    let output = binary()
        .arg("init")
        .current_dir(&nested)
        .output()
        .expect("the binary runs");

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        !nested.join("ratchet.config.jsonc").exists(),
        "init must not write a second config"
    );
    assert!(
        stdout(&output).contains("already exists"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn a_counter_error_keeps_the_metadata_of_the_rule() {
    let repo = Repo::new(counter_error_config(), Some(&baseline("a", 1)));

    let output = repo.run(&["check", "--format", "json"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    assert_eq!(report["rules"][0]["status"], "counter-error");
    assert_eq!(report["rules"][0]["direction"], "ceiling");
    assert_eq!(report["rules"][0]["unit"], "call sites");
    assert_eq!(report["rules"][0]["class"], "layer violation");
    assert_eq!(report["rules"][0]["tier"], "slow");
}

#[test]
fn a_count_without_rules_is_a_successful_run() {
    let repo = Repo::new("[]", None);

    let output = repo.run(&["count", "--format", "json"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    assert_eq!(report["ok"], true);
    assert_eq!(report["summary"]["rules"], 0);
}

#[test]
fn a_generated_artifact_is_counted_and_the_tree_stays_clean() {
    let repo = Repo::new(
        r#"[
          {
            "rules": {
              "a": {
                "class": "c",
                "incident": "i",
                "unit": "files",
                "direction": "ceiling",
                "count": {
                  "file": "docs/_audit-raw.json",
                  "path": "coherence.items",
                  "generate": ["sh", "generate.sh"],
                  "output_arg": "--output"
                }
              }
            }
          }
        ]"#,
        Some(&baseline("a", 3)),
    );
    repo.write(
        "generate.sh",
        "#!/bin/sh\nprintf '{\"coherence\": {\"items\": [\"src/a.ts\", \"src/b.ts\", \"docs/c.ts\"]}}' > \"$2\"\n",
    );
    // The committed artifact is stale: the count must come from the generator.
    repo.write(
        "docs/_audit-raw.json",
        "{\"coherence\": {\"items\": [\"stale.ts\"]}}",
    );
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "artifact"]);

    let output = repo.run(&["check", "--format", "json"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    assert_eq!(report["rules"][0]["count"], 3);
    // The tree is unchanged: the generator wrote to a temporary path.
    assert_eq!(repo.status(), "");
    assert!(
        repo.read("docs/_audit-raw.json").contains("stale.ts"),
        "the artifact in the tree is untouched"
    );
}

#[test]
fn a_generated_artifact_is_counted_from_a_temporary_path() {
    let repo = Repo::new(
        r#"[
          {
            "rules": {
              "a": {
                "class": "c",
                "incident": "i",
                "unit": "files",
                "direction": "ceiling",
                "count": {
                  "file": "docs/_audit-raw.json",
                  "path": "coherence.items",
                  "generate": ["sh", "generate.sh"],
                  "output_arg": "--output"
                }
              }
            }
          }
        ]"#,
        Some(&baseline("a", 3)),
    );
    repo.write(
        "generate.sh",
        "#!/bin/sh\nprintf '{\"coherence\": {\"items\": [\"src/a.ts\", \"src/b.ts\", \"docs/c.ts\"]}}' > \"$2\"\n",
    );
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "generator"]);

    let output = repo.run(&["check", "--format", "json"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    assert_eq!(report["rules"][0]["count"], 3);
    assert_eq!(repo.status(), "");
}

#[test]
fn a_failing_generator_leaves_the_tree_unchanged() {
    let repo = Repo::new(
        r#"[
          {
            "rules": {
              "a": {
                "class": "c",
                "incident": "i",
                "unit": "files",
                "direction": "ceiling",
                "count": {
                  "file": "docs/_audit-raw.json",
                  "path": "coherence.items",
                  "generate": ["sh", "generate.sh"],
                  "output_arg": "--output"
                }
              }
            }
          }
        ]"#,
        Some(&baseline("a", 3)),
    );
    repo.write(
        "generate.sh",
        "#!/bin/sh\nprintf 'broken' > \"$2\"\nexit 1\n",
    );
    repo.write("docs/_audit-raw.json", "{\"coherence\": {\"items\": []}}");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "generator"]);

    let output = repo.run(&["check"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    assert_eq!(repo.status(), "");
    assert!(repo.read("docs/_audit-raw.json").contains("items"));
}

#[test]
fn the_monotonicity_check_takes_precedence_over_the_count_comparison() {
    let config = r#"[
      {
        "rules": {
          "tests": {
            "class": "coverage",
            "incident": "a test was deleted",
            "unit": "tests",
            "direction": "floor",
            "count": { "command": ["echo", "1"] }
          }
        }
      }
    ]"#;
    let repo = Repo::new(config, Some(&baseline("tests", 5)));
    // The floor baseline is lowered and the count is below it as well.
    repo.write("ratchet.baseline.json", &baseline("tests", 2));

    let output = repo.run(&["check", "--format", "json"]);

    assert_eq!(code(&output), 1, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    assert_eq!(report["rules"][0]["status"], "baseline-lowered");
    // The count comparison is reported as an additional note.
    let text = stderr(&output);
    assert!(
        text.contains("the count 1 also fails against the baseline 2 (under-floor)"),
        "{text}"
    );
}

// --- the determinism rule ----------------------------------------------------

/// Asserts that a message contains nothing machine-specific.
///
/// An absolute path, a `file://` URL, a temporary directory, a process id or a
/// nanosecond timestamp make the same tree produce different bytes.
fn assert_deterministic(text: &str) {
    for forbidden in [
        "file://",
        "/var/folders",
        "/private/var",
        "/tmp/",
        // The temporary directory of the generator: it carries the process id
        // and a nanosecond timestamp.
        "ratchet-generate-",
    ] {
        assert!(
            !text.contains(forbidden),
            "the output contains \"{forbidden}\": {text}"
        );
    }

    // The machine name must not survive either.
    if let Some(hostname) = hostname() {
        assert!(
            !text.contains(&hostname),
            "the output contains the machine name: {text}"
        );
    }

    // No timestamp: a nanosecond clock has 19 digits, a millisecond clock 13.
    let digits = text
        .split(|character: char| !character.is_ascii_digit())
        .filter(|run| !run.is_empty());
    for run in digits {
        assert!(
            run.len() < 12,
            "the output contains a timestamp-like number ({run}): {text}"
        );
    }
}

/// The name of the machine the test runs on.
fn hostname() -> Option<String> {
    if let Ok(name) = std::env::var("HOSTNAME") {
        if !name.is_empty() {
            return Some(name);
        }
    }
    let output = Command::new("hostname").output().ok()?;
    let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if name.is_empty() { None } else { Some(name) }
}

#[test]
fn a_failing_generator_does_not_leak_the_temporary_path() {
    let repo = Repo::new(
        r#"[
          {
            "rules": {
              "a": {
                "class": "c",
                "incident": "i",
                "unit": "files",
                "direction": "ceiling",
                "count": {
                  "file": "docs/_audit-raw.json",
                  "path": "coherence.items",
                  "generate": ["sh", "generate.sh"],
                  "output_arg": "--output"
                }
              }
            }
          }
        ]"#,
        Some(&baseline("a", 3)),
    );
    // The generator prints the path it was given — the temporary one.
    repo.write(
        "generate.sh",
        "#!/bin/sh\necho \"cannot write to $2\" >&2\nexit 3\n",
    );
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "generator"]);

    let first = repo.run(&["check"]);
    let second = repo.run(&["check"]);

    assert_eq!(code(&first), 2, "{}", stderr(&first));
    let text = format!("{}{}", stdout(&first), stderr(&first));
    assert_deterministic(&text);
    // The generator's own message and the file it was supposed to write stay.
    assert!(text.contains("exited with status 3"), "{text}");
    assert!(text.contains("docs/_audit-raw.json"), "{text}");
    // Two runs of the same tree produce the same bytes.
    assert_eq!(stdout(&first), stdout(&second));
    assert_eq!(stderr(&first), stderr(&second));
}

#[test]
fn a_failing_generator_does_not_relay_its_output() {
    let repo = Repo::new(
        r#"[
          {
            "rules": {
              "a": {
                "class": "c",
                "incident": "i",
                "unit": "files",
                "direction": "ceiling",
                "count": {
                  "file": "docs/_audit-raw.json",
                  "path": "coherence.items",
                  "generate": ["sh", "generate.sh"],
                  "output_arg": "--output"
                }
              }
            }
          }
        ]"#,
        Some(&baseline("a", 3)),
    );
    // The generator prints the path it was given — the temporary one.
    repo.write(
        "generate.sh",
        "#!/bin/sh\necho \"cannot write to $2\" >&2\nexit 3\n",
    );
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "generator"]);

    let first = repo.run(&["check"]);
    let second = repo.run(&["check"]);

    assert_eq!(code(&first), 2, "{}", stderr(&first));
    let text = format!("{}{}", stdout(&first), stderr(&first));
    // The report carries ratchet's own words, never the output of the
    // generator (SPEC 6.2).
    assert!(!text.contains("cannot write to"), "{text}");
    assert!(!text.contains("ratchet-generate-"), "{text}");
    assert_deterministic(&text);
    // The generator's own message and the file it was supposed to write stay.
    assert!(
        text.contains("the generator for docs/_audit-raw.json"),
        "{text}"
    );
    assert!(text.contains("exited with status 3"), "{text}");
    // Two runs of the same tree produce the same bytes.
    assert_eq!(stdout(&first), stdout(&second));
    assert_eq!(stderr(&first), stderr(&second));
}

#[test]
fn a_broken_javascript_config_does_not_relay_nodes_output() {
    if !node_available() {
        eprintln!(
            "*** SKIPPED: node is not on the PATH — this JavaScript test did NOT run. \
             Install node (or run the CI, which installs it) to exercise it. ***"
        );
        return;
    }
    let repo = Repo::plain("", None);
    std::fs::remove_file(repo.path().join("ratchet.config.jsonc")).expect("the config is removed");
    // A syntax error: node prints the file URL of the config.
    repo.write("ratchet.config.mjs", "export default [\n");

    let output = repo.run(&["check"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let text = stderr(&output);
    // The report says that the config could not be evaluated; node's message
    // belongs to the diagnostic channel.
    assert!(text.contains("could not be evaluated"), "{text}");
    assert!(!text.contains("SyntaxError"), "{text}");
    assert!(!text.contains("file://"), "{text}");
    assert_deterministic(&text);
}

#[test]
fn a_non_serialisable_javascript_config_does_not_relay_nodes_output() {
    if !node_available() {
        eprintln!(
            "*** SKIPPED: node is not on the PATH — this JavaScript test did NOT run. \
             Install node (or run the CI, which installs it) to exercise it. ***"
        );
        return;
    }
    let repo = Repo::plain("", None);
    std::fs::remove_file(repo.path().join("ratchet.config.jsonc")).expect("the config is removed");
    // A circular structure: node prints the URL of the evaluated script.
    repo.write(
        "ratchet.config.mjs",
        "const x = {};\nx.self = x;\nexport default x;\n",
    );

    let output = repo.run(&["check"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let text = stderr(&output);
    assert!(text.contains("could not be evaluated"), "{text}");
    assert!(!text.contains("TypeError"), "{text}");
    assert!(!text.contains("file://"), "{text}");
    assert_deterministic(&text);
}

#[test]
fn a_scope_of_src_matches_the_directory_itself() {
    let config = r#"[
      {
        "rules": {
          "a": {
            "class": "c",
            "incident": "i",
            "unit": "u",
            "direction": "ceiling",
            "count": { "command": ["echo", "1"] },
            "files": ["src/**"]
          }
        }
      }
    ]"#;
    let repo = Repo::new(config, Some(&baseline("a", 1)));
    repo.write("src/main.rs", "fn main() {}");

    // The README example: `ratchet check src/` selects the rule.
    let output = repo.run(&["check", "src/"]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert!(stdout(&output).contains("PASS"), "{}", stdout(&output));

    let output = repo.run(&["check", "src"]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert!(stdout(&output).contains("PASS"), "{}", stdout(&output));
}

#[test]
fn a_scope_that_selects_nothing_still_fails() {
    let config = r#"[
      {
        "rules": {
          "a": {
            "class": "c",
            "incident": "i",
            "unit": "u",
            "direction": "ceiling",
            "count": { "command": ["echo", "1"] },
            "files": ["src/**"]
          }
        }
      }
    ]"#;
    let repo = Repo::new(config, Some(&baseline("a", 1)));
    repo.write("docs/readme.md", "hello");

    let output = repo.run(&["check", "docs/readme.md"]);

    assert_eq!(code(&output), 1, "{}", stderr(&output));
    assert!(!stdout(&output).contains("PASS"), "{}", stdout(&output));
}

#[cfg(unix)]
#[test]
fn a_generator_that_replaces_the_file_does_not_touch_the_tree() {
    use std::os::unix::fs::PermissionsExt;

    let repo = Repo::new(
        r#"[
          {
            "rules": {
              "a": {
                "class": "c",
                "incident": "i",
                "unit": "files",
                "direction": "ceiling",
                "count": {
                  "file": "docs/_audit-raw.json",
                  "path": "coherence.items",
                  "generate": ["sh", "generate.sh"],
                  "output_arg": "--output"
                }
              }
            }
          }
        ]"#,
        Some(&baseline("a", 3)),
    );
    // The generator writes the fresh bytes to the temporary path it was
    // given — the tree is not its business.
    repo.write(
        "generate.sh",
        "#!/bin/sh\nprintf '{\"coherence\": {\"items\": [\"a.ts\", \"b.ts\", \"c.ts\"]}}' > \"$2\"\nchmod 600 \"$2\"\n",
    );
    repo.write(
        "docs/_audit-raw.json",
        "{\"coherence\": {\"items\": [\"a.ts\"]}}",
    );
    std::fs::set_permissions(
        repo.path().join("docs/_audit-raw.json"),
        std::fs::Permissions::from_mode(0o755),
    )
    .expect("the permissions are set");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "artifact"]);

    let output = repo.run(&["check", "--format", "json"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    assert_eq!(report["rules"][0]["count"], 3);
    // The engine never writes into the tree: the artifact is untouched.
    assert_eq!(repo.status(), "");
    let mode = std::fs::metadata(repo.path().join("docs/_audit-raw.json"))
        .expect("the file exists")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o755, "the permissions are unchanged");
}

#[test]
fn the_json_report_carries_the_reason_of_a_failure() {
    let repo = Repo::new(ceiling_config(), Some(&baseline("todo-markers", 2)));

    let output = repo.run(&["check", "--format", "json"]);

    assert_eq!(code(&output), 1, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    assert_eq!(report["rules"][0]["status"], "over-baseline");
    assert_eq!(report["rules"][0]["detail"], "count 3 > baseline 2");
}

#[test]
fn the_json_report_carries_the_reason_of_a_counter_error() {
    let repo = Repo::new(counter_error_config(), Some(&baseline("a", 1)));

    let output = repo.run(&["check", "--format", "json"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    assert_eq!(report["rules"][0]["status"], "counter-error");
    let detail = report["rules"][0]["detail"]
        .as_str()
        .expect("the counter error has a detail");
    assert!(detail.contains("exited with status"), "{detail}");
}

#[test]
fn a_green_rule_has_no_detail() {
    let repo = Repo::new(ceiling_config(), Some(&baseline("todo-markers", 5)));

    let output = repo.run(&["check", "--format", "json"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    assert_eq!(report["rules"][0]["status"], "ok");
    assert!(report["rules"][0].get("detail").is_none());
}

#[test]
fn the_baseline_command_prints_the_path_it_wrote() {
    let repo = Repo::new(ceiling_config(), None);

    let output = repo.run(&["baseline", "init"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("ratchet.baseline.json"), "{text}");
    assert_deterministic(&text);
}

#[test]
fn baseline_path_names_the_default_baseline() {
    let repo = Repo::new(ceiling_config(), None);

    let output = repo.run(&["baseline", "path"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    // The path and nothing else: the output is meant for a shell.
    assert_eq!(stdout(&output), "ratchet.baseline.json\n");
}

#[test]
fn baseline_path_names_the_baseline_the_config_names() {
    let repo = Repo::new(
        r#"[
          {
            "name": "test",
            "baseline": "custom/baseline.json",
            "rules": {
              "a": {
                "class": "c",
                "incident": "i",
                "unit": "u",
                "direction": "ceiling",
                "count": { "command": ["echo", "1"] }
              }
            }
          }
        ]"#,
        None,
    );

    let output = repo.run(&["baseline", "path"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert_eq!(stdout(&output), "custom/baseline.json\n");
}

#[test]
fn baseline_path_names_the_file_the_other_commands_write() {
    // The path is the one of the file the baseline commands write — one
    // resolution, not two.
    let repo = Repo::new(
        r#"[
          {
            "name": "test",
            "baseline": "custom/baseline.json",
            "rules": {
              "a": {
                "class": "c",
                "incident": "i",
                "unit": "u",
                "direction": "ceiling",
                "count": { "command": ["echo", "1"] }
              }
            }
          }
        ]"#,
        None,
    );

    let path = repo.run(&["baseline", "path"]);
    let init = repo.run(&["baseline", "init"]);

    assert_eq!(code(&init), 0, "{}", stderr(&init));
    let named = stdout(&path).trim().to_string();
    assert_eq!(named, "custom/baseline.json");
    assert!(
        repo.path().join(&named).is_file(),
        "the baseline command wrote the file the path names"
    );
}

#[cfg(unix)]
#[test]
fn baseline_writer_creates_the_resolved_directory_behind_a_link() {
    let repo = Repo::new(
        r#"[
          {
            "baseline": "linked/ratchet.baseline.json",
            "rules": {
              "a": {
                "class": "c",
                "incident": "i",
                "unit": "u",
                "direction": "ceiling",
                "count": { "command": ["echo", "1"] }
              }
            }
          }
        ]"#,
        None,
    );
    std::os::unix::fs::symlink("sub", repo.path().join("linked")).expect("the link is made");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "linked baseline directory"]);

    let output = repo.run(&["baseline", "init"]);
    let path = repo.run(&["baseline", "path"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert_eq!(stdout(&path), "sub/ratchet.baseline.json\n");
    let written: Value = serde_json::from_str(&repo.read("sub/ratchet.baseline.json"))
        .expect("the baseline is written behind the link");
    assert_eq!(written["rules"]["a"]["value"], 1);
    assert!(
        std::fs::symlink_metadata(repo.path().join("linked"))
            .expect("the link remains")
            .file_type()
            .is_symlink()
    );
    assert_eq!(repo.status(), "?? sub/\n");
}

#[cfg(unix)]
#[test]
fn baseline_writer_rolls_back_resolved_directories_when_a_later_creation_fails() {
    let too_long = "x".repeat(300);
    let repo = Repo::new(
        &format!(
            r#"[
              {{
                "baseline": "linked/ok/{too_long}/ratchet.baseline.json",
                "rules": {{
                  "a": {{
                    "class": "c",
                    "incident": "i",
                    "unit": "u",
                    "direction": "ceiling",
                    "count": {{ "command": ["echo", "1"] }}
                  }}
                }}
              }}
            ]"#
        ),
        None,
    );
    std::os::unix::fs::symlink("sub", repo.path().join("linked")).expect("the link is made");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "linked baseline directory"]);

    let output = repo.run(&["baseline", "init"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let text = stderr(&output);
    assert!(
        text.contains(&format!(
            "cannot create the baseline directory sub/ok/{too_long}"
        )),
        "{text}"
    );
    assert!(!text.contains("working tree was modified"), "{text}");
    assert!(
        !repo.path().join("sub").exists(),
        "directories from the failed attempt were left behind"
    );
    assert_eq!(
        repo.status(),
        "",
        "the failed baseline write changed the tree"
    );
}

#[test]
fn baseline_path_of_a_javascript_config_names_its_baseline() {
    if !node_available() {
        eprintln!(
            "*** SKIPPED: node is not on the PATH — this JavaScript test did NOT run. \
             Install node (or run the CI, which installs it) to exercise it. ***"
        );
        return;
    }
    let repo = Repo::plain("", None);
    std::fs::remove_file(repo.path().join("ratchet.config.jsonc")).expect("the config is removed");
    repo.write(
        "ratchet.config.mjs",
        "export default [\n  {\n    name: \"test\",\n    baseline: \"custom/baseline.json\",\n    rules: {},\n  },\n];\n",
    );

    let output = repo.run(&["baseline", "path"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert_eq!(stdout(&output), "custom/baseline.json\n");
}

#[test]
fn baseline_path_without_a_config_exits_two() {
    let repo = Repo::plain("", None);
    std::fs::remove_file(repo.path().join("ratchet.config.jsonc")).expect("the config is removed");

    let output = repo.run_without_git(&["baseline", "path"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    assert!(
        stderr(&output).contains("no config found"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn baseline_path_of_a_broken_config_exits_two() {
    let repo = Repo::plain("this is not a config", None);

    let output = repo.run(&["baseline", "path"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    assert!(
        stderr(&output).contains("invalid JSON"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn baseline_path_of_a_baseline_outside_the_repository_root_exits_two() {
    let repo = Repo::plain(r#"[{"baseline": "../outside.json", "rules": {}}]"#, None);

    let output = repo.run(&["baseline", "path"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    assert!(
        stderr(&output).contains("baseline must be inside the repository root"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn baseline_init_does_not_count_the_rules_that_have_a_baseline() {
    // The counter of the rule fails, but the rule has a baseline already: the
    // command must not run the counter.
    let config = r#"[
      {
        "rules": {
          "broken": {
            "class": "c",
            "incident": "i",
            "unit": "u",
            "direction": "ceiling",
            "count": { "command": ["false"] }
          }
        }
      }
    ]"#;
    let repo = Repo::new(config, Some(&baseline("broken", 1)));

    let output = repo.run(&["baseline", "init"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert!(
        stdout(&output).contains("kept baseline 1"),
        "{}",
        stdout(&output)
    );
    assert!(repo.read("ratchet.baseline.json").contains("\"value\": 1"));
}

#[test]
fn baseline_init_still_fails_when_a_needed_counter_fails() {
    let repo = Repo::new(counter_error_config(), None);

    let output = repo.run(&["baseline", "init"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    assert!(
        stdout(&output).contains("the counter exited with status 1"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn init_writes_the_config_where_the_discovery_finds_it() {
    let repo = Repo::new(ceiling_config(), None);
    std::fs::remove_file(repo.path().join("ratchet.config.jsonc")).expect("the config is removed");
    let nested = repo.path().join("nested");
    std::fs::create_dir_all(&nested).expect("the directory is created");

    let output = binary()
        .arg("init")
        .current_dir(&nested)
        .output()
        .expect("the binary runs");

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    // The config is written into the root of the repository, not into the
    // subdirectory the command ran in.
    assert!(repo.path().join("ratchet.config.jsonc").exists());
    assert!(!nested.join("ratchet.config.jsonc").exists());
}

#[test]
fn a_counter_that_prints_the_working_directory_does_not_relay_it() {
    let repo = Repo::new(
        r#"[
          {
            "rules": {
              "a": {
                "class": "c",
                "incident": "i",
                "unit": "u",
                "direction": "ceiling",
                "count": { "command": ["sh", "count.sh"] }
              }
            }
          }
        ]"#,
        Some(&baseline("a", 1)),
    );
    // The counter prints the absolute path of the repository.
    repo.write(
        "count.sh",
        "#!/bin/sh\necho \"failed to read $PWD/docs/audit.json\" >&2\nexit 1\n",
    );
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "counter"]);

    let output = repo.run(&["check"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let text = format!("{}{}", stdout(&output), stderr(&output));
    // The report carries the exit status, not the output of the counter.
    assert!(!text.contains("failed to read"), "{text}");
    assert!(!text.contains("docs/audit.json"), "{text}");
    assert_deterministic(&text);
    assert!(text.contains("the counter exited with status 1"), "{text}");
}

// --- the report and the diagnostic channel ----------------------------------

#[test]
fn the_debug_channel_contains_the_output_of_the_counter() {
    let repo = Repo::new(
        r#"[
          {
            "rules": {
              "a": {
                "class": "c",
                "incident": "i",
                "unit": "u",
                "direction": "ceiling",
                "count": { "command": ["sh", "count.sh"] }
              }
            }
          }
        ]"#,
        Some(&baseline("a", 1)),
    );
    repo.write(
        "count.sh",
        "#!/bin/sh\necho 'the reason the counter failed' >&2\nexit 1\n",
    );
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "counter"]);

    let output = repo.run(&["check", "--debug"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let text = stderr(&output);
    // The diagnostic channel carries the raw output of the subprocess.
    assert!(text.contains("the reason the counter failed"), "{text}");
    // The report itself stays clean.
    assert!(!stdout(&output).contains("the reason the counter failed"));
}

#[test]
fn the_debug_channel_contains_the_output_of_the_generator() {
    let repo = Repo::new(
        r#"[
          {
            "rules": {
              "a": {
                "class": "c",
                "incident": "i",
                "unit": "files",
                "direction": "ceiling",
                "count": {
                  "file": "docs/_audit-raw.json",
                  "path": "coherence.items",
                  "generate": ["sh", "generate.sh"],
                  "output_arg": "--output"
                }
              }
            }
          }
        ]"#,
        Some(&baseline("a", 3)),
    );
    repo.write(
        "generate.sh",
        "#!/bin/sh\necho 'the reason the generator failed' >&2\nexit 3\n",
    );
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "generator"]);

    let output = repo.run(&["check", "--debug"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let text = stderr(&output);
    assert!(text.contains("the reason the generator failed"), "{text}");
    assert!(!stdout(&output).contains("the reason the generator failed"));
}

#[test]
fn the_debug_channel_contains_the_output_of_node() {
    if !node_available() {
        eprintln!(
            "*** SKIPPED: node is not on the PATH — this JavaScript test did NOT run. \
             Install node (or run the CI, which installs it) to exercise it. ***"
        );
        return;
    }
    let repo = Repo::plain("", None);
    std::fs::remove_file(repo.path().join("ratchet.config.jsonc")).expect("the config is removed");
    repo.write("ratchet.config.mjs", "export default [\\n");

    let output = repo.run(&["check", "--debug"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let text = stderr(&output);
    // Node's message belongs to the diagnostic channel.
    assert!(text.contains("SyntaxError"), "{text}");
    // The report says that the config could not be evaluated.
    assert!(text.contains("could not be evaluated"), "{text}");
}

#[test]
fn the_debug_channel_prints_the_resolved_config_for_every_command() {
    let repo = Repo::new(ceiling_config(), Some(&baseline("todo-markers", 5)));

    for arguments in [
        vec!["check", "--debug"],
        vec!["count", "--debug"],
        vec!["baseline", "init", "--debug"],
        vec!["baseline", "update", "--debug"],
        vec!["drift", "--debug"],
    ] {
        let output = repo.run(&arguments);
        let text = stderr(&output);
        assert!(
            text.contains("resolved config"),
            "{arguments:?} did not print the resolved config: {text}"
        );
        assert!(
            text.contains("todo-markers"),
            "{arguments:?} did not print the resolved config: {text}"
        );
    }
}

#[test]
fn the_debug_channel_prints_the_counter_invocations_for_every_command() {
    let repo = Repo::new(ceiling_config(), Some(&baseline("todo-markers", 5)));

    for arguments in [
        vec!["check", "--debug"],
        vec!["count", "--debug"],
        vec!["baseline", "update", "--debug"],
    ] {
        let output = repo.run(&arguments);
        let text = stderr(&output);
        assert!(
            text.contains("counter todo-markers"),
            "{arguments:?} did not print the counter invocation: {text}"
        );
    }

    // `baseline init` only counts the rules that have no baseline yet.
    let fresh = Repo::new(ceiling_config(), None);
    let output = fresh.run(&["baseline", "init", "--debug"]);
    let text = stderr(&output);
    assert!(
        text.contains("counter todo-markers"),
        "baseline init did not print the counter invocation: {text}"
    );
}

#[test]
fn a_generate_without_an_output_arg_is_a_configuration_error() {
    let repo = Repo::new(
        r#"[
          {
            "rules": {
              "a": {
                "class": "c",
                "incident": "i",
                "unit": "files",
                "direction": "ceiling",
                "count": {
                  "file": "docs/_audit-raw.json",
                  "path": "coherence.items",
                  "generate": ["sh", "generate.sh"]
                }
              }
            }
          }
        ]"#,
        Some(&baseline("a", 3)),
    );
    repo.write("generate.sh", "#!/bin/sh\nprintf 'x' > \"$2\"\n");
    repo.write("docs/_audit-raw.json", "{\"coherence\": {\"items\": []}}");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "generator"]);

    let output = repo.run(&["check"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let text = stderr(&output);
    assert!(text.contains("rules.a.count.output_arg"), "{text}");
    assert!(text.contains("is required by"), "{text}");
    // The tree is untouched.
    assert_eq!(repo.status(), "");
}

#[test]
fn a_baseline_outside_the_repository_root_is_a_configuration_error() {
    let repo = Repo::new(
        r#"[
          {
            "baseline": "../outside.json",
            "rules": {
              "a": {
                "class": "c",
                "incident": "i",
                "unit": "u",
                "direction": "ceiling",
                "count": { "command": ["echo", "1"] }
              }
            }
          }
        ]"#,
        Some(&baseline("a", 1)),
    );

    let output = repo.run(&["check"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let text = stderr(&output);
    assert!(
        text.contains("baseline must be inside the repository root"),
        "{text}"
    );
    // The gate is not silently disabled.
    assert!(!text.contains("monotonicity skipped"), "{text}");
}

#[test]
fn an_absolute_baseline_is_a_configuration_error() {
    let repo = Repo::new(
        r#"[
          {
            "baseline": "/etc/ratchet-baseline.json",
            "rules": {
              "a": {
                "class": "c",
                "incident": "i",
                "unit": "u",
                "direction": "ceiling",
                "count": { "command": ["echo", "1"] }
              }
            }
          }
        ]"#,
        Some(&baseline("a", 1)),
    );

    let output = repo.run(&["check"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let text = stderr(&output);
    // The path of a project is relative: the message says what is wrong.
    assert!(text.contains("has to be a relative path"), "{text}");
    assert!(!text.contains("baseline must be inside"), "{text}");
    assert!(!text.contains("monotonicity skipped"), "{text}");
}

#[cfg(unix)]
#[test]
fn a_baseline_through_a_linked_directory_outside_the_root_is_refused() {
    // The directory of the baseline is a link that points out of the root:
    // the baseline would be written there, and it would never be committed —
    // the monotonicity gate would be silently off.
    let repo = Repo::new(
        r#"[
          {
            "baseline": "linked/ratchet.baseline.json",
            "rules": {
              "a": {
                "class": "c",
                "incident": "i",
                "unit": "u",
                "direction": "ceiling",
                "count": { "command": ["echo", "1"] }
              }
            }
          }
        ]"#,
        None,
    );
    let outside = Outside::new("baseline");
    std::os::unix::fs::symlink(outside.path(), repo.path().join("linked"))
        .expect("the link is made");

    let output = repo.run(&["baseline", "init"]);

    // The refusal is a configuration error, and the gate is not silently
    // disabled.
    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let text = stderr(&output);
    assert!(
        text.contains("baseline must be inside the repository root"),
        "{text}"
    );
    assert!(!text.contains("monotonicity skipped"), "{text}");
    // And the baseline was not written outside the root.
    assert!(
        !outside.file("ratchet.baseline.json").exists(),
        "the baseline was written outside the repository root"
    );
}

#[cfg(unix)]
#[test]
fn a_baseline_through_a_chain_of_links_outside_the_root_is_refused() {
    // The chain is what carries the path out: `ratchet.baseline.json` →
    // `real/baseline.json` → a file outside the root that is not there.  A
    // single link is refused already; the chain is the shape that slips
    // through — and it is the shape that writes outside.
    let repo = Repo::new(
        r#"[
          {
            "baseline": "ratchet.baseline.json",
            "rules": {
              "a": {
                "class": "c",
                "incident": "i",
                "unit": "u",
                "direction": "ceiling",
                "count": { "command": ["echo", "1"] }
              }
            }
          }
        ]"#,
        None,
    );
    let outside = Outside::new("baseline-chain");
    link_chain(
        &repo.path().join("ratchet.baseline.json"),
        &repo.path().join("real/baseline.json"),
        &outside.file("shared-baseline.json"),
    );

    let output = repo.run(&["baseline", "init"]);

    // The refusal names the reason, and the gate is not silently disabled.
    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let text = stderr(&output);
    assert!(
        text.contains("baseline must be inside the repository root"),
        "{text}"
    );
    assert!(!text.contains("monotonicity skipped"), "{text}");
    // And the baseline was not written outside the root.
    assert!(
        !outside.file("shared-baseline.json").exists(),
        "the baseline was written outside the repository root"
    );
}

#[cfg(unix)]
#[test]
fn a_baseline_behind_a_cycle_of_links_is_refused() {
    // The links do not end: the path cannot be resolved, and a path that
    // cannot be resolved is not inside the root.  The run must not hang.
    let repo = Repo::new(
        r#"[
          {
            "baseline": "ratchet.baseline.json",
            "rules": {
              "a": {
                "class": "c",
                "incident": "i",
                "unit": "u",
                "direction": "ceiling",
                "count": { "command": ["echo", "1"] }
              }
            }
          }
        ]"#,
        None,
    );
    std::os::unix::fs::symlink("b", repo.path().join("ratchet.baseline.json"))
        .expect("the first link is made");
    std::os::unix::fs::symlink("ratchet.baseline.json", repo.path().join("b"))
        .expect("the second link is made");

    let output = repo.run(&["baseline", "init"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let text = stderr(&output);
    assert!(text.contains("could not be resolved"), "{text}");
    assert!(
        text.contains("the links of the path could not be followed to the end"),
        "{text}"
    );
}

#[test]
fn a_counter_that_prints_a_timestamp_and_the_machine_name_does_not_relay_it() {
    let repo = Repo::new(
        r#"[
          {
            "rules": {
              "a": {
                "class": "c",
                "incident": "i",
                "unit": "u",
                "direction": "ceiling",
                "count": { "command": ["sh", "count.sh"] }
              }
            }
          }
        ]"#,
        Some(&baseline("a", 1)),
    );
    repo.write(
        "count.sh",
        "#!/bin/sh\necho \"run on $(hostname) at $(date +%s)\" >&2\nexit 1\n",
    );
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "counter"]);

    let output = repo.run(&["check"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let text = format!("{}{}", stdout(&output), stderr(&output));
    // Neither the machine name nor the timestamp reaches the report.
    assert!(!text.contains("run on"), "{text}");
    assert_deterministic(&text);
    assert!(text.contains("the counter exited with status 1"), "{text}");
}

// --- the fourth round of findings -------------------------------------------

/// The entries of a directory.
fn entries(path: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(path)
        .expect("the directory is readable")
        .map(|entry| {
            entry
                .expect("the entry is readable")
                .path()
                .display()
                .to_string()
        })
        .collect();
    names.sort();
    names
}

#[test]
fn a_failing_generator_leaves_no_temporary_directory_behind() {
    let repo = Repo::new(
        r#"[
          {
            "rules": {
              "a": {
                "class": "c",
                "incident": "i",
                "unit": "files",
                "direction": "ceiling",
                "count": {
                  "file": "docs/_audit-raw.json",
                  "path": "coherence.items",
                  "generate": ["sh", "generate.sh"],
                  "output_arg": "--output"
                }
              }
            }
          }
        ]"#,
        Some(&baseline("a", 3)),
    );
    repo.write("generate.sh", "#!/bin/sh\necho 'the reason' >&2\nexit 3\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "generator"]);

    // The engine writes its temporary output into TMPDIR.
    let temporary = TempDir::new().expect("a temporary directory");
    let output = binary()
        .arg("check")
        .current_dir(repo.path())
        .env("TMPDIR", temporary.path())
        .env("TMP", temporary.path())
        .env("TEMP", temporary.path())
        .output()
        .expect("the binary runs");

    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(
        entries(temporary.path()).is_empty(),
        "the temporary directory was left behind: {:?}",
        entries(temporary.path())
    );
}

#[test]
fn a_generator_that_writes_nothing_leaves_no_temporary_directory_behind() {
    let repo = Repo::new(
        r#"[
          {
            "rules": {
              "a": {
                "class": "c",
                "incident": "i",
                "unit": "files",
                "direction": "ceiling",
                "count": {
                  "file": "docs/_audit-raw.json",
                  "path": "coherence.items",
                  "generate": ["sh", "generate.sh"],
                  "output_arg": "--output"
                }
              }
            }
          }
        ]"#,
        Some(&baseline("a", 3)),
    );
    // The generator succeeds but writes nothing.
    repo.write("generate.sh", "#!/bin/sh\nexit 0\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "generator"]);

    let temporary = TempDir::new().expect("a temporary directory");
    let output = binary()
        .arg("check")
        .current_dir(repo.path())
        .env("TMPDIR", temporary.path())
        .env("TMP", temporary.path())
        .env("TEMP", temporary.path())
        .output()
        .expect("the binary runs");

    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(
        entries(temporary.path()).is_empty(),
        "the temporary directory was left behind: {:?}",
        entries(temporary.path())
    );
}

#[test]
fn the_report_shows_a_repository_relative_path_for_the_generator() {
    // The artifact is a path of the repository: a path that leaves the root
    // is refused by the rule, before the generator runs (the file counter
    // with a `generate` is no exception).
    let repo = Repo::new(
        r#"[
          {
            "rules": {
              "a": {
                "class": "c",
                "incident": "i",
                "unit": "files",
                "direction": "ceiling",
                "count": {
                  "file": "docs/ratchet-probe.json",
                  "path": "coherence.items",
                  "generate": ["sh", "generate.sh"],
                  "output_arg": "--output"
                }
              }
            }
          }
        ]"#,
        Some(&baseline("a", 3)),
    );
    repo.write("generate.sh", "#!/bin/sh\nexit 2\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "generator"]);

    let output = repo.run(&["check"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let text = format!("{}{}", stdout(&output), stderr(&output));
    // The report shows the repository-relative path, never an absolute one.
    assert!(
        text.contains("the generator for docs/ratchet-probe.json"),
        "{text}"
    );
    assert!(
        !text.contains(&repo.path().display().to_string()),
        "the report contains the absolute path: {text}"
    );
    assert_deterministic(&text);
}

#[test]
fn the_diagnostic_channel_contains_the_stdout_of_a_javascript_config() {
    if !node_available() {
        eprintln!(
            "*** SKIPPED: node is not on the PATH — this JavaScript test did NOT run. \
             Install node (or run the CI, which installs it) to exercise it. ***"
        );
        return;
    }
    let repo = Repo::plain("", None);
    std::fs::remove_file(repo.path().join("ratchet.config.jsonc")).expect("the config is removed");
    // The module writes to stdout: the export is not valid JSON any more.
    repo.write(
        "ratchet.config.mjs",
        "console.log(\"noise from the config module\");\nexport default [];\n",
    );

    let output = repo.run(&["check", "--debug"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let text = stderr(&output);
    // The report says what happened; node's stdout is in the diagnostic
    // channel.
    assert!(text.contains("did not export valid JSON"), "{text}");
    assert!(
        text.contains("noise from the config module"),
        "the diagnostic channel does not contain node's stdout: {text}"
    );
}

#[test]
fn init_debug_prints_the_resolved_config() {
    let repo = Repo::new(ceiling_config(), Some(&baseline("todo-markers", 5)));

    let output = repo.run(&["init", "--debug"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let text = stderr(&output);
    assert!(text.contains("resolved config"), "{text}");
    assert!(text.contains("todo-markers"), "{text}");
}

#[test]
fn init_debug_says_that_there_is_nothing_to_resolve() {
    let repo = Repo::new(ceiling_config(), None);
    std::fs::remove_file(repo.path().join("ratchet.config.jsonc")).expect("the config is removed");

    let output = repo.run(&["init", "--debug"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let text = stderr(&output);
    assert!(
        text.contains("no config found — nothing to resolve"),
        "{text}"
    );
}

// --- the fifth round of findings --------------------------------------------

/// The documented stylish example, exactly as the binary prints it.
const DOCUMENTED_EXAMPLE: &str = "ratchet — 3 rules, 2 failed\n\n  FAIL  missing-baseline              no baseline for module-test-coverage\n  FAIL  unexpected-inner-html         7 > 5  (files)\n  PASS  unwrapped-action-calls        9 ≤ 11  (call sites)\n\nNOT COHERENT — 2 of 3 gates failed\n";

/// The root of the repository — the test reads the documents from there.
fn repository_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The documented stylish example, read out of a document.
///
/// The example is the fenced code block that starts with `ratchet — `.  A
/// document that cannot be read, that has no such block, or that has more
/// than one is an error: the test must fail, never skip.
fn documented_example(document: &Path) -> Result<String, String> {
    let text = std::fs::read_to_string(document)
        .map_err(|e| format!("{} cannot be read: {e}", document.display()))?;

    let mut blocks: Vec<String> = Vec::new();
    let mut current: Option<Vec<&str>> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            match &mut current {
                None => current = Some(Vec::new()),
                Some(lines) => {
                    let content = lines.join("\n");
                    if content.starts_with("ratchet — ") {
                        blocks.push(format!("{content}\n"));
                    }
                    current = None;
                }
            }
        } else if let Some(lines) = &mut current {
            lines.push(line);
        }
    }

    match blocks.len() {
        0 => Err(format!(
            "{} has no documented stylish example",
            document.display()
        )),
        1 => Ok(blocks.remove(0)),
        _ => Err(format!(
            "{} has more than one documented stylish example",
            document.display()
        )),
    }
}

#[test]
fn the_stylish_output_is_the_documented_one() {
    // The example of README.md and SPEC.md §6.1 — byte for byte, read out of
    // the documents: they cannot drift away from the implementation.
    let config = r#"[
      {
        "rules": {
          "unwrapped-action-calls": {
            "class": "layer violation",
            "incident": "F-09: a server action was called without the shared error helper",
            "unit": "call sites",
            "direction": "ceiling",
            "count": { "command": ["echo", "9"] }
          },
          "unexpected-inner-html": {
            "class": "layer violation",
            "incident": "an unescaped HTML sink reached the renderer",
            "unit": "files",
            "direction": "ceiling",
            "count": { "command": ["echo", "7"] }
          },
          "module-test-coverage": {
            "class": "coverage",
            "incident": "a module was merged without tests",
            "unit": "modules",
            "direction": "ceiling",
            "count": { "command": ["echo", "1"] }
          }
        }
      }
    ]"#;
    let baseline = "{\n  \"version\": 1,\n  \"rules\": {\n    \"unwrapped-action-calls\": { \"value\": 11 },\n    \"unexpected-inner-html\": { \"value\": 5 }\n  }\n}\n";
    let repo = Repo::new(config, Some(baseline));

    let output = repo.run(&["check"]);

    assert_eq!(code(&output), 1, "{}", stderr(&output));
    for document in ["README.md", "SPEC.md"] {
        let path = repository_root().join(document);
        let expected = documented_example(&path).unwrap_or_else(|error| panic!("{error}"));
        // The document carries the example the binary prints.
        assert_eq!(expected, DOCUMENTED_EXAMPLE, "{document} drifted");
        assert_eq!(stdout(&output), expected, "{document} drifted");
    }
}

#[test]
fn a_document_without_an_example_is_an_error() {
    let directory = TempDir::new().expect("a temporary directory");
    let document = directory.path().join("without-example.md");
    std::fs::write(&document, "# A document\n\nNo example here.\n").expect("the file is written");

    let error = documented_example(&document).expect_err("the extraction must fail");

    assert!(error.contains("no documented stylish example"), "{error}");

    // A document that cannot be read is an error as well.
    let missing = directory.path().join("does-not-exist.md");
    let error = documented_example(&missing).expect_err("the extraction must fail");
    assert!(error.contains("cannot be read"), "{error}");
}

#[test]
fn a_document_with_two_examples_is_an_error() {
    let directory = TempDir::new().expect("a temporary directory");
    let document = directory.path().join("two-examples.md");
    std::fs::write(
        &document,
        "# A document\n\n```\nratchet — 1 rule, 0 failed\n\nPASS  a  1 ≤ 1  (u)\n\nCOHERENT — 1 of 1 gates passed\n```\n\nand another one:\n\n```\nratchet — 3 rules, 2 failed\n\nFAIL  a  7 > 5  (files)\n\nNOT COHERENT — 2 of 3 gates failed\n```\n",
    )
    .expect("the file is written");

    let error = documented_example(&document).expect_err("the extraction must fail");

    assert!(
        error.contains("more than one documented stylish example"),
        "{error}"
    );
    assert!(error.contains("two-examples.md"), "{error}");
}

#[test]
fn a_generator_that_could_not_be_started_says_so() {
    let repo = Repo::new(
        r#"[
          {
            "rules": {
              "a": {
                "class": "c",
                "incident": "i",
                "unit": "files",
                "direction": "ceiling",
                "count": {
                  "file": "docs/_audit-raw.json",
                  "path": "coherence.items",
                  "generate": ["ratchet-generator-that-does-not-exist"],
                  "output_arg": "--output"
                }
              }
            }
          }
        ]"#,
        Some(&baseline("a", 3)),
    );

    let output = repo.run(&["check"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let text = format!("{}{}", stdout(&output), stderr(&output));
    // The process never started: it did not exit with anything.
    assert!(
        text.contains("the generator for docs/_audit-raw.json could not be started"),
        "{text}"
    );
    assert!(!text.contains("exited with an unknown status"), "{text}");
}

#[test]
fn the_diagnostic_channel_shows_the_argv_of_the_generator() {
    let repo = Repo::new(
        r#"[
          {
            "rules": {
              "a": {
                "class": "c",
                "incident": "i",
                "unit": "files",
                "direction": "ceiling",
                "count": {
                  "file": "docs/_audit-raw.json",
                  "path": "coherence.items",
                  "generate": ["sh", "generate.sh"],
                  "output_arg": "--output"
                }
              }
            }
          }
        ]"#,
        Some(&baseline("a", 3)),
    );
    repo.write(
        "generate.sh",
        "#!/bin/sh\nprintf '{\"coherence\": {\"items\": []}}' > \"$2\"\n",
    );
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "generator"]);

    let output = repo.run(&["check", "--debug"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let text = stderr(&output);
    // The argv that was really executed — the output path included.
    assert!(text.contains("sh generate.sh --output"), "{text}");
    assert!(text.contains("ratchet-generate-"), "{text}");

    // The argv is in the diagnostic channel even when the artifact is wrong.
    repo.write("generate.sh", "#!/bin/sh\nprintf '{}' > \"$2\"\n");
    let output = repo.run(&["check", "--debug"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let text = stderr(&output);
    assert!(text.contains("sh generate.sh --output"), "{text}");
    // The report says what happened; the diagnostic channel shows the argv.
    assert!(stdout(&output).contains("not found"), "{}", stdout(&output));
}

#[test]
fn the_diagnostic_channel_shows_the_stderr_of_a_javascript_config() {
    if !node_available() {
        eprintln!(
            "*** SKIPPED: node is not on the PATH — this JavaScript test did NOT run. \
             Install node (or run the CI, which installs it) to exercise it. ***"
        );
        return;
    }
    let repo = Repo::plain("", None);
    std::fs::remove_file(repo.path().join("ratchet.config.jsonc")).expect("the config is removed");
    // The module writes to stdout (the export is not valid JSON any more) and
    // a warning to stderr.
    repo.write(
        "ratchet.config.mjs",
        "console.error(\"a warning from the config module\");\nconsole.log(\"noise\");\nexport default [];\n",
    );

    let output = repo.run(&["check", "--debug"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let text = stderr(&output);
    assert!(text.contains("a warning from the config module"), "{text}");
    assert!(text.contains("noise"), "{text}");
    // The transcript has the same shape as everywhere else.
    assert!(text.contains("--- stdout ---"), "{text}");
    assert!(text.contains("--- stderr ---"), "{text}");
}

// --- the seventh round of findings ------------------------------------------

#[test]
fn the_error_comes_before_the_transcript() {
    // A broken JavaScript config: the run fails and the diagnostic channel
    // carries the transcript of node.
    if !node_available() {
        eprintln!(
            "*** SKIPPED: node is not on the PATH — this JavaScript test did NOT run. \
             Install node (or run the CI, which installs it) to exercise it. ***"
        );
        return;
    }
    let repo = Repo::plain("", None);
    std::fs::remove_file(repo.path().join("ratchet.config.jsonc")).expect("the config is removed");
    repo.write("ratchet.config.mjs", "export default [\n");

    let output = repo.run(&["check", "--debug"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let text = stderr(&output);
    let error = text.find("ratchet: error:").expect("the error is reported");
    let transcript = text
        .find("ratchet: debug:")
        .expect("the transcript is printed");
    assert!(
        error < transcript,
        "the error must come before the transcript: {text}"
    );
}

#[test]
fn the_error_comes_before_the_transcript_of_a_failing_counter() {
    // A failing counter is a failed gate, not an operational error: the
    // report carries the rule, the transcript carries the output of the
    // counter.
    let repo = Repo::new(
        r#"[
          {
            "rules": {
              "a": {
                "class": "c",
                "incident": "i",
                "unit": "u",
                "direction": "ceiling",
                "count": { "command": ["sh", "count.sh"] }
              }
            }
          }
        ]"#,
        Some(&baseline("a", 1)),
    );
    repo.write(
        "count.sh",
        "#!/bin/sh\necho 'the reason the counter failed' >&2\nexit 1\n",
    );
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "counter"]);

    let output = repo.run(&["check", "--debug"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let text = stderr(&output);
    // The report says what happened, the diagnostic channel the output.
    assert!(
        stdout(&output).contains("the counter exited with status 1"),
        "{text}"
    );
    assert!(
        text.contains("the reason the counter failed"),
        "the diagnostic channel does not carry the output: {text}"
    );
}

#[test]
fn an_empty_class_incident_or_unit_is_a_configuration_error() {
    for (field, value) in [("class", ""), ("incident", ""), ("unit", "")] {
        let config = format!(
            r#"[
              {{
                "rules": {{
                  "a": {{
                    "class": "c",
                    "incident": "i",
                    "unit": "u",
                    "direction": "ceiling",
                    "count": {{ "command": ["echo", "1"] }},
                    "{field}": "{value}"
                  }}
                }}
              }}
            ]"#
        );
        let repo = Repo::new(&config, Some(&baseline("a", 1)));

        let output = repo.run(&["check"]);

        assert_eq!(code(&output), 2, "{field}: {}", stderr(&output));
        let text = stderr(&output);
        assert!(
            text.contains(&format!("rules.a.{field}")),
            "{field}: {text}"
        );
        assert!(text.contains("must not be empty"), "{field}: {text}");
    }
}

// --- the drift gates ---------------------------------------------------------

/// A config with one generated file and no rule.
fn generated_config(command: &[&str], output_arg: Option<&str>) -> String {
    generated_config_at("docs/generated.md", command, output_arg)
}

/// A config with one generated file at `path` and no rule.
fn generated_config_at(path: &str, command: &[&str], output_arg: Option<&str>) -> String {
    let argv: Vec<String> = command.iter().map(|part| format!("\"{part}\"")).collect();
    let mut entry = format!(
        "{{ \"path\": \"{path}\", \"command\": [{}]",
        argv.join(", ")
    );
    if let Some(output_arg) = output_arg {
        entry.push_str(&format!(", \"output_arg\": \"{output_arg}\""));
    }
    entry.push_str(" }");
    format!("[{{ \"rules\": {{}}, \"generated\": [{entry}] }}]")
}

/// The mode of a file in the repository.
#[cfg(unix)]
fn mode_of(repo: &Repo, name: &str) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(repo.path().join(name))
        .expect("the file exists")
        .permissions()
        .mode()
        & 0o7777
}

/// Makes a directory read-only: nothing can be created in it.
#[cfg(unix)]
fn make_read_only(directory: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o555))
        .expect("the directory is read-only");
}

/// Makes a directory writable again.
#[cfg(unix)]
fn make_writable(directory: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o755))
        .expect("the directory is writable");
}

/// Sets the mode of a file in the repository.
#[cfg(unix)]
fn set_mode(repo: &Repo, name: &str, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(
        repo.path().join(name),
        std::fs::Permissions::from_mode(mode),
    )
    .expect("the mode is set");
}

/// Sets the time a file in the repository was written last.
fn set_modified(repo: &Repo, name: &str, time: std::time::SystemTime) {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(repo.path().join(name))
        .expect("the file is writable");
    let times = std::fs::FileTimes::new().set_modified(time);
    file.set_times(times).expect("the time is set");
}

/// The time a file in the repository was written last.
fn modified_of(repo: &Repo, name: &str) -> std::time::SystemTime {
    std::fs::metadata(repo.path().join(name))
        .expect("the file is readable")
        .modified()
        .expect("the file has a time")
}

#[test]
fn a_stale_generated_file_fails_with_a_unified_diff() {
    let repo = Repo::new(
        &generated_config(&["sh", "generate.sh"], Some("--output")),
        None,
    );
    repo.write("generate.sh", "#!/bin/sh\nprintf 'fresh\\n' > \"$2\"\n");
    repo.write("docs/generated.md", "stale\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "generated"]);

    let output = repo.run(&["drift"]);

    assert_eq!(code(&output), 1, "{}", stderr(&output));
    let text = stdout(&output);
    // The diff shows the difference between the committed file and the
    // produced one, with the repository-relative path.
    assert!(text.contains("docs/generated.md"), "{text}");
    assert!(text.contains("--- docs/generated.md"), "{text}");
    assert!(text.contains("+++ docs/generated.md"), "{text}");
    assert!(text.contains("-stale"), "{text}");
    assert!(text.contains("+fresh"), "{text}");
    assert!(text.contains("@@"), "{text}");
    // The verdict line agrees with the exit code.
    assert!(text.contains("NOT COHERENT"), "{text}");
}

#[test]
fn a_generated_file_in_sync_is_green() {
    let repo = Repo::new(
        &generated_config(&["sh", "generate.sh"], Some("--output")),
        None,
    );
    repo.write("generate.sh", "#!/bin/sh\nprintf 'fresh\\n' > \"$2\"\n");
    repo.write("docs/generated.md", "fresh\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "generated"]);

    let output = repo.run(&["drift"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert!(stdout(&output).contains("COHERENT"), "{}", stdout(&output));
}

#[test]
fn a_generated_file_that_is_not_committed_is_new_and_not_a_failure() {
    let repo = Repo::new(
        &generated_config(&["sh", "generate.sh"], Some("--output")),
        None,
    );
    repo.write("generate.sh", "#!/bin/sh\nprintf 'fresh\\n' > \"$2\"\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "generator"]);

    let output = repo.run(&["drift", "--format", "json"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    assert_eq!(report["summary"]["generated"], 1);
    assert_eq!(report["summary"]["drifted"], 0);
    assert_eq!(report["generated"][0]["status"], "new");
    assert_eq!(report["generated"][0]["path"], "docs/generated.md");
    assert_eq!(report["generated"][0]["diff"], Value::Null);
}

#[test]
fn the_fix_writes_the_generated_file_and_reports_it() {
    let repo = Repo::new(
        &generated_config(&["sh", "generate.sh"], Some("--output")),
        None,
    );
    repo.write("generate.sh", "#!/bin/sh\nprintf 'fresh\\n' > \"$2\"\n");
    repo.write("docs/generated.md", "stale\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "generated"]);

    let output = repo.run(&["drift", "--fix"]);

    // The produced content is in the working tree now.
    assert_eq!(repo.read("docs/generated.md"), "fresh\n");
    // The gate stays failed: the committed file is still stale (SPEC 7).
    assert_eq!(code(&output), 1, "{}", stderr(&output));
    // The report says what it did.
    let text = stdout(&output);
    assert!(
        text.contains("the produced content was written to the working tree"),
        "{text}"
    );
    assert!(text.contains("docs/generated.md"), "{text}");
    // The fix touches the generated file and nothing else.
    assert_eq!(repo.status(), " M docs/generated.md\n");
}

#[test]
fn the_fix_keeps_the_gate_failed_until_the_fix_is_committed() {
    let repo = Repo::new(
        &generated_config(&["sh", "generate.sh"], Some("--output")),
        None,
    );
    repo.write("generate.sh", "#!/bin/sh\nprintf 'fresh\\n' > \"$2\"\n");
    repo.write("docs/generated.md", "stale\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "generated"]);

    let output = repo.run(&["drift", "--fix", "--format", "json"]);

    // The status is still `drift`: the gate measures the committed artifact.
    assert_eq!(code(&output), 1, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    assert_eq!(report["generated"][0]["status"], "drift");
    assert_eq!(report["summary"]["drifted"], 1);
    let detail = report["generated"][0]["detail"]
        .as_str()
        .expect("the drift entry has a detail");
    assert!(detail.contains("working tree"), "{detail}");
    assert!(detail.contains("commit"), "{detail}");

    // Once the fix is committed, the gate is green.
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "fixed"]);
    let output = repo.run(&["drift", "--format", "json"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    assert_eq!(report["generated"][0]["status"], "ok");
    assert_eq!(report["summary"]["drifted"], 0);
    assert_eq!(report["ok"], true);
}

#[test]
fn the_fix_does_not_touch_hand_written_text() {
    let repo = Repo::new(
        &generated_config(&["sh", "generate.sh"], Some("--output")),
        None,
    );
    repo.write("generate.sh", "#!/bin/sh\nprintf 'fresh\\n' > \"$2\"\n");
    repo.write("docs/generated.md", "stale\n");
    repo.write("docs/hand-written.md", "mine\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "generated"]);

    let output = repo.run(&["drift", "--fix"]);

    // The gate stays failed until the fix is committed.
    assert_eq!(code(&output), 1, "{}", stderr(&output));
    assert_eq!(repo.read("docs/hand-written.md"), "mine\n");
    assert_eq!(repo.status(), " M docs/generated.md\n");
}

#[test]
fn a_drift_run_leaves_the_tree_unchanged() {
    let repo = Repo::new(
        &generated_config(&["sh", "generate.sh"], Some("--output")),
        None,
    );
    repo.write("generate.sh", "#!/bin/sh\nprintf 'fresh\\n' > \"$2\"\n");
    repo.write("docs/generated.md", "stale\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "generated"]);

    let output = repo.run(&["drift"]);

    assert_eq!(code(&output), 1, "{}", stderr(&output));
    // The tree is unchanged: the generator wrote to a temporary path.
    assert_eq!(repo.status(), "", "the drift run changed the working tree");
    assert_eq!(repo.read("docs/generated.md"), "stale\n");
}

#[test]
fn the_in_place_generator_leaves_the_tree_byte_and_mode_identical() {
    let repo = Repo::new(&generated_config(&["sh", "generate.sh"], None), None);
    // The generator replaces the file — with a different mode.
    repo.write(
        "generate.sh",
        "#!/bin/sh\nprintf 'produced\\n' > docs/generated.md\nchmod 700 docs/generated.md\n",
    );
    repo.write("docs/generated.md", "original\n");
    #[cfg(unix)]
    set_mode(&repo, "docs/generated.md", 0o644);
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "generated"]);

    let output = repo.run(&["drift"]);

    assert_eq!(code(&output), 1, "{}", stderr(&output));
    // The engine restored the file: byte- and mode-identical.
    assert_eq!(repo.status(), "", "the drift run changed the working tree");
    assert_eq!(repo.read("docs/generated.md"), "original\n");
    #[cfg(unix)]
    assert_eq!(mode_of(&repo, "docs/generated.md"), 0o644);
}

#[test]
fn an_in_place_generator_that_fails_halfway_leaves_the_tree_unchanged() {
    let repo = Repo::new(&generated_config(&["sh", "generate.sh"], None), None);
    // The generator writes the file and then fails.
    repo.write(
        "generate.sh",
        "#!/bin/sh\nprintf 'broken\\n' > docs/generated.md\nexit 3\n",
    );
    repo.write("docs/generated.md", "original\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "generated"]);

    let output = repo.run(&["drift"]);

    // A generator that could not be run is an operational error.
    assert_eq!(code(&output), 2, "{}", stderr(&output));
    assert!(
        stderr(&output).contains("exited with status 3"),
        "{}",
        stderr(&output)
    );
    // The tree is unchanged nevertheless: the restore happens anyway.
    assert_eq!(repo.status(), "", "the drift run changed the working tree");
    assert_eq!(repo.read("docs/generated.md"), "original\n");
}

#[cfg(unix)]
#[test]
fn a_symlinked_generated_file_is_not_written_through() {
    let repo = Repo::new(&generated_config(&["sh", "generate.sh"], None), None);
    repo.write(
        "generate.sh",
        "#!/bin/sh\nprintf 'produced\\n' > docs/generated.md\n",
    );
    repo.write("docs/real.md", "original\n");
    std::os::unix::fs::symlink("real.md", repo.path().join("docs/generated.md"))
        .expect("the link is created");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "symlink"]);

    let output = repo.run(&["drift"]);

    // The committed content of the file behind the link is not what the
    // generator produces: the gate fails.
    assert_eq!(code(&output), 1, "{}", stderr(&output));
    // The file behind the link keeps its content: the engine never writes
    // through a symbolic link.
    assert_eq!(repo.read("docs/real.md"), "original\n");
    // The link is still a link.
    let metadata =
        std::fs::symlink_metadata(repo.path().join("docs/generated.md")).expect("the link exists");
    assert!(metadata.file_type().is_symlink());
    // And the tree is unchanged.
    assert_eq!(repo.status(), "", "the drift run changed the working tree");
}

#[test]
fn a_generator_that_cannot_be_started_is_an_operational_error() {
    let repo = Repo::new(
        &generated_config(&["ratchet-generator-that-does-not-exist"], Some("--output")),
        None,
    );

    let output = repo.run(&["drift"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let text = format!("{}{}", stdout(&output), stderr(&output));
    assert!(
        text.contains("the generator for docs/generated.md could not be started"),
        "{text}"
    );
    assert_deterministic(&text);
}

#[test]
fn the_drift_gates_run_as_part_of_the_check() {
    let config = r#"[
      {
        "rules": {
          "a": {
            "class": "c",
            "incident": "i",
            "unit": "u",
            "direction": "ceiling",
            "count": { "command": ["echo", "1"] }
          }
        },
        "generated": [
          { "path": "docs/generated.md", "command": ["sh", "generate.sh"], "output_arg": "--output" }
        ]
      }
    ]"#;
    let repo = Repo::new(config, Some(&baseline("a", 3)));
    repo.write("generate.sh", "#!/bin/sh\nprintf 'fresh\\n' > \"$2\"\n");
    repo.write("docs/generated.md", "stale\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "generated"]);

    let output = repo.run(&["check", "--format", "json"]);

    assert_eq!(code(&output), 1, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    // The rule passed, the generated file drifted.
    assert_eq!(report["rules"][0]["status"], "ok");
    assert_eq!(report["summary"]["generated"], 1);
    assert_eq!(report["summary"]["drifted"], 1);
    assert_eq!(report["generated"][0]["status"], "drift");
    assert_eq!(report["ok"], false);
}

#[test]
fn two_drift_runs_produce_the_same_bytes() {
    let repo = Repo::new(
        &generated_config(&["sh", "generate.sh"], Some("--output")),
        None,
    );
    repo.write("generate.sh", "#!/bin/sh\nprintf 'fresh\\n' > \"$2\"\n");
    repo.write("docs/generated.md", "stale\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "generated"]);

    let first = repo.run(&["drift"]);
    let second = repo.run(&["drift"]);

    assert_eq!(code(&first), 1, "{}", stderr(&first));
    assert_eq!(stdout(&first), stdout(&second));
    assert_eq!(stderr(&first), stderr(&second));
    let text = format!("{}{}", stdout(&first), stderr(&first));
    assert_deterministic(&text);
}

#[test]
fn the_drift_command_selects_one_generated_file() {
    let config = r#"[
      {
        "rules": {},
        "generated": [
          { "path": "docs/a.md", "command": ["sh", "generate.sh"], "output_arg": "--output" },
          { "path": "docs/b.md", "command": ["sh", "generate.sh"], "output_arg": "--output" }
        ]
      }
    ]"#;
    let repo = Repo::new(config, None);
    repo.write("generate.sh", "#!/bin/sh\nprintf 'fresh\\n' > \"$2\"\n");
    repo.write("docs/a.md", "stale\n");
    repo.write("docs/b.md", "fresh\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "generated"]);

    let output = repo.run(&["drift", "--path", "docs/b.md"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("docs/b.md"), "{text}");
    assert!(!text.contains("docs/a.md"), "{text}");
}

#[test]
fn a_drift_path_without_a_gate_is_an_operational_error() {
    let repo = Repo::new(
        &generated_config(&["sh", "generate.sh"], Some("--output")),
        None,
    );
    repo.write("generate.sh", "#!/bin/sh\nprintf 'fresh\\n' > \"$2\"\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "generator"]);

    let output = repo.run(&["drift", "--path", "docs/typo.md"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    assert!(
        stderr(&output).contains("no drift gate for docs/typo.md"),
        "{}",
        stderr(&output)
    );
}

// --- the tiers ---------------------------------------------------------------

/// A config with two fast rules and one slow rule.
fn tier_config(fast_floor: i64) -> String {
    format!(
        r#"[
          {{
            "tiers": {{ "fast": {{ "floor": {fast_floor} }}, "slow": {{ "floor": 1 }} }},
            "rules": {{
              "a": {{ "class": "c", "incident": "i", "unit": "u", "direction": "ceiling",
                      "tier": "fast", "count": {{ "command": ["echo", "1"] }} }},
              "b": {{ "class": "c", "incident": "i", "unit": "u", "direction": "ceiling",
                      "tier": "fast", "count": {{ "command": ["echo", "1"] }} }},
              "c": {{ "class": "c", "incident": "i", "unit": "u", "direction": "ceiling",
                      "tier": "slow", "count": {{ "command": ["echo", "1"] }} }}
            }}
          }}
        ]"#
    )
}

/// A baseline with three records.
fn three_baselines() -> String {
    "{\n  \"version\": 1,\n  \"rules\": {\n    \"a\": { \"value\": 3 },\n    \"b\": { \"value\": 3 },\n    \"c\": { \"value\": 3 }\n  }\n}\n".to_string()
}

#[test]
fn the_tier_selects_the_rules_of_that_tier() {
    let repo = Repo::new(&tier_config(2), Some(&three_baselines()));

    let output = repo.run(&["check", "--tier", "fast", "--format", "json"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    assert_eq!(report["summary"]["rules"], 2);
    assert_eq!(report["summary"]["executed"], 2);
    let ids: Vec<&str> = report["rules"]
        .as_array()
        .expect("an array")
        .iter()
        .map(|rule| rule["id"].as_str().expect("an id"))
        .collect();
    assert_eq!(ids, vec!["a", "b"]);
}

#[test]
fn the_slow_tier_selects_the_slow_rules() {
    let repo = Repo::new(&tier_config(2), Some(&three_baselines()));

    let output = repo.run(&["check", "--tier", "slow", "--format", "json"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    assert_eq!(report["summary"]["rules"], 1);
    assert_eq!(report["rules"][0]["id"], "c");
}

#[test]
fn a_tier_below_its_floor_fails() {
    // The floor of the fast tier is five, the tier has two rules.
    let repo = Repo::new(&tier_config(5), Some(&three_baselines()));

    let output = repo.run(&["check", "--tier", "fast"]);

    assert_eq!(code(&output), 1, "{}", stderr(&output));
    let text = stderr(&output);
    assert!(text.contains("the tier `fast`"), "{text}");
    assert!(text.contains("the floor is 5"), "{text}");
}

#[test]
fn an_empty_tier_selection_fails_with_a_note_naming_the_tier() {
    let config = r#"[
      {
        "tiers": { "slow": { "floor": 1 } },
        "rules": {
          "a": { "class": "c", "incident": "i", "unit": "u", "direction": "ceiling",
                 "tier": "fast", "count": { "command": ["echo", "1"] } }
        }
      }
    ]"#;
    let repo = Repo::new(config, Some(&baseline("a", 3)));

    let output = repo.run(&["check", "--tier", "slow"]);

    assert_eq!(code(&output), 1, "{}", stderr(&output));
    let text = stderr(&output);
    assert!(text.contains("the tier `slow`"), "{text}");
    assert!(text.contains("empty selection"), "{text}");
    assert!(!stdout(&output).contains("PASS"), "{}", stdout(&output));
}

#[test]
fn a_check_that_ran_and_failed_counts_as_executed() {
    // The counter of the rule fails: the check ran, so the floor is reached.
    let config = r#"[
      {
        "tiers": { "fast": { "floor": 1 } },
        "rules": {
          "a": { "class": "c", "incident": "i", "unit": "u", "direction": "ceiling",
                 "tier": "fast", "count": { "command": ["false"] } }
        }
      }
    ]"#;
    let repo = Repo::new(config, Some(&baseline("a", 3)));

    let output = repo.run(&["check", "--tier", "fast"]);

    // The counter error is an operational error, not a floor failure.
    assert_eq!(code(&output), 2, "{}", stderr(&output));
    assert!(
        stdout(&output).contains("the counter exited with status 1"),
        "{}",
        stdout(&output)
    );
    // The check ran, so the floor of one is reached.
    assert!(
        !stderr(&output).contains("the floor is"),
        "{}",
        stderr(&output)
    );
}

// --- the path exclusion ------------------------------------------------------

#[test]
fn the_ignore_patterns_do_not_turn_a_failing_gate_green() {
    let config = r#"[
      {
        "rules": {
          "a": { "class": "c", "incident": "i", "unit": "u", "direction": "ceiling",
                 "count": { "command": ["echo", "2"] },
                 "files": ["src/**"] }
        }
      }
    ]"#;
    let repo = Repo::new(config, Some(&baseline("a", 1)));
    repo.write("src/main.rs", "fn main() {}");
    repo.write("src/generated/out.rs", "generated");

    let output = repo.run(&["check", "src/"]);

    assert_eq!(code(&output), 1, "{}", stderr(&output));

    // Ignoring one path of the scope does not turn the gate green: the rule
    // still runs on the rest.
    let output = repo.run(&["check", "src/", "--ignore", "src/generated/**"]);

    assert_eq!(code(&output), 1, "{}", stderr(&output));
    assert!(stdout(&output).contains("2 > 1"), "{}", stdout(&output));
}

#[test]
fn a_rule_whose_whole_scope_is_ignored_is_skipped() {
    let config = r#"[
      {
        "rules": {
          "a": { "class": "c", "incident": "i", "unit": "u", "direction": "ceiling",
                 "count": { "command": ["echo", "2"] },
                 "files": ["src/generated/**"] }
        }
      }
    ]"#;
    let repo = Repo::new(config, Some(&baseline("a", 1)));
    repo.write("src/generated/out.rs", "generated");

    let output = repo.run(&["check", "--ignore", "src/generated/**"]);

    // The rule is skipped: nothing is left to run on.  An empty selection is
    // never a pass.
    assert_eq!(code(&output), 1, "{}", stderr(&output));
    assert!(!stdout(&output).contains("PASS"), "{}", stdout(&output));
    assert!(
        stderr(&output).contains("empty selection"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn an_ignored_path_of_the_selection_leaves_nothing_to_run_on() {
    let config = r#"[
      {
        "rules": {
          "a": { "class": "c", "incident": "i", "unit": "u", "direction": "ceiling",
                 "count": { "command": ["echo", "2"] },
                 "files": ["src/**"] }
        }
      }
    ]"#;
    let repo = Repo::new(config, Some(&baseline("a", 1)));
    repo.write("src/main.rs", "fn main() {}");

    let output = repo.run(&["check", "src/", "--ignore", "src/**"]);

    assert_eq!(code(&output), 1, "{}", stderr(&output));
    assert!(!stdout(&output).contains("PASS"), "{}", stdout(&output));
}

#[test]
fn an_invalid_ignore_pattern_exits_two() {
    let repo = Repo::new(ceiling_config(), Some(&baseline("todo-markers", 5)));

    let output = repo.run(&["check", "--ignore", "src/["]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    assert!(stderr(&output).contains("--ignore"), "{}", stderr(&output));
}

#[test]
fn the_diagnostic_channel_shows_the_argv_of_the_drift_generator() {
    let repo = Repo::new(
        &generated_config(&["sh", "generate.sh"], Some("--output")),
        None,
    );
    repo.write("generate.sh", "#!/bin/sh\nprintf 'fresh\\n' > \"$2\"\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "generator"]);

    let output = repo.run(&["drift", "--debug"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let text = stderr(&output);
    // The argv that was really executed — the output path included.
    assert!(text.contains("sh generate.sh --output"), "{text}");
    // And the resolved config.
    assert!(text.contains("resolved config"), "{text}");
}

#[test]
fn a_drift_generator_does_not_relay_its_output() {
    let repo = Repo::new(
        &generated_config(&["sh", "generate.sh"], Some("--output")),
        None,
    );
    // The generator prints the absolute path it was given.
    repo.write(
        "generate.sh",
        "#!/bin/sh\necho \"cannot write to $2\" >&2\nexit 3\n",
    );
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "generator"]);

    let output = repo.run(&["drift"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let text = format!("{}{}", stdout(&output), stderr(&output));
    // The report carries ratchet's own words, never the output of the
    // generator (SPEC 6.2).
    assert!(!text.contains("cannot write to"), "{text}");
    assert!(text.contains("exited with status 3"), "{text}");
    assert_deterministic(&text);
}

#[cfg(unix)]
#[test]
fn a_failing_restore_is_an_operational_error() {
    use std::os::unix::fs::PermissionsExt;

    let repo = Repo::new(&generated_config(&["sh", "generate.sh"], None), None);
    repo.write(
        "generate.sh",
        "#!/bin/sh\nprintf 'produced\\n' > docs/generated.md\n",
    );
    repo.write("docs/generated.md", "original\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "generated"]);

    // The directory of the generated file cannot be written: the restore
    // cannot create its temporary file.
    let directory = repo.path().join("docs");
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o500))
        .expect("the directory is made read-only");
    let output = repo.run(&["drift"]);
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
        .expect("the directory is writable again");

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let text = stderr(&output);
    // The error names the file and says that the tree was modified.
    assert!(
        text.contains("the restore of docs/generated.md failed"),
        "{text}"
    );
    assert!(text.contains("the working tree was modified"), "{text}");
}

#[test]
fn the_fix_writes_a_generated_file_that_is_not_committed_yet() {
    let repo = Repo::new(
        &generated_config(&["sh", "generate.sh"], Some("--output")),
        None,
    );
    repo.write("generate.sh", "#!/bin/sh\nprintf 'fresh\\n' > \"$2\"\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "generator"]);

    let output = repo.run(&["drift", "--fix"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    // The file is in the tree now and can be committed.
    assert_eq!(repo.read("docs/generated.md"), "fresh\n");
    assert!(repo.status().contains("docs"), "{}", repo.status());

    // The status of a file that is not committed is `new` — not a failure.
    let output = repo.run(&["drift", "--format", "json"]);
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    assert_eq!(report["generated"][0]["status"], "new");
}

#[test]
fn a_temporary_output_fix_removes_its_traversal_directory_for_a_root_target() {
    let repo = Repo::new(
        &generated_config_at("a/../x.md", &["sh", "generate.sh"], Some("--output")),
        None,
    );
    repo.write("generate.sh", "#!/bin/sh\nprintf 'fresh\\n' > \"$2\"\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "generator"]);

    let output = repo.run(&["drift", "--fix"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert_eq!(repo.read("x.md"), "fresh\n");
    assert!(
        !repo.path().join("a").exists(),
        "the traversal-only directory was left behind"
    );
}

#[test]
fn an_in_place_fix_keeps_the_resolved_parent_and_removes_traversal_only_dirs() {
    let repo = Repo::new(
        &generated_config_at("a/b/../x.md", &["sh", "generate.sh"], None),
        None,
    );
    repo.write(
        "generate.sh",
        "#!/bin/sh\nmkdir -p a/b\nprintf 'fresh\\n' > a/b/../x.md\n",
    );
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "generator"]);

    let output = repo.run(&["drift", "--fix"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert_eq!(repo.read("a/x.md"), "fresh\n");
    assert!(
        repo.path().join("a").is_dir(),
        "the resolved target parent was removed"
    );
    assert!(
        !repo.path().join("a/b").exists(),
        "the traversal-only directory was left behind"
    );
}

#[test]
fn an_in_place_fix_leaves_a_nonempty_traversal_directory_alone() {
    let repo = Repo::new(
        &generated_config_at("a/../x.md", &["sh", "generate.sh"], None),
        None,
    );
    repo.write(
        "generate.sh",
        "#!/bin/sh\nprintf 'fresh\\n' > a/../x.md\nprintf 'keep\\n' > a/side.txt\n",
    );
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "generator"]);

    let output = repo.run(&["drift", "--fix"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert_eq!(repo.read("x.md"), "fresh\n");
    assert_eq!(repo.read("a/side.txt"), "keep\n");
}

#[test]
fn an_in_place_fix_leaves_an_empty_replacement_traversal_directory_alone() {
    let repo = Repo::new(
        &generated_config_at("a/../x.md", &["sh", "generate.sh"], None),
        None,
    );
    // Keep the old directory at another name so its filesystem ID cannot be
    // reused before cleanup checks the replacement at `a`.
    repo.write(
        "generate.sh",
        "#!/bin/sh\nprintf 'fresh\\n' > a/../x.md\nmv a a-original\nmkdir a\n",
    );
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "generator"]);

    let output = repo.run(&["drift", "--fix"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let replacement = repo.path().join("a");
    assert!(
        replacement.is_dir(),
        "the replacement directory was removed"
    );
    assert!(
        std::fs::read_dir(&replacement)
            .expect("the replacement directory is readable")
            .next()
            .is_none(),
        "the replacement directory is not empty"
    );
    let text = stderr(&output);
    assert!(
        text.contains("directory identity changed; refusing cleanup"),
        "{text}"
    );
    assert!(text.contains("the working tree was modified"), "{text}");
    assert!(repo.path().join("a-original").is_dir());
}

#[test]
fn an_in_place_fix_reports_traversal_cleanup_failures_as_modified_tree_errors() {
    let repo = Repo::new(
        &generated_config_at("a/../x.md", &["sh", "generate.sh"], None),
        None,
    );
    repo.write(
        "generate.sh",
        "#!/bin/sh\nprintf 'fresh\\n' > a/../x.md\nrmdir a\nprintf 'replacement' > a\n",
    );
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "generator"]);

    let output = repo.run(&["drift", "--fix"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let text = stderr(&output);
    assert!(
        text.contains("could not remove created traversal directories: a"),
        "{text}"
    );
    assert!(text.contains("the working tree was modified"), "{text}");
    assert!(
        !text.contains(repo.path().to_str().expect("the root is UTF-8")),
        "the error contains an absolute path: {text}"
    );
}

#[test]
fn the_quiet_flag_prints_only_the_drift_failures() {
    let config = r#"[
      {
        "rules": {},
        "generated": [
          { "path": "docs/a.md", "command": ["sh", "generate.sh"], "output_arg": "--output" },
          { "path": "docs/b.md", "command": ["sh", "generate.sh"], "output_arg": "--output" }
        ]
      }
    ]"#;
    let repo = Repo::new(config, None);
    repo.write("generate.sh", "#!/bin/sh\nprintf 'fresh\\n' > \"$2\"\n");
    repo.write("docs/a.md", "stale\n");
    repo.write("docs/b.md", "fresh\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "generated"]);

    let output = repo.run(&["drift", "--quiet"]);

    assert_eq!(code(&output), 1, "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("docs/a.md"), "{text}");
    assert!(!text.contains("docs/b.md"), "{text}");
    assert!(!text.contains("ratchet —"), "{text}");
}

#[test]
fn the_check_can_fix_the_generated_files() {
    let config = r#"[
      {
        "rules": {
          "a": {
            "class": "c",
            "incident": "i",
            "unit": "u",
            "direction": "ceiling",
            "count": { "command": ["echo", "1"] }
          }
        },
        "generated": [
          { "path": "docs/generated.md", "command": ["sh", "generate.sh"], "output_arg": "--output" }
        ]
      }
    ]"#;
    let repo = Repo::new(config, Some(&baseline("a", 3)));
    repo.write("generate.sh", "#!/bin/sh\nprintf 'fresh\\n' > \"$2\"\n");
    repo.write("docs/generated.md", "stale\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "generated"]);

    let output = repo.run(&["check", "--fix"]);

    // The drift gate stays failed: the committed file is still stale.
    assert_eq!(code(&output), 1, "{}", stderr(&output));
    assert_eq!(repo.read("docs/generated.md"), "fresh\n");
    assert_eq!(repo.status(), " M docs/generated.md\n");
}

#[test]
fn a_generated_file_outside_the_repository_root_is_an_operational_error() {
    let config = r#"[
      {
        "rules": {},
        "generated": [
          { "path": "../outside.md", "command": ["sh", "generate.sh"], "output_arg": "--output" }
        ]
      }
    ]"#;
    let repo = Repo::new(config, None);
    repo.write("generate.sh", "#!/bin/sh\nprintf 'fresh\\n' > \"$2\"\n");

    let output = repo.run(&["drift"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    assert!(
        stderr(&output).contains("must lie inside the repository root"),
        "{}",
        stderr(&output)
    );
}

// --- the fix round -----------------------------------------------------------

#[test]
fn a_slow_generated_file_is_skipped_in_the_fast_tier() {
    let config = r#"[
      {
        "tiers": { "fast": { "floor": 1 } },
        "rules": {
          "a": { "class": "c", "incident": "i", "unit": "u", "direction": "ceiling",
                 "tier": "fast", "count": { "command": ["echo", "1"] } }
        },
        "generated": [
          { "path": "docs/slow.md", "command": ["sh", "slow.sh"], "output_arg": "--output", "tier": "slow" }
        ]
      }
    ]"#;
    let repo = Repo::new(config, Some(&baseline("a", 3)));
    repo.write("slow.sh", "#!/bin/sh\nprintf 'slow\\n' > \"$2\"\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "generator"]);

    // The fast tier skips the slow generator: a fast tier that waits for a
    // slow generator is not fast.
    let output = repo.run(&["check", "--tier", "fast", "--format", "json"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    assert_eq!(report["summary"]["generated"], 0);
    // The note says why the gate did not run.
    let notes = report["notes"].as_array().expect("the notes are an array");
    assert!(
        notes.iter().any(|note| {
            note.as_str()
                .unwrap_or_default()
                .contains("the drift gate docs/slow.md is skipped")
        }),
        "{notes:?}"
    );

    // Without `--tier` the slow generator runs.
    let output = repo.run(&["check", "--format", "json"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    assert_eq!(report["summary"]["generated"], 1);
    assert_eq!(report["generated"][0]["path"], "docs/slow.md");
}

#[test]
fn a_generated_file_without_a_tier_is_a_fast_one() {
    let config = r#"[
      {
        "rules": {
          "a": { "class": "c", "incident": "i", "unit": "u", "direction": "ceiling",
                 "tier": "fast", "count": { "command": ["echo", "1"] } }
        },
        "generated": [
          { "path": "docs/fast.md", "command": ["sh", "fast.sh"], "output_arg": "--output" }
        ]
      }
    ]"#;
    let repo = Repo::new(config, Some(&baseline("a", 3)));
    repo.write("fast.sh", "#!/bin/sh\nprintf 'fast\\n' > \"$2\"\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "generator"]);

    // The entry has no tier: it is a fast one and runs in the fast tier.
    let output = repo.run(&["check", "--tier", "fast", "--format", "json"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    assert_eq!(report["summary"]["generated"], 1);
}

#[test]
fn the_ignore_patterns_do_not_exclude_a_drift_gate() {
    let config = r#"[
      {
        "rules": {
          "a": { "class": "c", "incident": "i", "unit": "u", "direction": "ceiling",
                 "count": { "command": ["echo", "1"] } }
        },
        "generated": [
          { "path": "docs/generated.md", "command": ["sh", "generate.sh"], "output_arg": "--output" }
        ]
      }
    ]"#;
    let repo = Repo::new(config, Some(&baseline("a", 3)));
    repo.write("generate.sh", "#!/bin/sh\nprintf 'fresh\\n' > \"$2\"\n");
    repo.write("docs/generated.md", "stale\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "generated"]);

    // The pattern matches the generated file — and the drift gate runs anyway:
    // a generated file is registered deliberately.
    let output = repo.run(&["check", "--ignore", "docs/**", "--format", "json"]);

    assert_eq!(code(&output), 1, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    assert_eq!(report["summary"]["generated"], 1);
    assert_eq!(report["summary"]["drifted"], 1);
    assert_eq!(report["generated"][0]["status"], "drift");
}

#[test]
fn without_a_tier_the_floor_of_every_declared_tier_applies() {
    // The floor of the fast tier is five, the tier has two rules.
    let repo = Repo::new(&tier_config(5), Some(&three_baselines()));

    let output = repo.run(&["check", "--format", "json"]);

    assert_eq!(code(&output), 1, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    // The JSON says why the run is not green.
    let notes = report["notes"].as_array().expect("the notes are an array");
    assert!(
        notes.iter().any(|note| {
            let note = note.as_str().unwrap_or_default();
            note.contains("the tier `fast`") && note.contains("the floor is 5")
        }),
        "{notes:?}"
    );
    // The same note is on stderr.
    assert!(
        stderr(&output).contains("the floor is 5"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn a_declared_tier_without_a_rule_is_the_empty_selection() {
    let config = r#"[
      {
        "tiers": { "slow": { "floor": 1 } },
        "rules": {
          "a": { "class": "c", "incident": "i", "unit": "u", "direction": "ceiling",
                 "tier": "fast", "count": { "command": ["echo", "1"] } }
        }
      }
    ]"#;
    let repo = Repo::new(config, Some(&baseline("a", 3)));

    // The slow tier is declared and has no rule: it is the empty selection
    // and fails, even though the fast tier is green.
    let output = repo.run(&["check"]);

    assert_eq!(code(&output), 1, "{}", stderr(&output));
    let text = stderr(&output);
    assert!(text.contains("the tier `slow`"), "{text}");
    assert!(text.contains("empty selection"), "{text}");
}

#[test]
fn the_json_report_carries_the_notes() {
    let repo = Repo::new(ceiling_config(), Some(&baseline("todo-markers", 5)));

    let output = repo.run(&["check", "--format", "json"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    // The array is there — and empty when there is nothing to say.
    let notes = report["notes"].as_array().expect("the notes are an array");
    assert!(notes.is_empty(), "{notes:?}");
}

#[test]
fn the_json_report_carries_the_notes_of_a_failing_run() {
    let repo = Repo::new(ceiling_config(), None);

    let output = repo.run(&["check", "--format", "json"]);

    assert_eq!(code(&output), 1, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    let notes = report["notes"].as_array().expect("the notes are an array");
    assert!(
        notes
            .iter()
            .any(|note| note.as_str().unwrap_or_default().contains("baseline init")),
        "{notes:?}"
    );
}

// --- the floor counts the drift gates too ------------------------------------

/// A config with one fast rule, one fast generator and one slow generator.
fn tier_with_generators_config(fast_floor: i64) -> String {
    format!(
        r#"[
          {{
            "tiers": {{ "fast": {{ "floor": {fast_floor} }} }},
            "rules": {{
              "a": {{ "class": "c", "incident": "i", "unit": "u", "direction": "ceiling",
                      "tier": "fast", "count": {{ "command": ["echo", "1"] }} }}
            }},
            "generated": [
              {{ "path": "docs/fast.md", "command": ["sh", "fast.sh"], "output_arg": "--output" }},
              {{ "path": "docs/slow.md", "command": ["sh", "slow.sh"], "output_arg": "--output", "tier": "slow" }}
            ]
          }}
        ]"#
    )
}

/// A repository with the two generators of the config above.
fn repo_with_generators(config: &str) -> Repo {
    let repo = Repo::new(config, Some(&baseline("a", 3)));
    repo.write("fast.sh", "#!/bin/sh\nprintf 'fast\\n' > \"$2\"\n");
    repo.write("slow.sh", "#!/bin/sh\nprintf 'slow\\n' > \"$2\"\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "generators"]);
    repo
}

#[test]
fn a_drift_gate_does_not_count_towards_the_floor_of_its_tier() {
    // One rule and one fast generator: the floor of one is met by the rule.
    let repo = repo_with_generators(&tier_with_generators_config(1));

    let output = repo.run(&["check", "--format", "json"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    // The floor is met: no note says otherwise.
    let notes = report["notes"].as_array().expect("the notes are an array");
    assert!(notes.is_empty(), "{notes:?}");
}

#[test]
fn the_floor_counts_the_rules_only() {
    // One rule and two generators: the floor of two is not met — a drift gate
    // is not a check (SPEC 3.3).
    let repo = repo_with_generators(&tier_with_generators_config(2));

    let output = repo.run(&["check", "--format", "json"]);

    assert_eq!(code(&output), 1, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    // The note names the tier and the floor — and the number it counted.
    let notes = report["notes"].as_array().expect("the notes are an array");
    assert!(
        notes.iter().any(|note| {
            let note = note.as_str().unwrap_or_default();
            note.contains("the tier `fast`") && note.contains("the floor is 2")
        }),
        "{notes:?}"
    );
    assert!(
        notes.iter().any(|note| note
            .as_str()
            .unwrap_or_default()
            .contains("executed 1 check")),
        "{notes:?}"
    );
    // The same note is on stderr.
    assert!(
        stderr(&output).contains("the floor is 2"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn a_slow_generator_does_not_count_towards_the_floor_of_the_slow_tier() {
    // The fast floor is three: the rule is one check, and the generators are
    // not checks at all (SPEC 3.3).
    let repo = repo_with_generators(&tier_with_generators_config(3));

    let output = repo.run(&["check", "--tier", "fast", "--format", "json"]);

    assert_eq!(code(&output), 1, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    let notes = report["notes"].as_array().expect("the notes are an array");
    // The fast tier is short of its floor: the rule is one check.
    assert!(
        notes.iter().any(|note| {
            let note = note.as_str().unwrap_or_default();
            note.contains("the tier `fast`") && note.contains("the floor is 3")
        }),
        "{notes:?}"
    );

    // Without `--tier` the slow generator runs — and the slow tier holds no
    // rule: it is not a tier that runs.
    let output = repo.run(&["check", "--format", "json"]);

    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    let notes = report["notes"].as_array().expect("the notes are an array");
    assert!(
        !notes.iter().any(|note| note
            .as_str()
            .unwrap_or_default()
            .contains("the tier `slow`")),
        "{notes:?}"
    );
    // The fast tier is still short of its floor: one check, not three.
    assert!(
        notes.iter().any(|note| {
            let note = note.as_str().unwrap_or_default();
            note.contains("the tier `fast`") && note.contains("executed 1 check")
        }),
        "{notes:?}"
    );
}

// --- the findings of the verification round ----------------------------------

/// True when `patch` is on the PATH.
#[cfg(unix)]
fn patch_available() -> bool {
    Command::new("patch")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// The object a committed path points at.
fn head_object(repo: &Repo, path: &str) -> String {
    let output = Command::new("git")
        .args(["ls-tree", "HEAD", "--", path])
        .current_dir(repo.path())
        .output()
        .expect("git runs");
    let text = String::from_utf8_lossy(&output.stdout);
    text.split_whitespace()
        .nth(2)
        .expect("the tree entry has an object")
        .to_string()
}

/// Removes the loose object of a committed path.
fn remove_loose_object(repo: &Repo, path: &str) {
    let sha = head_object(repo, path);
    let object = repo
        .path()
        .join(".git/objects")
        .join(&sha[..2])
        .join(&sha[2..]);
    std::fs::remove_file(&object).expect("the loose object is removed");
}

#[test]
fn a_committed_file_that_cannot_be_read_is_an_operational_error() {
    let repo = Repo::new(
        &generated_config(&["sh", "generate.sh"], Some("--output")),
        None,
    );
    repo.write("generate.sh", "#!/bin/sh\nprintf 'fresh\\n' > \"$2\"\n");
    repo.write("docs/generated.md", "stale\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "generated"]);

    // The file is in the committed tree, but its blob is gone: a partial
    // clone or a damaged repository.
    remove_loose_object(&repo, "docs/generated.md");

    let output = repo.run(&["drift", "--format", "json"]);

    // The gate does not go green: a broken reference must not disable it.
    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let text = format!("{}{}", stdout(&output), stderr(&output));
    assert!(
        text.contains("the committed file docs/generated.md cannot be read"),
        "{text}"
    );
}

#[test]
fn a_directory_in_the_committed_tree_is_an_operational_error() {
    let config = r#"[
      {
        "rules": {},
        "generated": [
          { "path": "docs", "command": ["sh", "generate.sh"], "output_arg": "--output" }
        ]
      }
    ]"#;
    let repo = Repo::new(config, None);
    repo.write("generate.sh", "#!/bin/sh\nprintf 'fresh\\n' > \"$2\"\n");
    repo.write("docs/inner.md", "inner\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "generated"]);

    let output = repo.run(&["drift"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let text = stderr(&output);
    assert!(
        text.contains("the committed path docs is a directory, not a file"),
        "{text}"
    );
}

#[test]
fn a_difference_that_is_only_the_trailing_newline_is_shown() {
    let repo = Repo::new(
        &generated_config(&["sh", "generate.sh"], Some("--output")),
        None,
    );
    // The generator produces the same lines — without the newline at the end.
    repo.write("generate.sh", "#!/bin/sh\nprintf 'one\\ntwo' > \"$2\"\n");
    repo.write("docs/generated.md", "one\ntwo\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "generated"]);

    let output = repo.run(&["drift", "--format", "json"]);

    // The gate fails: the files differ.
    assert_eq!(code(&output), 1, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    let diff = report["generated"][0]["diff"]
        .as_str()
        .expect("the drift entry has a diff");
    // The difference shows up: the marker stands behind the line it describes.
    assert!(diff.contains("\\ No newline at end of file"), "{diff}");
    assert!(diff.contains("-two"), "{diff}");
    assert!(diff.contains("+two"), "{diff}");
    assert!(diff.contains("@@"), "{diff}");
}

#[cfg(unix)]
#[test]
fn the_diff_is_a_patch_that_patch_accepts() {
    let repo = Repo::new(
        &generated_config(&["sh", "generate.sh"], Some("--output")),
        None,
    );
    repo.write("generate.sh", "#!/bin/sh\nprintf 'one\\ntwo' > \"$2\"\n");
    repo.write("docs/generated.md", "one\ntwo\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "generated"]);

    let output = repo.run(&["drift", "--format", "json"]);
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    let diff = report["generated"][0]["diff"]
        .as_str()
        .expect("the drift entry has a diff");

    if !patch_available() {
        eprintln!(
            "*** SKIPPED: patch is not on the PATH — this test did NOT check that the \
             diff is a patch that patch(1) accepts. ***"
        );
        return;
    }

    // The diff is a patch: patch(1) applies it to the committed file.
    let patch_file = repo.path().join("the.patch");
    std::fs::write(&patch_file, diff).expect("the patch is written");
    let status = Command::new("patch")
        .args(["-p0", "-s", "-i", "the.patch"])
        .current_dir(repo.path())
        .status()
        .expect("patch runs");
    assert!(status.success(), "patch rejected the diff:\n{diff}");

    // The file carries the produced content now.
    assert_eq!(
        std::fs::read(repo.path().join("docs/generated.md")).expect("the file is readable"),
        b"one\ntwo"
    );
}

#[test]
fn a_project_whose_only_gate_is_a_drift_gate_is_the_empty_selection() {
    // A drift gate is not a check (SPEC 3.3): a project without a rule has
    // no check at all, and the empty selection is never a pass.
    let config = r#"[
      {
        "tiers": { "fast": { "floor": 1 } },
        "rules": {},
        "generated": [
          { "path": "docs/generated.md", "command": ["sh", "generate.sh"], "output_arg": "--output" }
        ]
      }
    ]"#;
    let repo = Repo::new(config, None);
    repo.write("generate.sh", "#!/bin/sh\nprintf 'fresh\\n' > \"$2\"\n");
    repo.write("docs/generated.md", "fresh\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "generated"]);

    let output = repo.run(&["check", "--format", "json"]);

    assert_eq!(code(&output), 1, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    assert_eq!(report["ok"], false);
    // The drift gate ran — and it is not a check.
    assert_eq!(report["summary"]["generated"], 1);
    assert_eq!(report["summary"]["executed"], 0);
    // The notes say why the run is not green.
    let notes = report["notes"].as_array().expect("the notes are an array");
    assert!(
        notes.iter().any(|note| note
            .as_str()
            .unwrap_or_default()
            .contains("empty selection")),
        "{notes:?}"
    );
}

#[test]
fn a_drift_command_without_a_generated_entry_fails() {
    let repo = Repo::new("[]", None);

    let output = repo.run(&["drift"]);

    // An empty selection is never a pass.
    assert_eq!(code(&output), 1, "{}", stderr(&output));
    assert!(
        stderr(&output).contains("empty selection"),
        "{}",
        stderr(&output)
    );
    // The verdict line agrees with the exit code: it must not say COHERENT
    // while the run fails.
    let text = stdout(&output);
    assert!(text.contains("NOT COHERENT"), "{text}");
    assert!(!text.contains("\nCOHERENT —"), "{text}");
}

#[test]
fn a_generator_that_replaces_the_file_is_not_mistaken_for_one_that_did_nothing() {
    let repo = Repo::new(&generated_config(&["node", "generate.js"], None), None);
    // The reproducible-build pattern: the file is replaced by a copy that
    // keeps the time of the original.
    repo.write(
        "generate.js",
        "const fs = require('node:fs');\nconst source = 'docs/generated.md';\nconst temporary = source + '.tmp';\nconst stat = fs.statSync(source);\nfs.copyFileSync(source, temporary);\nfs.utimesSync(temporary, stat.atime, stat.mtime);\nfs.renameSync(temporary, source);\n",
    );
    repo.write("docs/generated.md", "original\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "generated"]);

    let output = repo.run(&["drift"]);

    // The generator did its job: the gate is green, not an operational error.
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert!(stdout(&output).contains("COHERENT"), "{}", stdout(&output));
    // The tree is unchanged: the restore put the original bytes back.
    assert_eq!(repo.status(), "", "the drift run changed the working tree");
    assert_eq!(repo.read("docs/generated.md"), "original\n");
}

#[test]
fn a_generator_that_writes_nothing_is_an_operational_error() {
    let repo = Repo::new(&generated_config(&["sh", "generate.sh"], None), None);
    // The generator succeeds and leaves the file alone.
    repo.write("generate.sh", "#!/bin/sh\nexit 0\n");
    repo.write("docs/generated.md", "original\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "generated"]);

    let output = repo.run(&["drift"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let text = stderr(&output);
    assert!(
        text.contains("the generator for docs/generated.md produced nothing"),
        "{text}"
    );
    // The tree is unchanged nevertheless.
    assert_eq!(repo.status(), "", "the drift run changed the working tree");
    assert_eq!(repo.read("docs/generated.md"), "original\n");
}

#[test]
fn the_summary_counts_the_rules_as_executed_checks() {
    let config = r#"[
      {
        "rules": {
          "a": { "class": "c", "incident": "i", "unit": "u", "direction": "ceiling",
                 "count": { "command": ["echo", "1"] } }
        },
        "generated": [
          { "path": "docs/a.md", "command": ["sh", "generate.sh"], "output_arg": "--output" },
          { "path": "docs/b.md", "command": ["sh", "generate.sh"], "output_arg": "--output" }
        ]
      }
    ]"#;
    let repo = Repo::new(config, Some(&baseline("a", 3)));
    repo.write("generate.sh", "#!/bin/sh\nprintf 'fresh\\n' > \"$2\"\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "generator"]);

    let output = repo.run(&["check", "--format", "json"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    // One rule and two drift gates: the drift gates are not checks (SPEC
    // 3.3), so one check ran.
    assert_eq!(report["summary"]["rules"], 1);
    assert_eq!(report["summary"]["generated"], 2);
    assert_eq!(report["summary"]["executed"], 1);
}

// --- the damaged repository --------------------------------------------------

/// The commit the repository is at.
fn head_commit(repo: &Repo) -> String {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo.path())
        .output()
        .expect("git runs");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Removes the loose object with the given name.
fn remove_loose(repo: &Repo, sha: &str) {
    let object = repo
        .path()
        .join(".git/objects")
        .join(&sha[..2])
        .join(&sha[2..]);
    std::fs::remove_file(&object).expect("the loose object is removed");
}

#[test]
fn a_damaged_repository_is_an_operational_error() {
    let repo = Repo::new(
        &generated_config(&["sh", "generate.sh"], Some("--output")),
        None,
    );
    repo.write("generate.sh", "#!/bin/sh\nprintf 'fresh\\n' > \"$2\"\n");
    repo.write("docs/generated.md", "stale\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "generated"]);

    // The commit object is gone: HEAD cannot be resolved, and the ref that
    // names it is still there.  The repository is damaged, not empty.
    let commit = head_commit(&repo);
    remove_loose(&repo, &commit);

    let output = repo.run(&["drift", "--format", "json"]);

    // The gate does not go green.
    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let text = format!("{}{}", stdout(&output), stderr(&output));
    assert!(
        text.contains("the committed file docs/generated.md cannot be read"),
        "{text}"
    );
}

#[test]
fn an_unborn_head_has_nothing_committed_yet() {
    let repo = Repo::new(
        &generated_config(&["sh", "generate.sh"], Some("--output")),
        None,
    );
    repo.write("generate.sh", "#!/bin/sh\nprintf 'fresh\\n' > \"$2\"\n");
    repo.write("docs/generated.md", "stale\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "generated"]);

    // HEAD points at a branch that has no commit yet, while another branch
    // does: the decision is made on the ref HEAD points to, and that ref
    // does not exist — nothing is committed on it.
    repo.git(&["checkout", "--orphan", "feature"]);

    let output = repo.run(&["drift", "--format", "json"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    assert_eq!(report["generated"][0]["status"], "new");
    assert_eq!(report["ok"], true);
}

#[test]
fn a_detached_head_with_a_broken_commit_is_an_operational_error() {
    let repo = Repo::new(
        &generated_config(&["sh", "generate.sh"], Some("--output")),
        None,
    );
    repo.write("generate.sh", "#!/bin/sh\nprintf 'fresh\\n' > \"$2\"\n");
    repo.write("docs/generated.md", "stale\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "generated"]);

    // A detached HEAD points at a commit: the commit object is gone.
    repo.git(&["checkout", "--detach", "HEAD"]);
    let commit = head_commit(&repo);
    remove_loose(&repo, &commit);

    let output = repo.run(&["drift"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let text = stderr(&output);
    assert!(
        text.contains("the committed file docs/generated.md cannot be read"),
        "{text}"
    );
}

#[test]
fn a_repository_without_a_commit_has_nothing_committed_yet() {
    let repo = Repo::plain(
        &generated_config(&["sh", "generate.sh"], Some("--output")),
        None,
    );
    repo.git(&["init", "-q", "."]);
    repo.write("generate.sh", "#!/bin/sh\nprintf 'fresh\\n' > \"$2\"\n");
    repo.write("docs/generated.md", "stale\n");

    let output = repo.run(&["drift", "--format", "json"]);

    // Nothing was ever committed: the file is new, not a failure.
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    assert_eq!(report["generated"][0]["status"], "new");
    assert_eq!(report["ok"], true);
}

// --- the deliberate raise ----------------------------------------------------

/// A config with a ceiling rule and a floor rule.
fn two_directions_config() -> &'static str {
    r#"[
      {
        "rules": {
          "todo-markers": {
            "class": "unfinished work",
            "incident": "a TODO marker was merged without a tracking issue",
            "unit": "markers",
            "direction": "ceiling",
            "count": { "command": ["echo", "3"] }
          },
          "tests": {
            "class": "coverage",
            "incident": "a test was deleted",
            "unit": "tests",
            "direction": "floor",
            "count": { "command": ["echo", "3"] }
          }
        }
      }
    ]"#
}

/// A baseline with two records.
fn two_records_baseline() -> String {
    "{\n  \"version\": 1,\n  \"rules\": {\n    \"tests\": { \"value\": 5 },\n    \"todo-markers\": { \"value\": 5 }\n  }\n}\n".to_string()
}

/// The baseline of the repository, parsed.
fn baseline_of(repo: &Repo) -> Value {
    serde_json::from_str(&repo.read("ratchet.baseline.json")).expect("the baseline is JSON")
}

#[test]
fn baseline_raise_writes_the_value_and_the_reason() {
    let repo = Repo::new(two_directions_config(), Some(&two_records_baseline()));

    let output = repo.run(&[
        "baseline",
        "raise",
        "--rule",
        "todo-markers",
        "--value",
        "7",
        "--reason",
        "the self-update needs two more crates",
    ]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let baseline = baseline_of(&repo);
    assert_eq!(baseline["rules"]["todo-markers"]["value"], 7);
    assert_eq!(
        baseline["rules"]["todo-markers"]["reason"],
        "the self-update needs two more crates"
    );
    // The other rule is untouched.
    assert_eq!(baseline["rules"]["tests"]["value"], 5);
    assert!(baseline["rules"]["tests"].get("reason").is_none());
    // The command touches the baseline file and nothing else.
    assert_eq!(repo.status(), " M ratchet.baseline.json\n");
}

#[test]
fn baseline_raise_without_a_reason_is_refused() {
    let repo = Repo::new(two_directions_config(), Some(&two_records_baseline()));
    let before = repo.read("ratchet.baseline.json");

    let output = repo.run(&[
        "baseline",
        "raise",
        "--rule",
        "todo-markers",
        "--value",
        "7",
    ]);

    assert_eq!(code(&output), 1, "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("refusing: a raise needs a reason"), "{text}");
    // The baseline file is byte-identical.
    assert_eq!(repo.read("ratchet.baseline.json"), before);
    assert_eq!(repo.status(), "");
}

#[test]
fn baseline_raise_refuses_the_paydown_direction() {
    let repo = Repo::new(two_directions_config(), Some(&two_records_baseline()));
    let before = repo.read("ratchet.baseline.json");

    // A ceiling that goes down is the paydown's job.
    let output = repo.run(&[
        "baseline",
        "raise",
        "--rule",
        "todo-markers",
        "--value",
        "3",
        "--reason",
        "because",
    ]);

    assert_eq!(code(&output), 1, "{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.contains("that is the paydown — use `ratchet baseline update`"),
        "{text}"
    );
    assert_eq!(repo.read("ratchet.baseline.json"), before);

    // A floor that goes up is the paydown's job as well.
    let output = repo.run(&[
        "baseline", "raise", "--rule", "tests", "--value", "7", "--reason", "because",
    ]);

    assert_eq!(code(&output), 1, "{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.contains("that is the paydown — use `ratchet baseline update`"),
        "{text}"
    );
    assert_eq!(repo.read("ratchet.baseline.json"), before);
}

#[test]
fn baseline_raise_lowers_a_floor_deliberately() {
    // A floor may only increase: the move against that direction is a
    // deliberate lowering.
    let repo = Repo::new(two_directions_config(), Some(&two_records_baseline()));

    let output = repo.run(&[
        "baseline",
        "raise",
        "--rule",
        "tests",
        "--value",
        "3",
        "--reason",
        "the floor was written off",
    ]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let baseline = baseline_of(&repo);
    assert_eq!(baseline["rules"]["tests"]["value"], 3);
    assert_eq!(
        baseline["rules"]["tests"]["reason"],
        "the floor was written off"
    );
    // The ceiling is untouched.
    assert_eq!(baseline["rules"]["todo-markers"]["value"], 5);
}

#[test]
fn baseline_raise_needs_a_rule() {
    let repo = Repo::new(ceiling_config(), Some(&baseline("todo-markers", 5)));

    let output = repo.run(&["baseline", "raise", "--value", "7", "--reason", "because"]);

    assert_eq!(code(&output), 2);
    assert!(stderr(&output).contains("needs a rule"));
}

#[test]
fn baseline_raise_of_an_unknown_rule_exits_two() {
    let repo = Repo::new(ceiling_config(), Some(&baseline("todo-markers", 5)));

    let output = repo.run(&[
        "baseline", "raise", "--rule", "nope", "--value", "7", "--reason", "because",
    ]);

    assert_eq!(code(&output), 2);
    assert!(stderr(&output).contains("unknown rule: nope"));
}

#[test]
fn baseline_raise_of_a_rule_without_a_baseline_is_refused() {
    let repo = Repo::new(ceiling_config(), None);

    let output = repo.run(&[
        "baseline",
        "raise",
        "--rule",
        "todo-markers",
        "--value",
        "7",
        "--reason",
        "because",
    ]);

    assert_eq!(code(&output), 1, "{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.contains("refusing: no baseline for todo-markers — run ratchet baseline init"),
        "{text}"
    );
    assert!(!repo.path().join("ratchet.baseline.json").exists());
}

#[test]
fn the_json_report_names_the_raise_command() {
    let repo = Repo::new(ceiling_config(), Some(&baseline("todo-markers", 5)));

    let output = repo.run(&[
        "baseline",
        "raise",
        "--rule",
        "todo-markers",
        "--value",
        "7",
        "--reason",
        "because",
        "--format",
        "json",
    ]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    assert_eq!(report["baseline"]["command"], "raise");
    assert_eq!(report["ok"], true);
    assert_eq!(report["rules"][0]["action"], "updated");
    assert_eq!(report["rules"][0]["value"], 7);
}

#[test]
fn the_monotonicity_check_accepts_a_raise_with_a_reason() {
    let repo = Repo::new(ceiling_config(), Some(&baseline("todo-markers", 5)));
    // The working tree raises the baseline and says why.
    repo.write(
        "ratchet.baseline.json",
        "{\n  \"version\": 1,\n  \"rules\": {\n    \"todo-markers\": { \"value\": 7, \"reason\": \"the self-update needs two more crates\" }\n  }\n}\n",
    );

    let output = repo.run(&["check"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    // The report names the reason: it is in the baseline file, so it is in the
    // diff.
    let text = format!("{}{}", stdout(&output), stderr(&output));
    assert!(
        text.contains("the baseline was raised deliberately, 5 → 7"),
        "{text}"
    );
    assert!(
        text.contains("the self-update needs two more crates"),
        "{text}"
    );

    // The note is in the JSON report too: a machine consumer must not have to
    // read stderr to learn why the run is green (SPEC 6.3).
    let output = repo.run(&["check", "--format", "json"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    let notes: Vec<&str> = report["notes"]
        .as_array()
        .expect("an array")
        .iter()
        .map(|note| note.as_str().expect("a string"))
        .collect();
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(
        notes[0].contains("the baseline was raised deliberately, 5 → 7"),
        "{notes:?}"
    );
    assert!(
        notes[0].contains("the self-update needs two more crates"),
        "{notes:?}"
    );
}

#[test]
fn the_monotonicity_check_rejects_a_raise_without_a_reason() {
    let repo = Repo::new(ceiling_config(), Some(&baseline("todo-markers", 5)));
    // The working tree raises the baseline without saying why.
    repo.write("ratchet.baseline.json", &baseline("todo-markers", 7));

    let output = repo.run(&["check", "--format", "json"]);

    assert_eq!(code(&output), 1, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    assert_eq!(report["rules"][0]["status"], "baseline-raised");
}

#[test]
fn the_monotonicity_check_accepts_a_lowered_floor_with_a_reason() {
    let repo = Repo::new(floor_config(), Some(&baseline("tests", 5)));
    repo.write(
        "ratchet.baseline.json",
        "{\n  \"version\": 1,\n  \"rules\": {\n    \"tests\": { \"value\": 3, \"reason\": \"the floor was written off\" }\n  }\n}\n",
    );

    let output = repo.run(&["check"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let text = format!("{}{}", stdout(&output), stderr(&output));
    assert!(
        text.contains("the baseline was lowered deliberately, 5 → 3"),
        "{text}"
    );
    assert!(text.contains("the floor was written off"), "{text}");
}

#[test]
fn the_monotonicity_check_rejects_a_lowered_floor_without_a_reason() {
    let repo = Repo::new(floor_config(), Some(&baseline("tests", 5)));
    repo.write("ratchet.baseline.json", &baseline("tests", 3));

    let output = repo.run(&["check", "--format", "json"]);

    assert_eq!(code(&output), 1, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    assert_eq!(report["rules"][0]["status"], "baseline-lowered");
}

#[test]
fn the_paydown_still_refuses_the_forbidden_direction() {
    // A ceiling whose count is above the baseline: the paydown may not raise
    // it.
    let ceiling = Repo::new(ceiling_config(), Some(&baseline("todo-markers", 2)));
    let before = ceiling.read("ratchet.baseline.json");

    let output = ceiling.run(&["baseline", "update", "--rule", "todo-markers"]);

    assert_eq!(code(&output), 1, "{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.contains("refusing: count 3 > baseline 2 — a ceiling may only decrease"),
        "{text}"
    );
    assert_eq!(ceiling.read("ratchet.baseline.json"), before);

    // A floor whose count is below the baseline: the paydown may not lower it.
    let floor = Repo::new(floor_config(), Some(&baseline("tests", 5)));
    let before = floor.read("ratchet.baseline.json");

    let output = floor.run(&["baseline", "update", "--rule", "tests"]);

    assert_eq!(code(&output), 1, "{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.contains("refusing: count 3 < baseline 5 — a floor may only increase"),
        "{text}"
    );
    assert_eq!(floor.read("ratchet.baseline.json"), before);
}

#[test]
fn baseline_raise_of_a_negative_value_is_refused() {
    let repo = Repo::new(ceiling_config(), Some(&baseline("todo-markers", 5)));
    let before = repo.read("ratchet.baseline.json");

    let output = repo.run(&[
        "baseline",
        "raise",
        "--rule",
        "todo-markers",
        "--value",
        "-3",
        "--reason",
        "because",
    ]);

    assert_eq!(code(&output), 1, "{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.contains("refusing: a baseline cannot be negative"),
        "{text}"
    );
    // The baseline file is untouched.
    assert_eq!(repo.read("ratchet.baseline.json"), before);
}

#[test]
fn baseline_raise_with_an_empty_reason_is_refused() {
    let repo = Repo::new(ceiling_config(), Some(&baseline("todo-markers", 5)));
    let before = repo.read("ratchet.baseline.json");

    let output = repo.run(&[
        "baseline",
        "raise",
        "--rule",
        "todo-markers",
        "--value",
        "7",
        "--reason",
        "",
    ]);

    assert_eq!(code(&output), 1, "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("refusing: a raise needs a reason"), "{text}");
    // The baseline file is untouched.
    assert_eq!(repo.read("ratchet.baseline.json"), before);
}

#[test]
fn a_deliberate_raise_does_not_excuse_an_unreasoned_raise_of_another_rule() {
    let repo = Repo::new(two_directions_config(), Some(&two_records_baseline()));
    // The ceiling is raised deliberately; the floor is lowered without a
    // reason.
    repo.write(
        "ratchet.baseline.json",
        "{\n  \"version\": 1,\n  \"rules\": {\n    \"tests\": { \"value\": 3 },\n    \"todo-markers\": { \"value\": 7, \"reason\": \"the self-update needs two more crates\" }\n  }\n}\n",
    );

    let output = repo.run(&["check", "--format", "json"]);

    // The reason of one rule does not excuse the unreasoned move of the other.
    assert_eq!(code(&output), 1, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    let rules = report["rules"].as_array().expect("an array");
    let status = |id: &str| -> &str {
        rules
            .iter()
            .find(|rule| rule["id"] == id)
            .expect("the rule is in the report")["status"]
            .as_str()
            .expect("a status")
    };
    assert_eq!(status("todo-markers"), "ok");
    assert_eq!(status("tests"), "baseline-lowered");
    // The deliberate raise is reported as a note, the other move as a failure.
    let notes: Vec<&str> = report["notes"]
        .as_array()
        .expect("an array")
        .iter()
        .map(|note| note.as_str().expect("a string"))
        .collect();
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(
        notes[0].contains("todo-markers: the baseline was raised deliberately"),
        "{notes:?}"
    );
}

#[test]
fn a_deliberate_lowering_does_not_excuse_an_unreasoned_raise_of_another_rule() {
    let repo = Repo::new(two_directions_config(), Some(&two_records_baseline()));
    // The floor is lowered deliberately; the ceiling is raised without a
    // reason.
    repo.write(
        "ratchet.baseline.json",
        "{\n  \"version\": 1,\n  \"rules\": {\n    \"tests\": { \"value\": 3, \"reason\": \"the floor was written off\" },\n    \"todo-markers\": { \"value\": 7 }\n  }\n}\n",
    );

    let output = repo.run(&["check", "--format", "json"]);

    // The reason of one rule does not excuse the unreasoned move of the other.
    assert_eq!(code(&output), 1, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    let rules = report["rules"].as_array().expect("an array");
    let status = |id: &str| -> &str {
        rules
            .iter()
            .find(|rule| rule["id"] == id)
            .expect("the rule is in the report")["status"]
            .as_str()
            .expect("a status")
    };
    assert_eq!(status("tests"), "ok");
    assert_eq!(status("todo-markers"), "baseline-raised");
    // The deliberate move is reported as a note, the other move as a failure.
    let notes: Vec<&str> = report["notes"]
        .as_array()
        .expect("an array")
        .iter()
        .map(|note| note.as_str().expect("a string"))
        .collect();
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(
        notes[0].contains("tests: the baseline was lowered deliberately"),
        "{notes:?}"
    );
}

// --- the pre-release check ----------------------------------------------------

/// A config with one pre-release check and no rule.
fn verify_config(name: &str, command: &[&str], snapshot: &str) -> String {
    let argv: Vec<String> = command.iter().map(|part| format!("\"{part}\"")).collect();
    format!(
        "[{{ \"rules\": {{}}, \"verify\": [{{ \"name\": \"{name}\", \"command\": [{}], \
         \"snapshot\": \"{snapshot}\" }}] }}]",
        argv.join(", ")
    )
}

/// A config with one pre-release check that writes to a given path.
fn verify_config_with_output_arg(
    name: &str,
    command: &[&str],
    output_arg: &str,
    snapshot: &str,
) -> String {
    let argv: Vec<String> = command.iter().map(|part| format!("\"{part}\"")).collect();
    format!(
        "[{{ \"rules\": {{}}, \"verify\": [{{ \"name\": \"{name}\", \"command\": [{}], \
         \"output_arg\": \"{output_arg}\", \"snapshot\": \"{snapshot}\" }}] }}]",
        argv.join(", ")
    )
}

#[test]
fn a_snapshot_that_holds_the_output_is_green() {
    let repo = Repo::new(
        &verify_config(
            "benchmark",
            &["sh", "bench.sh", "<out>"],
            "snapshots/benchmark.txt",
        ),
        None,
    );
    repo.write("bench.sh", "#!/bin/sh\nprintf '100 ops/s\\n' > \"$1\"\n");
    repo.write("snapshots/benchmark.txt", "100 ops/s\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "snapshot"]);

    let output = repo.run(&["verify"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("PASS"), "{text}");
    assert!(text.contains("benchmark"), "{text}");
    assert!(text.contains("COHERENT"), "{text}");
}

#[test]
fn a_snapshot_that_differs_fails_with_a_unified_diff() {
    let repo = Repo::new(
        &verify_config(
            "benchmark",
            &["sh", "bench.sh", "<out>"],
            "snapshots/benchmark.txt",
        ),
        None,
    );
    repo.write("bench.sh", "#!/bin/sh\nprintf '103 ops/s\\n' > \"$1\"\n");
    repo.write("snapshots/benchmark.txt", "100 ops/s\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "snapshot"]);

    let output = repo.run(&["verify"]);

    assert_eq!(code(&output), 1, "{}", stderr(&output));
    let text = stdout(&output);
    // The diff shows the difference between the committed snapshot and the
    // output, with the repository-relative path.
    assert!(text.contains("--- snapshots/benchmark.txt"), "{text}");
    assert!(text.contains("+++ snapshots/benchmark.txt"), "{text}");
    assert!(text.contains("-100 ops/s"), "{text}");
    assert!(text.contains("+103 ops/s"), "{text}");
    assert!(text.contains("@@"), "{text}");
    // The verdict line agrees with the exit code.
    assert!(text.contains("NOT COHERENT"), "{text}");
}

#[cfg(unix)]
#[test]
fn verify_compares_a_committed_file_behind_a_link_and_update_writes_there() {
    let repo = Repo::new(
        &verify_config("benchmark", &["sh", "bench.sh"], "docs/benchmark.txt"),
        None,
    );
    repo.write("bench.sh", "#!/bin/sh\nprintf '103 ops/s\\n'\n");
    repo.write("sub/benchmark.txt", "100 ops/s\n");
    std::os::unix::fs::symlink("sub", repo.path().join("docs")).expect("the link is made");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "linked snapshot"]);

    let output = repo.run(&["verify", "--format", "json"]);

    assert_eq!(code(&output), 1, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    assert_eq!(report["verify"][0]["status"], "drift");
    let diff = report["verify"][0]["diff"]
        .as_str()
        .expect("the diff exists");
    assert!(diff.contains("--- docs/benchmark.txt"), "{diff}");
    assert!(diff.contains("+++ docs/benchmark.txt"), "{diff}");
    assert!(diff.contains("-100 ops/s"), "{diff}");
    assert!(diff.contains("+103 ops/s"), "{diff}");
    assert_eq!(repo.read("sub/benchmark.txt"), "100 ops/s\n");
    assert_eq!(repo.status(), "", "verify changed the working tree");

    let output = repo.run(&["verify", "--update", "--format", "json"]);

    // The update writes through the named directory link, but the committed
    // snapshot remains stale and the report continues to use the named path.
    assert_eq!(code(&output), 1, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    assert_eq!(report["verify"][0]["status"], "drift");
    assert!(
        report["verify"][0]["diff"]
            .as_str()
            .unwrap()
            .contains("docs/benchmark.txt")
    );
    assert_eq!(repo.read("sub/benchmark.txt"), "103 ops/s\n");
    assert!(
        std::fs::symlink_metadata(repo.path().join("docs"))
            .expect("the link remains")
            .file_type()
            .is_symlink()
    );
    assert_eq!(repo.status(), " M sub/benchmark.txt\n");

    repo.git(&["add", "sub/benchmark.txt"]);
    repo.git(&["commit", "-q", "-m", "updated linked snapshot"]);
    let output = repo.run(&["verify"]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert!(stdout(&output).contains("COHERENT"), "{}", stdout(&output));
}

#[test]
fn a_snapshot_that_is_not_committed_is_new_and_not_a_failure() {
    let repo = Repo::new(
        &verify_config(
            "benchmark",
            &["sh", "bench.sh", "<out>"],
            "snapshots/benchmark.txt",
        ),
        None,
    );
    repo.write("bench.sh", "#!/bin/sh\nprintf '100 ops/s\\n' > \"$1\"\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "command"]);

    let output = repo.run(&["verify", "--format", "json"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    // The envelope is the check document's envelope (§6.3): the `generated`
    // array is replaced by a `verify` array.
    assert_eq!(report["schema"], 1);
    assert_eq!(report["summary"]["generated"], 1);
    assert_eq!(report["summary"]["drifted"], 0);
    assert_eq!(report["summary"]["rules"], 0);
    assert_eq!(report["rules"].as_array().expect("an array").len(), 0);
    assert_eq!(report["verify"][0]["status"], "new");
    assert_eq!(report["verify"][0]["name"], "benchmark");
    assert_eq!(report["verify"][0]["diff"], Value::Null);
    assert_eq!(report["ok"], true);
}

#[test]
fn the_json_report_carries_the_diff_of_a_deviation() {
    let repo = Repo::new(
        &verify_config(
            "benchmark",
            &["sh", "bench.sh", "<out>"],
            "snapshots/benchmark.txt",
        ),
        None,
    );
    repo.write("bench.sh", "#!/bin/sh\nprintf '103 ops/s\\n' > \"$1\"\n");
    repo.write("snapshots/benchmark.txt", "100 ops/s\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "snapshot"]);

    let output = repo.run(&["verify", "--format", "json"]);

    assert_eq!(code(&output), 1, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    assert_eq!(report["summary"]["drifted"], 1);
    assert_eq!(report["summary"]["generated"], 1);
    assert_eq!(report["verify"][0]["status"], "drift");
    let diff = report["verify"][0]["diff"]
        .as_str()
        .expect("the entry has a diff");
    assert!(diff.contains("-100 ops/s"), "{diff}");
    assert!(diff.contains("+103 ops/s"), "{diff}");
}

#[test]
fn a_check_that_cannot_be_started_is_an_operational_error() {
    let repo = Repo::new(
        &verify_config(
            "benchmark",
            &["ratchet-check-that-does-not-exist"],
            "snapshots/out.txt",
        ),
        None,
    );

    let output = repo.run(&["verify"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let text = format!("{}{}", stdout(&output), stderr(&output));
    // The message names the entry.
    assert!(
        text.contains("the check benchmark could not be started"),
        "{text}"
    );
    assert_deterministic(&text);
}

#[test]
fn a_check_that_fails_is_an_operational_error() {
    let repo = Repo::new(
        &verify_config("benchmark", &["sh", "bench.sh"], "snapshots/benchmark.txt"),
        None,
    );
    repo.write("bench.sh", "#!/bin/sh\necho 'the reason' >&2\nexit 3\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "command"]);

    let output = repo.run(&["verify"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let text = format!("{}{}", stdout(&output), stderr(&output));
    assert!(
        text.contains("the check benchmark exited with status 3"),
        "{text}"
    );
    // The report carries ratchet's own words, never the output of the
    // command (SPEC 6.2).
    assert!(!text.contains("the reason"), "{text}");
    assert_deterministic(&text);
}

#[test]
fn a_check_that_writes_nothing_is_an_operational_error() {
    let repo = Repo::new(
        &verify_config_with_output_arg(
            "benchmark",
            &["sh", "bench.sh"],
            "--output",
            "snapshots/benchmark.txt",
        ),
        None,
    );
    // The command succeeds but writes nothing.
    repo.write("bench.sh", "#!/bin/sh\nexit 0\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "command"]);

    let output = repo.run(&["verify"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let text = format!("{}{}", stdout(&output), stderr(&output));
    assert!(
        text.contains("the check benchmark wrote no output"),
        "{text}"
    );
    assert_deterministic(&text);
}

#[test]
fn a_verify_run_leaves_the_tree_byte_and_mode_identical() {
    let repo = Repo::new(
        &verify_config(
            "benchmark",
            &["sh", "bench.sh", "<out>"],
            "snapshots/benchmark.txt",
        ),
        None,
    );
    repo.write("bench.sh", "#!/bin/sh\nprintf '103 ops/s\\n' > \"$1\"\n");
    repo.write("snapshots/benchmark.txt", "100 ops/s\n");
    #[cfg(unix)]
    set_mode(&repo, "snapshots/benchmark.txt", 0o644);
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "snapshot"]);

    let output = repo.run(&["verify"]);

    assert_eq!(code(&output), 1, "{}", stderr(&output));
    // The tree is unchanged: the command wrote to a temporary path.
    assert_eq!(repo.status(), "", "the verify run changed the working tree");
    assert_eq!(repo.read("snapshots/benchmark.txt"), "100 ops/s\n");
    #[cfg(unix)]
    assert_eq!(mode_of(&repo, "snapshots/benchmark.txt"), 0o644);
}

#[test]
fn a_command_that_writes_into_the_tree_leaves_the_snapshot_alone() {
    let repo = Repo::new(
        &verify_config(
            "benchmark",
            &["sh", "bench.sh", "<out>"],
            "snapshots/benchmark.txt",
        ),
        None,
    );
    // The command writes into the tree — the engine cannot undo that — and
    // into the snapshot: the snapshot is the file the engine owns.
    repo.write(
        "bench.sh",
        "#!/bin/sh\nprintf '103 ops/s\\n' > \"$1\"\nprintf 'side effect\\n' > side-effect.txt\n",
    );
    repo.write("snapshots/benchmark.txt", "100 ops/s\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "snapshot"]);

    let output = repo.run(&["verify"]);

    assert_eq!(code(&output), 1, "{}", stderr(&output));
    // The snapshot is byte-identical: the engine restored it.
    assert_eq!(repo.read("snapshots/benchmark.txt"), "100 ops/s\n");
    // The side effect of the command is the command's doing.
    assert!(
        repo.status().contains("side-effect.txt"),
        "{}",
        repo.status()
    );
}

#[test]
fn the_update_writes_the_snapshot_and_the_written_snapshot_compares_equal() {
    let repo = Repo::new(
        &verify_config(
            "benchmark",
            &["sh", "bench.sh", "<out>"],
            "snapshots/benchmark.txt",
        ),
        None,
    );
    repo.write("bench.sh", "#!/bin/sh\nprintf '103 ops/s\\n' > \"$1\"\n");
    repo.write("snapshots/benchmark.txt", "100 ops/s\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "snapshot"]);

    let output = repo.run(&["verify", "--update"]);

    // The snapshot is written — the tool writes it, a human never edits it.
    assert_eq!(repo.read("snapshots/benchmark.txt"), "103 ops/s\n");
    // The check stays failed: the committed snapshot is still stale.
    assert_eq!(code(&output), 1, "{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.contains("the snapshot was written to the working tree"),
        "{text}"
    );
    // The fix touches the snapshot and nothing else.
    assert_eq!(repo.status(), " M snapshots/benchmark.txt\n");

    // Once the snapshot is committed, the check is green.
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "updated"]);
    let output = repo.run(&["verify"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert!(stdout(&output).contains("COHERENT"), "{}", stdout(&output));
}

#[test]
fn the_update_writes_a_snapshot_that_is_not_committed_yet() {
    let repo = Repo::new(
        &verify_config(
            "benchmark",
            &["sh", "bench.sh", "<out>"],
            "snapshots/benchmark.txt",
        ),
        None,
    );
    repo.write("bench.sh", "#!/bin/sh\nprintf '100 ops/s\\n' > \"$1\"\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "command"]);

    let output = repo.run(&["verify", "--update"]);

    // The snapshot exists now; it is not committed yet.
    assert_eq!(repo.read("snapshots/benchmark.txt"), "100 ops/s\n");
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert_eq!(repo.status(), "?? snapshots/\n");
}

#[cfg(unix)]
#[test]
fn verify_update_creates_a_missing_resolved_directory_behind_a_link() {
    let repo = Repo::new(
        &verify_config("benchmark", &["sh", "bench.sh"], "docs/benchmark.txt"),
        None,
    );
    repo.write("bench.sh", "#!/bin/sh\nprintf '100 ops/s\\n'\n");
    std::os::unix::fs::symlink("sub", repo.path().join("docs")).expect("the link is made");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "linked snapshot directory"]);

    let output = repo.run(&["verify", "--update"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert_eq!(repo.read("sub/benchmark.txt"), "100 ops/s\n");
    assert!(
        std::fs::symlink_metadata(repo.path().join("docs"))
            .expect("the link remains")
            .file_type()
            .is_symlink()
    );
    assert_eq!(repo.status(), "?? sub/\n");
}

#[cfg(unix)]
#[test]
fn verify_restore_removes_a_directory_created_behind_a_missing_link_target() {
    let repo = Repo::new(
        &verify_config(
            "benchmark",
            &["sh", "bench.sh", "<out>"],
            "docs/benchmark.txt",
        ),
        None,
    );
    repo.write(
        "bench.sh",
        "#!/bin/sh\nmkdir -p sub\nprintf 'side effect\\n' > docs/benchmark.txt\n\
         printf '100 ops/s\\n' > \"$1\"\n",
    );
    std::os::unix::fs::symlink("sub", repo.path().join("docs")).expect("the link is made");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "linked snapshot directory"]);

    let output = repo.run(&["verify"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert!(
        !repo.path().join("sub").exists(),
        "the resolved directory created by the command was left behind"
    );
    assert!(
        std::fs::symlink_metadata(repo.path().join("docs"))
            .expect("the link remains")
            .file_type()
            .is_symlink()
    );
    assert_eq!(repo.status(), "", "the verify run changed the working tree");
}

#[cfg(unix)]
#[test]
fn verify_restore_removes_a_directory_created_before_a_parent_traversal_behind_a_link() {
    let repo = Repo::new(
        &verify_config(
            "benchmark",
            &[
                "sh",
                "-c",
                "mkdir -p docs/x; printf '100 ops/s\\n' > docs/x/../snapshot.txt; \
                 printf '100 ops/s\\n'",
            ],
            "docs/x/../snapshot.txt",
        ),
        None,
    );
    repo.write("sub/snapshot.txt", "100 ops/s\n");
    std::os::unix::fs::symlink("sub", repo.path().join("docs")).expect("the link is made");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "linked snapshot traversal"]);

    let output = repo.run(&["verify"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert_eq!(repo.read("sub/snapshot.txt"), "100 ops/s\n");
    assert!(
        !repo.path().join("sub/x").exists(),
        "the intermediate directory created through the link was left behind"
    );
    assert!(
        std::fs::symlink_metadata(repo.path().join("docs"))
            .expect("the link remains")
            .file_type()
            .is_symlink()
    );
    assert_eq!(repo.status(), "", "the verify run changed the working tree");
}

#[cfg(unix)]
#[test]
fn verify_restore_refuses_a_snapshot_parent_replaced_with_an_outside_link() {
    let outside = Outside::new("verify-restore-parent-swap");
    std::fs::write(outside.file("snapshot.txt"), "outside sentinel\n")
        .expect("the outside sentinel is written");
    let repo = Repo::new(
        &verify_config(
            "benchmark",
            &["sh", "bench.sh", "<out>"],
            "sub/snapshot.txt",
        ),
        None,
    );
    repo.write(
        "bench.sh",
        &format!(
            "#!/bin/sh\nprintf 'produced\\n' > \"$1\"\nprintf 'command-modified\\n' > sub/snapshot.txt\n\
             mv sub sub-original\nln -s '{}' sub\n",
            outside.path().display()
        ),
    );
    repo.write("sub/snapshot.txt", "committed\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "snapshot"]);

    let output = repo.run(&["verify"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let message = stderr(&output);
    assert!(
        message.contains("the restore of sub/snapshot.txt failed"),
        "{message}"
    );
    assert!(
        message.contains("the working tree may be modified"),
        "{message}"
    );
    assert_eq!(
        std::fs::read_to_string(outside.file("snapshot.txt")).expect("the sentinel remains"),
        "outside sentinel\n"
    );
    assert_eq!(
        repo.read("sub-original/snapshot.txt"),
        "command-modified\n",
        "the moved in-repository object was not modified by restore"
    );
    assert!(
        std::fs::symlink_metadata(repo.path().join("sub"))
            .expect("the replacement link remains")
            .file_type()
            .is_symlink()
    );
}

#[cfg(unix)]
#[test]
fn verify_traversal_cleanup_refuses_a_parent_replaced_with_an_outside_link() {
    let outside = Outside::new("verify-traversal-parent-swap");
    std::fs::create_dir(outside.file("x")).expect("the outside x directory is created");
    let repo = Repo::new(
        &verify_config(
            "benchmark",
            &["sh", "bench.sh", "<out>"],
            "sub/x/../snapshot.txt",
        ),
        None,
    );
    repo.write(
        "bench.sh",
        &format!(
            "#!/bin/sh\nprintf 'produced\\n' > \"$1\"\nmkdir -p sub/x\n\
             printf 'command-modified\\n' > sub/x/../snapshot.txt\nmv sub sub-original\n\
             ln -s '{}' sub\n",
            outside.path().display()
        ),
    );
    repo.write("sub/snapshot.txt", "committed\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "snapshot"]);

    let output = repo.run(&["verify"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let message = stderr(&output);
    assert!(
        message.contains("the restore of sub/x/../snapshot.txt failed"),
        "{message}"
    );
    assert!(
        message.contains("could not safely remove the created directories"),
        "{message}"
    );
    assert!(
        message.contains("the working tree may be modified"),
        "{message}"
    );
    assert!(outside.file("x").is_dir(), "outside/x was removed");
    assert_eq!(
        std::fs::read_dir(outside.file("x"))
            .expect("outside/x remains readable")
            .count(),
        0,
        "outside/x is no longer empty"
    );
    assert!(
        repo.path().join("sub-original/x").is_dir(),
        "the moved in-repository traversal directory was removed"
    );
    assert_eq!(repo.read("sub-original/snapshot.txt"), "command-modified\n");
}

#[cfg(windows)]
#[test]
fn verify_refuses_a_junction_to_an_outside_directory_in_a_traversal_path() {
    let outside = Outside::new("verify-windows-traversal-junction");
    let escape = outside.file("x");
    std::fs::create_dir(&escape).expect("the outside x directory is created");
    let repo = Repo::new(
        &verify_config(
            "benchmark",
            &["cmd", "/c", "bench.bat", "<out>"],
            "sub/x/../snapshot.txt",
        ),
        None,
    );
    // The command replaces the missing traversal directory with a junction to
    // outside/x, so `sub/x/..` resolves to `outside` although Win32 normalizes
    // the `..` lexically. Replacing the tracked parent itself is refused by the
    // retained no-delete handles, so the junction is the reachable attack.
    repo.write(
        "bench.bat",
        &format!(
            "@echo off\r\necho produced> \"%~1\"\r\n\
             echo command-modified> \"sub\\snapshot.txt\"\r\n\
             mklink /J \"sub\\x\" \"{}\" >nul\r\n",
            escape.display()
        ),
    );
    repo.write("sub/snapshot.txt", "committed\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "snapshot"]);

    let output = repo.run(&["verify"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let message = stderr(&output);
    assert!(
        message.contains("left alone because cleanup validation failed"),
        "the junction candidate is refused: {message}"
    );
    assert!(
        message.contains("could not safely remove the created directories"),
        "the refusal is reported as a cleanup failure: {message}"
    );
    assert!(
        repo.read("sub/snapshot.txt")
            .starts_with("command-modified"),
        "the command did not run against the tracked snapshot"
    );
    assert!(escape.is_dir(), "outside/x was removed");
    assert_eq!(
        std::fs::read_dir(&escape)
            .expect("outside/x remains readable")
            .count(),
        0,
        "outside/x is no longer empty"
    );
    assert!(
        !outside.file("snapshot.txt").exists(),
        "the restore wrote the snapshot outside the repository"
    );
}

#[test]
fn verify_update_creates_the_resolved_snapshot_without_a_traversal_directory() {
    let repo = Repo::new(
        &verify_config("benchmark", &["sh", "bench.sh"], "a/../snapshot.txt"),
        None,
    );
    repo.write("bench.sh", "#!/bin/sh\nprintf '100 ops/s\\n'\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "command"]);

    let output = repo.run(&["verify", "--update"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert_eq!(repo.read("snapshot.txt"), "100 ops/s\n");
    assert!(
        !repo.path().join("a").exists(),
        "the traversal directory was created unnecessarily"
    );
    assert!(repo.path().is_dir(), "the repository root was removed");
}

#[test]
fn a_snapshot_that_resolves_to_the_repository_root_is_refused_without_changes() {
    let repo = Repo::new(
        &verify_config("benchmark", &["sh", "bench.sh"], "a/.."),
        None,
    );
    repo.write("bench.sh", "#!/bin/sh\nprintf '100 ops/s\\n'\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "command"]);

    let output = repo.run(&["verify", "--update"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    assert!(
        stderr(&output).contains("the snapshot a/.. is a directory, not a file"),
        "{}",
        stderr(&output)
    );
    assert!(repo.path().is_dir(), "the repository root was removed");
    assert!(
        !repo.path().join("a").exists(),
        "the traversal directory was left behind"
    );
    assert_eq!(repo.status(), "", "the refused update changed the tree");
}

#[cfg(unix)]
#[test]
fn verify_update_rolls_back_directories_when_a_later_creation_fails() {
    let too_long = "x".repeat(300);
    let snapshot = format!("ok/{too_long}/snapshot.txt");
    let repo = Repo::new(
        &verify_config("benchmark", &["sh", "bench.sh"], &snapshot),
        None,
    );
    repo.write("bench.sh", "#!/bin/sh\nprintf '100 ops/s\\n'\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "command"]);

    let output = repo.run(&["verify", "--update"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let text = stderr(&output);
    assert!(
        text.contains("cannot create the snapshot directory ok/"),
        "{text}"
    );
    assert!(!text.contains("working tree was modified"), "{text}");
    assert!(
        !repo.path().join("ok").exists(),
        "the earlier directory was left behind"
    );
    assert_eq!(repo.status(), "", "the failed update changed the tree");
}

#[test]
fn a_snapshot_outside_the_repository_root_is_refused() {
    let repo = Repo::new(
        &verify_config("benchmark", &["sh", "bench.sh"], "../escape.txt"),
        None,
    );

    let output = repo.run(&["verify"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    assert!(
        stderr(&output).contains("must lie inside the repository root"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn an_absolute_snapshot_path_is_refused() {
    let repo = Repo::new(
        &verify_config("benchmark", &["sh", "bench.sh"], "/tmp/escape.txt"),
        None,
    );

    let output = repo.run(&["verify"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    // The path of a project is relative: the message says what is wrong.
    assert!(
        stderr(&output).contains("has to be a relative path"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn two_verify_runs_produce_the_same_bytes() {
    let repo = Repo::new(
        &verify_config(
            "benchmark",
            &["sh", "bench.sh", "<out>"],
            "snapshots/benchmark.txt",
        ),
        None,
    );
    repo.write("bench.sh", "#!/bin/sh\nprintf '103 ops/s\\n' > \"$1\"\n");
    repo.write("snapshots/benchmark.txt", "100 ops/s\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "snapshot"]);

    let first = repo.run(&["verify"]);
    let second = repo.run(&["verify"]);

    assert_eq!(code(&first), 1, "{}", stderr(&first));
    assert_eq!(stdout(&first), stdout(&second));
    assert_eq!(stderr(&first), stderr(&second));
    let text = format!("{}{}", stdout(&first), stderr(&first));
    assert_deterministic(&text);
}

#[test]
fn the_stdout_of_the_command_is_the_output_without_an_output_path() {
    let repo = Repo::new(
        &verify_config("benchmark", &["sh", "bench.sh"], "snapshots/benchmark.txt"),
        None,
    );
    repo.write("bench.sh", "#!/bin/sh\nprintf '100 ops/s\\n'\n");
    repo.write("snapshots/benchmark.txt", "100 ops/s\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "snapshot"]);

    let output = repo.run(&["verify"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert!(stdout(&output).contains("COHERENT"), "{}", stdout(&output));
}

#[test]
fn the_output_arg_names_the_path_the_command_writes_to() {
    let repo = Repo::new(
        &verify_config_with_output_arg(
            "benchmark",
            &["sh", "bench.sh"],
            "--output",
            "snapshots/benchmark.txt",
        ),
        None,
    );
    repo.write("bench.sh", "#!/bin/sh\nprintf '100 ops/s\\n' > \"$2\"\n");
    repo.write("snapshots/benchmark.txt", "100 ops/s\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "snapshot"]);

    let output = repo.run(&["verify"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert!(stdout(&output).contains("COHERENT"), "{}", stdout(&output));
}

#[test]
fn the_verify_checks_are_not_part_of_the_check() {
    let config = r#"[
      {
        "rules": {
          "a": {
            "class": "c",
            "incident": "i",
            "unit": "u",
            "direction": "ceiling",
            "count": { "command": ["echo", "1"] }
          }
        },
        "verify": [
          { "name": "benchmark", "command": ["sh", "bench.sh", "<out>"], "snapshot": "snapshots/benchmark.txt" }
        ]
      }
    ]"#;
    let repo = Repo::new(config, Some(&baseline("a", 3)));
    repo.write("bench.sh", "#!/bin/sh\nprintf '103 ops/s\\n' > \"$1\"\n");
    repo.write("snapshots/benchmark.txt", "100 ops/s\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "snapshot"]);

    let output = repo.run(&["check", "--format", "json"]);

    // The check runs the rules and the drift gates — not the pre-release
    // checks: they belong to the release pipeline.
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).expect("the stdout is JSON");
    assert_eq!(report["rules"][0]["status"], "ok");
    assert_eq!(report["summary"]["generated"], 0);
}

#[test]
fn a_verify_command_without_a_check_is_not_a_pass() {
    let repo = Repo::new(r#"[{ "rules": {} }]"#, None);

    let output = repo.run(&["verify"]);

    assert_eq!(code(&output), 1, "{}", stderr(&output));
    assert!(
        stderr(&output).contains("an empty selection is never a pass"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn the_verify_command_selects_one_check() {
    let config = r#"[
      {
        "rules": {},
        "verify": [
          { "name": "alpha", "command": ["sh", "bench.sh", "<out>"], "snapshot": "snapshots/a.txt" },
          { "name": "beta", "command": ["sh", "bench.sh", "<out>"], "snapshot": "snapshots/b.txt" }
        ]
      }
    ]"#;
    let repo = Repo::new(config, None);
    repo.write("bench.sh", "#!/bin/sh\nprintf '103 ops/s\\n' > \"$1\"\n");
    repo.write("snapshots/a.txt", "100 ops/s\n");
    repo.write("snapshots/b.txt", "103 ops/s\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "snapshot"]);

    let output = repo.run(&["verify", "--name", "beta"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("beta"), "{text}");
    assert!(!text.contains("alpha"), "{text}");
}

#[test]
fn a_check_name_without_a_check_is_an_operational_error() {
    let repo = Repo::new(
        &verify_config("benchmark", &["sh", "bench.sh"], "snapshots/benchmark.txt"),
        None,
    );

    let output = repo.run(&["verify", "--name", "typo"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    assert!(
        stderr(&output).contains("no check for typo"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn the_config_refuses_a_command_with_the_placeholder_and_an_output_arg() {
    let repo = Repo::plain(
        &verify_config_with_output_arg(
            "benchmark",
            &["sh", "bench.sh", "<out>"],
            "--output",
            "snapshots/benchmark.txt",
        ),
        None,
    );

    let output = repo.run(&["verify"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let text = stderr(&output);
    assert!(
        text.contains("config error at verify[0].output_arg"),
        "{text}"
    );
    assert!(text.contains("one of them names the output path"), "{text}");
}

#[test]
fn the_config_refuses_a_check_without_a_name_or_a_command() {
    let repo = Repo::plain(
        r#"[{ "rules": {}, "verify": [{ "command": ["echo", "1"], "snapshot": "s.txt" }] }]"#,
        None,
    );

    let output = repo.run(&["verify"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    assert!(
        stderr(&output).contains("config error at verify[0].name"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn the_config_refuses_two_checks_with_the_same_name() {
    let repo = Repo::plain(
        r#"[{ "rules": {}, "verify": [
             { "name": "a", "command": ["echo", "1"], "snapshot": "a.txt" },
             { "name": "a", "command": ["echo", "2"], "snapshot": "b.txt" }
           ] }]"#,
        None,
    );

    let output = repo.run(&["verify"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let text = stderr(&output);
    assert!(text.contains("config error at verify[1].name"), "{text}");
    assert!(text.contains("is used twice"), "{text}");
}

#[test]
fn without_git_the_verify_comparison_is_skipped_with_a_note() {
    let repo = Repo::plain(
        &verify_config("benchmark", &["sh", "bench.sh"], "snapshots/benchmark.txt"),
        None,
    );
    repo.write("bench.sh", "#!/bin/sh\nprintf '100 ops/s\\n'\n");

    let output = repo.run_without_git(&["verify"]);

    // The check still runs; there is no committed snapshot to compare with.
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let text = stderr(&output);
    assert!(
        text.contains("the verify comparison is skipped: not a git repository"),
        "{text}"
    );
    assert!(stdout(&output).contains("PASS"), "{}", stdout(&output));
}

#[test]
fn a_snapshot_that_is_a_directory_is_refused() {
    let repo = Repo::new(
        &verify_config("benchmark", &["sh", "bench.sh"], "snapshots/benchmark.txt"),
        None,
    );
    repo.write("bench.sh", "#!/bin/sh\nprintf '100 ops/s\\n'\n");
    repo.write("snapshots/benchmark.txt/inner.txt", "not a snapshot\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "snapshot"]);

    let output = repo.run(&["verify"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    assert!(
        stderr(&output).contains("is a directory, not a file"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn a_committed_snapshot_that_cannot_be_read_is_an_operational_error() {
    let repo = Repo::new(
        &verify_config("benchmark", &["sh", "bench.sh"], "snapshots/benchmark.txt"),
        None,
    );
    repo.write("bench.sh", "#!/bin/sh\nprintf '100 ops/s\\n'\n");
    // The committed path is a directory, not a file.
    repo.write("snapshots/benchmark.txt/inner.txt", "not a snapshot\n");
    repo.write("snapshots/benchmark.txt/other.txt", "not a snapshot\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "snapshot"]);
    // The working tree does not have it any more; the committed tree does.
    std::fs::remove_dir_all(repo.path().join("snapshots/benchmark.txt"))
        .expect("the directory is removed");

    let output = repo.run(&["verify"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    assert!(
        stderr(&output).contains("is a directory, not a file"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn the_placeholder_is_replaced_inside_an_argument() {
    let repo = Repo::new(
        &verify_config(
            "benchmark",
            &["sh", "bench.sh", "--output=<out>"],
            "snapshots/benchmark.txt",
        ),
        None,
    );
    // The command takes the path out of the argument: `--output=<path>`.
    repo.write(
        "bench.sh",
        "#!/bin/sh\nprintf '100 ops/s\\n' > \"${1#--output=}\"\n",
    );
    repo.write("snapshots/benchmark.txt", "100 ops/s\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "snapshot"]);

    let output = repo.run(&["verify"]);

    // The command wrote to the temporary path: the check compares that file
    // with the snapshot, and the two are the same.
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert!(stdout(&output).contains("COHERENT"), "{}", stdout(&output));
    // The literal placeholder did not reach the command: no file named
    // `<out>` was created in the repository root.
    assert!(
        !repo.path().join("<out>").exists(),
        "the command wrote a file named <out>"
    );
    assert_eq!(repo.status(), "", "the verify run changed the working tree");
}

#[test]
fn the_placeholder_is_replaced_inside_an_argument_with_a_suffix() {
    let repo = Repo::new(
        &verify_config(
            "benchmark",
            &["sh", "bench.sh", "<out>.txt"],
            "snapshots/benchmark.txt",
        ),
        None,
    );
    repo.write("bench.sh", "#!/bin/sh\nprintf '100 ops/s\\n' > \"$1\"\n");
    repo.write("snapshots/benchmark.txt", "100 ops/s\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "snapshot"]);

    let output = repo.run(&["verify"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert!(stdout(&output).contains("COHERENT"), "{}", stdout(&output));
}

#[test]
fn the_config_refuses_a_command_with_the_placeholder_inside_an_argument_and_an_output_arg() {
    let repo = Repo::plain(
        &verify_config_with_output_arg(
            "benchmark",
            &["sh", "bench.sh", "--output=<out>"],
            "--output",
            "snapshots/benchmark.txt",
        ),
        None,
    );

    let output = repo.run(&["verify"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let text = stderr(&output);
    assert!(
        text.contains("config error at verify[0].output_arg"),
        "{text}"
    );
    assert!(text.contains("one of them names the output path"), "{text}");
}

#[test]
fn a_command_that_rewrites_the_snapshot_leaves_its_time_alone() {
    let repo = Repo::new(
        &verify_config(
            "benchmark",
            &["sh", "bench.sh", "<out>"],
            "snapshots/benchmark.txt",
        ),
        None,
    );
    // The command writes its output to the temporary path **and** rewrites
    // the snapshot with the very same bytes: a command that leaves the file
    // alone would not exercise the restore.
    repo.write(
        "bench.sh",
        "#!/bin/sh\nprintf '100 ops/s\\n' > \"$1\"\nprintf '100 ops/s\\n' > snapshots/benchmark.txt\n",
    );
    repo.write("snapshots/benchmark.txt", "100 ops/s\n");
    // The snapshot has a time of its own: the engine has to give it back.
    let time = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
    set_modified(&repo, "snapshots/benchmark.txt", time);
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "snapshot"]);

    let output = repo.run(&["verify"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    // The bytes are the same ...
    assert_eq!(repo.read("snapshots/benchmark.txt"), "100 ops/s\n");
    // ... and so is the time: the engine gave the file its time back.  A
    // restore that keeps the bytes but loses the time is a restore that
    // changed the tree.
    assert_eq!(modified_of(&repo, "snapshots/benchmark.txt"), time);
    assert_eq!(repo.status(), "", "the verify run changed the working tree");
}

#[test]
fn a_command_that_overwrites_the_snapshot_gives_it_its_bytes_and_its_time_back() {
    let repo = Repo::new(
        &verify_config(
            "benchmark",
            &["sh", "bench.sh", "<out>"],
            "snapshots/benchmark.txt",
        ),
        None,
    );
    // The command overwrites the snapshot with other bytes.
    repo.write(
        "bench.sh",
        "#!/bin/sh\nprintf '100 ops/s\\n' > \"$1\"\nprintf '999 ops/s\\n' > snapshots/benchmark.txt\n",
    );
    repo.write("snapshots/benchmark.txt", "100 ops/s\n");
    let time = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
    set_modified(&repo, "snapshots/benchmark.txt", time);
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "snapshot"]);

    let output = repo.run(&["verify"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    // The bytes of the snapshot are back ...
    assert_eq!(repo.read("snapshots/benchmark.txt"), "100 ops/s\n");
    // ... and so is its time.
    assert_eq!(modified_of(&repo, "snapshots/benchmark.txt"), time);
    assert_eq!(repo.status(), "", "the verify run changed the working tree");
}

#[test]
fn a_restore_that_fails_is_an_operational_error() {
    let repo = Repo::new(
        &verify_config(
            "benchmark",
            &["sh", "bench.sh", "<out>"],
            "snapshots/benchmark.txt",
        ),
        None,
    );
    // The command takes the snapshot away: the restore cannot write the file
    // back.  A swallowed restore error would be a mutated tree that nobody
    // was told about — the run has to end with an operational error.
    repo.write(
        "bench.sh",
        "#!/bin/sh\nprintf '100 ops/s\\n' > \"$1\"\nrm -rf snapshots\n",
    );
    repo.write("snapshots/benchmark.txt", "100 ops/s\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "snapshot"]);

    let output = repo.run(&["verify"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let message = stderr(&output);
    assert!(message.contains("the restore of"), "{message}");
    assert!(
        message.contains("the working tree may be modified"),
        "{message}"
    );
}

#[test]
fn a_command_that_leaves_the_snapshot_alone_leaves_its_time_alone() {
    let repo = Repo::new(
        &verify_config(
            "benchmark",
            &["sh", "bench.sh", "<out>"],
            "snapshots/benchmark.txt",
        ),
        None,
    );
    repo.write("bench.sh", "#!/bin/sh\nprintf '100 ops/s\\n' > \"$1\"\n");
    repo.write("snapshots/benchmark.txt", "100 ops/s\n");
    let time = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
    set_modified(&repo, "snapshots/benchmark.txt", time);
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "snapshot"]);

    let output = repo.run(&["verify"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    // The engine touched the file with its marker and gave the time back.
    assert_eq!(modified_of(&repo, "snapshots/benchmark.txt"), time);
    assert_eq!(repo.status(), "", "the verify run changed the working tree");
}

#[cfg(unix)]
#[test]
fn a_symlinked_snapshot_is_refused() {
    let repo = Repo::new(
        &verify_config(
            "benchmark",
            &["sh", "bench.sh", "<out>"],
            "snapshots/benchmark.txt",
        ),
        None,
    );
    repo.write("bench.sh", "#!/bin/sh\nprintf '100 ops/s\\n' > \"$1\"\n");
    repo.write("snapshots/real.txt", "100 ops/s\n");
    std::os::unix::fs::symlink("real.txt", repo.path().join("snapshots/benchmark.txt"))
        .expect("the link is created");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "link"]);

    let output = repo.run(&["verify"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let text = stderr(&output);
    // The message names the check and the path.
    assert!(text.contains("the check benchmark"), "{text}");
    assert!(text.contains("snapshots/benchmark.txt"), "{text}");
    assert!(text.contains("is a symbolic link"), "{text}");
}

#[cfg(unix)]
#[test]
fn a_snapshot_that_leaves_the_root_through_a_link_is_refused() {
    let outside = TempDir::new().expect("a temporary directory");
    let repo = Repo::new(
        &verify_config(
            "benchmark",
            &["sh", "bench.sh", "<out>"],
            "snapshots/benchmark.txt",
        ),
        None,
    );
    repo.write("bench.sh", "#!/bin/sh\nprintf '100 ops/s\\n' > \"$1\"\n");
    // The directory of the snapshot is a link out of the repository.
    std::os::unix::fs::symlink(outside.path(), repo.path().join("snapshots"))
        .expect("the link is created");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "link"]);

    let output = repo.run(&["verify", "--update"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let text = stderr(&output);
    assert!(text.contains("the check benchmark"), "{text}");
    assert!(
        text.contains("must lie inside the repository root"),
        "{text}"
    );
    // Nothing was written outside the tree.
    assert!(!outside.path().join("benchmark.txt").exists());
}

#[cfg(unix)]
#[test]
fn a_snapshot_through_a_link_and_a_parent_is_refused() {
    // "link/../snap.txt" is ambiguous: the operating system reads it as the
    // parent of the link's **target**, and `--update` would write there.
    let repo = Repo::new(
        &verify_config(
            "benchmark",
            &["sh", "bench.sh", "<out>"],
            "link/../snap.txt",
        ),
        None,
    );
    repo.write("bench.sh", "#!/bin/sh\nprintf '100 ops/s\\n' > \"$1\"\n");
    let outside = Outside::new("parent-snapshot");
    std::os::unix::fs::symlink(outside.path(), repo.path().join("link")).expect("the link is made");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "link"]);

    let output = repo.run(&["verify", "--update"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let text = stderr(&output);
    assert!(text.contains("the check benchmark"), "{text}");
    assert!(text.contains("is ambiguous"), "{text}");
    // Nothing was written outside the tree.
    assert!(
        !outside.file("snap.txt").exists(),
        "the snapshot was written outside the repository root"
    );
}

#[cfg(unix)]
#[test]
fn a_snapshot_behind_a_chain_of_links_is_refused() {
    // The snapshot is a chain: `snap.txt` → `real/snap.txt` → a file outside
    // the root that is not there.
    let repo = Repo::new(
        &verify_config("benchmark", &["sh", "bench.sh", "<out>"], "snap.txt"),
        None,
    );
    repo.write("bench.sh", "#!/bin/sh\nprintf '100 ops/s\\n' > \"$1\"\n");
    let outside = Outside::new("snapshot-chain");
    link_chain(
        &repo.path().join("snap.txt"),
        &repo.path().join("real/snap.txt"),
        &outside.file("shared-snap.txt"),
    );
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "link"]);

    let output = repo.run(&["verify", "--update"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let text = stderr(&output);
    assert!(text.contains("the check benchmark"), "{text}");
    assert!(
        text.contains("must lie inside the repository root"),
        "{text}"
    );
    // Nothing was written outside the tree.
    assert!(
        !outside.file("shared-snap.txt").exists(),
        "the snapshot was written outside the repository root"
    );
}

#[test]
fn an_empty_directory_the_command_created_is_removed() {
    let repo = Repo::new(
        &verify_config(
            "benchmark",
            &["sh", "bench.sh", "<out>"],
            "snapshots/benchmark.txt",
        ),
        None,
    );
    // The command creates the directory of the snapshot and leaves it empty.
    repo.write(
        "bench.sh",
        "#!/bin/sh\nmkdir -p snapshots\nprintf '100 ops/s\\n' > \"$1\"\n",
    );
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "command"]);

    let output = repo.run(&["verify"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert!(
        !repo.path().join("snapshots").exists(),
        "the empty directory was left behind"
    );
    assert_eq!(repo.status(), "", "the verify run changed the working tree");
}

#[test]
fn an_empty_directory_the_command_created_is_removed_even_when_it_wrote_the_snapshot() {
    let repo = Repo::new(
        &verify_config(
            "benchmark",
            &["sh", "bench.sh", "<out>"],
            "snapshots/benchmark.txt",
        ),
        None,
    );
    // The command creates the snapshot — and the directory it lies in — and
    // writes its output to the temporary path.
    repo.write(
        "bench.sh",
        "#!/bin/sh\nmkdir -p snapshots\nprintf '100 ops/s\\n' > snapshots/benchmark.txt\n\
         printf '100 ops/s\\n' > \"$1\"\n",
    );
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "command"]);

    let output = repo.run(&["verify"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    // The snapshot is gone again: it was not there before.
    assert!(
        !repo.path().join("snapshots/benchmark.txt").exists(),
        "the snapshot was left behind"
    );
    // And so is the empty directory.
    assert!(
        !repo.path().join("snapshots").exists(),
        "the empty directory was left behind"
    );
    assert_eq!(repo.status(), "", "the verify run changed the working tree");
}

#[test]
fn the_diagnostic_channel_shows_the_argv_of_the_check() {
    let repo = Repo::new(
        &verify_config(
            "benchmark",
            &["sh", "bench.sh", "<out>"],
            "snapshots/benchmark.txt",
        ),
        None,
    );
    repo.write("bench.sh", "#!/bin/sh\nprintf '100 ops/s\\n' > \"$1\"\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "command"]);

    let output = repo.run(&["verify", "--debug"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let text = stderr(&output);
    // The argv that was really executed — the output path included.
    assert!(text.contains("sh bench.sh"), "{text}");
    // And the resolved config.
    assert!(text.contains("resolved config"), "{text}");
}

#[test]
fn a_drift_gate_creates_the_directory_of_the_generated_file() {
    // A project cannot commit an empty directory: a generator that writes a
    // file into a new directory would fail, and the gate would be red from
    // the first run.  The engine creates the directory for it — and removes
    // it again.
    let repo = Repo::new(
        &generated_config(&["sh", "gen.sh", "docs/generated.md"], None),
        None,
    );
    // The generator writes the file and nothing else: the directory is the
    // engine's business, not the generator's.
    repo.write("gen.sh", "#!/bin/sh\nprintf 'generated\\n' > \"$1\"\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "the generator"]);

    let output = repo.run(&["drift"]);

    // The gate runs: the file is new (it is not committed yet), and the
    // directory it needed was created for it.
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    // And the tree is unchanged: the file the generator wrote is gone again.
    assert_eq!(repo.status(), "", "the drift run changed the working tree");
    assert!(
        !repo.path().join("docs").exists(),
        "the directory the engine created was left behind"
    );
}

#[cfg(unix)]
#[test]
fn a_drift_gate_creates_the_directory_a_link_points_at() {
    // The directory of the generated file is a link to a directory that does
    // not exist yet — inside the root.  The engine creates the directory
    // **where the system resolves the path to**: there the generator writes
    // the file, and there the restore removes it again.
    let repo = Repo::new(
        &generated_config(&["sh", "gen.sh", "docs/generated.md"], None),
        None,
    );
    repo.write("gen.sh", "#!/bin/sh\nprintf 'generated\\n' > \"$1\"\n");
    // The link points at a directory of the root that is not there yet.
    std::os::unix::fs::symlink("sub", repo.path().join("docs")).expect("the link is made");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "the generator"]);

    let output = repo.run(&["drift"]);

    // The gate runs: the directory was created, and the generator wrote the
    // file behind the link.
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    // And the tree is unchanged: the file the generator wrote is gone again.
    assert_eq!(repo.status(), "", "the drift run changed the working tree");
    assert!(
        !repo.path().join("sub").exists(),
        "the directory the engine created was left behind"
    );
    // And the link is still the link it was.
    assert!(
        std::fs::symlink_metadata(repo.path().join("docs")).is_ok(),
        "the link was removed"
    );
}

#[test]
fn a_drift_gate_creates_the_directory_a_parent_needs() {
    // The path walks through a directory that does not exist: `sub/../x.md`.
    // A `..` can only pop a directory that exists, so the engine creates
    // `sub` — the file itself lands in the root.
    let repo = Repo::new(
        &generated_config_at("sub/../x.md", &["sh", "gen.sh", "sub/../x.md"], None),
        None,
    );
    repo.write("gen.sh", "#!/bin/sh\nprintf 'generated\\n' > \"$1\"\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "the generator"]);

    let output = repo.run(&["drift"]);

    // The gate runs: the generator wrote the file.
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    // And the tree is unchanged: the file the generator wrote is gone again,
    // and so is the directory the engine created for the `..`.
    assert_eq!(repo.status(), "", "the drift run changed the working tree");
    assert!(
        !repo.path().join("sub").exists(),
        "the directory the engine created was left behind"
    );
}

#[test]
fn a_drift_gate_removes_every_directory_it_created() {
    // The path walks through two directories that do not exist:
    // `a/b/../x.md`.  The engine creates `a` and `a/b` — and the restore has
    // to remove them in the other way round: the deepest first, so that the
    // shallower one is empty when it is removed.
    let repo = Repo::new(
        &generated_config_at("a/b/../x.md", &["sh", "gen.sh", "a/b/../x.md"], None),
        None,
    );
    repo.write("gen.sh", "#!/bin/sh\nprintf 'generated\\n' > \"$1\"\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "the generator"]);

    let output = repo.run(&["drift"]);

    // The gate runs: the generator wrote the file.
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    // And the tree is unchanged: the file the generator wrote is gone again.
    assert_eq!(repo.status(), "", "the drift run changed the working tree");
    assert!(
        !repo.path().join("a").exists(),
        "the directory the engine created was left behind"
    );
    assert!(
        !repo.path().join("a/b").exists(),
        "the directory the engine created was left behind"
    );
}

#[cfg(unix)]
#[test]
fn a_drift_gate_rolls_back_directories_when_a_later_component_cannot_be_created() {
    let too_long = "x".repeat(300);
    let generated = format!("ok/{too_long}/generated.md");
    let repo = Repo::new(
        &generated_config_at(&generated, &["sh", "gen.sh", &generated], None),
        None,
    );
    repo.write("gen.sh", "#!/bin/sh\nprintf 'generated\\n' > \"$1\"\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "the generator"]);

    let output = repo.run(&["drift"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    assert!(stderr(&output).contains("cannot create the directory"));
    assert!(
        !repo.path().join("ok").exists(),
        "the earlier directory in the failed chain was left behind"
    );
    assert_eq!(repo.status(), "", "the failed drift run changed the tree");
}

#[test]
fn a_generated_target_that_resolves_to_the_repository_root_is_refused() {
    let repo = Repo::new(
        &generated_config_at("a/..", &["sh", "gen.sh", "a/.."], None),
        None,
    );
    repo.write("gen.sh", "#!/bin/sh\nprintf 'generated\\n' > \"$1\"\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "the generator"]);

    let output = repo.run(&["drift"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    assert!(
        stderr(&output).contains("resolves to the repository root, not a file"),
        "{}",
        stderr(&output)
    );
    assert!(repo.path().is_dir(), "the repository root was removed");
    assert!(
        !repo.path().join("a").exists(),
        "an unnecessary directory remains"
    );
    assert_eq!(repo.status(), "", "the refused drift run changed the tree");
}

#[test]
fn a_drift_gate_removes_the_deeper_directory_before_the_shallower_one() {
    // The path walks through two directories that do not exist:
    // `a/b/c/../x.md`.  The engine creates `a`, `a/b` and `a/b/c` — and the
    // restore has to remove them in the right order: the directories below a
    // directory come before it, so that it is empty when it is removed.
    let repo = Repo::new(
        &generated_config_at("a/b/c/../x.md", &["sh", "gen.sh", "a/b/c/../x.md"], None),
        None,
    );
    repo.write("gen.sh", "#!/bin/sh\nprintf 'generated\\n' > \"$1\"\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "the generator"]);

    let output = repo.run(&["drift"]);

    // The gate runs: the generator wrote the file.
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    // And the tree is unchanged: the file the generator wrote is gone again.
    assert_eq!(repo.status(), "", "the drift run changed the working tree");
    assert!(
        !repo.path().join("a").exists(),
        "the directory the engine created was left behind"
    );
    assert!(
        !repo.path().join("a/b").exists(),
        "the directory the engine created was left behind"
    );
    assert!(
        !repo.path().join("a/b/c").exists(),
        "the directory the engine created was left behind"
    );
}

#[cfg(unix)]
#[test]
fn a_drift_gate_through_a_linked_directory_compares_the_committed_file() {
    // Git keeps a symbolic link as a blob whose content is the path of its
    // target, and the files of the target are committed beside it: the
    // committed tree has no entry at the **named** path, and the comparison
    // has to follow the link to find the file — otherwise the gate measures
    // nothing and reports a green that says nothing.
    let repo = Repo::new(
        &generated_config(&["sh", "gen.sh", "docs/generated.md"], None),
        None,
    );
    repo.write("gen.sh", "#!/bin/sh\nprintf 'generated\\n' > \"$1\"\n");
    repo.write("sub/generated.md", "committed\n");
    std::os::unix::fs::symlink("sub", repo.path().join("docs")).expect("the link is made");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "the generator"]);

    let output = repo.run(&["drift"]);

    // The gate compares the committed file: it differs from what the
    // generator produces.
    assert_eq!(code(&output), 1, "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("docs/generated.md"), "{text}");
    assert!(text.contains("differs from the committed file"), "{text}");
    // The diff names the **named** path too: that is the path the project
    // named, and the path the user knows.
    assert!(text.contains("--- docs/generated.md"), "{text}");
    assert!(!text.contains("--- sub/generated.md"), "{text}");
    // And the tree is unchanged.
    assert_eq!(repo.status(), "", "the drift run changed the working tree");
}

#[cfg(unix)]
#[test]
fn a_drift_gate_with_fix_writes_the_file_behind_a_linked_directory() {
    // The `--fix` mode writes the produced content into the generated file:
    // the directory is created where the system resolves the path to, and
    // the file lands behind the link.  The `--fix` mode does not restore —
    // the tree keeps the produced file.
    let repo = Repo::new(&generated_config(&["sh", "gen.sh"], Some("--output")), None);
    repo.write("gen.sh", "#!/bin/sh\nprintf 'generated\\n' > \"$2\"\n");
    std::os::unix::fs::symlink("sub", repo.path().join("docs")).expect("the link is made");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "the generator"]);

    let output = repo.run(&["drift", "--fix"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    // The produced file landed behind the link.
    assert_eq!(repo.read("sub/generated.md"), "generated\n");
}

#[cfg(unix)]
#[test]
fn a_drift_gate_through_a_linked_directory_outside_the_root_is_refused() {
    // The directory inside the root is a link that points out of it: the
    // write would leave the tree the engine owns.
    let repo = Repo::new(
        &generated_config(&["sh", "gen.sh", "docs/generated.md"], None),
        None,
    );
    repo.write("gen.sh", "#!/bin/sh\nprintf 'generated\\n' > \"$1\"\n");
    let outside = Outside::new("drift");
    std::os::unix::fs::symlink(outside.path(), repo.path().join("docs")).expect("the link is made");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "the generator"]);

    let output = repo.run(&["drift"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    assert!(
        stderr(&output).contains("inside the repository root"),
        "{}",
        stderr(&output)
    );
    // Nothing was written outside the root.
    assert!(
        !outside.file("generated.md").exists(),
        "the gate wrote outside the repository root"
    );
}

#[cfg(unix)]
#[test]
fn a_drift_gate_through_a_linked_directory_and_a_new_directory_is_refused() {
    // The directory of the generated file does not exist yet, and the
    // directory above it is a link that points out of the root: the engine
    // would create the directory — and write the file — outside the tree it
    // owns.
    let repo = Repo::new(
        &generated_config_at(
            "docs/sub/generated.md",
            &["sh", "gen.sh", "docs/sub/generated.md"],
            None,
        ),
        None,
    );
    repo.write("gen.sh", "#!/bin/sh\nprintf 'generated\\n' > \"$1\"\n");
    let outside = Outside::new("drift-nested");
    std::os::unix::fs::symlink(outside.path(), repo.path().join("docs")).expect("the link is made");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "the generator"]);

    let output = repo.run(&["drift"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    assert!(
        stderr(&output).contains("inside the repository root"),
        "{}",
        stderr(&output)
    );
    // Nothing was created outside the root.
    assert!(
        !outside.path().join("sub").exists(),
        "the gate created a directory outside the repository root"
    );
}

#[cfg(unix)]
#[test]
fn a_drift_gate_through_a_chain_of_links_outside_the_root_is_refused() {
    // The generated file is a chain: `docs/generated.md` → `docs/real.md` →
    // a file outside the root that is not there.  The in-place generator
    // would write through it.
    let repo = Repo::new(
        &generated_config(&["sh", "gen.sh", "docs/generated.md"], None),
        None,
    );
    repo.write("gen.sh", "#!/bin/sh\nprintf 'generated\\n' > \"$1\"\n");
    let outside = Outside::new("drift-chain");
    link_chain(
        &repo.path().join("docs/generated.md"),
        &repo.path().join("docs/real.md"),
        &outside.file("shared-generated.md"),
    );
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "the generator"]);

    let output = repo.run(&["drift"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    assert!(
        stderr(&output).contains("inside the repository root"),
        "{}",
        stderr(&output)
    );
    // Nothing was written outside the root.
    assert!(
        !outside.file("shared-generated.md").exists(),
        "the gate wrote outside the repository root"
    );
}

#[cfg(unix)]
#[test]
fn a_drift_gate_with_a_temporary_output_through_a_chain_is_refused() {
    // The same chain, in the mode where the generator writes to a temporary
    // path: the tree is never touched, and the gate is still refused.
    let repo = Repo::new(
        &generated_config(&["sh", "gen.sh", "docs/generated.md"], Some("--output")),
        None,
    );
    repo.write("gen.sh", "#!/bin/sh\nprintf 'generated\\n' > \"$2\"\n");
    let outside = Outside::new("drift-chain-temp");
    link_chain(
        &repo.path().join("docs/generated.md"),
        &repo.path().join("docs/real.md"),
        &outside.file("shared-generated.md"),
    );
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "the generator"]);

    let output = repo.run(&["drift"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    assert!(
        stderr(&output).contains("inside the repository root"),
        "{}",
        stderr(&output)
    );
    // Nothing was written outside the root.
    assert!(
        !outside.file("shared-generated.md").exists(),
        "the gate wrote outside the repository root"
    );
}

#[cfg(unix)]
#[test]
fn a_drift_gate_through_a_link_and_a_parent_is_refused() {
    // "link/../generated.md" is ambiguous: the operating system reads it as
    // the parent of the link's **target** — and the generator would write
    // there.
    let repo = Repo::new(
        &generated_config_at(
            "link/../generated.md",
            &["sh", "gen.sh", "link/../generated.md"],
            None,
        ),
        None,
    );
    repo.write("gen.sh", "#!/bin/sh\nprintf 'generated\\n' > \"$1\"\n");
    let outside = Outside::new("parent-drift");
    std::os::unix::fs::symlink(outside.path(), repo.path().join("link")).expect("the link is made");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "the generator"]);

    let output = repo.run(&["drift"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    assert!(
        stderr(&output).contains("is ambiguous"),
        "{}",
        stderr(&output)
    );
    // Nothing was written outside the root.
    assert!(
        !outside.file("generated.md").exists(),
        "the gate wrote outside the repository root"
    );
}

#[cfg(unix)]
#[test]
fn a_drift_gate_with_a_temporary_output_through_a_link_and_a_parent_is_refused() {
    // The same shape, in the mode where the generator writes to a temporary
    // path: the tree is never touched, and the gate is still refused.
    let repo = Repo::new(
        &generated_config_at(
            "link/../generated.md",
            &["sh", "gen.sh", "link/../generated.md"],
            Some("--output"),
        ),
        None,
    );
    repo.write("gen.sh", "#!/bin/sh\nprintf 'generated\\n' > \"$2\"\n");
    let outside = Outside::new("parent-drift-temp");
    std::os::unix::fs::symlink(outside.path(), repo.path().join("link")).expect("the link is made");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "the generator"]);

    let output = repo.run(&["drift"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    assert!(
        stderr(&output).contains("is ambiguous"),
        "{}",
        stderr(&output)
    );
    // Nothing was written outside the root.
    assert!(
        !outside.file("generated.md").exists(),
        "the gate wrote outside the repository root"
    );
}

#[cfg(unix)]
#[test]
fn a_drift_gate_with_fix_through_a_link_and_a_parent_is_refused() {
    // The `--fix` mode writes the produced content into the generated file:
    // the path has to be refused before that happens.
    let repo = Repo::new(
        &generated_config_at(
            "link/../generated.md",
            &["sh", "gen.sh", "link/../generated.md"],
            None,
        ),
        None,
    );
    repo.write("gen.sh", "#!/bin/sh\nprintf 'generated\\n' > \"$1\"\n");
    let outside = Outside::new("parent-drift-fix");
    std::os::unix::fs::symlink(outside.path(), repo.path().join("link")).expect("the link is made");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "the generator"]);

    let output = repo.run(&["drift", "--fix"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    assert!(
        stderr(&output).contains("is ambiguous"),
        "{}",
        stderr(&output)
    );
    // Nothing was written outside the root.
    assert!(
        !outside.file("generated.md").exists(),
        "the gate wrote outside the repository root"
    );
}

#[test]
fn a_drift_gate_with_an_absolute_path_is_refused() {
    let repo = Repo::new(
        &generated_config_at(
            "/etc/generated.md",
            &["sh", "gen.sh", "/etc/generated.md"],
            None,
        ),
        None,
    );
    repo.write("gen.sh", "#!/bin/sh\nprintf 'generated\\n' > \"$1\"\n");

    let output = repo.run(&["drift"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    assert!(
        stderr(&output).contains("has to be a relative path"),
        "{}",
        stderr(&output)
    );
}

#[cfg(unix)]
#[test]
fn a_drift_gate_through_a_chain_in_a_directory_of_the_path_is_refused() {
    // The chain is in a directory of the path, not at the file position:
    // `docs` → `real` → a directory outside the root that is not there.  The
    // engine would create the directory — and write the file — outside.
    let repo = Repo::new(
        &generated_config(&["sh", "gen.sh", "docs/generated.md"], None),
        None,
    );
    repo.write("gen.sh", "#!/bin/sh\nprintf 'generated\\n' > \"$1\"\n");
    let outside = Outside::new("drift-chain-directory");
    link_chain(
        &repo.path().join("docs"),
        &repo.path().join("real"),
        &outside.path().join("sub"),
    );
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "the generator"]);

    let output = repo.run(&["drift"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    assert!(
        stderr(&output).contains("inside the repository root"),
        "{}",
        stderr(&output)
    );
    // Nothing was created outside the root.
    assert!(
        !outside.path().join("sub").exists(),
        "the gate created a directory outside the repository root"
    );
}

#[cfg(unix)]
#[test]
fn a_drift_gate_with_a_target_that_is_a_link_outside_the_root_is_refused() {
    // The generated file itself is a link to a file outside the root: the
    // generator would write through it, and the comparison would follow it
    // out of the tree the engine owns.
    let repo = Repo::new(
        &generated_config(&["sh", "gen.sh", "docs/generated.md"], None),
        None,
    );
    repo.write("gen.sh", "#!/bin/sh\nprintf 'generated\\n' > \"$1\"\n");
    std::fs::create_dir_all(repo.path().join("docs")).expect("the directory is created");
    let outside = Outside::new("drift-target");
    std::os::unix::fs::symlink(
        outside.file("generated.md"),
        repo.path().join("docs/generated.md"),
    )
    .expect("the link is made");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "the generator"]);

    let output = repo.run(&["drift"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    assert!(
        stderr(&output).contains("inside the repository root"),
        "{}",
        stderr(&output)
    );
    // Nothing was written outside the root.
    assert!(
        !outside.file("generated.md").exists(),
        "the gate wrote outside the repository root"
    );
}

#[cfg(unix)]
#[test]
fn a_drift_gate_with_a_dangling_link_is_refused() {
    // The generated file is a link that points at nothing: the generator
    // would create the file behind it, and the restore cannot take that
    // file back — the tree would change.
    let repo = Repo::new(
        &generated_config(&["sh", "gen.sh", "docs/generated.md"], None),
        None,
    );
    repo.write("gen.sh", "#!/bin/sh\nprintf 'generated\\n' > \"$1\"\n");
    std::fs::create_dir_all(repo.path().join("docs")).expect("the directory is created");
    // The link points at a file inside the root that is not there: the path
    // itself is inside the root, and the link is the problem.
    std::os::unix::fs::symlink("real.md", repo.path().join("docs/generated.md"))
        .expect("the link is made");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "the generator"]);

    let output = repo.run(&["drift"]);

    // The error names the file and the reason.
    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let text = stderr(&output);
    assert!(text.contains("docs/generated.md"), "{text}");
    assert!(text.contains("points at nothing"), "{text}");
    // Nothing was created behind the link.
    assert!(
        !repo.path().join("docs/real.md").exists(),
        "the gate created the file behind the dangling link"
    );
}

#[cfg(unix)]
#[test]
fn a_drift_gate_with_a_dangling_link_and_a_temporary_output_is_refused() {
    // The same link, in the mode where the generator writes to a temporary
    // path: the tree is never touched, so the reason is the comparison —
    // the message must not blame the generator.
    let repo = Repo::new(
        &generated_config(&["sh", "gen.sh", "docs/generated.md"], Some("--output")),
        None,
    );
    repo.write("gen.sh", "#!/bin/sh\nprintf 'generated\\n' > \"$2\"\n");
    std::fs::create_dir_all(repo.path().join("docs")).expect("the directory is created");
    std::os::unix::fs::symlink("real.md", repo.path().join("docs/generated.md"))
        .expect("the link is made");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "the generator"]);

    let output = repo.run(&["drift"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let text = stderr(&output);
    assert!(text.contains("points at nothing"), "{text}");
    assert!(
        text.contains("the comparison would follow it to a file that is not there"),
        "{text}"
    );
    // The generator never writes through the link in this mode.
    assert!(
        !text.contains("the generator would write through it"),
        "{text}"
    );
}

#[cfg(unix)]
#[test]
fn a_drift_gate_with_a_target_that_is_a_link_inside_the_root_is_allowed() {
    // The carve-out stays: the file may be a link as long as its target
    // lies inside the root — the comparison follows the link.
    let repo = Repo::new(
        &generated_config(&["sh", "gen.sh", "docs/generated.md"], None),
        None,
    );
    repo.write("gen.sh", "#!/bin/sh\nprintf 'generated\\n' > \"$1\"\n");
    repo.write("docs/real.md", "generated\n");
    std::os::unix::fs::symlink("real.md", repo.path().join("docs/generated.md"))
        .expect("the link is made");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "the generator"]);

    let output = repo.run(&["drift"]);

    // The gate compares the file behind the link.
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert_eq!(repo.status(), "", "the drift run changed the working tree");
    assert_eq!(repo.read("docs/real.md"), "generated\n");
}

#[cfg(unix)]
#[test]
fn a_directory_of_the_generated_file_that_cannot_be_created_is_an_error() {
    // The directory is the engine's business: a failure to create it is the
    // engine's error, and the user is not sent to the generator.
    let repo = Repo::new(
        &generated_config_at(
            "docs/sub/generated.md",
            &["sh", "gen.sh", "docs/sub/generated.md"],
            None,
        ),
        None,
    );
    repo.write("gen.sh", "#!/bin/sh\nprintf 'generated\\n' > \"$1\"\n");
    repo.write("docs/keep.md", "keep\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "the generator"]);

    // The directory cannot be written into: the run fails — with the
    // engine's error, not with the generator's.
    let directory = repo.path().join("docs");
    make_read_only(&directory);
    let output = repo.run(&["drift"]);
    make_writable(&directory);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let text = stderr(&output);
    assert!(text.contains("cannot create the directory"), "{text}");
    assert!(text.contains("docs/sub"), "{text}");
    // The generator is not the one that is blamed.
    assert!(!text.contains("exited with status"), "{text}");
}

/// A scoped per-file rule: `metric` measures every Java file, scope `auth`.
const STALE_SCOPED_CONFIG: &str = r#"{
  "rules": {
    "metric": {
      "class": "file growth", "incident": "test", "unit": "bytes",
      "direction": "ceiling", "description": "All Java sources", "aggregate": "max",
      "count": {"file_metrics": {"files": ["**/*.java"], "metric": "size_bytes"}},
      "scopes": [{"id": "auth", "description": "Auth", "files": ["src/auth/**"]}]
    }
  }
}"#;

/// `STALE_SCOPED_CONFIG` plus a scope whose file does not exist.
const EMPTY_SCOPE_CONFIG: &str = r#"{
  "rules": {
    "metric": {
      "class": "file growth", "incident": "test", "unit": "bytes",
      "direction": "ceiling", "description": "All Java sources", "aggregate": "max",
      "count": {"file_metrics": {"files": ["**/*.java"], "metric": "size_bytes"}},
      "scopes": [
        {"id": "auth", "description": "Auth", "files": ["src/auth/**"]},
        {"id": "legacy", "description": "Legacy", "files": ["src/legacy/**"]}
      ]
    }
  }
}"#;

/// A baseline with a removed rule and a removed scope next to a live one.
const STALE_BASELINE: &str = r#"{
  "version": 1,
  "rules": {
    "removed": {"value": 1},
    "metric": {
      "value": 6,
      "scopes": {
        "auth": {"value": 3},
        "gone": {"value": 9}
      }
    }
  }
}"#;

/// Writes the two Java files `STALE_SCOPED_CONFIG` measures (3 and 6 bytes).
fn write_stale_java(repo: &Repo) {
    repo.write("src/auth/Login.java", "abc");
    repo.write("src/Other.java", "123456");
}

#[test]
fn stale_artifacts_are_notes_by_default_and_check_never_prunes() {
    let repo = Repo::plain(STALE_SCOPED_CONFIG, Some(STALE_BASELINE));
    write_stale_java(&repo);
    let before = repo.read("ratchet.baseline.json");

    let output = repo.run(&["check"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let notes = stderr(&output);
    assert!(
        notes.contains("baseline record for unknown rule \"removed\""),
        "{notes}"
    );
    assert!(
        notes.contains("baseline scope \"gone\" for rule \"metric\" is not configured"),
        "{notes}"
    );
    assert!(notes.contains("ignored"), "{notes}");
    assert!(stdout(&output).contains("COHERENT"), "{}", stdout(&output));
    assert_eq!(
        repo.read("ratchet.baseline.json"),
        before,
        "a check never prunes"
    );
}

#[test]
fn check_strict_fails_on_every_stale_artifact_and_report_strict_exits_two() {
    let repo = Repo::plain(STALE_SCOPED_CONFIG, Some(STALE_BASELINE));
    write_stale_java(&repo);

    let output = repo.run(&["check", "--strict", "--format", "json"]);
    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let report: Value = serde_json::from_slice(&output.stdout).expect("JSON report");
    assert_eq!(report["ok"], false);
    let notes: Vec<&str> = report["notes"]
        .as_array()
        .expect("notes")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    let notes = notes.join("\n");
    assert!(
        notes.contains(
            "baseline record for unknown rule \"removed\" — remove it or run ratchet baseline update"
        ),
        "{notes}"
    );
    assert!(
        notes.contains(
            "baseline scope \"gone\" for rule \"metric\" is not configured — remove it or run \
             ratchet baseline update"
        ),
        "{notes}"
    );

    let repeated = repo.run(&["check", "--strict", "--format", "json"]);
    assert_eq!(
        output.stdout, repeated.stdout,
        "strict reports are deterministic"
    );

    let markdown = repo.run(&["report", "--strict"]);
    assert_eq!(code(&markdown), 2, "{}", stderr(&markdown));
    let text = stdout(&markdown);
    assert!(text.contains("NOT COHERENT"), "{text}");
    assert!(text.contains("unknown rule \"removed\""), "{text}");
}

#[test]
fn an_empty_configured_scope_is_skipped_by_default_and_fails_under_strict() {
    let repo = Repo::plain(EMPTY_SCOPE_CONFIG, Some(STALE_BASELINE));
    write_stale_java(&repo);

    let output = repo.run(&["check", "--format", "json"]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert!(
        stderr(&output).contains(
            "rule metric scope legacy selects no files — ignored (remove the scope from the config)"
        ),
        "{}",
        stderr(&output)
    );
    let report: Value = serde_json::from_slice(&output.stdout).expect("JSON report");
    let rules = report["rules"].as_array().expect("rules");
    // The empty scope contributes no result; the global fallback is compared.
    assert!(
        rules.iter().all(|rule| rule["scope"] != "legacy"),
        "{rules:?}"
    );
    assert!(
        rules
            .iter()
            .any(|rule| rule["scope"].is_null() && rule["count"] == 6 && rule["baseline"] == 6),
        "the global fallback still runs: {rules:?}"
    );

    let strict = repo.run(&["check", "--strict"]);
    assert_eq!(code(&strict), 2, "{}", stderr(&strict));
    assert!(
        stdout(&strict).contains("scope legacy (Legacy) selected no files"),
        "{}",
        stdout(&strict)
    );
}

#[test]
fn a_strict_empty_scope_does_not_get_the_counter_debug_hint() {
    let repo = Repo::plain(EMPTY_SCOPE_CONFIG, Some(STALE_BASELINE));
    write_stale_java(&repo);

    // The counter ran and delivered its measurements; only the configured
    // scope is empty, so its output cannot explain the result.
    let empty = repo.run(&["check", "--strict"]);
    assert_eq!(code(&empty), 2, "{}", stderr(&empty));
    assert!(
        !stderr(&empty).contains("hint: run with --debug to see the output of the counter"),
        "{}",
        stderr(&empty)
    );

    // A genuine counter failure still gets the hint.
    let broken = r#"{
      "rules": {
        "broken": {
          "class": "unfinished work", "incident": "test", "unit": "files",
          "direction": "ceiling",
          "count": {"command": ["echo", "not a count"]}
        }
      }
    }"#;
    let broken_repo = Repo::plain(broken, Some(&baseline("broken", 1)));
    let failed = broken_repo.run(&["check"]);
    assert_eq!(code(&failed), 2, "{}", stderr(&failed));
    assert!(
        stderr(&failed).contains("hint: run with --debug to see the output of the counter"),
        "{}",
        stderr(&failed)
    );
}

#[test]
fn a_count_empty_scope_does_not_get_the_counter_debug_hint() {
    let repo = Repo::plain(EMPTY_SCOPE_CONFIG, Some(STALE_BASELINE));
    write_stale_java(&repo);

    // The counter ran and delivered its measurements; only the configured
    // scope is empty, so its output cannot explain the result.  `count` still
    // fails closed.
    let empty = repo.run(&["count"]);
    assert_eq!(code(&empty), 2, "{}", stderr(&empty));
    assert!(
        stdout(&empty).contains("scope legacy (Legacy) selected no files"),
        "{}",
        stdout(&empty)
    );
    assert!(
        !stderr(&empty).contains("hint: run with --debug to see the output of the counter"),
        "{}",
        stderr(&empty)
    );

    // A genuine counter failure still gets the hint.
    let broken = r#"{
      "rules": {
        "broken": {
          "class": "unfinished work", "incident": "test", "unit": "files",
          "direction": "ceiling",
          "count": {"command": ["echo", "not a count"]}
        }
      }
    }"#;
    let broken_repo = Repo::plain(broken, Some(&baseline("broken", 1)));
    let failed = broken_repo.run(&["count"]);
    assert_eq!(code(&failed), 2, "{}", stderr(&failed));
    assert!(
        stderr(&failed).contains("hint: run with --debug to see the output of the counter"),
        "{}",
        stderr(&failed)
    );
}

#[test]
fn baseline_init_documents_a_tree_with_stale_records() {
    let repo = Repo::plain(ceiling_config(), Some(&baseline("removed", 5)));

    let output = repo.run(&["baseline", "init"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert!(
        stdout(&output).contains("left untouched (run ratchet baseline update to prune)"),
        "{}",
        stdout(&output)
    );
    let written: Value =
        serde_json::from_str(&repo.read("ratchet.baseline.json")).expect("baseline JSON");
    assert_eq!(
        written["rules"]["removed"]["value"], 5,
        "the stale record stays"
    );
    assert_eq!(
        written["rules"]["todo-markers"]["value"], 3,
        "the missing baseline is written"
    );
}

#[test]
fn baseline_init_strict_refuses_without_writing() {
    let stale = baseline("removed", 5);
    let repo = Repo::plain(ceiling_config(), Some(&stale));

    let output = repo.run(&["baseline", "init", "--strict"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    assert!(
        stderr(&output).contains("baseline record for unknown rule \"removed\""),
        "{}",
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("remove it or run ratchet baseline update"),
        "{}",
        stderr(&output)
    );
    assert_eq!(
        repo.read("ratchet.baseline.json"),
        stale,
        "a failed strict init writes nothing"
    );
}

#[test]
fn baseline_init_strict_writes_when_nothing_is_stale() {
    let repo = Repo::plain(ceiling_config(), None);

    let output = repo.run(&["baseline", "init", "--strict"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let written: Value =
        serde_json::from_str(&repo.read("ratchet.baseline.json")).expect("baseline JSON");
    assert_eq!(written["rules"]["todo-markers"]["value"], 3);
}

#[test]
fn baseline_update_prunes_rule_and_scope_records_and_leaves_a_green_strict_check() {
    let config = r#"{
      "rules": {
        "metric": {
          "class": "file growth", "incident": "test", "unit": "bytes",
          "direction": "ceiling", "description": "All Java sources", "aggregate": "max",
          "count": {"file_metrics": {"files": ["**/*.java"], "metric": "size_bytes"}},
          "scopes": [
            {"id": "auth", "description": "Auth", "files": ["src/auth/**"]},
            {"id": "legacy", "description": "Legacy", "files": ["src/legacy/**"]}
          ]
        }
      }
    }"#;
    let before = r#"{
      "version": 1,
      "rules": {
        "removed": {"value": 1},
        "metric": {
          "value": 4,
          "scopes": {
            "auth": {"value": 3},
            "gone": {"value": 9},
            "legacy": {"value": 7}
          }
        }
      }
    }"#;
    let repo = Repo::plain(config, Some(before));
    repo.write("src/auth/Login.java", "abc");
    repo.write("src/Other.java", "1234");

    let output = repo.run(&["baseline", "update"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.contains("rule \"removed\" is no longer configured — pruned"),
        "{text}"
    );
    assert!(
        text.contains("scope \"gone\" is no longer configured — pruned"),
        "{text}"
    );
    assert!(
        text.contains("scope \"legacy\" selects no files — pruned"),
        "{text}"
    );
    let written: Value =
        serde_json::from_str(&repo.read("ratchet.baseline.json")).expect("baseline JSON");
    assert!(written["rules"].get("removed").is_none(), "{written}");
    assert!(written["rules"]["metric"]["scopes"].get("gone").is_none());
    assert!(written["rules"]["metric"]["scopes"].get("legacy").is_none());
    assert_eq!(written["rules"]["metric"]["scopes"]["auth"]["value"], 3);
    assert_eq!(written["rules"]["metric"]["value"], 4);

    // The engine never mutates the config: the project drops the scope that
    // no longer selects a file.  The record is already pruned, so the run is
    // green and nothing stale is left.
    let cleaned = config.replace(
        r#",
            {"id": "legacy", "description": "Legacy", "files": ["src/legacy/**"]}"#,
        "",
    );
    repo.write("ratchet.config.jsonc", &cleaned);
    let check = repo.run(&["check", "--strict"]);
    assert_eq!(code(&check), 0, "{}", stderr(&check));
    let notes = stderr(&check);
    assert!(!notes.contains("unknown rule"), "{notes}");
    assert!(!notes.contains("is not configured"), "{notes}");
    assert!(!notes.contains("selects no files"), "{notes}");
}

#[test]
fn a_reintroduced_scope_gets_the_fresh_value_after_a_prune() {
    let config = r#"{
      "rules": {
        "metric": {
          "class": "file growth", "incident": "test", "unit": "bytes",
          "direction": "ceiling", "description": "All Java sources", "aggregate": "max",
          "count": {"file_metrics": {"files": ["**/*.java"], "metric": "size_bytes"}},
          "scopes": [{"id": "legacy", "description": "Legacy", "files": ["src/legacy/**"]}]
        }
      }
    }"#;
    let before = r#"{
      "version": 1,
      "rules": {
        "metric": {
          "value": 4,
          "scopes": {"legacy": {"value": 7}}
        }
      }
    }"#;
    let repo = Repo::plain(config, Some(before));
    repo.write("src/Other.java", "1234");
    // The scope selects nothing: the old record is stale.
    let pruned = repo.run(&["baseline", "update"]);
    assert_eq!(code(&pruned), 0, "{}", stderr(&pruned));
    assert!(!repo.read("ratchet.baseline.json").contains("\"legacy\""));

    // The scope comes back with a file: the fresh value is 5, not the old 7.
    repo.write("src/legacy/Old.java", "12345");
    let initialized = repo.run(&["baseline", "init"]);
    assert_eq!(code(&initialized), 0, "{}", stderr(&initialized));
    let written: Value =
        serde_json::from_str(&repo.read("ratchet.baseline.json")).expect("baseline JSON");
    assert_eq!(written["rules"]["metric"]["scopes"]["legacy"]["value"], 5);
}

#[test]
fn strict_is_accepted_by_count_and_baseline_update() {
    let count_repo = Repo::plain(ceiling_config(), Some(&baseline("todo-markers", 5)));
    let counted = count_repo.run(&["count", "--strict"]);
    assert_eq!(code(&counted), 0, "{}", stderr(&counted));

    // The update stays the cleanup step: `--strict` is a no-op there.
    let update_repo = Repo::plain(
        ceiling_config(),
        Some(&baseline_with_unknown_rule("todo-markers", 3, "removed")),
    );
    let updated = update_repo.run(&["baseline", "update", "--strict"]);
    assert_eq!(code(&updated), 0, "{}", stderr(&updated));
    let written: Value =
        serde_json::from_str(&update_repo.read("ratchet.baseline.json")).expect("baseline JSON");
    assert!(written["rules"].get("removed").is_none());
    assert_eq!(written["rules"]["todo-markers"]["value"], 3);
}

#[test]
fn a_tolerance_only_run_with_zero_executed_checks_still_fails() {
    let config = r#"{
      "rules": {
        "docs": {
          "class": "documentation", "incident": "test", "unit": "files",
          "direction": "ceiling", "files": ["docs/**"],
          "count": {"command": ["echo", "1"]}
        }
      }
    }"#;
    let before = baseline_with_unknown_rule("docs", 1, "removed");
    let repo = Repo::plain(config, Some(&before));
    repo.write("src/main.rs", "fn main() {}\n");

    let output = repo.run(&["check", "src/main.rs"]);

    assert_eq!(code(&output), 1, "{}", stderr(&output));
    assert!(
        stderr(&output).contains("an empty selection is never a pass"),
        "{}",
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("baseline record for unknown rule \"removed\""),
        "{}",
        stderr(&output)
    );
    assert_eq!(repo.read("ratchet.baseline.json"), before);
}

/// A baseline with one live and one unknown rule record.
fn baseline_with_unknown_rule(rule: &str, value: i64, unknown: &str) -> String {
    format!(
        "{{\n  \"version\": 1,\n  \"rules\": {{\n    \"{rule}\": {{ \"value\": {value} }},\n    \"{unknown}\": {{ \"value\": 1 }}\n  }}\n}}\n"
    )
}

#[test]
fn a_failed_strict_init_creates_no_baseline_directory() {
    let config = r#"{
      "baseline": "data/ratchet.baseline.json",
      "rules": {
        "metric": {
          "class": "file growth", "incident": "test", "unit": "bytes",
          "direction": "ceiling", "description": "All Java sources", "aggregate": "max",
          "count": {"file_metrics": {"files": ["src/**/*.java"], "metric": "size_bytes"}},
          "scopes": [{"id": "gone", "description": "Gone", "files": ["src/gone/**"]}]
        }
      }
    }"#;
    let repo = Repo::plain(config, None);
    repo.write("src/A.java", "abc");

    let output = repo.run(&["baseline", "init", "--strict"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    assert!(
        stderr(&output).contains("scope gone (Gone) selected no files"),
        "{}",
        stderr(&output)
    );
    assert!(
        !repo.path().join("data").exists(),
        "a failed strict init creates nothing"
    );
}

/// An item counter with a dead `exclude` pattern among expansions: note,
/// never an error, and the note names the dead pattern.
#[test]
fn a_dead_exclude_pattern_is_a_note_in_every_mode() {
    let config = r#"{
      "rules": {
        "items": {
          "class": "unfinished work", "incident": "test", "unit": "files",
          "direction": "ceiling",
          "exclude": ["a/**", "b/**", "dead/**"],
          "count": {"command": ["echo", "[\"a/one.rs\",\"b/two.rs\"]"]}
        }
      }
    }"#;
    let repo = Repo::plain(config, Some(&baseline("items", 0)));

    let output = repo.run(&["check"]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let notes = stderr(&output);
    assert_eq!(
        notes
            .lines()
            .filter(|line| line.contains("matches nothing"))
            .count(),
        1,
        "exactly one dead pattern: {notes}"
    );
    assert!(
        notes.contains("exclude pattern \"dead/**\" for rule \"items\" matches nothing — ignored"),
        "{notes}"
    );
    assert!(
        !notes.contains("exclude pattern \"a/**\"") && !notes.contains("exclude pattern \"b/**\""),
        "the expanded patterns matched: {notes}"
    );

    let strict = repo.run(&["check", "--strict"]);
    assert_eq!(code(&strict), 0, "{}", stderr(&strict));
    assert!(
        stderr(&strict)
            .contains("exclude pattern \"dead/**\" for rule \"items\" matches nothing — ignored"),
        "{}",
        stderr(&strict)
    );

    // A pattern that matches an item is not a note.
    let matching = config.replace("\"dead/**\"", "\"a/**\"");
    let matching_repo = Repo::plain(&matching, Some(&baseline("items", 0)));
    let matched = matching_repo.run(&["check", "--strict"]);
    assert_eq!(code(&matched), 0, "{}", stderr(&matched));
    assert!(
        !stderr(&matched).contains("matches nothing"),
        "{}",
        stderr(&matched)
    );
}

/// The same dead-pattern note for the per-file measurement path.
#[test]
fn a_dead_exclude_pattern_on_measurements_names_the_dead_pattern() {
    let config = r#"{
      "rules": {
        "metric": {
          "class": "file growth", "incident": "test", "unit": "bytes",
          "direction": "ceiling", "description": "Measured files", "aggregate": "max",
          "exclude": ["src/a/**", "src/b/**", "vendor/**"],
          "count": {"file_metrics": {"files": ["**/*.java"], "metric": "size_bytes"}}
        }
      }
    }"#;
    let repo = Repo::plain(config, Some(&baseline("metric", 6)));
    repo.write("src/a/A.java", "a");
    repo.write("src/b/B.java", "bb");
    repo.write("src/Other.java", "123456");

    let output = repo.run(&["check"]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let notes = stderr(&output);
    assert_eq!(
        notes
            .lines()
            .filter(|line| line.contains("matches nothing"))
            .count(),
        1,
        "exactly one dead pattern: {notes}"
    );
    assert!(
        notes.contains(
            "exclude pattern \"vendor/**\" for rule \"metric\" matches nothing — ignored"
        ),
        "{notes}"
    );
    assert!(
        !notes.contains("exclude pattern \"src/a/**\"")
            && !notes.contains("exclude pattern \"src/b/**\""),
        "the expanded patterns matched: {notes}"
    );

    let strict = repo.run(&["check", "--strict"]);
    assert_eq!(code(&strict), 0, "{}", stderr(&strict));
    assert!(
        stderr(&strict).contains(
            "exclude pattern \"vendor/**\" for rule \"metric\" matches nothing — ignored"
        ),
        "{}",
        stderr(&strict)
    );
}

/// A per-file rule: `files` are measured, one scope.
fn scoped_metric_rule(
    files: &str,
    scope_id: &str,
    scope_files: &str,
    tier: Option<&str>,
) -> String {
    let tier = tier
        .map(|tier| format!("\"tier\": \"{tier}\","))
        .unwrap_or_default();
    format!(
        r#"{{"class": "file growth", "incident": "test", "unit": "bytes",
            {tier}
            "direction": "ceiling", "description": "Measured files", "aggregate": "max",
            "count": {{"file_metrics": {{"files": ["{files}"], "metric": "size_bytes"}}}},
            "scopes": [{{"id": "{scope_id}", "description": "{scope_id}", "files": ["{scope_files}"]}}]}}"#
    )
}

#[test]
fn baseline_update_with_a_rule_prunes_only_that_rules_records() {
    let config = format!(
        r#"{{"rules": {{"metric": {}, "other": {}}}}}"#,
        scoped_metric_rule("src/auth/**", "auth", "src/auth/**", None),
        scoped_metric_rule("src/other/**", "live", "src/other/**", None),
    );
    let before = r#"{
      "version": 1,
      "rules": {
        "removed": {"value": 1},
        "metric": {
          "value": 1,
          "scopes": {"auth": {"value": 3}, "gone": {"value": 9}}
        },
        "other": {
          "value": 1,
          "scopes": {"live": {"value": 5}, "stale2": {"value": 9}}
        }
      }
    }"#;
    let repo = Repo::plain(&config, Some(before));
    repo.write("src/auth/Login.java", "abc");
    repo.write("src/other/Other.java", "abc");

    let output = repo.run(&["baseline", "update", "--rule", "metric"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.contains("scope \"gone\" is no longer configured — pruned"),
        "{text}"
    );
    assert!(!text.contains("stale2"), "{text}");
    assert!(!text.contains("rule \"removed\""), "{text}");
    let written: Value =
        serde_json::from_str(&repo.read("ratchet.baseline.json")).expect("baseline JSON");
    assert!(written["rules"].get("removed").is_some(), "{written}");
    assert!(
        written["rules"]["metric"]["scopes"].get("gone").is_none(),
        "{written}"
    );
    assert_eq!(written["rules"]["metric"]["scopes"]["auth"]["value"], 3);
    assert_eq!(written["rules"]["other"]["scopes"]["stale2"]["value"], 9);
    assert_eq!(written["rules"]["other"]["scopes"]["live"]["value"], 5);
}

#[test]
fn strict_isolation_ignores_stale_artifacts_outside_the_selection() {
    // `--rule` ignores another rule's stale scope.
    let rule_config = r#"{
      "rules": {
        "metric": {
          "class": "unfinished work", "incident": "test", "unit": "markers",
          "direction": "ceiling", "count": {"command": ["echo", "3"]}
        },
        "other": {
          "class": "unfinished work", "incident": "test", "unit": "markers",
          "direction": "ceiling", "count": {"command": ["echo", "3"]}
        }
      }
    }"#;
    let rule_baseline = r#"{
      "version": 1,
      "rules": {
        "metric": {"value": 3},
        "other": {"value": 3, "scopes": {"stale": {"value": 9}}}
      }
    }"#;
    let by_rule = Repo::plain(rule_config, Some(rule_baseline));
    let rule_run = by_rule.run(&["check", "--strict", "--rule", "metric"]);
    assert_eq!(code(&rule_run), 0, "{}", stderr(&rule_run));
    assert!(
        !stderr(&rule_run).contains("stale"),
        "{}",
        stderr(&rule_run)
    );

    // `--tier slow` ignores a fast rule's stale scope.
    let tier_config = format!(
        r#"{{"rules": {{"metric": {}, "slow": {}}}}}"#,
        scoped_metric_rule("src/auth/**", "auth", "src/auth/**", Some("fast")),
        scoped_metric_rule("src/slow/**", "slow", "src/slow/**", Some("slow")),
    );
    let tier_baseline = r#"{
      "version": 1,
      "rules": {
        "metric": {
          "value": 3,
          "scopes": {"auth": {"value": 3}, "stale": {"value": 9}}
        },
        "slow": {"value": 3, "scopes": {"slow": {"value": 3}}}
      }
    }"#;
    let by_tier = Repo::plain(&tier_config, Some(tier_baseline));
    by_tier.write("src/auth/Login.java", "abc");
    by_tier.write("src/slow/Slow.java", "abc");
    let tier_run = by_tier.run(&["check", "--strict", "--tier", "slow"]);
    assert_eq!(code(&tier_run), 0, "{}", stderr(&tier_run));
    assert!(
        !stderr(&tier_run).contains("stale"),
        "{}",
        stderr(&tier_run)
    );

    // A path selection ignores a rule whose scope does not intersect it.
    let path_config = r#"{
      "rules": {
        "metric": {
          "class": "unfinished work", "incident": "test", "unit": "markers",
          "direction": "ceiling", "files": ["src/auth/**"],
          "count": {"command": ["echo", "3"]}
        },
        "other": {
          "class": "unfinished work", "incident": "test", "unit": "markers",
          "direction": "ceiling", "files": ["src/other/**"],
          "count": {"command": ["echo", "3"]}
        }
      }
    }"#;
    let path_baseline = r#"{
      "version": 1,
      "rules": {
        "metric": {"value": 3},
        "other": {"value": 3, "scopes": {"stale": {"value": 9}}}
      }
    }"#;
    let by_path = Repo::plain(path_config, Some(path_baseline));
    by_path.write("src/auth/Login.java", "abc");
    let path_run = by_path.run(&["check", "--strict", "src/auth/Login.java"]);
    assert_eq!(code(&path_run), 0, "{}", stderr(&path_run));
    assert!(
        !stderr(&path_run).contains("stale"),
        "{}",
        stderr(&path_run)
    );
}

#[test]
fn baseline_init_strict_refuses_on_an_unknown_scope_record() {
    let before = r#"{
      "version": 1,
      "rules": {
        "metric": {
          "value": 6,
          "scopes": {"auth": {"value": 3}, "gone": {"value": 9}}
        }
      }
    }"#;
    let repo = Repo::plain(STALE_SCOPED_CONFIG, Some(before));
    write_stale_java(&repo);

    let output = repo.run(&["baseline", "init", "--strict"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    assert!(
        stderr(&output).contains("baseline scope \"gone\" for rule \"metric\" is not configured"),
        "{}",
        stderr(&output)
    );
    assert_eq!(
        repo.read("ratchet.baseline.json"),
        before,
        "a failed strict init writes nothing"
    );
}

#[test]
fn default_baseline_init_reports_stale_scopes_as_ignored_actions() {
    let before = r#"{
      "version": 1,
      "rules": {
        "metric": {
          "value": 6,
          "scopes": {"auth": {"value": 3}, "gone": {"value": 9}}
        }
      }
    }"#;
    let repo = Repo::plain(EMPTY_SCOPE_CONFIG, Some(before));
    write_stale_java(&repo);

    let output = repo.run(&["baseline", "init", "--format", "json"]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let report: Value = serde_json::from_slice(&output.stdout).expect("JSON report");
    let actions = report["rules"].as_array().expect("actions");
    let ignored: Vec<(&str, &str)> = actions
        .iter()
        .filter(|action| action["action"] == "ignored")
        .map(|action| {
            (
                action["id"].as_str().expect("id"),
                action["action"].as_str().expect("action"),
            )
        })
        .collect();
    assert!(ignored.contains(&("metric#gone", "ignored")), "{actions:?}");
    assert!(
        ignored.contains(&("metric#legacy", "ignored")),
        "{actions:?}"
    );
    assert_eq!(
        ignored.len(),
        2,
        "only the two stale artifacts: {actions:?}"
    );
}

#[test]
fn a_pruned_empty_scope_still_fails_a_strict_check_until_the_config_is_cleaned() {
    let config = r#"{
      "rules": {
        "metric": {
          "class": "file growth", "incident": "test", "unit": "bytes",
          "direction": "ceiling", "description": "All Java sources", "aggregate": "max",
          "count": {"file_metrics": {"files": ["**/*.java"], "metric": "size_bytes"}},
          "scopes": [
            {"id": "auth", "description": "Auth", "files": ["src/auth/**"]},
            {"id": "legacy", "description": "Legacy", "files": ["src/legacy/**"]}
          ]
        }
      }
    }"#;
    let before = r#"{
      "version": 1,
      "rules": {
        "metric": {
          "value": 4,
          "scopes": {"auth": {"value": 3}, "legacy": {"value": 7}}
        }
      }
    }"#;
    let repo = Repo::plain(config, Some(before));
    repo.write("src/auth/Login.java", "abc");

    let pruned = repo.run(&["baseline", "update"]);
    assert_eq!(code(&pruned), 0, "{}", stderr(&pruned));
    assert!(!repo.read("ratchet.baseline.json").contains("\"legacy\""));

    // The engine never mutates the config: the scope is still configured and
    // still selects no file, so the strict check keeps failing.
    let check = repo.run(&["check", "--strict"]);
    assert_eq!(code(&check), 2, "{}", stderr(&check));
    assert!(
        stdout(&check).contains("scope legacy (Legacy) selected no files"),
        "{}",
        stdout(&check)
    );
}

#[test]
fn a_scope_whose_files_are_all_excluded_is_not_empty() {
    let config = r#"{
      "rules": {
        "metric": {
          "class": "file growth", "incident": "test", "unit": "bytes",
          "direction": "ceiling", "description": "All Java sources", "aggregate": "max",
          "exclude": ["src/auth/**"],
          "count": {"file_metrics": {"files": ["**/*.java"], "metric": "size_bytes"}},
          "scopes": [{"id": "auth", "description": "Auth", "files": ["src/auth/**"]}]
        }
      }
    }"#;
    let before = r#"{
      "version": 1,
      "rules": {
        "metric": {
          "value": 6,
          "scopes": {"auth": {"value": 3}}
        }
      }
    }"#;
    let repo = Repo::plain(config, Some(before));
    repo.write("src/auth/Login.java", "abc");
    repo.write("src/Other.java", "123456");

    let checked = repo.run(&["check"]);
    assert_eq!(code(&checked), 0, "{}", stderr(&checked));
    assert!(
        !stderr(&checked).contains("selects no files"),
        "{}",
        stderr(&checked)
    );
    assert!(
        !stderr(&checked).contains("matches nothing"),
        "{}",
        stderr(&checked)
    );

    let updated = repo.run(&["baseline", "update"]);
    assert_eq!(code(&updated), 0, "{}", stderr(&updated));
    let written: Value =
        serde_json::from_str(&repo.read("ratchet.baseline.json")).expect("baseline JSON");
    assert_eq!(
        written["rules"]["metric"]["scopes"]["auth"]["value"], 3,
        "the scope still matches files: it is never pruned"
    );

    let strict = repo.run(&["check", "--strict"]);
    assert_eq!(code(&strict), 0, "{}", stderr(&strict));
    assert!(
        !stderr(&strict).contains("selects no files"),
        "{}",
        stderr(&strict)
    );
}

#[test]
fn the_stylish_verdict_names_a_stale_only_failure() {
    let baseline = r#"{
      "version": 1,
      "rules": {
        "todo-markers": {"value": 3},
        "removed": {"value": 1}
      }
    }"#;
    let repo = Repo::plain(ceiling_config(), Some(baseline));

    let output = repo.run(&["check", "--strict"]);

    assert_eq!(code(&output), 2, "{}", stderr(&output));
    assert!(
        stdout(&output).contains("NOT COHERENT — 0 of 1 gates failed, 1 stale artifact"),
        "{}",
        stdout(&output)
    );

    let two = r#"{
      "version": 1,
      "rules": {
        "todo-markers": {"value": 3},
        "removed": {"value": 1},
        "also-removed": {"value": 2}
      }
    }"#;
    let plural = Repo::plain(ceiling_config(), Some(two));
    let output = plural.run(&["check", "--strict"]);
    assert_eq!(code(&output), 2, "{}", stderr(&output));
    assert!(
        stdout(&output).contains("NOT COHERENT — 0 of 1 gates failed, 2 stale artifacts"),
        "{}",
        stdout(&output)
    );

    // A failing gate and a stale artifact at once: the count of stale
    // artifacts is appended to a headline that already names a failed gate
    // (SPEC 6.1).
    let mixed_baseline = r#"{
      "version": 1,
      "rules": {
        "todo-markers": {"value": 2},
        "removed": {"value": 1}
      }
    }"#;
    let mixed = Repo::plain(ceiling_config(), Some(mixed_baseline));
    let output = mixed.run(&["check", "--strict"]);
    assert_eq!(code(&output), 2, "{}", stderr(&output));
    assert!(
        stdout(&output).contains("NOT COHERENT — 1 of 1 gates failed, 1 stale artifact"),
        "{}",
        stdout(&output)
    );
}
