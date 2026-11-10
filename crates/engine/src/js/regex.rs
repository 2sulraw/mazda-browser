//! From-scratch regular-expression engine (no crates; zero dependencies
//! beyond `std`). Written for the browser's JS interpreter: small, honestly
//! bounded, and good enough for real site patterns.
//!
//! # Design (one paragraph)
//! Patterns are parsed into a small AST, then compiled to a flat instruction
//! program (Char/Any/Class/Split/Jmp/Save/Backref/Assert/Look/SubEnd/Match —
//! a textbook backtracking VM layout). Matching itself is a NON-RECURSIVE
//! loop with an explicit backtrack stack, because loops must not consume
//! native stack for long inputs and backrefs/lookaround need the exact
//! backpointers a Thompson NFA cannot represent (lookaround only; it runs as
//! a bounded-depth recursive sub-VM sharing the same step budget). Stack
//! entries are copy-on-write `Rc` capture snapshots: `Save` mutates through
//! `Rc::make_mut`, so restoring `k` stack entries costs O(1) unless the
//! branches actually diverged.
//!
//! # Resource bounds (RAM/CPU honesty) — hard caps, not best effort
//! - `MAX_STEPS` (100k instruction dispatches per `find` call): a
//!   catastrophic-backtracking pattern (`(a+)+$` vs a long failing string)
//!   terminates at the cap and reports NO MATCH (with the honest limitation
//!   that an over-limit result is "no match", not a pending-forever hang).
//! - `MAX_BACKTRACK` (8k stack entries): bounds the backtrack stack; each
//!   entry is 24 bytes + shared caps snapshot, so worst-case backtrack
//!   memory is ~200 KB per running match.
//! - Capture groups capped at 99 (`\1`..`\99`); lookahead nesting capped at
//!   6; `{n,m}` unrolling capped so a compiled program stays under
//!   `MAX_PROG` (50k instructions) — bigger is an honest compile error.
//! - Compiled programs are deduped per (pattern, flags) on the engine
//!   (see `JsEngine::regex_cache`), so a literal pays one compile.

use std::rc::Rc;

/// CPU bound: instruction dispatches per `find` call. Exceed → no match.
pub(crate) const MAX_STEPS: usize = 100_000;
/// RAM bound: backtrack stack entries (~24 B each + shared caps).
pub(crate) const MAX_BACKTRACK: usize = 8_000;
/// lookaround recursion depth bound
pub(crate) const MAX_LOOK_DEPTH: usize = 6;
/// compiled program size cap
pub(crate) const MAX_PROG: usize = 50_000;
/// capture groups cap (\1..\99)
const MAX_GROUPS: usize = 99;

#[derive(Debug)]
pub(crate) struct RegexError {
    pub msg: String,
}

fn rerr<T>(msg: impl Into<String>) -> Result<T, RegexError> {
    Err(RegexError { msg: msg.into() })
}

// ---------------------------------------------------------------------------
// compiled program
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub(crate) enum Inst {
    /// match one exact char (case folded at match time when ci)
    Char(char),
    /// `.` — matches any char except `\n` unless dot_all
    Any(bool),
    /// char class: sorted inclusive ranges, negated
    Class(Box<ClassSet>),
    /// try pc A first, then pc B (order encodes greedy/lazy)
    Split(usize, usize),
    Jmp(usize),
    /// caps[i] = current position (i = 2g for open, 2g+1 for close)
    Save(usize),
    Backref(usize),
    /// zero-width: run sub program [start..end); if `negate`, invert result.
    /// Captures made inside a positive lookaround persist (JS semantics).
    Look(bool, usize, usize),
    /// zero-width lookbehind: same shape, but the sub run must END at the
    /// current position, so the VM tries the candidate starts left of it.
    LookBack(bool, usize, usize),
    /// end of a lookaround sub program (never reached by the main path)
    SubEnd,
    Match,
    /// `^` — multiline
    AssertStart(bool),
    /// `$` — multiline
    AssertEnd(bool),
    /// `\b` (true) / `\B` (false)
    WordB(bool),
}

#[derive(Debug, Clone)]
pub(crate) struct ClassSet {
    pub ranges: Vec<(char, char)>,
    /// `\p{...}` members, each with its own `\P` negation. OR'd with the
    /// ranges; the class-level `neg` inverts the whole set.
    pub props: Vec<(PropKind, bool)>,
    pub neg: bool,
}

/// Unicode property escapes the engine can answer exactly from `std`'s char
/// predicates (see `prop_matches`). Kept deliberately small: an unknown
/// property is an honest compile error, not a silent never-match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PropKind {
    /// `\p{L}` / `\p{Letter}` — alphabetic
    Letter,
    /// `\p{Lu}` — uppercase
    Upper,
    /// `\p{Ll}` — lowercase
    Lower,
    /// `\p{N}` / `\p{Number}` — numeric
    Number,
    /// `\p{Nd}` / `\p{Decimal_Number}` — decimal digits (approximated by
    /// `is_numeric`, which also accepts `½`-style numbers)
    Digit,
    /// `\p{Z}` / `\p{Zs}` — whitespace
    Space,
    /// `\p{ASCII}`
    Ascii,
    /// `\p{RI}` - regional indicator symbols (flag halves)
    RegionalIndicator,
    /// `\p{Emoji}` - honest shortcut: the symbol and pictograph blocks
    Emoji,
    /// `\p{EMod}` - the emoji modifier (skin-tone) code points
    EmojiModifier,
    /// `\p{Bidi_Control}` - the bidirectional-formatting controls
    BidiControl,
    /// `\p{Pc}` / `\p{Connector_Punctuation}` - the ten connector
    /// punctuation code points (underscore and its presentation forms).
    ConnectorPunctuation,
    /// `\p{M}` / `\p{Mn}` / `\p{Mc}` / `\p{Me}` — combining marks.
    /// shortcut: the engine has no Unicode tables, so the three mark
    /// subcategories are not distinguished; all of them answer from the same
    /// combining-mark block list (`MARK_RANGES`).
    Mark,
    /// `\p{Script=…}` — the scripts GitHub's confusable check asks for.
    /// shortcut: script membership is the script's Unicode BLOCK (see
    /// `script_matches`), not the per-character Script property.
    Greek,
    Hebrew,
    Arabic,
    Syriac,
    Thaana,
    Mandaic,
    Hiragana,
    Katakana,
    Han,
}
/// capture snapshot: caps\[i\] = Some((start_char, end_char));
/// caps\[0\]/caps\[1\] are the whole match. Copy-on-write via Rc.
type Caps = Rc<Vec<Option<(usize, usize)>>>;

pub(crate) struct Regex {
    pub src: String,
    pub flags: String,
    pub global: bool,
    pub sticky: bool,
    pub ignore_case: bool,
    pub multiline: bool,
    pub dot_all: bool,
    prog: Vec<Inst>,
    /// leading literal char (fast-scan optimization for non-anchored find
    /// loops); only set when the program's first real instruction is a Char
    first_char: Option<char>,
    /// number of capture groups (excluding the whole match slot)
    pub n_groups: usize,
    /// named capture groups: `(name, group number)`, for `match.groups`
    pub names: Vec<(String, usize)>,
}

impl Regex {
    pub fn source(&self) -> String {
        self.src.clone()
    }
}

pub(crate) struct MatchVal {
    pub start: usize,
    pub end: usize,
    pub caps: Vec<Option<(usize, usize)>>,
}

// ---------------------------------------------------------------------------
// parse
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum Node {
    Empty,
    A(A),
    Concat(Vec<Node>),
    Alt(Vec<Node>),
    /// capture group (number 1..=99)
    Group(usize, Box<Node>),
    Repeat {
        min: u32,
        max: Option<u32>,
        greedy: bool,
        body: Box<Node>,
    },
    Look(bool, Box<Node>),
    /// `(?<=…)` (false) / `(?<!…)` (true) — matches leftwards
    LookBehind(bool, Box<Node>),
}

