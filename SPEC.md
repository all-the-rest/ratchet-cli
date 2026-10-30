# SPEC — `ratchet` v0

Normative specification for the `ratchet` binary. The implementation follows
this document; where the code and this document disagree, this document wins
and the code is the bug.

`ratchet` implements the mechanics of the `ratchet-gates` skill: gates that
compare a **count** against a **baseline** that may only move in one declared
direction, and **drift gates** for generated files.

---

## 1. Vocabulary

| Term | Meaning |
|---|---|
| rule | one gate, one defect class |
| class | the defect class the rule counts (e.g. *layer violation*) |
| incident | the incident that motivated the rule (see 3.2) |
| unit | what one count unit is (e.g. *call sites*) |
| direction | `ceiling` (count must not exceed the baseline) or `floor` (count must not fall below it) |
| count | the current measurement |
| baseline | the committed number the count is compared against |
| tier | `fast` (seconds, no infrastructure) or `slow` (real infrastructure) |
| drift | a generated file differs from what its generator produces |

## 2. Files

| File | Role |
|---|---|
| `ratchet.config.jsonc` | the only source of truth for rules. Also accepted: `ratchet.config.json`, `ratchet.config.mjs`, `ratchet.config.cjs`. |
| `ratchet.baseline.json` | machine-written baseline, one record per rule. Never hand-edited. |

Discovery order for the config: `--config <path>` → `./ratchet.config.jsonc`,
`./ratchet.config.json`, `./ratchet.config.mjs`, `./ratchet.config.cjs` →
walk up from the working directory to the git root. `--no-config` skips the
lookup (only valid together with an explicit `--config`).

The baseline path is fixed (`ratchet.baseline.json`, next to the config) unless
the config sets `"baseline": "<path>"` in a top-level object.

The baseline file must lie **inside the repository root** (§2.1). A path outside it
cannot be compared with the committed version, and a baseline that cannot be
compared is a gate that does not gate. A config that points outside the root is
an operational error (§10).

### 2.1 The rule of the paths

Every path a project names — the baseline, the artifact of a counter, the
registry, a generated file, the snapshot of a check, the output of the report,
the config of `init`, the files of the release — follows **one rule**: the path
is **relative**, and it lies **inside the repository root**, **resolved**.

* **relative** — a committed config has to work on every machine: the same
  repository is checked out in different places, and an absolute path names
  one of them. A path of a project is therefore relative to the
  **repository root** — to the **directory of the config** for the baseline,
  which lives next to the config (§2).
* **inside** — a path outside the root names a file the project does not own.
  The engine would read it, write it, or compare it with the committed
  version — and none of that is the engine's business.
* **resolved** — the path is resolved **component by component**, the way the
  operating system resolves it, and the **whole** path is compared with the
  resolved root:
  * `.` is skipped;
  * `..` pops the resolved prefix;
  * a name that is a symbolic link is **followed**: its target is resolved the
    same way, and the walk continues from there.

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
root: there the comparison follows the link, as the drift gate intends (§7).

A path that breaks the rule is an **operational error** (§10) — the run ends
with exit code 2. A counter whose path breaks the rule reports its rule as a
`counter-error`: the same exit code, and the same kind of failure. The
**wording** is the subject's own: the baseline, the report, a counter, the
drift gate, the pre-release check, `init` and the release step each name the
path in their own words, and the baseline, the report and the counters name
the path of the config in a `config error at <path>` prefix.

## 3. Config format

The config is a **flat config** in the ESLint shape: an array of config
objects. Multiple objects are merged; a later object overrides an earlier one
for the same rule id. A single config object at the root is accepted as
shorthand for a one-element array.

```jsonc
[
  {
    "name": "noema",
    "tiers": {
      "fast": { "floor": 5 },
      "slow": { "floor": 1 }
    },
    "generated": [
      { "path": "docs/MISSING_FEATURES.md", "command": ["npm", "run", "docs:gaps"] }
    ],
    "rules": {
      "unwrapped-action-calls": {
        "class": "layer violation",
        "incident": "F-09: a server action was called without the shared error helper",
        "unit": "call sites",
        "direction": "ceiling",
        "tier": "fast",
        "count": { "command": ["node", "scripts/audit-inventory.mjs", "--print"] }
      },
      "unexpected-inner-html": {
        "class": "layer violation",
        "incident": "an unescaped HTML sink reached the renderer",
        "unit": "files",
        "direction": "ceiling",
        "tier": "fast",
        "count": { "file": "docs/_audit-raw.json", "path": "coherence.dangerouslySetInnerHTML" },
        "exclude": ["src/lib/markdown.ts", "src/lib/html-sanitize.ts"]
      }
    }
  }
]
```

### 3.1 Config object fields

| Field | Type | Meaning |
|---|---|---|
| `name` | string | optional label for this config object |
| `preset` | string | a rule set shipped with the binary, merged **before** this object (see 3.8) |
| `tiers` | object | per-tier settings (see 3.3) |
| `generated` | array | drift-gate entries (see §7) |
| `verify` | array | pre-release checks (see §15) |
| `rules` | object | rule id → rule object (see 3.2) |
| `release` | object | the release step and the changelog (see §16) |

### 3.2 Rule object fields

| Field | Type | Required | Meaning |
|---|---|---|---|
| `class` | string | yes | the defect class |
| `incident` | string | yes | the incident that motivated the rule (see below) |
| `unit` | string | yes | the count unit |
| `direction` | `ceiling` \| `floor` | yes | the allowed direction of the baseline |
| `tier` | `fast` \| `slow` | no (default `fast`) | which tier the rule belongs to |
| `count` | object | yes | how the count is produced (see 3.4) |
| `exclude` | string[] | no | glob patterns removed from the counted **items** (path-shaped exemptions only) |
| `files` | string[] | no | glob scope of the rule; without it the rule always runs |
| `description` | string | no | free text |
| `aggregate` | `count` \| `max` | required for per-file measurements | how file values reduce |
| `scopes` | array | no | named file scopes with independent baselines |

`exclude` requires a counter that yields **items**; a scalar counter plus
`exclude` is a configuration error.

**The `incident` field.** For a rule whose class has already occurred, the
incident names that concrete instance — it is what a later reader judges the
rule against when the count moves. For a **prevention rule** (a class whose
count is zero today), the incident says so plainly — for example *"prevention:
no instance in this project yet"* — instead of naming an incident that did not
happen. A gate that claims a history it does not have is worse than no gate:
the next reader cannot tell a real regression from a remembered one.

### 3.3 Tiers

```jsonc
{ "tiers": { "fast": { "floor": 5 }, "slow": { "floor": 1 } } }
```

`floor` is the minimum number of checks that must **execute** in that tier.
Below the floor the tier fails. A tier that executes **zero** checks always
fails — an empty selection is never a pass.

A check that ran and failed counts as executed: it fails anyway (exit 2), so
the floor only says how many checks the tier has to run. A check that was
never selected does not count — that is the empty selection, and it fails.

A **check** is a **rule**. The drift gates are not counted: they are gates over
the *generated* files, and the report — which is a generated file — cannot be
part of the count that decides whether the report is coherent. A tier that
declares a floor therefore needs that many rules.

The run as a whole fails when **no check ran at all** — the empty selection is
never a pass, whether the selection consists of rules, of drift gates, or of
both.

The floor of **every tier that runs** is enforced. Without `--tier`, every
declared tier's floor applies — the floor is what stops a tier from being
quietly narrowed, and a narrowing that only shows up when somebody selects
the tier by hand is a narrowing that survives.

### 3.4 Counter

Exactly one of:

```jsonc
{ "command": ["node", "scripts/count.mjs", "--json"] }
{ "file": "docs/_audit-raw.json", "path": "coherence.unwrappedCallSites" }
{ "file": "docs/_audit-raw.json", "path": "coherence.unwrappedCallSites", "generate": ["node", "scripts/audit-inventory.mjs"], "output_arg": "--output" }
{ "registry": "ratchet.findings.json", "severities": ["critical", "high"], "statuses": ["open", "in-progress"] }
{ "issues": { "repository": "owner/repo", "label": "ratchet" }, "severities": ["critical", "high"], "statuses": ["open"] }
{ "file_metrics": { "files": ["**/*.java"], "metric": "size_bytes" } }
{ "agent_rule": "unverified-commits", "since": "2026-10-30" }
```

* `command` — an argv array. **No shell** is involved; the arguments are passed
  through verbatim. The working directory is the repository root.
