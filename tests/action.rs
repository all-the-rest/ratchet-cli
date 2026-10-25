//! The GitHub Action: the install step, the two modes, the drift guard.
//!
//! The tests never touch the network: the install step is pointed at a fake
//! release that a local socket serves.  The mode scripts run the **real**
//! binary — the one the test suite built — so the resolution of the baseline
//! path is the resolution of the binary and not a second implementation.

mod common;

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use common::TempDir;
use ratchet::update::{self, Platform, Version};

/// The root of the crate; the files of the action are read from there.
const ROOT: &str = env!("CARGO_MANIFEST_DIR");

/// The path of a file of the repository.
fn path_of(name: &str) -> PathBuf {
    Path::new(ROOT).join(name)
}

/// A path as the Bash scripts expect to receive it.
fn bash_path(path: &Path) -> String {
    let path = path.to_string_lossy();
    #[cfg(windows)]
    {
        path.replace('\\', "/")
    }
    #[cfg(not(windows))]
    {
        path.into_owned()
    }
}

/// Starts the Git Bash found on PATH, rather than a system `bash` launcher.
///
/// On Windows, Rust searches system directories before the parent PATH when
/// `Command::new` has no explicit child PATH. Supplying the inherited PATH
/// makes the runner's Git Bash entry take precedence.
fn bash_command() -> Command {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut command = Command::new("bash");
    command.env("PATH", path);
    command
}

/// A path with platform-specific separators normalized for comparison.
fn normalized_path(path: &str) -> String {
    #[cfg(windows)]
    {
        path.replace('\\', "/")
    }
    #[cfg(not(windows))]
    {
        path.to_string()
    }
}

/// A path quoted for use as a Bash word.
fn bash_quote(path: &Path) -> String {
    format!("'{}'", bash_path(path).replace('\'', "'\\''"))
}

/// The action itself.
const ACTION: &str = "action.yml";

/// The install step of the action.
const INSTALL: &str = "action/install.sh";

/// The check of the mode of the action.
const MODE: &str = "action/mode.sh";

/// The validate mode of the action.
const VALIDATE: &str = "action/validate.sh";

/// The update mode of the action.
const UPDATE: &str = "action/update.sh";

/// The release workflow: the platforms of the release.
const WORKFLOW: &str = ".github/workflows/release.yml";

/// A config with one ceiling rule that counts three.
const CEILING_CONFIG: &str = r#"[
  {
    "name": "test",
    "rules": {
      "counted": {
        "class": "a class",
        "incident": "an instance",
        "unit": "things",
        "direction": "ceiling",
        "count": { "command": ["echo", "3"] }
      }
    }
  }
]
"#;

/// A config with a ceiling that pays down and a floor that refuses.
const MIXED_CONFIG: &str = r#"[
  {
    "name": "test",
    "rules": {
      "counted": {
        "class": "a class",
        "incident": "an instance",
        "unit": "things",
        "direction": "ceiling",
        "count": { "command": ["echo", "3"] }
      },
      "tests": {
        "class": "coverage",
        "incident": "a test was deleted",
        "unit": "tests",
        "direction": "floor",
        "count": { "command": ["echo", "1"] }
      }
    }
  }
]
"#;

/// The baseline of the test project: the count is three, the baseline is
/// five — the update pays it down.
const BASELINE: &str =
    "{\n  \"version\": 1,\n  \"rules\": {\n    \"counted\": { \"value\": 5 }\n  }\n}\n";

/// The baseline of the mixed project: the ceiling pays down, the floor
/// refuses.
const MIXED_BASELINE: &str = "{\n  \"version\": 1,\n  \"rules\": {\n  \"counted\": { \"value\": 5 },\n  \"tests\": { \"value\": 2 }\n}\n}\n";

/// The stdout of a run.
fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The stderr of a run.
fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Reads a file of the repository.
fn read(name: &str) -> String {
    fs::read_to_string(path_of(name))
        .unwrap_or_else(|error| panic!("{} cannot be read: {error}", path_of(name).display()))
}

/// The version of this crate, read from the manifest.
///
/// The version is **read** and not written down here: the default of the
/// action has to name the release of this manifest.
fn manifest_version() -> String {
    let manifest = read("Cargo.toml");
    let mut section = String::new();
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            section = trimmed.to_string();
            continue;
        }
        if section != "[package]" {
            continue;
        }
        let Some(rest) = trimmed.strip_prefix("version") else {
            continue;
        };
        let rest = rest.trim_start().strip_prefix('=').unwrap_or(rest).trim();
        let rest = rest.strip_prefix('"').unwrap_or(rest);
        let rest = rest.strip_suffix('"').unwrap_or(rest);
        if !rest.is_empty() {
            return rest.to_string();
        }
    }
    panic!("the manifest of this crate has no version");
}

/// Fills the placeholders of a template.
fn substitute(template: &str, values: &[(&str, &str)]) -> String {
    let mut result = template.to_string();
    for (key, value) in values {
        result = result.replace(&format!("{{{key}}}"), value);
    }
    result
}

/// The value of a variable of a script.
///
/// The variable is the first line that assigns it; the value is unquoted.
fn value_of(script: &str, key: &str) -> String {
    let prefix = format!("{key}=");
    script
        .lines()
        .filter_map(|line| {
            let rest = line.trim().strip_prefix(&prefix)?;
            let value = rest.trim();
            let value = value.strip_prefix('\'').unwrap_or(value);
            let value = value.strip_suffix('\'').unwrap_or(value);
            let value = value.strip_prefix('"').unwrap_or(value);
            let value = value.strip_suffix('"').unwrap_or(value);
            Some(value.to_string())
        })
        .next()
        .unwrap_or_else(|| panic!("the script has no {key}"))
}

/// The values of a key of the workflow, in the order of the file.
fn values_of(workflow: &str, key: &str) -> Vec<String> {
    let prefix = format!("{key}:");
    workflow
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            let rest = trimmed.strip_prefix(&prefix)?;
            let value = rest.trim();
            let value = value.strip_prefix('\'').unwrap_or(value);
            let value = value.strip_suffix('\'').unwrap_or(value);
            let value = value.strip_prefix('"').unwrap_or(value);
            let value = value.strip_suffix('"').unwrap_or(value);
            Some(value.to_string())
        })
        .collect()
}

/// A step of a composite action.
struct Step {
    /// The name of the step.
    name: String,
    /// The condition of the step (`if:`), if it has one.
    condition: Option<String>,
    /// The command of the step (`run:`).
    command: String,
}

