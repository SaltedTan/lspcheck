# Changelog

## Unreleased

### Added

- Under `--mutate`, lspcheck warns on stderr when `mutation.run_timeout` leaves less than a minute beyond one case's
  budget in which cases can start, since a slow machine may then run fewer cases than asked with every verdict
  passing. The run, its report and its exit status are unchanged.
- Under `--mutate`, the report counts the Error and Warning messages the server sent with `window/showMessage`,
  `window/logMessage` or `window/showMessageRequest` during the cases, so a contained crash the server reports only
  as a message is visible. The JSON report's `mutation.messages` has the counts and the first three, with the case
  each arrived in, cut to 200 characters; the terminal and Markdown reports add a line quoting them when there is at
  least one. They change no verdict.
- K1 now also sends follow-up requests that need an item from an earlier answer, passing the answer's first item
  back unchanged: `callHierarchy/incomingCalls` and `outgoingCalls`, `typeHierarchy/supertypes` and `subtypes`, and,
  when their `resolveProvider` is `true`, `completionItem/resolve`, `codeLens/resolve`, `documentLink/resolve`,
  `inlayHint/resolve` and `workspaceSymbol/resolve`. Their answers are reported in K1's detail, and a MethodNotFound
  answer is noted with the item, but follow-ups don't change K1's verdict. A source answer with no usable item gets
  no follow-up, and the detail says so.

### Fixed

- Under `--mutate`, when some of M2's unanswered requests may have been answered in output lspcheck didn't see, its
  failure detail now names and counts only the requests whose waits it saw in full, and says how many others may have
  been answered, and why. The verdict is unchanged.
- Likewise, when the reply to some of R4's unanswered odd-id requests may have been in output lspcheck didn't see, its
  failure detail lists as unanswered only the ids whose waits it saw in full, and the others as possibly answered,
  with why. The verdict is unchanged.

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
