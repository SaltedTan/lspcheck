# Mutation on the stock servers

**Status:** first measurement, 2026-11-15, of the `[mutation]` sections in the 11
[example profiles](../examples/profiles/README.md) that the [stock-server matrix](../ci/stock-servers/README.md) runs
([design, step 7](source-mutations.md#implementation-plan)). It is the evidence for the matrix's
`.mutation.verdicts` files, which its [mutation jobs](../ci/stock-servers/README.md) compare against. The findings
about these servers stay in this repository and are not reported to their projects.

## How it was run

lspcheck at d9ba0ef with the profiles of this change, built with `cargo build --release`, on Linux x86_64 with 28
CPUs. Each server ran its example profile, with the binary under the name the profile gives found on `PATH`:

```sh
lspcheck --profile examples/profiles/SERVER.toml --mutate --mutation-seed 20261115 --mutation-cases 10 \
  --json report.json --transcripts DIR
```

Each server ran three times, one server after another, then once more while one busy loop (`yes > /dev/null`) per CPU
saturated the machine. Separately, each server but lemminx ran once with `--mutation-cases 100` and
`--only M1,M2,M3,D1,D2,D6,K1` ([below](#one-hundred-cases)). The transcripts are not kept in the repository.

| Server | Version run | Matrix pin |
|---|---|---|
| gopls | v0.24.0, with Go 1.26.8 | v0.24.0, Go 1.27.3 |
| rust-analyzer | 0.3.3105-standalone, with cargo 1.99.1 | release 2026-11-02, the runner's cargo |
| lua-language-server | 3.19.2 | 3.19.2 |
| basedpyright | 1.40.2, Node 24.21.0 | 1.40.2, the runner's Node |
| clangd | 23.1.0 | 23.1.0 |
| marksman | 2026-02-08 | 2026-02-08 |
| ruff | 0.17.4 | 0.17.4 |
| texlab | 5.26.0 | v5.26.0 |
| zls | 0.17.0, no zig on `PATH` | 0.17.0 |
| terraform-ls | 0.39.1 | 0.39.1 |
| lemminx | 0.11, native binary from vscode-xml 0.29.3 | the same |

## The profiles

Each `[mutation]` section spells out a lexicon for its language: line and block comments, strings with their escape
character, extra identifier characters where the language has them (`-` in Terraform and Markdown, `\`, `@` and `:` in
LaTeX, so that `\label` and `sec:greeting` are single tokens), brackets (`<>` alone for XML), keywords and a few
malformed fragments for the `fragment` operator. Every profile keeps the default operators, cases, steps, requests and
bounds. No server needed an operator left out. The test `the_example_mutation_sections_validate_and_fit_their_run`
(in `src/profile.rs`) checks that each section validates and that its case budget (1055 s at the profiles' 30 s
request timeout) fits the default one-hour `run_timeout`. It also checks that the lexicon recognises at least one of
its keywords in the profile's seed text, and a comment or a string wherever the seed contains a delimiter the lexicon
declares, and that every server of the stock matrix has a section.

`[mutation.recovery].expect = "baseline"` is stated only where every probe the server uses answers from the text
alone: marksman (headings), zls without zig (parsed file), terraform-ls (parsed file), ruff (lint diagnostics) and
texlab (parsed file, no other files or build output). The other profiles leave it out, with a comment saying why. The
diagnostics of gopls, rust-analyzer, basedpyright and clangd depend on what each loads beside the text (module graph,
sysroot, type stubs, system headers). lua-language-server sometimes labels stale diagnostics with a new version (its
`D1 fail|pass` line). For lemminx it makes no difference. Without `expect`, a recovery not seen is `ERROR`, as the
design intends. No profile sets `no_errors`: with pushed diagnostics it makes every case wait the whole
`recovery_timeout`, and with `expect` stated it adds little.

Nothing changes in default runs: `ci/stock-servers/compare` matched the ruff, texlab, lemminx and marksman baselines
with the new sections, and in every `--mutate` run below, all checks but M1 to M3 and the four extended checks gave
the stock verdicts.

## Results with 10 cases

Verdicts are for runs 1 to 3 and the run under load. One value means all four agreed. "Run" is the whole lspcheck
run, most of it the other scenarios. "Scenario" is the mutation scenario from start to end, including the settle
period and the baselines. "Per case" is the time from one case's plan to the next one's, over the quiet runs.

| Server | M1 | M2 | M3 | D1 | D2 | D6 | K1 | Run (s) | Scenario (s) | Per case mean / max (s) | Under load: run (s), per case |
|---|---|---|---|---|---|---|---|---|---|---|---|
| gopls | pass | pass | pass | pass / pass / pass / N/E | pass | pass | pass | 38 | 2.8–2.9 | 0.08 / 0.23 | 41, 0.13 / 0.30 |
| rust-analyzer | pass | pass | pass | fail | N/E | pass | pass | 15 | 3.9–4.1 | 0.02 / 0.08 | 19, 0.08 / 0.19 |
| lua-language-server | pass | pass | pass | fail | pass | skip | pass | 105–106 | 7.2 | 0.01 / 0.02 | 107, 0.01 / 0.02 |
| basedpyright | pass | pass | pass | pass | pass | skip | pass | 12 | 6.9 | 0.46 / 1.93 | 19, 0.74 / 3.77 |
| clangd | pass | pass | pass | fail | pass | pass | pass | 3 | 2.2 | 0.01 / 0.02 | 4, 0.03 / 0.06 |
| marksman | pass | pass | pass | N/E | N/E | skip | pass | 51 | 2.7–2.8 | 0.01 / 0.05 | 60, 0.04 / 0.12 |
| ruff | pass | pass | pass | pass | N/E | skip | pass | 12 | 2.0–2.1 | 0.00 / 0.01 | 13, 0.01 / 0.02 |
| texlab | fail | pass | error | N/E | N/E | skip | pass | 46–47 | 2.4 | 0.03 / 0.08 (3 cases) | 52, 0.03 / 0.09 |
| zls | pass | pass | pass | pass | N/E | fail | pass | 17 | 2.1 | 0.00 / 0.01 | 17, 0.00 / 0.01 |
| terraform-ls | pass | pass | pass | pass | N/E | skip | pass | 83 | 7.3–7.5 | 0.04 / 0.07 | 84, 0.07 / 0.10 |
| lemminx | error | error | error | pass | N/E | pass | error | 195 | 60.0 | — (no case ran) | 196, — |

Probes used for M3 (unadvertised ones are dropped automatically): documentSymbol, foldingRange and pushed
diagnostics for gopls and clangd; documentSymbol, foldingRange and pulled diagnostics for rust-analyzer;
documentSymbol and foldingRange for lua-language-server, zls and texlab; documentSymbol and pushed diagnostics for
basedpyright; documentSymbol alone for marksman and terraform-ls; pulled diagnostics alone for ruff. Every case that
reached its restoration recovered within 0.5 s, most within a few milliseconds. M2's error answers are lawful answers
and are counted in its detail: gopls answers some requests with error code 0, clangd with -32001, rust-analyzer with
-32602 and terraform-ls with -32098.

### Each FAIL and ERROR

- **texlab M1 `FAIL`, M3 `ERROR`** (a crash in the server). In case 2, step 3, texlab panics
  (`crates/texlab/src/server.rs:422:77: called Option::unwrap() on a None value`) and exits with status 101 on reading
  this change to `\documentclass{article$}` + U+0301 U+10FFFF + `\n\begin{document}\n\section{Greeting}\label`:
  `(0,27)-(2,0)` → `"\r\\begin{document}\r"`, then `(2,0)-(2,24)` → `""`. The first change leaves lone `\r` line ends,
  so the second change's line 2 exists only if a lone `\r` ends a line, as LSP 3.17 says it does. Replaying the change
  with both splits confirms it is valid under the specification's split, and that under a `\n`-only split line 2
  doesn't exist. lspcheck checks every change it sends with `text::apply_change`. M3 is `ERROR` because the run
  stopped before the remaining cases' recovery was judged. Case 2 alone reproduces the crash. Same in all four runs.
- **lemminx M1, M2, M3 and K1 `ERROR`** (a known server bug). lemminx never answers an `initialize` whose client
  capabilities include `textDocument.moniker: {}`, which K1's capabilities scenario and the mutation scenario both
  send, so no case runs. This is the same bug as its stock `K1 error`. A profile can't change the client capabilities
  (`[client].capabilities` is still planned).
- **D1 `FAIL` for rust-analyzer, lua-language-server and clangd** comes from the documents scenario, as in their stock
  baselines. In the mutation scenario D1 passes for all three: 8, 2 and 10 informative positions.
- **zls D6 `FAIL`** comes from the documents scenario, as in its baseline (its `\n`-only line split). The mutation
  scenario passes with 5 replies.

### The one difference between runs

**gopls D1** was `pass` in the three quiet runs and `N/E` under load. Its only informative positions (a position with
non-ASCII text before it) are in one `publishDiagnostics` for version 10 (case 3), at `(9,31)`. In the quiet runs gopls
published it 30 to 60 ms after reading version 10, before lspcheck sent version 11 (the step's requests had all been
answered). Under load it published nothing for version 10 before version 11 arrived, then published for version 11.
LSP doesn't require a publish for every version, and lspcheck's edits and requests are the same in every run; what
differs is whether gopls's diagnostics finish before the next change. No output was missed. This is the server's
scheduling, not lspcheck's timing, so the matrix would need `D1 pass|not-exercised` for gopls, citing this run. The
other servers' informative positions are in replies to requests, and were the same in all four runs (ruff 2,
zls 9, terraform-ls 4, lua-language-server 2, clangd 10, rust-analyzer 8).

## One hundred cases

Within the first ten cases, only texlab's and marksman's texts ever hold a lone `\r` (case 2's last step, whose
`line-ends` turns every line end into one; texlab crashes on it, and marksman recovers). For the other nine seed
texts, the first such case is case 30: version 93 makes one line end a lone `\r`, and version 94 edits the lines after
it. One run of each server with `--mutation-cases 100` gave:

| Server | M1 | M2 | M3 | D1 | D6 | Cases run | Scenario |
|---|---|---|---|---|---|---|---|
| gopls | pass | pass | error | pass | fail | 100 | 19 min |
| rust-analyzer | pass | pass | error | fail (documents) | pass | 82 | 43 min, stopped by `run_timeout` |
| clangd | pass | pass | error | fail (documents) | pass | 73 | 43 min, stopped by `run_timeout` |
| terraform-ls | pass | pass | fail | fail | skip | 100 | 34 min |
| zls | pass | pass | pass | pass | fail | 100 | 2 s |
| texlab | fail | pass | error | N/E | skip | 3 | 2 s (case 2's crash) |
| lua-language-server, basedpyright, marksman, ruff | pass | pass | pass | as with 10 | as with 10 | 100 | 3 to 32 s |

All the new verdicts but gopls's D6 and terraform-ls's D1 trace back to one behaviour: these servers don't end a line at
a lone `\r`, as LSP 3.17 says they must. That shifts every line after the `\r`, so a later change's range names lines
the server's copy doesn't have. lspcheck's changes are right under the specification's split (checked by replaying them
under both splits), and its restoration is one change whose range covers the whole document, `(0,0)` to `(lines, 0)` in
the specification's lines. Replaying case 30's four `didChange` notifications alone, with a small client, against each
server:

- **gopls** and **rust-analyzer** silently drop a change whose range ends past their last line. The restoration is
  dropped and the document stays as it was, so documentSymbol answers one line short (gopls: `greet` 4–5 instead of
  4–6) until a later change happens to land. Restoring with a full-text change instead recovers at once. gopls logs
  nothing. M3 is `ERROR` (not seen to recover) because their profiles state no `expect`.
- **clangd** removes the document on version 94's change: every later request about it is answered with
  `-32602 trying to get AST for non-added document`, even after a full-text change. M2 passes (an error is an
  answer). M3 is `ERROR`, as a terminal probe answered with an error must be.
- **terraform-ls** behaves like gopls: after version 93 its `output "greeting"` symbol disappears, and the range-based
  restoration doesn't bring it back, while a full-text one does. Its profile states `expect = "baseline"`, so M3
  `FAIL`s on its terminal probe 60 s after the restoration; case 30 alone reproduces it. The answer is wrong about the
  restored text, which is what M3 judges. It is not lawful drift: the text is clean and the server's copy isn't.
  terraform-ls also fails D1 in cases 34 and 38: after an astral character it counts columns in code points, though
  UTF-16 was negotiated (both reproduced alone).
- **zls** fails D6 in case 60, where the text has only lone `\r` line ends, so zls sees one line of 193 characters,
  and answers `(0,0)-(0,193)` for the position `(3,37)`. In case 78 the text has one lone `\r`, so zls's whole
  document ends at `(8,0)` and the specification's at `(9,0)`, the position asked about (both reproduced alone).
- **gopls** fails D6 in cases 24, 45 and 59, each reproduced alone. In each the position is the document's end, on
  the empty line after its last line end, and gopls's widest selection range stops before it: in case 24 the position
  is `(12,0)` and the range ends at the last `}`, `(11,1)`; in case 45 (`(10,0)`) an unterminated string stops it at
  `(5,17)`; in case 59 (`(10,0)`) it ends at `(9,1)`. No lone `\r` is involved: with terraform-ls's D1, these are the
  only new findings that aren't the line split.

None of these is an lspcheck bug. They do show two costs worth knowing before choosing counts:

- A server that loses track of the document after case 30 makes every later case wait the whole `recovery_timeout`
  (60 s with these profiles) before its terminal probe. That is why rust-analyzer and clangd ran out of `run_timeout`,
  and why their isolated runs weren't run ("`mutation.run_timeout` left 1018 s, less than the 1235 s an isolated run
  may take").
- One fault can explain most of a run's failing cases (gopls 43, rust-analyzer 101): M3's detail names the first, and
  isolation the first three, which is where to look.

## Case counts

When the server keeps up, a case takes milliseconds; basedpyright's take 0.46 s on average and up to 2 s (4 s under
load), most of it waiting for its pushed diagnostics to return to the baseline. Ten cases add at most 5 s to a run on a
quiet machine (about 7 s for basedpyright under load), and 100 cases at most 32 s. The cost that matters is a case that
doesn't recover: each takes `recovery_timeout`, and when one fault desynchronizes the document, every later case does.

- **Matrix: 10 cases**, as the design planned. Verdicts were the same in all four runs except gopls's D1 above, and
  the scenario took under 10 s for every server but lemminx. Ten cases already find texlab's crash (the same
  lone-`\r` handling, reached in case 2). They don't reach case 30: a larger count would add the lone-`\r` findings
  above, but would cost 10 to 40 minutes per job for gopls, rust-analyzer, clangd and terraform-ls, and no repeated
  runs at that count have shown those verdicts stable. If the matrix should cover them, 40 cases (10 desynchronized
  cases at 60 s each, about 10 minutes per job) is the smallest count that does. Measure it before pinning.
- **Default: keep 20.** For a server that recovers, 20 cases take seconds; for one that doesn't, each extra case adds
  `recovery_timeout`. The numbers don't call for a change in this PR. A user who wants more should pass
  `--mutation-cases` and expect a minute per desynchronized case.

## What to pin in `.mutation.verdicts`

With `--mutation-seed 20261115 --mutation-cases 10`, every check but M1 to M3 and the extended checks as in the
server's `.verdicts`, and these:

| Server | M1 | M2 | M3 | D1 | D2 | D6 | K1 |
|---|---|---|---|---|---|---|---|
| gopls | pass | pass | pass | `pass\|not-exercised` (gopls's diagnostics may skip a version; above) | pass | pass | pass |
| rust-analyzer | pass | pass | pass | fail | not-exercised | pass | pass |
| lua-language-server | pass | pass | pass | `fail\|pass` (unchanged: its documents-scenario race) | pass | skip | pass |
| basedpyright | pass | pass | pass | pass | pass | skip | pass |
| clangd | pass | pass | pass | fail | pass | pass | pass |
| marksman | pass | pass | pass | not-exercised | not-exercised | skip | pass |
| ruff | pass | pass | pass | pass | not-exercised | skip | pass |
| texlab | fail | pass | error | not-exercised | not-exercised | skip | pass |
| zls | pass | pass | pass | pass | not-exercised | fail | pass |
| terraform-ls | pass | pass | pass | pass | not-exercised | skip | pass |
| lemminx | error | error | error | pass | not-exercised | pass | error |

rust-analyzer's stock `D1` line has no alternative. The other stock alternatives (gopls's framing race,
lua-language-server's diagnostics race, rust-analyzer's descendant) still apply and were carried over. lemminx's job
adds a minute for no evidence and runs anyway, for a uniform matrix; that is the maintainer's choice.

## The matrix's first runs

The 11 `.mutation.verdicts` files record the table above, with each default file's lines and alternatives carried over.
The runner's toolchains differ from these measurements (Go 1.27.3 for gopls; the runner's cargo and Node), so the
first runs of the matrix's mutation jobs were the check on the record. A verdict that differs from the table is
evidence about the runner, not a baseline to accept without inspecting its transcript.

The workflow ran three times on the change that added the jobs (2026-11-15, one `pull_request` run and two
`workflow_dispatch` runs of the same commit). All 22 jobs passed each time: every mutation job's verdicts matched its
file. gopls's D1 was `pass` in the runs inspected, and lua-language-server's documents-scenario D1 and D3 took both of
their recorded values (`fail` in one run, `pass` in another), as in its default job; its mutation-scenario checks
passed. A mutation job took 1 to 4 minutes, at most about a minute longer than its server's default job; lemminx's was
the longest (about 4 minutes, its `initialize` timeouts).
