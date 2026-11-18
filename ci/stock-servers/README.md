# Stock-server verdicts

The [stock-servers workflow](../../.github/workflows/stock-servers.yml) builds lspcheck from the commit under test and
runs it against pinned releases of gopls, rust-analyzer, lua-language-server, basedpyright, clangd, marksman, ruff,
texlab, zls, terraform-ls and lemminx, each with its [example profile](../../examples/profiles/). Each
`<server>.verdicts` file here lists the verdicts the workflow allows for every check, one `ID verdict` line per check
in catalogue order. The comparison succeeds when the verdicts match, whatever they are, and fails on any difference,
showing it as a diff ([`compare`](compare) does the comparison). An unreadable or malformed report also fails the job.

Each matrix job first runs [`test_compare.py`](test_compare.py); a test failure stops the job before the comparator
is used.

Each server has two jobs. The default one runs the example profile and compares against `<server>.verdicts`, as above.
The mutation one runs the same profile with `--mutate --mutation-seed 20261115 --mutation-cases 10` and compares
against `<server>.mutation.verdicts`; the seed and count are pinned in the workflow's `env` (`MUTATION_SEED`,
`MUTATION_CASES`) so that every mutation job runs the same plan. A mutation file has the default file's lines in the
same order: the other checks' verdicts and comments carry over, and M1 to M3 and the [extended checks'
D1, D2, D6 and K1](../../docs/source-mutations.md#verdict-rules) are those of the mutation run. The two files are
compared separately, so one run cannot excuse a change in the other.

A line may list alternatives, as in `F1 pass|fail`, only for a check that a race in the server itself can flip, with a
comment above it that says what the race is and where it was seen. Four are known:
[gopls](gopls.verdicts) sometimes exits mid-frame (which in its [mutation run](gopls.mutation.verdicts) also leaves M3
inconclusive), [lua-language-server](lua-language-server.verdicts) sometimes
publishes stale diagnostics within lspcheck's wait, [rust-analyzer](rust-analyzer.verdicts) exits without stopping its
sysroot `rustc`, which can outlive lspcheck's grace for descendants, and [gopls](gopls.mutation.verdicts) sometimes
publishes diagnostics for only some of the mutation scenario's versions.

The workflow owns the pinned release versions and SHA-256 digests; the `.verdicts` files own the expected results.
Server releases and Go are pinned, but rust-analyzer's project toolchain and basedpyright's Node come from the runner
image. A difference can therefore reflect a change in lspcheck or the runner environment. Inspect the diff and
transcripts before updating a baseline, and explain intended verdict changes in the pull request. A new check adds a
line to every default and mutation file.

The expected verdicts include these servers' failures. Each was explained from its transcript before it was recorded.
They are lspcheck's findings about those releases, kept here to detect changes in lspcheck, and are not reported to
the servers' projects; that is the maintainer's decision.

## Updating

To regenerate a file after an intended change, run the server's profile with the release build and keep the verdicts:

```sh
cargo build --release
target/release/lspcheck --profile examples/profiles/gopls.toml --json /tmp/gopls.json
jq -r '.results[] | "\(.id) \(.verdict)"' /tmp/gopls.json > /tmp/gopls.verdicts
ci/stock-servers/compare ci/stock-servers/gopls.verdicts /tmp/gopls.json   # what changed
```

Then update the file, keeping its comments and alternatives.

A mutation file is regenerated the same way, with the workflow's pinned seed and count, and the exit status is expected
to be non-zero for the servers that fail M1 to M3:

```sh
target/release/lspcheck --profile examples/profiles/gopls.toml --mutate --mutation-seed 20261115 --mutation-cases 10 \
  --json /tmp/gopls-mutation.json
jq -r '.results[] | "\(.id) \(.verdict)"' /tmp/gopls-mutation.json > /tmp/gopls.mutation.verdicts
ci/stock-servers/compare ci/stock-servers/gopls.mutation.verdicts /tmp/gopls-mutation.json   # what changed
```

Keep the seed and count that the workflow's `env` pins; a different plan gives different verdicts.

Run it a few times first: investigate any verdict that changes between runs of the same server before recording it.
A verdict that varies because of lspcheck's own timing is a bug in lspcheck, not an alternative to list.
To move a server to a newer release, update its pins in the workflow, then
regenerate both its files and explain every changed verdict from the transcripts (the workflow keeps them as
artifacts).
