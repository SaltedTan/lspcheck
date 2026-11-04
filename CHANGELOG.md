# Changelog

## Unreleased

- First checks:
  - F1, stdout framing, F2, Content-Length counted in bytes, and F8, the declared charset;
  - F3 to F7, how the server reads its input: split writes, coalesced frames, multibyte text, and the client's
    `Content-Type` header in both charset spellings;
  - J4, MethodNotFound for unknown requests;
  - L1 to L8, the initialize, shutdown and exit lifecycle, including a second initialize, requests after shutdown,
    and the end of input without exit;
  - L9, leftover processes.
- Terminal, JSON (`lspcheck-report-v1`) and JSON Lines transcript output.
- Inspection limits (`--max-output-mib`, `--max-frame-mib`) that make results inconclusive rather than failed, when the
  missing output could explain them. Requests lspcheck couldn't deliver give errors, not failures.
- `lspcheck-standin`, a test server with switchable faults.
