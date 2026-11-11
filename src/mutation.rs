// Copyright 2026 The lspcheck Authors.
// SPDX-License-Identifier: Apache-2.0

//! The planner of seeded source mutation ([design](../docs/source-mutations.md)):
//! the generator, the lexicon that says where tokens are, the operators, the
//! plan of a case as a pure function of the seed and the clean texts, and the
//! content changes that send a planned edit to a server.
//!
//! Pure functions only: no I/O, no session. Nothing runs a plan yet; the
//! `mutation` scenario that will is a later step.
//!
//! The generator, the order of its draws and what each operator does are a
//! [`FORMAT`]: the same seed, case, texts and configuration give the same
//! plan, byte for byte, under the same format. Changing any of them changes
//! what a seed means, so the format number goes up with it.

use crate::text::{self, ContentChange, Encoding, Position, Range};

/// The mutation format: the generator, its draw order and the operators.
/// Recorded with every plan so a seed reproduces its plan.
pub const FORMAT: u32 = 1;

/// Most mutations in one step.
pub const MAX_MUTATIONS_PER_STEP: usize = 3;
/// Most steps in one case.
pub const MAX_STEPS: usize = 3;
/// The smallest `max_document_bytes`: room for any one character, so that
/// the last resort, a bracket inserted into an emptied document, always fits.
pub const MIN_DOCUMENT_BYTES: usize = 4;
/// Most bytes a mutation other than a stress one may add to a document.
pub const INSERT_CAP: usize = 1024;
/// Most bytes a stress mutation may add to a document.
pub const STRESS_CAP: usize = 16 * 1024;
/// Operators drawn before an oversized result gives way to `truncate`.
const SIZE_TRIES: usize = 6;

/// The characters `unicode` and `insert-in-literal` insert: 2-, 3- and 4-byte
/// UTF-8, two characters outside the Basic Multilingual Plane (a surrogate
/// pair in UTF-16), a combining acute, a ZWJ, a no-break space, a zero-width
/// space, a byte-order mark and the last code point.
pub const UNICODE_SET: [char; 10] = [
    '\u{E9}',
    '\u{4E2D}',
    '\u{1F600}',
    '\u{1D518}',
    '\u{301}',
    '\u{200D}',
    '\u{A0}',
    '\u{200B}',
    '\u{FEFF}',
    '\u{10FFFF}',
];

/// SplitMix64: a 64-bit state advanced by a constant and mixed on output.
/// Its stream is fixed by its definition, unlike a crate's generator, which
/// may change between releases.
#[derive(Debug, Clone)]
pub struct SplitMix64 {
    state: u64,
}

const GOLDEN_GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;

/// SplitMix64's output function, which also mixes the seeds of a stream.
fn mix64(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// FNV-1a over a stream's label.
fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xCBF2_9CE4_8422_2325, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01B3))
}

impl SplitMix64 {
    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// The stream of one case and purpose, such as `"edits"` or
    /// `"requests"`. Each case has its own streams, so a case doesn't depend
    /// on the cases before it, and drawing requests can't change the edits.
    pub fn stream(seed: u64, case: u64, label: &str) -> Self {
        Self::new(mix64(mix64(mix64(seed) ^ case) ^ fnv1a64(label.as_bytes())))
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(GOLDEN_GAMMA);
        mix64(self.state)
    }

    /// A number in `0..n`, uniformly: draws below `2^64 mod n` are rejected,
    /// so no value is likelier than another. `n` must not be zero.
    pub fn below(&mut self, n: usize) -> usize {
        assert!(n > 0, "below(0)");
        let n = n as u64;
        let threshold = n.wrapping_neg() % n;
        loop {
            let x = self.next_u64();
            if x >= threshold {
                return (x % n) as usize;
            }
        }
    }

    /// A number in `lo..=hi`.
    pub fn between(&mut self, lo: usize, hi: usize) -> usize {
        assert!(lo <= hi, "between({lo}, {hi})");
        lo + self.below(hi - lo + 1)
    }

    /// One element of a non-empty slice.
    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

/// A string literal's delimiters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StringDelimiters {
    pub open: String,
    pub close: String,
    /// A character that makes the next one part of the string.
    pub escape: Option<char>,
}

/// Where tokens are. It only steers where edits land: no verdict depends on
/// it, so a wrong lexicon makes edits less pointed but can't make a verdict
/// wrong.
///
/// Identifiers are runs of Unicode letters, digits and `identifier_chars`
/// that don't start with an ASCII digit; numbers are such runs that do.
/// Whitespace other than line ends is one token per run, each line end
/// (`\r\n`, `\n` or `\r`) one token, and every other character a token of its
/// own. A string runs to its closing delimiter, across lines if need be; one
/// never closed runs to the end of its line. A block comment never closed
/// runs to the end of the document. Where several delimiters start at the
/// same place, the longest wins; of equal ones, a block comment, then a line
/// comment, then a string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lexicon {
    pub line_comments: Vec<String>,
    pub block_comments: Vec<(String, String)>,
    pub strings: Vec<StringDelimiters>,
    pub identifier_chars: String,
    pub brackets: Vec<(char, char)>,
    pub keywords: Vec<String>,
}

impl Default for Lexicon {
    /// The lexicon used without one in the profile: identifiers with `_`,
    /// the brackets `()`, `[]` and `{}`, and no comments, strings or keywords.
    fn default() -> Self {
        Self {
            line_comments: Vec::new(),
            block_comments: Vec::new(),
            strings: Vec::new(),
            identifier_chars: "_".to_owned(),
            brackets: vec![('(', ')'), ('[', ']'), ('{', '}')],
            keywords: Vec::new(),
        }
    }
}

/// What a token is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Identifier,
    Keyword,
    Number,
    /// A string literal; `open` and `close` are its delimiters' lengths in
    /// bytes, `close` zero when it was never closed.
    String {
        open: usize,
        close: usize,
    },
    /// A comment; `close` is zero for a line comment or a block comment
    /// never closed.
    Comment {
        block: bool,
        open: usize,
        close: usize,
    },
    Whitespace,
    LineEnd,
    Punctuation,
}

/// A token: its kind and its byte range in the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub start: usize,
    pub end: usize,
}

impl Token {
    fn text<'a>(&self, text: &'a str) -> &'a str {
        &text[self.start..self.end]
    }

    fn is_space(&self) -> bool {
        matches!(self.kind, TokenKind::Whitespace | TokenKind::LineEnd)
    }

    /// The bytes between a string's or comment's delimiters.
    fn body(&self) -> Option<(usize, usize)> {
        match self.kind {
            TokenKind::String { open, close } | TokenKind::Comment { open, close, .. } => {
                Some((self.start + open, self.end - close))
            }
            _ => None,
        }
    }
}

/// The end of the line that `from` is on: the start of its line end, or the
/// end of the text.
fn line_content_end(text: &str, from: usize) -> usize {
    text[from..].find(['\r', '\n']).map_or(text.len(), |i| from + i)
}

/// The end of a string opened at `body`: just past its closing delimiter, or
/// the end of its line when it is never closed, with the closing length.
///
/// `unclosed_from` is where an earlier search for the same delimiter ran to
/// the end of the text without a close; a string opened after it is taken as
/// never closed without searching again, which keeps lexing linear when many
/// strings are left open.
fn string_end(
    text: &str,
    body: usize,
    delimiters: &StringDelimiters,
    unclosed_from: &mut Option<usize>,
) -> (usize, usize) {
    if unclosed_from.is_some_and(|from| from <= body) {
        return (line_content_end(text, body), 0);
    }
    let mut i = body;
    while i < text.len() {
        if text[i..].starts_with(&delimiters.close) {
            return (i + delimiters.close.len(), delimiters.close.len());
        }
        let c = text[i..].chars().next().expect("char boundary");
        i += c.len_utf8();
        if Some(c) == delimiters.escape && i < text.len() {
            i += text[i..].chars().next().expect("char boundary").len_utf8();
        }
    }
    *unclosed_from = Some(body);
    (line_content_end(text, body), 0)
}

impl Lexicon {
    fn is_word_char(&self, c: char) -> bool {
        c.is_alphanumeric() || self.identifier_chars.contains(c)
    }