* `file` — a JSON file, optionally with a dotted `path` into it. Without
  `path` the root value is used. The file follows the **one rule** (§2.1): it
  is relative and it lies inside the repository root — resolved. The rule is
  about the path the project **names**, not about what the run reads from it:
  with `generate` the count comes from a temporary path, and the configured
  path is still the project's. A path that breaks the rule is an
  **operational error** (§10), and the message names the **configured** path.
* `generate` (only with `file`) — an argv array that regenerates the artifact.
  The binary runs it **before** reading the count and appends the **temporary
  output path** to the argv (`output_arg` is mandatory). The tree is never
  touched, and the count always comes from the freshly generated artifact — a
  count read from a stale artifact is a gate that lies.

  `generate` without `output_arg` is a configuration error: a generator that
  cannot write to a given path would have to write into the tree, and the tree
  is not the engine's to write. The in-place mode exists only for the drift
  gate (§7), which owns a verified restore.

  Without `generate`, the artifact is read as it is; the project is then
  responsible for its freshness. `output_arg` without `generate` is a
  configuration error — a silently ignored argument is a rule that does not do
  what it says.

`registry` and `issues` are the two **findings backends**: they do not produce
a raw value, they produce the findings the count is made of (§13). Both take
the optional `severities` and `statuses` lists, and an empty list is a
configuration error — a gate that counts nothing is green forever.

The value the counter produces may be:

| Value | Meaning |
|---|---|
| integer | the count |
| array of strings | the items; the count is the number of items after `exclude` |
| object `{"count": N}` | the count |
| object `{"count": N, "items": [...]}` | the count and the items |
| object `{"measurements": [{"path":"src/a.java", "value":N}]}` | nonnegative per-file values |

For the `command` form the stdout is parsed: trimmed, then tried as an integer,
then as JSON in the shapes above. Anything else is a counter error.

Per-file measurements may be emitted by a command counter using the
`measurements` object shape. Paths must be unique, repository-relative regular
files; malformed values, duplicates, absolute paths, traversal and paths that
resolve outside the root are operational errors. The `aggregate` is `count`
(distinct measured files) or `max` (greatest value). A rule with named scopes
must emit measurements; a scalar counter is rejected rather than silently
applying one value to every scope.

`file_metrics` is the built-in per-file source. Its `files` are path globs and
its metric is `size_bytes`, `line_count` or `max_line_length`. Byte size counts
raw bytes. Line count treats CRLF as one terminator, handles CR and LF, returns
zero for an empty file and adds no phantom line after a trailing terminator.
Maximum line length counts Unicode scalar values excluding terminators. The
walk is deterministic. A matching symbolic-link file is refused; an unmatched
symbolic-link file is ignored. Symbolic-link directories are refused rather
than traversed, so selected files cannot be silently omitted or read outside
the repository tree.

Named scopes take this shape:

```jsonc
"description": "All Java source files",
"aggregate": "max",
"scopes": [
  { "id": "auth", "description": "Authentication module", "files": ["src/auth/**"] },
  { "id": "legacy-login", "description": "Legacy login", "file": "src/auth/LegacyLogin.java" }
]
```

Exact files take precedence over module globs. Among globs, the greatest
literal/component specificity wins; equal specificity is an error. Unmatched
files contribute only to the global fallback. A configured scope that selects
no files is a **stale artifact** (§5.2): it is a note by default and an error
only under `--strict`. A scope whose files exist but are all removed by
`exclude` is not stale; it is simply not measured. Scope assignment and the
ambiguity check happen **before** `exclude`: an excluded path is still
assigned, so an ambiguous scope configuration surfaces even when every
ambiguous path is excluded. The rule-level `description` labels the global
fallback.

### 3.5 Unknown fields

An unknown field in the config, in a rule, or in a counter is a configuration
error. Typos fail loudly instead of silently disabling a gate.

### 3.6 JavaScript config

`ratchet.config.mjs` / `.cjs` is supported and is **data only**: it exports the
same JSON shape (default export). The engine is a standalone binary, so the
config cannot carry rule *functions* — a rule counts through `command` or
`file`.

The file is evaluated by `node`. If `node` is not on `PATH`, the error message
names the alternative (`ratchet.config.jsonc`). The bridge evaluates the module
and reads its default export; the result must be JSON-serialisable.

Known limitation: a repository path containing a backslash cannot be loaded
through the JS bridge (node rejects the percent-encoded backslash in a file
URL). The failure is closed — the error names the cause. Use
`ratchet.config.jsonc` in such a checkout.

### 3.8 Presets and built-in agent rules

A config object may name a **preset**: a rule set that ships with the binary.
The preset is merged *before* the object's own fields, so an object can add
rules or override one of the preset's rules by id.

```jsonc
[
  { "preset": "agent-rules" },
  { "rules": { "unverified-commits": { /* overrides the preset's rule */ } } }
]
```

A preset exists because `ratchet` has no `extends` across repositories: every
path a project names lies **inside** the repository root, so a rule set shared
between projects cannot be a file. A preset is versioned by the binary that
ships it — "every project runs the same rules" becomes a fact about the
release instead of a convention about copies.

The presets:

| Preset | Rules |
|---|---|
| `agent-rules` | `rule-file-name`, `rule-file-present`, `rule-file-max-lines`, `plan-status-line`, `gates-untouched`, `unverified-commits` |

The rules use the `agent_rule` counter, which runs a check compiled into the
binary — no script, no path, no drift between a rule and its counter:

| `agent_rule` | Counts |
|---|---|
| `rule-file-name` | tracked files named like `AGENTS.md` but spelled differently (case-sensitive) |
| `rule-file-present` | 1 when the root `AGENTS.md` exists, else 0 — used with `direction: floor` |
| `plan-status-line` | `docs/plans/*.md` without `**Status:**` in the first 15 lines; 0 without the directory |
| `gates-untouched` | commits not yet on `origin/main` (else `@{u}`) that change a gate path and product code in the same commit |
| `unverified-commits` | commits whose message has no `Verify:` line, in the window given by `since` (optional, `YYYY-MM-DD`) |

`since` belongs to `unverified-commits` alone; on any other rule it is a
configuration error, because a fixed tree cannot have an epoch. Without
`since` the whole history is counted: the number only grows, so the gate still
catches every new commit without a footer, and the baseline stays where it is.

The gate paths of `gates-untouched` are `.github/workflows/`,
`.github/actions/`, `.githooks/`, `.agents/gates/`, `scripts/gates.sh`,
`scripts/check_*.sh`, `ratchet.config.jsonc`, `ratchet.config.json` and
`ratchet.baseline.json`. Documentation (`docs/`, `*.md`, `*.json`, `*.yaml`,
`*.yml`) and the rule files themselves never count as product code: a fix that
documents itself is the normal case.

### 3.7 Glob patterns

`*` (any run of characters except `/`), `**` (zero or more path segments),
`?`, and `[...]` classes. Matched against repository-relative paths with `/`
as the separator.

A pattern that ends in `/**` also matches the directory itself: `src/**`
matches `src` and everything below it. A scope that selects nothing is a
failure, not a pass — the rule that would have run is not silently skipped.

## 4. Baseline file

```json
{
  "version": 1,
  "rules": {
    "unwrapped-action-calls": { "value": 11, "reason": "F-09 remainder, tracked in P1.2" },
    "java-file-size": { "value": 600, "reason": "current global fallback", "scopes": { "auth": { "value": 180 } } }
  }
}
```

* Machine-written by `ratchet baseline init`, `ratchet baseline update` and
  `ratchet baseline raise`.
* Never hand-edited.
* `reason` is optional and only present when one was given. It does more than
  explain a paydown: it is what makes a **deliberate move against the declared
  direction** reviewable — the reason travels with the record, so the diff on
  the baseline file carries it.
* Rules are sorted by id — the file is a stable diff.
* Each scope record stores its own `value` and optional `reason` under the
  parent rule's `scopes` map. Existing version-1 records without that map remain
  valid. `baseline init` and `baseline update` act independently on all
  executed scopes; `baseline raise --scope <id>` changes only that scope, while
  omitting `--scope` raises the global fallback.

The writer creates a missing parent directory at the path the baseline
**resolves to** (§2.1), not at a name that happens to lead there through a
link. It creates missing directories one at a time and tracks only those it
created. If a later creation fails, those directories are removed in reverse
order. A rollback failure names every directory left behind and says that the
working tree was modified; a failed creation that rolls back cleanly leaves no
directories behind. The error names the directory whose creation failed.

## 5. Commands

