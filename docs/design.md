# lspcheck design

Drafted 2026-11-04. Licence: Apache-2.0. Language: Rust.

**Status:** milestones M0 and M1 are done: the transport, process, and session layers, the stand-in server, and checks F1 to F8, J4, and L1 to L9. M2 is done: J1 to J3, J5 to J7, C1 to C3, D1 to D6 and [`replay`](#report). F1, F2, F8, J1, J2, J5, J7 and L9 are judged across every scenario, and J3 across every scenario whose input is well formed (except the deliberately malformed `not-json` scenario). M3 is under way: v0.1.0 is released with prebuilt binaries for five platforms (each checked against the stand-in on its own platform, with SHA-256 checksums and build provenance), after a manual run against gopls, rust-analyzer, lua-language-server and basedpyright. v0.2.0 adds [profiles](#profiles-optional-toml) (`[server]`, `[workspace]`, `[client].initialization_options` and `configuration`, `[document]`, `[allow]`) with example profiles for those four servers, [K1 and R1 to R4](#capabilities-and-robustness), and [JUnit and Markdown reports](#report); the [stock-server CI matrix](../ci/stock-servers/README.md) is implemented; `[client].capabilities` is still planned. Unreleased: [K2](#capabilities-and-robustness) validates what the server sends against the vendored LSP 3.17 meta-model, including duplicate results, judged across every scenario. M4 is done: virgil-lsp's CI checks its freshly built server with the pinned v0.1.0 release.

## Purpose
A command-line tool that starts **any** language server binary over stdio and checks that it speaks the protocol
correctly: framing, JSON-RPC correlation, lifecycle, cancellation, document positions, and process cleanup. It needs no
knowledge of the server's language for its core checks. It reports what it checked, what it could not exercise, and a
transcript that reproduces each failure.

```
lspcheck -- gopls                          # default checks, temp workspace, no profile
lspcheck --profile go.toml -- gopls serve  # with a sample workspace and settings
lspcheck --json report.json --junit junit.xml --transcripts out/ -- ./my-server --stdio
lspcheck list                              # the check catalogue with spec references
lspcheck replay out/L5.jsonl -- ./my-server   # re-run one failing session
```
See the [README](../README.md#exit-status) for exit statuses and `--strict`.

## Non-goals
- Not a certification or "LSP compliant" badge. Each check states its basis, and the report lists what wasn't exercised.
- Not semantic correctness of answers (hover text, definitions): that needs language oracles. Language profiles may add
  source mutation and recovery later, but not in the core.
- Not coverage-guided fuzzing (LSPFuzz does that with instrumented builds).
- Not an editor or client test.

## Verdicts
Each check has a **basis**: `MUST` (a normative spec rule), `SHOULD` (a spec recommendation), or `POLICY` (hygiene the spec
doesn't mandate, such as exiting on end of input). Each result is one of:
`pass`, `fail`, `skip` (see [SKIP semantics](../README.md#verdicts)), `not-exercised` (applicable but the run produced
no evidence, e.g. no non-ASCII output was ever seen), or `error` (the checker or environment failed; never a pass).
Lessons from virgil-lsp: an unexercised check must never render as a pass, and a checker failure must keep the first
useful evidence rather than be replaced by a cleanup error.

## Check catalogue
Each session starts a fresh server process unless noted, so one failure can't contaminate the next. Spec references
are to LSP 3.17 and JSON-RPC 2.0.

### Framing and output
| ID | Basis | Check |
|---|---|---|
| F1 | MUST | Every byte the server writes to stdout, from spawn to exit, belongs to a well-formed frame (startup banners, logs and stray newlines fail). |
| F2 | MUST | Each frame's `Content-Length` counts the body's **bytes**, not its characters or UTF-16 code units. A frame whose body isn't JSON fails F2 when a complete JSON value with non-ASCII text (and any whitespace after it) starts the body, no header section starts at the declared end, a valid header section or the end of the output follows the value, and the declared length is exactly the value's length in characters or UTF-16 code units. Any other wrong length breaks the framing and is left to F1, so that a server whose bodies or headers are malformed in some other way can't be blamed for this mistake by coincidence. Parsing is bounded by 4 × `Content-Length` bytes (enough for any value of that many characters); the search for header sections is bounded, and frames left unchecked make F2 `error` rather than `pass`. To get non-ASCII output, the framing scenario sends a request whose string id has some (once F5 has shown the server reads such input). `not-exercised` if no readable frame carried non-ASCII text. |
| F3 | MUST | Input split across writes is read: a request whose header is written one byte per write and whose body is cut at seeded points, then (when F5 passed) one cut inside 2-, 3- and 4-byte characters. The writes are 10 ms apart, so a server reading promptly sees them separately. |
| F4 | MUST | Several frames in one write are all read (two requests with a `$/setTrace` notification between them). |
| F5 | MUST | A `didOpen` whose text has 2-, 3- and 4-byte UTF-8 characters keeps the stream in sync: a following request, with such characters too, is answered. The `didOpen` is sent only when the server asks for open and close notifications (`textDocumentSync` a kind other than None, or `openClose: true`). |
| F6 | MUST | A `Content-Type: application/vscode-jsonrpc; charset=utf-8` header is accepted (it's an optional header the client may send). |
| F7 | SHOULD | The legacy `charset=utf8` spelling is treated as UTF-8. |
| F8 | SHOULD | A `Content-Type` the server sends declares UTF-8 (or the legacy `utf8`), the only supported charset. |

F3 to F7 share one scenario. After `initialize`, a plain request for an unknown method (which must be answered, see J4)
shows that the server answers such probes. Each variant then sends probes written its own way, and fails if one goes
unanswered. After a variant that didn't pass, a plain probe checks the server is still in step; if it isn't, the
remaining variants are `error`, since their failure would say nothing new. If the first plain probe goes unanswered,
all five are `not-exercised` (or `error`, when lspcheck's own evidence was incomplete). `initialize` itself is sent
whole, because a missing reply to it can't be told apart from a slow start.

### JSON-RPC correlation
| ID | Basis | Check |
|---|---|---|
| J1 | MUST | A reply's `id` is the same value as the request's: a number stays a number with the same exact value (`7` and `7.0` are the same; no rounding through floating point), and `"7"` stays a string. A reply that is only loosely equal (the number spelled as a string, or equal only after rounding) fails with both spellings; a request with a string id and one with a number id must each be answered exactly somewhere in the run. |
| J2 | MUST | No response is unmatched: no duplicate for a request already answered, none to an id lspcheck never sent, and none read before its request was sent. Responses with a null id and those to a request lspcheck couldn't deliver are lawful; loosely equal ids belong to J1. A request that never got its reply is judged by the check that sent it. |
| J3 | MUST | No response to notifications, including unknown ones and unknown `$/` ones. A response without an id (`id` missing or `null`) read after `initialized` (a reply to `exit` included), in a scenario whose input is all well formed (every one but `framing`, `pre-initialize` and `not-json`), answers one of lspcheck's notifications. |
| J4 | MUST | An unknown request method gets error `-32601` MethodNotFound; an unknown `$/` request too. |
| J5 | MUST | Envelopes: `"jsonrpc": "2.0"`, exactly one of `result`/`error`, error `code` an integer and `message` a string; request ids integers or strings, response ids integers, strings or `null`, integers (ids and error codes) within LSP's signed 32-bit range; `params`, when present, an object or an array. |
| J6 | MUST | Concurrent requests (sent before any reply) are each answered with the right id; order may differ. |
| J7 | MUST | Server-to-client requests don't reuse an unanswered id, and the server keeps serving after lspcheck answers them (judging rules below). The id's type and range are J5's. |

J1, J2, J5 and J7 are judged across every scenario from the responses and messages seen. The `json-rpc` scenario exercises J6
and J3: after `initialize`, four requests are written before any reply is read (two numeric ids, one numeric string and
one non-numeric string), and each must be answered with its exact id in any order; then two unknown notifications are
followed by two probes; this guarantees J3 is exercised. Across the other scenarios with well-formed input, any response
without an id read after `initialized` fails J3 too, since it can only answer a notification (C3 leaves to J3 the ones
it can't attribute to a cancel). The `not-json` scenario is excluded because its complete body is deliberately not JSON
and a null-id ParseError is lawful there. A write of lspcheck's that stops partway through a frame may also get a
null-id ParseError; a frame lspcheck skipped or couldn't read since `initialized`, or such a write, makes a pass an
error. A frame
lspcheck couldn't read anywhere (F1) also makes a J1, J2, J5 or J7 pass an error: it might have held what they judge.

J7 treats an earlier request as unanswered when lspcheck never wrote its whole answer. For a completed answer,
J7 times a reuse against when lspcheck began writing it: only a request read before then was certainly sent before
the server could have had the answer. Reusing an id at or after that time is treated as lawful if the earlier answer
was written in full. lspcheck answers `workspace/configuration` using the [profile rules](#profiles-optional-toml),
and returns the workspace for `workspace/workspaceFolders`, `applied: false` and
`success: false` for `workspace/applyEdit` and `window/showDocument`, `null` for registrations, progress tokens,
`window/showMessageRequest` and the refresh requests, and MethodNotFound otherwise. A server that stops answering after an answer may have stopped for another reason,
so that alone is never J7's failure. When the evidence is complete and no unanswered id was reused, J7 passes when
the server answers at least one request lspcheck sent after completing an answer to a server request. It is an error
when no answer was followed by service but at least one was followed by unanswered requests, and not exercised when
the server sent no requests or lspcheck sent none after completing an answer.

### Lifecycle
| ID | Basis | Check |
|---|---|---|
| L1 | SHOULD | A request before `initialize` gets error `-32002`; a notification sent before it (`workspace/didChangeConfiguration`) is dropped without a reply: no response other than those two answers may arrive before the answer to `initialize`. |
| L2 | MUST | `initialize` returns an object with `capabilities`; a chosen `positionEncoding` is one the client offered (offer `utf-8` first, `utf-16` last). |
| L3 | POLICY | A second `initialize` gets an error and the server keeps serving: a plain request answered before it is answered after it too. |
| L4 | MUST | `shutdown` returns `null` and the process keeps running until `exit`. |
| L5 | SHOULD | Requests after `shutdown` get error `-32600` InvalidRequest (tested with `workspace/symbol`, after L3 in the same scenario; if the server stopped answering at L3, L5 is `error`). |
| L6 | SHOULD | `exit` after `shutdown` ends the process with status 0 within the deadline. |
| L7 | SHOULD | `exit` without `shutdown` ends it with status 1. |
| L8 | POLICY | End of stdin without `exit`, after `initialize`, ends the process within the exit deadline, with any status (prevents orphaned servers). |
| L9 | POLICY | After exit no descendant remains. Profiles can allow named long-lived daemons. Where lspcheck can't see an escape through `setsid`/double fork (macOS), a pass says so in its detail. |

While it waits for the process to exit after `exit`, lspcheck keeps reading and answers the server's requests: a
server may read `exit` but handle it only once a request it sent just before is answered (gopls does this with the
progress token it asks for after `initialized`), and lspcheck sends `exit` without pausing. A server still running at
the deadline fails L6 or L7, unless lspcheck missed some of its output (a frame it skipped or couldn't read, or reading
that stopped), which may have held a request the server is waiting on; then the check is `error`. An answer
lspcheck couldn't write doesn't excuse it: a server waiting for an answer would be reading its input.
The exit deadline takes priority over queued output. Timeout verdicts are checked again after cleanup, so a frame
that began before the deadline but was only found truncated at the end of stdout also makes L6 or L7 `error`.

### Cancellation
| ID | Basis | Check |
|---|---|---|
| C1 | MUST | `$/cancelRequest` for a pending request still yields exactly one response: a result, error `-32800` RequestCancelled (advised), or another error. |
| C2 | MUST | Cancelling an unknown id or an already-answered id produces nothing and the server keeps serving. |
| C3 | MUST | No response to `$/cancelRequest` itself. |

C1 to C3 share one scenario. After `initialize` and a plain request, lspcheck sends one request (`workspace/symbol`
when the server advertises `workspaceSymbolProvider`, otherwise an unknown-method probe) and its `$/cancelRequest`
before reading anything, waits for the request's response, and sends a plain request. Next it cancels an id it never
sent and then the finished request, each followed by a plain request, and finally shuts the server down. A reply to a
cancel can come at any time, so the verdicts are given at the end, over everything read before `exit` was sent:

- a second response to the cancelled request fails C1 when it was read before lspcheck cancelled that request again,
  and C2 when after; a response carrying the never-sent id fails C2, and so does a plain request that goes unanswered
  after the cancels;
- a response without an id (`id` missing or `null`) can't be matched to the notification it answers, and until `exit`
  lspcheck sends only `initialized` (and, with a profile's settings, `workspace/didChangeConfiguration`) and the
  cancels. C3 fails on one read after the first cancel was written whose
  text names `$/cancelRequest` (as a MethodNotFound message usually does); any other is `error` for C3, since it may
  answer `initialized` or the settings (answering any notification is J3's failure).

If the server stopped answering, what it didn't send can't be judged, so those checks are `error`. A pass is also
`error` when lspcheck missed something where that check's evidence would be: a frame it skipped or couldn't read, a
server request it couldn't answer (the server may be waiting for it), or reading that stopped. For C1 that is from the
cancelled request until it is cancelled again, for C2 from its first cancel, and for C3 from the cancelled request,
each until `exit`. A missing response is excused only by something missed while lspcheck was waiting for it.

lspcheck times what it missed as spans, from the arrival of the first byte it couldn't inspect to the moment it knew
it had missed them, so a frame that began before a wait and ended after it still counts for that wait. Some losses are
found only later: a frame still arriving when a verdict is reached may turn out to be cut short when the stream ends.
So every `PASS` and `NOT EXERCISED` that relied on nothing being missed in a window is checked again at the end of its
scenario, once the server's output has been read to the end, and becomes `ERROR` if something missed falls in that
window. A missing response is checked the same way and becomes `ERROR` rather than `FAIL`. A failure lspcheck saw
directly is never replaced.

### Documents and positions
D1 to D6 are done.

| ID | Basis | Check |
|---|---|---|
| D1 | MUST | Every judged position in a reply or notification about an open document is at a character boundary of the text it refers to, in the negotiated encoding: never inside a UTF-16 surrogate pair or a UTF-8 multi-byte sequence. Positions past the end of a line or of the document aren't judged, since the spec lets clients clamp them. Splitting a grapheme cluster is lawful. |
| D2 | MUST | A `version` in `textDocument/publishDiagnostics` for an open document is one lspcheck sent for that URI before the notification was read: the version `didOpen` sent, and a later `didChange`'s. An older version is lawful, and so is none at all; `null` or a non-integer one is a type error for K2, not D2's finding. |
| D3 | MUST | After a `didChange`, positions refer to the changed text. lspcheck sends the change the advertised way: an incremental edit replaces a 4-byte character and adds a line, or the full text when the server asked for Full. |
| D4 | POLICY | A request about a closed document, and one about a URI that was never opened and doesn't exist, each get a response (any result or error), and the server keeps serving. |
| D5 | MUST | Lines are split at `\n`, `\r\n` and `\r`: positions in a document with CR LF and lone CR line endings fall on character boundaries of the lines the spec defines. |
| D6 | MUST | A `selectionRange` result has one entry per position, `result[i].range` contains `positions[i]`, and each `parent.range` contains its child's range. |

The checks share one scenario, `documents`. lspcheck initializes with client capabilities for folding ranges,
selection ranges, highlights, links, code lenses, pull diagnostics, inlay hints and formatting, and offers the usual
encodings (`utf-8`, `utf-32`, `utf-16`). The negotiated encoding is the server's `positionEncoding`, or UTF-16 when it
chose none. If the server doesn't ask for `didOpen`, D1 is `SKIP`. Otherwise lspcheck saves document A as `doc.txt` in
the workspace and opens it as `plaintext` at version 41. A's first line is six emoji, ` word `, six emoji: the start
and end of `word` land inside a character whichever way a server confuses UTF-8 bytes, UTF-16 units and code points.
Its other lines hold a combining mark, CJK characters, a ZWJ sequence, a flag and an ASCII line. lspcheck then sends
each request the server advertises (document symbols, folding ranges, links, code lenses, inlay hints, formatting,
pull diagnostics, and hover, highlights and selection ranges at positions in and around `word`), one at a time, each
bounded by `--timeout`, with every position counted in the negotiated encoding. It stops sending them after one goes
unanswered. Then it waits up to 3 s for diagnostics labelled with version 41.

lspcheck then reads the server's `textDocumentSync` kind (a number or `{change: n}`; absent or `0` means the server
takes no changes) and, when it takes them, sends one `didChange` to version 42: an incremental sync sends two content
changes, the first replacing the first emoji of the first line with four emoji (its range computed from lspcheck's own
text model in the negotiated encoding), the second inserting a line of ten emoji after it, so every later line moves
down one; a Full sync sends the whole changed text. lspcheck applies its own changes with its own text model to get
version 42, then sends the same requests again, with positions counted against version 42's text, one at a time, and
waits up to 3 s for diagnostics labelled with version 42. Then it closes document A with a `didClose` and sends
D4's requests: one advertised document request (preferably `documentSymbol`, otherwise `hover` at (0,0), otherwise
the first advertised of `foldingRange`, `documentHighlight` at (0,0) and `selectionRange` at (0,0)) about the closed
document, the same request about `missing.txt` in the workspace, which was never opened and doesn't exist, and a
plain `lspcheck/probe`, each sent alone and awaited by `--timeout`. If the server doesn't ask for `didOpen`, D1 to
D3 skip and D4 still runs, with the document never opened. Then it leaves.

Positions are found by a fixed table of where each reply and `publishDiagnostics` carries them, never by searching
for `line` and `character` (so a diagnostic's `data`, for instance, is never read). A position counts only when its URI
names the opened document, compared as written and then after percent-decoding and folding a drive letter's case. It
is judged when its line exists and its character is at most the line's length. It is *informative* when its line has
non-ASCII text before it, since only those can show encodings confused. A reply refers to the version current when its
request was sent, and diagnostics to the version they are labelled with, or else to any version sent before they were
read; a position is invalid only if it falls inside a character in every text it may refer to. The verdict is given
at the end, over everything read before `exit` was sent:

- **fail:** an invalid position. The detail names the first three, with where each was found and the counting that
  would have put it on a boundary (such as "on a boundary if counted in UTF-8 bytes"). That diagnosis is detail only.
  A failure seen stands even if evidence elsewhere is incomplete.
- **not-exercised:** the server chose UTF-32, where no offset can fall inside a character, or no informative position
  was judged and lspcheck missed nothing. The detail counts the positions seen, past the end of a line, on lines the document doesn't have, and in
  other documents. Positions past the end of a line that would fit counted another way are reported as a hint of that
  counting, which D1 can't prove.
- **error:** `initialize` failed, `didOpen` couldn't be sent, the server chose an encoding lspcheck didn't offer (L2's
  failure), or nothing failed but lspcheck missed something the server sent from the `didOpen` on (a frame it skipped
  or couldn't read, a server request it couldn't answer, or reading that stopped), which might have held positions.
- **pass:** otherwise, with the number of informative positions judged.

D2 is judged over the same window, by version instead of by position: a `publishDiagnostics` for the opened document
that carries an integer `version` must name one lspcheck sent for it before the notification was read, so a server
that labels with its own counter, or with one past the document's, fails; the detail gives the version read and the
versions sent before it. An older version is lawful. A notification without a version leaves D2 `N/E` with the
unversioned ones counted, and `null` or a non-integer `version` isn't judged here (K2's type check), nor is any URI
lspcheck didn't open. Every early exit that skips or voids D1 skips or voids D2 too, and a pass over missed evidence
is `ERROR` for D2 as it is for D1.

D3 judges the positions that can only refer to the changed text, against version 42's text: replies to requests sent
after the change, and diagnostics labelled 42. A late reply to a request sent before the change, or unlabelled
diagnostics, may refer to version 41, so a position in them that is invalid is D1's finding, not D3's:

- **skip:** the server doesn't ask for `didOpen`, or it takes no changes (`textDocumentSync` absent or `0`), so no
  `didChange` is sent.
- **error:** the server registered `textDocument/didChange` or `didOpen` dynamically before the change (a
  `client/registerCapability` request whose `params.registrations` array has an entry whose `method` is one of
  those; lspcheck declares no dynamic registration but answers registrations with success, so the server may
  expect another sync kind); the `didChange` couldn't be sent; or nothing failed but lspcheck missed evidence the
  server sent from the change on.
- **fail:** a position read after the change is inside a character of version 42 in the negotiated encoding, on no
  boundary within its line of version 42 under any other encoding (UTF-8, UTF-16 or UTF-32), and on a boundary of an
  *alternative* text, counted in the negotiated encoding. The alternative texts are the text before the change (an
  ignored change, a stale snapshot, or the file read from disk) and, for an incremental sync, the change applied with
  its range counted in each other encoding. No reading of the changed text explains such a position, and a text the
  change should have produced does. The detail names the first few, each with the alternative it fits.
- **error:** a position is invalid after the change, but some other reading of the changed text explains it, or no
  alternative does: "can't tell a change-handling fault from a counting fault; see D1".
- **not-exercised:** the server chose UTF-32, where no offset can fall inside a character, or no informative position
  was judged after the change.
- **pass:** otherwise, with the number of informative positions judged after the change.

Valid positions before the change are never taken as proof that the server counts correctly, so they never blame the
change: a coincidence (code-point offsets that equal UTF-16 ones), an echo of a position lspcheck sent, or a server
that counts some positions one way and some another all look the same. In code points every in-line offset is a
boundary, so a change mishandled only on line 0 usually stays `ERROR` (D1 fails it); positions on the inserted line,
whose offsets in the old text run past its code-point length, are what separate an ignored change from a counting
fault.
Positions in every alternative text are counted in the negotiated encoding: a server that ignored the
change, or counted its range wrongly, still counts its positions as agreed.

D4 judges that a response comes, not what it says: any result or error is lawful, including real results read from
disk, and the content of a reply is never judged. D1 to D3 judge only what was read before lspcheck began sending
`didClose`: after it, the server may read the file from disk, whose text lspcheck doesn't track (another tool may
have changed it), so neither D4's replies nor later diagnostics are judged by them.

- **skip:** the server advertises none of `hoverProvider`, `documentSymbolProvider`, `foldingRangeProvider`,
  `documentHighlightProvider` or `selectionRangeProvider`.
- **fail:** any of the three requests goes unanswered within `--timeout`. It goes through `fail_call`, so it is
  `error` when lspcheck couldn't deliver the request, or when missed evidence could have held the answer.
- **error:** the `didClose` couldn't be sent, or an earlier step left the session unable to send; or a D4 request
  went unanswered when the server's latest requests before it had gone unanswered too (it had stopped answering
  before any document was closed, which says nothing about documents that aren't open). A server that answers again
  in between has recovered, and is judged as usual.
- **pass:** all three are answered. The detail records each outcome: result (null, empty, or non-empty) or the
  error code, for the closed document, the never-opened one, and the probe.

Before D4, lspcheck opens document B, `crlf.txt` at version 7: `x` ending in a lone `\r`, eight emoji ending in
`\r\n`, nine spaces and `word`, eight emoji, and eleven spaces and `name`, the last three ending in `\n`. It sends
the advertised requests with positions on `word` and `name`, waits for B's diagnostics, and closes B. Each wrong split
moves `word` onto an emoji line, at offsets 9 and 13: past the end of eight emoji in code points, and inside a
character in UTF-8 and UTF-16. So no counting of the spec's lines explains them. D5 is judged over what is about B
and was read while B was open:

- **skip:** as D1.
- **fail:** a position about B is inside a character of B under the spec's split, in the negotiated encoding; on no
  in-line boundary of B under the spec's split in any other encoding (UTF-8, UTF-16 or UTF-32); and on a boundary of
  B under a split the spec doesn't allow (`\n` only; `\n` and `\r\n` but no lone `\r`; `\r\n` as two breaks),
  counted in the negotiated encoding. The detail names every wrong split each position fits.
- **error:** a position about B is invalid, but that doesn't hold ("can't tell a line-splitting fault from a counting
  fault; see D1"); B's `didOpen` couldn't be sent; or nothing failed but evidence was missed while B was open.
- **not-exercised:** UTF-32, or no informative position about B.
- **pass:** otherwise.

The design's original rule (a), "D1's positions in LF-only document A were informative and all valid", is left out
for the reason D3 dropped its own: valid positions elsewhere never prove the server splits B's lines correctly.

D6 judges the `selectionRange` replies to the requests lspcheck sent about the open documents (A at 41 or 42, B at
7), read while each was open (the same windows D1 uses). The text a reply refers to is the version current when its
request was sent; it is used only to clamp. For each reply with an array result, compared against the request's own
`positions`:

- **skip:** `selectionRangeProvider` isn't advertised, or no `didOpen` (as D1).
- **fail:** the array's length isn't the number of positions sent; a `result[i].range` doesn't contain `positions[i]`
  (start ≤ position ≤ end, comparing (line, character) pairs, after clamping each character to its line's length in
  the negotiated encoding); or a `parent.range` doesn't contain its child's range, anywhere up the chain. A seen
  failure stands even when evidence is incomplete. The detail names the first few.
- **error:** no failure seen, but `incomplete_within` reports missed evidence in the window D1 uses, or a
  `selectionRange` request couldn't be answered: lspcheck couldn't send it, or the server's output had closed before
  its reply (the whole rest of a batch is cut off then). D6 doesn't pass on part of its evidence.
- **not-exercised:** no containment comparison could be made: no reply with an array result (only `null`, errors, or
  no reply), or only entries that are malformed or on lines the text doesn't have.
- **pass:** otherwise; the detail says how many replies and entries were judged.

A range on a line the text doesn't have is not judged (counted in the detail); a malformed entry (no `range`, not an
object, a `parent` that isn't an object) is not judged here — it is a type error for a later check — but is counted.
The parent chain walk is bounded (1000 levels), and a chain cut short is counted.

Most language servers ignore a plain-text document, so expect D1 to be `N/E` for them; D2 needs
only one versioned publish for the document, so it is exercised wherever diagnostics are labelled. D4 runs wherever
a document request is advertised.

### Capabilities and robustness
| ID | Basis | Check |
|---|---|---|
| K1 | MUST | A request the server advertises, sent with valid parameters about an open document, isn't answered with MethodNotFound (-32601). |
| K2 | MUST | What the server sends validates against the LSP 3.17 [meta-model](../vendor/lsp-3.17/README.md): results of requests, and the params of its own requests and notifications. |
| R1 | POLICY | A well-framed body that isn't JSON gets ParseError `-32700` with id `null` or is ignored, and the server keeps serving. |
| R2 | POLICY | A request with parameters of the wrong types gets an error response (InvalidParams preferred), and the server keeps serving. |
| R3 | POLICY | A large document (default 4 MiB, configurable) is accepted or refused without the server failing, and it keeps serving. |
| R4 | POLICY | Unusual but valid request ids round-trip exactly: 0, negative, the 32-bit extremes, an empty string, a long string, a non-ASCII string. |

K1 runs in its own `capabilities` scenario. Advertising a capability declares that the server handles its request,
so answering that request with MethodNotFound contradicts the server's own initialize result. Any other error
(RequestFailed, ContentModified, InvalidParams for a position the server dislikes, or a code of its own) is lawful and
only noted in the detail. lspcheck initializes with client capabilities for every request below, then takes the
document from the profile's [`[document]`](#profiles-optional-toml), or else writes its own, `lspcheck-k1.txt`, as
`plaintext`: identifier-like words on four lines. It reads the document's text from the workspace (a profile's
positions are checked again against it, in case the fixture changed since the profile was read; a mismatch is
`ERROR`) and opens it with `didOpen` at version 1, when the server asks for `didOpen` and at least one request is
about a document. A capability is advertised when its value is `true` or an object;
`semanticTokensProvider` (always an object) advertises `full` and `range` separately, and `renameProvider` advertises
`prepareRename` only with `prepareProvider: true`. For each advertised capability, in this order, K1 sends:

| Capability | Method | Params beyond `textDocument` |
|---|---|---|
| hoverProvider | textDocument/hover | position |
| definitionProvider | textDocument/definition | position |
| declarationProvider | textDocument/declaration | position |
| typeDefinitionProvider | textDocument/typeDefinition | position |
| implementationProvider | textDocument/implementation | position |
| referencesProvider | textDocument/references | position, `context: {includeDeclaration: true}` |
| documentHighlightProvider | textDocument/documentHighlight | position |
| signatureHelpProvider | textDocument/signatureHelp | position |
| completionProvider | textDocument/completion | position |
| renameProvider.prepareProvider | textDocument/prepareRename | position |
| renameProvider | textDocument/rename | position, `newName: "lspcheckRenamed"` |
| linkedEditingRangeProvider | textDocument/linkedEditingRange | position |
| callHierarchyProvider | textDocument/prepareCallHierarchy | position |
| typeHierarchyProvider | textDocument/prepareTypeHierarchy | position |
| monikerProvider | textDocument/moniker | position |
| selectionRangeProvider | textDocument/selectionRange | `positions`: all of them |
| documentSymbolProvider | textDocument/documentSymbol | — |
| foldingRangeProvider | textDocument/foldingRange | — |
| codeLensProvider | textDocument/codeLens | — |
| documentLinkProvider | textDocument/documentLink | — |
| colorProvider | textDocument/documentColor | — |
| diagnosticProvider | textDocument/diagnostic | — |
| documentFormattingProvider | textDocument/formatting | `options: {tabSize: 4, insertSpaces: true}` |
| documentRangeFormattingProvider | textDocument/rangeFormatting | the whole document's range, options |
| codeActionProvider | textDocument/codeAction | the whole document's range, `context: {diagnostics: []}` |
| inlayHintProvider | textDocument/inlayHint | the whole document's range |
| semanticTokensProvider.full | textDocument/semanticTokens/full | — |
| semanticTokensProvider.range | textDocument/semanticTokens/range | the whole document's range |
| workspaceSymbolProvider | workspace/symbol | `{query: ""}` (no `textDocument`) |

A position request is sent once at each position: the profile's (at most five), or else one character into each of
the first three identifier-like tokens (`[A-Za-z_][A-Za-z0-9_]*`, matched leftmost first) of the document, or its
start when it has none. Positions are counted in the negotiated encoding (the server's `positionEncoding`, or
UTF-16), and the whole document's range ends at the end of its last line (the empty line after a final newline).
K1 doesn't send `workspace/executeCommand`, `willSaveWaitUntil`, `onTypeFormatting` or `inlineValue`: they need a
command, a save, a trigger character or a stopped debugger, which K1 can't set up faithfully.

Two things keep a request about the document from being sent, and the detail names them: a server that doesn't ask
for `didOpen` has no open document to be asked about (only `workspace/symbol` is sent), and a static registration's
`documentSelector` that doesn't select the document. A filter selects it when its `language` (if any) is the
document's language id and its `scheme` (if any) is `file`; a filter with a glob `pattern`, or for notebooks, isn't
evaluated and doesn't select it.

The requests are sent one at a time, each waiting up to `--timeout` for its reply. After one goes unanswered, the
remaining requests are sent without waiting for each reply. Only a delivery failure stops sending (the detail
lists the requests left unsent). LSP sets no deadline for a response, so a wait that ends is not evidence against
the server: after sending the requests, the scenario shuts the server down gracefully (`shutdown`, then `exit`),
answering the server's requests on the way. Once session cleanup has drained the final stdout, K1 judges every
request by its matched response, including replies read during shutdown or the final drain. A MethodNotFound reply
counts when its id equals the request's exactly and it was read at or after the request was sent, even after another
response with that id (the duplicate is J2's).

For either a failure or an error, the detail names every unanswered request. It gives the delivery failure's cause
(including the server's exit status when observed) once, on the request lspcheck couldn't send, then lists the
remaining requests in a single `not sent after that` note.

- **fail:** a request was answered with an error whose code is -32601. The detail names each such request (method
  and position). This is seen evidence and stands whatever happened to the other requests.
- **error:** initialize didn't succeed, the server chose an encoding lspcheck didn't offer (see L2), the document
  couldn't be written, read or opened (`didOpen` not delivered), or a request couldn't be delivered. A delivered
  request with no response by the end of the session is also `ERROR` (`no response to <method> at <position> by the
  end of the session, so K1 couldn't judge it`), unless another request got -32601, which fails K1.
- **not-exercised:** the initialize result has no capabilities object, or the server advertises none of the requests
  above, or all advertised requests were excluded by the `didOpen` or selector rules (the detail says why).
- **pass:** every request selected for delivery was answered with a result or another error by the end of the
  session. The detail counts the requests and methods, and lists the other errors by code with the requests that
  got them.

Responses to K1's requests are judged by J1, J2 and J5 like any other, and the scenario's input is all well formed,
so it is included in J3. K1 judges only the error code, never a result's content.

K2 is judged across every scenario, like J5, over every message lspcheck read. Its types come from `metaModel.json`
for LSP 3.17, vendored unmodified from microsoft/language-server-protocol under CC BY 4.0 (see
[its README](../vendor/lsp-3.17/README.md)) and parsed once. What is judged:

- the `result` of a success response that matches, by exact id, a request lspcheck sent for a method the meta-model
  defines, against that request's `result` type, including subsequent responses with that id (J2 also judges the
  duplication). Error responses are J5's. `shutdown`'s result is L4's, so one bug
  doesn't fail two checks;
- the `params` of a request or notification the server sent whose method the meta-model defines for the server to
  send (direction `serverToClient` or `both`), against its `params` type. Absent `params` where the meta-model gives
  them a type are a violation (an absent value isn't of that type); where it gives none, `params` aren't judged.

Not judged: methods the meta-model doesn't have (custom methods, and `$/` methods other than `$/cancelRequest`,
`$/progress`, `$/logTrace` and `$/setTrace`), definitions marked `proposed` (requests, structures, enumerations, type
aliases and properties), the replies to lspcheck's unknown-method probes, and frames lspcheck couldn't read (F1's).

A value is of a type when:

- **base types:** `integer` is a whole-valued JSON number from -2^31 to 2^31-1, including decimal and exponent
  spellings such as `41.0` and `4.1e1`; `uinteger` one from 0 to 2^31-1; `decimal` any number; `string`, `boolean`
  and `null` as named. `URI`, `DocumentUri` and `RegExp` are strings, whose syntax isn't judged.
- **references** name a structure, an enumeration or a type alias (the names are unique across the three).
- **structures** and structure literals: a JSON object in which every property that isn't optional, including those
  from `extends` and `mixins` (recursively; a structure's own property replaces an inherited one of the same name), is
  present and of its type. An optional property may be absent, but if present must be of its type, so `null` is a
  violation unless the type admits `null`. Properties the type doesn't name are lawful (LSP is structurally typed, and
  servers add extensions).
- **enumerations:** the value has the enumeration's base type and is one of its values, or any value of that type when
  the enumeration `supportsCustomValues` (such as `CodeActionKind` and `FoldingRangeKind`).
- **arrays:** every item is of the element type; **tuples:** exactly as many items as types, each of its type;
  **maps:** a JSON object whose values are of the value type (keys are JSON strings, so a key is judged only when the
  key type is a string enumeration); **and:** every type; **or:** at least one; **string, integer and boolean
  literals:** exactly that value.

The work is bounded: a message that would take more than 5,000,000 (value, type) steps to validate isn't judged, and
is counted in the detail; that is lspcheck's limit, not a protocol rule, so it never fails K2. Nesting is bounded by the
JSON parser's limit (values nested 128 deep aren't read), and validation runs on a thread with a 64 MiB stack, enough
for the deepest values the parser accepts in a debug build too (Windows gives its main thread only 1 MiB).

- **fail:** at least one violation was seen. The detail gives the count and names the first five that differ, each
  with its scenario and time, the method, whether it is a result or params, a path (`result.contents.kind`), what the
  meta-model requires there (`MarkupKind: "plaintext" | "markdown"`, `Position`, `uinteger (a whole number from 0 to
  2147483647)`, …) and the value found, shortened without traversing the rest of the value. For an `or` nothing matched
  it names the alternatives and, when one of them got deeper into the value than the others (through nested `or`s too),
  where that one failed. A violation
  seen stands even if other frames couldn't be read.
- **error:** no violation was seen, but a frame couldn't be read anywhere (as for J5: it might have held a message K2
  judges), lspcheck's inspection of the output was cut short, no scenario could start the server, or lspcheck
  couldn't start the thread that validates.
- **not-exercised:** nothing was judged: the server sent no result, request or notification the meta-model gives a
  type (the detail counts messages too large to judge).
- **pass:** otherwise. The detail counts the results, server requests and notifications validated, and any not judged
  for their size.

R1 runs in `not-json` after initialization. It writes one complete frame whose body is not JSON, then sends a plain
request. An ignored body is lawful. The body has no id, so only a response with `id: null` can answer it: one
with another error code than `-32700` (ParseError), or a result, fails R1. An unmatched response with another id is a
duplicate or unasked reply, which J2 judges, not R1. Both the response verdict and
ParseError count use unmatched response occurrences read from the start of the malformed-body write through the
end of the probe wait. Judging follows graceful shutdown so queued messages are included, with earlier and later
reads excluded. The plain request must be answered. An exit before the answer or an unanswered delivered request
fails; missing answers become `ERROR` when evidence from the start of the probe write through the end of its wait
is incomplete. Output skipped before the probe write cannot have held its answer. An exit while a
delivered probe is pending preserves that probe's wait window, ending before shutdown or cleanup. Once the probe
is answered, a later exit does not fail R1. Incomplete delivery of the malformed body is `ERROR`; an undeliverable
probe is also `ERROR` unless an observed exit establishes a failure. J2 doesn't attribute null-id errors to a
request, J5 validates their envelope, and this deliberately malformed-input scenario is excluded from J3.
After a failed probe, R1 waits up to `--exit-timeout` for the server to exit before attempting shutdown: a closed
stdin can precede the process's exit status, especially on Windows.

R3 runs in `large-document` after initialization. It writes `lspcheck-large.txt` into the scenario workspace and
opens it as `plaintext`; [usage](../README.md#use) documents the size option and default. Once the full `didOpen` is
delivered, lspcheck sends a plain request and waits up to `--startup-timeout`. The server may accept or refuse the
document; it must answer the probe. An incomplete `didOpen` is `ERROR`. A probe lspcheck couldn't deliver is also
`ERROR` unless an observed exit establishes a failure. A delivered probe that goes unanswered fails only when
lspcheck saw complete evidence during its wait, even if the server exits. Once the probe is answered, a later exit
does not fail R3. As with R1, a failed probe allows up to `--exit-timeout` to observe an exit before shutdown.

R4 runs in the separate `odd-ids` scenario after initialization. It sends `lspcheck/probe` with each of these ids,
one at a time: `0`, `-1`, `2147483647`, `-2147483648`, `""`, a 1000-character ASCII string, `"é中😀𝄞"` and
`"7"`. Each request waits for a response or its deadline before the next is sent; a closed output stream or a failed
write stops the sequence. It passes only when every request gets a response with its exact id (a result or an error is
acceptable). Only delivered requests with no exact-id response can fail R4. The detail names unanswered ids and any
unmatched response ids seen; long strings are truncated. Each unanswered request retains its own wait window for
the evidence recheck after cleanup; any fully inspected window establishes a failure. If every such window has
incomplete evidence, the verdict is `ERROR`. Initialization or delivery failure is also `ERROR` unless an earlier
delivered request establishes a failure. J1 and J2 independently judge mismatched ids under their `MUST` rules.
Numeric ids stay within LSP 3.17's signed 32-bit range, so `2^53` is deliberately excluded.

R2 runs in its own `wrong-params` scenario so a server crash cannot contaminate other checks. From the initialize
capabilities it selects the first advertised method among hover, definition, document symbols and workspace symbols.
It sends the corresponding method with a number where an object or string is required. After an error response,
it sends a plain probe; a response to that probe passes. `InvalidParams` is preferred but any error code passes
when the initial response satisfies [J5's envelope rules](#json-rpc-correlation).
A result or malformed response, no response to a delivered request, or a server that stops serving fails R2;
missing replies become `ERROR` when lspcheck missed output during that request's wait. This also applies when
the server exits while the delivered follow-up probe is pending: its wait window ends when the reply wait ends,
before waiting for process exit or cleanup. Initialization or delivery failure is
`ERROR`. If none of those capabilities is advertised, R2 is not exercised. The well-formed `wrong-params`,
`large-document` and `odd-ids` scenarios are included in [J3 judging](#json-rpc-correlation); `not-json` is not.

After successful initialization, all four robustness scenarios and `capabilities` attempt a bounded
`shutdown`/`exit` exchange before cleanup. This lets lawful output still in flight finish and lets L9 observe descendants that outlive the server.

## Profiles (optional TOML)
Profiles are implemented in `src/profile.rs`; the [README](../README.md#profiles) owns the supported profile example,
command and timeout precedence, path rules, and common usage. This section records implementation details and
planned extensions. The documents scenario still uses its [plain-text probe documents](#documents-and-positions);
only K1 uses `[document]`. [`examples/profiles/`](../examples/profiles/) has a profile for each of gopls,
rust-analyzer, lua-language-server and basedpyright.

This field is planned, and refused for now:

```toml
[client]
capabilities = "default"       # planned: or a path to a capabilities JSON
```

- **Parsing.** Serde's `deny_unknown_fields` enforces the accepted keys. TOML parsing errors include a line, column
  and the offending line; validation errors name the relevant key. Duration quantities must be non-negative whole
  numbers. The profile is validated before any scenario starts. The timeout flags are optional in clap, so defaults
  can be applied after merging with the profile.
- **Workspace validation.** Each scenario's `rootUri` and `workspaceFolders` point to its populated temporary
  workspace. The copy rechecks the fixture root and each entry without following symbolic links, because the
  fixture may have changed since validation. Inline files replace copied destinations before writing, so even a
  read-only fixture file can be overridden; replacements use default file creation permissions. Command arguments
  are passed as written.
  An inline file that needs a directory where the fixture or another inline file has a file, or that names a fixture
  directory, is refused.
- **lspcheck's own files.** `checks::OWN_FILES` enforces the reserved top-level names listed in the README.
  These include R3's `lspcheck-large.txt` and K1's `lspcheck-k1.txt`. Deeper paths such as `src/doc.txt` are the profile's.
- **`initialization_options`** is any TOML value, converted to JSON (a date or time becomes its TOML spelling; NaN or
  an infinite float is an error), and sent as `initializationOptions` in every initialize lspcheck sends: each
  scenario's, L3's second one and the pre-initialize scenario's. Absent, the field is omitted as before.
- **`configuration`** (a table). Each `workspace/configuration` item is answered by looking up its `section` as a
  dot-separated path into the table (`"gopls"` gives the `gopls` table, `"gopls.staticcheck"` gives `false`); an item
  without a section, or with a null or empty one, gets the whole table; a path that leads nowhere, or another
  non-string section, gets `null`. Without it, each item gets `null` as before. When it is given, lspcheck also sends
  `workspace/didChangeConfiguration` with `{"settings": <configuration>}` right after `initialized`, before the
  scenario's first request (some servers read settings only that way). That is one more notification after
  `initialized`: J3 already counts any id-less response after `initialized` as answering a notification, and C3's
  attribution reads it as one of the notifications it can't tell apart from `initialized` (`error`, not `fail`). The
  pre-initialize scenario's own `didChangeConfiguration` before initialize is unchanged.
- **`[document]`** names the document [K1](#capabilities-and-robustness) asks about: `path` (required) is a file the
  profile's workspace provides, from the fixture or inline (`/`-separated, the same name rules), whose content must
  be UTF-8 (an inline file's text replaces a fixture file's); `language_id` (required, not blank) is its `didOpen`
  language id; `positions` (optional, one to five) are `"LINE:COLUMN"` strings, both from 1, the column counted in
  Unicode code points, each on a line the document has (the spec's line split, so the empty line after a final
  newline counts) and at most one past the line's last character. Each is converted to the negotiated encoding when
  sent. Without `positions`, K1 uses identifier-like tokens of the document; without `[document]`, its own
  `lspcheck-k1.txt`.
- **`allow.descendants`.** L9 doesn't fail for a leftover process whose name (as lspcheck reports it: on Linux the
  kernel's command name, at most 15 bytes) is in the list. A `PASS` detail names what was allowed, per scenario; a
  leftover not on the list fails as before, and the detail of that failure names the allowed ones too.
- **`allow.skip`.** Each entry needs an `id` in the catalogue and a non-empty `reason`; an id listed twice is an
  error. A skipped check reports `SKIP` with `skipped by the profile: <reason>` and doesn't affect the exit status. A
  scenario runs when one of its checks, or a cross-cutting check, is wanted (by `--only`) and not skipped, and then
  runs whole, as without the skips, so no other verdict changes; a skipped check's verdict is replaced at the end. A
  scenario whose checks are all skipped or unselected doesn't run (when no cross-cutting check needs it), and its
  skipped checks are still reported.
- **Reports.** The JSON report gains a top-level `"profile"` (the path as given, or `null`); the schema id is
  unchanged. The terminal header line ends with `— profile PATH` when one is used.
- **Replay.** A transcript's workspace line records the files at the workspace's top (as before) and every file the
  profile put there, nested ones named with `/`. Text files remain JSON strings; binary files are objects with
  `"encoding":"base64"` and a `"data"` string in the standard base64 alphabet. An optional `"directories"` array
  preserves fixture directories, including empty ones. An optional `"file_modes"` object maps file names to numeric
  Unix read, write and execute permission bits (0 through 511); replay restores them on Unix after writing each
  file. Other platforms ignore these modes. Names must match recorded files; privileged mode bits are refused.
  Old transcripts without these additions still replay with default file permissions.
  Replay validates portable names and consistent ASCII case in every shared path component across recorded files
  and directories before writing. There is no size limit. A `{"dir":"meta","configuration":…}`
  line records the settings, and replay answers `workspace/configuration` from it. `initializationOptions` and the
  `didChangeConfiguration` are in the recorded bytes. Replay's own timeouts are still its flags; it doesn't read
  profiles.

## Report
- Terminal summary in catalogue order, with counts and a line per check.
- `--json`: a versioned report; `json_report` and `SCHEMA` in [src/report.rs](../src/report.rs) define its fields and
  schema identifier.
- `--junit`: JUnit XML for CI test tabs, one testsuite per represented check area in catalogue order with counts and a
  testcase per check. Every failed check is a `<failure>`, carrying its basis (`MUST`, `SHOULD` or `POLICY`) in `type`
  and its detail, specification link and transcript reference (when recorded) in the
  body; an error is an `<error>`, and skip and not-exercised are `<skipped>`. JUnit has no severity, so the process
  exit status, not the XML, applies `--strict`. It has no time or timestamp attributes; check details can still
  vary between runs (some name the milliseconds at which evidence was seen).
  Text and attributes use XML 1.0 escaping: illegal characters become U+FFFD, and attribute LF, CR and tab characters
  become numeric character references (`&#10;`, `&#13;`, `&#9;`) to preserve arbitrary server text.
- `--markdown`: a heading with pass/fail counts and the server command in a backtick-safe code span, followed by
  the terminal report's counts line, then a table with failures
  and errors first and every cell escaped for Markdown (including literal ampersands and tildes), then the transcript
  paths of failed or errored checks. Code spans display CR/LF as `\r`/`\n` so commands and paths stay within one paragraph.
- Terminal, JUnit and Markdown evidence references use `transcript: PATH` for per-scenario failures and errors, and
  `transcripts: DIR` for cross-cutting failures and errors, using the shared parent of the recorded scenario paths.
- `--transcripts DIR`: one JSONL per session with direction, monotonic timestamp, raw bytes (base64 when not UTF-8),
  how the reader split them into frames, and stderr. Each file includes a meta line naming the workspace and the
  files it held, so a replay can recreate it. Each event takes a sequence number from a process-wide atomic counter:
  each write piece reserves one before its first byte, and a read takes one when recorded. `Session::finish` sorts
  events by this number once; serialization preserves that order. Successful writes are appended after flushing,
  so this keeps a reply read during that flush after the request it answers, even with the same timestamp.
- `lspcheck replay TRANSCRIPT -- SERVER`: re-runs one recorded session against a server. It recreates the recorded
  workspace (refusing a non-empty or symlink path; preserving an existing empty directory), re-sends every client write
  in order (gaps under 200 ms kept as pauses), and answers server-to-client requests with the same auto-answers a live run
  uses, with a profile's recorded settings for `workspace/configuration`. It reports each request's outcome against the recorded one (result / error code / no reply), the exit status
  recorded versus now, and how many server-to-client requests were answered. An outcome is the first response with
  the request's id read after the write holding the request's last byte began, before the id was next sent and before
  lspcheck began stopping the server; both runs are matched the same way. It judges no check: the summary says to
  run `lspcheck --only ID` to judge. `--transcript FILE` writes the replay's own transcript without overwriting an
  existing path. Transcripts without a workspace line replay in the current directory with a note. The transcript
  includes `{"dir":"meta","event":"stdin closed"}` when lspcheck closes stdin. Exit 0 when the replay ran, 2 for usage
  errors and unreadable transcripts.

## Architecture
Single crate, `src/` modules:
- `transport`: incremental frame reader over raw bytes (records everything, bounded frame size, typed framing
  errors), frame writer able to split or coalesce on purpose.
- `process`: spawn with a dedicated process group (Unix) or Job object (Windows). A small sentinel process joins the
  group first so the PGID can't be recycled while we scan for descendants (the reuse bug virgil-lsp hit). Kill escalation:
  SIGTERM, grace, SIGKILL. Descendant scan: Linux `/proc`, macOS `proc_listpids`, Windows job process list.
- `session`: tokio tasks for stdout (parsed there, so framing evidence is complete even if replies aren't consumed)
  and stderr (always drained); stdin writes bounded by a deadline. Generated request ids are random positive 31-bit
  integers, so a server can't prepare a reply before it sees the request (output already read when a request is sent
  can't answer it, but bytes still in the pipe can, so the randomness is what matters); ids match by **JSON value** (`1` and `1.0` are the
  same number). Scenarios can also choose explicit ids; R4 reserves its test ids before initialization so generated
  ids cannot collide with them. Each call has one absolute deadline covering its write and its reply; each scenario
  has its own timeouts and a bounded cleanup. After a write stops partway through a frame, lspcheck sends nothing more.
- `checks`: one module per area; each check declares id, basis, spec link, and a function over a `Session`.
- `capabilities`: K1's request table, positions and verdict, as pure functions.
- `metamodel`: the vendored LSP 3.17 meta-model, parsed once into typed structures, and K2's validation of JSON values
  against its types, as pure functions.
- `profile`: reads and validates a profile (`toml` with `serde`'s `deny_unknown_fields`), copies its files into each
  scenario's workspace, and merges its command and timeouts with the command line's.
- `report`, `cli` (clap).
Dependency versions, features and platform conditions are defined in [Cargo.toml](../Cargo.toml).
No network access at runtime.

## How we know it works
- `standin`: a test-only server binary (in the workspace) with fault flags, one per check:
  `--fault stdout-banner`, `char-length`, `string-id-to-number`, `reply-twice`, `reply-unsent-id`,
  `reply-to-notification`, `no-jsonrpc-field`, `result-and-error`, `wrong-unknown-error`, `exit-on-shutdown`,
  `exit-status`, `ignore-eof`, `orphan-child`, `bad-encoding`, … **Every check has a test that fails it on its fault and
  passes the clean stand-in.** That guards against false greens.
- Self-tests for the harness: partial frames, stderr floods, a server that stops reading stdin, a hung server, signals
  during spawn, cleanup after a crash.
- The [stock-server CI matrix](../ci/stock-servers/README.md) documents its baseline contract and update procedure.
  Additional coverage of the virgil-lsp release archive is still planned.
- Platforms: Linux and macOS required in CI; Windows built and tested, descendant checks there best effort at first.

## Milestones
- **M0** repository, CI (fmt, clippy, test on Linux/macOS/Windows), transport + process + session, stand-in skeleton.
- **M1** framing and lifecycle checks (F*, L*), terminal + JSON report.
- **M2** correlation, cancellation, documents (J*, C*, D*), transcripts and `replay`.
- **M3** profiles, K1, JUnit/Markdown (released in v0.2.0); stock-server CI matrix (done); v0.1.0 release with prebuilt
  binaries (Linux x86_64/aarch64 static, macOS arm64/x86_64, Windows x86_64) and checksums. crates.io publishing needs the
  maintainer's token.
- **M4** dogfood in virgil-lsp: a pinned binary download in CI (a dependency + workflow change, so a human merge there).
  Done: virgil-lsp's `lspcheck-linux` job checks its freshly built server with the pinned v0.1.0 release.
- Unreleased: K2 meta-model validation.
- Later: language profiles with seeded source mutation and recovery oracles, ported from
  `test/protocol/source_mutations.py`.

## Open questions
- MSRV policy: pin the toolchain in `rust-toolchain.toml` and support the last few stable releases.