    fn is_bracket(&self, c: char) -> bool {
        self.brackets.iter().any(|&(o, k)| c == o || c == k)
    }

    /// Splits a text into tokens that cover it, in order, with no gaps.
    pub fn tokens(&self, text: &str) -> Vec<Token> {
        let mut tokens = Vec::new();
        let mut unclosed_from = vec![None; self.strings.len()];
        let mut i = 0;
        while i < text.len() {
            let (kind, end) = self.token_at(text, i, &mut unclosed_from);
            tokens.push(Token { kind, start: i, end });
            i = end;
        }
        tokens
    }

    fn token_at(&self, text: &str, i: usize, unclosed_from: &mut [Option<usize>]) -> (TokenKind, usize) {
        let rest = &text[i..];
        // The longest delimiter that starts here; equal lengths prefer a
        // block comment (0), then a line comment (1), then a string (2).
        let mut best: Option<(usize, usize, usize)> = None; // (length, rank, index)
        let mut consider = |len: usize, rank: usize, index: usize| {
            if len > 0 && best.is_none_or(|(l, r, _)| len > l || (len == l && rank < r)) {
                best = Some((len, rank, index));
            }
        };
        for (n, (open, _)) in self.block_comments.iter().enumerate() {
            if rest.starts_with(open.as_str()) {
                consider(open.len(), 0, n);
            }
        }
        for (n, open) in self.line_comments.iter().enumerate() {
            if rest.starts_with(open.as_str()) {
                consider(open.len(), 1, n);
            }
        }
        for (n, s) in self.strings.iter().enumerate() {
            if rest.starts_with(s.open.as_str()) {
                consider(s.open.len(), 2, n);
            }
        }
        match best {
            Some((open, 0, n)) => {
                let close = &self.block_comments[n].1;
                return match text[i + open..].find(close.as_str()) {
                    Some(at) => {
                        (TokenKind::Comment { block: true, open, close: close.len() }, i + open + at + close.len())
                    }
                    None => (TokenKind::Comment { block: true, open, close: 0 }, text.len()),
                };
            }
            Some((open, 1, _)) => {
                return (TokenKind::Comment { block: false, open, close: 0 }, line_content_end(text, i));
            }
            Some((open, _, n)) => {
                let (end, close) = string_end(text, i + open, &self.strings[n], &mut unclosed_from[n]);
                return (TokenKind::String { open, close }, end);
            }
            None => {}
        }
        if rest.starts_with("\r\n") {
            return (TokenKind::LineEnd, i + 2);
        }
        let c = rest.chars().next().expect("char boundary");
        if c == '\r' || c == '\n' {
            return (TokenKind::LineEnd, i + 1);
        }
        let run = |pred: &dyn Fn(char) -> bool| i + rest.find(|c: char| !pred(c)).unwrap_or(rest.len());
        if c.is_whitespace() {
            return (TokenKind::Whitespace, run(&|c| c.is_whitespace() && c != '\r' && c != '\n'));
        }
        if self.is_word_char(c) {
            let end = run(&|c| self.is_word_char(c));
            let kind = if c.is_ascii_digit() {
                TokenKind::Number
            } else if self.keywords.iter().any(|k| k == &text[i..end]) {
                TokenKind::Keyword
            } else {
                TokenKind::Identifier
            };
            return (kind, end);
        }
        (TokenKind::Punctuation, i + c.len_utf8())
    }
}

/// An edit operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Operator {
    DeleteTokens,
    DuplicateTokens,
    SwapTokens,
    ReplaceToken,
    Truncate,
    DeleteLine,
    DuplicateLine,
    UnbalanceBracket,
    OpenString,
    OpenComment,
    InsertInLiteral,
    Unicode,
    LineEnds,
    Rename,
    Fragment,
    Nest,
    RepeatLine,
    Control,
}

impl Operator {
    pub const ALL: [Operator; 18] = [
        Self::DeleteTokens,
        Self::DuplicateTokens,
        Self::SwapTokens,
        Self::ReplaceToken,
        Self::Truncate,
        Self::DeleteLine,
        Self::DuplicateLine,
        Self::UnbalanceBracket,
        Self::OpenString,
        Self::OpenComment,
        Self::InsertInLiteral,
        Self::Unicode,
        Self::LineEnds,
        Self::Rename,
        Self::Fragment,
        Self::Nest,
        Self::RepeatLine,
        Self::Control,
    ];

    /// The operator's name in profiles and records.
    pub fn name(self) -> &'static str {
        match self {
            Self::DeleteTokens => "delete-tokens",
            Self::DuplicateTokens => "duplicate-tokens",
            Self::SwapTokens => "swap-tokens",
            Self::ReplaceToken => "replace-token",
            Self::Truncate => "truncate",
            Self::DeleteLine => "delete-line",
            Self::DuplicateLine => "duplicate-line",
            Self::UnbalanceBracket => "unbalance-bracket",
            Self::OpenString => "open-string",
            Self::OpenComment => "open-comment",
            Self::InsertInLiteral => "insert-in-literal",
            Self::Unicode => "unicode",
            Self::LineEnds => "line-ends",
            Self::Rename => "rename",
            Self::Fragment => "fragment",
            Self::Nest => "nest",
            Self::RepeatLine => "repeat-line",
            Self::Control => "control",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|o| o.name() == name)
    }

    /// Stress operators insert up to the room left in the document, not
    /// [`INSERT_CAP`].
    pub fn is_stress(self) -> bool {
        matches!(self, Self::Nest | Self::RepeatLine)
    }

    /// Whether a profile gets this operator without naming it: all but the
    /// stress operators and `control`.
    pub fn is_default(self) -> bool {
        !matches!(self, Self::Nest | Self::RepeatLine | Self::Control)
    }

    /// The operators a profile gets without naming any.
    pub fn defaults() -> Vec<Self> {
        Self::ALL.into_iter().filter(|o| o.is_default()).collect()
    }
}

/// What the planner may do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Most steps per case, 1 to [`MAX_STEPS`].
    pub steps: usize,
    /// The operators drawn from, each equally likely. Not empty.
    pub operators: Vec<Operator>,
    /// What `fragment` inserts.
    pub fragments: Vec<String>,
    /// Most bytes a mutated document may hold, at least
    /// [`MIN_DOCUMENT_BYTES`]. A seed within it stays within it.
    pub max_document_bytes: usize,
    pub lexicon: Lexicon,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            steps: MAX_STEPS,
            operators: Operator::defaults(),
            fragments: Vec::new(),
            max_document_bytes: 64 * 1024,
            lexicon: Lexicon::default(),
        }
    }
}

/// One planned edit: `removed` bytes at byte `offset` of the text before it
/// are replaced by `inserted`. An operator that changes several places
/// (`rename`, `line-ends`) is one edit from the first change to the last.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mutation {
    pub operator: Operator,
    /// The operator found nothing to change, so a token was inserted instead.
    pub fallback: bool,
    pub offset: usize,
    pub removed: usize,
    pub inserted: String,
}

impl Mutation {
    /// The text after this edit.
    pub fn apply(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len() + self.inserted.len());
        out.push_str(&text[..self.offset]);
        out.push_str(&self.inserted);
        out.push_str(&text[self.offset + self.removed..]);
        out
    }
}

/// One step of a case: mutations of one document, sent in one `didChange`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// The index of the document in the texts the case was planned from.
    pub document: usize,
    pub mutations: Vec<Mutation>,
}

/// The plan of one case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Case {
    pub index: u64,
    pub steps: Vec<Step>,
}