#[derive(Debug)]
enum A {
    Char(char),
    Any,
    Class(ClassSet),
    Backref(usize),
    StartMl(bool),
    EndMl(bool),
    WordB(bool),
}

struct PatParser {
    c: Vec<char>,
    i: usize,
    group: usize,
    /// `(?<name>...)` name -> group number, in pattern order
    names: Vec<(String, usize)>,
    multiline: bool,
    dot_all: bool,
}

impl PatParser {
    fn peek(&self) -> Option<char> {
        self.c.get(self.i).copied()
    }
    fn next(&mut self) -> Option<char> {
        let ch = self.peek();
        if ch.is_some() {
            self.i += 1;
        }
        ch
    }
    fn eat(&mut self, ch: char) -> bool {
        if self.peek() == Some(ch) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn parse(&mut self) -> Result<Node, RegexError> {
        let n = self.alt()?;
        if self.i < self.c.len() {
            return rerr("unexpected closing brace in pattern");
        }
        Ok(n)
    }

    fn alt(&mut self) -> Result<Node, RegexError> {
        let mut branches = vec![self.concat()?];
        while self.eat('|') {
            branches.push(self.concat()?);
        }
        if branches.len() == 1 {
            Ok(branches.pop().unwrap())
        } else {
            Ok(Node::Alt(branches))
        }
    }

    fn concat(&mut self) -> Result<Node, RegexError> {
        let mut items: Vec<Node> = Vec::new();
        loop {
            match self.peek() {
                None | Some('|') | Some(')') => break,
                _ => {
                    let atom = self.atom()?;
                    let atom = self.quantify(atom)?;
                    items.push(atom);
                }
            }
        }
        match items.len() {
            0 => Ok(Node::Empty),
            1 => Ok(items.pop().unwrap()),
            _ => Ok(Node::Concat(items)),
        }
    }

    /// attach a quantifier to a single atom
    fn quantify(&mut self, atom: Node) -> Result<Node, RegexError> {
        let (min, max) = match self.peek() {
            Some('*') => {
                self.i += 1;
                (0, None)
            }
            Some('+') => {
                self.i += 1;
                (1, None)
            }
            Some('?') => {
                self.i += 1;
                (0, Some(1))
            }
            Some('{') => {
                // JS treats `{` that is not a valid `{n,m}` as a literal
                match self.braces()? {
                    Some(mm) => mm,
                    None => {
                        return Ok(Node::Concat(vec![atom, Node::A(A::Char('{'))]));
                    }
                }
            }
            _ => return Ok(atom),
        };
        // a quantifier with nothing to repeat (e.g. `*abc` or `a**`) is an
        // error in JS; we mirror that honestly
        if matches!(atom, Node::Empty) || matches!(atom, Node::Look(true, _)) {
            return rerr("nothing to repeat");
        }
        let greedy = !self.eat('?');
        Ok(Node::Repeat {
            min,
            max,
            greedy,
            body: Box::new(atom),
        })
    }

    /// parse `{n}` `{n,}` `{n,m}`; returns None when it is a literal `{`
    fn braces(&mut self) -> Result<Option<(u32, Option<u32>)>, RegexError> {
        let save = self.i;
        self.i += 1; // consume `{`
        let mut n1 = String::new();
        while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            n1.push(self.next().unwrap());
        }
        if n1.is_empty() {
            self.i = save;
            return Ok(None);
        }
        let min: u32 = n1.parse().map_err(|_| RegexError {
            msg: "quantifier count too large".into(),
        })?;
        if self.eat('}') {
            if min > 10_000 {
                return rerr("quantifier count too large");
            }
            return Ok(Some((min, Some(min))));
        }
        if self.eat(',') {
            let mut n2 = String::new();
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                n2.push(self.next().unwrap());
            }
            if self.eat('}') {
                let max = if n2.is_empty() {
                    None
                } else {
                    let m: u32 = n2.parse().map_err(|_| RegexError {
                        msg: "quantifier count too large".into(),
                    })?;
                    if m > 10_000 || m < min {
                        return rerr("invalid quantifier range");
                    }
                    Some(m)
                };
                return Ok(Some((min, max)));
            }
        }
        self.i = save; // literal brace
        Ok(None)
    }

    fn atom(&mut self) -> Result<Node, RegexError> {
        match self.peek() {
            Some('(') => {
                self.i += 1;
                if self.eat('?') {
                    match self.peek() {
                        Some(':') => {
                            self.i += 1;
                            let inner = self.alt()?;
                            self.expect_rparen()?;
                            Ok(inner)
                        }
                        Some('=') => {
                            self.i += 1;
                            let inner = self.alt()?;
                            self.expect_rparen()?;
                            Ok(Node::Look(false, Box::new(inner)))
                        }
                        Some('!') => {
                            self.i += 1;
                            let inner = self.alt()?;
                            self.expect_rparen()?;
                            Ok(Node::Look(true, Box::new(inner)))
                        }
                        Some('<') => {
                            // `(?<=` / `(?<!` are lookbehind; `(?<name>` is a
                            // named capture group, numbered like a plain `(`
                            self.i += 1;
                            if let Some(c @ ('=' | '!')) = self.peek() {
                                self.i += 1;
                                let inner = self.alt()?;
                                self.expect_rparen()?;
                                return Ok(Node::LookBehind(c == '!', Box::new(inner)));
                            }
                            let name = self.group_name()?;
                            if self.names.iter().any(|(n, _)| n == &name) {
                                return rerr(&format!("duplicate capture group name `{name}`"));
                            }
                            let g = self.group + 1;
                            if g > MAX_GROUPS {
                                return rerr("too many capture groups");
                            }
                            self.group = g;
                            self.names.push((name, g));
                            let inner = self.alt()?;
                            self.expect_rparen()?;
                            Ok(Node::Group(g, Box::new(inner)))
                        }
                        _ => rerr("unsupported group (?...)"),
                    }
                } else {
                    let g = self.group + 1;
                    if g > MAX_GROUPS {
                        return rerr("too many capture groups");
                    }
                    self.group = g;
                    let inner = self.alt()?;
                    self.expect_rparen()?;
                    Ok(Node::Group(g, Box::new(inner)))
                }
            }
            Some('[') => self.class(),
            Some('.') => {
                self.i += 1;
                Ok(Node::A(A::Any))
            }
            Some('^') => {
                self.i += 1;
                Ok(Node::A(A::StartMl(self.multiline)))
            }
            Some('$') => {
                self.i += 1;
                Ok(Node::A(A::EndMl(self.multiline)))
            }
            Some('*') | Some('+') | Some('?') | Some(')') => {
                rerr("nothing to repeat / unbalanced group")
            }
            Some('\\') => {
                self.i += 1;
                self.escape()
            }
            Some(ch) => {
                self.i += 1;
                Ok(Node::A(A::Char(ch)))
            }
            None => rerr("unexpected end of pattern"),
        }
    }

    fn expect_rparen(&mut self) -> Result<(), RegexError> {
        if self.eat(')') {
            Ok(())
        } else {
            rerr("unbalanced group")
        }
    }

    /// `name>` after `(?<` or `\k<`: an identifier-like name (letters, `_`,
    /// `$`, non-ASCII alphanumerics; digits only after the first char).
    fn group_name(&mut self) -> Result<String, RegexError> {
        let mut name = String::new();
        loop {
            let Some(c) = self.peek() else {
                return rerr("unterminated capture group name");
            };
            if c == '>' {
                self.i += 1;
                break;
            }
            let first = name.is_empty();
            let ok = c == '_'
                || c == '$'
                || if first { c.is_alphabetic() } else { c.is_alphanumeric() };
            if !ok {
                return rerr("invalid capture group name");
            }
            name.push(c);
            self.i += 1;
        }
        if name.is_empty() {
            return rerr("empty capture group name");
        }
        Ok(name)
    }

    fn escape(&mut self) -> Result<Node, RegexError> {
        let ch = match self.next() {
            Some(c) => c,
            None => return rerr("trailing backslash in pattern"),
        };
        if matches!(ch, 'p' | 'P') {
            let negated = ch == 'P';
            let kind = self.prop_kind()?;
            return Ok(Node::A(A::Class(ClassSet {
                ranges: Vec::new(),
                props: vec![(kind, negated)],
                neg: false,
            })));
        }
        Ok(Node::A(match ch {
            'd' => A::Class(dig_set(false)),
            'D' => A::Class(dig_set(true)),
            'w' => A::Class(word_set(false)),
            'W' => A::Class(word_set(true)),
            's' => A::Class(space_set(false)),
            'S' => A::Class(space_set(true)),
            'b' => A::WordB(true),
            'B' => A::WordB(false),
            'n' => A::Char('\n'),
            'r' => A::Char('\r'),
            't' => A::Char('\t'),
            'f' => A::Char('\u{c}'),
            'v' => A::Char('\u{b}'),
            '0' => A::Char('\0'),
            'x' => {
                let h = self.hex(2)?;
                A::Char(char::from_u32(h).ok_or_else(|| match_char_escape())?)
            }
            'u' => A::Char(self.unicode_escape()?),
            'c' => {
                // \cX control escape: A-Z -> ctrl char
                let l = self.peek().unwrap_or('\0');
                if ('A'..='Z').contains(&l) {
                    self.i += 1;
                    A::Char((l as u8 - b'A' + 1) as char)
                } else {
                    return rerr("bad \\c escape");
                }
            }
            '1'..='9' => {
                let mut n = ch.to_digit(10).unwrap();
                // greedy two-digit backref like JS
                if let Some(d2) = self.peek() {
                    if d2.is_ascii_digit() {
                        let cand = n * 10 + d2.to_digit(10).unwrap();
                        if cand <= MAX_GROUPS as u32 && cand <= self.group as u32 {
                            self.i += 1;
                            n = cand;
                        }
                    }
                }
                if n as usize > self.group {
                    return rerr("backref to a group that does not exist yet");
                }
                A::Backref(n as usize)
            }
            'k' => {
                // `\k<name>`: named backreference. Forward references (a name
                // defined later in the pattern) are valid JS but stay a gap
                // here, like the numeric backref rule above.
                if self.peek() == Some('<') {
                    self.i += 1;
                    let name = self.group_name()?;
                    let g = self
                        .names
                        .iter()
                        .find(|(n, _)| n == &name)
                        .map(|(_, g)| *g)
                        .ok_or_else(|| RegexError {
                            msg: format!("backref to an unknown group name `{name}`"),
                        })?;
                    A::Backref(g)
                } else {
                    A::Char('k') // Annex B identity escape
                }
            }
            c if c.is_ascii_alphanumeric() => {
                return rerr(&format!("unsupported escape `\\{}` in pattern", c));
            }
            other => A::Char(other), // punctuation identity escapes `\. \/ \[ ...`
        }))
    }

    fn hex(&mut self, n: usize) -> Result<u32, RegexError> {
        let mut s = String::new();
        for _ in 0..n {
            match self.peek() {
                Some(c) if c.is_ascii_hexdigit() => {
                    s.push(c);
                    self.i += 1;
                }
                _ => return rerr("bad hexadecimal escape in pattern"),
            }
        }
        u32::from_str_radix(&s, 16).map_err(|_| match_char_escape())
    }

    /// The code unit after a `\u` escape: `\uXXXX` or `\u{...}`. Lone
    /// surrogates map to U+FFFD (same convention as the string-literal
    /// unescaper). Used from the atom path and both ends of a class range,
    /// since the splitter's `[\u{E0020}-\u{E007E}]` puts the brace form at
    /// the upper bound.
    fn unicode_escape(&mut self) -> Result<char, RegexError> {
        if self.peek() == Some('{') {
            self.i += 1;
            let mut h = String::new();
            while matches!(self.peek(), Some(c) if c.is_ascii_hexdigit()) {
                h.push(self.next().unwrap());
            }
            if !self.eat('}') || h.is_empty() {
                return rerr("bad \\u{...} escape");
            }
            let v = u32::from_str_radix(&h, 16).map_err(|_| match_char_escape())?;
            Ok(char::from_u32(v).unwrap_or('\u{fffd}'))
        } else {
            Ok(char::from_u32(self.hex(4)?).unwrap_or('\u{fffd}'))
        }
    }

    fn class(&mut self) -> Result<Node, RegexError> {
        self.i += 1; // `[`
        let mut neg = false;
        if self.eat('^') {
            neg = true;
        }
        let mut ranges: Vec<(char, char)> = Vec::new();
        let mut props: Vec<(PropKind, bool)> = Vec::new();
        let mut first = true;
        loop {
            let ch = match self.peek() {
                None => return rerr("unterminated character class"),
                Some(']') if !first => {
                    self.i += 1;
                    break;
                }
                Some(c) => {
                    self.i += 1;
                    c
                }
            };
            first = false;
            let lo = match ch {
                '\\' => match self.next() {
                    None => return rerr("trailing backslash in class"),
                    Some('d') | Some('D') | Some('w') | Some('W') | Some('s') | Some('S') => {
                        let set = match self.c[self.i - 1] {
                            'd' => dig_set(false),
                            'D' => dig_set(true),
                            'w' => word_set(false),
                            'W' => word_set(true),
                            's' => space_set(false),
                            _ => space_set(true),
                        };
                        ranges.extend(set.ranges);
                        continue;
                    }
                    // inside a class \b is BACKSPACE (JS quirk), \B invalid
                    Some('b') => '\u{8}',
                    Some('n') => '\n',
                    Some('r') => '\r',
                    Some('t') => '\t',
                    Some('f') => '\u{c}',
                    Some('v') => '\u{b}',
                    Some('0') => '\0',
                    Some('x') => char::from_u32(self.hex(2)?).ok_or_else(match_char_escape)?,
                    Some('u') => self.unicode_escape()?,
                    Some('p') | Some('P') => {
                        let negated = self.c[self.i - 1] == 'P';
                        props.push((self.prop_kind()?, negated));
                        continue;
                    }
                    Some(c) if c.is_ascii_alphanumeric() => {
                        return rerr(&format!("unsupported escape `\\{}` in class", c));
                    }
                    Some(c) => c,
                },
                c => c,
            };
            // range?
            if self.peek() == Some('-')
                && self.c.get(self.i + 1).copied() != Some(']')
                && self.i + 1 < self.c.len()
            {
                self.i += 1;
                let hi = match self.next() {
                    Some('\\') => match self.next() {
                        Some('n') => '\n',
                        Some('r') => '\r',
                        Some('t') => '\t',
                        Some('x') => char::from_u32(self.hex(2)?).ok_or_else(match_char_escape)?,
                        Some('u') => self.unicode_escape()?,
                        Some(c) => c,
                        None => return rerr("unterminated character class"),
                    },
                    Some(c) => c,
                    None => return rerr("unterminated character class"),
                };
                if (hi as u32) < (lo as u32) {
                    return rerr("character class range out of order");
                }
                ranges.push((lo, hi));
            } else {
                ranges.push((lo, lo));
            }
        }
        Ok(Node::A(A::Class(ClassSet {
            ranges,
            props,
            neg,
        })))
    }

    /// Parse the property name after `\p` / `\P`: `{Name}` or a single
    /// letter. Unknown names are an honest compile error.
    fn prop_kind(&mut self) -> Result<PropKind, RegexError> {
        let name = if self.eat('{') {
            let mut s = String::new();
            loop {
                match self.next() {
                    Some('}') => break,
                    Some(c) => s.push(c),
                    None => return rerr("unterminated \\p{...} escape"),
                }
            }
            s
        } else {
            match self.next() {
                Some(c) => c.to_string(),
                None => return rerr("trailing \\p escape"),
            }
        };
        prop_kind(&name).ok_or_else(|| RegexError {
            msg: format!("unsupported unicode property escape \\p{{{name}}}"),
        })
    }
}