```
ratchet [check] [paths…] [options]
ratchet count [--rule <id>] [--format <stylish|json>]
ratchet baseline init [--rule <id>] [--reason <text>]
ratchet baseline update [--rule <id>] [--reason <text>]
ratchet baseline path
ratchet baseline raise --rule <id> [--scope <id>] --value <n> --reason "<text>"
ratchet drift [--path <path>] [--format <stylish|json>]
ratchet verify [--name <name>] [--update]
ratchet report [--output <path>] [--format md]
ratchet release [--from <tag>] [--dry-run]
ratchet init
ratchet update
```

The default command is `check`.

### 5.1 Options

| Option | Meaning |
|---|---|
| `--config <path>` | use this config file |
| `--no-config` | do not look for a config (requires `--config`) |
| `--format <stylish\|json\|md>` | output format, default `stylish` (`md` only for `report`) |
| `--output <path>` | write the report to this file (relative to the repository root) — a `report` option |
| `--fix` | apply the fixable: regenerate generated files — a **`check`/`drift`** option, not a global one |
| `--tier <fast\|slow>` | run only the rules of that tier — a `check` option |
| `--rule <id>` | restrict to one rule |
| `--strict` | fail closed on stale artifacts — a baseline record for a rule or scope the config no longer has, and a configured scope that selects no files; meaningful for `check`, `report` and `baseline init`, accepted (no-op) everywhere else |
| `--scope <id>` | address one named scope — `baseline raise` only |
| `--ignore <pattern>` | glob of paths to exclude from the run (repeatable) |
| `--name <name>` | run only this check — a `verify` option |
| `--update` | write the snapshot instead of comparing it — a `verify` option |
| `--from <tag>` | compute the release from this tag instead of the last release tag — a `release` option |
| `--dry-run` | print the release, change nothing — a `release` option |
| `--debug` | print the resolved config and every counter invocation |
| `--quiet` | only print failures |
| `--version`, `--help` | |

`--ignore` takes glob patterns of paths to exclude from the selection. A rule
whose scope does not intersect the remaining paths is skipped. Ignoring a path
never turns a failing gate green: a rule that would have run on the ignored
path still runs on the rest.

`--ignore` applies to the **rules' path scope**. It does not exclude a drift
gate: a generated file is registered deliberately, and a pattern that happens to
match it must not silently disable the gate.

### 5.2 `check`

1. Load and validate the config.
2. For every rule: run the counter → count (+ items).
3. Compare the count with the baseline:
   * `ceiling` — pass iff `count ≤ baseline`
   * `floor` — pass iff `count ≥ baseline`
   * no baseline → status `missing-baseline`, fail, hint: `ratchet baseline init`
4. Monotonicity — compare the **working-tree** baseline with the **committed**
   baseline (`git show HEAD:<baseline>`):
   * `ceiling` — the baseline must not have **increased** → `baseline-raised`, fail
   * `floor` — the baseline must not have **decreased** → `baseline-lowered`, fail
   * no git, or the file is not in `HEAD` → skipped, reported as a note, not a failure
   * the committed baseline exists but cannot be parsed → **operational error**
     (exit 2). A machine-written baseline that does not parse must not silently
     disable the gate.

   When the monotonicity check fails, it takes precedence over the count
   comparison in the report: the baseline is the invalid artifact, and the
   count comparison is reported only as an additional note.

   The monotonicity check accepts a move against the declared direction **only**
   when the baseline record carries a reason. A raise without a reason is
   rejected like any other forbidden move; a raise with a reason is allowed,
   because the reason is in the file and therefore in the diff — the change is
   visible, deliberate and reviewable on its own.
5. Tiers — `--tier` selects one tier. The number of **executed** checks in the
   selected tier must be ≥ the tier's `floor`, and must be > 0. Zero executed
   checks in the selected tier → fail ("empty selection").
6. Drift — the drift gates run as part of `check` (§7); `ratchet drift` runs
   only them. A drift is a **gate failure** (exit 1) — a stale committed file
   is the defect — while a generator that could not be run is an **operational
   error** (exit 2).
7. Exit code: `0` all green, `1` at least one gate failed, `2` operational error.

**Stale artifacts.** An exception artifact is *stale* when it belongs to
something the config no longer has:

* a baseline rule record whose rule is not configured;
* a baseline scope record whose scope that rule does not configure;
* a configured scope of a per-file measurement rule that selects no file.

Without `--strict` a stale artifact is a **note**, never a failure: the run is
green when everything still configured is green. A baseline record is named
with the hint `ratchet baseline update` (which prunes it, §5.5); a configured
scope that selects no file is named with the hint to remove the scope from the
config, because the engine never edits the config. A configured scope that
selects no file is skipped with a note for that scope only — the rule's other
scopes and the global fallback still run and are compared. There is no pruning
in `check`.

With `--strict` stale artifacts are **operational errors** (exit 2), one
message per artifact, and the run does not stop at the first. An empty
configured scope is today's counter error `scope <id> (<description>) selected
no files`; the rule contributes no measured result, and under `--strict` that
counter error is exit `2`. `report --strict` exits `2` as well when the
produced report is NOT COHERENT because of a stale artifact.

An `exclude` pattern that matches nothing is **always a note**, in every mode:
`exclude pattern "<pattern>" for rule "<rule>" matches nothing — ignored`. It
is never an error — not even under `--strict` — and it is never pruned,
because the engine never mutates the config.

The tolerance never makes an empty selection a pass: a run whose executed
checks drop to zero still fails. A rule whose counter selects no file at all —
no configured scope and no global fallback — is still a counter error. With
`--rule` the run does not check unknown rule records (naming a removed rule
already fails), and it checks only that rule's scope records and scopes.

`paths…` restrict the run to the rules whose counter is affected by those
paths. A rule without a path scope always runs. A rule may declare
`"files": ["src/**"]` to scope it; the rule is skipped when the selected paths
do not intersect its scope. A path that does not exist is an operational error
(exit 2) — a typo must not silently narrow the run into a green one.

### 5.3 `count`

Prints the current count per rule (no baseline comparison). Exit `0`, or `2` on
an operational error.

### 5.4 `baseline init`

Writes the **current count** as the baseline for every rule that has no
baseline yet. Existing baselines are never touched. `--rule` restricts it to
one rule. Touches only the baseline file.

Without `--strict` it **documents**: stale artifacts (§5.2) are left in place
and never fail the command. A baseline record for a removed rule, and a
baseline scope record for a removed scope, stay untouched — each is reported
as an `ignored` action saying so. A configured scope that selects no file
writes no record for itself and is reported as `ignored` as well. A rule
without `scopes`/`aggregate` that already has a baseline is still not counted,
even when its counter is broken; a per-file measurement rule (one with
`scopes` or `aggregate`) is counted regardless, because its scopes must be
validated.

With `--strict` the command validates **before it writes**. It collects the
unknown rule records (unless `--rule`), the unknown scope records, and the
empty configured scopes — the last by running the counter of every per-file
measurement rule, even one that already has a baseline. A counter failure
during this pre-flight is an operational error. If any stale artifact exists
the command exits `2`, lists every one of them, and the baseline file is
byte-identical afterwards: nothing is written, not even a parent directory.
If none exists it behaves like the default init and writes the missing
baselines.

### 5.5 `baseline update` (the paydown)

Moves the baseline to the current count, **only in the allowed direction**:

* `ceiling` — allowed iff `count ≤ current baseline`; otherwise refused:
  *"refusing: count 15 > baseline 12 — a ceiling may only decrease"*.
* `floor` — allowed iff `count ≥ current baseline`; otherwise refused
  analogously.

Rules that may not move are reported as refused and are **not** modified; the
command then exits `1` so the refusal is noticed. `--rule` restricts it to one
rule. `--reason <text>` is stored in the baseline record. Touches only the
baseline file. Never runs automatically.

`baseline update` is also the **cleanup step**, and it is always tolerant:
`--strict` has no effect here. It prunes every stale artifact (§5.2) from the
baseline and reports each as `pruned`:

* a record for a rule that is no longer configured — without `--rule`;
* a scope record for a scope the rule no longer configures — `--rule` limits
  this to that rule;
* the record of a configured scope that selects no file — only when the
  counter ran successfully, because an empty scope cannot be detected
  otherwise.

Pruning counts as a change, is saved with the rest, and is never a failure.
`baseline update` removes only the baseline record of a scope that is still
configured but selects no files; under `--strict` the scope stays a stale
artifact until the project removes it from the config, because the engine
never mutates the config.
A pruned scope id that is re-introduced later has no record: the next
`baseline init` writes the fresh value, never the old one.

A move **against** the declared direction is possible only as its own
deliberate command:

```
ratchet baseline raise --rule <id> --value <n> --reason "<text>"
```

It writes the new value **with the reason** and touches nothing else. A raise
without a reason is refused like any other forbidden move. `baseline update`
keeps its shape: it moves only in the allowed direction and refuses the other.

`ratchet baseline path` prints the repository-relative path of the baseline
file — the path the binary itself uses. It is the answer to the question "which
file is my baseline?", and it is what a script outside the binary must ask
instead of re-implementing the config's resolution.

### 5.6 `init`

Writes a starter `ratchet.config.jsonc` (one example rule) if no config
exists. Never overwrites an existing config.

The config is a path the project names, and the **one rule** (§2.1) applies
to it: a link at that position that leads out of the tree is refused, and the
starter config is not written through it.

### 5.7 `update`

See §9.

## 6. Output

### 6.1 `stylish` (default)

```
ratchet — 3 rules, 2 failed

  FAIL  missing-baseline              no baseline for module-test-coverage
  FAIL  unexpected-inner-html         7 > 5  (files)
  PASS  unwrapped-action-calls        9 ≤ 11  (call sites)

