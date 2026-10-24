# AGENTS.md — ratchet-cli

`ratchet` is a standalone binary that enforces **ratchet gates**: quality gates
that compare a **count** against a **baseline** that may only move in one
declared direction, plus **drift gates** for generated files.

**Rules only, no decisions.** Durable decisions live in `SPEC.md`; open work in
GitHub Issues.

## Language

English — code, comments, docs, commit messages.

## Green commands (the gate)

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build && ./target/debug/ratchet check
```

All four must pass before a commit. `cargo fmt` (without `--check`) fixes
formatting; never hand-format.

The last command is the repository's own gate: `cargo build` produces the
binary in `target/debug/`, and `ratchet check` runs it from the repository
root. The rules live in `ratchet.config.jsonc`, the counters in
`scripts/count.sh`, the baselines in `ratchet.baseline.json`. A gate that
fails is a failed gate, not a gate to be moved: the baseline of a ceiling may
only decrease, and `ratchet baseline update` is the only way to move it.

## Dependencies

Keep them few and justified. Every dependency is a supply-chain and a build-time
cost; a standard-library implementation is preferred over a crate for anything
small. No network access at runtime except the (optional, once-a-day) update
check and the opt-in `issues` counter — a project that does not configure
`issues` never opens a connection.

## Commit convention

One Conventional Commit per self-contained change
(`feat|fix|docs|refactor|test|chore|perf` + optional scope). `git add` with
explicit paths — never `git add -A`. Verify with `git show --stat` before
committing. The merge queue squashes, so `main` carries one commit per PR;
amend only unpushed commits.

## Open work — GitHub Issues

Every actionable item is a GitHub issue with a Conventional-commit-style title,
one severity label (`severity:critical`, `severity:high`, `severity:medium`,
`severity:low` or `severity:info`) and an assignee. A pull request closes its
issue with `Closes #N`. There is no `AGENTS.todo.md` in this repository.

## Branches, commits, pull requests

Never commit to `main`. Branches use `<type>/<slug>`. Use one Conventional
Commit per self-contained change, stage explicit paths, and verify with
`git show --stat`. A pull request names the issue it closes and its verification.
The required CI checks are the three matrix jobs (`ubuntu-latest`,
`macos-latest`, `windows-latest`). Merge only through the merge queue with
`gh pr merge`; never use `--admin` and never push directly to `main`.

## Design rules that are not negotiable

1. **The engine never mutates the tree** — except `--fix`, which exists for
   exactly that one purpose (regenerating generated files).
2. **Deterministic output**: sorted, no timestamps, no absolute paths, no
   machine names. The same tree produces the same bytes.
3. **A baseline is machine-written**, never hand-edited, and may only move in
   the direction the rule declares.
4. **A counter is project-specific, the mechanic is not.** The binary owns
   baseline handling, direction, monotonicity and reporting; the project owns
   the counting.
5. **Fail closed**: an unreadable config, a crashed counter or an empty
   selection is a failure, never a green.

## Release

`cargo build --release` today; the GitHub Actions release workflow (tag `v*`)
lands with a later task. Its artifacts are per-OS/arch tarballs with a
`checksums.txt`; `ratchet update` verifies the checksum before replacing the
binary.