fn match_char_escape() -> RegexError {
    RegexError {
        msg: "bad character escape in pattern".into(),
    }
}

/// The property names accepted by `\p{...}` (long and short forms).
fn prop_kind(name: &str) -> Option<PropKind> {
    // `\p{Script=Greek}` / `\p{sc=Greek}` (and the Script_Extensions alias,
    // which this engine answers with the same block test). A property name
    // with `=` is never a binary property.
    if let Some((prop, value)) = name.split_once('=') {
        return match prop {
            "Script" | "sc" | "Script_Extensions" | "scx" => script_kind(value),
            _ => None,
        };
    }
    Some(match name {
        "L" | "Letter" => PropKind::Letter,
        "Lu" | "Uppercase_Letter" => PropKind::Upper,
        "Ll" | "Lowercase_Letter" => PropKind::Lower,
        "N" | "Number" => PropKind::Number,
        "Nd" | "Decimal_Number" => PropKind::Digit,
        "Z" | "Separator" | "Zs" | "Space_Separator" => PropKind::Space,
        "ASCII" => PropKind::Ascii,
        // GitHub's grapheme splitter (a `…|./gu` alternation) needs the emoji
        // properties to compile: without them the whole landing module was a
        // SyntaxError.
        "RI" | "Regional_Indicator" => PropKind::RegionalIndicator,
        "Emoji" => PropKind::Emoji,
        "EMod" | "Emoji_Modifier" => PropKind::EmojiModifier,
        // GitHub's code-view bundle classifies bidi controls to render source
        // files; without the property the whole bundle was a SyntaxError.
        "Bidi_C" | "Bidi_Control" => PropKind::BidiControl,
        // the same bundle tests source text for combining marks with
        // `\p{Mn}`, `\p{Mc}`, `\p{Me}` and their union
        "M" | "Mark" | "Mn" | "Nonspacing_Mark" | "Mc" | "Spacing_Mark" | "Me"
        | "Enclosing_Mark" => PropKind::Mark,
        // the issues page's "word character" class is
        // `[^\p{Letter}\p{Mark}\p{Number}\p{Connector_Punctuation}\p{Emoji}]`,
        // and the whole structured-issues module was a SyntaxError without it
        "Pc" | "Connector_Punctuation" => PropKind::ConnectorPunctuation,
        _ => return None,
    })
}