/// The steps of a composite action, in the order of the file.
///
/// The steps are read as blocks: a step owns everything up to the next step,
/// so the condition of a step belongs to **that** step and not to the file.
fn steps(yaml: &str) -> Vec<Step> {
    let lines: Vec<&str> = yaml.lines().collect();
    let starts: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.trim_start().starts_with("- name:"))
        .map(|(index, _)| index)
        .collect();
    let mut steps = Vec::new();
    for (position, start) in starts.iter().enumerate() {
        let end = starts.get(position + 1).copied().unwrap_or(lines.len());
        let block = &lines[*start..end];
        let name = block[0]
            .trim_start()
            .trim_start_matches("- name:")
            .trim()
            .to_string();
        let condition = block.iter().find_map(|line| {
            line.trim()
                .strip_prefix("if:")
                .map(|rest| rest.trim().to_string())
        });
        let command = match block
            .iter()
            .position(|line| line.trim_start().starts_with("run:"))
        {
            Some(index) => {
                let inline = block[index]
                    .trim_start()
                    .trim_start_matches("run:")
                    .trim()
                    .to_string();
                if inline.is_empty() {
                    block_of(&block[index + 1..])
                } else {
                    inline
                }
            }
            None => String::new(),
        };
        steps.push(Step {
            name,
            condition,
            command,
        });
    }
    steps
}

/// The block of lines below a key, with the indentation of the block removed.
fn block_of(lines: &[&str]) -> String {
    let indentation = lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.len() - line.trim_start().len())
        .min()
        .unwrap_or(0);
    lines
        .iter()
        .map(|line| {
            if line.trim().is_empty() {
                String::new()
            } else {
                line[indentation..].to_string()
            }
        })
        .collect::<Vec<String>>()
        .join("\n")
}

/// The inputs the action declares: the name and the default of every input.
fn declared_inputs() -> Vec<(String, Option<String>)> {
    let action = read(ACTION);
    let mut inputs: Vec<(String, Option<String>)> = Vec::new();
    let mut inside = false;
    for line in action.lines() {
        if line == "inputs:" {
            inside = true;
            continue;
        }
        if !inside {
            continue;
        }
        if !line.starts_with(' ') {
            break;
        }
        let indentation = line.len() - line.trim_start().len();
        let trimmed = line.trim();
        if indentation == 2 && trimmed.ends_with(':') {
            inputs.push((trimmed.trim_end_matches(':').to_string(), None));
        } else if indentation == 4 {
            if let Some(value) = trimmed.strip_prefix("default:") {
                if let Some(input) = inputs.last_mut() {
                    let value = value.trim();
                    let value = value.strip_prefix('\'').unwrap_or(value);
                    let value = value.strip_suffix('\'').unwrap_or(value);
                    let value = value.strip_prefix('"').unwrap_or(value);
                    let value = value.strip_suffix('"').unwrap_or(value);
                    input.1 = Some(value.to_string());
                }
            }
        }
    }
    inputs
}

/// The default of an input of the action.
fn default_of(inputs: &[(String, Option<String>)], name: &str) -> Option<String> {
    inputs
        .iter()
        .find(|(input, _)| input == name)
        .and_then(|(_, value)| value.clone())
}

/// A fake release: a local socket that answers with the given routes.
struct FakeRelease {
    /// The base of the assets.
    base: String,
}

impl FakeRelease {
    /// Starts a server that serves the given routes.
    ///
    /// The routes are the path and the body of an answer.
    fn start(routes: &[(String, Vec<u8>)]) -> FakeRelease {
        let listener = TcpListener::bind("127.0.0.1:0").expect("the socket is bound");
        let address = listener.local_addr().expect("the address is known");
        let routes: Vec<(String, Vec<u8>)> = routes.to_vec();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else {
                    continue;
                };
                let path = read_path(&mut stream);
                let body = routes
                    .iter()
                    .find(|(route, _)| *route == path)
                    .map(|(_, body)| body.clone());
                let mut response = match &body {
                    Some(body) => head("200 OK", body.len()),
                    None => head("404 Not Found", 0),
                };
                if let Some(body) = body {
                    response.extend_from_slice(&body);
                }
                let _ = stream.write_all(&response);
                let _ = stream.flush();
            }
        });
        FakeRelease {
            base: format!("http://{address}/download"),
        }
    }
}

/// The head of a response.
fn head(status: &str, length: usize) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status}\r\nContent-Length: {length}\r\n\
         Content-Type: application/octet-stream\r\nConnection: close\r\n\r\n"
    )
    .into_bytes()
}

/// The path of a request, read from its request line.
fn read_path(stream: &mut TcpStream) -> String {
    let mut request = Vec::new();
    let mut buffer = [0u8; 1024];
    loop {
        let Ok(read) = stream.read(&mut buffer) else {
            return String::new();
        };
        if read == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..read]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let text = String::from_utf8_lossy(&request);
    let line = text.lines().next().unwrap_or_default();
    let mut fields = line.split_whitespace();
    let _method = fields.next();
    let path = fields.next().unwrap_or_default();
    path.split('?').next().unwrap_or_default().to_string()
}

/// True when the machine can download a file: the install step needs a
/// downloader, and the test skips itself when there is none.
fn downloader_available() -> bool {
    ["curl", "wget"].iter().any(|program| {
        Command::new(program)
            .arg("--version")
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    })
}