NOT COHERENT — 2 of 3 gates failed
```

The rules are sorted by id, the label column is the longest label plus six
characters plus a two-space separator, and the unit is separated by two spaces.
The example is the byte-exact output of the binary — the test
`the_stylish_output_is_the_documented_one` compares the whole stdout against
it, so the document cannot drift away from the implementation.

Under `--strict`, every NOT-COHERENT headline appends the stale-artifact count
when the run has at least one — not only when they are the sole failure:
`NOT COHERENT — N of M gates failed, K stale artifact(s)` — `stale artifact`
for one, `stale artifacts` for more. A stale artifact is not a gate, so `N`
and `M` count only the gates; the JSON schema is unchanged.

Every path in the output is **relative to the repository root**. An absolute
path, a machine name or a timestamp makes the same tree produce different
bytes — the determinism rule is not negotiable.

### 6.2 The report and the diagnostic channel

The **report** — the stylish output and the JSON document — contains **only
ratchet's own words**: a rule id, a status, a count, a baseline, a path
relative to the repository root, the exit status of a failed subprocess.

The output of a counter, a generator or `node` is **never relayed into the
report**. A subprocess that prints a timestamp, a hostname or an absolute path
would make the same tree produce different bytes, and no filter can be relied
on to catch every form of it.

A failed subprocess is therefore reported as *what* it was — **which rule and
which exit status** — not as *what it printed*. The command line is **not** part
of the report: an argument may be an absolute path, and §6.1 requires every path
in the report to be relative. The argv belongs to the diagnostic channel.

The same separation applies to a counter whose output cannot be parsed: the
report says that the counter did not print a count; the offending output goes
to the diagnostic channel.

The **diagnostic channel** is `--debug`: it prints the resolved config, every
counter invocation and the raw output of every subprocess — including the argv
and everything a subprocess printed. It is a transcript for one human reading
one run and is **explicitly exempt from the determinism rule**: two runs with
`--debug` may differ. Without `--debug`, two runs over the same tree produce
byte-identical output.

### 6.3 `json`

```json
{
  "schema": 1,
  "ok": false,
  "summary": { "rules": 3, "failed": 1, "executed": 3, "generated": 1, "drifted": 0 },
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
  "generated": [{ "path": "docs/MISSING_FEATURES.md", "status": "ok", "diff": null }],
  "notes": ["the tier `slow` executed 2 checks, the floor is 5"]
}
```

Rule status values: `ok`, `over-baseline`, `under-floor`, `baseline-raised`,
`baseline-lowered`, `missing-baseline`, `counter-error`.
Generated status values: `ok`, `drift`, `new`.

A rule entry may carry a `detail` field: the reason for a status that is not
`ok`. It is absent when there is nothing to say. A machine consumer must be
able to learn **why** a rule failed without parsing the human output.

The `notes` array carries the same notes the human output prints on stderr —
the tier floor, the skipped monotonicity comparison, a skipped drift gate, a
baseline that was raised deliberately (with its reason). It is empty when there
is nothing to say. A machine consumer must be able to learn why the run is not
green **without** the note being only on stderr: a report that says `ok: false`
and gives no reason is a report that cannot be acted on.

## 7. Drift gates

```jsonc
{ "generated": [ { "path": "docs/MISSING_FEATURES.md", "command": ["npm", "run", "docs:gaps"], "output_arg": "--output" } ] }
```

A `generated[]` entry may declare a `tier` (`fast` | `slow`, default `fast`).
`--tier fast` skips the slow generators: a generator that needs a database, a
browser or the network belongs to the slow tier, and a fast tier that waits for
it is a fast tier that is not fast.

The generated file follows the **one rule** (§2.1): it is relative and it lies
inside the repository root — resolved, the file included. A path that leaves
the root would let the engine write outside the tree it owns, and a link that
points out of it would take the write with it. The file itself may be a link,
as long as its **target** lies inside the root: there the file is the subject,
and the comparison follows the link (the pre-release check refuses a linked
snapshot instead, because there the *output* is the subject — §15).
A generated target that resolves to the repository root is refused: the root
is a directory, never a generated file to restore or remove.

A **dangling** link at the file position is refused with a named error. In the
in-place mode the generator would write through it and create the file behind
it, and the restore cannot take that file back — the tree would change. In the
temporary-output mode the tree is never touched, but the comparison would
follow the link to a file that is not there, and it would say nothing about
the generated file.

The generator must be able to write to a **given path**:

* with `output_arg` — the binary appends the temporary output path to the argv.
* without `output_arg` — the generator writes the file in place. The binary
  then owns a **verified restore**:
  1. snapshot the original bytes **and the file mode**;
  2. **create the directories the file needs** if they do not exist: a project
     cannot commit an empty directory, and a generator that writes a file into
     a new directory would fail otherwise — the gate would be red from its
     first run. The engine creates the directories the **operating system**
     needs to walk the named path: the **missing directories of the
     traversal** — `sub/../x.md` needs `sub`, because a `..` can only pop a
     directory that exists — and the **directory the path resolves to**
     (§2.1), the place the file lands in when a link leads there. The missing
     directories of the traversal are created **as they are named**: the path
     the project named is the path the engine walks. The directory the path
     resolves to is created **where the path resolves to**. A directory that
     cannot be created is an **operational error** that names the directory
     and the cause: the failure is the engine's, and the user is not sent to
     the generator. Directories are created one at a time and tracked as they
     succeed. On Unix, identity is the device and inode from a retained open
     directory handle. Directory creation itself returns no handle: POSIX
     `mkdirat` returns only status, and Linux rejects `open` with
     `O_CREAT | O_DIRECTORY`. Opening the created pathname is therefore a
     separate operation, and a replacement between creation and opening may
     be recorded as the candidate. This create-to-open race is accepted. On
     Windows, identity is the volume serial number and full 128-bit file ID
     from the high-resolution file-ID API; the legacy
     64-bit file index is never used because it can collide on filesystems
     such as ReFS. The high-resolution Windows ID is captured through the
     pathname rather than from the retained handle, leaving a narrow race
     between checking/opening the directory and capturing the pathname
     identity: a replacement in that interval may be recorded as the
     candidate.
     This race is accepted, in addition to the final check-to-remove race
     below. The retained handle helps prevent identity reuse where possible.
     If the filesystem cannot provide a high-resolution ID, that candidate
     is left in place and reported as an operational error; earlier candidates
     from the attempt are rolled back when they can still be identified.
     Existing directories remain usable. Each candidate is checked through
     the path authority before creation; after creation, its filesystem
     identity and containment are checked again. If post-creation validation
     fails, the candidate is left in place and named in the operational error.
     If a later creation fails, the engine attempts rollback only for paths
     it can still identify by their captured identity. Immediately before
     removing a path, it verifies both that the pathname still matches the
     identity captured for that candidate and that it still resolves inside
     the repository root. If identity cannot be established, has changed, or
     containment fails, cleanup leaves the pathname alone and reports the
     failure. Removal uses the pathname. Portable APIs do not make the final
     identity-and-containment check and removal atomic, so a replacement in
     that narrow check-to-remove interval may still be removed. This race is
     accepted;
  3. run the generator;
  4. read the produced bytes;
  5. restore: write the snapshot to a **temporary file in the same directory**,
     give it the original mode, and **rename** it over the target — a rename
     replaces a symlink instead of writing through it, and it does not need
     write permission on the target itself. If the target **is** a symbolic
     link, the restore writes the file **behind** the link and recreates the
     link: a rename over the link would replace it with a regular file and
     change the tree, and the tree must not change. A generator writes
     **behind** a link, so the file behind the link is the one that changes —
     a file created behind a **dangling** link is not removed, which is why a
     dangling link is refused;
  6. in an ordinary check (without `--fix`), the engine attempts to remove the
     empty directories of step 2. A directory that still holds something is
     not the engine's to remove: it was not left behind by the engine alone.
     Cleanup is attempted even if file or link restoration failed. Immediately
     before each removal, the engine verifies that the pathname still matches
     the identity captured for the created directory and still resolves
     inside the repository root. If identity differs, cannot be established,
     or containment fails, it leaves the path and reports an operational
     error.
     Removal is pathname-based, so the check and removal have a narrow,
     unavoidable TOCTOU race: a replacement in that interval may be removed.
     This race is accepted after the complete identity-and-containment check;
     the operation is not an atomic identity-bound deletion. A cleanup error
     names every directory it could not remove
     relative to the repository root and says that the working tree was
     modified;
  7. in an ordinary check, **verify** the restore (read the bytes back and
     compare), when file restoration and cleanup succeeded.

  If the restore fails, the run ends with an **operational error** that names
  the file and says that the working tree was modified. A swallowed restore
  error is a mutated tree that nobody was told about.

The comparison is against the **committed** file (`git show HEAD:<path>`), not
against the working-tree copy. The **lookup** uses the file the system
resolves the named path to: git keeps a symbolic link as a blob whose content
is the path of its target, and the files of the target are committed beside
it: the committed tree has no entry at the **named** path, and the comparison
has to follow the link to find the file. The report and the messages keep the
**named** path: that is the path the project named and the path the user
knows. The diff is a unified diff (committed → produced) with repository-relative
paths.

A file that is not in `HEAD` yet has the status `new` — informational, not a
failure.

`--fix` writes the produced content to the target path and reports what it
did. The gate stays **failed**: the committed file is still stale — the gate
measures the **committed** artifact, and it only turns green once the fix is
committed. A run with `--fix` in a pipeline is therefore never green against a
stale HEAD; that is deliberate, not a bug. `--fix` touches **only** the
content of generated files, never hand-written text. In either generator mode,
after the content has been fixed, the engine attempts to remove empty,
engine-created directories used only to traverse the named path; the resolved
target's parent directories are retained. Pre-existing and non-empty
directories are never removed. Immediately before each pathname removal the
engine verifies that the path still matches the identity captured for the
created directory and still resolves inside the repository root. If identity
differs, cannot be established, or containment fails, the path is left alone
and the run reports an operational error. The identity-and-containment check
and pathname removal cannot be made atomic with the portable APIs used here:
a replacement in the narrow check-to-remove interval may still be removed.
This TOCTOU race is accepted; cleanup is not claimed to be atomic
identity-bound deletion. Normal cleanup succeeds when the path is still the
tracked empty directory. A cleanup error names each path relative to the
repository root and says that the working tree was modified. The same
behavior applies to ordinary check restore cleanup above. Unix create-to-open
identity capture and Windows pathname identity capture have the accepted
races described above. Windows uses the high-resolution 128-bit file ID, not
the legacy 64-bit index; if that ID is unavailable, the newly created
candidate is left in place and the run reports an operational error.

## 8. Git

* Repository root: `git rev-parse --show-toplevel`; fallback: the directory of
  the config file.
* Committed baseline: `git show HEAD:<baseline-path>`.
* Committed generated file: `git show HEAD:<path>`.
* No git, or not a repository → the committed comparisons are skipped, a note
  is printed, the gate still runs.
* A committed file that exists but cannot be parsed is an **operational error**
  (exit 2), not a skip — a broken reference must not disable a gate.

## 9. Self-update

* `ratchet update` downloads the newest release asset for the platform from the
  GitHub Releases API, verifies the SHA-256 against `checksums.txt`, replaces
  the binary atomically and prints the new version.
* A release publishes **two** assets per platform: the **archive**
  (`ratchet-<version>-<os>-<arch>.tar.gz`, `.zip` on windows) for the manual
  install, and the **raw binary** (`ratchet-<version>-<os>-<arch>`, `.exe` on
  windows) for the self-update. The self-update takes the binary: unpacking an
  archive would need an inflate implementation, and the binary has no runtime
  dependency. Both names come from one stem, and the release workflow and the
  binary are checked against each other so the two cannot drift.
* The replacement: on unix the new binary is written next to the old one and
  **renamed** over it (the running process keeps its inode); on windows a
  running executable cannot be replaced — the new binary is written next to it
  and the command says what to do. Doing nothing silently is the worst of the
  three.
* A **wrong checksum** refuses the update and leaves the binary untouched.
* The version comparison is hand-written for `MAJOR.MINOR.PATCH` (a pre-release
  suffix is ignored, a missing field counts as zero). A malformed version
  fails closed: the update is refused, nothing is downloaded, nothing is
  touched.
* The version comes from the manifest (`Cargo.toml`), never from a string in
  the code, and the release workflow **checks the tag against the manifest**: a
  tag `v0.2.0` with a manifest that says `0.1.0` fails the release.
* An update **check** runs at most once per 24 h. The timestamp lives in
  `$XDG_CACHE_HOME/ratchet/update-check` (fallback `~/.cache/ratchet/update-check`).
  It is **synchronous but bounded** (2 s for the whole request) and it happens
  at most once a day; it is silent on every failure and it never changes the
  exit code. "Never blocks" means: it never decides the outcome and it never
  fails the run.
* Disable with `--no-update-check` or `RATCHET_NO_UPDATE_CHECK=1`. The CI sets
  the variable, so the CI runs stay hermetic.

## 10. Exit codes

| Code | Meaning |
|---|---|
| `0` | green |
| `1` | at least one gate failed |
| `2` | operational error: invalid config, missing config, crashed counter, unreadable file |

`ratchet report` is the exception: it exits **0** when it produced a report
(§14) — the report is a *generator*, and a generator that failed because the
run is red would fail the gate that invoked it. The run's verdict is in the
report's content. `ratchet check` is the gate.

## 11. Non-goals of v0

* No Node API and no npm plugin registry.
* No rule *functions* in the config — the engine is a runtime-free binary.
* No automatic fix of violations; `--fix` only regenerates generated files.
* No automatic baseline change; `baseline update` is always explicit.

## 12. The GitHub Action

A native action in this repository (`action.yml`), usable as
`uses: all-the-rest/ratchet-cli@v1` in any project. The tag `v1` is a
**moving major tag** — §12.1.

**Two modes** (the input `mode`):

| mode | what it does | when |
|---|---|---|
| `validate` | installs the binary, runs `ratchet check`; a failing gate fails the job | every push and every pull request |
| `update` | installs the binary, runs `ratchet baseline update` and **commits** the changed baseline | a deliberate, scheduled or manually triggered run |

An unknown mode **fails the job**: a mode that runs neither step would leave
the job green without having done anything.

**The inputs:** `mode`, `version` (the binary version the action installs),
`config`, `tier` (the tier of the **validate** mode — `ratchet baseline update`
has no tier selection, so the update mode ignores it), `args`,
`baseline-reason`.

**The install:** the **raw binary** asset of the release (no `tar`, no `unzip`,
no platform-specific unpack step), verified against the release's
`checksums.txt` — `sha256sum` or `shasum -a 256`, whichever the runner has. A
missing checksum or a mismatch **fails the step**; nothing is installed from an
unverified download.

**The naming:** the asset name is defined **once**, in the binary. The action
and the release workflow mirror it, and a test substitutes their templates and
compares the result with the binary's function for every platform — a renamed
asset fails the test instead of the action.

**The update mode** commits the baseline file and nothing else, with the
identity `ratchet[bot]`, and it **never pushes** — the push is the calling
workflow's decision. It needs a `baseline-reason` (an empty one fails the step)
and it moves the baseline only in the declared direction: a deliberate raise
stays a human decision, because a bot that raises on every push would defeat
the monotonicity check.

The action **asks the binary** which file is the baseline (`ratchet baseline
path`) — it never guesses it from the config. A script that re-implements the
config's resolution answers a different question than the binary, and a wrong
answer is a commit of the wrong file. The action also **refuses a dirty
working tree**: with a clean tree, the file the binary changed is the baseline
file, and the commit contains it and nothing else.

**The permissions** are the workflow's: the action cannot grant them. The
`validate` mode needs `contents: read`, the `update` mode `contents: write`.

**Two version lines:** the action is pinned by a tag in this repository, the
binary by the action's `version` input. A project pins the action; the action
pins the binary — the project never downloads a binary itself.

### 12.1 The moving tag `v1`

The tag `v1` is a **moving major tag**: it names the commit that last touched
the action — `action.yml` and the `action/` directory with the scripts of the
composite action. A consumer on `uses: all-the-rest/ratchet-cli@v1` runs the
action of that commit, and the tag is what makes "the action of this
repository" a name that needs no version of its own.

The tag is moved by a **workflow**, not by hand. The run finds the last commit
of the history that touched the action and points the tag at it, and the push
that changes the action is the push that moves the tag. A forgotten move is
then not a thing that can happen: the drift that a hand-moved tag hides — a
consumer on an old action, with nothing in the history to show it — has no way
in. The move happens on the push of a **release commit** as well: a release
moves the `version` input of the action, and that is a change of `action.yml`.
The release itself never touches the tag — the tag of the action names the
action, the tag of the release names a version (§16.3).

The run **fails closed**: a history in which no commit touched the action ends
it with an error, because a tag that names nothing names nothing. It is
**idempotent**: when the tag already names the target, the run pushes nothing
— a run that has nothing to say does not touch the remote. A manual run moves
the tag as well, which is the repair after a rebase: a rewritten history has
a new commit that touched the action, and the tag follows it.

**The permission** belongs to that workflow alone: the write it needs is the
tag, and no other workflow of the repository has it. The CI keeps
`contents: read`, and the release writes only in the job that publishes the
release — a workflow that does not need the write does not get it.

## 13. The findings registry

A JSON registry in the project: `ratchet.findings.json`.

The registry follows the **one rule** (§2.1): it is relative and it lies
inside the repository root — resolved. A path that breaks the rule is an
**operational error** (§10), and the message names the **configured** path.

```json
{
  "version": 1,
  "findings": [
    {
      "id": "SEC-01",
      "title": "org admins reassign foreign users",
      "severity": "critical",
      "status": "fixed",
      "status_since": "2026-10-19",
      "owner": "…",
      "ticket": "D-12",
      "regression_test": "OrgAdminDelegationBoundaryTest",
      "path": "backend/src/…",
      "note": "…"
    }
  ]
}
```

### The fields

| Field | Required | Meaning |
|---|---|---|
| `id` | yes | stable and unique; minted once where the source has none (`UI-001`, …) |
| `title` | yes | one line |
| `severity` | yes | one of the canonical levels |
| `status` | yes | one of the canonical statuses |
| `status_since` | no | the date the status was reached |
| `owner` | yes, unless the status is `fixed` | who owns the finding |
| `ticket` | yes, unless the status is `fixed` | the decision, the review round or the issue that tracks it |
| `regression_test` | no | the test that proves the fix |
| `path` | no | where the defect lives — a **pointer** for a reader, not a path the tool reads |
| `note` | no | free text |

An `owner` and a `ticket` are mandatory for everything that is not `fixed`: a
closed finding has its commit and its regression test, an open one has nobody
— a graveyard entry.

### The vocabulary

The levels and the statuses are **not** decided here: they are defined once in
the `ratchet-gates` skill
(`~/dev/agents-skills/.agents/skills/ratchet-gates/references/vocabulary.md`):

* the levels `critical`, `high`, `medium`, `low`, `info`;
* the statuses `open`, `in-progress`, `fixed`, `accepted`, `wontfix`.

An unknown level or status is a **hard error** (exit 2, with the offending
entry named), never a fallback. A registry that cannot be read is not a gate
that reports zero findings — it is an error.

### The counting rule

The count is the number of findings whose status **counts**: `open` and
`in-progress`. `accepted` and `wontfix` do not count but stay visible — a
decision is not a deletion. `fixed` is resolved.

**Which levels count is named by the rule**, not by the engine:

```jsonc
"count": { "registry": "ratchet.findings.json", "severities": ["critical", "high"], "statuses": ["open", "in-progress"] }
```

The rule's lists are **authoritative** when they are present: `statuses` names
the statuses that count, `severities` the levels that count. Without them, every
level counts and the counting statuses of the vocabulary count. An empty list is
a configuration error — a gate that counts nothing is green forever.

A rule with `direction: "ceiling"` and `baseline: 0` **is** the boolean gate
("no open finding"): the same mechanic, no second kind of gate.

### The registry is a view

The prose document (the security review closure, the UI review log) stays the
**human record**. The registry is the machine-readable view of it. The two are
migrated in the same commit and must not disagree about the severity of a
finding — a status written into the severity field destroys the severity, and
the migration has to recover it from the row.

### The issues backend

A project that already tracks its findings as GitHub issues can count them
there instead of keeping a second list beside them:

```jsonc
"open-findings": {
  "tier": "slow",
  "count": {
    "issues": {
      "repository": "owner/repo",
      "label": "ratchet",
      "severity_prefix": "severity:",
      "default_severity": "medium"
    },
    "severities": ["critical", "high"],
    "statuses": ["open"]
  }
}
```

`repository` is **required** and has the form `owner/repo`. `label` (default
`ratchet`), `severity_prefix` (default `severity:`) and `default_severity`
(default `medium`) are optional. `severities` and `statuses` are the lists of
the registry: they narrow the count in the same way, and without them every
level counts and the counting statuses count. `issues` is mutually exclusive
with `command`, `file` and `registry`. An unknown field, an empty label, an
unknown default level or a `repository` outside the form is a configuration
error that names its path.

**Membership.** The API is asked for the issues that carry the membership label
(`state=all&labels=<label>&per_page=100`, 100 per page). Pull requests are not
findings although the issues API carries them: an entry with a `pull_request`
field is skipped.

**The mapping.**

| Issue | Finding |
|---|---|
| label `<severity_prefix><level>` | `severity`; a label with the prefix and a level outside the vocabulary is an **operational error** that names the issue and the label |
| no severity label | `default_severity` |
| `state: "open"` | `status: "open"` |
| `state: "closed"` | `status: "fixed"` |
| the first assignee | `owner`; empty when the issue has none |
| `#<number>` | `ticket` and `id`; `title` is the issue title, `path` stays empty |