/// Script values accepted by `\p{Script=…}`: the ISO 15924 short tag and the
/// long name. The set is exactly what GitHub's code-view confusable check
/// asks for; an unlisted script is an honest compile error.
fn script_kind(v: &str) -> Option<PropKind> {
    Some(match v {
        "Greek" | "Grek" => PropKind::Greek,
        "Hebrew" | "Hebr" => PropKind::Hebrew,
        "Arabic" | "Arab" => PropKind::Arabic,
        "Syriac" | "Syrc" => PropKind::Syriac,
        "Thaana" | "Thaa" => PropKind::Thaana,
        "Mandaic" | "Mand" => PropKind::Mandaic,
        "Hiragana" | "Hira" => PropKind::Hiragana,
        "Katakana" | "Kana" => PropKind::Katakana,
        "Han" | "Hani" => PropKind::Han,
        _ => return None,
    })
}

fn prop_matches(kind: PropKind, ch: char) -> bool {
    match kind {
        PropKind::Letter => ch.is_alphabetic(),
        PropKind::Upper => ch.is_uppercase(),
        PropKind::Lower => ch.is_lowercase(),
        PropKind::Number | PropKind::Digit => ch.is_numeric(),
        PropKind::Space => ch.is_whitespace(),
        PropKind::Ascii => ch.is_ascii(),
        PropKind::RegionalIndicator => ('\u{1F1E6}'..='\u{1F1FF}').contains(&ch),
        PropKind::EmojiModifier => ('\u{1F3FB}'..='\u{1F3FF}').contains(&ch),
        // exact: ALM, LRM, RLM, the embedding/override controls and the
        // isolate controls (UAX #9)
        PropKind::BidiControl => matches!(
            ch as u32,
            0x061C | 0x200E | 0x200F | 0x202A..=0x202E | 0x2066..=0x2069
        ),
        // exact: the Unicode general category Pc is a closed list, and the
        // engine's `char` predicates have no connector test
        PropKind::ConnectorPunctuation => matches!(
            ch as u32,
            0x005F | 0x203F | 0x2040 | 0x2054 | 0xFE33 | 0xFE34 | 0xFE4D..=0xFE4F | 0xFF3F
        ),
        // honest shortcut: the Unicode `Emoji` property is approximated by
        // the symbol/pictograph blocks (and the keycap bases). The splitter
        // only groups a cluster; it never indexes into the matched text.
        PropKind::Emoji => is_emoji(ch),
        // shortcut: `Mn`/`Mc`/`Me` share one combining-mark block list, so a
        // spacing mark answers true for `\p{Mn}` too (see `PropKind::Mark`)
        PropKind::Mark => MARK_RANGES.iter().any(|&(a, b)| (a..=b).contains(&ch)),
        // script shortcuts: block membership, not the per-character property
        PropKind::Greek => in_ranges(ch, &[('\u{0370}', '\u{03FF}'), ('\u{1F00}', '\u{1FFF}')]),
        PropKind::Hebrew => in_ranges(ch, &[('\u{0590}', '\u{05FF}'), ('\u{FB1D}', '\u{FB4F}')]),
        PropKind::Arabic => in_ranges(
            ch,
            &[
                ('\u{0600}', '\u{06FF}'),
                ('\u{0750}', '\u{077F}'),
                ('\u{08A0}', '\u{08FF}'),
                ('\u{FB50}', '\u{FDFF}'),
                ('\u{FE70}', '\u{FEFF}'),
                ('\u{1EE00}', '\u{1EEFF}'),
            ],
        ),
        PropKind::Syriac => in_ranges(ch, &[('\u{0700}', '\u{074F}'), ('\u{0860}', '\u{086F}')]),
        PropKind::Thaana => in_ranges(ch, &[('\u{0780}', '\u{07BF}')]),
        PropKind::Mandaic => in_ranges(ch, &[('\u{0840}', '\u{085F}')]),
        PropKind::Hiragana => in_ranges(ch, &[('\u{3040}', '\u{309F}')]),
        PropKind::Katakana => in_ranges(
            ch,
            &[
                ('\u{30A0}', '\u{30FF}'),
                ('\u{31F0}', '\u{31FF}'),
                ('\u{FF66}', '\u{FF9D}'),
            ],
        ),
        PropKind::Han => in_ranges(
            ch,
            &[
                ('\u{3400}', '\u{4DBF}'),
                ('\u{4E00}', '\u{9FFF}'),
                ('\u{F900}', '\u{FAFF}'),
                ('\u{20000}', '\u{2EBEF}'),
                ('\u{30000}', '\u{3134F}'),
            ],
        ),
    }
}

fn in_ranges(ch: char, ranges: &[(char, char)]) -> bool {
    ranges.iter().any(|&(a, b)| (a..=b).contains(&ch))
}