/// Plans case `index` of a run seeded with `seed`, over the clean texts of
/// the seed documents (not empty). A pure function: its edits come only from
/// the case's `"edits"` stream, the texts and the configuration.
///
/// Draw order (part of [`FORMAT`]): the number of steps; then for each step
/// its document and its number of mutations; then for each mutation its
/// operator and the operator's own draws. An oversized result draws a new
/// operator, up to six times, then `truncate` makes it.
pub fn plan_case(seed: u64, index: u64, texts: &[String], config: &Config) -> Case {
    assert!(!texts.is_empty(), "no seed documents");
    assert!(!config.operators.is_empty(), "no operators");
    assert!(config.max_document_bytes >= MIN_DOCUMENT_BYTES, "max_document_bytes below {MIN_DOCUMENT_BYTES}");
    let mut rng = SplitMix64::stream(seed, index, "edits");
    let mut current: Vec<String> = texts.to_vec();
    let steps = rng.between(1, config.steps.clamp(1, MAX_STEPS));
    let mut planned = Vec::with_capacity(steps);
    for _ in 0..steps {
        let document = rng.below(texts.len());
        let count = rng.between(1, MAX_MUTATIONS_PER_STEP);
        let mut mutations = Vec::with_capacity(count);
        for _ in 0..count {
            let m = plan_mutation(&mut rng, &current[document], config);
            current[document] = m.apply(&current[document]);
            mutations.push(m);
        }
        planned.push(Step { document, mutations });
    }
    Case { index, steps: planned }
}

/// One mutation of `text`, within the document size.
fn plan_mutation(rng: &mut SplitMix64, text: &str, config: &Config) -> Mutation {
    let fits = |m: &Mutation| text.len() - m.removed + m.inserted.len() <= config.max_document_bytes;
    for _ in 0..SIZE_TRIES {
        let operator = *rng.pick(&config.operators);
        let m = apply_operator(rng, text, operator, config);
        if fits(&m) {
            return m;
        }
    }
    apply_operator(rng, text, Operator::Truncate, config)
}

/// An edit as (offset, removed, inserted).
type Splice = (usize, usize, String);

/// What one operator does to `text`, or the fallback insertion when it finds
/// nothing to change.
fn apply_operator(rng: &mut SplitMix64, text: &str, operator: Operator, config: &Config) -> Mutation {
    let lexicon = &config.lexicon;
    let tokens = lexicon.tokens(text);
    let ctx = Context { text, tokens: &tokens, lexicon, room: config.max_document_bytes.saturating_sub(text.len()) };
    let splice = match operator {
        Operator::DeleteTokens => ctx.delete_tokens(rng),
        Operator::DuplicateTokens => ctx.duplicate_tokens(rng),
        Operator::SwapTokens => ctx.swap_tokens(rng),
        Operator::ReplaceToken => ctx.replace_token(rng),
        Operator::Truncate => ctx.truncate(rng),
        Operator::DeleteLine => ctx.delete_line(rng),
        Operator::DuplicateLine => ctx.duplicate_line(rng),
        Operator::UnbalanceBracket => ctx.unbalance_bracket(rng),
        Operator::OpenString => ctx.open_string(rng),
        Operator::OpenComment => ctx.open_comment(rng),
        Operator::InsertInLiteral => ctx.insert_in_literal(rng),
        Operator::Unicode => {
            let c = *rng.pick(&UNICODE_SET);
            ctx.insert_at_boundary(rng, c.to_string())
        }
        Operator::LineEnds => ctx.line_ends(rng),
        Operator::Rename => ctx.rename(rng),
        Operator::Fragment => ctx.fragment(rng, &config.fragments),
        Operator::Nest => ctx.nest(rng),
        Operator::RepeatLine => ctx.repeat_line(rng),
        Operator::Control => {
            let controls: Vec<char> = ('\0'..='\u{1F}').filter(|c| !matches!(c, '\t' | '\n' | '\r')).collect();
            let c = *rng.pick(&controls);
            ctx.insert_at_boundary(rng, c.to_string())
        }
    };
    match splice {
        Some((offset, removed, inserted)) => Mutation { operator, fallback: false, offset, removed, inserted },
        None => {
            let (offset, removed, inserted) = ctx.fallback(rng);
            Mutation { operator, fallback: true, offset, removed, inserted }
        }
    }
}

struct Context<'a> {
    text: &'a str,
    tokens: &'a [Token],
    lexicon: &'a Lexicon,
    /// Bytes the document may still grow by.
    room: usize,
}

/// The byte range of every line end of `text`, `\r\n`, `\n` or `\r`, in order,
/// wherever it is: between tokens or inside a string or block comment.
fn line_ends(text: &str) -> Vec<(usize, usize)> {
    let b = text.as_bytes();
    let mut ends = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\r' if b.get(i + 1) == Some(&b'\n') => {
                ends.push((i, i + 2));
                i += 2;
            }
            b'\r' | b'\n' => {
                ends.push((i, i + 1));
                i += 1;
            }
            _ => i += 1,
        }
    }
    ends
}

/// Whether byte `i` of `text` falls between a `\r` and a `\n`.
fn inside_crlf(text: &str, i: usize) -> bool {
    let b = text.as_bytes();
    i > 0 && i < b.len() && b[i - 1] == b'\r' && b[i] == b'\n'
}

