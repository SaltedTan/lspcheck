# Seeded source mutation and recovery

**Status:** Accepted, 2026-11-11; the questions it left open are settled under [Decisions](#decisions). Steps 1 to 3 of
the [implementation plan](#implementation-plan) are in place: the planner (`src/mutation.rs`), the profile's
`[mutation]` section, and the scenario, which judges M1 and M2. `--mutate` and its two flags are experimental and
hidden from `--help` until recovery (M3) is judged; until then the scenario doesn't settle or take baselines, M3 is
`SKIP`, and D1, D2, D6 and K1 aren't extended to it. See [what step 3 leaves out](#step-3) and the
[design status](design.md) for its place in the roadmap.

## Context

lspcheck's document checks open plain-text documents of its own. Most language servers ignore plain text, so on real
servers D1, D2 and D6 are usually `N/E`: the code that parses, analyses and answers about real source is never asked a
question whose answer lspcheck can judge. And nothing checks what a server does while its source is broken, which is
most of the time in an editor.

virgil-lsp has a harness for this, `test/protocol/source_mutations.py`: it edits real source with seeded, token-aware
mutations, sends requests after each edit, restores the clean source and checks the server recovers. Some of it is
generic and some only makes sense for Virgil:

| Generic: port it | Virgil-specific: leave it |
|---|---|
| Seeded plan, one RNG stream per case, identical bytes for the same seed and inputs | Repair seeds and their cursor variants; the observer file |
| Token-aware edits found by a lexer, no grammar | Virgil's token regex, keywords and malformed literals (`0i`, `0x`) |
| Edit families: delete, duplicate, swap, replace, truncate, brackets, strings, comments, line ends, non-ASCII, rename, bounded stress edits | Worker processes, compiler trap signatures, JVM exception tables, `KNOWN_WORKER_FAILURES` |
| Every request gets one reply; the server keeps serving | Exact clean-source answers (hover text, a definition's location, supertypes) |
| Request, case and run deadlines checked by every read, write and wait | The coverage manifest that fails a case on a new capability |
| Recovery once the source is clean again | The list of documented error codes per method |
| Replayable case records, bounded artifacts that keep a failure's evidence | `window/showMessage` matching against the server log |

Two things in the harness must not be ported. It sends positions in the middle of a surrogate pair on purpose; a
client must not, and a server that mishandles such a position would be judged on lspcheck's own violation. And its
recovery oracle knows what Virgil's clean source must answer; lspcheck can't.

## Decision

Add an opt-in `mutation` scenario, run only with `--mutate`. It opens seed files from the profile's workspace, records
how the server answers about the clean text, then runs a number of seeded cases. A case edits the open documents in one
to three steps, sends requests about the broken text after each step, restores the clean text and waits for the
server's answers to return to what they were. Then the scenario shuts the server down.

It adds three checks, all `POLICY`, because LSP doesn't say what a server must do with source that doesn't parse. The
protocol rules are judged by checks that already exist, which gain evidence from real source and from many versions.

### What is checked

M1 to M3 are listed in the [check catalogue](design.md#mutation).

Spec references, in the style of the catalogue: M1 and M3 rest on
[`textDocument/didChange`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#textDocument_didChange),
M2 on [`responseMessage`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#responseMessage)
and [JSON-RPC 2.0 §5](https://www.jsonrpc.org/specification#response_object). That a request is answered at all is a
`MUST` of both; answering in a time lspcheck picks is not, so M2 is `POLICY`, like D4 and R3. Surviving broken source
and recovering from it are lspcheck's policy, not the specification's.

Existing checks keep being judged on the new scenario's evidence:

- **Cross-cutting, unchanged:** F1, F2, F8, J1, J2, J3 (the scenario's input is well formed), J5, J7, L9 and K2. Real
  servers send real results and diagnostics here, so K2 validates far more than in the other scenarios.
- **Extended to the mutated documents:** D1 (positions on character boundaries), D2 (diagnostics versions), D6
  (selection-range containment) and K1 (an advertised request isn't answered with MethodNotFound). Their rules don't
  depend on the text, only on knowing it, and lspcheck knows every version it sent. Each is judged per scenario, and
  the two verdicts combine as `fail` over `error` over `pass` over `not-exercised`
  over `skip`; the detail names the scenario and case. K1 takes only its failure from the mutation scenario: an
  unanswered mutation request is M2's, so it never makes K1 `ERROR`.
- **Not extended:** D3 and D5 rest on documents built so that one specific mistake lands inside a character; a
  random edit can't make that diagnosis, so D1 judges those positions. D4, the C, L1 to L8 and R checks need their own
  scenarios.

Without `--mutate`, nothing above changes: the scenario doesn't run and M1 to M3 report `SKIP` (`mutation is off;
pass --mutate`), which doesn't affect the exit status ([catalogue](design.md#mutation)).
With `--mutate` and a seed file, the scenario runs when an M check or a cross-cutting check is selected and not
skipped by the profile.

### The scenario

1. Initialize with K1's client capabilities plus `window.workDoneProgress` and versioned `publishDiagnostics`. If the
   server doesn't ask for `didOpen`, or takes no changes (`textDocumentSync` absent or `0`), M1 to M3 are `SKIP`.
2. Open each seed file at version 1 with the profile's language id. The files stay as they are on disk: lspcheck never
   writes mutated text, and `didOpen` puts the document's content under client ownership, so the server must not read
   it from disk while it is open.
3. **Settle**: wait until no work-done progress the server began is unfinished and no `publishDiagnostics` or
   `$/progress` has arrived for 1 s, bounded by `settle_timeout`. A server that never settles is noted in the detail;
   the baseline's stability rule below still protects M3.
4. **Baseline**: send each recovery probe (below) about each seed document, then again each second, until two answers
   in a row are equal after projection, or `settle_timeout` ends. A probe with a stable answer is used; one that
   errors, varies or isn't advertised isn't, and the detail says why. Pushed diagnostics are a notification, never
   requested, and a lawful server may publish once per version: their baseline is the publishes labelled with version 1
   read before the first mutation. If there is at least one and all of them are equal after projection, the last is the
   baseline; if they differ, or none arrived, pushed diagnostics aren't used (and `no_errors` can only be established
   from a pull report).
5. **Cases**, in order, while the run's budget lasts. Case *k* has its own RNG streams, so it doesn't depend on the
   cases before it or on the server's timing. Each step applies one to three mutations to one document, sends them in
   one `didChange`, then sends the step's requests (default four) without waiting between them, and waits for each
   reply until its own deadline.
6. **Restore**: one `didChange` replaces each mutated document's whole text with its clean text, at the next version:
   the full text for a server with full sync, and for incremental sync one change whose range covers the whole current
   document.
   Then lspcheck polls the stable probes, 250 ms apart and doubling to 2 s, until every answer equals its baseline or
   `recovery_timeout` ends. Each poll is a request like any other, judged by M2 and the cross-cutting checks.
7. After the last case, `leave()`: `shutdown`, `exit`, and L9's scan.

### Mutations

**What the profile supplies.** The seed files (default: `[document]`'s file and language id), and optionally a lexicon
that says where tokens are. Without one, a default lexicon is used: identifiers are runs of Unicode letters, digits and
`_`; numbers are runs that start with an ASCII digit; whitespace and line ends are their own tokens; every other
character is a one-character punctuation token; the brackets are `()`, `[]` and `{}`. With a lexicon, line comments,
non-nesting block comments, strings with an escape character, extra identifier characters and keywords are recognised.
A string runs to its closing delimiter, across lines if need be; one never closed runs to the end of its line, and a
block comment never closed to the end of the document. Where several delimiters start at the same place, the longest
wins.

The lexicon only decides *where* to edit. No oracle depends on it, so a wrong lexicon makes the edits less pointed but
can't make a verdict wrong. That is why lspcheck can stay ignorant of the language.

**Operators.** Each picks its target with the case's RNG. If it finds nothing to change (including a `swap-tokens` swap
that leaves identical text), it inserts a token from the document's own tokens instead, and the record says so. If none
of the document's tokens fits the [growth bound](#bounds) (including an empty seed, or one a `truncate` emptied), the
fallback inserts the lexicon's first opening bracket, else `(`, at offset 0, so the plan stays a total function of the
seed. Planner tests cover an empty seed and mutations after a truncation to empty.

| Operator | Edit |
|---|---|
| `delete-tokens` | Delete 1 to 3 consecutive non-space tokens. |
| `duplicate-tokens` | Repeat 1 to 4 consecutive tokens after themselves. |
| `swap-tokens` | Swap two tokens of the same class (identifier, number, string). |
| `replace-token` | Replace a token with one drawn from the document's tokens, the brackets and the keywords. |
| `truncate` | Cut the document at a token boundary. |
| `delete-line`, `duplicate-line` | Delete or repeat one physical line, including its line end when present. |
| `unbalance-bracket` | Delete, duplicate or insert one bracket of a lexicon pair. |
| `open-string`, `open-comment` | Remove a string's or block comment's closing delimiter, or insert a lone opening one (needs a lexicon). |
| `insert-in-literal` | Insert characters from the fixed set below inside a string or comment, or else inside an identifier. |
| `unicode` | Insert one character of the fixed set at a token boundary. |
| `line-ends` | Make one line end, or every one, `\r\n` or a lone `\r`. |
| `rename` | Replace every occurrence of one identifier with another of the document's. |
| `fragment` | Insert one of the profile's `fragments` (malformed literals and the like) at a token boundary. |
| `nest`, `repeat-line` | Stress: nest a bracket pair deeply, or repeat a line many times, up to the room left (off by default). |
| `control` | Insert a C0 control character or U+0000 (off by default). |

The line operators find `\r\n`, `\n` and lone `\r` directly in the text, including inside multiline strings and block
comments. `duplicate-line` and `repeat-line` also copy a final line without a line end: each copy gets the document's
last line end, or `\n` if it has none, and is inserted before the original final line.

The fixed character set is `é` (2 bytes in UTF-8), `中` (3), `😀` and `𝔘` (4, a surrogate pair in UTF-16), a combining
acute, a ZWJ, U+00A0, U+200B, U+FEFF and U+10FFFF. Every inserted character is a Unicode scalar value. Invalid UTF-8 and
lone surrogates (`"\ud800"` in JSON) are never sent: LSP text is Unicode, and a server's reaction to text that isn't
would be judged on lspcheck's own violation.

No edit starts or ends between a `\r` and a `\n`. No position can name that point (a line's length excludes its end),
so such an edit could not be sent incrementally, and splitting the pair would change two line breaks at once.

**Sending edits.** lspcheck sends what the server's `textDocumentSync` kind asks for. For incremental sync, each mutation becomes one content change: the smallest range that differs (common prefix and
suffix, in characters), widened so that neither end falls between a `\r` and a `\n` in the text before or after the
change (changing `x\r\ny` to `x\ry` has a common prefix `x\r`, and deleting only the `\n` would start inside a CR LF;
widened, the change replaces `\r\n` with `\r`), counted in the negotiated encoding against the text left by the change before it, as the
specification orders them. lspcheck applies each change with `text::apply_change` and asserts the result is the planned
text, so a bug in its own range arithmetic stops the run as an internal error, never as a verdict. Full sync sends the
whole text. There is no option to send full text to a server that asked for incremental changes: LSP says that after
`didOpen` such a server receives only incremental updates, so a range-less change would be lspcheck's own violation.

**Determinism.** A case's edits come from SplitMix64 seeded by (seed, case index, `"edits"`), and its requests and
positions from a second stream seeded by (seed, case index, `"requests"`). Which methods the server advertises therefore
changes the requests but not the texts. The generator, the draw order and the operators are part of a mutation
`format` number recorded in the report and the transcript; a seed reproduces its plan under the same format. The plan
is a pure function, so it is unit tested directly, including a pinned golden case. No new dependency: SplitMix64 is a
few lines, and a crate's generator may change its stream between releases. Only the server's timing varies between
runs: how many recovery polls are sent, and what the server answers.

### Requests and positions

Each step sends `requests_per_step` requests, round robin over the advertised methods of K1's table that the profile's
`requests` allows (default: all of them), with a cursor that runs across cases so every method is exercised. Their
params are K1's. Positions are drawn from the current text: half inside an identifier, a tenth at the end of the
document, the rest at any character boundary, never inside a character and never between `\r` and `\n`, counted in
the negotiated encoding. `workspace/symbol` queries up to 64 characters of an identifier from the text, or an empty
string when there is none. Rename and formatting edits are never applied. The requests of one step are written back
to back and waited for together, each with its own deadline from when its write began.

### Recovery

A recovery probe is a request whose answer depends only on a document's text, compared after a projection that keeps
what describes the text and drops what may lawfully vary:

| Probe | Kept | Dropped |
|---|---|---|
| `textDocument/documentSymbol` | name, kind, ranges, children; for `SymbolInformation`, the range and container name | `detail`, `tags`, `deprecated`, `data` |
| `textDocument/foldingRange` | start and end lines and characters, kind | `collapsedText` |
| `textDocument/diagnostic` (a full report) | each item's range, severity, code, source | `message`, `data`, `relatedInformation`, `codeDescription`, `resultId` |
| `publishDiagnostics` labelled with a version | the same per diagnostic | the same |

Every array of the projection compares as a multiset, recursively (a symbol's `children` too), because servers may
order them by hash; each element is projected before the comparison. `semanticTokens` is left out for now: its
`data` is long, and a legend change between answers would need decoding. Unlabelled `publishDiagnostics` aren't used:
lspcheck can't tell which text they describe. The profile's `recovery.probes` may narrow the list.

A case has **recovered** when every stable probe of every seed document, asked after the restoring `didChange` was
delivered, gets an answer equal to its baseline (for pushed diagnostics, a publish labelled with the restored version).
lspcheck can observe that without knowing the language. It cannot decide:

- whether the baseline was right: the clean answer might already be wrong;
- whether an answer that never returns to baseline is a fault. A server's answers can legitimately depend on state
  outside the text: an index that finished loading after the baseline, a cache, a project-wide analysis. Only someone
  who knows the server can say its answers for this seed are fixed;
- whether a slow server would have recovered after the bound.

So a recovery not seen within the bound is `ERROR`, not `FAIL`, unless the profile states an expectation. Two can be
stated in `[mutation.recovery]`:

- `expect = "baseline"`: the server's answers about the clean seed are fixed, and return to them within
  `recovery_timeout` of the restoration (the profile's own policy deadline). Differing answers before the last are
  lawful (a server catching up). A polled probe that hasn't recovered when `recovery_timeout` ends gets one more
  request, the **terminal probe**, written after the deadline; M3 fails when its answer differs from the stable
  baseline. Judging the answer to a request sent after the deadline, not the last poll before it, keeps the verdict
  independent of lspcheck's poll interval: a server that recovered at 19.8 s of 20 answers the terminal probe with the
  baseline, whenever the last poll was. Pushed diagnostics (when they are a stable probe) are compared the same way
  through the push rule below: the last publish labelled with the restored version and read by the end of
  `recovery_timeout` fails M3 when its projection differs from the baseline publish's.
- `no_errors = true`: the seed has no Error-severity diagnostics. It must hold at baseline (pushed diagnostics labelled
  1, or a pull report); if it doesn't, M3 is `ERROR` ("the profile and the server disagree before any mutation"),
  since lspcheck can't tell which is wrong. After restoration, a pulled report is judged by its terminal probe, as
  above. Pushed diagnostics aren't sampled: lspcheck reads every publish as it arrives, so the last publish labelled
  with the restored version and read by the end of `recovery_timeout` is the server's own final word within the
  profile's deadline; if it includes an error, M3 fails. No publish labelled with the restored version by then is
  `ERROR`, not a failure: LSP doesn't require a server to publish for every version.

Either way the server confirmed the expectation itself before the mutations, then answered against it about the same
text past the deadline. A failure needs complete evidence: the terminal probe delivered and answered (for pushed
diagnostics, the whole bound read), and nothing missed in lspcheck's reading of the server's output (a skipped or
cut-short frame) between the deciding answer and its end (for a pull, from the terminal probe's write to its answer;
for a push, from that publish to the end of the bound). Anything less, including polling stopped early by lspcheck's
poll budget or a terminal probe left unanswered, makes M3 `ERROR` for that case, with what was missing named. That is
seen evidence, which is what `FAIL` needs. Without a stated expectation, the same
observation is reported as `ERROR` with the difference shown, and the profile's author decides.

### Verdict rules

Every verdict follows the [rules that keep verdicts trustworthy](../AGENTS.md#rules-that-keep-verdicts-trustworthy).
`FAIL` needs something lspcheck saw; lspcheck's budgets never make a `FAIL`; a request lspcheck couldn't deliver is
`ERROR`; every `PASS` and `N/E` that relied on nothing being missed in a window goes through `report_judged` and the
end-of-scenario recheck, and every missing reply through `fail_absent_between`.

**M1.**
- *skip:* `--mutate` not given, no seed file, no `didOpen`, or no change sync.
- *fail:* the process exited, or lspcheck read end of file on its stdout, once the first mutation's `didChange` was
  fully written and before lspcheck sent `shutdown`. Only an exit or an end of file lspcheck saw counts: today's session API reports
  both end of file and lspcheck stopping its own reading (output over `--max-output-mib`) as `Recv::Closed`, so it must
  tell the two apart first, and stopped inspection makes M1 `ERROR`, never `FAIL`. The detail gives the exit status, the case and step, its operators and their offsets. After a write
  lspcheck couldn't finish, it waits up to `--exit-timeout` for an exit and checks for closed output before judging.
- *error:* the server exited or closed output before the first mutation was fully written (during settle or baseline,
  which says nothing about broken source); the first mutation write failed; `initialize` failed; a later `didChange`
  couldn't be delivered and neither an exit nor closed output was seen.
- *not-exercised:* no mutation was sent, for example because the remaining run budget couldn't admit a case.
- *pass:* otherwise, with the number of cases and versions sent.

**M2.** Judges every delivered request of the scenario after the first mutation: the step requests and the recovery
polls.
- *fail:* a delivered request had no response by its deadline (`--timeout` from when its write began). Each goes
  through `fail_absent_between`, so missed evidence in its window makes it `ERROR`. A request still pending when the
  server exited is M1's, not M2's.
- *error:* a request couldn't be delivered; or a request's wait was cut short by a budget of lspcheck's (see Bounds),
  which can only happen when lspcheck itself is slow.
- *not-exercised:* no request was delivered after a mutation.
- *pass:* every delivered request got a response. Any error is an answer, including ContentModified (-32801),
  ServerCancelled (-32802) and RequestFailed (-32803); the detail counts them by code.

**M3.**
- *skip:* as M1.
- *not-exercised:* no probe had a stable baseline (each one's reason is listed), or no case reached its restoration.
- *fail:* only as stated above, with `expect` or `no_errors`. The detail names the case, the probe, the first
  difference after projection (a path and both values, shortened like K2's) and how long lspcheck polled. Unlike a
  position inside a character, this failure rests on nothing having superseded the last answer, so missed evidence
  after it, or polling stopped by the poll budget before the bound, makes it `ERROR`, never `FAIL`.
- *error:* without a stated expectation, a probe didn't return to its baseline within `recovery_timeout`
  ("not seen to recover within 20 s; lspcheck's bound, not a protocol rule: set `[mutation.recovery].expect` if the
  answers are fixed"); the restoring `didChange` couldn't be delivered; the server exited first (M1's failure); a
  stated expectation was contradicted at baseline; or nothing failed but evidence was missed in a recovery window.
- *pass:* every case that reached restoration recovered. The detail gives the cases, the probes used and the slowest
  recovery time.

**Extended checks.** In the mutation scenario, D1 judges positions about the seed documents from their `didOpen` to
`shutdown`, with D1's candidate rule: a reply refers to the version current when its request was sent or a later one
sent before it was read; a labelled publish to its version; an unlabelled one to any version sent before it was read.
lspcheck keeps the last 64 versions of each document. A position whose candidate texts reach further back isn't judged
and is counted, so a long run's memory stays bounded and no position is judged against a text lspcheck no longer has.
D2 judges every labelled publish against the versions sent before it was read; D6 every `selectionRange` reply; K1
every MethodNotFound to an advertised request about a seed document.

Two of the existing judges need changes before they can be reused here, because the documents scenario sends few
versions and evicts none:

- **D6** clamps positions against the text current when the request was sent (`judge_selection_reply`). A reply may
  lawfully describe a later text: a request names column 9 of a ten-character line, a later edit shortens it to three,
  and a range ending at column 3 contains the position as clamped against that later text. In the mutation scenario D6
  uses D1's candidate rule: an entry or parent fails only when containment fails against every candidate text, each
  with its own clamping. A reply whose candidates include an evicted text has its containment left unjudged, and
  counted; its entry count is still judged, since one entry per position doesn't depend on the text.
- **D2** takes the versions it accepts from the documents it keeps (`OpenDoc.versions`). Evicting texts must not evict
  versions, or a lawful publish labelled with an old version would fail. lspcheck keeps every version number it sent,
  with when its `didChange` was written, for the whole session (a few bytes each, at most `cases × (steps + 1) + 1`
  per document); only the text snapshots are bounded to 64.

### Profile schema

`[mutation]` is validated with the rest of the profile (`deny_unknown_fields`, errors name the key, exit status 2),
before any server starts. It is inert without `--mutate`, so a profile can carry it without changing default runs.

```toml
[mutation]
files = ["main.lua"]           # seed files the workspace provides; default: [document].path
language_id = "lua"            # their didOpen language id; default: [document].language_id
seed = 20261109                # default: chosen at random and reported
cases = 20                     # default 20, at most 1000
steps = 3                      # most steps per case, 1 to 3
operators = ["delete-tokens", "unbalance-bracket", "open-string", "unicode", "line-ends", "rename"]  # default: all but nest, repeat-line, control
fragments = ["0x", "1e", "..."]                  # for the fragment operator
requests = ["textDocument/documentSymbol", "textDocument/hover", "textDocument/completion"]  # default: K1's advertised methods
requests_per_step = 4          # at most 16
max_document_kib = 64          # per document, seed included; at most 1024
run_timeout = "1h"             # at most "4h"

[mutation.lexicon]             # optional; see Mutations for the default
line_comments = ["--"]
block_comments = [["--[[", "]]"]]
strings = [{ open = '"', close = '"', escape = '\' }, { open = "'", close = "'", escape = '\' }, { open = "[[", close = "]]" }]
identifier_chars = "_"
brackets = ["()", "[]", "{}"]
keywords = ["local", "function", "end", "return", "if", "then", "else"]

[mutation.recovery]
probes = ["textDocument/documentSymbol", "publishDiagnostics"]   # default: every advertised probe
settle_timeout = "30s"         # default: the startup timeout, at most "5m"
recovery_timeout = "20s"       # default: twice the request timeout
expect = "observe"             # or "baseline"
no_errors = true               # default false
```

That example, added to [`lua-language-server.toml`](../examples/profiles/lua-language-server.toml), mutates `main.lua`.
Validation refuses: a seed file the workspace doesn't provide or that isn't UTF-8; a seed larger than
`max_document_kib`; no seed file and no `[document]`; an unknown operator, probe or method; a `requests` method that
isn't in K1's table; `steps`, `cases` or `requests_per_step` out of range; an empty delimiter; a bracket entry that
isn't two characters; a delimiter or identifier character that is a line end; `expect` other than the two values.
It also refuses: an empty list where one is given (`files`, `operators`, `requests`, `recovery.probes`), an entry
listed twice in any of them, `max_document_kib` outside 1 to 1024, a negative `seed`, a `run_timeout` over `"4h"`
or `settle_timeout` over `"5m"`, an empty fragment or one longer than the growth bound (1 KiB), an empty keyword, a
string escape that isn't one character, and seed files without a language id (`language_id`, else `[document]`'s).
A lexicon key left out keeps the default lexicon's value, so `keywords = [...]` alone adds keywords to the default;
a key that is given replaces the default's value, so `identifier_chars = "-"` drops `_` (write `"-_"` to keep it).
`requests` are asked in K1's order whatever the profile's. Durations use the [profile syntax](../README.md#profiles).
A seed chosen at random is below 2^53, so it survives any JSON reader and fits a TOML integer.

`--mutation-seed N` and `--mutation-cases N` require `--mutate` and override the profile's values. `--mutate` resolves
the `[mutation]` settings; with a `[document]` but no `[mutation]`, it selects that file with the default lexicon and
defaults (its size is checked against the default 64 KiB then, not when the profile is read). With no seed file it
resolves an empty seed list; once the scenario is implemented, that will make M1 to M3 `SKIP` ("no seed file").

### Bounds

| Bound | Default | Cap | When it is reached |
|---|---|---|---|
| Per request | `--timeout` from when its write began | — | the request is unanswered (M2) |
| Per case | `(steps + 1) × (1 + requests_per_step) × --timeout` (every write and every reply of the steps, each with its own deadline) `+ documents × --timeout` (the restoration's writes) `+ recovery_timeout + documents × probes × 2 × --timeout` (each terminal probe's write and reply) `+ 5 s` | — | no further step starts; the restoration is always sent |
| Per run | `run_timeout`, 1 h | 4 h | no new case starts; the detail says how many ran |
| Settle and baseline | `settle_timeout` | 5 min | proceed; unstable probes aren't used |
| Cases | 20 | 1000 | usage error above the cap |
| Mutations per step; steps per case | 1 to 3; `steps` | 3; 3 | — |
| Growth per mutation (bytes inserted beyond those removed) | 1 KiB; stress operators the room left | 16 KiB | the operator inserts less |
| Document size | `max_document_kib`, 64 | 1024 | the operator is redrawn, `truncate` after six tries |
| Requests per step; recovery polls per case | 4; 40 | 16; 40 | the poll stops: recovery not seen, M3 `ERROR` for that case even with an expectation |
| Versions kept per document | 64 | 64 | older positions aren't judged, counted |
| Bytes written to the server | 64 MiB | 256 MiB | no new case starts unless its worst case fits (below) |

In the case budget, `probes` counts only request probes: pushed diagnostics need no terminal request. Before the
server starts, an omitted `recovery.probes` counts all three request probes, regardless of what it may advertise.

A case starts only when the run has at least the case budget left, so the run deadline never cuts a case short, and a
request's own deadline is never shortened by a budget except when lspcheck itself overran. The case budget counts
every delivery as taking its full write deadline, so a server slow to read (backpressure) uses up deadlines the budget
already reserved, not the run's. The budget is a worst case: with the example profiles' 30 s request timeout it is
over ten minutes per case, though a case usually takes seconds, which is why `run_timeout` defaults to an hour. A
configuration whose case budget exceeds `run_timeout` is a usage error naming both (exit status 2), checked under
`--mutate` when there is a seed file, before the server starts, so a run never silently admits no case. The planned
stock mutation profiles will each be checked by a test. If a case's budget runs out anyway, no further step starts,
but the restoration is still sent, with its own write deadline; if it can't be delivered, the scenario ends there (`ERROR` for M3 and the
cases not run), so no case ever starts on a document lspcheck couldn't restore.

The written-bytes bound is a cap, not a threshold: everything lspcheck writes in the scenario counts (the seed
`didOpen`s, baseline probes, every `didChange`, request, poll and answer to a server request, and `shutdown` and
`exit`), and before a case starts lspcheck reserves its worst case: the case's `didChange` notifications at their
exact serialized size, which the plan fixes before the case starts (a step's notification can carry three changes,
each up to a whole document), plus, computed from the caps, its requests, the restoration, the most polls and the
terminal probes, and the shutdown allowance. A case whose worst case doesn't fit doesn't start. Answers to the server's own requests (such as
`workspace/configuration`, which a profile can make large) can't be bounded in advance, so the cap is also enforced
before every write: answers may use only what isn't reserved for the current case's restoration and the shutdown.
When that is gone, lspcheck stops the scenario: it sends the restoration and shutdown from their reserve, leaves the
server's request unanswered, and M3 and the cases not run are `ERROR` (lspcheck's budget, never a failure). Server output stays under
`--max-output-mib` and `--max-frame-mib`; missing what they exclude makes waits inconclusive as everywhere else. With
the bytes written capped, a transcript holds at most that plus what the server wrote. None of these bounds is a
protocol rule: reaching one gives `ERROR` or `N/E` with the bound named, or just a smaller run, never `FAIL`.

These are the complete design's bounds; [step 3](#step-3) currently leaves answers to server requests uncapped
throughout the scenario. Enforcement in the session's send path is planned for step 4.

### Reports, transcripts and reproduction

The JSON report gains a top-level `"mutation"`: `null` when the scenario didn't run, or the seed, the format, the
cases planned and run, and why the run stopped early, if it did. The terminal header adds
`— mutation seed 20261109, 20 cases`. The schema id is unchanged, as when `"profile"` was added.

The scenario's transcript, `mutation.jsonl`, is an ordinary session transcript, so `lspcheck replay mutation.jsonl --
SERVER` replays it as it is: the workspace line recreates the seed files, and every `didChange`, request and poll is
re-sent byte for byte. Before each case lspcheck records a meta line, `{"dir":"meta","mutation":{"format":1,"seed":…,
"case":17,"steps":[{"document":"main.lua","version":5,"edits":[{"operator":"open-string","offset":40,"removed":1,
"inserted":"","sha256":…}]}]}}`, with offsets and lengths in UTF-8 bytes of the text before the edit; replay ignores
meta lines it doesn't know, so no change to replay is needed.
Each edit records the SHA-256 of its inserted text, with `fallback: true` when its operator found nothing to change.
Inserted text up to 64 characters is recorded in `inserted`; longer text uses its UTF-8 byte count in `inserted_bytes`
and its first 64 characters in `inserted_prefix`. The full text is in the recorded `didChange`.

A whole session can be long, so a later step adds **isolation**: after the run, for each of the first three failing
or erroring cases, lspcheck runs that case alone in a fresh session (open, settle, baseline, the case's steps, restore,
probe), from the same seed, and writes `mutation-case-17.jsonl`. The report says whether the failure reproduced. An
isolation run judges nothing, like `replay`: the verdicts come from the main run alone, so they can't depend on
whether a flaky reproduction worked. **Minimisation** builds on isolation and is off by default: drop steps, then
mutations, then requests, keeping an attempt only when the same check fails the same way, at most 16 fresh sessions
in all, each with the case budget, inside the run deadline. Its result is a transcript and a line of detail, never a
verdict.

### Stand-in faults and tests

The stand-in gains bracket tracking: it counts unmatched `()[]{}` in each open document, publishes a diagnostic per
unmatched bracket, and answers `documentSymbol` from its words as now. A test profile in `tests/fixtures/` holds a
seed with brackets, strings, a comment and non-ASCII text, and uses only `unbalance-bracket` with a fixed seed, so every
case breaks the balance and restoring it repairs it. The faults react to the text, never to time, so no test depends on
a window. Lawful faults count replies, not milliseconds.

| Fault | Effect | Test |
|---|---|---|
| `exit-on-unbalanced` | exit when a document's brackets become unbalanced | M1 fails |
| `flood-while-unbalanced` | lawful: write diagnostics past `--max-output-mib` while unbalanced, and keep serving | M1 `ERROR` (inspection stopped), never `FAIL` |
| `close-output-mid-restoration` | partway through reading a large document's restoration, send 20,000 requests and close stdout, then read nothing more after it | M1 `ERROR` (the end of output is queued behind requests lspcheck can't answer before shutdown), never `PASS` |
| `late-wrong-count` | answer one `selectionRange` with `[]` after more than 64 later versions | D6 fails from the mutation scenario |
| `recover-before-deadline` | lawful: after rebalancing, answer `documentSymbol` with `[]` until just before `recovery_timeout`, counted in replies to a poll schedule the test fixes, then with the baseline | nothing fails with `expect = "baseline"`: the terminal probe sees the baseline |
| `ignore-while-unbalanced` | don't answer requests about a document while it is unbalanced | M2 fails |
| `stuck-after-unbalanced` | once a document was unbalanced, answer its `documentSymbol` with `[]` forever | M3 `ERROR` without `expect`; fails with `expect = "baseline"` |
| `recovery-in-skipped-frame` | after rebalancing, answer the first poll with `[]`, then send the recovered answer in a frame larger than `--max-frame-mib` | M3 `ERROR` with `expect = "baseline"`, never `FAIL` |
| `errors-after-unbalanced` | once unbalanced, publish an Error diagnostic forever | M3 fails with `no_errors = true` |
| `warning-after-unbalanced` | once unbalanced, publish an extra Warning forever | M3 fails with `expect = "baseline"` and pushed diagnostics as the only probe; passes with `no_errors = true` alone |
| `method-not-found-while-unbalanced` | answer advertised requests with -32601 while unbalanced | K1 fails from the mutation scenario |
| `codepoint-positions-after-change` (exists) | count positions in code points after a change | D1 fails from the mutation scenario (with `unicode`) |
| `diagnostics-own-version` (exists) | label diagnostics with its own counter | D2 fails from the mutation scenario |
| `slow-recovery` | lawful: answer the first three `documentSymbol` after rebalancing with `[]` | nothing fails; M3 passes |
| `reworded-diagnostics` | lawful: put a counter in every diagnostic message | nothing fails |
| `no-diagnostics` (exists) | lawful: never publish diagnostics | nothing fails; pushed diagnostics unused |
| `content-modified-while-unbalanced` | lawful: answer requests with -32801 while unbalanced | nothing fails; M2 passes |
| `shuffled-symbols` | lawful: return symbols, and each symbol's children, in a different order each time, with a changing `detail` | nothing fails |
| `unstable-symbols` | lawful: add a symbol named by a counter to every answer | M3 `N/E`, never `FAIL` |

The clean stand-in passes M1 to M3 and every extended check. Unit tests cover the planner: the same seed gives the same
plan bytes; a pinned golden case; no edit inside a character or a CRLF; sizes within the caps; incremental changes that
`apply_change` turns into the planned text in all three encodings, over many seeds; the default and a given lexicon;
every profile validation error; line-end changes CR LF → CR, CR → CR LF, LF → CR LF and CR LF → LF, whose
minimal diffs would split a CR LF without the widening. Each regression test is run against the commit before its fix, as AGENTS.md asks.

### Stock servers

The matrix would add one job per server: the same example profile, with its `[mutation]` section, run with
`--mutate --mutation-seed <pinned> --mutation-cases 10`, compared against `<server>.mutation.verdicts`. Default runs and
their `.verdicts` files gain only the [disabled-mutation `SKIP`s](#what-is-checked).

Mutation brings timing into play, and the README's rule still holds: alternatives are only for races in the server
itself. Known risks:

- **Background loading.** rust-analyzer loads the sysroot and gopls the module graph after `initialize`. A baseline
  taken mid-load is unstable (the probe isn't used) or, worse, stable but early. Settling on work-done progress is the
  defence; if a server reports no progress, a longer `settle_timeout` in its profile, never an alternative verdict.
- **Stale diagnostics.** lua-language-server sometimes labels the previous text's diagnostics with the new version
  (its `D1 fail|pass` and `D3 fail|pass` lines). With dozens of versions this is likelier, and the same race can fail
  the extended D1 or, after a restoration, M3. That is a server race, allowed as an alternative only once a transcript
  shows it.
- **Slow answers on broken source.** M2 fails on a reply later than `--timeout`. The profiles' 30 s keeps that rare;
  a server that sometimes answers in 31 s is a server race, recorded with its transcript.
- **lspcheck's own timing.** The recovery poll interval and the settle quiet period are lspcheck's choices. A verdict
  that flips because of them is a bug in lspcheck, fixed there, not listed.

Run each server's job a few times before recording, as the README asks. The workflow change needs the maintainer's
merge, and any finding about these servers stays in this repository unless the maintainer decides otherwise.

## Consequences

- Real source supplies evidence for D1, D2 and D6; K2 also sees replies and diagnostics across many mutated versions.
- A `--mutate` run takes minutes, not seconds; it is opt-in, and default runs are unchanged except for three `SKIP`
  lines, in the terminal, JSON, JUnit and Markdown reports.
- M3's `ERROR` for a recovery not seen makes a `--mutate` run exit 2 until the profile states an expectation or
  skips M3. That is deliberate: lspcheck doesn't know, and says so.
- The planner's generator and draw order become a format lspcheck must version: changing them changes what a seed
  means, so the format number goes up.
- No new dependency, no network, no change to `replay`.

## Implementation plan

Each step is one small pull request with its tests, docs and changelog entry where user-visible.

1. **Planner.** `src/mutation.rs`: SplitMix64, the default lexicon and the profile lexicon, the operators, the plan
   as a pure function, minimal-range incremental changes checked with `text::apply_change`. Unit tests only; nothing
   runs it yet.
2. **Profile and flags.** `[mutation]` parsing and validation, `--mutate`, `--mutation-seed`, `--mutation-cases`; M1 to
   M3 in the catalogue, reporting `SKIP` without `--mutate`. Stock `.verdicts` files gain their `skip` lines.
3. **Scenario, M1 and M2.** Open, mutate, request, restore (without probes), `leave()`; the transcript meta line; the
   JSON `"mutation"` field. Stand-in bracket tracking and the M1 and M2 faults.
4. **Extended checks.** D1, D2, D6 and K1 over the mutation scenario, with the 64-text window, D2's whole-session
   version history, D6's candidate texts, and their faults (including a lawful delayed `selectionRange` reply about a
   later, shorter text, and a lawful publish labelled with an evicted version).
5. **Recovery, M3.** Settle, stable baselines, projections, polling, `expect` and `no_errors`; the recovery faults,
   lawful ones included.
6. **Isolation.** Rerunning failing cases alone and writing their transcripts.
7. **Stock matrix.** `[mutation]` in the example profiles, a mutation job and `.mutation.verdicts` (a workflow change,
   so the maintainer merges it).
8. **Minimisation**, if isolation shows it is wanted.

### Step 3

Step 3 runs the scenario without the parts later steps add, and says so where it matters:

- No settle or baseline (step 5): the cases start right after the seed documents are opened. M3 is `SKIP` ("lspcheck
  doesn't judge recovery yet").
- Before sending `initialize`, lspcheck checks its exact serialized request (including the profile's
  `initializationOptions`), `initialized`, optional `workspace/didChangeConfiguration`, and 1 KiB for shutdown against
  the 64 MiB cap. If they do not fit, M1 and M2 are `ERROR` and the scenario writes nothing.
- Before opening any seed file, lspcheck checks that all serialized `didOpen` notifications, bytes already written,
  and 1 KiB for shutdown fit the 64 MiB cap; otherwise M1 and M2 are `ERROR` and no seed is opened.
- The case budget and `run_timeout` are as above; planning, serialization and hashing consume the admitted case's
  budget. Before a case starts, lspcheck checks that the bytes it has written
  so far plus the case's reserve fit the written-bytes cap; the reserve counts the case's `didChange` notifications at
  their exact size, 2 KiB plus twice the URI's length for each request, and 1 KiB for `shutdown` and `exit`. For each
  document, the restoration reserve is the largest restoration notification over every step's resulting text and
  version, plus a small margin, even if the plan ends with the clean text. Answers to the server's own requests
  anywhere in the mutation scenario (startup, cases, early leave and shutdown) aren't yet held to the 64 MiB cap
  before each write; they count towards the next case's check, if any. Enforcing the cap on answers once, in the
  session's send path, is planned for step 4.
- Each request's deadline is `--timeout` from when lspcheck began writing it; the step's requests are written back to
  back and then waited for in order, each until its own deadline. Answering queued server requests happens before
  that window starts. Before later writes, after each wait and before classifying a failed write, lspcheck absorbs
  queued output and checks the process and its output. A complete observation that finds the server running with
  output open at or after a request's deadline establishes its timeout when no in-time reply was received. That
  timeout remains M2's if a later request in the batch sees an exit, closed output or incomplete inspection. At batch
  settlement, replies are judged by receive timestamp: an in-time reply answers the request; a late reply is
  unanswered. Without an established timeout or any reply, a request when exit or closed output is observed belongs
  to M1; observing an exit after a deadline alone does not prove the server survived that deadline. A timeout settled
  in an earlier step also keeps its outcome if the server exits or answers later.
- Absorption is bounded by the write timeout. If it cannot drain the queued output, every request without an
  in-time answer or previously established timeout is `ERROR`. Each absence window ends at that request's deadline;
  an inspection stop after that deadline cannot excuse its timeout. The end-of-session recheck still includes frames
  spanning the deadline.
- M1's window starts once the first mutation's `didChange` is fully written and ends before shutdown. An exit or
  closed output before that boundary, or a failed first mutation write, is `ERROR`. A failed later write with observed
  closed output counts as server loss even if the process remains alive. A stop in lspcheck's inspection remains
  `ERROR`, including when a write fails and the server exits.
  At the shutdown boundary, new exit or EOF evidence supersedes a recorded delivery failure, preserving its location;
  an inspection stop supersedes either classification. Before that boundary lspcheck takes in everything it has
  already read, within the write timeout, since an end of output may be queued behind it. If it can't (answers to the
  server's requests that the server doesn't read can block it), M1 is `ERROR` unless a loss was already seen.
- A request K1 wouldn't send about the document (a `documentSelector` that doesn't select it) is skipped, and the
  round robin moves on.
- Case plans use the [transcript format above](#reports-transcripts-and-reproduction).

## Decisions

The proposal left these questions open; the maintainer settled them on 2026-11-11.

- **A recovery not seen, without a stated expectation, is `ERROR`, not `N/E`.** lspcheck's verdicts define `ERROR` as
  "the result is unknown: … a timeout ran out", which is exactly this case, and `N/E` as a run that produced no
  evidence, which this one did. `N/E` would also hide a server that never recovers. A server whose answers lawfully
  drift exits 2 under `--mutate` until its profile states an expectation, narrows `recovery.probes`, or skips M3 with
  a reason; the detail says which.
- **M1 to M3 are listed as `SKIP` without `--mutate`.** Every report and verdicts file keeps the whole catalogue, as it
  does for checks that don't apply to a server, and shows what wasn't run. `SKIP` doesn't affect the exit status.
- **D1, D2, D6 and K1 are extended, as proposed, not duplicated as new checks.** A check names a rule, and the rule
  doesn't change with the text; the cross-cutting checks already gather evidence from every scenario. The detail names
  the scenario and case, so a failure found only under `--mutate` is clear, and default runs (and the stock baselines)
  don't change.
- **`--strict` covers the M checks** like every other `POLICY` check: no exception to explain.
- **The default case count stays 20** for now. Step 7 measures it on the stock servers and sets the default (and the
  matrix's count) from those timings before a release ships `--mutate`.
- **No lexicon presets yet.** Profiles spell out their lexicon. If the stock profiles in step 7 repeat the same tables,
  presets can be added then, as data.
- **Servers that take no changes stay skipped.** Mutating them by `didClose` and `didOpen` would test a different thing
  (reopening, not editing), and none of the servers tried so far needs it.
- **`semanticTokens/full` isn't a recovery probe** in the first version. Comparing it through its legend can come later
  if the other probes prove too few.