/// The combining-mark blocks (`Mn` ∪ `Mc` ∪ `Me`), by script. Deliberately
/// block-granular: it over-accepts a few non-mark code points that share a
/// block with marks, but every real combining mark in the scripts below is in
/// it.
const MARK_RANGES: &[(char, char)] = &[
    ('\u{0300}', '\u{036F}'), // combining diacritics
    ('\u{0483}', '\u{0489}'), // Cyrillic
    ('\u{0591}', '\u{05BD}'),
    ('\u{05BF}', '\u{05BF}'),
    ('\u{05C1}', '\u{05C2}'),
    ('\u{05C4}', '\u{05C5}'),
    ('\u{05C7}', '\u{05C7}'), // Hebrew
    ('\u{0610}', '\u{061A}'),
    ('\u{064B}', '\u{065F}'),
    ('\u{0670}', '\u{0670}'),
    ('\u{06D6}', '\u{06ED}'), // Arabic
    ('\u{0711}', '\u{0711}'),
    ('\u{0730}', '\u{074A}'), // Syriac
    ('\u{07A6}', '\u{07B0}'),
    ('\u{07EB}', '\u{07F3}'), // Thaana / NKo
    ('\u{0816}', '\u{082D}'), // Samaritan
    ('\u{0859}', '\u{085B}'), // Mandaic
    ('\u{08D4}', '\u{0903}'), // Arabic extended / Devanagari
    ('\u{093A}', '\u{094F}'),
    ('\u{0951}', '\u{0957}'),
    ('\u{0962}', '\u{0963}'), // Devanagari
    ('\u{0981}', '\u{0983}'),
    ('\u{09BC}', '\u{09BC}'),
    ('\u{09BE}', '\u{09CD}'),
    ('\u{09D7}', '\u{09D7}'),
    ('\u{09E2}', '\u{09E3}'), // Bengali
    ('\u{0A01}', '\u{0A03}'),
    ('\u{0A3C}', '\u{0A42}'),
    ('\u{0A47}', '\u{0A48}'),
    ('\u{0A4B}', '\u{0A4D}'),
    ('\u{0A51}', '\u{0A51}'),
    ('\u{0A70}', '\u{0A71}'),
    ('\u{0A75}', '\u{0A75}'), // Gurmukhi
    ('\u{0A81}', '\u{0A83}'),
    ('\u{0ABC}', '\u{0ABC}'),
    ('\u{0ABE}', '\u{0AC5}'),
    ('\u{0AC7}', '\u{0ACD}'),
    ('\u{0AE2}', '\u{0AE3}'), // Gujarati
    ('\u{0B01}', '\u{0B03}'),
    ('\u{0B3C}', '\u{0B44}'),
    ('\u{0B47}', '\u{0B48}'),
    ('\u{0B4B}', '\u{0B4D}'),
    ('\u{0B56}', '\u{0B57}'),
    ('\u{0B62}', '\u{0B63}'), // Oriya
    ('\u{0B82}', '\u{0B82}'),
    ('\u{0BBE}', '\u{0BCD}'),
    ('\u{0BD7}', '\u{0BD7}'), // Tamil
    ('\u{0C00}', '\u{0C04}'),
    ('\u{0C3E}', '\u{0C44}'),
    ('\u{0C46}', '\u{0C48}'),
    ('\u{0C4A}', '\u{0C4D}'),
    ('\u{0C55}', '\u{0C56}'),
    ('\u{0C62}', '\u{0C63}'), // Telugu
    ('\u{0C81}', '\u{0C83}'),
    ('\u{0CBC}', '\u{0CBC}'),
    ('\u{0CBE}', '\u{0CCD}'),
    ('\u{0CD5}', '\u{0CD6}'),
    ('\u{0CE2}', '\u{0CE3}'), // Kannada
    ('\u{0D01}', '\u{0D03}'),
    ('\u{0D3B}', '\u{0D44}'),
    ('\u{0D46}', '\u{0D4D}'),
    ('\u{0D57}', '\u{0D57}'),
    ('\u{0D62}', '\u{0D63}'), // Malayalam
    ('\u{0DCA}', '\u{0DDF}'), // Sinhala
    ('\u{0E31}', '\u{0E3A}'),
    ('\u{0E47}', '\u{0E4E}'), // Thai
    ('\u{0EB1}', '\u{0EBC}'),
    ('\u{0EC8}', '\u{0ECD}'), // Lao
    ('\u{0F18}', '\u{0F19}'),
    ('\u{0F35}', '\u{0F35}'),
    ('\u{0F37}', '\u{0F37}'),
    ('\u{0F39}', '\u{0F39}'),
    ('\u{0F3E}', '\u{0F3F}'),
    ('\u{0F71}', '\u{0F84}'),
    ('\u{0F86}', '\u{0F87}'),
    ('\u{0F8D}', '\u{0FBC}'), // Tibetan
    ('\u{102B}', '\u{103E}'),
    ('\u{1056}', '\u{1059}'),
    ('\u{105E}', '\u{1060}'),
    ('\u{1062}', '\u{1064}'),
    ('\u{1067}', '\u{106D}'),
    ('\u{1071}', '\u{1074}'),
    ('\u{1082}', '\u{108D}'),
    ('\u{108F}', '\u{108F}'),
    ('\u{109A}', '\u{109D}'), // Myanmar
    ('\u{135D}', '\u{135F}'), // Ethiopic
    ('\u{1712}', '\u{1714}'),
    ('\u{1732}', '\u{1734}'),
    ('\u{1752}', '\u{1753}'),
    ('\u{1772}', '\u{1773}'), // Tagalog / Hanunoo / Buhid / Tagbanwa
    ('\u{17B4}', '\u{17D3}'),
    ('\u{17DD}', '\u{17DD}'), // Khmer
    ('\u{180B}', '\u{180D}'), // Mongolian
    ('\u{1920}', '\u{1922}'),
    ('\u{1927}', '\u{1928}'),
    ('\u{1932}', '\u{1932}'),
    ('\u{1939}', '\u{193B}'), // Limbu
    ('\u{1A17}', '\u{1A1B}'),
    ('\u{1A55}', '\u{1A5E}'),
    ('\u{1A60}', '\u{1A7C}'), // Tai Tham
    ('\u{1AB0}', '\u{1ABE}'), // combining extended
    ('\u{1B00}', '\u{1B04}'),
    ('\u{1B34}', '\u{1B44}'),
    ('\u{1B6B}', '\u{1B73}'), // Balinese
    ('\u{1B80}', '\u{1B82}'),
    ('\u{1BA1}', '\u{1BAD}'), // Sundanese
    ('\u{1BE6}', '\u{1BF3}'), // Batak
    ('\u{1C24}', '\u{1C37}'), // Lepcha
    ('\u{1CD0}', '\u{1CF9}'), // Vedic
    ('\u{1DC0}', '\u{1DFF}'), // combining supplement
    ('\u{20D0}', '\u{20F0}'), // combining marks for symbols
    ('\u{2CEF}', '\u{2CF1}'),
    ('\u{2D7F}', '\u{2D7F}'),
    ('\u{2DE0}', '\u{2DFF}'), // Cyrillic extended
    ('\u{302A}', '\u{302F}'),
    ('\u{3099}', '\u{309A}'), // CJK / kana
    ('\u{A66F}', '\u{A672}'),
    ('\u{A674}', '\u{A67D}'),
    ('\u{A69E}', '\u{A69F}'), // Cyrillic
    ('\u{A6F0}', '\u{A6F1}'),
    ('\u{A802}', '\u{A802}'),
    ('\u{A806}', '\u{A806}'),
    ('\u{A80B}', '\u{A80B}'),
    ('\u{A823}', '\u{A827}'), // Bamum / Syloti Nagri
    ('\u{A880}', '\u{A881}'),
    ('\u{A8B4}', '\u{A8C5}'), // Saurashtra
    ('\u{A8E0}', '\u{A8F1}'), // Devanagari extended
    ('\u{A926}', '\u{A92D}'),
    ('\u{A947}', '\u{A953}'), // Kayah Li / Rejang
    ('\u{A980}', '\u{A983}'),
    ('\u{A9B3}', '\u{A9C0}'), // Javanese
    ('\u{A9E5}', '\u{A9E5}'),
    ('\u{AA29}', '\u{AA36}'),
    ('\u{AA43}', '\u{AA43}'),
    ('\u{AA4C}', '\u{AA4C}'),
    ('\u{AA7B}', '\u{AA7D}'), // Cham / Meetei Mayek
    ('\u{AAB0}', '\u{AABF}'),
    ('\u{AAC1}', '\u{AAC1}'), // Tai Viet
    ('\u{AAEB}', '\u{AAEF}'),
    ('\u{AAF5}', '\u{AAF6}'),
    ('\u{ABE3}', '\u{ABEA}'),
    ('\u{ABEC}', '\u{ABED}'), // Meetei Mayek extended
    ('\u{FB1E}', '\u{FB1E}'), // Hebrew presentation
    ('\u{FE00}', '\u{FE0F}'), // variation selectors
    ('\u{FE20}', '\u{FE2F}'), // combining half marks
    ('\u{101FD}', '\u{101FD}'),
    ('\u{102E0}', '\u{102E0}'),
    ('\u{10376}', '\u{1037A}'),
    ('\u{10A01}', '\u{10A0F}'),
    ('\u{10A38}', '\u{10A3F}'),
    ('\u{10AE5}', '\u{10AE6}'),
    ('\u{11000}', '\u{11002}'),
    ('\u{11038}', '\u{11046}'),
    ('\u{1107F}', '\u{11082}'),
    ('\u{110B0}', '\u{110BA}'), // Brahmi / Kaithi / Chakma / Sharada
    ('\u{11100}', '\u{11102}'),
    ('\u{11127}', '\u{11134}'),
    ('\u{11173}', '\u{11173}'),
    ('\u{11180}', '\u{11182}'),
    ('\u{111B3}', '\u{111C0}'), // Khojki / Khudawadi
    ('\u{1122C}', '\u{11237}'),
    ('\u{112DF}', '\u{112EA}'), // Takri / Grantha
    ('\u{11300}', '\u{11303}'),
    ('\u{1133B}', '\u{1134D}'),
    ('\u{11357}', '\u{11357}'),
    ('\u{11362}', '\u{11374}'), // Newa / Tirhuta
    ('\u{11435}', '\u{11446}'),
    ('\u{114B0}', '\u{114C3}'), // Modi / Shiddham
    ('\u{115AF}', '\u{115C0}'),
    ('\u{11630}', '\u{11640}'), // Grantha / Newa
    ('\u{116AB}', '\u{116B7}'),
    ('\u{1171D}', '\u{1172B}'), // Takri / Ahom
    ('\u{11C2F}', '\u{11C3F}'), // Bhaiksuki
    ('\u{16AF0}', '\u{16AF4}'),
    ('\u{16B30}', '\u{16B36}'),
    ('\u{16F51}', '\u{16F92}'), // Miao / Kharoshthi
    ('\u{1BC9D}', '\u{1BC9E}'), // Duployan
    ('\u{1D165}', '\u{1D169}'),
    ('\u{1D16D}', '\u{1D182}'),
    ('\u{1D185}', '\u{1D18B}'),
    ('\u{1D1AA}', '\u{1D1AD}'), // musical symbols
    ('\u{1D242}', '\u{1D244}'),
    ('\u{1DA00}', '\u{1DA36}'),
    ('\u{1DA3B}', '\u{1DA6C}'),
    ('\u{1DA75}', '\u{1DA75}'),
    ('\u{1DA84}', '\u{1DA84}'),
    ('\u{1DA9B}', '\u{1DAAF}'), // signwriting
    ('\u{1E000}', '\u{1E02A}'),
    ('\u{1E8D0}', '\u{1E8D6}'),
    ('\u{1E944}', '\u{1E94A}'), // Glagolitic / Adlam
    ('\u{E0100}', '\u{E01EF}'), // variation selectors supplement
];

