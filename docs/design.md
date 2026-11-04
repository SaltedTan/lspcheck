# lspcheck design

Drafted 2026-11-04. Licence: Apache-2.0. Language: Rust.

**Status:** milestone M0 and part of M1 are done: the transport, process, and session layers, the stand-in server, and checks F1, F3 to F8, J4, L1, L2, L4, L6, L7 and L9. F1, F8 and L9 are judged across every scenario. Everything else below is planned.

## Purpose
A command-line tool that starts **any** language server binary over stdio and checks that it speaks the protocol
correctly: framing, JSON-RPC correlation, lifecycle, cancellation, document positions, and process cleanup. It needs no
knowledge of the server's language for its core checks. It reports what it checked, what it could not exercise, and a
transcript that reproduces each failure.

```
lspcheck -- gopls                          # default checks, temp workspace, no profile
lspcheck --profile go.toml -- gopls serve  # with a sample workspace and document
lspcheck --json report.json --junit junit.xml --transcripts out/ -- ./my-server --stdio
lspcheck list                              # the check catalogue with spec references
lspcheck replay out/L5.jsonl -- ./my-server   # re-run one failing session
```
Exit status: 0 when no MUST check failed; 1 when one did; 2 for a usage or checker error. `--strict` also fails on
SHOULD checks.

## Non-goals
- Not a certification or "LSP compliant" badge. Each check states its basis, and the report lists what wasn't exercised.
- Not semantic correctness of answers (hover text, definitions): that needs language oracles. Language profiles may add
  source mutation and recovery later, but not in the core.
- Not coverage-guided fuzzing (LSPFuzz does that with instrumented builds).
- Not an editor or client test.