The findings are sorted by issue number. The report shows the findings the
count was made of, exactly as it does for the registry (§14), and the same
answer produces the same bytes.

**The failure modes.** A missing token, an unreachable API, a status other than
200, an answer that is not JSON, a missing field or an unknown state is an
**operational error** (exit 2). The pagination follows the `Link` header with
`rel="next"` and is bounded to 20 pages; more is an operational error. A
counter that cannot read the tracker never reports zero findings.

**The network exception.** The engine opens no connection at runtime except the
update check; this counter is the second exception, and it is **opt-in per
config** — a project that does not configure `issues` never opens a connection.
The token comes from `GITHUB_TOKEN`, then `GH_TOKEN`, then `gh auth token` from
the installed GitHub CLI. If all three sources are unavailable, the counter
fails closed. The base of the API is
`https://api.github.com` and can be overridden **only** by the environment
variable `RATCHET_ISSUES_API_BASE`, never by the config: the request carries
the token, and a base in a committed config would let the config decide where
the token of the person who runs the gate is sent. A rule that counts issues
belongs to the **slow** tier: the counter is a network call, and the fast tier
is defined as one that needs no infrastructure (§7). Since rules otherwise
default to `fast`, an issues rule must explicitly set `"tier": "slow"`; an
omitted tier or `"fast"` is a configuration error.