fn is_emoji(ch: char) -> bool {
    matches!(
        ch as u32,
        0x23
            | 0x2A
            | 0x30..=0x39
            | 0xA9
            | 0xAE
            | 0x203C
            | 0x2049
            | 0x2122
            | 0x2139
            | 0x2194..=0x21AA
            | 0x231A..=0x23FF
            | 0x24C2
            | 0x25AA..=0x25AB
            | 0x25B6
            | 0x25C0
            | 0x25FB..=0x27BF
            | 0x2934..=0x2935
            | 0x2B05..=0x2B07
            | 0x2B1B..=0x2B1C
            | 0x2B50
            | 0x2B55
            | 0x3030
            | 0x303D
            | 0x3297
            | 0x3299
            | 0x1F000..=0x1FAFF
    )
}

fn dig_set(neg: bool) -> ClassSet {
    ClassSet {
        ranges: vec![('0', '9')],
        props: Vec::new(),
        neg,
    }
}

fn word_set(neg: bool) -> ClassSet {
    ClassSet {
        ranges: vec![('0', '9'), ('A', 'Z'), ('_', '_'), ('a', 'z')],
        props: Vec::new(),
        neg,
    }
}

fn space_set(neg: bool) -> ClassSet {
    ClassSet {
        ranges: vec![
            ('\t', '\r'),
            (' ', ' '),
            ('\u{a0}', '\u{a0}'), // honest shortcut: NBSP only, not the full Unicode space list
            ('\u{feff}', '\u{feff}'),
        ],
        props: Vec::new(),
        neg,
    }
}

// ---------------------------------------------------------------------------
// compile
// ---------------------------------------------------------------------------

struct Compiler {
    prog: Vec<Inst>,
    /// lookaround nesting depth (compile-time MAX_LOOK_DEPTH cap)
    depth: usize,
    /// `s` flag: does `.` match newlines
    dot_all: bool,
}

fn compile_node(n: &Node, dot_all: bool) -> Result<(Vec<Inst>, Option<char>), RegexError> {
    let mut cx = Compiler {
        prog: Vec::new(),
        depth: 0,
        dot_all,
    };
    // whole-match capture slots 0/1 wrap the program
    cx.prog.push(Inst::Save(0));
    cx.emit(n)?;
    cx.prog.push(Inst::Save(1));
    cx.prog.push(Inst::Match);
    // leading literal: the first non-skippable instruction, when it is
    // exactly a Char — lets non-anchored find loops skip hopeless starts
    let mut first_char = None;
    for inst in cx.prog.iter() {
        match inst {
            Inst::Char(c) => {
                first_char = Some(*c);
                break;
            }
            Inst::AssertStart(_) | Inst::Save(_) => continue,
            _ => break,
        }
    }
    Ok((cx.prog, first_char))
}

impl Compiler {
    fn emit(&mut self, n: &Node) -> Result<(), RegexError> {
        self.depth_guard()?;
        match n {
            Node::Empty => {}
            Node::A(a) => self.emit_atom(a),
            Node::Concat(items) => {
                for i in items {
                    self.emit(i)?;
                }
            }
            Node::Alt(branches) => self.emit_alt(branches)?,
            Node::Group(g, inner) => {
                self.prog.push(Inst::Save(2 * g));
                self.emit(inner)?;
                self.prog.push(Inst::Save(2 * g + 1));
            }
            Node::Look(negate, inner) => {
                self.emit_look(*negate, inner)?;
            }
            Node::LookBehind(negate, inner) => {
                self.emit_look_behind(*negate, inner)?;
            }
            Node::Repeat {
                min,
                max,
                greedy,
                body,
            } => self.emit_repeat(*min, *max, *greedy, body)?,
        }
        Ok(())
    }

