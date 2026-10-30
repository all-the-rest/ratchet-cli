# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-10-21

The first release. This entry is written by hand — from `0.2.0` on, every
block is written by `ratchet release`.

### Added

- the gates: a count against a baseline that may only move in one declared
  direction
- the drift gates: the generated files, compared with their committed
  counterpart
- the findings registry: a list of findings becomes a number
- the Markdown report: a run rendered into one file
- the pre-release check: a command compared with its committed snapshot
- the GitHub Action: validate the gates, or move the baseline deliberately
- the self-update: the newest release, checksum verified

## [0.2.0] - 2026-10-21

### Added

- the release step — the version and the changelog from the history (release)

### Fixed

- the publish needs the repository, and the release can run again (release)
- the report says that it is generated — and how (report)

## [0.3.0] - 2026-10-21

### Added

- the drift gate creates the directory of the generated file (drift)

### Fixed

- the paths of a project may not leave the repository root (path)

## [0.3.1] - 2026-10-22

### Fixed

- the moving tag `v1` follows the action (action)
- the drift gate creates the directories the system needs (drift)

## [0.3.2] - 2026-10-23

### Fixed

- create verify and baseline directories at resolved paths (paths)
- compare linked snapshots at the resolved path (verify)
- roll back partial directory creation (drift)
- clean traversal directories behind links (verify)
- clean traversal directories during fixes (drift)
- revalidate snapshot restore paths (verify)
- resolve Windows-only clippy errors (ci)
- support config paths and pinning tests (windows)
- resolve action CI path failures (windows)
- resolve CI portability failures (windows)
- escape paths in report test config (windows)
- test deferred binary updates (windows)
- address approved-review follow-ups (verify)

## [0.3.3] - 2026-10-23

### Fixed

- build the linux assets on ubuntu 22.04 (release)

## [0.4.0] - 2026-10-24

### Added

- count GitHub findings as a backend (issues)
- add path-scoped metric gates (scopes)

## [0.5.0] - 2026-10-24

### Added

- use GitHub CLI token fallback (issues)

## [0.5.1] - 2026-10-24

### Fixed

- ignore unselected symlink files (file-metrics)

## [0.6.0] - 2026-10-30

### Added

- tolerate outdated exception artifacts and add --strict (#15) (stale)
- ship the agent-rules preset with built-in counters (#16) (config)

## [0.6.1] - 2026-10-30

### Fixed

- skip symlinked directories instead of failing (#19) (file_metrics)
