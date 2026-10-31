//! From-scratch regular-expression engine (no crates; zero dependencies
//! beyond `std`). Written for the browser's JS interpreter: small, honestly
//! bounded, and good enough for real site patterns.
//!
//! # Design (one paragraph)
//! Patterns are parsed into a small AST, then compiled to a flat instruction
//! program (Char/Any/Class/Split/Jmp/Save/Backref/Assert/Look/SubEnd/Match â€”
//! a textbook backtracking VM layout). Matching itself is a NON-RECURSIVE
//! loop with an explicit backtrack stack, because loops must not consume
//! native stack for long inputs and backrefs/lookaround need the exact
//! backpointers a Thompson NFA cannot represent (lookaround only; it runs as
//! a bounded-depth recursive sub-VM sharing the same step budget). Stack
//! entries are copy-on-write `Rc` capture snapshots: `Save` mutates through
//! `Rc::make_mut`, so restoring `k` stack entries costs O(1) unless the
//! branches actually diverged.
//!
//! # Resource bounds (RAM/CPU honesty) â€” hard caps, not best effort
//! - `MAX_STEPS` (100k instruction dispatches per `find` call): a
//!   catastrophic-backtracking pattern (`(a+)+$` vs a long failing string)
//!   terminates at the cap and reports NO MATCH (with the honest limitation
//!   that an over-limit result is "no match", not a pending-forever hang).
//! - `MAX_BACKTRACK` (8k stack entries): bounds the backtrack stack; each
//!   entry is 24 bytes + shared caps snapshot, so worst-case backtrack
//!   memory is ~200 KB per running match.
//! - Capture groups capped at 99 (`\1`..`\99`); lookahead nesting capped at
//!   6; `{n,m}` unrolling capped so a compiled program stays under
//!   `MAX_PROG` (50k instructions) â€” bigger is an honest compile error.
//! - Compiled programs are deduped per (pattern, flags) on the engine
//!   (see `JsEngine::regex_cache`), so a literal pays one compile.

use std::rc::Rc;

/// CPU bound: instruction dispatches per `find` call. Exceed â†’ no match.
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
    /// `.` â€” matches any char except `\n` unless dot_all
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
    /// end of a lookaround sub program (never reached by the main path)
    SubEnd,
    Match,
    /// `^` â€” multiline
    AssertStart(bool),
    /// `$` â€” multiline
    AssertEnd(bool),
    /// `\b` (true) / `\B` (false)
    WordB(bool),
}

#[derive(Debug, Clone)]
pub(crate) struct ClassSet {
    pub ranges: Vec<(char, char)>,
    pub neg: bool,
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
                            // honest gap: lookbehind (and named groups,
                            // which share its syntax) are not supported
                            rerr("lookbehind / named groups are not supported")
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

    fn escape(&mut self) -> Result<Node, RegexError> {
        let ch = match self.next() {
            Some(c) => c,
            None => return rerr("trailing backslash in pattern"),
        };
        if matches!(ch, 'p' | 'P') {
            return rerr("unicode property escapes (\\p) are not supported");
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
            'u' => {
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
                    A::Char(char::from_u32(v).unwrap_or('\u{fffd}'))
                } else {
                    let hh = self.hex(4)?;
                    // honest shortcut: lone surrogates map to U+FFFD (same
                    // convention as the string-literal unescaper)
                    A::Char(char::from_u32(hh).unwrap_or('\u{fffd}'))
                }
            }
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

    fn class(&mut self) -> Result<Node, RegexError> {
        self.i += 1; // `[`
        let mut neg = false;
        if self.eat('^') {
            neg = true;
        }
        let mut ranges: Vec<(char, char)> = Vec::new();
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
                    Some('u') => {
                        if self.peek() == Some('{') {
                            self.i += 1;
                            let mut h = String::new();
                            while matches!(self.peek(), Some(c) if c.is_ascii_hexdigit()) {
                                h.push(self.next().unwrap());
                            }
                            if !self.eat('}') || h.is_empty() {
                                return rerr("bad \\u{...} escape in class");
                            }
                            char::from_u32(u32::from_str_radix(&h, 16).map_err(|_| match_char_escape())?)
                                .unwrap_or('\u{fffd}')
                        } else {
                            char::from_u32(self.hex(4)?).unwrap_or('\u{fffd}')
                        }
                    }
                    Some('p') | Some('P') => {
                        return rerr("unicode property escapes (\\p) are not supported")
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
                        Some('u') => {
                            char::from_u32(self.hex(4)?).unwrap_or('\u{fffd}')
                        }
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
        Ok(Node::A(A::Class(ClassSet { ranges, neg })))
    }
}

fn match_char_escape() -> RegexError {
    RegexError {
        msg: "bad character escape in pattern".into(),
    }
}

fn dig_set(neg: bool) -> ClassSet {
    ClassSet { ranges: vec![('0', '9')], neg }
}

fn word_set(neg: bool) -> ClassSet {
    ClassSet {
        ranges: vec![('0', '9'), ('A', 'Z'), ('_', '_'), ('a', 'z')],
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
    // exactly a Char â€” lets non-anchored find loops skip hopeless starts
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
            'u' | 'v' => return rerr("unicode flag is not supported"),
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
/// Unicode lowercase mapping â€” an honest simplification)
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
    let hit = |c: char| set.ranges.iter().any(|&(lo, hi)| c >= lo && c <= hi);
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
/// the exact capture state (copy-on-write Rc â€” the clone is a pointer bump
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
                    run(prog, input, ci, caps.clone(), start, pos, steps, depth + 1)?;
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
            Inst::SubEnd => return Ok(Some(caps)),
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