    fn depth_guard(&self) -> Result<(), RegexError> {
        if self.prog.len() > MAX_PROG {
            return rerr("pattern compiles too large");
        }
        Ok(())
    }

    fn emit_atom(&mut self, a: &A) {
        let inst = match a {
            A::Char(c) => Inst::Char(*c),
            A::Any => Inst::Any(self.dot_all),
            A::Class(cs) => Inst::Class(Box::new(cs.clone())),
            A::Backref(g) => Inst::Backref(*g),
            A::StartMl(m) => Inst::AssertStart(*m),
            A::EndMl(m) => Inst::AssertEnd(*m),
            A::WordB(w) => Inst::WordB(*w),
        };
        self.prog.push(inst);
    }

    fn emit_alt(&mut self, branches: &[Node]) -> Result<(), RegexError> {
        // classic chain: split -> branch0 -> jmp end; split -> branch1 -> ...
        let mut jumps = Vec::new();
        for (i, b) in branches.iter().enumerate() {
            if i + 1 < branches.len() {
                let sp = self.prog.len();
                self.prog.push(Inst::Split(0, 0)); // patched below
                let bstart = self.prog.len();
                self.emit(b)?;
                let j = self.prog.len();
                self.prog.push(Inst::Jmp(0)); // patched below
                jumps.push(j);
                let next_sp = self.prog.len();
                self.prog[sp] = Inst::Split(bstart, next_sp);
            } else {
                self.emit(b)?;
            }
        }
        let end = self.prog.len();
        for j in jumps {
            self.prog[j] = Inst::Jmp(end);
        }
        Ok(())
    }

    fn emit_look(&mut self, negate: bool, inner: &Node) -> Result<(), RegexError> {
        self.depth += 1;
        if self.depth > MAX_LOOK_DEPTH {
            return rerr("lookaround nested too deeply");
        }
        let look_pc = self.prog.len();
        self.prog.push(Inst::Look(negate, 0, 0)); // targets patched below
        self.emit(inner)?;
        self.prog.push(Inst::SubEnd);
        let end = self.prog.len();
        self.prog[look_pc] = Inst::Look(negate, look_pc + 1, end);
        self.depth -= 1;
        Ok(())
    }

    fn emit_look_behind(&mut self, negate: bool, inner: &Node) -> Result<(), RegexError> {
        self.depth += 1;
        if self.depth > MAX_LOOK_DEPTH {
            return rerr("lookaround nested too deeply");
        }
        let look_pc = self.prog.len();
        self.prog.push(Inst::LookBack(negate, 0, 0)); // targets patched below
        self.emit(inner)?;
        self.prog.push(Inst::SubEnd);
        let end = self.prog.len();
        self.prog[look_pc] = Inst::LookBack(negate, look_pc + 1, end);
        self.depth -= 1;
        Ok(())
    }

