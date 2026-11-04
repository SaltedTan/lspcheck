# lspcheck

lspcheck starts a language server and checks that it speaks the
[Language Server Protocol](https://microsoft.github.io/language-server-protocol/) correctly. It checks message
framing, JSON-RPC replies, the initialize/shutdown/exit lifecycle, and whether the server leaves processes behind. It
works with any server that talks over stdio, whatever language it is written in, and needs no configuration.

```console
$ lspcheck -- basedpyright-langserver --stdio
lspcheck 0.1.0 — basedpyright-langserver --stdio
  PASS  F1  MUST    Every byte on stdout belongs to a well-formed frame
  PASS  F8  SHOULD  A Content-Type the server sends declares UTF-8
  PASS  J4  MUST    An unknown request gets MethodNotFound (-32601), including a $/ request
  FAIL  L1  SHOULD  A request before initialize gets ServerNotInitialized (-32002)
                     got result "[]" instead of ServerNotInitialized
  PASS  L2  MUST    initialize returns capabilities and an offered position encoding
  PASS  L4  MUST    shutdown returns null and the server keeps running until exit
  PASS  L6  SHOULD  exit after shutdown ends the process with status 0
  PASS  L7  SHOULD  exit without shutdown ends the process with status 1
  PASS  L9  POLICY  No descendant process outlives the server
9 checks: 8 pass, 1 fail (0 MUST, 1 SHOULD, 0 POLICY), 0 skipped, 0 not exercised, 0 errors
```

lspcheck is early: it has 9 checks so far. [The design](docs/design.md) lists the rest, which cover framing edge
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

lspcheck runs the server several times, once per scenario, each time with an empty temporary workspace as its working
directory. A server path with a directory part, such as `./my-server`, is resolved before that; a bare name is looked up
on `PATH`. Framing and leftover-process checks are judged across every scenario.

With `--transcripts DIR`, lspcheck writes one JSON Lines file per scenario. It holds the bytes that reached the server's
stdin and the bytes read from its stdout, exactly as written and read, how lspcheck split them into frames, the first MiB of
stderr, and timestamps, so you can see what happened in a failing scenario.

lspcheck inspects at most `--max-output-mib` (256) of stdout per scenario and frames of at most `--max-frame-mib` (64).
These are lspcheck's own limits, not protocol rules: past them it keeps reading so the server isn't blocked, but stops
inspecting. A reply that never arrived is then reported as `ERROR` (inconclusive), not `FAIL`, if the output lspcheck
didn't inspect began before it stopped waiting, since the reply might have been in it. A failure lspcheck saw
directly, such as a wrong result, stays `FAIL`. The same happens when some of the server's frames can't be read at
all, and when lspcheck couldn't finish writing to a server that stopped reading its input: a request lspcheck
couldn't deliver gives `ERROR`, never `FAIL`. A header section longer than 1 MiB also stops inspection.

### Verdicts

| Verdict | Meaning |
| --- | --- |
| `PASS` | The rule held. |
| `FAIL` | The rule was broken; the detail line says how. |
| `SKIP` | The rule doesn't apply to this server. |
| `N/E` | Not exercised: the rule applies, but the run produced no evidence either way. |
| `ERROR` | The result is unknown: the server couldn't start or refused to initialize, a timeout ran out, or the evidence is incomplete. It says nothing about the rule. |

Each check has a basis: `MUST` for a normative rule of the LSP or JSON-RPC 2.0 specification, `SHOULD` for one of
their recommendations, and `POLICY` for hygiene that the specification doesn't require but clients rely on, such as
not leaving child processes behind.

### Exit status

- **0:** no `MUST` check failed.
- **1:** a `MUST` check failed, or with `--strict`, any check failed.
- **2:** a usage error, or an `ERROR` verdict with no `MUST` failure.

Some servers refuse to initialize without project files or settings. lspcheck reports that as `ERROR`, with the
server's message, rather than as a failure.

## Platforms

lspcheck runs on Linux, macOS and Windows. L9 finds the server's leftover processes through its process group on Linux
and macOS. On Linux lspcheck also makes itself a child subreaper, so it finds descendants that left the group with
`setsid`. On macOS such a descendant can't be seen, and the L9 result says so. On Windows L9 reports `N/E` for now.

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
