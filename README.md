# lspcheck

lspcheck starts a language server and checks that it speaks the
[Language Server Protocol](https://microsoft.github.io/language-server-protocol/) correctly. It checks message
framing, JSON-RPC replies, the initialize/shutdown/exit lifecycle, document positions, request parameter handling,
malformed JSON and large-document robustness, unusual request ids, whether what the server sends has the types the LSP
meta-model gives it, and whether the server leaves processes behind. It
works with any server that talks over stdio, whatever language it is written in, and needs no configuration; an
optional [profile](#profiles) gives a server the project files and settings it needs.

Example output:

```console
$ lspcheck -- basedpyright-langserver --stdio
lspcheck 0.4.0 — basedpyright-langserver --stdio
  PASS  F1  MUST    Every byte on stdout belongs to a well-formed frame
  PASS  F2  MUST    Content-Length counts the body's bytes, not its characters
  PASS  F3  MUST    Input split across writes, down to single header bytes, is read
  PASS  F4  MUST    Several frames in one write are all read
  PASS  F5  MUST    Input with 2-, 3- and 4-byte UTF-8 characters keeps the stream in step
  PASS  F6  MUST    A Content-Type header with charset=utf-8 from the client is accepted
  PASS  F7  SHOULD  The legacy charset=utf8 spelling from the client is accepted
  PASS  F8  SHOULD  A Content-Type the server sends declares UTF-8
  PASS  J1  MUST    A reply keeps its request's id: a number stays a number, exactly
  PASS  J2  MUST    Exactly one response per request: none duplicated or unasked
  PASS  J3  MUST    Notifications get no response, including unknown and $/ ones
                     no response answered a notification
  PASS  J4  MUST    An unknown request gets MethodNotFound (-32601), including a $/ request
  PASS  J5  MUST    Envelopes are well formed: jsonrpc 2.0, one of result/error, an integer code
  PASS  J6  MUST    Concurrent requests are each answered with the right id
  PASS  J7  MUST    The server's own requests don't reuse an unanswered id, and it keeps serving after they are answered
                     24 server request(s), no unanswered id reused; the server answered lspcheck's later requests after 20 of lspcheck's answers
  FAIL  L1  SHOULD  Before initialize, a request gets ServerNotInitialized (-32002) and a notification no reply
                     got result "[]" instead of ServerNotInitialized
  PASS  L2  MUST    initialize returns capabilities and an offered position encoding
  FAIL  L3  POLICY  A second initialize gets an error and the server keeps serving
                     a second initialize got result "{\"capabilities\":{\"textDocumentSync\":{\"willSave\":true,\"change\":2,\"openClose\":true"… instead of an error
  PASS  L4  MUST    shutdown returns null and the server keeps running until exit
  FAIL  L5  SHOULD  A request after shutdown gets InvalidRequest (-32600)
                     a request after shutdown got result "[]" instead of InvalidRequest
  PASS  L6  SHOULD  exit after shutdown ends the process with status 0
  PASS  L7  SHOULD  exit without shutdown ends the process with status 1
  PASS  L8  POLICY  End of input without exit ends the process
  PASS  L9  POLICY  No descendant process outlives the server
  PASS  C1  MUST    A cancelled request still gets exactly one response
                     workspace/symbol (the server advertises workspaceSymbolProvider): answered with a result, and only once
  PASS  C2  MUST    Cancelling an unknown or finished request produces nothing
                     cancelling an id never sent and a finished request drew no reply, and the server kept serving
  PASS  C3  MUST    No response to $/cancelRequest itself
                     no response answered a $/cancelRequest notification
  PASS  D1  MUST    Positions fall on character boundaries of the text, in the negotiated encoding
                     12 informative position(s) on character boundaries, counted in UTF-16 code units; 58 position(s) seen in the opened document, 0 past the end of a line, 0 on a line it doesn't have, 0 in other documents
  PASS  D2  MUST    A diagnostics version is one the document had when the notification was read
                     3 versioned diagnostics notification(s) named a version lspcheck had sent
  PASS  D3  MUST    After a didChange, positions refer to the changed text
                     5 informative position(s) on character boundaries after the change, counted in UTF-16 code units; 14 position(s) read after the change
  PASS  D4  POLICY  A request about a document that isn't open gets a response, and the server keeps serving
                     all three answered: the closed document got result (empty); the never-opened document got result (empty); the plain request got error -32601
  PASS  D5  MUST    Lines are split at \n, \r\n and \r
                     3 informative position(s) on character boundaries about the line-ending document, counted in UTF-16 code units; 32 position(s) about it
  SKIP  D6  MUST    A selectionRange result has one entry per position, each range contains its position, and each parent contains its child
                     the server advertises no selectionRangeProvider, so lspcheck sends no selectionRange request
  PASS  K1  MUST    An advertised request, sent with valid parameters about an open document, isn't answered with MethodNotFound (-32601)
                     34 request(s) for 16 advertised method(s) were answered, none with MethodNotFound
  PASS  K2  MUST    What the server sends validates against the LSP 3.17 meta-model: results, and its requests' and notifications' params
                     74 result(s), 49 server request(s) and 115 notification(s) were of their types
  PASS  R1  POLICY  A well-framed body that isn't JSON gets ParseError (-32700) with id null or is ignored, and the server keeps serving
                     the server returned 1 ParseError (-32700) response(s) with id null and answered a later plain request
  FAIL  R2  POLICY  A request with parameters of the wrong types gets an error response (InvalidParams preferred), and the server keeps serving
                     textDocument/hover returned a result for parameters of the wrong types: "null"
  PASS  R3  POLICY  A large document (default 4 MiB, configurable) is accepted or refused without the server failing
                     answered a plain request after 0.012 s following didOpen of a 4 MiB document
  PASS  R4  POLICY  Unusual but valid request ids round-trip exactly: 0, negative, the 32-bit extremes, an empty string, a long string, a non-ASCII string
                     all 8 requests got responses with their exact ids
  SKIP  M1  POLICY  The server doesn't exit, or close its output, while its source is mutated or being restored
                     mutation is off; pass --mutate
  SKIP  M2  POLICY  Every request about a mutated document gets a response (any result or error) within --timeout
                     mutation is off; pass --mutate
  SKIP  M3  POLICY  Once the clean text is restored, the server's answers about it return to those it gave before the mutations
                     mutation is off; pass --mutate
42 checks: 34 pass, 4 fail (0 MUST, 2 SHOULD, 2 POLICY), 4 skipped, 0 not exercised, 0 errors
```

lspcheck is early: its checks cover framing, JSON-RPC correlation, the lifecycle, cancellation, document positions,
advertised capabilities, message types, and parameter, malformed-body, large-document and request-id robustness.
The [capabilities and robustness reference](docs/design.md#capabilities-and-robustness) explains the K1, K2 and R
checks' verdict rules. M1 to M3 belong to [seeded source mutation](docs/source-mutations.md), which is being
built: without `--mutate` they are `SKIP`, which doesn't affect the exit status.
[The design](docs/design.md) lists the rest, which cover framing edge cases, request ids, documents and robustness.

## What it doesn't do

lspcheck doesn't certify a server or award a badge. It doesn't judge whether answers are *right* (whether a hover is
accurate, or a definition points to the right place), because that needs knowledge of the language. It isn't a fuzzer
and doesn't test editors or clients. Every result says which rule it checks and how strong that rule is.

## Install

Download the archive for your platform from the [releases](https://github.com/SaltedTan/lspcheck/releases), check it
against `SHA256SUMS`, and put `lspcheck` on your `PATH`. The Linux binaries are static, so they run on any
distribution. On Linux or macOS:

```sh
# The subshell stops at the first failure, such as a checksum mismatch, without closing your shell.
(
  set -eu
  v=0.4.0 target=x86_64-unknown-linux-musl   # or aarch64-unknown-linux-musl, x86_64-apple-darwin,
                                             # aarch64-apple-darwin
  base=https://github.com/SaltedTan/lspcheck/releases/download/v$v
  curl -fsSLO "$base/lspcheck-$v-$target.tar.gz" -O "$base/SHA256SUMS"
  sha256sum --check --ignore-missing SHA256SUMS   # on macOS: shasum -a 256 --check --ignore-missing SHA256SUMS
  tar -xzf "lspcheck-$v-$target.tar.gz"
  "./lspcheck-$v-$target/lspcheck" --version
)
```

On Windows, download the `x86_64-pc-windows-msvc` ZIP archive, verify its SHA-256 digest against `SHA256SUMS`,
then extract it and put `lspcheck.exe` on your `PATH`.

Each archive also carries build provenance from the release workflow, which the GitHub CLI can check. Run this where
you downloaded the archive, substituting your platform's archive name:
`gh attestation verify lspcheck-0.4.0-x86_64-unknown-linux-musl.tar.gz --repo SaltedTan/lspcheck`.

To build from source instead, with Rust 1.99 or later:

```sh
cargo install --locked --git https://github.com/SaltedTan/lspcheck --tag v0.4.0
```

### In CI

Pin a version and its checksum, so the check only changes when you upgrade it. Checks are added and refined between
versions, and the [changelog](CHANGELOG.md) says which. The exit status fails the job when a `MUST` check fails
(`--strict` also fails it on `SHOULD` and `POLICY` failures). `--json` and `--transcripts` keep the evidence, and
`--junit` and `--markdown` feed CI test tabs and the GitHub step summary:

```yaml
- name: Check the language server with lspcheck
  env:
    LSPCHECK_VERSION: 0.4.0
    LSPCHECK_SHA256: <the archive's 64-character SHA-256 digest from SHA256SUMS>
  run: |
    name=lspcheck-$LSPCHECK_VERSION-x86_64-unknown-linux-musl
    curl -fsSLO "https://github.com/SaltedTan/lspcheck/releases/download/v$LSPCHECK_VERSION/$name.tar.gz"
    echo "$LSPCHECK_SHA256  $name.tar.gz" | sha256sum --check
    tar -xzf "$name.tar.gz"
    "$name/lspcheck" --json lspcheck.json --junit lspcheck.xml --markdown "$GITHUB_STEP_SUMMARY" \
      --transcripts lspcheck-transcripts -- ./build/my-server --stdio
```

## Use

```sh
lspcheck -- my-server --stdio            # run every check
lspcheck --only L4,L6 -- my-server       # run some checks
lspcheck --large-document-mib 8 -- my-server --stdio
lspcheck --json report.json --transcripts out/ -- my-server
lspcheck --junit report.xml --markdown summary.md -- my-server
lspcheck --profile gopls.toml            # with a profile that names the server
lspcheck list                            # every check, with its specification section
```

`--json` writes a versioned machine-readable report, `--junit` a JUnit XML report for CI test tabs,
and `--markdown` a summary table for a CI step summary. Any combination may be given; these flags apply to checks,
not `replay`. Report files are overwritten; a write failure prints a diagnostic and exits with status 2.
With `--transcripts`, terminal, JUnit and Markdown reports point to the evidence for failed or errored checks.
See the [report format reference](docs/design.md#report) for verdict mappings, `--strict` in JUnit, escaping and
transcript references.

lspcheck runs the server several times, once per scenario, each time with a fresh temporary workspace as its working
directory: empty, or holding a [profile](#profiles)'s files. A server path with a directory part, such as `./my-server`,
is resolved before that; a bare name is looked up on `PATH`. The checks on the server's output (F1, F2, F8) and on
leftover processes (L9) are judged across every scenario, as are the JSON-RPC correlation checks J1, J2, J5 and J7, and
K2.

The JSON-RPC scenario writes four requests, with numeric and string ids, before reading any reply (J6), and then sends
unknown notifications followed by probes to show that a notification gets no response (J3). A reply whose id only
loosely matches the request's (a number spelled as a string, or one that lost precision as a double) fails J1, and a
duplicate, unasked or premature reply fails J2.

lspcheck answers the server's own requests as it reads them, including during
[exit waits](docs/design.md#lifecycle). J7 checks for unanswered id reuse and continued service
after those answers; the [design reference](docs/design.md#json-rpc-correlation) defines the judging rules and the
answers. A profile's `[client].configuration` answers `workspace/configuration`; without one, each item gets `null`.

The checks on how the server reads its input (F3 to F7) send small requests the server doesn't know, which every server
must answer, written in different ways: split across many writes, several frames in one write, with a `Content-Type`
header, or carrying multibyte characters. See the [framing reference](docs/design.md#framing-and-output) for scenario
isolation, plain-request baselines, and when a variant is `ERROR` or `N/E`.

F2 needs output with non-ASCII text, so lspcheck sends a request whose string id has some, which the response must
echo. A server that escapes all non-ASCII text as `\u` sequences (which is lawful) gives no evidence, and F2 reports
`N/E`.

D1 opens a plain-text document whose lines mix emoji, a combining mark and CJK characters, asks each document request
the server advertises (symbols, folding ranges, highlights, selection ranges and others), and reads the diagnostics it
publishes. Every position the server sends about the document must fall between characters in the position encoding
the two sides agreed (UTF-16 unless the server chose another): never between the halves of a UTF-16 surrogate pair or
inside a UTF-8 multibyte sequence. The document is built so that counting in the wrong units lands inside a
character, and a failure says which counting would have fit. A position past the end of a line isn't judged, because
the client clamps it. Most language servers ignore a plain-text document, and then D1 is `N/E`.

The same scenario also checks diagnostics versions (D2): when the server labels a `publishDiagnostics` for the opened
document with a `version`, it must be one lspcheck sent for it before the notification was read — so a server that
labels with its own counter, or with one the document never had, fails. An older version is lawful, a notification
without a version leaves D2 `N/E`, and a `version` that is `null` or not an integer isn't judged here.

The same scenario also checks positions after a change (D3): lspcheck sends a `didChange` the way the server's
textDocumentSync kind asks (an incremental edit that replaces a 4-byte character and adds a line, or the full text),
then asks the document requests again and reads the new diagnostics. Every position the server sends after the change
must refer to the changed text. A position that falls inside a character of the changed text fails D3 when no other
reading of the changed text explains it (counted in another encoding) and a text the change should have produced does
(the text before the change, or the change applied counting its range in another encoding); otherwise D3 is `ERROR`
and D1 carries the failure. A server that takes no changes leaves D3 `SKIP`.

The same scenario also checks documents that aren't open (D4): after the change, lspcheck closes document A with a
`didClose`, then sends one advertised document request about the closed document, the same request about a file in
the workspace that was never opened and doesn't exist, and a plain request, each awaited alone. Each must get a
response — any result or error is lawful, including one read from disk — and the server must keep serving. A server
that advertises none of the document requests leaves D4 `SKIP`.

The same scenario also checks line endings (D5): before closing A, lspcheck opens a second document, B, whose lines end
in a lone `\r`, `\r\n` and `\n`, with a word on two lines that a wrong split moves onto a line of emoji. A position
about B that falls inside a character fails D5 when no counting of the spec's lines explains it (in any encoding) and
a split the spec doesn't allow does (`\n` only, `\r\n` counted as two breaks, or no lone `\r`); otherwise D5 is
`ERROR` and D1 carries the failure.

The same scenario also checks selection ranges (D6): for each `selectionRange` request lspcheck sends about an open
document, the result must have one entry per position, each entry's `range` must contain its position, and each
`parent.range` must contain its child's range, all the way up the chain. Comparisons clamp each character to its
line's length in the negotiated encoding, as a client would; a range on a line the text doesn't have isn't judged,
and malformed entries are left to [K2](docs/design.md#capabilities-and-robustness). See the
[D6 reference](docs/design.md#documents-and-positions) for line-split hints in failure details.
A server that advertises no `selectionRangeProvider` leaves D6 `SKIP`; one that answers with only `null`, errors, or
no reply, or whose ranges can't be compared, leaves it `N/E`. A `selectionRange` request lspcheck couldn't send, or one
cut off because the server's output closed, makes D6 `ERROR` unless it already failed.

K1 checks that the server handles what it advertises: it opens a document (a profile's
[`[document]`](#profiles), or a plain-text one of lspcheck's own) and probes advertised methods without judging the
content of their results. See the [K1 reference](docs/design.md#capabilities-and-robustness) for request coverage,
MethodNotFound failures, late and duplicate replies, and details naming unanswered and undeliverable requests.

K2 checks server message types against the LSP 3.17 meta-model: results of known requests lspcheck sent, and params
of known server-to-client requests and notifications. A failure names where the value is (`result.contents`), what
the meta-model requires there and what was found. K2 accepts unknown values of the tag and kind enumerations clients
must tolerate. See the [K2 reference](docs/design.md#capabilities-and-robustness) for the enumerations covered,
validation rules and inspection limits.

R1 checks continued service after malformed JSON; R3 checks it after opening a large document.
If a follow-up request fails, both checks wait up to `--exit-timeout` for an unprompted server exit before judging it.
`--large-document-mib N` sets R3's document size (4 MiB by default, greater than zero required), and
`--startup-timeout` gives its follow-up request time to be answered. See the
[robustness reference](docs/design.md#capabilities-and-robustness) for response windows, verdicts and evidence rules.

With `--transcripts DIR`, lspcheck writes one JSON Lines file per scenario. It holds the bytes that reached the server's
stdin and the bytes read from its stdout, exactly as written and read, how lspcheck split them into frames, the first MiB of
stderr, and timestamps, so you can see what happened in a failing scenario. Each file includes a meta line naming the
workspace the scenario ran in and the files it held, so a replay can recreate it. A `stdin closed` meta event marks when
lspcheck closed the server's input. Events are in the order they began, keeping a fast server's reply after the write it
answers even when the client's pipe flush finishes later.

### Profiles

Many servers refuse to initialize, or check little, without a project or settings. A profile is a TOML file that
gives lspcheck what a server needs, and says what its maintainers accept. The sections and their top-level keys
are optional, except that `[document]` needs `path` and `language_id`; each `allow.skip` entry requires both `id`
and `reason`.
The example below uses supported fields; [planned profile fields](docs/design.md#profiles-optional-toml) are not
accepted yet.

```toml
[server]
command = ["gopls", "serve"]   # used when no command follows `--`
startup_timeout = "30s"        # like --startup-timeout
request_timeout = "10s"        # like --timeout
exit_timeout = "5s"            # like --exit-timeout

[workspace]                    # each scenario's fresh temporary workspace gets these files
path = "fixtures/go"           # a directory copied in, relative to this file
files = { "go.mod" = "module example\n\ngo 1.22\n" }   # written after copying `path`, over any same-named file

[client]
initialization_options = { hints = true }              # initializeParams.initializationOptions
configuration = { gopls = { staticcheck = false } }    # answers to workspace/configuration

[allow]
descendants = ["gopls-helper"]                         # process names allowed to outlive exit (L9)
skip = [{ id = "L7", reason = "exits 0 by design, see issue 12" }]

[document]                     # what K1 opens and asks about
path = "main.go"               # a file the workspace above provides, `/`-separated
language_id = "go"             # its didOpen languageId
positions = ["3:6", "10:2"]    # optional, at most 5: LINE:COLUMN from 1, COLUMN in characters (code points)
```

```sh
lspcheck --profile go.toml                    # the profile's command
lspcheck --profile go.toml -- ./gopls serve   # this command instead
lspcheck --profile go.toml --timeout 30 -- gopls
```

- **Precedence.** A command after `--` overrides `[server].command`; with neither, lspcheck stops with a usage error.
  A timeout flag on the command line overrides the profile's, which overrides the default (30 s to initialize, 10 s per
  request, 5 s to exit). Durations are strings: `"500ms"`, `"30s"`, `"2m"` or `"1h"`. `--only`, `--strict`, `--json`,
  `--transcripts` and the size limits are command-line only.
- **Paths** are relative to the profile file's directory: `[workspace].path`, and a `[server].command` program with a
  directory part (`bin/server`; a bare name is looked up on `PATH`). The fixture directory may hold only regular files
  and directories: symbolic links, including the fixture root itself, are refused. Workspace file and directory
  names must be portable relative paths with `/` between directories: empty, absolute, `.` and `..` parts,
  backslashes, control characters, `: < > " | ? *`, trailing dots or spaces, and Windows device names (`CON`, `PRN`,
  `AUX`, `NUL`, `COM1`–`COM9`, `LPT1`–`LPT9`, also with extensions) are refused. Paths must use consistent ASCII case
  in every component across fixture and inline entries; `src/A.json` with `SRC/b.json`, or `A.json` with `a.json`,
  is a usage error naming both paths. An inline file may replace a fixture file with the exact same spelling,
  including a read-only file; the replacement uses default file creation permissions. Missing directories are created.
- **lspcheck's own files.** lspcheck writes `doc.txt`, `crlf.txt`, `lspcheck-utf8.txt`, `lspcheck-large.txt` and
  `lspcheck-k1.txt` at the top of the workspace, and asks about `missing.txt` there, which must not exist. A profile can't put a file or directory at those names,
  compared ignoring ASCII case.
- **`[client]`.** `initialization_options` is converted using the [TOML-to-JSON rules](docs/design.md#profiles-optional-toml)
  and sent in every `initialize`, including L3's second one. When the `configuration` table is given, lspcheck answers
  each `workspace/configuration` item by looking up
  its `section` as a dot-separated path in it (`"gopls.staticcheck"` gives `false`; a missing path gives `null`; an item
  without a section gets the whole table), and sends it once in `workspace/didChangeConfiguration` right after
  `initialized`, for servers that read settings only that way.
- **`[allow]`.** L9 doesn't fail for a leftover process whose name, as lspcheck reports it, is in `descendants`; the
  `PASS` detail names what was allowed. (On Linux that name is the kernel's command name, at most 15 bytes.) A check in
  `skip` reports `SKIP` with the reason given, and doesn't affect the exit status. The
  [design reference](docs/design.md#profiles-optional-toml) explains when skipped checks let a scenario be omitted.
- **`[document]`.** K1 opens this file (at version 1, with the `language_id` given) instead of a plain-text document of
  lspcheck's own, and asks position requests at `positions`, each converted to the position encoding the server
  chose. `path` must name a file of `[workspace]` (from `path` or `files`) holding UTF-8 text; each position must be
  on a line the file has, at most one column past the line's last character. Without `positions`, K1
  [chooses positions from the document's text](docs/design.md#capabilities-and-robustness).
  See the [mutation reference](docs/source-mutations.md#profile-schema) for mutation seed selection.
- **`[mutation]`** configures [seeded source mutation](docs/source-mutations.md#profile-schema). The section is
  validated with the rest of the profile, and used only with `--mutate`, which is experimental and hidden from
  `--help` until recovery (M3) is judged: `lspcheck --profile p.toml --mutate [--mutation-seed N]
  [--mutation-cases N]` mutates the profile's seed files and judges M1 and M2. See the
  [mutation reference](docs/source-mutations.md#verdict-rules) for verdicts and
  [step 3's limits](docs/source-mutations.md#step-3) for what is implemented so far. The JSON `mutation.stopped`
  field records the reason and location when server loss stops the scenario.
- **Errors.** The profile is read and checked before any server starts. An unknown section or key, a value of the wrong
  type, a bad duration, a missing fixture, a symbolic link in it, an unsafe file name, an unknown check id, or a
  `[document]` whose file, language id or positions don't fit is a usage error (exit status 2) naming the file and
  the key, with TOML's line and column where it has them.

The [example profile catalogue](examples/profiles/README.md) lists the available servers and what each needs on
`PATH`. Each profile is self-contained, with a tiny project and a `[document]`:

```sh
lspcheck --profile examples/profiles/gopls.toml --only K1
```

The report names the profile: in the terminal's first line, and as `"profile"` in the JSON report (`null` without
one). Transcripts record the profile's files, including binary contents, directories (including empty ones), and
settings, so `lspcheck replay` recreates the workspace and answers `workspace/configuration` the same way. On Unix,
transcripts also record and restore file read, write and execute permissions, so fixture helpers remain executable.

### Reproducing a failure

`lspcheck replay` re-runs one recorded session against a server, without judging any check:

```sh
lspcheck replay out/lifecycle.jsonl -- my-server --stdio
```

It recreates the recorded workspace (refusing an existing non-empty one or recorded paths with inconsistent ASCII
case in any component), re-sends every client write in the transcript in order, and reports what the server did this
time: each request's outcome against the recorded one, the exit status, and how many server-to-client requests were
answered. Responses the server sends once lspcheck has begun
stopping it don't count as replies, in the recording or the replay. Gaps under 200 ms between writes are kept as pauses;
longer ones are sent back-to-back. Server-to-client requests are answered with the same auto-answers a live run uses,
including a profile's settings when the transcript recorded them, so they don't stall the replay. The replay's own transcript goes to `--transcript FILE`; it never overwrites an existing
file, so pass `--transcript` to choose another path or remove the existing output. The workspace is removed afterwards
only when replay created it; an existing empty workspace is kept. The exit status is 0 when the replay ran, 2 for usage
errors and transcripts that can't be read. Replay shows *what changed*; to judge checks on the server, run `lspcheck` on it.

lspcheck inspects at most `--max-output-mib` (256) of stdout per scenario and frames of at most `--max-frame-mib` (64).
These are lspcheck's own limits, not protocol rules: past them it keeps reading so the server isn't blocked, but stops
inspecting. A reply that never arrived is then reported as `ERROR` (inconclusive), not `FAIL`, if a frame lspcheck
skipped was arriving at any time between sending the request and giving up on the reply, or lspcheck stopped inspecting
before it gave up, since the reply might have been in what it didn't inspect. See the
[robustness reference](docs/design.md#capabilities-and-robustness) for R1, R3 and R4's evidence rules.
A frame that was complete before the request was sent can't have held its reply, so it excuses nothing.
Likewise, a `PASS` or `NOT EXERCISED` that relied on lspcheck seeing everything in a window becomes `ERROR` if it finds,
once the server's output has been read to the end, that it missed something there (a frame still arriving at the time
may turn out to be cut short). A failure lspcheck saw
directly, such as a wrong result, stays `FAIL`. The same happens when some of the server's frames can't be read at all,
and when lspcheck couldn't finish writing to a server that stopped reading its input: a request lspcheck couldn't
deliver gives `ERROR`, never `FAIL`. A header section longer than 1 MiB also stops inspection.

### Verdicts

| Verdict | Meaning |
| --- | --- |
| `PASS` | The rule held. |
| `FAIL` | The rule was broken; the detail line says how. |
| `SKIP` | The check is disabled, doesn't apply to this server, or the profile skips it with a reason. |
| `N/E` | Not exercised: the rule applies, but the run produced no evidence either way. |
| `ERROR` | The result is unknown: the server couldn't start or refused to initialize, a timeout ran out, or the evidence is incomplete. It says nothing about the rule. |

Each check has a basis: `MUST` for a normative rule of the LSP or JSON-RPC 2.0 specification, `SHOULD` for one of
their recommendations, and `POLICY` for hygiene that the specification doesn't require but clients rely on, such as
not leaving child processes behind.

### Exit status

- **0:** no `MUST` check failed.
- **1:** a `MUST` check failed, or with `--strict`, any check failed.
- **2:** a usage or report-writing error, or an `ERROR` verdict with no `MUST` failure.

Some servers refuse to initialize without project files or settings. lspcheck reports that as `ERROR`, with the
server's message, rather than as a failure; a [profile](#profiles) can give the server what it needs.

## Platforms

lspcheck runs on Linux, macOS and Windows. L9 finds the server's leftover processes through its process group on Linux
and macOS. On Linux lspcheck also makes itself a child subreaper, so it finds descendants that left the group with
`setsid`. On macOS such a descendant can't be seen, and the L9 result says so. On Windows L9 reports `N/E` for now.

## Development

```sh
cargo test --all-features     # unit tests, and runs against the stand-in server
cargo clippy --all-targets --all-features -- -D warnings
```

`lspcheck-standin` is a small, correct language server with faults that can be switched on one at a time
(`--list-faults`). Every check has a test showing that the clean stand-in passes and the right fault makes it fail.
That test is required, because a check that can't fail is worse than no check.

The [stock-server CI matrix](ci/stock-servers/README.md) adds regression coverage against real language servers.

## Licence

Apache-2.0. See [LICENSE](LICENSE).

K2 validates against the LSP 3.17 meta-model (`vendor/lsp-3.17/metaModel.json`), © Microsoft Corporation, which is
licensed under [CC BY 4.0](https://creativecommons.org/licenses/by/4.0/) and included unmodified; see
[its README](vendor/lsp-3.17/README.md) for the source.
