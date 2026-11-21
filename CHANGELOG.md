# Changelog

## Unreleased

### Added

- Profiles accept `[client.capabilities]`, a table merged over lspcheck's client capabilities in every `initialize`
  it sends. The profile wins where both set a key (objects merge key by key; arrays and scalars replace lspcheck's),
  and lspcheck's own capabilities stay where the profile is silent. Two values are refused because lspcheck couldn't
  judge under them: `textDocument.synchronization.dynamicRegistration = true`, and a `general.positionEncodings`
  entry other than `utf-8`, `utf-16` or `utf-32`. A set that isn't of the LSP 3.17 `ClientCapabilities` type once
  merged is refused, naming the key and the scenarios affected. A key LSP 3.17 doesn't define is sent anyway, with a
  warning on stderr. L2 judges the server's `positionEncoding` against the offer actually sent, and names a profile's
  offer in its detail. When the capabilities set `workspace.workspaceFolders = false`, lspcheck sends no
  `workspaceFolders` param; `rootUri` still names the workspace. The placeholder `capabilities = "default"` is refused
  with a message showing the table form.

### Fixed

- **L2** now judges the `positionEncoding` of every scenario's first initialize, each against the offer sent with
  it, not only the lifecycle scenario's. A server that chose an encoding it wasn't offered in another scenario passed
  L2, while D1 and the other position checks there said "not judged, see L2". The failure names the scenarios and the
  offer, and a violation seen fails L2 even when the lifecycle scenario couldn't finish initializing. A first
  initialize answered only after `--startup-timeout` ran out is judged from the result lspcheck read later. When no
  violation was seen but a scenario's first initialize got no result lspcheck saw while it missed output (such as a
  frame over `--max-frame-mib`), L2 is `ERROR`, naming the scenario, instead of passing. The report links the
  transcript of the scenario L2's verdict came from. `--only L2` now runs every scenario.
- **K1** no longer fails on a MethodNotFound for a request the server may have withdrawn with
  `client/unregisterCapability`. Its answer isn't judged when, before it, lspcheck had read any
  `client/registerCapability` or `client/unregisterCapability` for the request's registration method
  (`textDocument/semanticTokens` for the semantic tokens requests, `textDocument/rename` for `prepareRename`),
  whatever its id or document selector, or when the client capabilities let the server register the method
  dynamically and lspcheck missed output between the initialize answer and the MethodNotFound. lspcheck doesn't track
  which registrations are in force, so a server that sends a stray registration and then wrongly answers
  MethodNotFound isn't failed. Such a request is still sent, and the detail names it and why. This applies in the
  capabilities and mutation scenarios, and K1 with nothing else to judge is `N/E`.
- **C1** and **R4** no longer pass when the server answers their request with a message carrying the id but no
  well-formed outcome, by J5's rules: exactly one of `result` and `error`, and the error an object with an integer
  `code` (signed 32-bit) and a string `message`. So neither, both, `error: null` and `error: {}` don't count. Such a message still
  fails J5. C1 and R4 now wait past it for a proper answer with that id until the request's deadline, and fail only if
  none comes and they read the whole wait; their detail then says such a message came. Only a response read once
  lspcheck registered the request (just before writing it) counts, so R4 no longer takes a reply that came before it. Other checks still take such a
  message as the request's reply, so one malformed reply doesn't stall every later wait. R4 no longer calls the answer
  it took after such a message "likely id mangling", but still lists any other response with that id.
- **C2** can now fail only on a response carrying an id never sent, or on a plain request after the cancels that gets
  no proper answer in time. It now cancels a plain request of its own that the server has answered properly, rather
  than C1's cancelled request. A response with that request's id after its cancel makes C2 `error`, whatever it holds,
  RequestCancelled (-32800) included, since it may be a late second answer (which J2 fails). If the plain request got
  only a message with its id, C2 doesn't cancel it, since it may still be pending, and is `error` unless cancelling the
  unknown id fails it. C2 no longer passes after cancelling only an id never sent when C1's request went unanswered:
  it now tests both cases. A second response to C1's request now fails **C1** whenever it arrives before `exit`; it
  used to fail C2 instead when it came after C2's cancels. C1 now watches for a second response until `exit`, so
  output lspcheck missed anywhere in that span makes a C1 pass `error`.
