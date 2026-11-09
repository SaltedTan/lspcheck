# Example profiles

Small, self-contained [profiles](../../README.md#profiles) for widely used language servers. Each writes a tiny
project into the workspace with inline `[workspace.files]` and names one of its files in `[document]`, with positions
on real identifiers, so [K1](../../docs/design.md#capabilities-and-robustness) asks the server about code it
understands.

| Profile | Server | Needs on `PATH` |
|---|---|---|
| [gopls.toml](gopls.toml) | `gopls serve` | `gopls`, `go` |
| [rust-analyzer.toml](rust-analyzer.toml) | `rust-analyzer` | `rust-analyzer`, `cargo` |
| [lua-language-server.toml](lua-language-server.toml) | `lua-language-server` | `lua-language-server` |
| [basedpyright.toml](basedpyright.toml) | `basedpyright-langserver --stdio` | `basedpyright-langserver` |

```sh
lspcheck --profile examples/profiles/gopls.toml --only K1
lspcheck --profile examples/profiles/gopls.toml -- ~/go/bin/gopls serve   # another binary
```

They give generous timeouts, since a server may index the project before it answers. A server still loading its
workspace may answer with empty results or errors other than MethodNotFound; K1 accepts those and lists the error
codes in its detail. CI runs every check with each profile against a pinned release of its server and compares the verdicts with
[`ci/stock-servers`](../../ci/stock-servers/), so a change in how lspcheck judges a real server shows up.