## Verdicts
Each check has a **basis**: `MUST` (a normative spec rule), `SHOULD` (a spec recommendation), or `POLICY` (hygiene the spec
doesn't mandate, such as exiting on end of input). Each result is one of:
`pass`, `fail`, `skip` (not applicable, e.g. capability not advertised), `not-exercised` (applicable but the run produced
no evidence, e.g. no non-ASCII output was ever seen), or `error` (the checker or environment failed; never a pass).
Lessons from virgil-lsp: an unexercised check must never render as a pass, and a checker failure must keep the first
useful evidence rather than be replaced by a cleanup error.

## Check catalogue (v0.1)
Each session starts a fresh server process unless noted, so one failure can't contaminate the next. Spec references
are to LSP 3.17 and JSON-RPC 2.0.

### Framing and output
| ID | Basis | Check |
|---|---|---|
| F1 | MUST | Every byte the server writes to stdout, from spawn to exit, belongs to a well-formed frame (startup banners, logs and stray newlines fail). |
| F2 | MUST | Each frame's `Content-Length` equals the body's length in **bytes**. `not-exercised` if no frame carried non-ASCII text. |
| F3 | MUST | Input split across writes is read: a request whose header is written one byte per write and whose body is cut at seeded points, then (when F5 passed) one cut inside 2-, 3- and 4-byte characters. The writes are 10 ms apart, so a server reading promptly sees them separately. |
| F4 | MUST | Several frames in one write are all read (two requests with a `$/setTrace` notification between them). |
| F5 | MUST | A `didOpen` whose text has 2-, 3- and 4-byte UTF-8 characters keeps the stream in sync: a following request, with such characters too, is answered. The `didOpen` is sent only when the server asks for open and close notifications (`textDocumentSync` a kind other than None, or `openClose: true`). |
| F6 | MUST | A `Content-Type: application/vscode-jsonrpc; charset=utf-8` header is accepted (it's an optional header the client may send). |
| F7 | SHOULD | The legacy `charset=utf8` spelling is treated as UTF-8. |
| F8 | SHOULD | A `Content-Type` the server sends declares UTF-8 (or the legacy `utf8`), the only supported charset. |

F3 to F7 share one scenario. After `initialize`, a plain request for an unknown method (which must be answered, see J4)
shows that the server answers such probes. Each variant then sends probes written its own way, and fails if one goes
unanswered. After a variant that didn't pass, a plain probe checks the server is still in step; if it isn't, the
remaining variants are `error`, since their failure would say nothing new. If the first plain probe goes unanswered,
all five are `not-exercised` (or `error`, when lspcheck's own evidence was incomplete). `initialize` itself is sent
whole, because a missing reply to it can't be told apart from a slow start.

### JSON-RPC correlation
| ID | Basis | Check |
|---|---|---|
| J1 | MUST | A reply's `id` is the same value as the request's: a number stays a number with the same exact value (`7` and `7.0` are the same; no rounding through floating point), and `"7"` stays a string. |
| J2 | MUST | Exactly one response per request within its deadline; no duplicates, no responses to ids never sent. |
| J3 | MUST | No response to notifications, including unknown ones and unknown `$/` ones. |
| J4 | MUST | An unknown request method gets error `-32601` MethodNotFound; an unknown `$/` request too. |
| J5 | MUST | Envelopes: `"jsonrpc": "2.0"`, exactly one of `result`/`error`, error `code` an integer and `message` a string. |
| J6 | MUST | Concurrent requests (sent before any reply) are each answered with the right id; order may differ. |
| J7 | MUST | Server-to-client requests have well-formed unique ids; the checker answers them (configurable) and checks the server tolerates the answers. |

### Lifecycle
| ID | Basis | Check |
|---|---|---|
| L1 | SHOULD | A request before `initialize` gets error `-32002`; a notification before it is dropped without a reply (the notification half isn't tested yet). |
| L2 | MUST | `initialize` returns an object with `capabilities`; a chosen `positionEncoding` is one the client offered (offer `utf-8` first, `utf-16` last). |
| L3 | POLICY | A second `initialize` gets an error and the server keeps serving. |
| L4 | MUST | `shutdown` returns `null` and the process keeps running until `exit`. |
| L5 | SHOULD | Requests after `shutdown` get error `-32600` InvalidRequest. |
| L6 | SHOULD | `exit` after `shutdown` ends the process with status 0 within the deadline. |
| L7 | SHOULD | `exit` without `shutdown` ends it with status 1. |
| L8 | POLICY | End of stdin without `exit` ends the process within the deadline (prevents orphaned servers). |
| L9 | POLICY | After exit no descendant remains. Profiles can allow named long-lived daemons. Where lspcheck can't see an escape through `setsid`/double fork (macOS), a pass says so in its detail. |

### Cancellation
| ID | Basis | Check |
|---|---|---|
| C1 | MUST | `$/cancelRequest` for a pending request still yields exactly one response: a result, or error `-32800` RequestCancelled (either wins the race lawfully). |
| C2 | MUST | Cancelling an unknown id or an already-answered id produces nothing and the server keeps serving. |
| C3 | MUST | No response to `$/cancelRequest` itself. |

### Documents and positions (richer with a profile document)
| ID | Basis | Check |
|---|---|---|
| D1 | MUST | Every `Range`/`Position` in any reply or notification that refers to an open document lies within that document's current text in the negotiated encoding (line exists; character ≤ line length in UTF-16/UTF-8/UTF-32 units). Text with emoji and combining marks makes UTF-8/UTF-16 confusion visible. |
| D2 | MUST | `publishDiagnostics` for an open document carries, if any, its latest version, and in-bounds ranges. |
| D3 | MUST | Edits follow the advertised sync kind (full or incremental); after an incremental edit across a 4-byte character, D1 still holds. |
| D4 | POLICY | Requests on a closed or never-opened document get an error or empty result, and the server keeps serving. |

### Capabilities and robustness
| ID | Basis | Check |
|---|---|---|
| K1 | MUST | Every advertised request capability with a position or document parameter answers (not MethodNotFound) at profile positions or generic ones. |
| K2 | MUST | Replies validate against the LSP 3.17 metaModel (vendored, CC BY 4.0 attribution). **v0.2.** |
| R1 | POLICY | A well-framed body that isn't JSON gets ParseError `-32700` with id `null` or is ignored, and the server keeps serving. |
| R2 | POLICY | Wrong param types get an error (InvalidParams preferred), not a crash. |
| R3 | POLICY | A large document (default 4 MiB, configurable) is accepted or refused with an error; the server keeps serving. |
| R4 | POLICY | Unusual but valid ids (0, negative, 2^53, long and non-ASCII strings) round-trip. |

## Profiles (optional TOML)
```toml
[server]
command = ["gopls"]          # or given after `--`
startup_timeout = "30s"
request_timeout = "10s"

[workspace]                 # copied into a fresh temp dir per session; rootUri and workspaceFolders point there
path = "fixtures/go"        # or inline:
files = { "go.mod" = "module example\n" }

[document]
path = "main.go"
language_id = "go"
positions = ["main.go:3:6"] # where K1 probes; otherwise identifier-looking tokens are used

[client]
initialization_options = {}
configuration = {}          # answers to workspace/configuration
capabilities = "default"    # or a path to a capabilities JSON

[allow]
descendants = []            # process names allowed to outlive exit
skip = []                   # check ids, with a reason each
```
Without a profile: an empty temp workspace and a `plaintext` document. Generic checks still run; position checks report
`not-exercised` where the server produced no positions.

## Report
- Terminal summary grouped by area, with counts and a line per failure naming the transcript.
- `--json`: versioned `report-v1` schema (checks, basis, verdict, evidence, timings, server argv, checker version,
  host OS/arch, profile hash). `--junit` for CI test tabs; `--markdown` for a GitHub step summary.
- `--transcripts DIR`: one JSONL per session with direction, monotonic timestamp, raw bytes (base64 when not UTF-8),
  how the reader split them into frames, and stderr. `replay` re-sends the client side and waits for the recorded replies.

## Architecture
Single crate, `src/` modules:
- `transport`: incremental frame reader over raw bytes (records everything, bounded frame size, typed framing
  errors), frame writer able to split or coalesce on purpose.
- `process`: spawn with a dedicated process group (Unix) or Job object (Windows). A small sentinel process joins the
  group first so the PGID can't be recycled while we scan for descendants (the reuse bug virgil-lsp hit). Kill escalation:
  SIGTERM, grace, SIGKILL. Descendant scan: Linux `/proc`, macOS `proc_listpids`, Windows job process list.
- `session`: tokio tasks for stdout (parsed there, so framing evidence is complete even if replies aren't consumed)
  and stderr (always drained); stdin writes bounded by a deadline. Request ids are random 31-bit integers, so a server
  can't prepare a reply before it sees the request (output already read when a request is sent can't answer it, but
  bytes still in the pipe can, so the randomness is what matters); ids match by **JSON value** (`1` and `1.0` are the
  same number). Each call has one absolute deadline covering its write and its reply; each scenario has its own
  timeouts and a bounded cleanup. After a write stops partway through a frame, lspcheck sends nothing more.
- `checks`: one module per area; each check declares id, basis, spec link, and a function over a `Session`.
- `profile`, `report`, `cli` (clap).
Dependencies: tokio, serde, serde_json (`raw_value`, `preserve_order`), clap, toml, thiserror; `nix` on Unix,
`windows-sys` on Windows. No network access at runtime.

## How we know it works
- `standin`: a test-only server binary (in the workspace) with fault flags, one per check:
  `--fault stdout-banner`, `char-length`, `id-to-int`, `reply-twice`, `reply-to-notification`, `no-method-not-found`,
  `exit-on-shutdown`, `exit-code-0-without-shutdown`, `ignore-eof`, `orphan-child`, `utf8-positions-as-utf16`, …
  **Every check has a test that fails it on its fault and passes the clean stand-in.** That guards against false greens.
- Self-tests for the harness: partial frames, stderr floods, a server that stops reading stdin, a hung server, signals
  during spawn, cleanup after a crash.
- Real servers in CI (pinned versions; results recorded as expected-results files, not required to all pass):
  lua-language-server, gopls, basedpyright, rust-analyzer, and the virgil-lsp release archive.
- Platforms: Linux and macOS required in CI; Windows built and tested, descendant checks there best effort at first.

## Milestones
- **M0** repository, CI (fmt, clippy, test on Linux/macOS/Windows), transport + process + session, stand-in skeleton.
- **M1** framing and lifecycle checks (F*, L*), terminal + JSON report.
- **M2** correlation, cancellation, documents (J*, C*, D*), transcripts and `replay`.
- **M3** profiles, K1, R*, JUnit/Markdown; stock-server CI matrix; v0.1.0 release with prebuilt binaries
  (Linux x86_64/aarch64 static, macOS arm64/x86_64, Windows x86_64) and checksums. crates.io publishing needs the
  maintainer's token.
- **M4** dogfood in virgil-lsp: a pinned binary download in CI (a dependency + workflow change, so a human merge there).
- Later: K2 metaModel validation; language profiles with seeded source mutation and recovery oracles, ported from
  `test/protocol/source_mutations.py`.

## Open questions
- Whether findings in other projects' servers from the CI matrix are reported to them (outward-facing; ask first).
- MSRV policy: pin the toolchain in `rust-toolchain.toml` and support the last few stable releases.