- **C1** and **C2** no longer fail when an answer they waited for arrives after `--timeout` but before `exit`.
  `--timeout` is lspcheck's limit, not the protocol's: C1 judges such an answer as usual, and a plain request after
  C2's cancels answered that late makes C2 `error` (and C3 is then judged as usual). Output lspcheck missed before
  `exit`, such as a frame over `--max-frame-mib`, could have held such an answer, so it now makes a missing answer
  `error` rather than `fail`.
- **C3** now fails only on a MethodNotFound (-32601) response without an id whose error `message` names
  `$/cancelRequest` as a whole word, not as part of a longer name such as `$/cancelRequestMetrics`, read after
  lspcheck began writing the first cancel, its code read by value as J5 does (so `-32601.0` counts). Any other response without an id now gives `error` rather than `fail`,
  including one with another code that names `$/cancelRequest` (such as `initialized` failing) or that mentions
  cancelRequest only in its `data`. One read before the cancel was written no longer counts for C3 at all (J3 still
  judges it).
- **L6** and **L7** no longer fail a server that may have been waiting on lspcheck when the exit deadline passed. A
  server still running now fails only if lspcheck saw it running a whole `--exit-timeout` after it last had something
  of the server's to handle (a message to take in, or a request to answer), if that was by the deadline. Once the
  session has ended, the failure stands only if lspcheck read nothing more of the server's stdout after that, every
  request the server sent had been answered in full by then, and lspcheck lost no frame and read no message that isn't
  JSON-RPC. So a request still queued at the deadline, or still being decoded behind a large frame, output written
  after it, and a request lspcheck couldn't answer now make the check `error`, saying which. A server that asks
  something after `exit` and then hangs still fails, after up to twice `--exit-timeout`. lspcheck also no longer fails
  a server that exited in time while lspcheck was still writing an answer to one of its requests, which a process the
  server left holding its stdin can keep going until the deadline: it now watches for the exit while it writes. An
  exit lspcheck saw only after the deadline (while it prepared a large answer, say) is `error`, with both times.
- **L9** no longer fails on a process that had exited before the 1 s grace after the server's exit ended, but that a
  search begun before the end of the grace and finished after it (a slow `/proc` read or `ps`) had seen. Only a search
  begun once the grace is over now counts a process as left behind. A search that fails earlier is tried again; if the
  one begun after the grace fails, L9 is `error`.
- **L9** now fails for a descendant that left the server's process group (with `setsid`, say) while its parent in the
  group is still running. lspcheck missed it until its parent exited, so L9 passed when the parent's name was in
  `allow.descendants`. lspcheck now finds every process descended from the group's members, and checks each one
  against the list. A pid read as a parent may pass to an unrelated process while lspcheck lists processes, so on
  Linux a process found through its parent counts only once a second listing shows the same parent, by pid and start
  time, still named by the same child; one lspcheck can't confirm makes L9 `error` rather than fail it. On macOS,
  where `ps` gives no start time precise enough, a process found through its parent always does.
- On macOS, the `ps` lspcheck runs to find leftover processes now writes to a temporary file instead of a pipe. A
  process left holding its output can no longer keep a thread reading it after the search has given up, and no longer
  makes L9 `error` with "ps's output didn't end". lspcheck reads at most 16 MiB of it; more, or a read cut short,
  makes the search fail rather than judge from part of the list. Each search for leftover processes, and each `ps`
  run within it, now ends by a deadline.
- **L4** no longer fails a server that answers `shutdown` with an error, which LSP allows when an exception happens
  during shutdown. L4 now passes if the server keeps running after it, and the detail names the error. The error must
  be well formed by J5's rules (an integer code in the signed 32-bit range and a string message), and not
  MethodNotFound (-32601), since every server must implement `shutdown`. A result other than `null`, any other error,
  or exiting before `exit` still fails L4. L4 now passes only once lspcheck has seen the server still running just
  before it writes `exit`, so a server that exits in between fails L4 too. Its title, which JUnit reports use as the
  test name, now reads "shutdown returns null (or an error) and the server keeps running until exit".
