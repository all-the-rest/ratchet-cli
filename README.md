# ratchet

`ratchet` is a standalone binary that enforces **ratchet gates**: quality
gates that compare a **count** against a **baseline** that may only move in one
declared direction — or, deliberately and with a reason, against it.

## The idea in five lines

1. Every rule counts one defect class — layer violations, missing tests, raw
   HTML sinks.
2. The count is compared against a **baseline**, a number in the repository.
3. A **ceiling** rule fails when the count rises above its baseline; a
   **floor** rule fails when the count falls below it.
4. The baseline itself moves in the declared direction: a ceiling may only
   decrease, a floor may only increase. Raising a ceiling to make a gate green
   is a **failed gate**, not a fix — the one exception is a deliberate raise
   that carries its reason into the diff.
5. The baseline moves only when a human says so, never automatically: in the
   declared direction by `ratchet baseline update`, against it only by
   `ratchet baseline raise`.

The engine owns the baseline, the direction, the monotonicity and the
reporting. The project owns the counting: a rule produces its number through a
command, a JSON file, a findings registry or the issues of a repository.

## Install

The releases are on GitHub: every tag `v<version>` publishes the binary of
five platforms and a `checksums.txt`. Installing it looks like this:

```sh
tar -xzf ratchet-<version>-<os>-<arch>.tar.gz
sha256sum --check checksums.txt
mv ratchet /usr/local/bin/
```

Or from source:

```sh
cargo build --release
install -m 0755 target/release/ratchet /usr/local/bin/
ratchet --version
```

