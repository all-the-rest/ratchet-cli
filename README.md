# ratchet

`ratchet` is a small, standalone binary that keeps a project's quality debt
from growing silently. It compares a **count** — TODO markers, unescaped HTML
sinks, files over a size limit — against a **baseline** committed next to the
code. A **ceiling** rule fails when the count rises above its baseline, a
**floor** rule fails when it falls below it, and the baseline itself may only
move in the direction the rule declares: a ceiling may sink, a floor may rise,
and every move is a reviewed change. The second kind of gate is the **drift
gate**: a committed, generated file must still equal what its generator
produces. Use ratchet for the gate "no new occurrences, and the existing ones
get paid down" across many rules, without a bespoke script per rule: the
project owns the *counting* (one command that prints a number), ratchet owns
the *mechanic* — baselines, direction, monotonicity, reporting, exit codes.

## Install

### Release tarball

Every `v<version>` tag publishes a tarball per platform
(`linux-x86_64`, `linux-aarch64`, `macos-x86_64`, `macos-aarch64`,
`windows-x86_64` — `.zip` on Windows) plus a `checksums.txt`. Download yours,
verify it, put the binary on your `PATH`:

```sh
VERSION=0.6.4
BASE="https://github.com/all-the-rest/ratchet-cli/releases/download/v$VERSION"
ASSET="ratchet-$VERSION-macos-aarch64.tar.gz"
curl -LO "$BASE/$ASSET"
curl -LO "$BASE/checksums.txt"
# Linux: sha256sum --ignore-missing --check checksums.txt
shasum -a 256 -c <(grep macos-aarch64 checksums.txt)
tar -xzf "$ASSET"
sudo install -m 0755 ratchet /usr/local/bin/
```

The checksum is fetched from the same origin as the asset, so that origin is
the trust anchor: it protects against corruption, not a hostile origin.

### From source

Rust 1.85 or newer (the crate uses edition 2024):

```sh
git clone https://github.com/all-the-rest/ratchet-cli
cd ratchet-cli
cargo install --path .        # or: cargo build --release
```

### Updating

`ratchet update` installs the newest release of this repository: it resolves
the latest release, downloads the asset for the platform, verifies its SHA-256
against `checksums.txt` and only then replaces the binary. A checksum that
does not match refuses the update and leaves the binary untouched.

`ratchet check` looks once a day for a newer version and prints one line when
there is one. The check is silent on failure, and is disabled with
`--no-update-check` or `RATCHET_NO_UPDATE_CHECK=1` — which is what CI sets.

## 60-second quickstart

```sh
ratchet init                 # write a starter ratchet.config.jsonc
$EDITOR ratchet.config.jsonc # replace the starter rule with yours (below)
ratchet baseline init        # record the current counts as the baseline
ratchet check                # run the gates
ratchet report --format md   # render the run as Markdown
```

`ratchet init` never overwrites an existing config; `ratchet baseline init`
writes `ratchet.baseline.json` for every rule without one and leaves existing
records alone. Commit both files — the baseline is what future runs compare.

From then on the loop is: make a change, run `ratchet check`, and when debt is
paid down, run `ratchet baseline update` and commit the moved baseline. A
failing gate is a failed gate, not one to be moved — moving a ceiling up needs
`baseline raise` and a reason.

## A minimal config

The config is the only source of truth for the rules: `ratchet.config.jsonc`
(JSON with comments and trailing commas; `.json`, `.mjs` and `.cjs` are
accepted too), looked up from the working directory up to the git root.

```jsonc
// ratchet.config.jsonc — one rule, one defect class.
[
  {
    "name": "my-project",
    "rules": {
      "todo-markers": {
        // What the rule counts, and the incident that motivated it — a rule
        // for a class that has not occurred yet says so plainly.
        "class": "unfinished work",
        "incident": "prevention: no instance in this project yet",
        // What one count unit is — it is printed in the report.
        "unit": "markers",
        // "ceiling": the count may not rise above the baseline.
        "direction": "ceiling",
        // When it runs: "fast" (seconds, no infrastructure) or "slow".
        "tier": "fast",
        // A command that prints one integer.  No shell is involved, so
        // `sh -c` is written out explicitly.
        "count": {
          "command": [
            "sh",
            "-c",
            "grep -rI --include='*.ts' --include='*.tsx' -o 'TODO' . | wc -l | tr -d '[:space:]'"
          ]
        }
      }
    }
  }
]
```

The counter's stdout is parsed as an integer, as JSON, or as an item list; a
counter that fails, prints garbage or prints a negative number is a failure,
never a green. A rule can also count from a JSON file, a findings registry or
GitHub issues, or measure files with `file_metrics`. An unknown field anywhere
is a configuration error, so a typo fails loudly instead of silently disabling
a gate; a config object may name a shipped preset — `{ "preset": "agent-rules" }`
— and override rules after it.

## Commands