impl Context<'_> {
    fn cap(&self) -> usize {
        INSERT_CAP.min(self.room)
    }

    /// Token starts and the end of the text: where an insertion lands
    /// between tokens. Never between a `\r` and a `\n`, which are one token.
    fn boundaries(&self) -> Vec<usize> {
        self.tokens.iter().map(|t| t.start).chain([self.text.len()]).collect()
    }

    fn insert_at_boundary(&self, rng: &mut SplitMix64, inserted: String) -> Option<Splice> {
        if inserted.len() > self.cap() {
            return None;
        }
        let at = *rng.pick(&self.boundaries());
        Some((at, 0, inserted))
    }

    /// The fallback: a copy of one of the document's own tokens at a token
    /// boundary, or, with none that fits, the lexicon's first opening
    /// bracket (else `(`) at offset 0.
    fn fallback(&self, rng: &mut SplitMix64) -> Splice {
        let fitting: Vec<&Token> = self.tokens.iter().filter(|t| t.end - t.start <= self.cap()).collect();
        if fitting.is_empty() {
            let bracket = self.lexicon.brackets.first().map_or('(', |&(open, _)| open);
            return (0, 0, bracket.to_string());
        }
        let token = rng.pick(&fitting).text(self.text).to_owned();
        let at = *rng.pick(&self.boundaries());
        (at, 0, token)
    }

    fn non_space(&self) -> Vec<&Token> {
        self.tokens.iter().filter(|t| !t.is_space()).collect()
    }

    fn delete_tokens(&self, rng: &mut SplitMix64) -> Option<Splice> {
        let words = self.non_space();
        if words.is_empty() {
            return None;
        }
        let first = rng.below(words.len());
        let count = rng.between(1, 3).min(words.len() - first);
        let (start, end) = (words[first].start, words[first + count - 1].end);
        Some((start, end - start, String::new()))
    }

    fn duplicate_tokens(&self, rng: &mut SplitMix64) -> Option<Splice> {
        let cap = self.cap();
        let starts: Vec<usize> =
            (0..self.tokens.len()).filter(|&i| self.tokens[i].end - self.tokens[i].start <= cap).collect();
        if starts.is_empty() {
            return None;
        }
        let first = *rng.pick(&starts);
        let mut count = rng.between(1, 4).min(self.tokens.len() - first);
        while self.tokens[first + count - 1].end - self.tokens[first].start > cap {
            count -= 1;
        }
        let (start, end) = (self.tokens[first].start, self.tokens[first + count - 1].end);
        Some((end, 0, self.text[start..end].to_owned()))
    }

    fn swap_tokens(&self, rng: &mut SplitMix64) -> Option<Splice> {
        let class = |t: &Token| match t.kind {
            TokenKind::Identifier => Some(0),
            TokenKind::Number => Some(1),
            TokenKind::String { .. } => Some(2),
            _ => None,
        };
        // Classes with two tokens of different text.
        let groups: Vec<Vec<&Token>> = (0..3)
            .map(|c| self.tokens.iter().filter(|t| class(t) == Some(c)).collect::<Vec<_>>())
            .filter(|g| g.iter().any(|t| t.text(self.text) != g[0].text(self.text)))
            .collect();
        if groups.is_empty() {
            return None;
        }
        let group = rng.pick(&groups);
        let a = *rng.pick(group);
        let others: Vec<&Token> = group.iter().copied().filter(|t| t.text(self.text) != a.text(self.text)).collect();
        let b = *rng.pick(&others);
        let (first, second) = if a.start < b.start { (a, b) } else { (b, a) };
        let swapped =
            format!("{}{}{}", second.text(self.text), &self.text[first.end..second.start], first.text(self.text));
        // Adjacent tokens can swap into the same text: `''` and `'` in `'''`.
        if swapped == self.text[first.start..second.end] {
            return None;
        }
        Some((first.start, second.end - first.start, swapped))
    }

    fn replace_token(&self, rng: &mut SplitMix64) -> Option<Splice> {
        let words = self.non_space();
        if words.is_empty() {
            return None;
        }
        let target = *rng.pick(&words);
        let old = target.text(self.text);
        let cap = self.cap() + old.len();
        let mut pool: Vec<String> = words.iter().map(|t| t.text(self.text).to_owned()).collect();
        for &(open, close) in &self.lexicon.brackets {
            pool.push(open.to_string());
            pool.push(close.to_string());
        }
        pool.extend(self.lexicon.keywords.iter().cloned());
        pool.retain(|p| p != old && p.len() <= cap);
        if pool.is_empty() {
            return None;
        }
        Some((target.start, old.len(), rng.pick(&pool).clone()))
    }

    fn truncate(&self, rng: &mut SplitMix64) -> Option<Splice> {
        if self.tokens.is_empty() {
            return None;
        }
        let at = rng.pick(self.tokens).start;
        Some((at, self.text.len() - at, String::new()))
    }

    /// Each line's byte range with its line end. Lines come from the text,
    /// not the tokens: a string or block comment may hold line ends.
    fn lines(&self) -> Vec<(usize, usize)> {
        let mut lines = Vec::new();
        let mut start = 0;
        for (_, end) in line_ends(self.text) {
            lines.push((start, end));
            start = end;
        }
        if start < self.text.len() {
            lines.push((start, self.text.len()));
        }
        lines
    }

    fn delete_line(&self, rng: &mut SplitMix64) -> Option<Splice> {
        let lines = self.lines();
        if lines.is_empty() {
            return None;
        }
        let (start, end) = *rng.pick(&lines);
        Some((start, end - start, String::new()))
    }

    /// The line end a copied last line gets: the document's last, else `\n`.
    fn some_line_end(&self) -> &str {
        line_ends(self.text).last().map_or("\n", |&(start, end)| &self.text[start..end])
    }

    /// Where a copy of each line goes, and the copy: after a line with its
    /// end, or before the last line, with a line end added, when it has none.
    fn line_copies(&self, longest: usize) -> Vec<(usize, String)> {
        self.lines()
            .into_iter()
            .map(|(start, end)| {
                let line = &self.text[start..end];
                if line.ends_with(['\r', '\n']) {
                    (end, line.to_owned())
                } else {
                    (start, format!("{line}{}", self.some_line_end()))
                }
            })
            .filter(|(_, copy)| copy.len() <= longest)
            .collect()
    }

    fn duplicate_line(&self, rng: &mut SplitMix64) -> Option<Splice> {
        let copies = self.line_copies(self.cap());
        if copies.is_empty() {
            return None;
        }
        let (at, copy) = rng.pick(&copies).clone();
        Some((at, 0, copy))
    }

    fn unbalance_bracket(&self, rng: &mut SplitMix64) -> Option<Splice> {
        if self.lexicon.brackets.is_empty() || self.cap() == 0 {
            return None;
        }
        let present: Vec<&Token> = self
            .tokens
            .iter()
            .filter(|t| {
                t.kind == TokenKind::Punctuation
                    && self.lexicon.is_bracket(self.text[t.start..].chars().next().unwrap())
            })
            .collect();
        // 0 deletes a bracket, 1 duplicates one, 2 inserts one.
        let action = if present.is_empty() { 2 } else { rng.below(3) };
        match action {
            0 => {
                let t = *rng.pick(&present);
                Some((t.start, t.end - t.start, String::new()))
            }
            1 => {
                let t = *rng.pick(&present);
                Some((t.end, 0, t.text(self.text).to_owned()))
            }
            _ => {
                let chars: Vec<char> = self.lexicon.brackets.iter().flat_map(|&(o, c)| [o, c]).collect();
                let c = *rng.pick(&chars);
                self.insert_at_boundary(rng, c.to_string())
            }
        }
    }

    /// Removes a closed literal's closing delimiter, or inserts a lone
    /// opening one at a token boundary.
    fn open_literal(&self, rng: &mut SplitMix64, closed: &[&Token], openers: &[String]) -> Option<Splice> {
        if openers.is_empty() {
            return None;
        }
        if !closed.is_empty() && rng.below(2) == 0 {
            let t = *rng.pick(closed);
            let (_, body_end) = t.body().expect("a literal");
            return Some((body_end, t.end - body_end, String::new()));
        }
        let open = rng.pick(openers).clone();
        self.insert_at_boundary(rng, open)
    }

    fn open_string(&self, rng: &mut SplitMix64) -> Option<Splice> {
        let closed: Vec<&Token> =
            self.tokens.iter().filter(|t| matches!(t.kind, TokenKind::String { close, .. } if close > 0)).collect();
        let openers: Vec<String> = self.lexicon.strings.iter().map(|s| s.open.clone()).collect();
        self.open_literal(rng, &closed, &openers)
    }

    fn open_comment(&self, rng: &mut SplitMix64) -> Option<Splice> {
        let closed: Vec<&Token> = self
            .tokens
            .iter()
            .filter(|t| matches!(t.kind, TokenKind::Comment { block: true, close, .. } if close > 0))
            .collect();
        let openers: Vec<String> = self.lexicon.block_comments.iter().map(|(o, _)| o.clone()).collect();
        self.open_literal(rng, &closed, &openers)
    }

    /// 1 to 3 characters of [`UNICODE_SET`] at a character boundary inside a
    /// string's or comment's body, else strictly inside an identifier.
    fn insert_in_literal(&self, rng: &mut SplitMix64) -> Option<Splice> {
        let literals: Vec<(usize, usize)> = self.tokens.iter().filter_map(Token::body).collect();
        let places: Vec<(usize, usize)> = if literals.is_empty() {
            // Identifiers of two characters or more, without their ends.
            self.tokens
                .iter()
                .filter(|t| t.kind == TokenKind::Identifier && t.text(self.text).chars().nth(1).is_some())
                .map(|t| {
                    let first = t.text(self.text).chars().next().unwrap().len_utf8();
                    let last = t.text(self.text).chars().next_back().unwrap().len_utf8();
                    (t.start + first, t.end - last)
                })
                .collect()
        } else {
            literals
        };
        if places.is_empty() {
            return None;
        }
        let (start, end) = *rng.pick(&places);
        let offsets: Vec<usize> =
            (start..=end).filter(|&i| self.text.is_char_boundary(i) && !inside_crlf(self.text, i)).collect();
        let count = rng.between(1, 3);
        let inserted: String = (0..count).map(|_| UNICODE_SET[rng.below(UNICODE_SET.len())]).collect();
        if inserted.len() > self.cap() || offsets.is_empty() {
            return None;
        }
        Some((*rng.pick(&offsets), 0, inserted))
    }

    /// Makes one line end, or every one, `\r\n` or a lone `\r`.
    fn line_ends(&self, rng: &mut SplitMix64) -> Option<Splice> {
        let ends = line_ends(self.text);
        if ends.is_empty() {
            return None;
        }
        let targets = ["\r\n", "\r"];
        let first_target = rng.below(2);
        let every = rng.below(2) == 1;
        for target in [targets[first_target], targets[1 - first_target]] {
            let differing: Vec<(usize, usize)> =
                ends.iter().copied().filter(|&(start, end)| &self.text[start..end] != target).collect();
            // A CR LF grows the document by one byte for each line end it replaces.
            if differing.is_empty() || (target.len() > 1 && self.cap() == 0) {
                continue;
            }
            let growth = differing.len() * (target.len() - 1);
            let chosen: Vec<(usize, usize)> =
                if every && growth <= self.cap() { differing } else { vec![*rng.pick(&differing)] };
            let (start, end) = (chosen[0].0, chosen[chosen.len() - 1].1);
            let mut replaced = String::new();
            let mut at = start;
            for (line_end_start, line_end_end) in chosen {
                replaced.push_str(&self.text[at..line_end_start]);
                replaced.push_str(target);
                at = line_end_end;
            }
            return Some((start, end - start, replaced));
        }
        None
    }

    fn rename(&self, rng: &mut SplitMix64) -> Option<Splice> {
        let mut names: Vec<&str> =
            self.tokens.iter().filter(|t| t.kind == TokenKind::Identifier).map(|t| t.text(self.text)).collect();
        names.sort_unstable();
        names.dedup();
        if names.len() < 2 {
            return None;
        }
        let from = *rng.pick(&names);
        let occurrences: Vec<&Token> =
            self.tokens.iter().filter(|t| t.kind == TokenKind::Identifier && t.text(self.text) == from).collect();
        let fitting: Vec<&str> = names
            .iter()
            .copied()
            .filter(|to| *to != from && occurrences.len() * to.len().saturating_sub(from.len()) <= self.cap())
            .collect();
        if fitting.is_empty() {
            return None;
        }
        let to = *rng.pick(&fitting);
        let (start, end) = (occurrences[0].start, occurrences[occurrences.len() - 1].end);
        let mut replaced = String::new();
        let mut at = start;
        for t in occurrences {
            replaced.push_str(&self.text[at..t.start]);
            replaced.push_str(to);
            at = t.end;
        }
        Some((start, end - start, replaced))
    }

    fn fragment(&self, rng: &mut SplitMix64, fragments: &[String]) -> Option<Splice> {
        let fitting: Vec<&String> = fragments.iter().filter(|f| !f.is_empty() && f.len() <= self.cap()).collect();
        if fitting.is_empty() {
            return None;
        }
        let f = (*rng.pick(&fitting)).clone();
        self.insert_at_boundary(rng, f)
    }

    fn stress_budget(&self) -> usize {
        STRESS_CAP.min(self.room)
    }

    fn nest(&self, rng: &mut SplitMix64) -> Option<Splice> {
        let &(open, close) = self.lexicon.brackets.first()?;
        let pair = open.len_utf8() + close.len_utf8();
        let deepest = self.stress_budget() / pair;
        if deepest == 0 {
            return None;
        }
        let depth = rng.between(1, deepest);
        let mut nested = String::with_capacity(depth * pair);
        (0..depth).for_each(|_| nested.push(open));
        (0..depth).for_each(|_| nested.push(close));
        let at = *rng.pick(&self.boundaries());
        Some((at, 0, nested))
    }

    fn repeat_line(&self, rng: &mut SplitMix64) -> Option<Splice> {
        let budget = self.stress_budget();
        let copies = self.line_copies(budget);
        if copies.is_empty() {
            return None;
        }
        let (at, copy) = rng.pick(&copies).clone();
        let times = rng.between(1, budget / copy.len());
        Some((at, 0, copy.repeat(times)))
    }
}