## 14. The Markdown report

`ratchet report --format md [--output <path>]` renders a run into **one
Markdown file**.

The path of `--output` follows the **one rule** (§2.1): it is relative and it
lies inside the repository root — resolved. A path that breaks the rule is an
operational error (§10). The **directory** of the output has to exist: the command
writes one file and changes nothing else, and it does not create a directory
that the project did not have.

The report and the number must come from **one source** — otherwise they
contradict each other. A gate result that is only visible on the terminal is
not reviewable, and a status table maintained by hand drifts from the numbers
it claims to report.

**The sections:**

0. a header that says the file is **generated** — "do not edit by hand" — and
   names the command that regenerates it. A derived file that does not say so
   is a file somebody edits by hand, and the edit is lost at the next
   regeneration;
1. a summary (the rules, the failed, the executed) **and the run's verdict**;
2. one row per rule (id, class, direction, tier, count, baseline, status, and the
   **detail** — the reason a status is not `ok`);
3. for every rule that counts a **registry**: the findings that count (id,
   severity, status, title, owner, ticket) — the report says *which* findings
   are open, not only how many;
4. the run's **notes** (the tier floor, the skipped comparisons).

**The report renders the gates that do not depend on the report.** The drift
gates are not in it: a report that contained the result of the comparison with
itself would be a regress, and the report is exactly what the drift gate
compares. The drift gates' verdicts belong to the `check` — and to the diff
the gate prints. The report is a snapshot of the **rules**: their counts, their
baselines and the findings behind them.

