# lspcheck

lspcheck starts a language server and checks that it speaks the
[Language Server Protocol](https://microsoft.github.io/language-server-protocol/) correctly. It checks message
framing, JSON-RPC replies, the initialize/shutdown/exit lifecycle, and whether the server leaves processes behind. It
works with any server that talks over stdio, whatever language it is written in, and needs no configuration.

```console
$ lspcheck -- basedpyright-langserver --stdio
lspcheck 0.1.0 — basedpyright-langserver --stdio
  PASS  F1  MUST    Every byte on stdout belongs to a well-formed frame
  PASS  F2  MUST    Content-Length counts the body's bytes, not its characters
  PASS  F3  MUST    Input split across writes, down to single header bytes, is read
  PASS  F4  MUST    Several frames in one write are all read
  PASS  F5  MUST    Input with 2-, 3- and 4-byte UTF-8 characters keeps the stream in step
  PASS  F6  MUST    A Content-Type header with charset=utf-8 from the client is accepted
  PASS  F7  SHOULD  The legacy charset=utf8 spelling from the client is accepted
  PASS  F8  SHOULD  A Content-Type the server sends declares UTF-8
  PASS  J1  MUST    A reply keeps its request's id: a number stays a number, exactly
  PASS  J2  MUST    Exactly one response per request: none duplicated or unasked
  PASS  J3  MUST    Notifications get no response, including unknown and $/ ones
  PASS  J4  MUST    An unknown request gets MethodNotFound (-32601), including a $/ request
  PASS  J5  MUST    Envelopes are well formed: jsonrpc 2.0, one of result/error, an integer code
  PASS  J6  MUST    Concurrent requests are each answered with the right id
  FAIL  L1  SHOULD  Before initialize, a request gets ServerNotInitialized (-32002) and a notification no reply
                     got result "[]" instead of ServerNotInitialized
  PASS  L2  MUST    initialize returns capabilities and an offered position encoding
  FAIL  L3  POLICY  A second initialize gets an error and the server keeps serving
                     a second initialize got result "{\"capabilities\":{\"textDocumentSync\":{\"willSave\":true,\"change\":2,\"openClose\":true"… instead of an error
  PASS  L4  MUST    shutdown returns null and the server keeps running until exit
  FAIL  L5  SHOULD  A request after shutdown gets InvalidRequest (-32600)
                     a request after shutdown got result "[]" instead of InvalidRequest
  PASS  L6  SHOULD  exit after shutdown ends the process with status 0
  PASS  L7  SHOULD  exit without shutdown ends the process with status 1
  PASS  L8  POLICY  End of input without exit ends the process
  PASS  L9  POLICY  No descendant process outlives the server
  PASS  C1  MUST    A cancelled request still gets exactly one response
  PASS  C2  MUST    Cancelling an unknown or finished request produces nothing
  PASS  C3  MUST    No response to $/cancelRequest itself
26 checks: 23 pass, 3 fail (0 MUST, 2 SHOULD, 1 POLICY), 0 skipped, 0 not exercised, 0 errors
```

lspcheck is early: it has 26 checks so far, covering framing, JSON-RPC correlation, the lifecycle and cancellation.
[The design](docs/design.md) lists the rest, which cover framing edge cases, request ids, document positions and
robustness.

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
on `PATH`. The checks on the server's output (F1, F2, F8) and on leftover processes (L9) are judged across every
scenario, as are the JSON-RPC correlation checks J1, J2 and J5.

The JSON-RPC scenario writes four requests, with numeric and string ids, before reading any reply (J6), and then sends
unknown notifications followed by probes to show that a notification gets no response (J3). A reply whose id only
loosely matches the request's (a number spelled as a string, or one that lost precision as a double) fails J1, and a
duplicate, unasked or premature reply fails J2.

The checks on how the server reads its input (F3 to F7) send small requests the server doesn't know, which every server
must answer, written in different ways: split across many writes, several frames in one write, with a `Content-Type`
header, or carrying multibyte characters. Each way is judged only after a plain request has been answered, so lspcheck
knows the server answers such requests and that its input is in step. If the server stops answering after one of
them fails, the rest are reported as `ERROR`, not `FAIL`; if it never answers a plain request, they are `N/E`.

F2 needs output with non-ASCII text, so lspcheck sends a request whose string id has some, which the response must
echo. A server that escapes all non-ASCII text as `\u` sequences (which is lawful) gives no evidence, and F2 reports
`N/E`.

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