The intended way to update an installed binary is `ratchet update`: it
downloads the newest release asset for the platform, verifies the SHA-256
against `checksums.txt` and replaces the binary. See
[the self-update](#the-self-update) below.

The **linux binaries are built on Ubuntu 22.04** (glibc 2.35), not on the
newest runner image: a binary built on 24.04 links glibc 2.39 and refuses to
start on Debian 12 or Ubuntu 22.04 with `GLIBC_2.39 not found`. The oldest
supported image sets the floor for every consumer.

## Usage

```sh
ratchet                 # the same as `ratchet check`
ratchet check           # run every gate
ratchet check src/      # run the gates whose scope touches src/
ratchet check --tier fast # run the gates of one tier
ratchet count           # print the current count per rule
ratchet baseline init   # write the current count as the baseline
ratchet baseline update # pay a ceiling down / a floor up — in that direction only
ratchet baseline path   # print the path of the baseline file
ratchet baseline raise --rule <id> --value <n> --reason "<text>" # move a baseline against its direction — with a reason
ratchet baseline raise --rule <id> --scope <scope-id> --value <n> --reason "<text>" # raise only one path scope
ratchet drift           # run the drift gates: the generated files
ratchet verify          # run the pre-release checks: the commands against their snapshots
ratchet report --format md # render the run into a Markdown report
ratchet release         # compute the next version and the changelog
ratchet update          # update the binary to the newest release
ratchet init            # write a starter config
```

## Contributing

Open work lives in GitHub Issues. Branch from `main`, run the green commands
locally, then open a PR that closes the issue:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build && ./target/debug/ratchet check
```

The checks run on pull requests and on `merge_group`; merge through the queue,
which squashes the PR into one commit on `main`.

## Configuration

The config is the only source of truth for the rules. It is a *flat* config in
the ESLint shape: an array of config objects that are merged. A later object
overrides an earlier one for the same rule id. A single config object at the
root is accepted as shorthand for a one-element array.

The baseline file lives next to the config and must lie **inside the
repository root**: a baseline that cannot be compared with its committed
version is a gate that does not gate. A config that points outside the root is
an operational error.

Every path a project names — the baseline, the artifact of a counter, the
registry, a generated file, the snapshot of a check, the output of the report,
the config of `init`, the files of the release — follows **one rule**: it is
**relative**, and it lies **inside the repository root**, **resolved**.

- **relative** — a committed config has to work on every machine: the same
  repository is checked out in different places, and an absolute path names
  one of them. A path of a project is therefore relative to the **repository
  root** — to the **directory of the config** for the baseline, which lives
  next to the config.
- **inside** — a path outside the root names a file the project does not own.
  The engine would read it, write it, or compare it with the committed
  version — and none of that is the engine's business.
- **resolved** — the path is resolved **component by component**, the way the
  operating system resolves it, and the **whole** path is compared with the
  resolved root: `.` is skipped, `..` pops the resolved prefix, and a name
  that is a symbolic link is **followed** — its target is resolved the same
  way, and the walk continues from there.

  Neither a link in a directory of the path nor a link at the file position
  can therefore carry the path out of the root. The **chain** of links is
  followed to its **final** target: the target of a link may be a link itself,
  and a link that points at a link that points out of the root carries the
  path out of it.

**A `..` directly after a link is refused.** The operating system reads
`link/..` as the parent of the link's **target**, a reader of the config as
the parent of the named directory: the two readings differ, and the tool does
not pick one. A `..` that does not follow a link keeps working as it always
did (`docs/../x` stays `x`; `link/sub/..` is the parent of the link's target,
and there both readings agree).

**A path that cannot be resolved is refused.** The links of the path could
not be followed to the end: a cycle (`a → b → a`) or a chain that is longer
than the bound. The tool follows at most **32** links and refuses a path with
more — the system's own limit is smaller on some systems (15 on Darwin) and
larger on others (40 on Linux): where the system refuses earlier, the
operation fails with `ELOOP`. The tool refuses a path it cannot resolve, and
the system refuses a path it cannot open; both fail closed. A link the engine
cannot even **read** — one in a directory it may not search — is not seen as a
link at all: the path is treated as a plain name, and the operation that
follows fails with the permission error of the system.

The file itself may be a **link**, as long as its **target** lies inside the
root: there the comparison follows the link. A path that breaks the rule is an
**operational error** — the run ends with exit code 2. A counter whose path
breaks the rule reports its rule as a `counter-error`: the same exit code, and
the same kind of failure. The **wording** is the subject's own: the baseline,
the report, a counter, the drift gate, the pre-release check, `init` and the
release step each name the path in their own words.

The file is `ratchet.config.jsonc` (JSON with comments and trailing commas);
`ratchet.config.json`, `ratchet.config.mjs` and `ratchet.config.cjs` are
accepted as well. The lookup walks up from the working directory to the git
root.

```jsonc
[
  {
    // Optional label for this config object.
    "name": "noema",

    // Per tier settings; the minimum number of checks that must execute.
    "tiers": {
      "fast": { "floor": 5 },
      "slow": { "floor": 1 }
    },

    // One rule per defect class.
    "rules": {
      "unwrapped-action-calls": {
        // The defect class the rule counts.
        "class": "layer violation",
        // The incident that motivated the rule.  For a class that has already
        // occurred, the incident names that concrete instance; for a
        // prevention rule — a class whose count is zero today — it says so
        // plainly ("prevention: no instance in this project yet").
        "incident": "F-09: a server action was called without the shared error helper",
        // What one count unit is.
        "unit": "call sites",
        // The allowed direction of the baseline: "ceiling" or "floor".
        "direction": "ceiling",
        // Which tier the rule belongs to; "fast" (default) or "slow".
        "tier": "fast",
        // How the count is produced: a command (no shell) or a JSON file.
        "count": { "command": ["node", "scripts/audit-inventory.mjs", "--print"] }
      },

      "unexpected-inner-html": {
        "class": "layer violation",
        "incident": "an unescaped HTML sink reached the renderer",
        "unit": "files",
        "direction": "ceiling",
        "tier": "fast",
        // A JSON file, optionally with a dotted path into it.
        "count": { "file": "docs/_audit-raw.json", "path": "coherence.dangerouslySetInnerHTML" },
        // Glob patterns removed from the counted items; the items are
        // repository-relative paths.
        "exclude": ["src/lib/markdown.ts", "src/lib/html-sanitize.ts"],
        // The scope of the rule; without it the rule always runs.
        "files": ["src/**"]
      }
    },

    // The release step and the changelog.  See "The release step" below.
    "release": {
      // The changelog file; "CHANGELOG.md" by default.
      "changelog": "CHANGELOG.md",
      // The first version the tool manages; below it the changelog is
      // written by hand.
      "changelogFrom": "0.2.0",
      // The prefix of the tag of a release; "v" by default.
      "tagPrefix": "v",
      // Where the version of the project lives.
      "version": { "path": "Cargo.toml", "format": "toml", "section": "package", "key": "version" }
    }
  }
]
```

An unknown field anywhere — in a config object, in a rule or in a counter — is
a **configuration error** (exit code 2) that names the offending path, e.g.
`rules.unwrapped-action-calls.direction`. Typos fail loudly instead of
silently disabling a gate. An empty `class`, `incident` or `unit` is a
configuration error as well: an empty string is a missing value that happens
to pass the type check.

### The incident

The `incident` is the incident that **motivated** the rule. For a rule whose
class has already occurred, it names that concrete instance — it is what a
later reader judges the rule against when the count moves:

```jsonc
"incident": "F-09: a server action was called without the shared error helper"
```

For a **prevention rule** — a class whose count is zero today — the incident
says so plainly instead of naming an incident that did not happen:

```jsonc
"incident": "prevention: no instance in this project yet"
```

A gate that claims a history it does not have is worse than no gate: the next
reader cannot tell a real regression from a remembered one.

### The counter

A counter produces one of these values:

| Value | Meaning |
|---|---|
| integer | the count |
| array of strings | the items; the count is the number of items after `exclude` |
| `{"count": N}` | the count |
| `{"count": N, "items": [...]}` | the count and the items |
| `{"measurements": [{"path":"src/auth/Login.java", "value":123}]}` | one nonnegative integer per repository-relative file |

For a command counter the stdout is parsed: trimmed, then tried as an integer,
then as JSON in the shapes above. The command runs in the repository root,
without a shell — the arguments are passed through verbatim. A counter that
fails, produces garbage or a negative number is a counter error.

For path-scoped metrics, a command can emit the `measurements` JSON shape above.
Each path must be unique and resolve to a regular file inside the repository;
malformed, duplicate, absolute, traversal and out-of-root paths fail closed.
Rules use `aggregate: "count"` (distinct files) or `aggregate: "max"` before
their values are compared with independent baselines.

The built-in `file_metrics` counter measures any glob-selected files without
language-specific parsing:

```jsonc
"java-file-size": {
  "class": "file growth", "incident": "large source files are hard to change",
  "unit": "bytes", "direction": "ceiling", "description": "All Java source files",
  "aggregate": "max",
  "count": { "file_metrics": { "files": ["**/*.java"], "metric": "size_bytes" } },
  "scopes": [
    { "id": "auth", "description": "Authentication module", "files": ["src/auth/**"] },
    { "id": "legacy-login", "description": "Legacy login", "file": "src/auth/LegacyLogin.java" }
  ]
}
```

Metrics are `size_bytes` (raw bytes), `line_count` (CRLF-aware logical physical
lines; an empty file is zero and a trailing newline adds no phantom line), and
`max_line_length` (Unicode scalar values, excluding line terminators). Scopes
assign every measurement once: exact file first, then the most-specific glob,
then the global fallback. Tied specificity and empty named scopes are errors.
Every scope has an independent baseline under the rule's `scopes` map; init
and update process each executed scope. Add `--scope <id>` to `baseline raise`
to raise only that named scope; without it, the global fallback is raised.
Scope IDs and descriptions appear in human, JSON and Markdown reports.

A **file** counter reads a JSON file of the repository — optionally a dotted
`path` into it. The file follows the **one rule**: it is relative and it lies
inside the repository root — resolved. The rule is about the path the project
**names**, not about what the run reads from it: with `generate` the count
comes from a temporary path, and the configured path is still the project's. A
path that breaks the rule is an **operational error**, and the message names
the **configured** path.

`exclude` needs a counter that yields items; a scalar counter plus `exclude` is
a configuration error.

### The findings registry

A **registry** counter reads a findings registry — a JSON file in the project,
`ratchet.findings.json` — and counts the findings that count:

```jsonc
"count": {
  "registry": "ratchet.findings.json",
  "severities": ["critical", "high"],
  "statuses": ["open", "in-progress"]
}
```

The file itself:

```json
{
  "version": 1,
  "findings": [
    {
      "id": "SEC-01",
      "title": "org admins reassign foreign users",
      "severity": "critical",
      "status": "open",
      "owner": "alice",
      "ticket": "D-12"
    },
    {
      "id": "SEC-02",
      "title": "a closed one",
      "severity": "high",
      "status": "fixed",
      "status_since": "2026-10-19",
      "regression_test": "OrgAdminDelegationBoundaryTest"
    }
  ]
}
```

* `id` (stable and unique), `title`, `severity` and `status` are required.
  `status_since`, `owner`, `ticket`, `regression_test`, `path` and `note` are
  optional — with one exception: **`owner` and `ticket` are mandatory for
  every finding that is not `fixed`**. A closed finding has its commit and its
  regression test, an open one has nobody — a graveyard entry.
* The levels are `critical`, `high`, `medium`, `low` and `info`; the statuses
  are `open`, `in-progress`, `fixed`, `accepted` and `wontfix`. A level or a
  status outside these lists is an **error**, never a fallback.
* The count is the number of the findings whose level is in `severities` —
  every level counts without the list — and whose status is in `statuses` —
  without the list the counting statuses, `open` and `in-progress`, count.
  `accepted` and `wontfix` are decisions: they do not count, but they stay in
  the file. A decision is not a deletion. `fixed` is resolved.
* The registry is a counter source like every other artifact: the count comes
  from the file, the baseline from the baseline file, the direction from the
  rule. A rule with `direction: "ceiling"` and a baseline of `0` **is** the
  boolean gate ("no open finding") — the same mechanic, no second kind of
  gate.
* A registry that cannot be read, that carries a version other than `1`, that
  has a missing field, a duplicate `id` or an unknown level or status is a
  **configuration error** (exit code 2) that names the offending entry. The
  registry follows the **one rule** too: it is relative and it lies inside the
  repository root — resolved. A path that breaks the rule is an operational
  error, and the message names the **configured** path.
* The registry is a **view** of the prose findings, not a second source of
  truth: the review log stays the human record, and the two are migrated in
  the same commit.

### The issues backend

A project that already tracks its findings as GitHub issues counts them there
instead of keeping a second list beside them:

```jsonc
"open-findings": {
  "tier": "slow",
  "count": {
    "issues": {
      "repository": "owner/repo",      // required, the form is owner/repo
      "label": "ratchet",              // optional, the membership label
      "severity_prefix": "severity:",  // optional, the prefix of the level label
      "default_severity": "medium"     // optional, an issue without a level
    },
    "severities": ["critical", "high"],
    "statuses": ["open"]
  }
}
```

* **Membership is a label.** Only issues that carry the label count — `ratchet`
  by default — so the tracker can hold other tickets without feeding the gate.
  Pull requests are not findings: an entry with a `pull_request` field is
  skipped although the issues API carries it.
* **The level is a label.** `<severity_prefix><level>` with `level` in
  `critical`, `high`, `medium`, `low`, `info`; an issue without such a label
  counts as `default_severity` (`medium`). A label with the prefix and a level
  outside the vocabulary is an **error** that names the issue and the label —
  never a silent fallback.
* **The mapping follows the registry.** An open issue is `open`, a closed one
  is `fixed`; the first assignee is the `owner` (empty when the issue has
  none), and `#<number>` is the `ticket` and the `id`. A `path` stays empty.
  The findings are sorted by issue number, so the same data produces the same
  bytes.
* **The filters are the registry's.** `severities` and `statuses` narrow the
  count; without `statuses` the counting statuses count — for issues that is
  `open`, because a closed issue is `fixed`. An empty list is a configuration
  error.
* **The token** comes from `GITHUB_TOKEN`, then `GH_TOKEN`, then the installed
  GitHub CLI (`gh auth token`). If none is available or the CLI is not
  authenticated, the gate is **not green**: the rule fails as a counter error
  and the run exits with code 2. The same is true for an unreachable API, a
  status other than 200, an answer that is not JSON, a missing field or an
  unknown state. The pagination follows the `Link` header of the API and is
  bounded to 20 pages; more is an error, never a truncated count.
* **The network exception.** The engine opens no connection at runtime except
  the update check. This counter is the second exception and it is **opt-in
  per config**: a project without an `issues` counter never opens a connection.
  The endpoint is `https://api.github.com`, overridable only through
  `RATCHET_ISSUES_API_BASE` (for tests and CI) — a base in the config would
  decide where the token of the person who runs the gate is sent. The counter
  is a network call: a rule that counts issues belongs to the `slow` tier.
  Since rules otherwise default to `fast`, set `"tier": "slow"` on the rule;
  omitting it or setting `fast` is a configuration error.

### The globs

`exclude` and `files` are globs over repository-relative paths: `*` matches any
run of characters except `/`, `**` matches zero or more path segments, `?` and
`[...]` work as usual. A pattern that ends in `/**` also matches the directory
itself — `src/**` matches `src` and everything below it. A scope that selects
nothing is a failure, not a pass: the rule that would have run is not silently
skipped.

### A generated artifact

A file counter can regenerate its artifact before the count is read — a count
read from a stale artifact is a gate that lies:

```jsonc
{
  "file": "docs/_audit-raw.json",
  "path": "coherence.unwrappedCallSites",
  "generate": ["node", "scripts/audit-inventory.mjs"],
  "output_arg": "--output"
}
```

The binary appends a **temporary output path** to the argv; the generator
writes there and the count is read from the fresh artifact. `output_arg` is
mandatory: a generator that cannot write to a given path would have to write
into the tree, and the tree is not the engine's to write. `generate` without
`output_arg` is a configuration error, and so is `output_arg` without
`generate`. The working tree stays exactly as it was.

### The drift gates

A **generated file** is a file that a generator produces. The `generated`
entries of the config name them:

```jsonc
"generated": [
  { "path": "docs/MISSING_FEATURES.md", "command": ["npm", "run", "docs:gaps"], "output_arg": "--output" }
]
```

The gate compares the **committed** file (`git show HEAD:<path>`) with what
the generator produces: a stale committed file is the defect, and the gate
fails with the unified diff of the difference:

```
--- docs/MISSING_FEATURES.md
+++ docs/MISSING_FEATURES.md
@@ -1,3 +1,3 @@
 # Title
 
-stale content
+fresh content
```

The **lookup** of the committed file uses the file the system resolves the
named path to: git keeps a symbolic link as a blob whose content is the path
of its target, and the files of the target are committed beside it: the
committed tree has no entry at the **named** path, and the comparison has to
follow the link to find the file. The report and the messages keep the
**named** path: that is the path the project named and the path the user
knows.

The diff is a **patch**: `patch -p0` applies it to the committed file. The
newline at the end of a file is part of the file, so a file that ends with a
newline and one that does not differ — and the difference shows up in the
diff. A generated file that is not valid UTF-8 produces a **lossy** diff:
the bytes that are not text are shown as the replacement character. The
verdict is still right — it is computed on the raw bytes — but the human
artifact is not a faithful rendering of a binary file.

The status of a generated file is `ok` (in sync), `drift` (the committed file
differs — a failed gate) or `new` (not committed yet — informational, not a
failure). A generator that cannot be run — or that runs and produces
nothing — is an **operational error** (exit 2), not a failed gate. So is a
committed file that cannot be read: a broken reference must not disable a
gate, and a committed path that is a directory is not a file at all. A
repository whose `HEAD` points at a branch that has no commit yet has nothing
committed yet (`new`); a repository whose `HEAD` names a commit that cannot be
read is damaged, and that is an operational error as well.

A `generated[]` entry may declare a `tier` (`fast` | `slow`, default `fast`).
`--tier fast` skips the slow generators: a generator that needs a database, a
browser or the network belongs to the slow tier, and a fast tier that waits
for it is a fast tier that is not fast.

The generated file follows the **one rule**: it is relative and it lies inside
the repository root — resolved, the file included. A path that leaves the root
would let the engine write outside the tree it owns, and a link that points
out of it would take the write with it. The file itself may be a link, as long
as its **target** lies inside the root: there the file is the subject, and the
comparison follows the link.

A **dangling** link at the file position is refused with a named error. In the
in-place mode the generator would write through it and create the file behind
it, and the restore cannot take that file back — the tree would change. In the
temporary-output mode the tree is never touched, but the comparison would
follow the link to a file that is not there, and it would say nothing about
the generated file.

Two generator modes exist:

- **with `output_arg`** — the generator writes to a temporary path; the tree
  is never touched.
- **without `output_arg`** — the generator writes the file **in place**. The
  binary then owns a **verified restore**: it snapshots the original bytes,
  the file mode *and* the time the file was written last, **creates the
  directories the file needs** if they do not exist, runs the
  generator, reads the produced bytes, writes the snapshot to a temporary
  file in the same directory, gives it the original mode and time and
  **renames** it over the target — a rename replaces a symbolic link instead
  of writing through it — verifies the restore and removes a directory the
  generator created and left **empty**. If the target **is** a symbolic link,
  the restore writes the file **behind** the link and recreates the link: a
  rename over the link would replace it with a regular file and change the
  tree, and the tree must not change. A generator writes **behind** a link, so
  the file behind the link is the one that changes — a file created behind a
  **dangling** link is not removed, which is why a dangling link is refused. A
  restore that fails ends the run with
  an operational error that names the file and says that the working tree was
  modified: a swallowed restore error is a mutated tree that nobody was told
  about.

  The directory is the engine's business, not the generator's: a project
  cannot commit an empty directory, and a generator that writes a file into a
  new directory would fail otherwise — the gate would be red from its first
  run. The engine creates the directories the **operating system** needs to
  walk the named path: the **missing directories of the traversal** —
  `sub/../x.md` needs `sub`, because a `..` can only pop a directory that
  exists — and the **directory the path resolves to**, the place the file
  lands in when a link leads there. The missing directories of the traversal
  are created **as they are named**: the path the project named is the path
  the engine walks. The directory the path resolves to is created **where the
  path resolves to**. A directory that cannot be created is an operational
  error that names the directory and the cause: the failure is the engine's,
  and the user is not sent to the generator. The restore removes the
  directories the engine created again — when they are **empty**: a directory
  that still holds something is not the engine's to remove.

  Before the generator runs, the file is touched with an **old** time — an
  hour into the past. The engine can then tell whether the generator wrote
  the file — a generator that leaves it alone produced nothing, and the
  comparison of the committed file with itself would be a green that says
  nothing. The marker works on every filesystem, whatever its timestamp
  granularity is.

  The generator sees an input that is an hour older than it is. A generator
  that compares the time of its input with the time of its output therefore
  regenerates instead of skipping — and a generator that embeds the time of
  its input in its output would produce a permanent drift, which the
  determinism rule forbids anyway. The tree is restored exactly afterwards:
  the bytes, the mode **and** the time.

  What the engine can see is the **inode**, the **time** and the **bytes** of
  the file. A write that changes none of them — the same bytes, written over
  the same inode, with the time of the original restored — is reported as
  *"produced nothing"*. That is the fail-closed direction: a false red,
  never a false green. A generator that **replaces** the file (`cp -p` and
  `mv`, the reproducible-build pattern) leaves a new inode behind and is
  recognized as a write ✓.

`ratchet drift` runs the drift gates alone; `ratchet check` runs them as part
of the whole run. A drift command without a generated entry fails: an empty
selection is never a pass. `--fix` writes the produced content into the
generated files — and only into the generated files, never into hand-written
text. The gate stays **failed**: the committed file is still stale, because
the gate measures the **committed** artifact. It turns green once the fix is
committed. A run with `--fix` in a pipeline is therefore never green against a
stale HEAD; that is deliberate, not a bug.

### The pre-release check

A **benchmark is a measurement, not a defect class**. It has no meaningful
"baseline that may only sink" — the value moves in both directions and only
matters at the moment of a release. So the pre-release check is **not a
ratchet: it has no direction**. It runs in the **release pipeline**, not in the
commit gate: it needs the infrastructure the release needs, and a permanent
gate over a measurement would be noise.

The `verify` entries of the config name the checks:

```jsonc
"verify": [
  {
    "name": "benchmark",
    "command": ["cargo", "bench", "--", "--output", "<out>"],
    "snapshot": "snapshots/benchmark.txt"
  }
]
```

* The **snapshot** is a committed file, machine-written, the same discipline as
  the baseline: never hand-edited, the diff is the review artifact.
* The path of the snapshot follows the **one rule** too: it is relative and it
  lies inside the repository root — resolved.
* The command runs with the working directory set to the **repository root**
  and writes to a **temporary path** — the check does not modify the tree.
* The comparison is against the **committed** snapshot, not the working-tree
  copy. A symbolic link in a directory component is followed for the Git
  lookup, which uses the repository-relative resolved path because Git does
  not traverse a committed link blob. The final snapshot itself must still be
  a regular file. Messages and diffs keep the configured path:

  ```
  --- snapshots/benchmark.txt
  +++ snapshots/benchmark.txt
  @@ -1,1 +1,1 @@
  -benchmark: 100 ops/s
  +benchmark: 103 ops/s
  ```

* The status of a check is `ok` (the snapshot is the output), `drift` (the
  snapshot differs — a failed check) or `new` (the snapshot is not committed
  yet — informational, not a failure).

**Where the output goes.** Three ways to say it:

| the entry | the output is |
|---|---|
| `"output_arg": "--output"` | the file the command writes to `--output <temporary path>` — the flag and the path are appended to the argv |
| the placeholder `<out>` in the command | the file the command writes to the temporary path the placeholder stands for |
| neither | the **stdout** of the command |

The placeholder is replaced **wherever it appears in an argument** — not only
as a whole argument: `--output=<out>` and `<out>.txt` name the output as
well. The literal `<out>` never reaches the command: a command that writes
`--output=<out>` has to write to the temporary file, or the engine would
compare the command's stdout with the snapshot and report a green that means
nothing. The temporary directory is the contract, not the file name: the
output is the file the command wrote there. A command that writes nothing is
an operational error; a command that writes more than one file is one too —
the output would be a guess.

The placeholder and the `output_arg` are two ways to name the same output
path; an entry that uses both is a configuration error.

**The snapshot has to be a regular file.** A symbolic link is refused with a
named error: the committed content of a link is its target, and comparing the
*output* with the target's content would say nothing about the output. (The
drift gate resolves a link instead — there the *file* is the subject, here the
*output* is. The two gates have different subjects and say so.)

