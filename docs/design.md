# lspcheck design

Drafted 2026-11-04. Licence: Apache-2.0. Language: Rust.

**Status:** milestones M0 and M1 are done: the transport, process, and session layers, the stand-in server, and checks F1 to F8, J4, and L1 to L9. M2 is done: J1 to J3, J5 to J7, C1 to C3, D1 to D6 and `replay`. F1, F2, F8, J1, J2, J5, J7 and L9 are judged across every scenario, and J3 across every scenario whose input is well formed. Exit waits answer server requests within their deadline; L6 and L7 timeouts are finalized after output cleanup.

## Purpose
A command-line tool that starts **any** language server binary over stdio and checks that it speaks the protocol
correctly: framing, JSON-RPC correlation, lifecycle, cancellation, document positions, and process cleanup. It needs no
knowledge of the server's language for its core checks. It reports what it checked, what it could not exercise, and a
transcript that reproduces each failure.

```
lspcheck -- gopls                          # default checks, temp workspace, no profile
lspcheck --profile go.toml -- gopls serve  # with a sample workspace and document
lspcheck --json report.json --junit junit.xml --transcripts out/ -- ./my-server --stdio
lspcheck list                              # the check catalogue with spec references
lspcheck replay out/L5.jsonl -- ./my-server   # re-run one failing session
```
Exit status: 0 when no MUST check failed; 1 when one did; 2 for a usage or checker error. `--strict` also fails on
SHOULD checks.

## Non-goals
- Not a certification or "LSP compliant" badge. Each check states its basis, and the report lists what wasn't exercised.
- Not semantic correctness of answers (hover text, definitions): that needs language oracles. Language profiles may add
  source mutation and recovery later, but not in the core.
- Not coverage-guided fuzzing (LSPFuzz does that with instrumented builds).
- Not an editor or client test.