/// True when `node` is on the PATH: a JavaScript config is evaluated with it.
fn node_available() -> bool {
    Command::new("node")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// The name of the asset of a platform, as the release names it.
fn asset_of(os: &str, arch: &str, version: &str) -> String {
    let exe = if os == "windows" { ".exe" } else { "" };
    format!("ratchet-{version}-{os}-{arch}{exe}")
}

/// The routes of a fake release: the asset and the checksums of a tag.
fn routes(tag: &str, asset: &str, body: Vec<u8>, checksums: &str) -> Vec<(String, Vec<u8>)> {
    vec![
        (format!("/download/{tag}/{asset}"), body),
        (
            format!("/download/{tag}/{}", update::CHECKSUMS_NAME),
            checksums.as_bytes().to_vec(),
        ),
    ]
}

/// The result of an install run.
struct Install {
    /// The output of the script.
    output: Output,
    /// The directory of the run; it holds the installed binary.
    _directory: TempDir,
    /// The directory the binary was installed into.
    bin: PathBuf,
    /// The file that holds the PATH of the following steps.
    path_file: PathBuf,
    /// The operating system of the simulated runner.
    runner_os: String,
}

impl Install {
    /// The binary that was installed.
    fn binary(&self) -> PathBuf {
        let executable = if self.runner_os == "Windows" {
            "ratchet.exe"
        } else {
            "ratchet"
        };
        self.bin.join(executable)
    }

    /// The PATH of the following steps.
    fn path(&self) -> String {
        fs::read_to_string(&self.path_file).unwrap_or_default()
    }
}

/// Runs the install step against a fake release.
fn run_install(release: &FakeRelease, version: &str, os: &str, arch: &str) -> Install {
    let directory = TempDir::new().expect("a temporary directory");
    let install = directory.path().join("bin");
    let path_file = directory.path().join("github-path");
    fs::write(&path_file, "").expect("the path file is written");
    let output = bash_command()
        .arg(bash_path(&path_of(INSTALL)))
        .env("RUNNER_OS", os)
        .env("RUNNER_ARCH", arch)
        .env("RATCHET_VERSION", version)
        .env("RATCHET_ASSET_BASE", &release.base)
        .env("RATCHET_INSTALL_DIR", &install)
        .env("GITHUB_PATH", &path_file)
        .output()
        .expect("the install script runs");
    Install {
        output,
        _directory: directory,
        bin: install,
        path_file,
        runner_os: os.to_string(),
    }
}

/// A stub of the binary: it records its arguments.
///
/// The stub is the binary of the validate mode: it never counts anything, it
/// only leaves a trace the test can read.
fn stub(directory: &Path, log: &Path) -> PathBuf {
    let bin = directory.join("stub-bin");
    fs::create_dir_all(&bin).expect("the directory is created");
    let script = format!(
        "#!/usr/bin/env bash\n\
         printf '%s\\n' \"$*\" >>{log}\n\
         exit \"${{RATCHET_STUB_EXIT:-0}}\"\n",
        log = bash_quote(log)
    );
    let path = bin.join("ratchet");
    fs::write(&path, script).expect("the stub is written");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
            .expect("the stub is executable");
    }
    bin
}

/// The PATH with the given directory in front of it.
fn path_with(bin: &Path) -> OsString {
    let current = std::env::var_os("PATH").unwrap_or_default();
    path_with_from(bin, &current)
}

/// The PATH with the given directory in front of the supplied path list.
fn path_with_from(bin: &Path, current: &OsStr) -> OsString {
    let mut paths = vec![bin.to_path_buf()];
    paths.extend(std::env::split_paths(current));
    std::env::join_paths(paths).expect("the PATH can be joined")
}

/// The PATH with the **real** binary in front of it.
///
/// The mode scripts call `ratchet`: the tests put the binary that the test
/// suite built on the PATH, so the resolution of the baseline path is the
/// resolution of the binary.
fn path_with_the_binary() -> OsString {
    let binary = Path::new(env!("CARGO_BIN_EXE_ratchet"));
    let directory = binary.parent().expect("the binary has a directory");
    path_with(directory)
}

#[test]
fn path_with_uses_the_native_path_list_format() {
    let bin = PathBuf::from("test-bin");
    let actual_path = path_with(&bin);
    let actual: Vec<PathBuf> = std::env::split_paths(&actual_path).collect();
    let mut expected = vec![bin];
    expected.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    assert_eq!(actual, expected);
}

#[cfg(unix)]
#[test]
fn path_with_preserves_non_utf8_entries() {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    let bin = PathBuf::from("test-bin");
    let non_utf8_entry = OsString::from_vec(b"/path/with/\xFF".to_vec());
    let current = std::env::join_paths([&non_utf8_entry]).expect("the PATH can be joined");

    let actual_path = path_with_from(&bin, &current);
    let actual: Vec<PathBuf> = std::env::split_paths(&actual_path).collect();

    assert_eq!(actual, vec![bin, PathBuf::from(&non_utf8_entry)]);
    assert_eq!(actual[1].as_os_str().as_bytes(), b"/path/with/\xFF");
}

#[test]
fn bash_paths_use_forward_slashes_on_windows() {
    #[cfg(windows)]
    assert_eq!(
        bash_path(Path::new(r"D:\runner\work\action\mode.sh")),
        "D:/runner/work/action/mode.sh"
    );
    #[cfg(not(windows))]
    assert_eq!(
        bash_path(Path::new("runner/work/action/mode.sh")),
        "runner/work/action/mode.sh"
    );
}

#[test]
fn bash_quoted_paths_escape_single_quotes() {
    assert_eq!(
        bash_quote(Path::new("a path/'quoted'")),
        r#"'a path/'\''quoted'\'''"#
    );
}

/// The arguments the stub recorded.
fn recorded(log: &Path) -> Vec<String> {
    let text = fs::read_to_string(log).unwrap_or_default();
    text.lines().map(|line| line.to_string()).collect()
}

/// Runs a git command in a repository.
fn git(directory: &Path, arguments: &[&str]) -> Output {
    Command::new("git")
        .args(arguments)
        .current_dir(directory)
        .env("GIT_AUTHOR_NAME", "ratchet test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "ratchet test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .expect("git runs")
}

/// The text a git command printed.
fn git_text(directory: &Path, arguments: &[&str]) -> String {
    let output = git(directory, arguments);
    assert!(
        output.status.success(),
        "git {arguments:?} failed: {}",
        stderr(&output)
    );
    stdout(&output)
}

/// A project with a config and a baseline, both committed.
///
/// The config may live in a subdirectory — a package of a monorepo — and may
/// name its baseline.
fn project(config_name: &str, config: &str, baseline_path: &str) -> TempDir {
    let directory = TempDir::new().expect("a temporary directory");
    write(&directory.path().join(config_name), config);
    write(&directory.path().join(baseline_path), BASELINE);
    git(directory.path(), &["init", "-q", "."]);
    git(directory.path(), &["add", "-A"]);
    git(directory.path(), &["commit", "-q", "-m", "init"]);
    directory
}

/// Writes a file, creating the directories above it.
fn write(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("the directory is created");
    }
    fs::write(path, content).expect("the file is written");
}

/// The files of the last commit of a project.
fn committed(project: &TempDir) -> Vec<String> {
    git_text(
        project.path(),
        &["show", "--name-only", "--pretty=", "HEAD"],
    )
    .lines()
    .map(|line| line.to_string())
    .filter(|line| !line.is_empty())
    .collect()
}

/// The content of a file in the last commit of a project.
fn committed_content(project: &TempDir, path: &str) -> String {
    git_text(project.path(), &["show", &format!("HEAD:{path}")])
}

/// Runs the update mode in a project.
fn run_update(project: &TempDir, config: &str, reason: &str) -> Output {
    bash_command()
        .arg(bash_path(&path_of(UPDATE)))
        .current_dir(project.path())
        .env("PATH", path_with_the_binary())
        .env("RATCHET_NO_UPDATE_CHECK", "1")
        .env("RATCHET_CONFIG", config)
        .env("RATCHET_REASON", reason)
        // The action sets the identity of the commit itself (`-c user.name`).
        // An ambient Git identity would override it, and the test would read
        // the identity of the machine instead of the action's.
        .env_remove("GIT_AUTHOR_NAME")
        .env_remove("GIT_AUTHOR_EMAIL")
        .env_remove("GIT_COMMITTER_NAME")
        .env_remove("GIT_COMMITTER_EMAIL")
        .output()
        .expect("the update script runs")
}