A link in a directory component is different: the comparison follows it to the
resolved committed path, while messages and diffs continue to name the
configured snapshot path.

`ratchet verify` runs the checks; `--name <name>` runs one of them. The exit
code is `0` when every check is `ok` or `new`, `1` when a snapshot differs
**or** when there is no check at all (an empty selection is never a pass), and
`2` on an operational error:

* the command could not be started, failed, or wrote nothing;
* the command wrote more than one file into the temporary directory — the
  output would be a guess;
* the committed snapshot could not be read;
* the snapshot path is outside the repository root — also when it leaves the
  root through a symbolic link;
* the snapshot is not a regular file — a link or a directory;
* the restore of the snapshot failed — the working tree was modified;
* the `--name` names a check that does not exist.

The message names the check — and, where a file is involved, the path as
well: a broken command is not a failed measurement, and whoever reads the
error has to know which check failed and which file it is about.

`--update` writes the produced output into the snapshot — the snapshot is
machine-written, never hand-edited. The check stays **failed**: the committed
snapshot is still stale until the file is committed, the same rule the drift
gate follows. A snapshot outside the repository root is refused — and the
refusal holds for a path that leaves the root through a symbolic link, so
`--update` cannot write outside the tree.

If its parent directory is missing, `--update` creates it at the snapshot's
resolved path, so an in-root link to a missing directory works. The command's
restore tracks missing directories at that resolved location and removes those
it finds empty. Parent creation is one directory at a time and rolls back
directories created by a failed attempt; if rollback leaves a directory
behind, the error names it and says that the working tree was modified.