| Command | What it does |
|---|---|
| `ratchet check [paths…]` | Run every gate and compare each count with its baseline. The default command — bare `ratchet` is the same. A path restricts the run to the rules whose `files` scope touches it. |
| `ratchet count` | Print the current count per rule, without a comparison. |
| `ratchet baseline init` | Write the current count as the baseline of every rule that has none. Existing baselines are never touched. |
| `ratchet baseline update` | Move the baseline towards the current count, in the allowed direction only (a ceiling down, a floor up). Also prunes stale records. |
| `ratchet baseline raise --rule <id> --value <n> --reason "<text>"` | Move a baseline against its direction on purpose. The reason is required and travels into the committed diff. |
| `ratchet baseline path` | Print the repository-relative path of the baseline file. |
| `ratchet drift` | Run the drift gates — the generated files — and nothing else. `--path` restricts the run to one file. |
| `ratchet verify` | Run the pre-release checks against their committed snapshots. `--name` runs one check, `--update` writes the produced output into the snapshot. |
| `ratchet report` | Render the run into a Markdown report; `--output <path>` writes it into the repository. |
| `ratchet release` | Compute the next version and the changelog block from the commits since the last release tag. It writes no tag. `--dry-run` changes nothing. |
| `ratchet update` | Update the binary to the newest release, checksum verified. |
| `ratchet init` | Write a starter `ratchet.config.jsonc` if none exists. |

The most useful options:

| Option | Meaning |
|---|---|
| `--config <path>` | Use this config file instead of the lookup. |
| `--rule <id>` | Restrict the run to one rule. |
| `--tier <fast\|slow>` | Run only the rules (and generators) of that tier — a `check` option. |
| `--format <stylish\|json\|md>` | The output format; `stylish` is the default, `md` belongs to `report`. |
| `--fix` | Regenerate the generated files (`check`, `drift`). The gate stays failed until the fix is committed — it measures the committed file. |
| `--strict` | Fail closed on stale artifacts: a baseline record for a rule the config no longer has, or a scope that selects no files. |
| `--ignore <pattern>` | Exclude matching paths from the rules' scope (repeatable). Drift gates are not excluded. |
| `--quiet` | Only print failures. |
| `--debug` | Print the resolved config and every counter invocation on stderr. |
| `--no-update-check` | Skip the once-a-day update check. |

## Example run

Three rules: a missing baseline fails loudly, a ceiling that went over fails
with `>`, a ceiling still under its baseline passes with `≤`. Count, baseline
and unit are printed, so the report explains itself:

```
ratchet — 3 rules, 2 failed

  FAIL  missing-baseline              no baseline for module-test-coverage
  FAIL  unexpected-inner-html         7 > 5  (files)
  PASS  unwrapped-action-calls        9 ≤ 11  (call sites)

NOT COHERENT — 2 of 3 gates failed
```

The exit code is `1`, and the fix is a real one: remove two HTML sinks (or
raise the baseline, with a reason), then initialize the missing baseline.
`--format json` prints the same run as a document; `ratchet report --format md`
renders it as a table with the verdict, the rules, the findings and the notes.

## In CI

The repository ships a composite GitHub Action; it installs the binary, so the
workflow downloads nothing itself:

```yaml
# .github/workflows/ratchet.yml
name: ratchet
on:
  push:
    branches: [main]
  pull_request:
permissions:
  contents: read
jobs:
  gates:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v5
      - uses: all-the-rest/ratchet-cli@v1
        with:
          mode: validate
          version: v0.6.4   # the release the action installs
          tier: fast        # optional: fast, slow, or empty for every tier
```

`mode: validate` runs `ratchet check`; a failing gate fails the job.
`mode: update` runs `ratchet baseline update` and commits the changed baseline
(never pushes) — use it only in a deliberate, scheduled or manually triggered
run, with `baseline-reason` set. `version` pins the binary, the action's tag
pins the action (`v1`, or a concrete tag/commit for a reproducible pin).

What the same gate is locally, before you push:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build && ./target/debug/ratchet check
```

## Exit codes

| Code | Meaning |
|---|---|
| `0` | green — every gate passed |
| `1` | at least one gate failed |
| `2` | operational error: invalid or missing config, a crashed counter, an unreadable file, an unknown command or path, a baseline that does not parse, a failed self-update |

`ratchet report` is the exception: it exits `0` whenever it produced a report,
because the verdict lives in the content and the report is itself a generated
file that a drift gate may check. `ratchet release` exits `1` when there is
nothing to release.

## Learn more

This README is the entry point. [`SPEC.md`](SPEC.md) is the **normative
specification** of the config, the path rule, the drift and verify gates, the
output formats, the action and the release step — deep design details included.

## Contributing

Open work lives in GitHub Issues: every actionable item is an issue with a
Conventional-commit-style title and a severity label. Branch from `main` as
`<type>/<slug>`, run the four green commands above, and open a PR naming the
issue it closes (`Closes #N`). Merge through the merge queue, which squashes
the PR into one commit on `main`; the required checks are the ubuntu, macOS and
Windows matrix jobs.

## License

Apache-2.0 — see [`LICENSE`](LICENSE). Status: pre-1.0 (`0.6.x`).