#[test]
fn the_action_declares_its_inputs() {
    let inputs = declared_inputs();
    let names: Vec<&str> = inputs.iter().map(|(name, _)| name.as_str()).collect();
    for expected in [
        "mode",
        "version",
        "config",
        "tier",
        "args",
        "baseline-reason",
    ] {
        assert!(
            names.contains(&expected),
            "the action does not declare {expected}: {names:?}"
        );
    }
    assert_eq!(
        default_of(&inputs, "mode").as_deref(),
        Some("validate"),
        "the default mode validates"
    );
    assert_eq!(
        default_of(&inputs, "config").as_deref(),
        Some("ratchet.config.jsonc")
    );
    assert_eq!(
        default_of(&inputs, "tier").as_deref(),
        Some(""),
        "an empty tier runs every tier"
    );
    assert_eq!(default_of(&inputs, "args").as_deref(), Some(""));
    assert_eq!(default_of(&inputs, "baseline-reason").as_deref(), Some(""));
}

#[test]
fn the_default_version_is_the_version_of_the_manifest() {
    let inputs = declared_inputs();
    let version = default_of(&inputs, "version").expect("the version has a default");
    assert_eq!(
        version,
        format!("v{}", manifest_version()),
        "the action installs the release of this manifest"
    );
}

#[test]
fn the_asset_names_of_the_action_match_the_binary() {
    let script = read(INSTALL);
    let template = value_of(&script, "ASSET_TEMPLATE");
    let workflow = read(WORKFLOW);
    let systems = values_of(&workflow, "asset_os");
    let architectures = values_of(&workflow, "asset_arch");
    let executables = values_of(&workflow, "asset_exe");
    let count = systems.len();
    assert!(count > 0, "the release builds something");
    assert_eq!(
        architectures.len(),
        count,
        "every build has an architecture"
    );
    assert_eq!(executables.len(), count, "every build has an executable");

    // The version of the manifest is the version of the release.
    let version = manifest_version();
    let parsed = Version::parse(&version).expect("the version parses");
    for index in 0..count {
        let platform = Platform {
            os: systems[index].clone(),
            arch: architectures[index].clone(),
        };
        let asset = substitute(
            &template,
            &[
                ("version", &version),
                ("os", &systems[index]),
                ("arch", &architectures[index]),
                ("exe", &executables[index]),
            ],
        );
        assert_eq!(
            asset,
            update::asset_name(&parsed, &platform),
            "the asset name of {platform:?} differs"
        );
    }
}

#[test]
fn the_install_step_downloads_from_the_release_of_the_repository() {
    let script = read(INSTALL);
    assert!(
        script.contains(update::DEFAULT_ASSET_BASE),
        "the install step downloads from the releases of this repository"
    );
}

#[test]
fn the_install_step_verifies_the_checksum() {
    let script = read(INSTALL);
    assert!(
        script.contains("sha256sum"),
        "the install step computes the SHA-256 with sha256sum"
    );
    assert!(
        script.contains("shasum -a 256"),
        "the install step computes the SHA-256 with shasum on macOS"
    );
    assert!(
        script.contains(update::CHECKSUMS_NAME),
        "the install step downloads the checksums of the release"
    );
}

#[test]
fn the_install_step_installs_the_binary_on_the_path() {
    if !downloader_available() {
        eprintln!("the machine has no downloader — the install step is not tested");
        return;
    }
    let version = "0.1.0";
    let asset = asset_of("linux", "x86_64", version);
    let body = b"the binary".to_vec();
    let checksums = format!("{}  {asset}\n", update::sha256_hex(&body));
    let release = FakeRelease::start(&routes(&format!("v{version}"), &asset, body, &checksums));

    let install = run_install(&release, version, "Linux", "X64");

    assert!(
        install.output.status.success(),
        "{}",
        stderr(&install.output)
    );
    assert_eq!(
        fs::read(install.binary()).expect("the binary is installed"),
        b"the binary"
    );
    let path = normalized_path(&install.path());
    let bin = bash_path(&install.bin);
    assert!(
        path.contains(&bin),
        "the directory of the binary goes on the PATH: {}",
        install.path()
    );
}

#[test]
fn the_install_step_downloads_the_asset_of_the_runner() {
    if !downloader_available() {
        eprintln!("the machine has no downloader — the install step is not tested");
        return;
    }
    let version = "0.2.0";
    let asset = asset_of("macos", "aarch64", version);
    let body = b"the binary of the mac".to_vec();
    let checksums = format!("{}  {asset}\n", update::sha256_hex(&body));
    // The release publishes the asset of macos only: a runner that asks for
    // another platform gets nothing.
    let release = FakeRelease::start(&routes(&format!("v{version}"), &asset, body, &checksums));

    let install = run_install(&release, version, "macOS", "ARM64");

    assert!(
        install.output.status.success(),
        "{}",
        stderr(&install.output)
    );
    assert_eq!(
        fs::read(install.binary()).expect("the binary is installed"),
        b"the binary of the mac"
    );
}

#[test]
fn a_wrong_checksum_fails_the_install_step() {
    if !downloader_available() {
        eprintln!("the machine has no downloader — the install step is not tested");
        return;
    }
    let version = "0.1.0";
    let asset = asset_of("linux", "x86_64", version);
    let body = b"the binary".to_vec();
    // The checksum of the release does not belong to the asset.
    let checksums = format!("{}  {asset}\n", "0".repeat(64));
    let release = FakeRelease::start(&routes(&format!("v{version}"), &asset, body, &checksums));

    let install = run_install(&release, version, "Linux", "X64");

    assert!(!install.output.status.success(), "the install must fail");
    let text = stderr(&install.output);
    assert!(text.contains("does not match"), "{text}");
    assert!(text.contains("nothing is installed"), "{text}");
    // Nothing is installed from an unverified download.
    assert!(
        !install.binary().exists(),
        "the binary must not be installed"
    );
}

#[test]
fn a_missing_checksum_fails_the_install_step() {
    if !downloader_available() {
        eprintln!("the machine has no downloader — the install step is not tested");
        return;
    }
    let version = "0.1.0";
    let asset = asset_of("linux", "x86_64", version);
    let body = b"the binary".to_vec();
    // The checksums of the release name another asset.
    let checksums = format!("{}  ratchet-0.1.0-linux-aarch64\n", "0".repeat(64));
    let release = FakeRelease::start(&routes(&format!("v{version}"), &asset, body, &checksums));

    let install = run_install(&release, version, "Linux", "X64");

    assert!(!install.output.status.success(), "the install must fail");
    let text = stderr(&install.output);
    assert!(text.contains("do not name"), "{text}");
    assert!(
        !install.binary().exists(),
        "the binary must not be installed"
    );
}