/// How a server takes changes, from its `textDocumentSync`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sync {
    Full,
    Incremental,
}

/// lspcheck's own arithmetic went wrong: a change didn't turn the text into
/// the planned one. It stops the run as an internal error, never a verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InternalError(pub String);

impl std::fmt::Display for InternalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "internal error in the mutation planner: {}", self.0)
    }
}

impl std::error::Error for InternalError {}

/// The position of byte `at` of `text`, counted in `encoding`; `None` between
/// a `\r` and a `\n`, which no position can name.
pub fn position_of(text: &str, at: usize, encoding: Encoding) -> Option<Position> {
    if inside_crlf(text, at) || !text.is_char_boundary(at) {
        return None;
    }
    let before = &text[..at];
    let line_start = before.rfind(['\r', '\n']).map_or(0, |i| i + 1);
    let line = text::split_lines(before).len() - 1;
    Some(Position { line, character: text::line_length(&text[line_start..at], encoding) })
}

/// The smallest change that turns `before` into `after`: their common prefix
/// and suffix in characters left out, widened so neither end falls between
/// a `\r` and a `\n` in either text, with its range counted in `encoding`
/// against `before`. Checked with [`text::apply_change`].
pub fn incremental_change(before: &str, after: &str, encoding: Encoding) -> Result<ContentChange, InternalError> {
    let (b, a) = (before.as_bytes(), after.as_bytes());
    let mut prefix = b.iter().zip(a).take_while(|(x, y)| x == y).count();
    while !before.is_char_boundary(prefix) {
        prefix -= 1;
    }
    let most = b.len().min(a.len()) - prefix;
    let mut suffix = b.iter().rev().zip(a.iter().rev()).take(most).take_while(|(x, y)| x == y).count();
    while !before.is_char_boundary(b.len() - suffix) {
        suffix -= 1;
    }
    loop {
        let mut widened = false;
        if inside_crlf(before, prefix) || inside_crlf(after, prefix) {
            prefix -= 1;
            widened = true;
        }
        if inside_crlf(before, b.len() - suffix) || inside_crlf(after, a.len() - suffix) {
            suffix -= 1;
            widened = true;
        }
        if !widened {
            break;
        }
    }
    let start = position_of(before, prefix, encoding);
    let end = position_of(before, b.len() - suffix, encoding);
    let (Some(start), Some(end)) = (start, end) else {
        return Err(InternalError(format!("a change's end falls inside a character or a CR LF ({prefix}, {suffix})")));
    };
    let change = ContentChange::Text { range: Range { start, end }, text: after[prefix..a.len() - suffix].to_owned() };
    check(before, after, encoding, change)
}

fn check(before: &str, after: &str, encoding: Encoding, change: ContentChange) -> Result<ContentChange, InternalError> {
    match text::apply_change(before, encoding, &change) {
        Ok(result) if result == after => Ok(change),
        Ok(_) => Err(InternalError(format!("the change {change:?} doesn't give the planned text"))),
        Err(e) => Err(InternalError(format!("the change {change:?} doesn't apply: {e:?}"))),
    }
}

/// The content changes of one step, and the document's text after it: one
/// change per mutation for incremental sync, each against the text the one
/// before left, or the whole final text for full sync.
pub fn step_changes(
    text: &str,
    step: &Step,
    encoding: Encoding,
    sync: Sync,
) -> Result<(Vec<ContentChange>, String), InternalError> {
    let mut current = text.to_owned();
    let mut changes = Vec::with_capacity(step.mutations.len());
    for m in &step.mutations {
        let next = m.apply(&current);
        if sync == Sync::Incremental {
            changes.push(incremental_change(&current, &next, encoding)?);
        }
        current = next;
    }
    if sync == Sync::Full {
        changes.push(ContentChange::Full(current.clone()));
    }
    Ok((changes, current))
}