- **L1** no longer passes when output lspcheck didn't read, such as a frame over `--max-frame-mib`, arrived between
  the notification it sends before `initialize` and the answer to `initialize`: that output could have been a reply
  to the notification. L1 is then `error` and says why. Output that comes once lspcheck has read the answer, such as a
  large log message on `initialized`, doesn't count. A reply lspcheck read still fails L1.
- **L7** no longer judges an exit status the server reached before lspcheck sent `exit`, such as a server that exits
  while a child it left behind keeps the conversation going. L7 is then not exercised, and says the server exited
  before `exit` was sent, as L6 already does.
- **L2** is judged from the `initialize` result even when lspcheck can't send `initialized` (or the profile's
  `workspace/didChangeConfiguration`) afterwards, for example because the server closed its input. L2 used to be
  `error` then; it now passes or fails on the result. J4, L4 and L6, which need the server initialized, are still
  `error`, and now say what couldn't be sent.
- **R1** now counts a reply to the non-JSON body that arrives after the plain request's answer, up to when lspcheck
  sends `exit`. A late response with id null and another error code than ParseError (-32700), or a result, used to be
  ignored and R1 passed saying the body was ignored. Read before lspcheck begins writing `shutdown` (once it has
  answered what the server asked meanwhile), it now fails R1; read after, it may be the server's error about
  `shutdown`, so R1 is `error`. A late ParseError still passes, and the
  detail now counts it instead of saying the body was ignored. The plain request must still be answered within
  `--timeout`.
- **R3** now fails, instead of being `error`, when the server leaves the plain request after the large document
  unanswered for its whole wait, which lspcheck read in full, and then sends output lspcheck can't read while it waits
  to see whether the server exits. That output came too late to hold the answer. A server that exits was already
  judged this way.

## 0.6.0 (2026-11-19)

This release fixes verdicts that could be wrong when lspcheck didn't see all of the server's output, and makes two
things easier to spot under `--mutate`. Some verdicts change from 0.5.0, each to what lspcheck actually observed:
F1 can go from `fail` to `error`, K1 from `pass` to `error`, and J3, K1 or M3 from `error` to `pass`. The entries
below say when.

### Added

- Under `--mutate`, lspcheck warns on stderr when `mutation.run_timeout` leaves less than a minute beyond one case's
  budget in which cases can start. A slow machine could then run fewer cases than asked while every verdict passes.
  The run, its report and its exit status are unchanged.
- Under `--mutate`, the report counts the Error and Warning messages the server sends with `window/showMessage`,
  `window/logMessage` or `window/showMessageRequest` from the first mutation until shutdown, so a contained crash the
  server reports only as a message is visible. The JSON report's `mutation.messages` has the counts and the first
  three messages, each with its case and method and cut to 200 characters; the terminal and Markdown reports add a
  line quoting them when there is at least one. They change no verdict.

### Fixed

- **F1** no longer fails on a frame lspcheck may have cut short itself. When lspcheck stops a server (or a process it
  left behind) that is still writing a frame, such as a slow `shutdown` reply lspcheck gave up waiting for, F1 is now
  `error` and says how much of the frame arrived, whether the stream ended in the frame's body or its header section.
  What counts is when lspcheck saw the stream end. A server that ends its output inside a frame by itself still fails
  F1, if lspcheck saw the end before stopping it; so does a header section already malformed when the stream ended.
- **K1** no longer passes when output lspcheck didn't read after K1's first request could have held a MethodNotFound
  for one of its requests: a frame over `--max-frame-mib` (even a duplicate after a result), reading that stopped, or
  stdout held open by a process lspcheck couldn't stop. K1 is then `error` and says why. A MethodNotFound lspcheck
  read still fails K1, and output missed before K1's requests doesn't affect it. Under `--mutate`, the same applies to
  K1's requests about the mutated documents.
- When the server's stdout stays open after lspcheck has stopped its processes, what lspcheck didn't read after
  giving up on it now counts as missed, so the checks that ask whether output was missed (K1 among them) are `error`
  instead of passing.
- A frame whose header the server wrote but whose body it never began, because it ended its output first, still
  fails F1 but no longer makes K1, J3, M3 or the other checks that ask whether output was missed `error`: it can't
  have held a message. gopls sometimes exits this way after `exit`.