**The report of a red run is not green.** The report carries the run's
**verdict** and its **notes** (the tier floor, the skipped comparisons). A
committed artifact that says "0 failed" while the run is red is exactly the
drift this feature exists to prevent.

**The report is drift-checked like every other derived artifact:** the project
registers it in `generated[]`, and the drift gate compares it with the
committed version. The tool checks its own output — and because the report
contains no drift verdict, that comparison has no regress.

**The report is deterministic:** sorted output, no timestamps, no absolute
paths, no machine names. The same tree produces the same bytes — otherwise the
drift gate would report a difference that does not exist.

## 15. The pre-release check

A command is executed, its output is compared with a committed snapshot, and a
deviation fails the check.

A **benchmark is a measurement, not a defect class**. It has no meaningful
"baseline that may only sink" — the value moves in both directions and only
matters at the moment of a release. Encoding it as a permanent gate would either
freeze a number or produce permanent noise. So the pre-release check is **not a
ratchet**: it has no direction.

```jsonc
{
  "verify": [
    {
      "name": "benchmark",
      "command": ["cargo", "bench", "--", "--output", "<out>"],
      "snapshot": "snapshots/benchmark.txt"
    }
  ]
}
```

* The **snapshot** is a committed file, machine-written, the same discipline as
  the baseline: never hand-edited, the diff is the review artifact.
* The path of the snapshot follows the **one rule** (§2.1): it is relative and
  it lies inside the repository root — resolved.
* The command writes to a **temporary path** (like the drift gate's
  `output_arg`), so the tree is not modified by the check.
* The comparison is a diff, and the report shows it — a benchmark that moved by
  3 % is not a failure of the code, it is a number a human looks at.
* The comparison is against the **committed** snapshot, not the working-tree
  copy. A symbolic link in a directory component is followed for the Git
  lookup, which uses the repository-relative resolved path because Git does
  not traverse a committed link blob. The final snapshot itself must still be
  a regular file. Messages and diffs keep the configured path.
* It runs in the **release pipeline**, not in the commit gate: it needs the
  infrastructure the release needs, and a permanent gate over a measurement
  would be noise.

The mechanic is the drift gate (§7) with a different comparison: the drift gate
compares a *generated file* with its committed counterpart, the pre-release
check compares a *command output* with a committed snapshot.

**The command.** `ratchet verify [--name <name>] [--update]`:

* the **statuses** are `ok`, `drift` and `new` — the same vocabulary as the
  drift gates, for the same reasons (a snapshot that is not committed yet is
  `new`: informational, not a failure);
* the **exit codes**: `0` when every entry is `ok` or `new`, `1` when a
  snapshot differs, `2` on an operational error (the command could not be
  started, it failed, it wrote nothing, it wrote more than one file — the
  output would be a guess, the committed snapshot could not be read, the
  snapshot path breaks the rule (§2.1), the snapshot is not a regular
  file, the restore of the snapshot failed). **An empty selection is not a
  pass**: a `verify` without an entry exits `1` — the same rule as the drift
  command;
* `--name` runs a single entry; an unknown name is an operational error;
* `--update` **writes** the snapshot (the tool writes it, a human never edits
  it) and the check stays **failed** while the committed snapshot is stale —
  the same rule as the drift gate's `--fix`: the file is written, the detail
  says "commit it", and it turns green after the commit;
* the JSON document reuses the **check document's envelope** (§6.3) with the
  `generated` array replaced by a `verify` array.

**The snapshot is a regular file.** A symbolic link is refused with a named
error: the committed content of a link is its target, and comparing the
*output* with the target's content would say nothing about the output. (The
drift gate resolves links, because there the *file* is the subject; the two
gates have different subjects and say so.)

**The placeholder `<out>`** in the command is replaced by the temporary path
**wherever it appears in an argument** — not only as a whole argument. A
command that writes `--output=<out>` must write to the temporary file, or the
engine would compare the command's stdout with the snapshot and report a
green that means nothing.

**The snapshot is restored after the command.** The engine owns exactly one
file — the snapshot — and it gives it back unchanged:

1. take the state of the file: its **bytes**, its **mode** and its **time**;
2. touch the file with a time far enough in the past, so that a write of the
   **same bytes** is still visible (the marker);
3. run the command;
4. restore: the bytes, the mode **and the time** — and **verify** the
   restore;
5. a directory the command created and left empty is removed. Directory
   tracking follows the resolved parent of the snapshot, including when a
   linked directory leads to a target that did not exist before the command;
   a link itself and the repository root are never treated as directories to
   remove.

Before the command, Verify retains the filesystem identity of the resolved
snapshot target (when it exists) and each existing directory in its resolved
parent chain. Unix identities use device and inode from held handles; those
handles do not prevent a directory from being renamed. Windows uses the
high-resolution 128-bit file ID; the low-resolution file index is not an
identity because it can collide on ReFS. Windows directory handles are opened
without delete sharing and with final-reparse-point following disabled. Their
metadata is checked and a reparse point is refused. The handles for the
resolved parent chain, including the repository root, remain open while the
command runs. Consequently, on Windows the child command cannot rename or
delete a tracked snapshot-parent directory. This is an intentional prototype
trade-off; commands that need to move one of those directories may fail. The
pre-command snapshot-file handle is the exception: it permits delete sharing,
so the command can still rename or replace the snapshot file itself.
Because Windows identity capture is path-based, this permissive initial file
handle does not pin the name against an independent concurrent rename during
initial state capture; the post-command no-delete pinning guarantee begins only
with the post-command operation handles.

After the command, Verify resolves the configured path again against the
canonical repository root and requires it to resolve to the same authorized
target; it also checks the retained parent identities before each pathname-
based operation, timestamp update, removal, rename, or verification. Windows
post-command target and temporary-file handles deny delete sharing, permit read
and write sharing, and open the final component without following a reparse
point. Handle metadata is checked and a reparse point is refused. While such a
file handle and the resolved directory-chain handles are alive, pathname-based
high-resolution file-ID capture cannot be redirected by replacing or renaming
that file entry. The Windows ID API is still path-based: Verify does not claim
that it atomically obtains an ID from the retained handle or binds every later
pathname operation to that handle.

Reads and writes through retained file handles are issued as individual calls;
Verify repeats the complete containment and target-or-temporary identity check
immediately before every `read` or `write` call. A short transfer is followed
by a fresh check before the next call. Windows no-delete sharing prevents a
post-command file entry from being renamed or replaced while that I/O handle is
open, but read/write sharing still permits another process to modify its
contents. Unix and Windows both retain the per-call validation; neither claims
that path validation and a later pathname operation are one atomic operation.
A regular snapshot file replaced by the command is treated as a write to the
snapshot and restored as before. Verify refuses a reparse point anywhere in the
resolved parent chain, at the snapshot target, and at the temporary restore
file. The resolved chain contains no link by construction, so a reparse point
appearing there is a replacement and fails closed; the same checks refuse
non-name-surrogate reparse points such as cloud placeholders, which are not
followed. Configured in-repository directory-link resolution — including Windows
junctions, which resolve before the chain is captured — and the configured path
shown in diagnostics remain unchanged.

Cleanup considers only the missing traversal/resolved-parent candidates
recorded before the command. For a candidate that now exists, Verify checks
canonical repository containment before reading it, captures its identity,
and checks containment and identity again immediately before `read_dir` and,
if it is empty, immediately before `remove_dir`. Pre-existing directories are
not candidates; non-empty directories are left alone. A candidate that was
replaced, cannot be identified, or no longer resolves inside the repository is
left alone and reported as an operational failure. On Windows, candidate
directory handles are retained while they are inspected. Immediately after the
final validation and before pathname removal, Verify releases only the
candidate's handle; its tracked ancestors remain pinned. If the directory is
not empty after that check, Verify attempts to pin the same captured directory
again before continuing. A pathname replacement in the final check-to-
`remove_dir` interval remains possible. Windows `remove_dir` acts on a
replacement symlink or junction object itself, or fails; it does not traverse
the link to remove its target.

These checks do not make pathname operations atomic or derive a file ID from
the retained handle. For every pathname operation, the accepted race includes
the interval between the final complete containment-and-identity validation
and that operation. For Unix handle-based I/O, a path may also be redirected
after validation even though the open handle continues to refer to its original
object. On Windows, while the post-command I/O handle is open, no-delete sharing
prevents that file entry and its pinned resolved parents from being renamed or
deleted; this does not prevent in-place writes through another handle and does
not make path-based rename/removal atomic. No validation covers a sequence of
calls. If a later check detects redirection, the affected loop stops before its
next read/write call and Verify reports the restore or update failure. A
restore, cleanup, or snapshot-update failure says that the working tree may be
modified.