## Verdicts
Each check has a **basis**: `MUST` (a normative spec rule), `SHOULD` (a spec recommendation), or `POLICY` (hygiene the spec
doesn't mandate, such as exiting on end of input). Each result is one of:
`pass`, `fail`, `skip` (not applicable, e.g. capability not advertised), `not-exercised` (applicable but the run produced
no evidence, e.g. no non-ASCII output was ever seen), or `error` (the checker or environment failed; never a pass).
Lessons from virgil-lsp: an unexercised check must never render as a pass, and a checker failure must keep the first
useful evidence rather than be replaced by a cleanup error.

## Check catalogue (v0.1)
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
| J3 | MUST | No response to notifications, including unknown ones and unknown `$/` ones. A response without an id (`id` missing or `null`) read after `initialized` (a reply to `exit` included), in a scenario whose input is all well formed (every one but `framing` and `pre-initialize`), answers one of lspcheck's notifications. |
| J4 | MUST | An unknown request method gets error `-32601` MethodNotFound; an unknown `$/` request too. |
| J5 | MUST | Envelopes: `"jsonrpc": "2.0"`, exactly one of `result`/`error`, error `code` an integer and `message` a string; request ids integers or strings, response ids integers, strings or `null`, integers (ids and error codes) within LSP's signed 32-bit range; `params`, when present, an object or an array. |
| J6 | MUST | Concurrent requests (sent before any reply) are each answered with the right id; order may differ. |
| J7 | MUST | Server-to-client requests don't reuse an unanswered id, and the server keeps serving after lspcheck answers them (judging rules below). The id's type and range are J5's. |

J1, J2, J5 and J7 are judged across every scenario from the responses and messages seen. The `json-rpc` scenario exercises J6
and J3: after `initialize`, four requests are written before any reply is read (two numeric ids, one numeric string and
one non-numeric string), and each must be answered with its exact id in any order; then two unknown notifications are
followed by two probes; this guarantees J3 is exercised. Across the other scenarios with well-formed input, any response
without an id read after `initialized` fails J3 too, since it can only answer a notification (C3 leaves to J3 the ones
it can't attribute to a cancel), until a write of lspcheck's stops partway through a frame: the server may answer
that malformed input with a null-id ParseError. A frame lspcheck skipped or couldn't read since `initialized`, or such a
write, makes a pass an error. A frame
lspcheck couldn't read anywhere (F1) also makes a J1, J2, J5 or J7 pass an error: it might have held what they judge.

J7 treats an earlier request as unanswered when lspcheck never wrote its whole answer. For a completed answer,
J7 times a reuse against when lspcheck began writing it: only a request read before then was certainly sent before
the server could have had the answer. Reusing an id at or after that time is treated as lawful if the earlier answer
was written in full. lspcheck's answers are fixed: `null` for each
`workspace/configuration` item, the workspace for `workspace/workspaceFolders`, `applied: false` and
`success: false` for `workspace/applyEdit` and `window/showDocument`, `null` for registrations, progress tokens,
`window/showMessageRequest` and the refresh requests, and MethodNotFound otherwise; a profile will make them
configurable (`[client]` below). A server that stops answering after an answer may have stopped for another reason,
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
  lspcheck sends only `initialized` and the cancels. C3 fails on one read after the first cancel was written whose
  text names `$/cancelRequest` (as a MethodNotFound message usually does); any other is `error` for C3, since it may
  answer `initialized` (answering any notification is J3's failure).

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

### Documents and positions (richer with a profile document)
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

Without a profile, most language servers ignore a plain-text document, so expect D1 to be `N/E` for them; D2 needs
only one versioned publish for the document, so it is exercised wherever diagnostics are labelled. D4 runs wherever
a document request is advertised.

### Capabilities and robustness
| ID | Basis | Check |
|---|---|---|
| K1 | MUST | Every advertised request capability with a position or document parameter answers (not MethodNotFound) at profile positions or generic ones. |
| K2 | MUST | Replies validate against the LSP 3.17 metaModel (vendored, CC BY 4.0 attribution). **v0.2.** |
| R1 | POLICY | A well-framed body that isn't JSON gets ParseError `-32700` with id `null` or is ignored, and the server keeps serving. |
| R2 | POLICY | Wrong param types get an error (InvalidParams preferred), not a crash. |
| R3 | POLICY | A large document (default 4 MiB, configurable) is accepted or refused with an error; the server keeps serving. |
| R4 | POLICY | Unusual but valid ids (0, negative, 2^53, long and non-ASCII strings) round-trip. |

## Profiles (optional TOML)
```toml
[server]
command = ["gopls"]          # or given after `--`
startup_timeout = "30s"
request_timeout = "10s"

[workspace]                 # copied into a fresh temp dir per session; rootUri and workspaceFolders point there
path = "fixtures/go"        # or inline:
files = { "go.mod" = "module example\n" }

[document]
path = "main.go"
language_id = "go"
positions = ["main.go:3:6"] # where K1 probes; otherwise identifier-looking tokens are used

[client]
initialization_options = {}
configuration = {}          # answers to workspace/configuration
capabilities = "default"    # or a path to a capabilities JSON

[allow]
descendants = []            # process names allowed to outlive exit
skip = []                   # check ids, with a reason each
```
Without a profile: an empty temp workspace and a `plaintext` document. Generic checks still run; position checks report
`not-exercised` where the server produced no positions.

## Report
- Terminal summary grouped by area, with counts and a line per failure naming the transcript.
- `--json`: versioned `report-v1` schema (checks, basis, verdict, evidence, timings, server argv, checker version,
  host OS/arch, profile hash). `--junit` for CI test tabs; `--markdown` for a GitHub step summary.
- `--transcripts DIR`: one JSONL per session with direction, monotonic timestamp, raw bytes (base64 when not UTF-8),
  how the reader split them into frames, and stderr. Each file includes a meta line naming the workspace and the
  files it held, so a replay can recreate it.
- `lspcheck replay TRANSCRIPT -- SERVER`: re-runs one recorded session against a server. It recreates the recorded
  workspace (refusing a non-empty or symlink path; preserving an existing empty directory), re-sends every client write
  in order (gaps under 200 ms kept as pauses), and answers server-to-client requests with the same auto-answers a live run
  uses. It reports each request's outcome against the recorded one (result / error code / no reply), the exit status
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
  and stderr (always drained); stdin writes bounded by a deadline. Request ids are random 31-bit integers, so a server
  can't prepare a reply before it sees the request (output already read when a request is sent can't answer it, but
  bytes still in the pipe can, so the randomness is what matters); ids match by **JSON value** (`1` and `1.0` are the
  same number). Each call has one absolute deadline covering its write and its reply; each scenario has its own
  timeouts and a bounded cleanup. After a write stops partway through a frame, lspcheck sends nothing more.
- `checks`: one module per area; each check declares id, basis, spec link, and a function over a `Session`.
- `profile`, `report`, `cli` (clap).
Dependencies: tokio, serde, serde_json (`raw_value`, `preserve_order`), clap, toml, thiserror; `nix` on Unix,
`windows-sys` on Windows. No network access at runtime.

## How we know it works
- `standin`: a test-only server binary (in the workspace) with fault flags, one per check:
  `--fault stdout-banner`, `char-length`, `string-id-to-number`, `reply-twice`, `reply-unsent-id`,
  `reply-to-notification`, `no-jsonrpc-field`, `result-and-error`, `wrong-unknown-error`, `exit-on-shutdown`,
  `exit-status`, `ignore-eof`, `orphan-child`, `bad-encoding`, … **Every check has a test that fails it on its fault and
  passes the clean stand-in.** That guards against false greens.
- Self-tests for the harness: partial frames, stderr floods, a server that stops reading stdin, a hung server, signals
  during spawn, cleanup after a crash.
- Real servers in CI (pinned versions; results recorded as expected-results files, not required to all pass):
  lua-language-server, gopls, basedpyright, rust-analyzer, and the virgil-lsp release archive.
- Platforms: Linux and macOS required in CI; Windows built and tested, descendant checks there best effort at first.

## Milestones
- **M0** repository, CI (fmt, clippy, test on Linux/macOS/Windows), transport + process + session, stand-in skeleton.
- **M1** framing and lifecycle checks (F*, L*), terminal + JSON report.
- **M2** correlation, cancellation, documents (J*, C*, D*), transcripts and `replay`.
- **M3** profiles, K1, R*, JUnit/Markdown; stock-server CI matrix; v0.1.0 release with prebuilt binaries
  (Linux x86_64/aarch64 static, macOS arm64/x86_64, Windows x86_64) and checksums. crates.io publishing needs the
  maintainer's token.
- **M4** dogfood in virgil-lsp: a pinned binary download in CI (a dependency + workflow change, so a human merge there).
- Later: K2 metaModel validation; language profiles with seeded source mutation and recovery oracles, ported from
  `test/protocol/source_mutations.py`.

## Open questions
- Whether findings in other projects' servers from the CI matrix are reported to them (outward-facing; ask first).
- MSRV policy: pin the toolchain in `rust-toolchain.toml` and support the last few stable releases.
