# Stock-server verdicts

The [stock-servers workflow](../../.github/workflows/stock-servers.yml) builds lspcheck from the commit under test and
runs it against pinned releases of gopls, rust-analyzer, lua-language-server and basedpyright, each with its
[example profile](../../examples/profiles/). Each `<server>.verdicts` file here lists the verdict the workflow expects
for every check, one `ID verdict` line per check in catalogue order. The job passes when the verdicts match exactly,
whatever they are, and fails on any difference, showing it as a diff.

The servers and the runner's tools are pinned, so a difference means lspcheck judged differently. Either a change to a
check meant it (update the file and say why in the pull request), or a check went wrong, which is what this workflow is
for: it catches a wrong verdict on a real server before a release does. A new check adds a line to every file.

The expected verdicts include these servers' failures. Each was explained from its transcript before it was recorded.
They are lspcheck's findings about those releases, kept here to detect changes in lspcheck, and are not reported to
the servers' projects; that is the maintainer's decision.

| Server | Pinned release | Failures expected |
| --- | --- | --- |
| gopls | v0.24.0 (Go 1.27.3) | J2, L1, L5, R1, R4 |
| rust-analyzer | 2026-11-02 | L7, D1, D5, R1 |
| lua-language-server | 3.19.2 | L1, L3 to L7, C1, D1, D3, R1 |
| basedpyright | 1.40.2 | L1, L3, L5, R2 |

## Updating

To regenerate a file after an intended change, run the server's profile with the release build and keep the verdicts:

```sh
cargo build --release
target/release/lspcheck --profile examples/profiles/gopls.toml --json /tmp/gopls.json
jq -r '.results[] | "\(.id) \(.verdict)"' /tmp/gopls.json > ci/stock-servers/gopls.verdicts
```

Run it a few times first: a verdict that changes between runs of the same server is a timing dependence in lspcheck,
and a bug. To move a server to a newer release, change its version and SHA-256 digest in the workflow, then
regenerate its file and explain every changed verdict from the transcripts (the workflow keeps them as an artifact).
