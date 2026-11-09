# Stock-server verdicts

The [stock-servers workflow](../../.github/workflows/stock-servers.yml) builds lspcheck from the commit under test and
runs it against pinned releases of gopls, rust-analyzer, lua-language-server and basedpyright, each with its
[example profile](../../examples/profiles/). Each `<server>.verdicts` file here lists the verdict the workflow expects
for every check, one `ID verdict` line per check in catalogue order. The job passes when the verdicts match,
whatever they are, and fails on any difference, showing it as a diff ([`compare`](compare) does the comparison).

A line may list alternatives, as in `F1 pass|fail`, only for a check that a race in the server itself can flip, with a
comment above it that says what the race is and where it was seen. gopls has one: after `exit`, it sometimes exits
with a frame's header written and its body not, which F1 fails on what it saw and which leaves J1, J3, J5 and J7
inconclusive about that frame.

The workflow owns the pinned release versions and SHA-256 digests; the `.verdicts` files own the expected results.
Server releases and Go are pinned, but rust-analyzer's project toolchain and basedpyright's Node come from the runner
image. A difference can therefore reflect a change in lspcheck or the runner environment. Inspect the diff and
transcripts before updating a baseline, and explain intended verdict changes in the pull request. A new check adds a
line to every file.

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

Run it a few times first: investigate any verdict that changes between runs of the same server before recording it.
A verdict that varies because of lspcheck's own timing is a bug in lspcheck, not an alternative to list.
To move a server to a newer release, update its pins in the workflow, then
regenerate its file and explain every changed verdict from the transcripts (the workflow keeps them as an artifact).
