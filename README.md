# lspcheck

lspcheck starts a language server and checks that it speaks the
[Language Server Protocol](https://microsoft.github.io/language-server-protocol/) correctly. It checks message
framing, JSON-RPC replies, the initialize/shutdown/exit lifecycle, and whether the server leaves processes behind. It
works with any server that talks over stdio, whatever language it is written in, and needs no configuration.

```console
$ lspcheck -- basedpyright-langserver --stdio
lspcheck 0.1.0 — basedpyright-langserver --stdio
  PASS  F1  MUST    Every byte on stdout belongs to a well-formed frame
  PASS  J4  MUST    An unknown request gets MethodNotFound (-32601), including a $/ request
  FAIL  L1  SHOULD  A request before initialize gets ServerNotInitialized (-32002)
                     got result "[]" instead of ServerNotInitialized
  PASS  L2  MUST    initialize returns capabilities and an offered position encoding
  PASS  L4  MUST    shutdown returns null and the server stays alive until exit
  PASS  L6  SHOULD  exit after shutdown ends the process with status 0
  PASS  L7  SHOULD  exit without shutdown ends the process with status 1
  PASS  L9  POLICY  No descendant process outlives the server
8 checks: 7 pass, 1 fail (0 MUST, 1 SHOULD, 0 POLICY), 0 skipped, 0 not exercised, 0 errors
```

lspcheck is early: it has 8 checks so far. [The design](docs/design.md) lists the rest, which cover framing edge
cases, request ids, cancellation, document positions and robustness.

## What it doesn't do

lspcheck doesn't certify a server or award a badge. It doesn't judge whether answers are *right* (whether a hover is
accurate, or a definition points to the right place), because that needs knowledge of the language. It isn't a fuzzer
and doesn't test editors or clients. Every result says which rule it checks and how strong that rule is.

## Install

Until the first release, build it from source with Rust 1.99 or later:

```sh
cargo install --locked --git https://github.com/SaltedTan/lspcheck
```

## Use

```sh
lspcheck -- my-server --stdio            # run every check
lspcheck --only L4,L6 -- my-server       # run some checks
lspcheck --json report.json --transcripts out/ -- my-server
lspcheck list                            # every check, with its specification section
```

Each check runs in a fresh server process with an empty temporary workspace, and the server's working directory is
that workspace. With `--transcripts`, lspcheck writes one JSON Lines file for each scenario. The file holds every byte
sent and received and the server's stderr, with timestamps, so you can see exactly what happened in a failing scenario.

### Verdicts

| Verdict | Meaning |
| --- | --- |
| `PASS` | The rule held. |
| `FAIL` | The rule was broken; the detail line says how. |
| `SKIP` | The rule doesn't apply to this server. |
| `N/E` | Not exercised: the rule applies, but the run produced no evidence either way. |
| `ERROR` | lspcheck or the environment failed, for example when the server couldn't start. It says nothing about the rule. |

Each check has a basis: `MUST` for a normative rule of the LSP or JSON-RPC 2.0 specification, `SHOULD` for one of
their recommendations, and `POLICY` for hygiene that the specification doesn't require but clients rely on, such as
not leaving child processes behind.

### Exit status

- **0:** no `MUST` check failed.
- **1:** a `MUST` check failed, or with `--strict`, any check failed.
- **2:** a usage error, or an `ERROR` verdict with no `MUST` failure.

## Platforms

lspcheck runs on Linux, macOS and Windows. It finds the server's child processes on Linux and macOS. On Linux it also
finds children that left the server's process group, because lspcheck makes itself a child subreaper. On Windows, check
L9 reports `N/E` for now.

## Development

```sh
cargo test --all-features     # unit tests, and runs against the stand-in server
cargo clippy --all-targets --all-features -- -D warnings
```

`lspcheck-standin` is a small, correct language server with faults that can be switched on one at a time
(`--list-faults`). Every check has a test showing that the clean stand-in passes and the right fault makes it fail.
That test is required, because a check that can't fail is worse than no check.

## Licence

Apache-2.0. See [LICENSE](LICENSE).