    /// quantified repetition. `{n,m}` expands by copying the body; a compile-
    /// size cap keeps the program bounded (over-cap is an honest error).
    fn emit_repeat(
        &mut self,
        min: u32,
        max: Option<u32>,
        greedy: bool,
        body: &Node,
    ) -> Result<(), RegexError> {
        // mandatory copies
        for _ in 0..min {
            self.emit(body)?;
            if self.prog.len() > MAX_PROG {
                return rerr("pattern compiles too large");
            }
        }
        match max {
            None => {
                // star loop: L: split(body, exit); body...; jmp L; exit:
                let l = self.prog.len();
                self.prog.push(Inst::Split(0, 0));
                let body_start = self.prog.len();
                self.emit(body)?;
                self.prog.push(Inst::Jmp(l));
                let exit = self.prog.len();
                self.prog[l] = if greedy {
                    Inst::Split(body_start, exit)
                } else {
                    Inst::Split(exit, body_start)
                };
            }
            Some(m) => {
                // (m - min) optional copies chained linearly: each block is
                // `split -> body`; the split's second arm skips to the end
                // (greedy tries the body first, lazy tries skipping first)
                let opts = m - min;
                let mut splits = Vec::new();
                for _ in 0..opts {
                    let sp = self.prog.len();
                    self.prog.push(Inst::Split(0, 0));
                    splits.push(sp);
                    self.emit(body)?;
                    if self.prog.len() > MAX_PROG {
                        return rerr("pattern compiles too large");
                    }
                }
                let end = self.prog.len();
                for sp in splits {
                    self.prog[sp] = if greedy {
                        Inst::Split(sp + 1, end)
                    } else {
                        Inst::Split(end, sp + 1)
                    };
                }
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// flags
// ---------------------------------------------------------------------------

fn parse_flags_str(flags: &str) -> Result<(bool, bool, bool, bool, bool), RegexError> {
    let mut g = false;
    let mut i = false;
    let mut m = false;
    let mut s = false;
    let mut y = false;
    for ch in flags.chars() {
        match ch {
            'g' if !g => g = true,
            'i' if !i => i = true,
            'm' if !m => m = true,
            's' if !s => s = true,
            'y' if !y => y = true,
            // `u`/`v` only change escape/class grammar; for the patterns we
            // support the matching result is identical, so accept and ignore
            // them. `d` (match indices) is likewise an output-only flag.
            'u' | 'v' | 'd' => {}
            _ => return rerr("invalid or repeated regex flag"),
        }
    }
    Ok((g, i, m, s, y))
}

pub(crate) fn compile(src: &str, flags: &str) -> Result<Regex, RegexError> {
    let (global, ignore_case, multiline, dot_all, sticky) = parse_flags_str(flags)?;
    let mut p = PatParser {
        c: src.chars().collect(),
        i: 0,
        group: 0,
        names: Vec::new(),
        multiline,
        dot_all,
    };
    let node = p.parse()?;
    let (prog, first_char) = compile_node(&node, dot_all)?;
    Ok(Regex {
        src: src.to_string(),
        flags: flags.to_string(),
        global,
        sticky,
        ignore_case,
        multiline,
        dot_all,
        prog,
        first_char,
        n_groups: p.group,
        names: p.names,
    })
}

// ---------------------------------------------------------------------------
// matching VM
// ---------------------------------------------------------------------------

fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

enum VmErr {
    /// step/stack budget exhausted: honest "no match" upstream
    Limit,
}

/// lowercase fold for `i`-flag comparisons (ASCII fast path, then the first
/// Unicode lowercase mapping — an honest simplification)
fn ch_fold(ch: char) -> char {
    if ch.is_ascii() {
        ch.to_ascii_lowercase()
    } else {
        ch.to_lowercase().next().unwrap_or(ch)
    }
}

fn ch_eq(a: char, b: char, ci: bool) -> bool {
    a == b || (ci && ch_fold(a) == ch_fold(b))
}

/// does `ch` satisfy this class? (`ci` handled here, ranges stay pre-folded)
fn in_class(set: &ClassSet, ch: char, ci: bool) -> bool {
    let hit = |c: char| {
        set.ranges.iter().any(|&(lo, hi)| c >= lo && c <= hi)
            || set
                .props
                .iter()
                .any(|&(kind, neg)| prop_matches(kind, c) != neg)
    };
    let mut m = hit(ch);
    if !m && ci {
        for v in [ch.to_ascii_lowercase(), ch.to_ascii_uppercase(), ch_fold(ch)] {
            if v != ch && hit(v) {
                m = true;
                break;
            }
        }
    }
    m != set.neg
}

/// explicit backtrack frame: caps ride along so restoring a frame restores
/// the exact capture state (copy-on-write Rc — the clone is a pointer bump
/// until a Save actually mutates through `Rc::make_mut`)
#[derive(Clone)]
struct Frame {
    pc: usize,
    pos: usize,
    caps: Caps,
}

/// One VM run. NON-RECURSIVE on the main path (explicit backtrack stack);
/// only lookaround spawns a bounded-depth sub-run that shares `steps`, so
/// the total CPU per `find` call stays inside MAX_STEPS.
fn run(
    prog: &[Inst],
    input: &[char],
    ci: bool,
    mut caps: Caps,
    mut pc: usize,
    mut pos: usize,
    steps: &mut usize,
    depth: usize,
    // lookbehind only: the sub program must finish exactly here
    end_at: Option<usize>,
) -> Result<Option<Caps>, VmErr> {
    let mut stack: Vec<Frame> = Vec::new();
    loop {
        *steps += 1;
        if *steps > MAX_STEPS {
            return Err(VmErr::Limit);
        }
        // dead end on this path: pop and resume the most recent choice
        macro_rules! backtrack {
            () => {
                match stack.pop() {
                    Some(f) => {
                        pc = f.pc;
                        pos = f.pos;
                        caps = f.caps;
                    }
                    None => return Ok(None),
                }
            };
        }
        match prog[pc] {
            Inst::Char(c) => {
                if pos < input.len() && ch_eq(input[pos], c, ci) {
                    pos += 1;
                    pc += 1;
                } else {
                    backtrack!();
                }
            }
            Inst::Any(dot_all) => {
                if pos < input.len() && (dot_all || input[pos] != '\n') {
                    pos += 1;
                    pc += 1;
                } else {
                    backtrack!();
                }
            }
            Inst::Class(ref set) => {
                if pos < input.len() && in_class(set, input[pos], ci) {
                    pos += 1;
                    pc += 1;
                } else {
                    backtrack!();
                }
            }
            Inst::Split(a, b) => {
                if stack.len() >= MAX_BACKTRACK {
                    return Err(VmErr::Limit);
                }
                stack.push(Frame {
                    pc: b,
                    pos,
                    caps: caps.clone(),
                });
                pc = a;
            }
            Inst::Jmp(t) => pc = t,
            Inst::Save(slot) => {
                // even slot opens (records `pos` twice); the matching odd
                // slot closes and rewrites (start, end)
                let start = if slot % 2 == 1 {
                    caps[slot - 1].map(|(s, _)| s).unwrap_or(pos)
                } else {
                    pos
                };
                Rc::make_mut(&mut caps)[slot] = Some((start, pos));
                pc += 1;
            }
            Inst::Backref(g) => match caps[2 * g + 1] {
                Some((s, e)) => {
                    let len = e - s;
                    let mut ok = pos + len <= input.len();
                    if ok {
                        for k in 0..len {
                            if !ch_eq(input[s + k], input[pos + k], ci) {
                                ok = false;
                                break;
                            }
                        }
                    }
                    if ok {
                        pos += len;
                        pc += 1;
                    } else {
                        backtrack!();
                    }
                }
                // backref to an unparticipated group matches the empty string
                None => pc += 1,
            },
            Inst::Look(negate, start, end) => {
                if depth >= MAX_LOOK_DEPTH {
                    return Err(VmErr::Limit);
                }
                // sub-run over the body shares the step budget; a positive
                // lookaround's captures persist, so its snapshot is adopted.
                // Look's `end` patch already points one past SubEnd (the
                // instruction AFTER the zero-width body), so the verdict
                // resumes there either way
                let sub =
                    run(prog, input, ci, caps.clone(), start, pos, steps, depth + 1, None)?;
                match sub {
                    Some(scaps) if !negate => {
                        caps = scaps;
                        pc = end;
                    }
                    Some(_) => backtrack!(),
                    None if negate => pc = end,
                    None => backtrack!(),
                }
            }
            Inst::LookBack(negate, start, end) => {
                if depth >= MAX_LOOK_DEPTH {
                    return Err(VmErr::Limit);
                }
                // Try the left contexts nearest-first. `end_at` is what makes
                // this a lookbehind: the sub run may take any path through its
                // body, but only one that lands exactly on the current
                // position counts, so `(?<=ab)` cannot match "b".
                // shortcut: with a capture inside an ambiguous body the group
                // is the nearest successful start's, where a browser's
                // right-to-left run would prefer the greedy one.
                let mut hit: Option<Caps> = None;
                let mut from = pos;
                loop {
                    let sub = run(
                        prog,
                        input,
                        ci,
                        caps.clone(),
                        start,
                        from,
                        steps,
                        depth + 1,
                        Some(pos),
                    )?;
                    if let Some(scaps) = sub {
                        hit = Some(scaps);
                        break;
                    }
                    if from == 0 {
                        break;
                    }
                    from -= 1;
                }
                match (hit, negate) {
                    (Some(scaps), false) => {
                        caps = scaps;
                        pc = end;
                    }
                    (Some(_), true) => backtrack!(),
                    (None, true) => pc = end,
                    (None, false) => backtrack!(),
                }
            }
            Inst::SubEnd => {
                // a lookbehind sub run must stop on the assertion's position;
                // any other success is just one more dead end
                if let Some(want) = end_at {
                    if pos != want {
                        backtrack!();
                    }
                }
                return Ok(Some(caps));
            }
            Inst::AssertStart(ml) => {
                if pos == 0 || (ml && pos > 0 && input[pos - 1] == '\n') {
                    pc += 1;
                } else {
                    backtrack!();
                }
            }
            Inst::AssertEnd(ml) => {
                let ok = pos == input.len() || (ml && input[pos] == '\n');
                if ok {
                    pc += 1;
                } else {
                    backtrack!();
                }
            }
            Inst::WordB(want) => {
                let a = if pos > 0 { is_word(input[pos - 1]) } else { false };
                let b = input.get(pos).copied().map_or(false, is_word);
                if (a != b) == want {
                    pc += 1;
                } else {
                    backtrack!();
                }
            }
            Inst::Match => return Ok(Some(caps)),
        }
    }
}

impl Regex {
    /// Match at and after `from`. Sticky (`y`) only tries exactly at `from`;
    /// global lastIndex bookkeeping between calls is the caller's job.
    /// The step budget is per call, so scanning a long string with a
    /// pathological pattern terminates as "no match" instead of hanging.
    pub(crate) fn find(&self, input: &[char], from: usize) -> Option<MatchVal> {
        let mut steps = 0usize;
        let mut pos = from.min(input.len());
        loop {
            // leading-literal fast skip (installed only when the program's
            // first real instruction is a literal Char)
            if let Some(fc) = self.first_char {
                match input.get(pos).copied() {
                    Some(c) if ch_eq(c, fc, self.ignore_case) => {}
                    Some(_) => {
                        if self.sticky {
                            return None;
                        }
                        pos += 1;
                        continue;
                    }
                    None => return None,
                }
            }
            let caps: Caps = Rc::new(vec![None; 2 * (self.n_groups + 1)]);
            match run(
                &self.prog,
                input,
                self.ignore_case,
                caps,
                0,
                pos,
                &mut steps,
                0,
                None,
            ) {
                Ok(Some(mcaps)) => {
                    // overall bounds = (opening save's start, closing save's
                    // end); slot tuples are (open,(s,s)) and (close,(s,e))
                    let (start, end) = (mcaps[0].unwrap().0, mcaps[1].unwrap().1);
                    return Some(MatchVal {
                        start,
                        end,
                        caps: (*mcaps).clone(),
                    });
                }
                Ok(None) => {
                    if self.sticky || pos >= input.len() {
                        return None;
                    }
                    pos += 1;
                }
                // budget exhausted: honest "no match" (see module docs)
                Err(VmErr::Limit) => return None,
            }
        }
    }
}