- Under `--mutate`, when some of M2's unanswered requests may have been answered in output lspcheck didn't see, M2's
  failure detail now names and counts only the requests whose waits it saw in full, and says how many others may have
  been answered, and why. Likewise R4's failure detail lists as unanswered only the ids whose waits it saw in full.
  Neither verdict changes.

## 0.5.0 (2026-11-16)

### Added

- Experimental [seeded source mutation](https://github.com/SaltedTan/lspcheck/blob/v0.5.0/docs/source-mutations.md). With a profile and `--mutate`, lspcheck
  opens the profile's real source (its `[mutation].files`, else its `[document]`), breaks it with seeded, token-aware
  edits, asks the server about each broken text, restores the clean text after each case and checks that the server
  recovers. Three new `POLICY` checks judge it:
  - **M1**: the server doesn't exit, or close its output, while its source is mutated or being restored.
  - **M2**: every request about a mutated document gets a response (any result or error) within `--timeout`.
  - **M3**: once the clean text is restored, the [recovery probes'](https://github.com/SaltedTan/lspcheck/blob/v0.5.0/docs/source-mutations.md#recovery) answers
    return to the stable baselines taken before the first mutation. `[mutation.recovery]` can also require them to
    equal the baseline (`expect = "baseline"`) or the restored text to have no Error diagnostic (`no_errors`).

  Under `--mutate`, D1, D2, D6 and K1 also judge what the server says about the mutated texts
  ([extended-check rules](https://github.com/SaltedTan/lspcheck/blob/v0.5.0/docs/source-mutations.md#verdict-rules)); each check's verdict combines with its own
  scenario's, and the detail names the scenario and case.
- `--mutate`, `--mutation-seed N` and `--mutation-cases N` run the scenario; the two flags override the profile. A
  seed plans the same edits every time, and a seed chosen at random is reported, so a run can be repeated. The first
  three failing or erroring cases are [run again alone](https://github.com/SaltedTan/lspcheck/blob/v0.5.0/docs/source-mutations.md#step-6), failing ones first, each
  with a fresh server, and the report says whether each reproduced and what in. An isolated run judges nothing, so no
  verdict depends on it.
- Profiles accept a [`[mutation]` section](https://github.com/SaltedTan/lspcheck/blob/v0.5.0/docs/source-mutations.md#profile-schema): seed files, seed, case
  count, operators, a lexicon for the language, the requests to ask, recovery probes and their expectations, and
  timeouts (`run_timeout` defaults to an hour). It is validated with the rest of the profile, a mistake in it being a
  usage error, and is inert without `--mutate`. Under `--mutate`, a case budget longer than `run_timeout` is a usage
  error too, so a run never silently admits no case.
- Reports: the JSON report gains `"mutation"` (`null` when the scenario doesn't run), with the seed, the cases planned
  and run, `stopped` (why the run stopped early, if it did) and `isolated`; the terminal header names the seed, and the
  terminal and Markdown reports give a line per isolated case. `--transcripts` writes `mutation.jsonl`, which
  `replay` replays as it is, with a meta line recording each case's plan, and `mutation-case-<k>.jsonl` for each
  isolated case.
- The 11 [example profiles](https://github.com/SaltedTan/lspcheck/blob/v0.5.0/examples/profiles/README.md) the stock-server matrix runs gain a `[mutation]` section
  with a lexicon for their language, and `expect = "baseline"` where the recovery probes answer from the text alone. A
  test checks that each validates and fits its `run_timeout`. Their behaviour on the pinned releases is recorded in
  [docs/mutation-stock-servers.md](https://github.com/SaltedTan/lspcheck/blob/v0.5.0/docs/mutation-stock-servers.md).
- The [stock-server matrix](https://github.com/SaltedTan/lspcheck/blob/v0.5.0/ci/stock-servers/README.md) runs a mutation job per
  server, `--mutate --mutation-seed 20261115 --mutation-cases 10`, compared against its `<server>.mutation.verdicts`.

### Changed

- Without `--mutate`, M1 to M3 are `SKIP` ("mutation is off; pass --mutate"), which doesn't affect the exit status.
  Reports gain three `SKIP` lines, and each stock-server `.verdicts` file three `skip` lines; no other verdict of a
  default run changes.
- Profile durations may also be given in hours, such as `"1h"`.

### Fixed

- D6 in the documents scenario judges a `selectionRange` reply against every text it may describe (D1's
  [candidate rule](https://github.com/SaltedTan/lspcheck/blob/v0.5.0/docs/design.md#documents-and-positions)), so a lawful late reply about the changed, shorter text
  no longer fails.

## 0.4.0 (2026-11-11)

### Added

- [Example profiles](https://github.com/SaltedTan/lspcheck/blob/v0.4.0/examples/profiles/README.md) for 14 more servers: clangd, typescript-language-server,
  bash-language-server, yaml-language-server, vscode-json-language-server, marksman, ruff, texlab, zls,
  jedi-language-server, pylsp, terraform-ls, lemminx and taplo, each with a tiny project and a `[document]`.
- The [stock-server CI matrix](https://github.com/SaltedTan/lspcheck/blob/v0.4.0/ci/stock-servers/README.md) also runs pinned releases of clangd, marksman,
  ruff, texlab, zls, terraform-ls and lemminx (11 servers in all), whose verdicts were each the same in repeated runs.

### Changed

- K2 accepts unknown values of the tag and kind enumerations the specification tells clients to tolerate
  (`SymbolKind`, `SymbolTag`, `CompletionItemKind`, `CompletionItemTag` and `DiagnosticTag`), so clangd's extension
  symbol tags (8 and up) no longer fail K2. A `null`, a string, or a negative or fractional number there still fails,
  and every other closed enumeration stays strict ([K2 rules](https://github.com/SaltedTan/lspcheck/blob/v0.4.0/docs/design.md#capabilities-and-robustness)).
- F6 and F7 (a request with a `Content-Type` header) run in a scenario of their own, `content-type`, with a fresh
  server, and `--transcripts` writes `content-type.jsonl` for it. A server that exits on that optional header
  (terraform-ls 0.39.1 exits on F7's legacy `charset=utf8`) no longer leaves F3 and F5 `error` and the run's exit
  status 2, and one thrown out of step by F3 or F5 no longer leaves F6 and F7 `error`.
- Failure details say more, with no change to any verdict: D6's containment failures include
  [line-split hints](https://github.com/SaltedTan/lspcheck/blob/v0.4.0/docs/design.md#documents-and-positions), and when no alternative of a K2 union matches,
  the detail names the closest alternative in more cases
  ([diagnostic rules](https://github.com/SaltedTan/lspcheck/blob/v0.4.0/docs/design.md#capabilities-and-robustness)).

## 0.3.0 (2026-11-09)

### Added

- K2 (`MUST`) validates what the server sends against the LSP 3.17 meta-model, across every scenario: the results of
  lspcheck's requests (except `shutdown`, which L4 judges) and the params of the server's own requests and
  notifications. Extra properties are lawful, and a whole number written as `41.0` counts as an integer. Running out of
  lspcheck's validation budget never fails K2 by itself: a message too large to finish is counted as not judged,
  unless a violation was already found in it ([reference](https://github.com/SaltedTan/lspcheck/blob/v0.3.0/docs/design.md#capabilities-and-robustness)).
  The meta-model is [vendored unmodified under CC BY 4.0](https://github.com/SaltedTan/lspcheck/blob/v0.3.0/vendor/lsp-3.17/README.md).
- A [stock-server CI matrix](https://github.com/SaltedTan/lspcheck/blob/v0.3.0/ci/stock-servers/README.md) runs
  lspcheck against pinned releases of gopls, rust-analyzer, lua-language-server and basedpyright and compares every
  verdict with the expected one.

## 0.2.0 (2026-11-09)

### Added

- Profiles: `lspcheck --profile PATH` reads a TOML file that gives a server what it needs and says what its
  maintainers accept ([README](https://github.com/SaltedTan/lspcheck/blob/v0.2.0/README.md#profiles), [reference](https://github.com/SaltedTan/lspcheck/blob/v0.2.0/docs/design.md#profiles-optional-toml)).
  - `[server]` sets the command (used when none follows `--`) and the timeouts; a flag on the command line still wins.
  - `[workspace]` copies a fixture directory and writes inline files into each scenario's workspace.
  - `[client]` sets `initializationOptions` and the settings that answer `workspace/configuration` (also sent in
    `workspace/didChangeConfiguration` after `initialized`).
  - `[document]` names the file K1 opens, its `language_id`, and up to five `LINE:COLUMN` positions.
  - `[allow]` lets named descendants outlive the server (L9) and skips checks with a reason.
  - A bad profile is a usage error (exit status 2) naming the key, before any server starts. Transcripts record the
    profile's files and settings, so `replay` recreates them.
- [Example profiles](https://github.com/SaltedTan/lspcheck/tree/v0.2.0/examples/profiles/) for gopls, rust-analyzer, lua-language-server and basedpyright.
- K1 (`MUST`) sends each request the server advertises about an open document and fails if one is answered with
  MethodNotFound (-32601). A request still unanswered at the end of the session makes K1 inconclusive.
- Robustness checks (`POLICY`), each in its own scenario:
  - R1 sends a well-framed body that isn't JSON and expects a null-id ParseError, or nothing, and continued service.
  - R2 sends wrongly typed parameters to an advertised request and expects an error response and continued service.
  - R3 opens a large document (`--large-document-mib`, 4 MiB by default) and expects continued service.
  - R4 checks that unusual but valid request ids (0, negative, the 32-bit extremes, empty, long and non-ASCII
    strings) come back exactly.
- `--junit PATH` and `--markdown PATH` write CI reports alongside JSON: a JUnit XML report for CI test tabs and a
  summary table for a CI step summary ([usage](https://github.com/SaltedTan/lspcheck/blob/v0.2.0/README.md#use), [format](https://github.com/SaltedTan/lspcheck/blob/v0.2.0/docs/design.md#report)).

### Changed

- The JSON report has a top-level `"profile"` field (`null` without a profile); the schema id is unchanged.
- With `--transcripts`, the terminal report also points to the transcripts directory for failures and errors of
  cross-cutting checks.
- Dependency: `toml` 1.2 (features `parse` and `serde`), to read profiles.

### Fixed

- Transcripts keep each request before its reply, even when the reply is read while the request's write is still
  flushing, so `replay` no longer reports a recorded reply as missing (seen on Windows).

## 0.1.0 (2026-11-08)

The first release: 33 checks of framing, JSON-RPC correlation, the lifecycle, cancellation and document positions,
for any language server that talks over stdio. Prebuilt binaries are attached for Linux (x86_64 and aarch64, static),
macOS (x86_64 and arm64) and Windows (x86_64), with SHA-256 checksums and build provenance.

- Checks:
  - F1, stdout framing, F2, Content-Length counted in bytes, and F8, the declared charset;
  - F3 to F7, how the server reads its input: split writes, coalesced frames, multibyte text, and the client's
    `Content-Type` header in both charset spellings;
  - J4, MethodNotFound for unknown requests;
  - J1 to J3, J5 and J6, JSON-RPC correlation: ids preserved, one response per request, none to a notification,
    well-formed envelopes, and concurrent requests with numeric and string ids;
  - J7, server-to-client request id reuse and continued service after lspcheck's answers
    ([judging rules](docs/design.md#json-rpc-correlation)). The stand-in now asks for a progress token after
    `initialized`, as many servers do;
  - L1 to L8, the initialize, shutdown and exit lifecycle, including a second initialize, requests after shutdown,
    and the end of input without exit;
  - L9, leftover processes;
  - C1 to C3, cancellation: a cancelled request gets exactly one response, cancelling an unknown or finished request
    produces nothing, and `$/cancelRequest` itself is never answered;
  - D1, positions: every position the server sends about an open document falls on a character boundary in the
    negotiated encoding, never inside a UTF-16 surrogate pair or a UTF-8 multi-byte sequence. A new `documents`
    scenario opens a document whose emoji make confused encodings land inside characters, asks the server's
    advertised document requests, and reads its diagnostics;
  - D2, diagnostics versions: a `version` in `publishDiagnostics` for an open document is one lspcheck sent for it
    before the notification was read; an older version is lawful, and so is none at all;
  - D3, positions after a change: the `documents` scenario now sends a `didChange` the way the server's
    `textDocumentSync` kind asks (an incremental edit that replaces a 4-byte character and adds a line, or the full
    text), then asks the document requests again and reads the new diagnostics. A position that falls inside a
    character of the changed text fails D3 when no other counting of the changed text explains it and a text the
    change should have produced does; otherwise D3 is an error and D1 carries the failure. A server that
    takes no changes leaves D3 skipped.
  - D4, documents that aren't open: the `documents` scenario now closes document A with a `didClose`, then sends one
    advertised document request about the closed document, the same request about a file that was never opened and
    doesn't exist, and a plain request, each awaited alone. Each must get a response — any result or error is
    lawful, including one read from disk — and the server must keep serving. A server that advertises none of the
    document requests leaves D4 skipped; D4 still runs when the server doesn't ask for `didOpen`.
  - D5, line endings: the `documents` scenario now opens a second document, B, whose lines end in `\r`, `\r\n` and
    `\n`, and sends the advertised document requests about it. A position about B that falls inside a character of
    B in the negotiated encoding, on no boundary of B counted any other way, and on a boundary of B under a line
    split the spec doesn't allow (LF only, CRLF as two breaks, or no lone CR) fails D5. A position that fits the
    spec's split in some other encoding is a counting fault (D1's), not a line-splitting fault, so D5 is an error
    and D1 carries the failure. UTF-32 leaves D5 not exercised, because no offset can fall inside a character.
  - D6, selection ranges: the `documents` scenario now judges the `selectionRange` replies to the requests it sends
    about the open documents. Each result must have one entry per position, each entry's `range` must contain its
    position, and each `parent.range` must contain its child's range, all the way up the chain, after clamping each
    character to its line's length in the negotiated encoding. A range on a line the text doesn't have isn't judged,
    and a malformed entry is a type error for a later check. A server that advertises no `selectionRangeProvider`
    leaves D6 skipped; one that answers with only `null`, errors, or no reply leaves it not exercised.
- The text model behind the document checks (`src/text.rs`): the spec's line split, the UTF-8/UTF-16/UTF-32
  position encodings, the character-boundary test, applying content changes, and the table of the positions a reply
  refers to.
- Terminal, JSON (`lspcheck-report-v1`) and JSON Lines transcript output. Each transcript opens with a meta line
  naming the workspace and the files it held.
- `lspcheck replay`: re-runs one recorded session against a server, recreating its workspace and re-sending every
  client write in order. It reports each request's outcome against the recorded one, the exit status recorded versus
  now, and how many server-to-client requests were answered; it judges no check. Replay preserves stdin-close events,
  only skips complete server responses, and refuses to overwrite output files or workspace contents it does not own.
- Inspection limits (`--max-output-mib`, `--max-frame-mib`) that make results inconclusive rather than failed, when the
  missing output could explain them. Requests lspcheck couldn't deliver give errors, not failures. What lspcheck
  missed is timed as a span, and excuses a missing reply only when it overlaps the time lspcheck spent waiting for it;
  a frame complete before the request was sent excuses nothing. A `PASS` or `NOT EXERCISED` reached while a frame was
  still arriving is checked again once the scenario's output has been read to the end, and becomes `ERROR` if that
  frame turned out to be cut short inside the window the check relied on.
- `lspcheck-standin`, a test server with switchable faults.
- A release workflow: a `vX.Y.Z` tag publishes `lspcheck` for Linux (x86_64 and aarch64, static), macOS (x86_64 and
  arm64) and Windows (x86_64), each checked against the stand-in on its own platform, with SHA-256 checksums
  (`SHA256SUMS`) and build provenance that `gh attestation verify` can check.
- lspcheck answers the server's requests while it waits for the server to exit, since a server may handle `exit` only
  once a request it sent is answered (gopls does). L6 and L7 are errors, not failures, when output lspcheck missed
  before the deadline could have held such a request; see the [lifecycle judging rules](docs/design.md#lifecycle).
- Checked against gopls, rust-analyzer, lua-language-server and basedpyright before release: every failure lspcheck
  reports for them was confirmed in the server's own behaviour.