The engine does not modify the tree: the command writes into a temporary
directory, and the snapshot — the one file the engine owns — is restored
afterwards, byte-, mode- **and** time-identical. The engine touches the file
with an old time before the command runs, so it can tell a command that
rewrites the snapshot with the same bytes from a command that leaves it
alone; either way the file gets its time back. A command with other side
effects is the command's doing: the engine cannot undo them.

The command sees a snapshot that is **an hour older** than it is. A command
that compares the time of its input with the time of its output therefore
rewrites the snapshot instead of skipping it — and a command that embeds the
time of its input in its output would produce a permanent drift, which the
determinism rule forbids anyway. The snapshot is restored exactly afterwards:
the bytes, the mode **and** the time.

The JSON document reuses the **check document's envelope** with the
`generated` array replaced by a `verify` array: the same `schema`, `ok`,
`summary` and `notes`, an empty `rules` array — the verify command runs no
rule — and one entry per check with its `name`, `status`, `diff` and
`detail`. In the summary, `generated` counts the checks and `drifted` the
checks whose snapshot differs.

### The JavaScript config

`ratchet.config.mjs` / `ratchet.config.cjs` exports the same JSON shape as its
default export and is evaluated with `node`. The config is **data only** — a
rule counts through `command` or `file`, never through a function.

```js
export default [
  {
    name: "noema",
    rules: {
      "unwrapped-action-calls": {
        class: "layer violation",
        incident: "F-09: a server action was called without the shared error helper",
        unit: "call sites",
        direction: "ceiling",
        count: { command: ["node", "scripts/audit-inventory.mjs", "--print"] },
      },
    },
  },
];
```

Known limitation: a repository whose path contains a **backslash** cannot be
loaded through the JS bridge — node rejects the percent-encoded backslash in a
file URL. The failure is closed and the error names the cause; use
`ratchet.config.jsonc` in such a checkout.

## The baseline file

`ratchet.baseline.json` is written by the tool and never hand-edited. The
rules are sorted by id, so the file is a stable diff.

When a baseline's parent directory is missing, the writer creates it at the
path the baseline resolves to — including behind an in-root directory link.
Directories are created one at a time; if a later creation fails, only the
directories created by that attempt are rolled back. An unsuccessful rollback
names the directory left behind and says that the working tree was modified.

```json
{
  "version": 1,
  "rules": {
    "unwrapped-action-calls": { "value": 11, "reason": "F-09 remainder, tracked in P1.2" }
  }
}
```

The `reason` is optional — but it is more than an explanation: it is what
makes a **deliberate move against the declared direction** reviewable. The
reason travels with the record, so the diff on the baseline file carries it.

## The commands