#[test]
fn a_version_that_is_no_version_fails_the_install_step() {
    if !downloader_available() {
        eprintln!("the machine has no downloader — the install step is not tested");
        return;
    }
    let release = FakeRelease::start(&[]);

    let install = run_install(&release, "garbage", "Linux", "X64");

    assert!(!install.output.status.success(), "the install must fail");
    let text = stderr(&install.output);
    // The message names the version — not the download that follows it.
    assert!(text.contains("is not a version"), "{text}");
    assert!(text.contains("MAJOR.MINOR.PATCH"), "{text}");
    assert!(!text.contains("download"), "{text}");
    assert!(
        !install.binary().exists(),
        "the binary must not be installed"
    );
}

#[test]
fn the_windows_runner_asks_for_the_executable() {
    if !downloader_available() {
        eprintln!("the machine has no downloader — the install step is not tested");
        return;
    }
    let version = "0.1.0";
    let asset = asset_of("linux", "x86_64", version);
    let body = b"the binary".to_vec();
    let checksums = format!("{}  {asset}\n", update::sha256_hex(&body));
    // The release publishes the asset of linux only.
    let release = FakeRelease::start(&routes(&format!("v{version}"), &asset, body, &checksums));

    let install = run_install(&release, version, "Windows", "X64");

    assert!(!install.output.status.success(), "the install must fail");
    let text = stderr(&install.output);
    // The name of the asset of windows carries the `.exe`.
    assert!(text.contains("ratchet-0.1.0-windows-x86_64.exe"), "{text}");
    assert!(
        !install.binary().exists(),
        "the binary must not be installed"
    );
}

#[test]
fn an_unknown_platform_fails_the_install_step() {
    if !downloader_available() {
        eprintln!("the machine has no downloader — the install step is not tested");
        return;
    }
    let release = FakeRelease::start(&[]);

    let install = run_install(&release, "0.1.0", "FreeBSD", "X64");

    assert!(!install.output.status.success(), "the install must fail");
    let text = stderr(&install.output);
    assert!(text.contains("unknown runner OS"), "{text}");
    assert!(
        !install.binary().exists(),
        "the binary must not be installed"
    );
}

#[test]
fn the_mode_check_accepts_the_two_modes() {
    for mode in ["validate", "update"] {
        let output = bash_command()
            .arg(bash_path(&path_of(MODE)))
            .env("RATCHET_MODE", mode)
            .output()
            .expect("the mode script runs");
        assert!(
            output.status.success(),
            "the mode {mode} must be accepted: {}",
            stderr(&output)
        );
    }
}

#[test]
fn an_unknown_mode_fails_the_action() {
    let output = bash_command()
        .arg(bash_path(&path_of(MODE)))
        .env("RATCHET_MODE", "whatever")
        .output()
        .expect("the mode script runs");

    assert!(!output.status.success(), "an unknown mode fails the job");
    let text = stderr(&output);
    assert!(text.contains("unknown mode"), "{text}");
    assert!(text.contains("validate"), "{text}");
    assert!(text.contains("update"), "{text}");
}

