# Changelog

## Unreleased

- First checks:
  - F1, stdout framing, F2, Content-Length counted in bytes, and F8, the declared charset;
  - F3 to F7, how the server reads its input: split writes, coalesced frames, multibyte text, and the client's
    `Content-Type` header in both charset spellings;
  - J4, MethodNotFound for unknown requests;
  - L1 to L8, the initialize, shutdown and exit lifecycle, including a second initialize, requests after shutdown,
    and the end of input without exit;
  - L9, leftover processes;
  - C1 to C3, cancellation: a cancelled request gets exactly one response, cancelling an unknown or finished request
    produces nothing, and `$/cancelRequest` itself is never answered.
- The text model behind the upcoming document checks (`src/text.rs`): the spec's line split, the UTF-8/UTF-16/UTF-32
  position encodings, the character-boundary test, applying content changes, and the table of the positions a reply
  refers to.
- Terminal, JSON (`lspcheck-report-v1`) and JSON Lines transcript output.
- Inspection limits (`--max-output-mib`, `--max-frame-mib`) that make results inconclusive rather than failed, when the
  missing output could explain them. Requests lspcheck couldn't deliver give errors, not failures.
- `lspcheck-standin`, a test server with switchable faults.
