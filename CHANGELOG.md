# Changelog

## Unreleased

- First checks:
  - F1, stdout framing, F2, Content-Length counted in bytes, and F8, the declared charset;
  - F3 to F7, how the server reads its input: split writes, coalesced frames, multibyte text, and the client's
    `Content-Type` header in both charset spellings;
  - J4, MethodNotFound for unknown requests;
  - J1 to J3, J5 and J6, JSON-RPC correlation: ids preserved, one response per request, none to a notification,
    well-formed envelopes, and concurrent requests with numeric and string ids;
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
    before the notification was read; an older version is lawful, and so is none at all.
- The text model behind the upcoming document checks (`src/text.rs`): the spec's line split, the UTF-8/UTF-16/UTF-32
  position encodings, the character-boundary test, applying content changes, and the table of the positions a reply
  refers to.
- Terminal, JSON (`lspcheck-report-v1`) and JSON Lines transcript output. Each transcript opens with a meta line
  naming the workspace and the files it held.
- `lspcheck replay`: re-runs one recorded session against a server, recreating its workspace and re-sending every
  client write in order. It reports each request's outcome against the recorded one, the exit status recorded versus
  now, and how many server-to-client requests were answered; it judges no check.
- Inspection limits (`--max-output-mib`, `--max-frame-mib`) that make results inconclusive rather than failed, when the
  missing output could explain them. Requests lspcheck couldn't deliver give errors, not failures.
- `lspcheck-standin`, a test server with switchable faults.