#[test]
fn a_mode_with_a_quote_does_not_run_a_command() {
    let directory = TempDir::new().expect("a temporary directory");
    let marker = directory.path().join("INJECTED");
    // The mode of a hostile workflow: it would close the quote and run a
    // command if the mode reached the shell as text.
    let mode = format!("x\"; touch \"{}\"; #", bash_path(&marker));

    let output = bash_command()
        .arg(bash_path(&path_of(MODE)))
        .current_dir(directory.path())
        .env("RATCHET_MODE", &mode)
        .output()
        .expect("the mode script runs");

    assert!(!output.status.success(), "an unknown mode fails the job");
    assert!(
        !marker.exists(),
        "the mode must not reach the shell as a command"
    );
    assert!(
        stderr(&output).contains("unknown mode"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn no_input_is_interpolated_into_a_command() {
    let action = read(ACTION);
    for line in action.lines() {
        if !line.contains("inputs.") {
            continue;
        }
        let trimmed = line.trim();
        // An `if:` is an expression of the runner, not a command of the shell.
        if trimmed.starts_with("if:") {
            continue;
        }
        assert!(
            trimmed.starts_with("RATCHET_"),
            "an input must not reach the shell as a command: {trimmed}"
        );
    }
}

/// The guard of the two mode steps.
///
/// The step that runs the validate script belongs to the validate mode, and
/// the step that runs the update script belongs to the update mode.  A swap
/// would run the wrong step in the wrong mode: in the update mode the
/// validate step would run, the update step would be skipped, and the job
/// would be **green and commit nothing**.
fn guard_the_mode_steps(action: &str) -> Result<(), String> {
    let steps = steps(action);
    let find = |script: &str| {
        steps
            .iter()
            .find(|step| step.command.contains(script))
            .map(|step| (step.name.clone(), step.condition.clone()))
    };

    let (name, condition) = find("action/validate.sh")
        .ok_or_else(|| "the action has no step that runs the validate script".to_string())?;
    if condition.as_deref() != Some("inputs.mode == 'validate'") {
        return Err(format!(
            "the step '{name}' runs the validate script, but its condition is {condition:?} \
             — it must be `inputs.mode == 'validate'`"
        ));
    }

    let (name, condition) = find("action/update.sh")
        .ok_or_else(|| "the action has no step that runs the update script".to_string())?;
    if condition.as_deref() != Some("inputs.mode == 'update'") {
        return Err(format!(
            "the step '{name}' runs the update script, but its condition is {condition:?} \
             — it must be `inputs.mode == 'update'`"
        ));
    }

    Ok(())
}

#[test]
fn the_two_modes_are_the_steps_of_the_action() {
    let action = read(ACTION);
    if let Err(error) = guard_the_mode_steps(&action) {
        panic!("{error}");
    }
    assert!(
        action.contains("using: composite"),
        "the action is a composite action"
    );
}

#[test]
fn a_swapped_condition_fails_the_guard() {
    // The mutation of the review: the two conditions are swapped.  The guard
    // must catch it — otherwise the job would be green and commit nothing.
    let action = read(ACTION);
    let swapped = action
        .replace(
            "if: inputs.mode == 'validate'",
            "if: inputs.mode == 'SWAPPED'",
        )
        .replace(
            "if: inputs.mode == 'update'",
            "if: inputs.mode == 'validate'",
        )
        .replace(
            "if: inputs.mode == 'SWAPPED'",
            "if: inputs.mode == 'update'",
        );
    assert!(swapped != action, "the mutation changed nothing");

    let error =
        guard_the_mode_steps(&swapped).expect_err("the swapped conditions must fail the guard");
    // The error names the step and the condition it carries.
    assert!(error.contains("Validate the gates"), "{error}");
    assert!(error.contains("inputs.mode == 'update'"), "{error}");
}

#[test]
fn the_check_of_the_mode_always_runs() {
    let action = read(ACTION);
    let steps = steps(&action);
    let check = steps
        .iter()
        .find(|step| step.name == "Check the mode")
        .expect("the action has the step");
    // The check is a script: the step has no `if:` of its own, so it always
    // runs — a mode that runs neither step cannot leave the job green.
    assert!(
        check.command.contains("action/mode.sh"),
        "the check of the mode is a script: {}",
        check.command
    );
    assert!(
        check.condition.is_none(),
        "the check of the mode must not be switched off: {:?}",
        check.condition
    );
}

#[test]
fn the_validate_mode_runs_the_gates_without_a_reason() {
    let directory = TempDir::new().expect("a temporary directory");
    let log = directory.path().join("log");
    let bin = stub(directory.path(), &log);

    let output = bash_command()
        .arg(bash_path(&path_of(VALIDATE)))
        .env("PATH", path_with(&bin))
        .env("RATCHET_CONFIG", "ratchet.config.jsonc")
        .env("RATCHET_TIER", "fast")
        .env("RATCHET_ARGS", "--ignore src/**")
        .output()
        .expect("the validate script runs");

    assert!(output.status.success(), "{}", stderr(&output));
    let lines = recorded(&log);
    assert_eq!(lines.len(), 1, "the binary is called once: {lines:?}");
    let line = &lines[0];
    assert!(line.contains("check"), "{line}");
    // The values are attached to their option: a value that starts with a
    // hyphen is a value and not another option.
    assert!(line.contains("--config=ratchet.config.jsonc"), "{line}");
    assert!(line.contains("--tier=fast"), "{line}");
    assert!(line.contains("--ignore src/**"), "{line}");
    // The validate mode never moves a baseline.
    assert!(!line.contains("--reason"), "{line}");
}

#[test]
fn a_failing_gate_fails_the_validate_mode() {
    let directory = TempDir::new().expect("a temporary directory");
    let log = directory.path().join("log");
    let bin = stub(directory.path(), &log);

    let output = bash_command()
        .arg(bash_path(&path_of(VALIDATE)))
        .env("PATH", path_with(&bin))
        .env("RATCHET_CONFIG", "ratchet.config.jsonc")
        .env("RATCHET_STUB_EXIT", "1")
        .output()
        .expect("the validate script runs");

    assert!(!output.status.success(), "a failing gate fails the job");
    assert_eq!(output.status.code(), Some(1), "the exit code of the gate");
}

#[test]
fn the_update_mode_moves_the_baseline_with_a_reason_and_commits_it() {
    let project = project(
        "ratchet.config.jsonc",
        CEILING_CONFIG,
        "ratchet.baseline.json",
    );

    let output = run_update(
        &project,
        "ratchet.config.jsonc",
        "the nightly run pays it down",
    );

    assert!(output.status.success(), "{}", stderr(&output));
    // The commit holds the baseline and nothing else.
    assert_eq!(committed(&project), vec!["ratchet.baseline.json"]);
    // The reason travels with the record — and with the commit.
    let content = committed_content(&project, "ratchet.baseline.json");
    assert!(
        content.contains("the nightly run pays it down"),
        "{content}"
    );
    assert!(content.contains("\"value\": 3"), "{content}");
    // The identity of the commit is the one of the action.
    let author = git_text(project.path(), &["log", "-1", "--pretty=%an <%ae>"]);
    assert!(author.contains("ratchet[bot]"), "{author}");
    let subject = git_text(project.path(), &["log", "-1", "--pretty=%s"]);
    assert_eq!(subject.trim(), "ratchet: the baseline moves");
    // The working tree is clean.
    let status = git_text(project.path(), &["status", "--porcelain"]);
    assert_eq!(status.trim(), "", "{status}");
}

#[test]
fn the_update_mode_commits_the_baseline_the_config_names() {
    let config = r#"[
      {
        "name": "test",
        "baseline": "custom/baseline.json",
        "rules": {
          "counted": {
            "class": "a class",
            "incident": "an instance",
            "unit": "things",
            "direction": "ceiling",
            "count": { "command": ["echo", "3"] }
          }
        }
      }
    ]"#;
    let project = project("ratchet.config.jsonc", config, "custom/baseline.json");

    let output = run_update(
        &project,
        "ratchet.config.jsonc",
        "the nightly run pays it down",
    );

    assert!(output.status.success(), "{}", stderr(&output));
    // The commit holds the file the config names — and nothing else.
    assert_eq!(committed(&project), vec!["custom/baseline.json"]);
}

#[test]
fn the_update_mode_commits_the_baseline_of_a_javascript_config() {
    if !node_available() {
        eprintln!(
            "*** SKIPPED: node is not on the PATH — this JavaScript test did NOT run. \
             Install node (or run the CI, which installs it) to exercise it. ***"
        );
        return;
    }
    let config = "export default [\n  {\n    name: \"test\",\n    baseline: \"custom/baseline.json\",\n    rules: {\n      counted: {\n        class: \"a class\",\n        incident: \"an instance\",\n        unit: \"things\",\n        direction: \"ceiling\",\n        count: { command: [\"echo\", \"3\"] },\n      },\n    },\n  },\n];\n";
    let project = project("ratchet.config.mjs", config, "custom/baseline.json");

    let output = run_update(
        &project,
        "ratchet.config.mjs",
        "the nightly run pays it down",
    );

    assert!(output.status.success(), "{}", stderr(&output));
    // The commit holds the file the JavaScript config names.
    assert_eq!(committed(&project), vec!["custom/baseline.json"]);
}

#[test]
fn the_update_mode_ignores_a_baseline_a_comment_names() {
    // A comment that names another baseline must not decide the file: the
    // binary resolves the config, the action asks it.
    let config = r#"[
      {
        "name": "test",
        // the baseline used to be "old/name.json"
        "baseline": "custom/baseline.json",
        "rules": {
          "counted": {
            "class": "a class",
            "incident": "an instance",
            "unit": "things",
            "direction": "ceiling",
            "count": { "command": ["echo", "3"] }
          }
        }
      }
    ]"#;
    let project = project("ratchet.config.jsonc", config, "custom/baseline.json");

    let output = run_update(
        &project,
        "ratchet.config.jsonc",
        "the nightly run pays it down",
    );

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(committed(&project), vec!["custom/baseline.json"]);
}

#[test]
fn the_update_mode_commits_the_baseline_of_a_nested_package() {
    // The config of a package of a monorepo: the baseline is next to it, and
    // the path is relative to the root of the repository.
    let project = project(
        "packages/app/ratchet.config.jsonc",
        CEILING_CONFIG,
        "packages/app/ratchet.baseline.json",
    );

    let output = run_update(
        &project,
        "packages/app/ratchet.config.jsonc",
        "the nightly run pays it down",
    );

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        committed(&project),
        vec!["packages/app/ratchet.baseline.json"]
    );
}