| Command | What it does |
|---|---|
| `ratchet [check] [paths…]` | Runs every gate and compares the count with the baseline. The default command. `paths…` restricts the run to the rules whose `files` scope touches them; a path that does not exist is an operational error. |
| `ratchet count` | Prints the current count per rule, without a comparison. |
| `ratchet baseline init` | Writes the current count as the baseline of every rule that has none. Existing baselines are never touched. Without `--strict` it **documents** stale records and leaves them be; with `--strict` it refuses **before writing** if any stale artifact exists. |
| `ratchet baseline update` | Moves the baseline towards the current count — **only** in the allowed direction. A ceiling may only decrease, a floor may only increase. A rule that may not move is reported as refused and left alone. It is also the cleanup step: it **prunes** stale rule and scope records. |
| `ratchet baseline path` | Prints the **repository-relative path** of the baseline file — the path the binary itself uses, and nothing else. It is the answer to "which file is my baseline?" for a script outside the binary: the resolution lives in one place, and a second implementation answers a different question than the binary. |
| `ratchet baseline raise` | Moves a baseline **against** its declared direction, on purpose, with a reason. A ceiling goes up, a floor goes down. The reason is required, and it travels with the record — see below. |
| `ratchet drift` | Runs the drift gates — the generated files — and nothing else. `--path` restricts the run to one generated file. |
| `ratchet verify` | Runs the pre-release checks — the commands of the `verify` entries against their committed snapshots — and nothing else. `--name` restricts the run to one check, `--update` writes the produced output into the snapshots. See [the pre-release check](#the-pre-release-check). |
| `ratchet report` | Renders the run into a **Markdown report**: the summary with the verdict, one row per rule, the findings that count and the notes of the run. The drift gates are not in it — a report that held the result of the comparison with itself would be a regress. `--output <path>` writes it into the repository instead of stdout. See [the Markdown report](#the-markdown-report). |
| `ratchet release` | Computes the next version from the commits since the last release tag and renders the changelog block of that version. It writes the version into the manifest of the project and appends the block to the changelog — and it writes **no tag**: a tag is a promise, and a promise is not made by a tool. `--from <tag>` computes the release from another tag, `--dry-run` prints the release and changes nothing. See [the release step](#the-release-step). |
| `ratchet update` | Updates the binary to the newest release of this repository: the asset for the platform is downloaded, its SHA-256 is verified against the `checksums.txt` of the release, and the binary is replaced. See [the self-update](#the-self-update). |
| `ratchet init` | Writes a starter `ratchet.config.jsonc` if no config exists. An existing config — also one in a parent directory of the repository — is never overwritten. The config is a path the project names: the **one rule** applies to it, and a link at that position that leads out of the tree is refused. |

### Options

| Option | Meaning |
|---|---|
| `--config <path>` | Use this config file. |
| `--no-config` | Do not look for a config; only valid together with `--config`. |
| `--format <stylish\|json\|md>` | The output format, `stylish` by default. `md` belongs to the `report` command: the other commands print the stylish output or the JSON document. |
| `--rule <id>` | Restrict the run to one rule. |
| `--strict` | Fail closed on **stale artifacts** — a baseline record for a rule or scope the config no longer has, and a configured scope that selects no files. Meaningful for `check`, `report` and `baseline init`; accepted as a no-op everywhere else. |
| `--fix` | Apply the fixable: write the produced content into the generated files. The gate stays failed until the fix is committed. A `check` and a `drift` option — the `report` writes a report and changes nothing else. |
| `--name <name>` | Run only the check of this name. A `verify` option. |
| `--update` | Write the produced output into the snapshots. A `verify` option: the snapshot is machine-written, and the check stays failed until the snapshot is committed. |
| `--from <tag>` | Compute the release from this tag instead of the last release tag. A `release` option. |
| `--dry-run` | Print the release, change nothing. A `release` option. |
| `--tier <fast\|slow>` | Run only the rules of that tier — and only the generators of that tier. A `check` option: `ratchet drift` runs every tier. |
| `--ignore <pattern>` | Exclude the paths that match the pattern from the **rules' path scope** (repeatable). It does not exclude a drift gate. |
| `--debug` | Print the resolved config and every counter invocation (on stderr). |
| `--quiet` | Only print failures. |
| `--no-update-check` | Do not check for a new version. The same is true for `RATCHET_NO_UPDATE_CHECK=1`. |
| `--version`, `--help` | |

`baseline init` and `baseline update` accept `--reason <text>`, which is stored
in the baseline record. For measurement rules they process each selected scope
independently. `baseline raise` needs `--rule <id>`, `--value <n>` and
`--reason <text>` — the reason is not optional there. Add `--scope <id>` to
raise only a named scope; without it the global fallback is addressed.

### The deliberate raise

A ratchet forbids the forbidden direction: a ceiling may only decrease, a
floor may only increase. That is the right rule for the debt a project pays
down — but a project sometimes has to move a baseline **against** its
direction on purpose: a dependency was added with a reason, the debt was
written off, a floor was lowered because the module it guarded is gone.

```sh
ratchet baseline raise --rule dependency-count --value 8 --reason "the self-update needs an HTTPS client and a SHA-256 implementation"
```

`baseline raise` is that command. It writes the new value **with the reason**
and touches nothing else.

* The move must be **against** the declared direction: a ceiling goes **up**, a
  floor goes **down**. A move in the allowed direction is the paydown's job —
  `ratchet baseline update` does it, and `baseline raise` refuses it: *"that is
  the paydown — use `ratchet baseline update`"*.
* The **reason is required**. A raise without one is refused like any other
  forbidden move, and the baseline file stays untouched.
* The reason **travels with the record**: it is written into
  `ratchet.baseline.json`, so the diff on the baseline file carries it. That is
  what makes the move reviewable — the change is visible, deliberate and
  reviewable on its own.

The monotonicity check accepts a raise that carries a reason — the reason is in
the file and therefore in the diff. A raise without a reason is rejected like
any other forbidden move (`baseline-raised` / `baseline-lowered`, exit `1`).
`baseline update` keeps its shape: it moves only in the allowed direction and
refuses the other.

### The tiers

The config names the tiers and their **floor** — the minimum number of checks
that must *execute* in that tier:

```jsonc
"tiers": { "fast": { "floor": 5 }, "slow": { "floor": 1 } }
```

`--tier fast` runs the rules of the fast tier only; without it, every rule
runs. The floor of **every tier that runs** is enforced: with `--tier` that is
the selected tier, without it every tier of the config. A tier that executes
fewer checks than its floor fails, and a tier that executes **no** check fails
as well — an empty selection is never a pass. A check that ran and failed
counts as executed (it fails anyway); a check that was never selected does
not.

The floor is what stops a tier from being quietly narrowed — a narrowing that
only shows up when somebody selects the tier by hand is a narrowing that
survives.

The floor counts the **rules** that execute in the tier: a check is a rule, and
a drift gate is not a check — it is a gate over a generated file, and the
report (which is a generated file) cannot be part of the count that decides
whether the report is coherent. A rule that ran and failed counts as executed,
and a generator that ran does not. A tier that declares a floor therefore
needs that many rules — a tier whose promised rules are all missing is exactly
the narrowing the floor exists to catch.

The drift gates are tiered like the rules: a `generated[]` entry belongs to
the `fast` tier unless it says otherwise. Their tier decides **when** they run
(`--tier fast` skips the slow generators); it does not add to the floor, and a
tier that holds nothing but a generator is not a tier that runs.

### The path exclusion

`--ignore` takes glob patterns of paths to exclude from the selection. A rule
whose scope does not intersect the remaining paths is skipped. Ignoring a
path never turns a failing gate green: a rule that would have run on the
ignored path still runs on the rest.

`--ignore` applies to the **rules' path scope**. It does not exclude a drift
gate: a generated file is registered deliberately, and a pattern that happens
to match it must not silently disable the gate.

### Stale artifacts

An artifact that was an exception once and is not one any more is a **stale
artifact**: a baseline record for a rule the config no longer has, a baseline
scope record for a scope the rule no longer configures, or a configured scope
of a per-file measurement rule that selects no file (its file was deleted, for
example).

By default a stale artifact never fails a run. It is reported as a note, and
the gate still runs on everything that is still configured:

```sh
$ ratchet check
ratchet: note: baseline record for unknown rule "legacy-login" — ignored (run ratchet baseline update to prune it)
ratchet: note: baseline scope "old-module" for rule "file-size" is not configured — ignored (run ratchet baseline update to prune it)
ratchet: note: rule file-size scope auth-legacy selects no files — ignored (remove the scope from the config)
```

A baseline record carries the `ratchet baseline update` hint; a configured
scope that selects no file carries the hint to remove the scope from the
config, because the engine never edits the config.

A configured scope that selects no file is skipped for that scope only: the
rule's other scopes and the global fallback still run and are compared. A rule
whose counter selects no file **at all** is still a counter error, and a run
whose executed checks drop to zero still fails — an empty selection is never a
pass.

`--strict` restores the fail-closed behaviour for the run where a typo must
not be silently green: every stale artifact is an operational error (exit `2`)
and is named, and an empty configured scope is today's
`scope <id> (<description>) selected no files`. `report --strict` exits `2`
when the report is NOT COHERENT because of a stale artifact.

An `exclude` pattern that matches nothing is **always a note**, in every mode:
`exclude pattern "<pattern>" for rule "<rule>" matches nothing — ignored`. It
is never an error — not even under `--strict` — and it is never pruned,
because the engine never mutates the config.

`baseline update` is the cleanup step: it prunes the stale records (it is
always tolerant, `--strict` has no effect there). A pruned scope id that is
re-introduced later starts without a record — the next `baseline init` writes
the fresh value, never the old one. It removes only the baseline record of a
scope that is still configured but selects no files; under `--strict` the
scope stays a stale artifact until the project removes it from the config,
because the engine never mutates the config.

`baseline init` without `--strict` **documents**: it leaves stale records in
place and exits `0`, so a baseline can be initialized on an old tree. With
`--strict` it validates first — including running the counters of the
per-file measurement rules — and refuses without writing anything if a stale
artifact exists.

## The output

`stylish` (the default):

```
ratchet — 3 rules, 2 failed

  FAIL  missing-baseline              no baseline for module-test-coverage
  FAIL  unexpected-inner-html         7 > 5  (files)
  PASS  unwrapped-action-calls        9 ≤ 11  (call sites)

NOT COHERENT — 2 of 3 gates failed
```

### The report and the diagnostic channel

The **report** — the stylish output and the JSON document — contains **only
ratchet's own words**: a rule id, a status, a count, a baseline, a path
relative to the repository root, the exit status of a failed subprocess. The
output of a counter, a generator or `node` is never relayed into the report: a
subprocess that prints a timestamp, a hostname or an absolute path would make
the same tree produce different bytes.

A failed subprocess is therefore reported as *what* it was, not as *what it
printed*:

```
  FAIL  a        the counter exited with status 1
```

The raw output belongs to the **diagnostic channel**: `--debug` prints the
resolved config, every counter invocation and the raw output of every
subprocess. `--debug` is a transcript for one human reading one run and is
exempt from the determinism rule — two runs with `--debug` may differ.

`json` prints nothing but the JSON object on stdout; the notes and the debug
log go to stderr:

```json
{
  "schema": 1,
  "ok": false,
  "summary": { "rules": 3, "failed": 1, "executed": 3, "generated": 0, "drifted": 0 },
  "rules": [
    {
      "id": "unwrapped-action-calls",
      "status": "ok",
      "count": 9,
      "baseline": 11,
      "direction": "ceiling",
      "unit": "call sites",
      "class": "layer violation",
      "tier": "fast"
    }
  ],
  "generated": [],
  "notes": []
}
```

The status of a rule is one of `ok`, `over-baseline`, `under-floor`,
`baseline-raised`, `baseline-lowered`, `missing-baseline` and `counter-error`.
A rule entry may carry a `detail` field: the reason for a status that is not
`ok`. It is absent when there is nothing to say, so a machine consumer can
learn **why** a rule failed without parsing the human output.

```
{
  "id": "unexpected-inner-html",
  "status": "over-baseline",
  "count": 7,
  "baseline": 5,
  "direction": "ceiling",
  "unit": "files",
  "class": "layer violation",
  "tier": "fast",
  "detail": "count 7 > baseline 5"
}
```

The `generated` array carries one entry per generated file: its `path`, its
`status` (`ok`, `drift` or `new`) and the unified `diff` — absent when there
is none. `summary.generated` counts the generated files, `summary.drifted`
the ones that drifted. `summary.executed` counts the **checks** that ran — the
rules: a drift gate is not a check.

```
{
  "path": "docs/MISSING_FEATURES.md",
  "status": "drift",
  "diff": "--- docs/MISSING_FEATURES.md\n+++ docs/MISSING_FEATURES.md\n@@ -1,3 +1,3 @@\n # Title\n \n-stale content\n+fresh content\n",
  "detail": "differs from the committed file"
}
```

The `notes` array carries the same notes the human output prints on stderr —
the tier floor, the skipped monotonicity comparison, a skipped drift gate, a
baseline that was raised deliberately (with its reason). It is empty when there
is nothing to say. A machine consumer must be able to learn why the run is not
green **without** the note being only on stderr: a report that says `ok: false`
and gives no reason is a report that cannot be acted on.

```
"notes": ["the tier `fast` executed 2 checks, the floor is 5"]
```

A baseline that moved against its committed counterpart **without a reason** is
reported as `baseline-raised` / `baseline-lowered` — even when the count
comparison failed as well: the baseline itself is the invalid artifact, and the
count comparison is reported as an additional note. With a reason the move is
accepted: the reason is in the file and therefore in the diff, so the run
carries a note that names the move and its reason instead of a failure. A run
that executes no **check** at all fails as well: an empty selection is never a
pass. A check is a **rule** — the drift gates are not counted, they are gates
over the *generated* files — so a project whose only gates are drift gates
runs no check at all and is red. A `count` over a config without rules is a
successful run.

Every path in the output is relative to the repository root: an absolute path,
a machine name or a timestamp would make the same tree produce different bytes.

The `count`, `baseline` and `init` commands use the same envelope
(`schema`, `ok`, `summary`, `rules`, `generated`, `notes`); `count` reports the count
without a baseline (`"baseline": null`), and the `baseline` commands report one
entry per rule with an `action` (`added`, `kept`, `updated`, `refused`).

### The Markdown report

`ratchet report --format md` renders the run into **one Markdown file**: the
summary with the verdict of the run, one row per rule, the findings that count
and the notes of the run.

```
ratchet report --format md                    # to stdout
ratchet report --format md --output docs/ratchet.md
```

The Markdown report is the default of the `report` command — `--format md` says
it explicitly, and the other commands print the stylish output or the JSON
document.

The path of `--output` follows the **one rule**: it is relative and it lies
inside the repository root — resolved. A path that breaks the rule is an
operational error. The **directory** of the output has to exist: the command
writes one file and changes nothing else, and it does not create a directory
that the project did not have.

The report is a **rendering of a run**, not a second run: the same counters and
the same numbers. The report runs the **rules** only — the drift gates are
gates over the *generated* files, and the report is one of them, so a report
that held the result of the comparison with itself would be a regress. The
drift gates' verdicts belong to the `check` and to the diff the gate prints.

**The report of a red run is not green.** The report carries the run's
**verdict** and its **notes**, and the row of a rule that is not green carries
the **reason**: the file alone says why.

**The exit code is the exit code of the command**: `0` when the report was
produced, `2` when it could not be produced. The verdict of the run is in the
**content**: the report is a generator, and a generator that failed because
the run is red would fail the drift gate that invoked it.

```markdown
# Ratchet report

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
```

A red run says so — the verdict, the notes and the reason of every rule that
is not green:

```markdown
**Verdict: NOT COHERENT** — the notes below say why.

| todo-markers | unfinished work | ceiling | fast | 3 | 2 | over-baseline | count 3 > baseline 2 |
```

The findings are **the findings the number was made of**: the registry is read
once, the count is the number of the findings that count, and the report shows
those findings. A report that counted them again could contradict the number
it claims to report.

The generator is **deterministic**: the rows are sorted, and there is no
timestamp, no absolute path and no machine name. The same tree produces the
same bytes — otherwise the drift gate would report a difference that does not
exist. The report is a function of the **rules** alone: a generated file that
drifts does not change it.

`--output <path>` writes the file; the path is relative to the repository
root, and a path outside it is an operational error. The command writes that
one file and touches nothing else: the directory of the report has to exist —
the command does not create it.

**The report is drift-checked like every other derived artifact.** The project
registers it in `generated[]`, and the drift gate compares the committed file
with the produced one — the tool checks its own output:

```jsonc
"generated": [
  { "path": "docs/ratchet.md",
    "command": ["ratchet", "report", "--format", "md", "--output", "docs/ratchet.md"] }
]
```

Because the report holds no drift verdict, the comparison has no regress: the
committed report and the generated one agree, and a hand-edited committed
report is reported as drift.

## The exit codes

| Code | Meaning |
|---|---|
| `0` | green |
| `1` | at least one gate failed |
| `2` | operational error: invalid config, missing config, crashed counter, unreadable file, unknown command or path, a committed baseline that does not parse, a failed self-update |

The **report** is the exception: it exits `0` whenever it produced a report,
even when the run is red. It is a generator: a generator that failed because
the run is red would fail the drift gate that invoked it. The verdict of the
run is in the **content** of the report — and the `check` on the same tree
exits `1`.

The **release** has its own middle: it exits `1` when there is nothing to
release — no commit since the last release changes the version. The tool did
its job; the release that was asked for does not exist.

## The self-update

`ratchet update` replaces the binary with the newest release of this
repository. It needs no config and no repository: it touches the binary and
nothing else — never the tree of the project it runs in.

```sh
ratchet update
```

The run has four steps:

1. the newest release is resolved through the GitHub API
   (`/repos/all-the-rest/ratchet-cli/releases/latest`);
2. its version is compared with the version of the running binary — a release
   that is not newer is reported and nothing happens;
3. the asset for the platform is downloaded and its SHA-256 is verified
   against the `checksums.txt` of the release — **a checksum that does not
   match refuses the update and leaves the binary untouched**;
4. the binary is replaced.

### The assets

The names of the assets are defined **once**, in the binary, and mirrored in
the release workflow. A test compares the two, so the self-update can never
look for an asset the release does not produce.

| Asset | Name | Used by |
|---|---|---|
| The binary | `ratchet-<version>-<os>-<arch>` (`.exe` on windows) | `ratchet update`, the GitHub Action |
| The archive | `ratchet-<version>-<os>-<arch>.tar.gz` (`.zip` on windows) | the manual install |

The platforms are `linux-x86_64`, `linux-aarch64`, `macos-x86_64`,
`macos-aarch64` and `windows-x86_64`. `checksums.txt` covers every asset of
the release.

### The replacement

On unix the new binary is written next to the old one and **renamed** over it:
the running process keeps its inode, the file on disk is the new one. On
windows a running executable cannot be replaced — the new binary is written
next to it as `ratchet.exe.new`, and the command says what to do with it.

### The update check

`ratchet check` looks once a day whether there is a newer version, and prints
one line when there is:

```
ratchet: note: a new version is available: 0.2.0 — run `ratchet update`
```

The check is **silent on every failure**: a network problem must never turn a
green gate red. It runs at most once per 24 h — the timestamp lives in
`$XDG_CACHE_HOME/ratchet/update-check`, or `~/.cache/ratchet/update-check` —
and it is disabled with `--no-update-check` or `RATCHET_NO_UPDATE_CHECK=1`.
The CI sets that variable: a green run never depends on the network.

### The endpoints

| Variable | Meaning |
|---|---|
| `RATCHET_UPDATE_API_BASE` | The base of the release API. `https://api.github.com` by default. |
| `RATCHET_UPDATE_ASSET_BASE` | The base of the release assets. `https://github.com/all-the-rest/ratchet-cli/releases/download` by default. |
| `RATCHET_UPDATE_TARGET` | The binary that is replaced. The running one by default. |
| `RATCHET_NO_UPDATE_CHECK` | `1` disables the update check. |

The first three exist so that the tests can serve a fake release from a local
socket. Every invocation of the binary in the tests carries
`RATCHET_NO_UPDATE_CHECK=1` — an update check would reach the network — and
the issues counter of the tests is pointed at a local socket: **the tests never
touch the network**, and they never write into the cache of the machine that
runs them.

## The GitHub Action

This repository carries a **native action** (`action.yml`), so a project that
uses the gates never downloads a binary itself:

```yaml
- uses: all-the-rest/ratchet-cli@v1
  with:
    mode: validate
```

The action has **two modes**:

| mode | what it does | when |
|---|---|---|
| `validate` | installs the binary and runs `ratchet check`; a failing gate fails the job | every push and every pull request |
| `update` | installs the binary, runs `ratchet baseline update` and **commits** the changed baseline | only a deliberate, scheduled or manually triggered run |

The update mode is **opt-in**: a bot that moves the baseline silently on every
push defeats the monotonicity check. It commits the baseline with the reason
of the run and sets the identity of the commit (`ratchet[bot]`) — it does
**not** push: the push is the decision of the calling workflow.

The update mode **asks** the binary which file is the baseline
(`ratchet baseline path`) — it never guesses the path from the config. A
script that re-implements the resolution answers a different question than
the binary, and a wrong answer is a commit of the wrong file — or a green run
that committed nothing at all. It also **refuses a dirty working tree** and a
**detached HEAD**: with a clean tree on a branch, the file the binary changed
is the baseline file, and the commit contains it and nothing else.

The action decides nothing else: the rules, the direction and the units stay in
the project's `ratchet.config.jsonc`; the `tier` input selects the tier of
the **validate** mode.

### The inputs

| Input | Default | Meaning |
|---|---|---|
| `mode` | `validate` | `validate` or `update`. An unknown mode fails the job. |
| `version` | `v0.3.1` | The version of the binary the action installs — the tag of the release. |
| `config` | `ratchet.config.jsonc` | The path of the config, relative to the workspace. |
| `tier` | *(empty)* | `fast`, `slow`, or empty for every tier. The **validate mode only**: `ratchet baseline update` has no tier selection, so the update mode ignores the input. |
| `args` | *(empty)* | Extra arguments for `ratchet check`; they are split on whitespace. |
| `baseline-reason` | *(empty)* | The reason the update mode stores with the baseline move. |

The `baseline-reason` is **required** in the update mode: the baseline move is
a reviewed artifact, and the reason is what makes it reviewable — it travels
with the record, so the diff of the baseline file carries it.

### The workflow

The gates only read the repository: the default permission is enough.

```yaml
# .github/workflows/ratchet.yml
name: ratchet

on:
  push:
    branches: [main]
  pull_request:
    branches: [main]

permissions:
  contents: read

jobs:
  gates:
    name: The gates
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v5

      - name: The ratchet gates
        uses: all-the-rest/ratchet-cli@v1
        with:
          mode: validate
          tier: fast
```

The update mode **writes**: the workflow has to grant the permission — the
action cannot grant one.

```yaml
# .github/workflows/ratchet-baseline.yml
name: ratchet baseline

on:
  workflow_dispatch:
  schedule:
    - cron: "17 4 * * 1"

permissions:
  contents: write

jobs:
  baseline:
    name: The baseline moves
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v5
        with:
          ref: main

      - name: The ratchet baseline
        uses: all-the-rest/ratchet-cli@v1
        with:
          mode: update
          baseline-reason: "the nightly run pays the ceiling down"

      - name: Push the baseline
        run: git push origin HEAD:${{ github.ref_name }}
```

### The two version lines

- the **action** is pinned by a tag of this repository (`v1` — a moving major
  tag, or a concrete tag for a reproducible pin);
- the **binary** is pinned by the `version` input of the action, and the
  assets of a release are immutable per tag.

A project therefore pins the action, and the action pins the binary. The two
lines move independently: a new action can install an old binary, and a new
binary needs no new action.

The default of the `version` input is the release of this repository. A test
compares it with the version of `Cargo.toml`: a release has to name its
version in the action as well, or the action would install a release that
does not exist.

### The moving tag `v1`

The tag `v1` **names the commit that last touched the action** — `action.yml`
and the `action/` directory with the scripts of the composite action. It is
moved by a **workflow** of this repository, on the push that changes the
action: a forgotten move is not a thing that can happen, and a consumer on
`all-the-rest/ratchet-cli@v1` always runs the action as it is on `main`.

The push of a **release commit** moves the tag as well — a release changes the
`version` input of the action, and that is a change of `action.yml`. The
release workflow itself never touches the tag: the tag of the action names the
action, the tag of the release names a version.

The run is **idempotent** — when the tag already names the commit that last
touched the action, nothing is pushed — and it can be started by hand, which
is the repair after a rebase: a rewritten history has a new commit that
touched the action, and the tag follows it.

A project that pins the action by its **commit SHA** — as the portal does,
because its supply-chain contract allows no moving tag — is not affected: the
SHA names one commit, and the workflow never moves it.

### The install step

The action installs the **raw binary** of the release
(`ratchet-<version>-<os>-<arch>`, `.exe` on windows) — not the archive: the
runner needs no `tar`, no `unzip` and no platform-specific unpack step. The
platforms are `linux-x86_64`, `linux-aarch64`, `macos-x86_64`,
`macos-aarch64` and `windows-x86_64`; a platform the release does not
publish fails the step.

The download is **verified**: the SHA-256 of the asset is compared with the
`checksums.txt` of the release (`sha256sum` on ubuntu, `shasum -a 256` on
macOS). A missing checksum or a mismatch **fails the step** — nothing is
installed from an unverified download.

The verification protects against **corruption**, not against a hostile
origin: the checksum is fetched from the same origin as the asset, so the
origin is the trust anchor. Only the calling workflow — trusted code — may
move it (`RATCHET_ASSET_BASE`); a workflow that takes the base from an
untrusted input has moved the trust anchor itself.

The name of the asset is defined **once**, in the binary, and mirrored in the
action and in the release workflow. A test substitutes the template of the
action and compares the result with the function of the binary for every
platform: a renamed asset fails the test instead of the action.

### The Marketplace listing

The action is listed in the **GitHub Marketplace** under the name
`all-the-rest-ratchet-cli` — the slug GitHub derives from the name in
`action.yml`. The name must be **unique** across the Marketplace: a plain
`ratchet` is refused because it collides with an existing action, user or
organisation name, and the release form reports that before it publishes. The
name is the identity of the listing — renaming it creates a second listing and
leaves the first one behind.

The listing is attached to a **release**, not to the repository, and only a
human can create it: the release form carries the checkbox *Publish this Action
to the GitHub Marketplace* and the once-only agreement to the Marketplace
developer terms. A workflow cannot tick that box — so the release workflow
creates a **draft** with the assets, and publishing the draft is the click that
also publishes the listing. Without that click the release stays invisible in
the Marketplace, which is silent: the tag, the action and the binary all work.

The click is owed **per release**: a new release that is published without the
checkbox leaves the Marketplace on the older version. The draft is therefore the
normal state of a release of this repository — `Releases → the draft →
Publish release` is the last step of a release, not an optional one.

## The release step

A release of this repository is **two steps**: the tag pushes the release
workflow, the workflow creates a **draft** release with the assets of the five
platforms, and the draft is published by hand — that is the click that also
publishes the Marketplace listing (see [the Marketplace
listing](#the-marketplace-listing)).

`ratchet release` computes the next version from the commits since the last
release tag and renders the changelog block of that version. The version and
the changelog are **derived facts**: both are determined by the commit
history, and a hand-written version drifts from the history silently — the
tag says `0.2.0` while the history contains a breaking change.

The release is configured in the config file:

```jsonc
{
  "release": {
    "changelog": "CHANGELOG.md",
    "changelogFrom": "0.2.0",
    "tagPrefix": "v",
    "version": { "path": "Cargo.toml", "format": "toml", "section": "package", "key": "version" }
  }
}
```

**Where the version lives** is the project's business — the tool writes the
version into the manifest of the build tool, whatever language that is:

| Format | The project owns | An example |
|---|---|---|
| `toml` | the section and the key | `Cargo.toml` (`[package] version`), `pyproject.toml` (`[project] version`) |
| `json` | the key | `package.json` (`version`) |
| `line` | the prefix and the suffix of the line | `VERSION := 1.0.0` in a `Makefile`, `version = "1.0.0"` in a Gradle build |

The write is a **targeted edit**: the line that carries the value is
replaced, the rest of the file keeps its bytes and its formatting. A tool
that rewrote a manifest with its own serializer would reformat a file it does
not own — a diff of the whole file instead of a diff of the version.

**The step** follows the commits since the last release tag:

| Commit | Step | Changelog section |
|---|---|---|
| `feat:` | minor | `Added` |
| `fix:` | patch | `Fixed` |
| `perf:` | patch | `Changed` |
| `refactor:` | none | `Changed` |
| `!` in the subject, or a `BREAKING CHANGE:` footer | major | `Changed`, marked as breaking |
| `docs:` `chore:` `test:` `build:` `ci:` | none | none |

The mapping is configurable (`"steps"`), because a pre-1.0 project treats
minor and patch differently: a project that maps `feat` to `patch` never
leaves the `0.x` line by accident.

**The changelog** is a Keep-a-Changelog block. The date is the date of the
newest commit of the release — a fact of the history, not the clock: a block
that carried the date of the day it was generated would make the same history
produce different bytes on different days.

```markdown
## [0.2.0] - 2026-10-21

### Added

- the pre-release check (verify)
```

A commit whose **scope** is `security` goes into the section `Security`. The
scope is the marker: a subject that mentions the word is not a finding — a
commit that mentions "security" in a roadmap note is not a security change,
and a marker that is sometimes right is a marker that is often wrong.

The generator **appends** one block; it never rewrites the file. Below
`changelogFrom` the changelog is written by hand — the tool refuses to write
a block for a version below it and says so. From that version on, every block
is machine-written.

The command writes **two** files: the version file and the changelog. Both
paths follow the **one rule**: they are relative and they lie inside the
repository root — resolved. It writes **no tag**: the tag is the decision of
the human, and the release workflow publishes what the tag names. `--dry-run`
prints the release and changes nothing.

## This repo gates itself

The repository is gated with the binary it ships. `ratchet.config.jsonc` holds
the rules, `scripts/count.sh` holds the counters, `ratchet.baseline.json` holds
the baselines — the baseline file is written by the tool, never by hand.

| Rule | Class | Unit | Direction | Value today |
|---|---|---|---|---|
| `dependency-count` | supply-chain debt | dependency entries | ceiling | 8 |
| `no-machine-path-in-source` | a machine-specific path baked into the source | occurrences | ceiling | 0 |
| `no-panic-in-source` | crash instead of a clean failure | occurrences | ceiling | 0 |
| `no-unfinished-marker` | unfinished work | occurrences | ceiling | 0 |
| `no-unsafe-in-source` | unsafe code | occurrences | ceiling | 0 |

Four of the five rules count zero today: they are **boolean gates** — *no
occurrence may appear* — and their baseline must never move. `dependency-count`
is the only ratchet: its baseline of 8 dependency entries may only decrease.
`ratchet baseline update` moves it in that direction; `ratchet baseline raise`
moves it against it — deliberately, with a reason that travels with the record.

The baseline of 8 is itself the record of a deliberate raise: the self-update
needs an HTTPS client and a SHA-256 implementation, and the reason says so:

```sh
ratchet baseline raise --rule dependency-count --value 8 --reason "the self-update needs an HTTPS client and a SHA-256 implementation"
```

The counters live in `scripts/count.sh`, one case per counter name; each one
prints exactly one integer. They are plain `sh` and the tools every unix has
— grep, awk, find, sort, wc and tr. The constructs beyond POSIX are `grep -o`
and the word boundary `\b` in the patterns: GNU grep documents the word
boundary, BSD grep supports it, and both target platforms (macOS and ubuntu)
have them.

The counters rest on three assumptions, stated here so that the class and the
count describe the same thing:

- The four source counters match every `*.rs` file under `src/` — the crate's
  own sources, the inline `#[cfg(test)]` modules included. The integration
  tests under `tests/` are a separate crate and are **not** counted. The match
  is **textual**: the word in a comment or in a string counts as well, so the
  count can only be too high, never too low.
- The counted files are Rust sources. A NUL byte in one of them means the
  crate does not build; BSD grep prints `Binary file <path> matches` on stdout
  for such a file, so the counter would count that line as one occurrence — a
  false red, never a false green.
- The dependency counter counts **entries**: a key at the start of a line. A
  multi-line inline table whose continuation starts at column 0 is counted
  twice, and a manifest with CRLF line endings counts its blank lines as
  entries. Both over-count, which is the safe direction.

Locally, after a build:

```sh
cargo build
./target/debug/ratchet check
```

In CI the same gate runs on both matrix legs (ubuntu and macos) as the step
*Self-gate: ratchet check*.

What the gates do **not** cover, named here rather than in a footnote:

- **The drift between `SPEC.md`, this README and `--help`.** The test suite
  checks the documented **stylish report example** in this README and in
  `SPEC.md` byte for byte; `--help` is covered by neither the gate nor the
  tests.
- **The engine writes to the terminal.** A source-level gate for it has to
  decide which code is *compiled* — test modules, `cfg` conditions,
  attributes, macros — and that is a Rust parser, not a text scan. A scan
  produces false greens: a multi-line attribute that rustfmt keeps
  multi-line —

  ```rust
  #[cfg(any(
      feature = "alpha",
      feature = "beta"
  ))]
  ```

  — swallows the rest of the file, and a `println!` behind it counts zero:
  the gate reports green. It also produces false reds: a `#[test] fn` at file
  scope breaks a scanner that looks for a `mod` behind the attribute, and the
  counter fails on ordinary Rust. A gate that can be silently defeated is
  worse than no gate, so the class is not gated. What covers it instead: the
  test `the_stylish_output_is_the_documented_one` compares the **whole
  stdout** of `ratchet check` byte for byte with the documented example, so a
  stray print that reaches the report path fails the test — and CI runs the
  test on every push to `main` and on every pull request targeting `main`. A
  print in a path that no test exercises is not caught.
- **A panic that is not a `panic!`.** The `no-panic-in-source` gate counts
  `panic!` and `unreachable!` only. `unwrap()`, `expect()` and `assert!` are
  not counted — 174 such occurrences exist in `src/`, all of them inside the
  `#[cfg(test)]` modules, while the gate reports 0. A ceiling on them would
  turn every new test red, so the class stays open and is named here.
- **The integration tests.** The four source counters match every `*.rs` file
  under `src/`; the integration tests under `tests/` are a separate crate and
  stay outside the count. A panic in a test is how a test fails, so the class
  *crash instead of a clean failure* does not apply to them — the class is
  about the tool's runtime behaviour. The price is named: the `panic!` in
  `tests/cli.rs` is not counted.

## Not implemented yet

`SPEC.md` describes more than this binary does today. The self-update, the
release workflow, the GitHub Action and the release step exist now, and the
first release (`v0.1.0`) is published — but one thing is still open: the
**release step** writes the version into **one** file of the project — a
project that keeps its version in several places (a lock file beside the
manifest, the default of the action's `version` input) still has to move the
second one by hand. In this repository the second place is the action: the
test that compares the default with the manifest catches a forgotten move,
but the release commit has to make it.

## Specification

`SPEC.md` is the normative specification. Where the code and the spec disagree,
the spec wins and the code is the bug.
