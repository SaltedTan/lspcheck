# Repository instructions for coding agents

lspcheck starts a language server over stdio and checks that it follows LSP 3.17 and JSON-RPC 2.0: framing, replies,
lifecycle, and whether it leaves processes behind. Read the [README](README.md) for what users see and the
[design](docs/design.md) for the check catalogue, architecture and milestones.

| Path | What it holds |
| --- | --- |
| `src/main.rs` | The command line (clap) and the hidden process-group sentinel mode. |
| `src/checks.rs` | The check catalogue, the scenarios that exercise it, and the verdict rules. |
| `src/session.rs` | One run of a server: pipes, deadlines, request matching, transcripts, cleanup. |
| `src/transport.rs` | The incremental frame reader and the header and media-type grammar. |
| `src/message.rs` | JSON-RPC classification, ids compared by exact value, message builders. |
| `src/process.rs` | Process groups, the Linux child subreaper, and the leftover-process search. |
| `src/text.rs` | The text model: line splits, position encodings, content changes, where replies carry positions. |
| `src/capabilities.rs` | K1: the requests advertised capabilities call for, their positions, and K1's verdict. |
| `src/metamodel.rs` | K2: the vendored LSP 3.17 meta-model, parsed once, and validation of JSON values against its types. |
| `vendor/lsp-3.17/` | The LSP 3.17 `metaModel.json`, unmodified (CC BY 4.0; attribution in its README). |
| `src/mutation.rs` | The seeded mutation planner: SplitMix64, the lexicon, the operators, the changes that send an edit, where requests are asked, and how edits are recorded. The `mutation` scenario that runs it is in `src/checks.rs`. |
| `src/sha256.rs` | SHA-256, for recording long inserted text without a dependency. |
| `src/documents.rs` | The documents lspcheck opens, the texts a position may refer to, and D1's judging. |
| `src/profile.rs` | `--profile`: reading and validating the TOML, the workspace files, and merging with the flags. |
| `src/report.rs`, `src/transcript.rs` | Reports; JSON Lines transcripts. |
| `src/bin/standin.rs` | `lspcheck-standin`, a correct test server with faults switched on one at a time. |
| `examples/profiles/` | Example profiles for stock servers, each with a tiny project and a `[document]`. |
| `ci/stock-servers/` | Expected verdicts of pinned stock servers, compared by `.github/workflows/stock-servers.yml`. |
| `tests/` | The CLI against the stand-in (`standin.rs`) and session-level guarantees (`session.rs`). |

## Build, test and lint

The toolchain is pinned in `rust-toolchain.toml`.

```sh
cargo build --all-features
cargo test --all-features                        # unit tests and runs against the stand-in
cargo test --all-features --test standin NAME    # one focused test
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
```

CI runs clippy on Linux and the tests on Linux, macOS and Windows. Code under `cfg(unix)`, `cfg(target_os = "linux")`
or `cfg(not(unix))` is easy to break for the other targets, so lint them locally as well:

```sh
rustup target add x86_64-apple-darwin x86_64-pc-windows-gnu
cargo clippy --all-targets --all-features --target x86_64-apple-darwin -- -D warnings
cargo clippy --all-targets --all-features --target x86_64-pc-windows-gnu -- -D warnings
```

Before a pull request, run fmt, clippy on all three targets and the full test suite. If you skip one, say which and
why.

## Rules that keep verdicts trustworthy

Server authors will act on what lspcheck reports, so a wrong verdict is the worst bug it can have.

- **Every check can fail.** Each check has a stand-in fault that makes it fail and a test showing that, and the clean
  stand-in passes it. Lawful but unusual behaviour gets a fault too, with a test that nothing fails.
- **A regression test must catch the bug.** Run a new test against the commit before the fix (a `git worktree` with
  the new stand-in and tests copied in, with `cargo test --no-fail-fast`) and confirm it fails there.
- **FAIL means lspcheck saw the rule broken.** A reply that never arrived is a failure only if nothing lspcheck failed
  to inspect could have held it. A request lspcheck couldn't deliver is an `ERROR`. lspcheck's own inspection limits
  are not protocol rules.
- **Bound every wait.** Every write, wait and cleanup step has an absolute deadline; nothing joins a thread or waits
  on a process without one.
- **No timing races in tests.** GitHub-hosted runners are slow and uneven. Don't rely on a narrow window; order events
  with a handshake (the stand-in's children print `ready`) and give deadlines generous margins.
- Each check states its basis (`MUST`, `SHOULD` or `POLICY`) and links its section of the specification.

## Changes

- Keep changes small and focused, with tests. Work on a branch and open a pull request; CI must pass.
- Changes to `.github/workflows/`, dependencies (`Cargo.toml`, `Cargo.lock`) or the pinned toolchain need the
  maintainer's merge.
- Update the README, `docs/design.md` (including its status line) and `CHANGELOG.md` under *Unreleased* for
  user-visible changes.
- New source files start with the copyright and SPDX header the existing files use. The project is Apache-2.0.
- Reporting findings to other projects' servers, such as those in a CI matrix, is outward-facing: ask the maintainer
  first.

## Releases

`.github/workflows/release.yml` builds `lspcheck` for Linux (x86_64 and aarch64, static musl), macOS (x86_64 and
arm64) and Windows (x86_64), each on a runner of its own platform, runs each binary against the stand-in, and on a
tag push publishes the archives with a `SHA256SUMS` file and build provenance. A pull request that changes the workflow,
and a manual run (including one targeting a tag), use *Unreleased* notes and do everything but attest and publish.
To release version X.Y.Z:

1. In a pull request, set `version` in `Cargo.toml` (and `Cargo.lock`), and rename the changelog's *Unreleased*
   heading to `## X.Y.Z (YYYY-MM-DD)`, with a fresh empty *Unreleased* above it. The release notes are that section.
2. After it merges, the maintainer tags the merge commit `vX.Y.Z` and pushes the tag. The workflow refuses a tag that
   doesn't match `Cargo.toml`, or a version the changelog has no section for.
