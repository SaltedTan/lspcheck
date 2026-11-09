# Changelog

## Unreleased

### Added

- Example profiles for clangd, typescript-language-server, bash-language-server, yaml-language-server,
  vscode-json-language-server, marksman, ruff, texlab and zls in [`examples/profiles/`](examples/profiles/), each with
  a tiny project and a `[document]`.

### Changed

- D6's failure detail says when a containment failure would hold under a line split the spec doesn't allow, naming
  the split as D5 does; the verdict is unchanged.

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