#[test]
fn the_update_mode_commits_the_file_the_binary_moved_and_not_the_one_it_found() {
    // The reproduction of the review: a stale `ratchet.baseline.json` next to
    // the real one.  The step must commit the file the binary moved — and
    // must not report a green run that committed nothing.
    let config = r#"[
      {
        "name": "test",
        "baseline": "custom/baseline.json",
        "rules": {
          "counted": {
            "class": "a class",
            "incident": "an instance",
            "unit": "things",
            "direction": "ceiling",
            "count": { "command": ["echo", "3"] }
          }
        }
      }
    ]"#;
    let project = project("ratchet.config.jsonc", config, "custom/baseline.json");
    // The stale file is committed: the tree is clean, and the file is not the
    // one the binary moves.
    write(
        &project.path().join("ratchet.baseline.json"),
        "{\n  \"version\": 1,\n  \"rules\": {}\n}\n",
    );
    git(project.path(), &["add", "-A"]);
    git(
        project.path(),
        &["commit", "-q", "-m", "the stale baseline"],
    );

    let output = run_update(
        &project,
        "ratchet.config.jsonc",
        "the nightly run pays it down",
    );

    assert!(output.status.success(), "{}", stderr(&output));
    // The commit holds the file the binary moved.
    assert_eq!(committed(&project), vec!["custom/baseline.json"]);
    // The stale file is untouched.
    let stale = fs::read_to_string(project.path().join("ratchet.baseline.json"))
        .expect("the stale baseline is readable");
    assert!(stale.contains("\"rules\": {}"), "{stale}");
}

#[test]
fn the_update_mode_refuses_a_dirty_working_tree() {
    let project = project(
        "ratchet.config.jsonc",
        CEILING_CONFIG,
        "ratchet.baseline.json",
    );
    // An unrelated change: the commit would carry it.
    write(&project.path().join("notes.txt"), "a note\n");

    let output = run_update(
        &project,
        "ratchet.config.jsonc",
        "the nightly run pays it down",
    );

    assert!(!output.status.success(), "the update must fail");
    let text = stderr(&output);
    assert!(text.contains("working tree is not clean"), "{text}");
    // Nothing was committed.
    let subject = git_text(project.path(), &["log", "-1", "--pretty=%s"]);
    assert_eq!(subject.trim(), "init");
}

#[test]
fn the_update_mode_refuses_a_detached_head() {
    let project = project(
        "ratchet.config.jsonc",
        CEILING_CONFIG,
        "ratchet.baseline.json",
    );
    git(project.path(), &["checkout", "--detach", "HEAD"]);

    let output = run_update(
        &project,
        "ratchet.config.jsonc",
        "the nightly run pays it down",
    );

    assert!(!output.status.success(), "the update must fail");
    let text = stderr(&output);
    assert!(text.contains("needs a branch"), "{text}");
    // Nothing was committed.
    let subject = git_text(project.path(), &["log", "-1", "--pretty=%s"]);
    assert_eq!(subject.trim(), "init");
}

#[test]
fn a_partially_moved_baseline_fails_the_update_mode_and_says_why() {
    let project = project(
        "ratchet.config.jsonc",
        MIXED_CONFIG,
        "ratchet.baseline.json",
    );
    // The floor fell below its baseline: the move of the ceiling is written,
    // the floor is refused — and the command fails.
    write(
        &project.path().join("ratchet.baseline.json"),
        MIXED_BASELINE,
    );
    git(project.path(), &["add", "-A"]);
    git(
        project.path(),
        &["commit", "-q", "-m", "the mixed baseline"],
    );

    let output = run_update(
        &project,
        "ratchet.config.jsonc",
        "the nightly run pays it down",
    );

    assert!(!output.status.success(), "the update must fail");
    let text = stderr(&output);
    // The output of the binary names the rule that refused the move.
    assert!(text.contains("refusing"), "{text}");
    assert!(text.contains("tests"), "{text}");
    // The step says what it did to the working tree.
    assert!(text.contains("working tree was left"), "{text}");
    // Nothing was committed.
    let subject = git_text(project.path(), &["log", "-1", "--pretty=%s"]);
    assert_eq!(subject.trim(), "the mixed baseline");
}

#[test]
fn the_update_mode_needs_a_reason() {
    let project = project(
        "ratchet.config.jsonc",
        CEILING_CONFIG,
        "ratchet.baseline.json",
    );

    let output = bash_command()
        .arg(bash_path(&path_of(UPDATE)))
        .current_dir(project.path())
        .env("PATH", path_with_the_binary())
        .env("RATCHET_CONFIG", "ratchet.config.jsonc")
        .output()
        .expect("the update script runs");

    assert!(!output.status.success(), "the update must fail");
    let text = stderr(&output);
    assert!(text.contains("needs a reason"), "{text}");
    // Nothing was committed.
    let subject = git_text(project.path(), &["log", "-1", "--pretty=%s"]);
    assert_eq!(subject.trim(), "init");
}

#[test]
fn a_baseline_path_with_a_space_is_committed() {
    // The path of the baseline may contain a space: the state of the working
    // tree is read in the machine-readable form, because the ordinary form
    // quotes such a path.
    let config = r#"[
      {
        "name": "test",
        "baseline": "custom dir/baseline.json",
        "rules": {
          "counted": {
            "class": "a class",
            "incident": "an instance",
            "unit": "things",
            "direction": "ceiling",
            "count": { "command": ["echo", "3"] }
          }
        }
      }
    ]"#;
    let project = project("ratchet.config.jsonc", config, "custom dir/baseline.json");

    let output = run_update(
        &project,
        "ratchet.config.jsonc",
        "the nightly run pays it down",
    );

    assert!(output.status.success(), "{}", stderr(&output));
    // The commit holds the file with the space in its name — and nothing
    // else.
    assert_eq!(committed(&project), vec!["custom dir/baseline.json"]);
    let content = committed_content(&project, "custom dir/baseline.json");
    assert!(content.contains("\"value\": 3"), "{content}");
}