When `--update` writes a snapshot whose parent does not exist, the missing
parent directories are created at the snapshot's **resolved** path. They are
created one at a time, and only directories created by that attempt are
tracked. If a later creation fails, the attempt rolls those directories back;
if rollback cannot remove one, the error names what remains and says that the
working tree may be modified.

A command that rewrites the snapshot with the same bytes is detected through
the marker and restored like any other write. If the restore fails, the run
ends with an **operational error** that names the file and says that the
working tree may be modified — the same rule as the drift gate (§7). A swallowed
restore error is a mutated tree that nobody was told about.

## 16. The release step and the changelog

`ratchet release [--from <tag>] [--dry-run]` computes the next version from
the commits since the last release and renders the changelog block of that
version.

**The version and the changelog are derived facts.** Both are determined by
the commit history; a hand-written version drifts from the history silently —
the tag says `0.2.0` while the history contains a breaking change. The
history is the reference, never a version string somebody typed.

### 16.1 The configuration

The release is configured in the config file:

```jsonc
{
  "release": {
    "changelog": "CHANGELOG.md",
    "changelogFrom": "0.2.0",
    "tagPrefix": "v",
    "version": { "path": "Cargo.toml", "format": "toml", "section": "package", "key": "version" },
    "steps": { "feat": "minor", "fix": "patch", "perf": "patch" }
  }
}
```

| Field | Type | Required | Meaning |
|---|---|---|---|
| `changelog` | string | no (default `CHANGELOG.md`) | the path of the changelog file, relative to the repository root |
| `changelogFrom` | string | no | the **first version the tool manages**; below it the changelog is written by hand |
| `tagPrefix` | string | no (default `v`) | the prefix of the tag of a release |
| `version` | object | yes | **where the version lives** (see 16.2) |
| `steps` | object | no | the mapping from the commit type to the version step (see 16.3) |

A missing `release` section means: the changelog is `CHANGELOG.md`, the tag
prefix is `v`, the step table is the default one — and the **version file is
not configured**, so `ratchet release` is a configuration error: the tool
does not guess where the version of a project lives. A project without a
version in a file (the version is only in the tag) cannot use the command.

### 16.2 The version file

The version lives in **the file the project names** — the manifest of the
build tool, whatever language that is:

| Format | The project owns | An example |
|---|---|---|
| `toml` | `section` and `key` | `Cargo.toml` (`[package] version`), `pyproject.toml` (`[project] version`) |
| `json` | `key` | `package.json` (`version`) |
| `line` | `prefix` and `suffix` | `VERSION := 1.0.0` in a `Makefile`, `version = "1.0.0"` in a Gradle build |

The tool **reads** the version from that file and **writes** the new version
into it. The write is a **targeted edit**: the line that carries the value is
replaced, the rest of the file keeps its bytes and its formatting. A tool
that rewrites a manifest with its own serializer would reformat a file it
does not own — a diff of the whole file instead of a diff of the version.
The write is **atomic**: the content goes to a temporary file in the same
directory and is renamed over the target, so an interrupted write cannot
truncate the file.

* `toml` — the section is a dotted path (`tool.poetry` is the section
  `[tool.poetry]`); the key is the name of the entry in it.
* `json` — the key is the name of the member; it has to be a member of the
  **root** of the document (`package.json` has one). The members of the root
  sit at the smallest indentation of the document, and the line of the key
  has to sit there: a key that only exists in a nested object is not the
  version of the project, and writing into it would put the version where
  nobody reads it. The document has to be written **one member per line** —
  a compact document has no lines to write into.
* `line` — the line that starts with the prefix and ends with the suffix
  carries the version between them. The suffix may be empty.

A version file that cannot be read, a version that cannot be parsed, a value
that is not there — every one of them is an **operational error** that names
the file. A release command that guesses where the version is would write a
version into a file that does not carry one.

### 16.3 The step

| Commit | Step | Changelog section |
|---|---|---|
| `feat:` | minor | `Added` |
| `fix:` | patch | `Fixed` |
| `perf:` | patch | `Changed` |
| `refactor:` | none | `Changed` |
| `!` in the subject, or a `BREAKING CHANGE:` footer | major | `Changed`, marked as breaking |
| `docs:` `chore:` `test:` `build:` `ci:` | none | none |

The **base version** is the version of the last release tag, and the version
in the version file **has to be that version**: a file that says something
else means that the release was already prepared, or that the version was
changed by hand. Both are refused — the run ends with an operational error
that names both versions.

The step is **configurable** (`steps`), because a pre-1.0 project treats minor
and patch differently: a project that maps `feat` to `patch` never leaves the
`0.x` line by accident. The default table is the one above; the mapping
covers the types the project wants to change, the others keep their default.
The key `breaking` is the step of a commit that breaks the contract — it is
`major` by default.

The commits are the ones **since the last release tag** — `--from <tag>`
names another one. The tag is the reference, never a version string somebody
typed.

**A release tag is the prefix and three numbers** — `v1.2.3`. Nothing more:
a pre-release (`v1.2.3-rc.1`) is not a release, and the moving major tag of
the action (`v1`) is not a release either. A tag that is not a release tag
does not count, and naming one explicitly is refused — the version it stands
for would be a guess.

The moving major tag of the action follows the action (§12.1): a release
moves the `version` input of the action, so the release commit touches
`action.yml`, and the push of that commit is what moves the tag. The release
itself does not touch it — the tag of the action names the action, the tag of
the release names a version.

With **no release tag at all**, the version in the version file is the
version of the release: the first release is the version the project chose,
and the step does not apply to it.

### 16.4 The changelog

The block is a **Keep-a-Changelog** block:

```markdown
## [0.2.0] - 2026-10-21

### Added

- the pre-release check (verify)

### Fixed

- the restore of a snapshot that the command changed
```

The sections are the ones of Keep a Changelog (`Added`, `Changed`,
`Deprecated`, `Removed`, `Fixed`, `Security`), filled from the commit type.
A commit whose **scope** is `security` goes into `Security`: the scope is
the marker, and a subject that mentions the word is not a finding — a commit
that mentions "security" in a roadmap note is not a security change, and a
marker that is sometimes right is a marker that is often wrong. A commit that
breaks the contract goes into `Changed`, marked as breaking.

The **date** is the **committer date** of the newest commit of the release —
the day the change landed in this history. It is a fact of the history, not
the clock. A block that carries the date of the day it was generated would
make the same history produce different bytes on different days, and the
determinism rule (§2) forbids that.

The entries are the subjects of the commits, without the type and the scope
prefix, with the scope in parentheses. The order is the order of the commits,
oldest first.

The generator **appends** one block to the changelog file; it never rewrites
the file. Below `changelogFrom` the changelog is **written by hand**: the
tool refuses to write a block for a version below it and says so. From that
version on, every block is machine-written — the tool owns that part of the
file, and the human owns the rest.

A changelog file that does not exist yet is created with the preamble of Keep
a Changelog.

### 16.5 The command

`ratchet release [--from <tag>] [--dry-run]`:

* it writes **two** files: the version file and the changelog file. Nothing
  else in the tree is touched;
* both paths follow the **one rule** (§2.1): they are relative and they lie
  inside the repository root — resolved. A symbolic link inside the root can
  point outside it, and the write would follow the link;
* it creates **no tag**: the tag is the decision of the human, and the
  release workflow publishes what the tag names;
* `--dry-run` prints the computed version and the block and changes nothing;
* the **exit codes**: `0` when the release was prepared, `1` when there is
  nothing to release (no commit since the last release changes the version),
  `2` on an operational error (not a git repository, the tag cannot be read,
  the tag is not a release tag, the version file cannot be read or written,
  the version in the file is not the version of the last release, the
  changelog is below `changelogFrom`).

The run prints the **version**, the **tag** of the release (`v0.2.0`), the
**changelog block** and what it wrote. The block is the artifact the command
produces, and a person who asks for a release wants to read it — before the
run writes it and after. The tag is the one thing the tool does not write — a
tag is a promise, and a promise is not made by a tool.

The **JSON document** of the command carries the version, the tag, the tag the
release was computed from, the step, the entries with their type, scope and
date, the block, the files that were written and the notes — the same
information as the stylish output, in the machine shape. The document is
printed like every other one: one newline at the end, no more.
