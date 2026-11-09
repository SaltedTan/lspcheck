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
| [clangd.toml](clangd.toml) | `clangd` | `clangd` |
| [typescript-language-server.toml](typescript-language-server.toml) | `typescript-language-server --stdio` | `typescript-language-server`, with the `typescript` package installed beside it |
| [bash-language-server.toml](bash-language-server.toml) | `bash-language-server start` | `bash-language-server` |
| [yaml-language-server.toml](yaml-language-server.toml) | `yaml-language-server --stdio` | `yaml-language-server` |
| [vscode-json-language-server.toml](vscode-json-language-server.toml) | `vscode-json-language-server --stdio` | `vscode-json-language-server` (from `vscode-langservers-extracted`) |
| [marksman.toml](marksman.toml) | `marksman server` | `marksman` |
| [ruff.toml](ruff.toml) | `ruff server` | `ruff` |
| [texlab.toml](texlab.toml) | `texlab` | `texlab` |
| [zls.toml](zls.toml) | `zls` | `zls` |
| [jedi-language-server.toml](jedi-language-server.toml) | `jedi-language-server` | `jedi-language-server` |
| [pylsp.toml](pylsp.toml) | `pylsp` | `pylsp` (from `python-lsp-server`) |
| [terraform-ls.toml](terraform-ls.toml) | `terraform-ls serve` | `terraform-ls` |
| [lemminx.toml](lemminx.toml) | `lemminx` | `lemminx` (the native binary, such as `lemminx-linux-x86_64`, under that name) |
| [taplo.toml](taplo.toml) | `taplo lsp stdio` | `taplo` (built with its `lsp` feature, as `taplo-cli` is) |

Checked on 2026-11-10 with jedi-language-server 0.47.0, python-lsp-server 1.15.0, terraform-ls 0.39.1,
lemminx 0.11 (native binary) and taplo 0.10.0. Verdicts were stable across repeated runs except for taplo:
at end of input it sometimes exits between writing a frame's header and its body. This is a race in the server,
so its framing verdict can vary between runs.

```sh
lspcheck --profile examples/profiles/gopls.toml --only K1
lspcheck --profile examples/profiles/gopls.toml -- ~/go/bin/gopls serve   # another binary
```

They give generous timeouts, since a server may index the project before it answers. A server still loading its
workspace may answer with empty results or errors other than MethodNotFound; K1 accepts those and lists the error
codes in its detail. The [stock-server CI matrix](../../ci/stock-servers/README.md) documents which profiles it runs.