/// The change that restores the clean text: the whole text for full sync,
/// and for incremental sync one change whose range covers the whole current
/// document.
pub fn restore_change(
    current: &str,
    clean: &str,
    encoding: Encoding,
    sync: Sync,
) -> Result<ContentChange, InternalError> {
    if sync == Sync::Full {
        return Ok(ContentChange::Full(clean.to_owned()));
    }
    let end = position_of(current, current.len(), encoding).expect("the end of a text is a position");
    let change =
        ContentChange::Text { range: Range { start: Position { line: 0, character: 0 }, end }, text: clean.to_owned() };
    check(current, clean, encoding, change)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENCODINGS: [Encoding; 3] = [Encoding::Utf8, Encoding::Utf16, Encoding::Utf32];

    /// A seed with brackets, strings, comments, keywords, numbers, non-ASCII
    /// text and every kind of line end.
    const LUA: &str = "-- a comment \u{1F600}\r\nlocal function greet(name)\r\n  local s = \"h\u{E9}llo, \\\"\" .. name\n  --[[ a\nblock ]] return s, 42, {1, 2}\rend\n\nprint(greet('w\u{4E2D}rld'))";

    fn lua() -> Lexicon {
        Lexicon {
            line_comments: vec!["--".into()],
            block_comments: vec![("--[[".into(), "]]".into())],
            strings: vec![
                StringDelimiters { open: "\"".into(), close: "\"".into(), escape: Some('\\') },
                StringDelimiters { open: "'".into(), close: "'".into(), escape: Some('\\') },
                StringDelimiters { open: "[[".into(), close: "]]".into(), escape: None },
            ],
            identifier_chars: "_".into(),
            brackets: vec![('(', ')'), ('[', ']'), ('{', '}')],
            keywords: ["local", "function", "end", "return"].map(String::from).to_vec(),
        }
    }

    fn all_operators(lexicon: Lexicon) -> Config {
        Config {
            operators: Operator::ALL.to_vec(),
            fragments: vec!["0x".into(), "1e".into(), "...".into()],
            lexicon,
            ..Config::default()
        }
    }

    /// The seeds the property tests run over: the Lua text, plain text with
    /// every line end, one line, an emoji run, a lone CR and LF mix, a single
    /// character and an empty document.
    fn seeds() -> Vec<String> {
        vec![
            LUA.to_owned(),
            "x\r\u{1F600}\u{1F600}\r\n word\n\u{1F600}\n name\n".to_owned(),
            "fn main() { let x = [1, 2]; }".to_owned(),
            "\u{1F600}\u{1F600}\u{1F600}".to_owned(),
            "\r\n\r\r\n\n\r".to_owned(),
            "a".to_owned(),
            String::new(),
        ]
    }

    #[test]
    fn splitmix64_gives_the_published_stream() {
        // The reference implementation's first outputs for seed 0.
        let mut rng = SplitMix64::new(0);
        assert_eq!(rng.next_u64(), 0xE220_A839_7B1D_CDAF);
        assert_eq!(rng.next_u64(), 0x6E78_9E6A_A1B9_65F4);
        assert_eq!(rng.next_u64(), 0x06C4_5D18_8009_454F);
    }

    #[test]
    fn streams_differ_by_case_and_label_and_repeat_for_the_same_ones() {
        let first = |seed, case, label| SplitMix64::stream(seed, case, label).next_u64();
        assert_eq!(first(7, 3, "edits"), first(7, 3, "edits"));
        assert_ne!(first(7, 3, "edits"), first(7, 3, "requests"));
        assert_ne!(first(7, 3, "edits"), first(7, 4, "edits"));
        assert_ne!(first(7, 3, "edits"), first(8, 3, "edits"));
    }

    #[test]
    fn below_stays_in_range_and_reaches_every_value() {
        let mut rng = SplitMix64::new(1);
        let mut seen = [false; 7];
        for _ in 0..1000 {
            let x = rng.below(7);
            seen[x] = true;
        }
        assert!(seen.iter().all(|&s| s));
        assert_eq!(rng.below(1), 0);
        assert!((0..1000).all(|_| (3..=5).contains(&rng.between(3, 5))));
    }

    fn kinds(lexicon: &Lexicon, text: &str) -> Vec<(TokenKind, String)> {
        lexicon.tokens(text).iter().map(|t| (t.kind, t.text(text).to_owned())).collect()
    }

    #[test]
    fn the_default_lexicon_finds_words_numbers_spaces_line_ends_and_punctuation() {
        use TokenKind::*;
        let got = kinds(&Lexicon::default(), "caf\u{E9}_1 42x\t \r\n\u{1F600}(\r\u{4E2D}\n");
        let want = [
            (Identifier, "caf\u{E9}_1"),
            (Whitespace, " "),
            (Number, "42x"),
            (Whitespace, "\t "),
            (LineEnd, "\r\n"),
            (Punctuation, "\u{1F600}"),
            (Punctuation, "("),
            (LineEnd, "\r"),
            (Identifier, "\u{4E2D}"),
            (LineEnd, "\n"),
        ];
        assert_eq!(got, want.map(|(k, s)| (k, s.to_owned())));
        // No comments or strings without a lexicon that names them.
        assert!(
            kinds(&Lexicon::default(), "\"a\" -- b").iter().all(|(k, _)| !matches!(k, String { .. } | Comment { .. }))
        );
    }

    #[test]
    fn a_lexicon_finds_comments_strings_and_keywords() {
        use TokenKind::*;
        let lex = lua();
        let got = kinds(&lex, "local s = \"a\\\"b\" -- c\n--[[ x\ny ]]'q");
        let want = [
            (Keyword, "local"),
            (Whitespace, " "),
            (Identifier, "s"),
            (Whitespace, " "),
            (Punctuation, "="),
            (Whitespace, " "),
            (String { open: 1, close: 1 }, "\"a\\\"b\""),
            (Whitespace, " "),
            (Comment { block: false, open: 2, close: 0 }, "-- c"),
            (LineEnd, "\n"),
            // The longest delimiter wins: `--[[` over `--`.
            (Comment { block: true, open: 4, close: 2 }, "--[[ x\ny ]]"),
            // A string never closed runs to the end of its line.
            (String { open: 1, close: 0 }, "'q"),
        ];
        assert_eq!(got, want.map(|(k, s)| (k, s.to_owned())));
        // A block comment never closed runs to the end of the document; a
        // string never closed stops at its line end.
        assert_eq!(kinds(&lex, "--[[ a\nb")[0], (Comment { block: true, open: 4, close: 0 }, "--[[ a\nb".to_owned()));
        assert_eq!(kinds(&lex, "'a\nb'")[0], (String { open: 1, close: 1 }, "'a\nb'".to_owned()));
        assert_eq!(kinds(&lex, "\"a\nb")[0], (String { open: 1, close: 0 }, "\"a".to_owned()));
        // Strings left open one after another each run to their line's end.
        let open = "[[\n".repeat(3);
        assert_eq!(
            kinds(&lex, &open).iter().filter(|(k, _)| *k == String { open: 2, close: 0 }).count(),
            3,
            "{:?}",
            kinds(&lex, &open)
        );
    }

    #[test]
    fn a_string_known_to_be_unclosed_is_not_searched_again() {
        let quote = StringDelimiters { open: "'".into(), close: "'".into(), escape: None };
        let mut unclosed_from = None;
        assert_eq!(string_end("a\nb", 0, &quote, &mut unclosed_from), (1, 0));
        assert_eq!(unclosed_from, Some(0));
        // Past that point the search isn't repeated: a search would find the
        // close at 3, but the string is taken as unclosed at once. Repeating
        // it for every open string made lexing quadratic.
        assert_eq!(string_end("a\nb'", 2, &quote, &mut unclosed_from), (4, 0));
        assert_eq!(string_end("a\nb'", 2, &quote, &mut None), (4, 1));
    }

    #[test]
    fn tokens_cover_the_text_without_gaps() {
        for lexicon in [Lexicon::default(), lua()] {
            for seed in seeds() {
                let tokens = lexicon.tokens(&seed);
                let mut at = 0;
                for t in &tokens {
                    assert_eq!(t.start, at);
                    assert!(t.end > t.start);
                    at = t.end;
                }
                assert_eq!(at, seed.len());
            }
        }
    }

    #[test]
    fn operators_have_names_and_defaults() {
        for o in Operator::ALL {
            assert_eq!(Operator::from_name(o.name()), Some(o));
        }
        assert_eq!(Operator::from_name("nope"), None);
        let defaults: Vec<&str> = Operator::defaults().iter().map(|o| o.name()).collect();
        assert_eq!(defaults.len(), 15);
        assert!(!defaults.iter().any(|n| ["nest", "repeat-line", "control"].contains(n)));
    }

    fn texts(seed: &str) -> Vec<String> {
        vec![seed.to_owned()]
    }

    #[test]
    fn the_same_seed_gives_the_same_plan() {
        let config = all_operators(lua());
        let docs = vec![LUA.to_owned(), "x = (1)\n".to_owned()];
        for case in 0..50 {
            assert_eq!(plan_case(9, case, &docs, &config), plan_case(9, case, &docs, &config));
        }
        let differ = (0..50).filter(|&c| plan_case(9, c, &docs, &config) != plan_case(10, c, &docs, &config)).count();
        assert!(differ > 40, "only {differ} of 50 cases differ between seeds");
    }

    /// The texts each step of a case leaves, in order.
    fn replay(texts: &[String], case: &Case) -> Vec<Vec<String>> {
        let mut current = texts.to_vec();
        let mut out = Vec::new();
        for step in &case.steps {
            let mut after = Vec::new();
            for m in &step.mutations {
                current[step.document] = m.apply(&current[step.document]);
                after.push(current[step.document].clone());
            }
            out.push(after);
        }
        out
    }

    /// A mutation as (operator, fallback, offset, removed, inserted).
    type Record<'a> = (&'a str, bool, usize, usize, &'a str);

    #[test]
    fn a_pinned_case_keeps_its_plan() {
        // Golden: if this changes, the format changed; raise FORMAT.
        let case = plan_case(20261109, 17, &texts(LUA), &Config { lexicon: lua(), ..Config::default() });
        let summary: Vec<(usize, Vec<Record>)> = case
            .steps
            .iter()
            .map(|s| {
                (
                    s.document,
                    s.mutations
                        .iter()
                        .map(|m| (m.operator.name(), m.fallback, m.offset, m.removed, m.inserted.as_str()))
                        .collect(),
                )
            })
            .collect();
        // One step on the only document: three characters inside the block
        // comment, the `2` of `{1, 2}` deleted, then the comment's `]]`.
        let golden = vec![(
            0,
            vec![
                ("insert-in-literal", false, 89, 0, "\u{1D518}\u{200D}\u{200B}"),
                ("delete-tokens", false, 126, 1, ""),
                ("open-comment", false, 105, 2, ""),
            ],
        )];
        assert_eq!(summary, golden);
        let after = replay(&texts(LUA), &case).concat().pop().unwrap();
        assert_eq!(
            after,
            "-- a comment \u{1F600}\r\nlocal function greet(name)\r\n  local s = \"h\u{E9}llo, \\\"\" .. name\n  --[[ a\n\u{1D518}\u{200D}\u{200B}block  return s, 42, {1, }\rend\n\nprint(greet('w\u{4E2D}rld'))"
        );
    }

    /// Every planned edit, over many cases and seeds, starts and ends at a
    /// character boundary and never between a `\r` and a `\n`, changes the
    /// text, stays within its size caps, and reaches the planned text by
    /// incremental changes in each encoding and by full sync.
    fn check_plans(config: &Config, cases: u64) {
        for seed_text in seeds() {
            let docs = texts(&seed_text);
            for index in 0..cases {
                let case = plan_case(31, index, &docs, config);
                assert!((1..=config.steps).contains(&case.steps.len()));
                let mut current = docs.clone();
                for step in &case.steps {
                    assert!((1..=MAX_MUTATIONS_PER_STEP).contains(&step.mutations.len()));
                    let before = current[step.document].clone();
                    let mut text = before.clone();
                    for m in &step.mutations {
                        let (start, end) = (m.offset, m.offset + m.removed);
                        assert!(text.is_char_boundary(start) && text.is_char_boundary(end), "{m:?}");
                        assert!(!inside_crlf(&text, start) && !inside_crlf(&text, end), "{m:?} in {text:?}");
                        let next = m.apply(&text);
                        assert_ne!(next, text, "{m:?} changed nothing");
                        let cap = if m.operator.is_stress() { STRESS_CAP } else { INSERT_CAP };
                        assert!(m.inserted.len().saturating_sub(m.removed) <= cap, "{m:?}");
                        assert!(next.len() <= config.max_document_bytes.max(text.len()), "{m:?}");
                        text = next;
                    }
                    for encoding in ENCODINGS {
                        let (changes, after) = step_changes(&before, step, encoding, Sync::Incremental).unwrap();
                        assert_eq!(after, text);
                        assert_eq!(changes.len(), step.mutations.len());
                        let mut applied = before.clone();
                        for change in &changes {
                            applied = text::apply_change(&applied, encoding, change).unwrap();
                        }
                        assert_eq!(applied, text);
                        let restore = restore_change(&text, &seed_text, encoding, Sync::Incremental).unwrap();
                        assert_eq!(text::apply_change(&text, encoding, &restore).unwrap(), seed_text);
                    }
                    let (full, after) = step_changes(&before, step, Encoding::Utf16, Sync::Full).unwrap();
                    assert_eq!(full, vec![ContentChange::Full(text.clone())]);
                    assert_eq!(after, text);
                    current[step.document] = text;
                }
            }
        }
    }

    #[test]
    fn plans_with_the_default_lexicon_keep_every_rule() {
        check_plans(&all_operators(Lexicon::default()), 120);
    }

    #[test]
    fn plans_with_a_lexicon_keep_every_rule() {
        check_plans(&all_operators(lua()), 120);
    }

    #[test]
    fn each_operator_alone_keeps_every_rule() {
        for operator in Operator::ALL {
            let config = Config { operators: vec![operator], ..all_operators(lua()) };
            check_plans(&config, 15);
        }
    }

    #[test]
    fn a_small_document_limit_holds() {
        // Brackets of four bytes each: the last resort's bracket only just fits.
        let wide = Lexicon { brackets: vec![('\u{1F600}', '\u{1F601}')], ..lua() };
        for lexicon in [lua(), wide] {
            for seed in seeds() {
                for slack in [0, 1, 8] {
                    let max_document_bytes = seed.len().max(MIN_DOCUMENT_BYTES) + slack;
                    let config = Config { max_document_bytes, ..all_operators(lexicon.clone()) };
                    for index in 0..60 {
                        let case = plan_case(5, index, &texts(&seed), &config);
                        for after in replay(&texts(&seed), &case).iter().flatten() {
                            assert!(after.len() <= max_document_bytes, "{seed:?} case {index}: {after:?}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    #[should_panic(expected = "max_document_bytes below 4")]
    fn a_document_limit_too_small_for_a_character_is_refused() {
        // An emptied document gets a bracket, which may take four bytes.
        let config = Config {
            operators: vec![Operator::DeleteTokens],
            max_document_bytes: 1,
            lexicon: Lexicon { brackets: vec![('\u{4E2D}', '\u{6587}')], ..Lexicon::default() },
            ..Config::default()
        };
        plan_case(0, 0, &texts(""), &config);
    }

    #[test]
    fn line_operators_see_line_ends_inside_comments_and_strings() {
        // A block comment over two lines is one token, but still two lines.
        let lexicon = Lexicon { block_comments: vec![("/*".into(), "*/".into())], ..lua() };
        let seed = "/*a\nb*/";
        for operator in [Operator::DeleteLine, Operator::DuplicateLine, Operator::RepeatLine, Operator::LineEnds] {
            let config = Config { steps: 1, operators: vec![operator], lexicon: lexicon.clone(), ..Config::default() };
            for index in 0..20 {
                let m = &plan_case(0, index, &texts(seed), &config).steps[0].mutations[0];
                assert!(!m.fallback, "{m:?}");
                let edit = (m.offset, m.removed, m.inserted.as_str());
                match operator {
                    Operator::DeleteLine => assert!([(0, 4, ""), (4, 3, "")].contains(&edit), "{m:?}"),
                    Operator::DuplicateLine => assert!([(4, 0, "/*a\n"), (4, 0, "b*/\n")].contains(&edit), "{m:?}"),
                    Operator::RepeatLine => {
                        assert_eq!((m.offset, m.removed), (4, 0));
                        let line = &m.inserted[..4];
                        assert!(["/*a\n", "b*/\n"].contains(&line), "{m:?}");
                        assert_eq!(m.inserted, line.repeat(m.inserted.len() / 4), "{m:?}");
                    }
                    _ => assert!([(3, 1, "\r\n"), (3, 1, "\r")].contains(&edit), "{m:?}"),
                }
            }
        }
        // A last line without a line end is repeated too, with one added.
        let config = Config { steps: 1, operators: vec![Operator::RepeatLine], ..Config::default() };
        let m = &plan_case(0, 0, &texts("x = 1"), &config).steps[0].mutations[0];
        assert!(!m.fallback && m.offset == 0 && m.inserted == "x = 1\n".repeat(m.inserted.len() / 6), "{m:?}");
        // A string over two lines, likewise.
        let config = Config { steps: 1, operators: vec![Operator::LineEnds], lexicon: lua(), ..Config::default() };
        let m = &plan_case(0, 0, &texts("x = 'a\r\nb'"), &config).steps[0].mutations[0];
        assert_eq!((m.fallback, m.offset, m.removed, m.inserted.as_str()), (false, 6, 2, "\r"));
    }

    #[test]
    fn an_empty_seed_gets_a_bracket() {
        for brackets in [vec![('{', '}')], vec![]] {
            let want = if brackets.is_empty() { "(" } else { "{" };
            let config = Config {
                operators: vec![Operator::DeleteTokens],
                lexicon: Lexicon { brackets, ..Lexicon::default() },
                ..Config::default()
            };
            let case = plan_case(1, 0, &texts(""), &config);
            let first = &case.steps[0].mutations[0];
            assert_eq!((first.fallback, first.offset, first.removed, first.inserted.as_str()), (true, 0, 0, want));
        }
    }

    #[test]
    fn a_truncation_to_empty_is_followed_by_a_bracket() {
        // `a` has one token, so truncate cuts at 0; the next truncate finds
        // nothing and inserts `(`, and the one after cuts it again.
        let config = Config { operators: vec![Operator::Truncate], ..Config::default() };
        for index in 0..20 {
            let case = plan_case(3, index, &texts("a"), &config);
            let mut text = "a".to_owned();
            for m in case.steps.iter().flat_map(|s| &s.mutations) {
                let empty = text.is_empty();
                text = m.apply(&text);
                assert_eq!(m.fallback, empty);
                assert_eq!(text, if empty { "(" } else { "" });
            }
        }
    }

    #[test]
    fn operators_without_what_they_need_fall_back_and_say_so() {
        // The default lexicon has no strings, comments or fragments.
        for operator in [Operator::OpenString, Operator::OpenComment, Operator::Fragment] {
            let config = Config { operators: vec![operator], ..Config::default() };
            let case = plan_case(2, 0, &texts("f(x)"), &config);
            assert!(case.steps.iter().flat_map(|s| &s.mutations).all(|m| m.fallback && m.removed == 0));
        }
        // With a lexicon, open-string removes a closing quote or inserts an
        // opening one.
        let config = Config { operators: vec![Operator::OpenString], lexicon: lua(), ..Config::default() };
        let mut removed = 0;
        let mut inserted = 0;
        for index in 0..100 {
            for m in plan_case(2, index, &texts("x = 'a'"), &config).steps.iter().flat_map(|s| &s.mutations) {
                assert!(!m.fallback);
                if m.removed > 0 {
                    removed += 1;
                } else {
                    assert!(["\"", "'", "[["].contains(&m.inserted.as_str()));
                    inserted += 1;
                }
            }
        }
        assert!(removed > 0 && inserted > 0);
    }

    #[test]
    fn a_swap_that_changes_nothing_falls_back() {
        // `'''` is the string `''` then the unclosed string `'`; swapping them
        // gives `'''` again.
        let quote = StringDelimiters { open: "'".into(), close: "'".into(), escape: None };
        let config = Config {
            operators: vec![Operator::SwapTokens],
            lexicon: Lexicon { strings: vec![quote], ..Lexicon::default() },
            ..Config::default()
        };
        let case = plan_case(0, 0, &texts("'''"), &config);
        let first = &case.steps[0].mutations[0];
        assert!(first.fallback, "{first:?}");
        assert_ne!(first.apply("'''"), "'''");
    }

    #[test]
    fn only_listed_characters_are_inserted_by_unicode_and_control() {
        for (operator, allowed) in [
            // The design's fixed set, spelled out rather than taken from UNICODE_SET.
            (
                Operator::Unicode,
                vec!['é', '中', '😀', '𝔘', '\u{301}', '\u{200D}', '\u{A0}', '\u{200B}', '\u{FEFF}', '\u{10FFFF}'],
            ),
            (Operator::Control, ('\0'..='\u{1F}').filter(|c| !matches!(c, '\t' | '\n' | '\r')).collect()),
        ] {
            let config = Config { operators: vec![operator], ..Config::default() };
            for index in 0..50 {
                for m in plan_case(4, index, &texts("a b"), &config).steps.iter().flat_map(|s| &s.mutations) {
                    assert_eq!(m.inserted.chars().count(), 1);
                    assert!(allowed.contains(&m.inserted.chars().next().unwrap()), "{m:?}");
                }
            }
        }
    }

    /// The change for one edit, and that it applies in every encoding.
    fn change(before: &str, after: &str, encoding: Encoding) -> (Range, String) {
        match incremental_change(before, after, encoding).unwrap() {
            ContentChange::Text { range, text } => (range, text),
            ContentChange::Full(_) => unreachable!(),
        }
    }

    fn pos(line: usize, character: usize) -> Position {
        Position { line, character }
    }

    #[test]
    fn line_end_changes_never_split_a_crlf() {
        // CR LF to CR: the common prefix `x\r` ends inside the CR LF, so the
        // change widens to replace `\r\n` with `\r`.
        assert_eq!(
            change("x\r\ny", "x\ry", Encoding::Utf16),
            (Range { start: pos(0, 1), end: pos(1, 0) }, "\r".into())
        );
        // CR to CR LF: the common prefix `x\r` ends inside the new CR LF.
        assert_eq!(
            change("x\ry", "x\r\ny", Encoding::Utf16),
            (Range { start: pos(0, 1), end: pos(1, 0) }, "\r\n".into())
        );
        // LF to CR LF: the common suffix `\ny` starts inside the new CR LF.
        assert_eq!(
            change("x\ny", "x\r\ny", Encoding::Utf16),
            (Range { start: pos(0, 1), end: pos(1, 0) }, "\r\n".into())
        );
        // CR LF to LF: the common suffix starts inside the old CR LF.
        assert_eq!(
            change("x\r\ny", "x\ny", Encoding::Utf16),
            (Range { start: pos(0, 1), end: pos(1, 0) }, "\n".into())
        );
        // Deleting a character that joins a CR and an LF into one line end.
        assert_eq!(
            change("a\rX\nb", "a\r\nb", Encoding::Utf16),
            (Range { start: pos(0, 1), end: pos(2, 0) }, "\r\n".into())
        );
    }

    #[test]
    fn changes_are_minimal_and_counted_in_the_encoding() {
        // `😀` is 4 UTF-8 bytes, 2 UTF-16 units, 1 code point.
        let before = "\u{1F600}ab\n\u{1F600}cd";
        let after = "\u{1F600}aXb\n\u{1F600}cd";
        for (encoding, character) in [(Encoding::Utf8, 5), (Encoding::Utf16, 3), (Encoding::Utf32, 2)] {
            assert_eq!(
                change(before, after, encoding),
                (Range { start: pos(0, character), end: pos(0, character) }, "X".into())
            );
        }
        // A shared byte inside different characters isn't a common prefix:
        // é (C3 A9) and è (C3 A8) share their first byte.
        assert_eq!(
            change("\u{E9}", "\u{E8}", Encoding::Utf8),
            (Range { start: pos(0, 0), end: pos(0, 2) }, "\u{E8}".into())
        );
        // Prefix and suffix don't overlap when a run is repeated.
        assert_eq!(change("aaa", "aaaa", Encoding::Utf8), (Range { start: pos(0, 3), end: pos(0, 3) }, "a".into()));
    }

    #[test]
    fn restoration_covers_the_whole_document() {
        for encoding in ENCODINGS {
            let change = restore_change("a\r\n\u{1F600}", "clean", encoding, Sync::Incremental).unwrap();
            let end = pos(
                1,
                match encoding {
                    Encoding::Utf8 => 4,
                    Encoding::Utf16 => 2,
                    Encoding::Utf32 => 1,
                },
            );
            assert_eq!(change, ContentChange::Text { range: Range { start: pos(0, 0), end }, text: "clean".into() });
        }
        assert_eq!(restore_change("x", "y", Encoding::Utf8, Sync::Full).unwrap(), ContentChange::Full("y".into()));
    }

    #[test]
    fn positions_are_never_inside_a_crlf_or_a_character() {
        assert_eq!(position_of("a\r\nb", 2, Encoding::Utf16), None);
        assert_eq!(position_of("\u{E9}", 1, Encoding::Utf8), None);
        assert_eq!(position_of("a\r\nb", 3, Encoding::Utf16), Some(pos(1, 0)));
        assert_eq!(position_of("a\rb\nc", 4, Encoding::Utf16), Some(pos(2, 0)));
        assert_eq!(position_of("a\r", 2, Encoding::Utf16), Some(pos(1, 0)));
    }
}