#[test]
fn a_reason_that_starts_with_a_hyphen_is_a_reason() {
    // A reason that starts with a hyphen is a value and not another option:
    // the value is attached to its option.
    let project = project(
        "ratchet.config.jsonc",
        CEILING_CONFIG,
        "ratchet.baseline.json",
    );

    let output = run_update(
        &project,
        "ratchet.config.jsonc",
        "-5 is a value, not an option",
    );

    assert!(output.status.success(), "{}", stderr(&output));
    // The reason travelled into the record.
    let content = committed_content(&project, "ratchet.baseline.json");
    assert!(
        content.contains("-5 is a value, not an option"),
        "{content}"
    );
}

#[test]
fn the_update_mode_does_not_push() {
    let script = read(UPDATE);
    assert!(
        !script.contains("git push"),
        "the action commits the baseline; the push is the decision of the workflow"
    );
}

#[test]
fn the_tier_belongs_to_the_validate_mode() {
    // `ratchet baseline update` has no tier selection: the update mode must
    // not pass one.
    let script = read(UPDATE);
    assert!(
        !script.contains("--tier"),
        "the update mode has no tier: `ratchet baseline update` does not select one"
    );
    let action = read(ACTION);
    assert!(
        action.contains("The update mode ignores it"),
        "the action says which mode the tier belongs to"
    );
}

/// The tag patterns of the release workflow.
///
/// The trigger is a **filter pattern** of GitHub, not a glob of the shell:
/// the workflow is read, not interpreted, and a pattern that does not match
/// the tag of the release disables the release **silently** — no red run,
/// nothing to notice.  The test therefore reads the patterns out of the
/// workflow and checks what they match.
fn tag_patterns(workflow: &str) -> Vec<String> {
    let line = workflow
        .lines()
        .find(|line| line.trim_start().starts_with("tags:"))
        .expect("the release workflow triggers on tags");
    let rest = line
        .trim_start()
        .strip_prefix("tags:")
        .expect("the line holds the tags");
    // The tags are a flow sequence: the brackets belong to the YAML, not to
    // the patterns.
    let rest = rest.trim().strip_prefix('[').unwrap_or(rest.trim());
    let rest = rest.strip_suffix(']').unwrap_or(rest);
    let patterns: Vec<String> = rest
        .split(',')
        .map(|value| value.trim().trim_matches(|c| c == '"' || c == '\''))
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
        .collect();
    assert!(!patterns.is_empty(), "the workflow triggers on tags");
    patterns
}

/// One element of a filter pattern.
struct Element {
    /// The characters the element matches — `None` means "any character".
    characters: Option<Vec<char>>,
    /// The least number of times the element appears.
    minimum: usize,
    /// The most times the element appears — `None` means "any number".
    maximum: Option<usize>,
}

impl Element {
    /// True when the element matches the character.
    fn matches(&self, character: char) -> bool {
        match &self.characters {
            None => true,
            Some(characters) => characters.contains(&character),
        }
    }
}

/// Parses a filter pattern into its elements.
///
/// The subset of the GitHub syntax that the trigger uses: a literal
/// character, a character class (`[0-9]`, `[a-z]`), `*` (any run of
/// characters), `?` (one character) and `+` (one or more of the preceding
/// element).
fn parse_pattern(pattern: &str) -> Vec<Element> {
    let characters: Vec<char> = pattern.chars().collect();
    let mut elements: Vec<Element> = Vec::new();
    let mut index = 0;
    while index < characters.len() {
        match characters[index] {
            '[' => {
                let mut class: Vec<char> = Vec::new();
                index += 1;
                while index < characters.len() && characters[index] != ']' {
                    // A range: `a-z`.
                    if index + 2 < characters.len() && characters[index + 1] == '-' {
                        for value in characters[index]..=characters[index + 2] {
                            class.push(value);
                        }
                        index += 3;
                        continue;
                    }
                    class.push(characters[index]);
                    index += 1;
                }
                index += 1;
                elements.push(Element {
                    characters: Some(class),
                    minimum: 1,
                    maximum: Some(1),
                });
            }
            '*' => {
                elements.push(Element {
                    characters: None,
                    minimum: 0,
                    maximum: None,
                });
                index += 1;
            }
            '?' => {
                elements.push(Element {
                    characters: None,
                    minimum: 1,
                    maximum: None,
                });
                index += 1;
            }
            '+' => {
                // One or more of the preceding element.
                let previous = elements.last_mut().expect("a `+` follows an element");
                previous.maximum = None;
                previous.minimum = previous.minimum.max(1);
                index += 1;
            }
            literal => {
                elements.push(Element {
                    characters: Some(vec![literal]),
                    minimum: 1,
                    maximum: Some(1),
                });
                index += 1;
            }
        }
    }
    elements
}

/// Matches a name against the elements of a pattern.
fn match_elements(elements: &[Element], name: &[char], index: usize, position: usize) -> bool {
    if index == elements.len() {
        return position == name.len();
    }
    let element = &elements[index];
    let maximum = element.maximum.unwrap_or(name.len() - position);
    let mut count = 0;
    loop {
        if count >= element.minimum {
            let next = position + count;
            if next <= name.len() && match_elements(elements, name, index + 1, next) {
                return true;
            }
        }
        if count >= maximum {
            break;
        }
        if position + count < name.len() && element.matches(name[position + count]) {
            count += 1;
            continue;
        }
        break;
    }
    false
}

/// True when the name matches the filter pattern.
fn matches_filter(pattern: &str, name: &str) -> bool {
    let elements = parse_pattern(pattern);
    let characters: Vec<char> = name.chars().collect();
    match_elements(&elements, &characters, 0, 0)
}

#[test]
fn the_release_runs_for_a_version_tag_and_not_for_the_action_tag() {
    let patterns = tag_patterns(&read(WORKFLOW));
    // The version tags of the releases.
    for tag in ["v0.1.0", "v1.2.3", "v10.20.30"] {
        assert!(
            patterns.iter().any(|pattern| matches_filter(pattern, tag)),
            "the release does not run for {tag}"
        );
    }
    // A pre-release carries a suffix — the version of the manifest may carry
    // one as well.
    assert!(
        patterns
            .iter()
            .any(|pattern| matches_filter(pattern, "v0.2.0-rc.1")),
        "the release does not run for a pre-release"
    );
    // The **action** carries a moving major tag beside the version tags: it
    // is not a release, and a release run for it would fail the version check
    // against the manifest.
    for tag in ["v1", "v1.2", "release-1.2.3", "main"] {
        assert!(
            !patterns.iter().any(|pattern| matches_filter(pattern, tag)),
            "the release runs for {tag} — that is not a version tag"
        );
    }
}
