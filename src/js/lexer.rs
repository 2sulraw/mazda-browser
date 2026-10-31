//! Hand-rolled lexer for the JS subset (see js.rs module docs for scope).
//!
//! Tokens carry byte spans (`Span`) so parser errors can report line/col.
//!
//! shortcuts (v1, all honest):
//! - regex literal vs division is decided with the standard lexical
//!   previous-token heuristic (a `/` after a value-ish token â€” number, str,
//!   template, identifier, `)`, `]`, prefix/postfix `++`/`--` â€” reads as
//!   division; anything else starts a regex literal). This cannot see parser
//!   state, so pathological spacing like `var r = {a:1} /2` (object literal
//!   then division) would mislex as a regex â€” such shapes are vanishingly
//!   rare in real code and produce a regex-parse error, not silent misbehavior.
//! - identifiers are ASCII-only (`A-Za-z_$` + digits). Non-ASCII bytes outside
//!   strings/comments/templates are a syntax error. Unicode identifiers are a
//!   later wave.
//! - keywords are strict (no contextual keywords): `let`/`of`/`in` etc. can
//!   never be used as identifiers.
//! - BigInt literals (`123n`) are a syntax error, not silently misparsed.
//! - `**` (exponent) is an explicit "not supported" error rather than being
//!   parsed as `* *`.

use std::rc::Rc;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Tok {
    Num(f64),
    Str(Rc<str>),
    /// Whole template literal, `` `lit${expr}lit${expr}lit` ``. Interpolated
    /// expressions are stored as token vectors; the parser turns each into an
    /// Expr (and the whole template into string concatenation).
    Template(Vec<TplPart>),
    Ident(Rc<str>),
    /// regex literal `/pattern/flags` â€” pattern and flags kept raw (escapes
    /// unescaped later by the regex engine, which owns them)
    Regex(Rc<str>, Rc<str>),
    /// `...` â€” spread in calls/arrays/objects, rest in params/patterns
    Spread,
    Kw(Kw),
    Op(Op),
    Punct(Punct),
    Eof,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TplPart {
    Lit(Rc<str>),
    Expr(Vec<Token>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Token {
    pub tok: Tok,
    pub start: u32,
    pub end: u32,
    /// a newline appeared before this token (used for ASI + `return` rules)
    pub nl: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kw {
    Var,
    Let,
    Const,
    Function,
    Return,
    If,
    Else,
    While,
    Do,
    For,
    In,
    Of,
    Break,
    Continue,
    New,
    Typeof,
    Void,
    Delete,
    True,
    False,
    Null,
    This,
    Try,
    Catch,
    Finally,
    Throw,
    Class,
    Extends,
    Static,
    Super,
    Switch,
    Case,
    Default,
    Instanceof,
    Async,
    Await,
    Yield,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Op {
    Assign,
    AddAssign,
    SubAssign,
    MulAssign,
    DivAssign,
    ModAssign,
    AndAssign,
    OrAssign,
    XorAssign,
    ShlAssign,
    ShrAssign,
    UShrAssign,
    StrictEq,
    StrictNe,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    And,
    Or,
    Xor,
    Not,
    BitNot,
    Shl,
    Shr,
    UShr,
    AndAnd,
    OrOr,
    Coalesce,
    Inc,
    Dec,
    Arrow,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Punct {
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBrack,
    RBrack,
    Semi,
    Comma,
    Dot,
    Colon,
    Question,
}

pub struct LexError {
    pub msg: String,
    pub offset: u32,
}

pub fn tokenize(src: &str) -> Result<Vec<Token>, LexError> {
    let mut lx = Lexer { src, pos: 0 };
    let mut out: Vec<Token> = Vec::new();
    loop {
        let t = lx.next_token(regex_allowed(out.last().map(|t: &Token| &t.tok)))?;
        let eof = t.tok == Tok::Eof;
        out.push(t);
        if eof {
            return Ok(out);
        }
    }
}

/// Lexical regex-vs-division decision (see module docs): a `/` directly
/// following a value-ish token is division; otherwise it opens a regex
/// literal. We deliberately cannot see parser state (block vs object `}`), so
/// after `}` we choose regex â€” statement boundaries are the common real-world
/// reading.
fn regex_allowed(prev: Option<&Tok>) -> bool {
    !matches!(
        prev,
        Some(Tok::Num(_))
            | Some(Tok::Str(_))
            | Some(Tok::Template(_))
            | Some(Tok::Ident(_))
            | Some(Tok::Kw(Kw::True | Kw::False | Kw::Null | Kw::This))
            | Some(Tok::Punct(Punct::RParen | Punct::RBrack))
            | Some(Tok::Op(Op::Inc | Op::Dec))
    )
}

struct Lexer<'a> {
    src: &'a str,
    pos: usize,
}

fn kw_of(s: &str) -> Option<Kw> {
    Some(match s {
        "var" => Kw::Var,
        "let" => Kw::Let,
        "const" => Kw::Const,
        "function" => Kw::Function,
        "return" => Kw::Return,
        "if" => Kw::If,
        "else" => Kw::Else,
        "while" => Kw::While,
        "do" => Kw::Do,
        "for" => Kw::For,
        "in" => Kw::In,
        "of" => Kw::Of,
        "break" => Kw::Break,
        "continue" => Kw::Continue,
        "new" => Kw::New,
        "typeof" => Kw::Typeof,
        "void" => Kw::Void,
        "delete" => Kw::Delete,
        "true" => Kw::True,
        "false" => Kw::False,
        "null" => Kw::Null,
        "this" => Kw::This,
        "try" => Kw::Try,
        "catch" => Kw::Catch,
        "finally" => Kw::Finally,
        "throw" => Kw::Throw,
        "class" => Kw::Class,
        "extends" => Kw::Extends,
        "static" => Kw::Static,
        "super" => Kw::Super,
        "switch" => Kw::Switch,
        "case" => Kw::Case,
        "default" => Kw::Default,
        "instanceof" => Kw::Instanceof,
        // contextual keywords promoted to reserved (matches the strict-keyword
        // convention of the rest of this lexer): `async`, `await`, `yield`
        "async" => Kw::Async,
        "await" => Kw::Await,
        "yield" => Kw::Yield,
        _ => return None,
    })
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b == b'$'
}

fn is_ident_char(b: u8) -> bool {
    is_ident_start(b) || b.is_ascii_digit()
}

impl<'a> Lexer<'a> {
    fn err<T>(&self, msg: &str, offset: usize) -> Result<T, LexError> {
        Err(LexError {
            msg: msg.to_string(),
            offset: offset as u32,
        })
    }

    fn peek(&self) -> Option<u8> {
        self.src.as_bytes().get(self.pos).copied()
    }

    fn peek2(&self) -> Option<u8> {
        self.src.as_bytes().get(self.pos + 1).copied()
    }

    /// Skip whitespace + comments; returns true if a newline was crossed.
    fn skip_ws(&mut self) -> bool {
        let mut nl = false;
        loop {
            match self.peek() {
                Some(b'\n') | Some(b'\r') => {
                    nl = true;
                    self.pos += 1;
                }
                Some(b' ') | Some(b'\t') | Some(0x0b) | Some(0x0c) => {
                    self.pos += 1
                }
                Some(b'/') if self.peek2() == Some(b'/') => {
                    while let Some(b) = self.peek() {
                        if b == b'\n' {
                            break;
                        }
                        self.pos += 1;
                    }
                }
                Some(b'/') if self.peek2() == Some(b'*') => {
                    self.pos += 2;
                    loop {
                        match self.peek() {
                            None => break, // unterminated block comment: tolerate at EOF
                            Some(b'*') if self.peek2() == Some(b'/') => {
                                self.pos += 2;
                                break;
                            }
                            Some(b'\n') | Some(b'\r') => {
                                nl = true;
                                self.pos += 1;
                            }
                            Some(_) => self.pos += 1,
                        }
                    }
                }
                _ => return nl,
            }
        }
    }

    fn next_token(&mut self, regex_ok: bool) -> Result<Token, LexError> {
        let nl = self.skip_ws();
        let start = self.pos;
        let b = match self.peek() {
            None => {
                return Ok(Token {
                    tok: Tok::Eof,
                    start: start as u32,
                    end: start as u32,
                    nl,
                })
            }
            Some(b) => b,
        };
        let tok = if is_ident_start(b) {
            self.pos += 1;
            while matches!(self.peek(), Some(c) if is_ident_char(c)) {
                self.pos += 1;
            }
            let s = &self.src[start..self.pos];
            match kw_of(s) {
                Some(k) => Tok::Kw(k),
                None => Tok::Ident(Rc::from(s)),
            }
        } else if b.is_ascii_digit() {
            self.read_number(start)?
        } else if b == b'.' && matches!(self.peek2(), Some(c) if c.is_ascii_digit()) {
            self.read_number(start)?
        } else if b == b'"' || b == b'\'' {
            self.pos += 1;
            let raw = self.read_raw_string(b, start)?;
            let un = self.unescape(raw, start)?;
            Tok::Str(Rc::from(un.as_str()))
        } else if b == b'`' {
            self.pos += 1;
            let parts = self.read_template(start)?;
            Tok::Template(parts)
        } else if b == b'/' && regex_ok {
            self.pos += 1; // consume the opening `/`; read_regex reads the body
            self.read_regex(start)?
        } else {
            self.read_punct(start, b)?
        };
        let end = self.pos;
        Ok(Token {
            tok,
            start: start as u32,
            end: end as u32,
            nl,
        })
    }

    fn read_number(&mut self, start: usize) -> Result<Tok, LexError> {
        let b0 = self.peek();
        if b0 == Some(b'0') {
            if let Some(x) = self.peek2() {
                let radix = match x {
                    b'x' | b'X' => 16,
                    b'b' | b'B' => 2,
                    b'o' | b'O' => 8,
                    _ => 0,
                };
                if radix != 0 {
                    self.pos += 2;
                    let dstart = self.pos;
                    while matches!(self.peek(), Some(c) if c.is_ascii_alphanumeric()) {
                        self.pos += 1;
                    }
                    if self.peek() == Some(b'n') {
                        return self.err("BigInt literals are not supported (later wave)", start);
                    }
                    let digits = &self.src[dstart..self.pos];
                    return match u64::from_str_radix(digits, radix) {
                        Ok(v) => Ok(Tok::Num(v as f64)),
                        Err(_) => self.err("malformed numeric literal", start),
                    };
                }
            }
        }
        // decimal: digits [. digits] [eE [+-] digits]
        while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            self.pos += 1;
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            let save = self.pos;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.pos += 1;
            }
            if matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                    self.pos += 1;
                }
            } else {
                self.pos = save; // `1e` is just `1` followed by ident `e`
            }
        }
        if self.peek() == Some(b'n') {
            return self.err("BigInt literals are not supported (later wave)", start);
        }
        let s = &self.src[start..self.pos];
        match s.parse::<f64>() {
            Ok(v) => Ok(Tok::Num(v)),
            Err(_) => self.err("malformed numeric literal", start),
        }
    }

    /// Scan the raw body of a quoted string (after the opening quote) up to
    /// the matching delimiter. Errors on EOF or raw newline.
    fn read_raw_string(&mut self, delim: u8, start: usize) -> Result<&'a str, LexError> {
        let body_start = self.pos;
        loop {
            match self.peek() {
                None | Some(b'\n') | Some(b'\r') => {
                    return self.err("unterminated string literal", start)
                }
                Some(b'\\') => {
                    self.pos += 2; // skip escaped char (may over-shoot on EOF; caught next loop)
                }
                Some(b) if b == delim => {
                    let raw = &self.src[body_start..self.pos];
                    self.pos += 1;
                    return Ok(raw);
                }
                Some(_) => self.pos += 1,
            }
        }
    }

    /// Scan a regex literal body (already past the opening `/`): `[...]`
    /// classes protect an inner `/`, `\x` escapes protect the next char.
    /// Pattern text is kept raw â€” the regex engine unescapes it, because it
    /// owns the escape sub-language.
    fn read_regex(&mut self, start: usize) -> Result<Tok, LexError> {
        let body_start = self.pos; // after the `/`
        let mut in_class = false;
        loop {
            match self.peek() {
                None | Some(b'\n') | Some(b'\r') => {
                    return self.err("unterminated regex literal", start)
                }
                Some(b'\\') => {
                    if self.peek2().is_none() {
                        return self.err("unterminated regex literal", start);
                    }
                    self.pos += 2;
                }
                Some(b'[') => {
                    in_class = true;
                    self.pos += 1;
                }
                Some(b']') => {
                    in_class = false;
                    self.pos += 1;
                }
                Some(b'/') if !in_class => {
                    let pattern = &self.src[body_start..self.pos];
                    self.pos += 1;
                    let fstart = self.pos;
                    while matches!(self.peek(), Some(b'g' | b'i' | b'm' | b's' | b'y')) {
                        self.pos += 1;
                    }
                    let flags = &self.src[fstart..self.pos];
                    return Ok(Tok::Regex(Rc::from(pattern), Rc::from(flags)));
                }
                Some(_) => self.pos += 1,
            }
        }
    }

    /// Interpret backslash escapes in a raw string body.
    fn unescape(&mut self, raw: &str, start: usize) -> Result<String, LexError> {
        if !raw.contains('\\') {
            return Ok(raw.to_string());
        }
        let bytes = raw.as_bytes();
        let mut out = String::with_capacity(raw.len());
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            if b != b'\\' {
                // copy one whole char (UTF-8 safe)
                let ch_len = (1..=4)
                    .find(|n| i + n <= bytes.len() && raw.is_char_boundary(i + n))
                    .unwrap_or(1);
                out.push_str(&raw[i..i + ch_len]);
                i += ch_len;
                continue;
            }
            i += 1;
            let e = match bytes.get(i) {
                None => return self.err("unterminated escape", start),
                Some(&e) => e,
            };
            i += 1;
            match e {
                b'n' => out.push('\n'),
                b't' => out.push('\t'),
                b'r' => out.push('\r'),
                b'b' => out.push('\u{8}'),
                b'f' => out.push('\u{c}'),
                b'v' => out.push('\u{b}'),
                b'0' => out.push('\0'),
                b'\\' => out.push('\\'),
                b'\'' => out.push('\''),
                b'"' => out.push('"'),
                b'`' => out.push('`'),
                b'\n' => {} // line continuation
                b'\r' => {
                    if bytes.get(i) == Some(&b'\n') {
                        i += 1;
                    }
                }
                b'x' => {
                    let h = raw.get(i..i + 2).ok_or_else(|| LexError {
                        msg: "bad \\x escape".into(),
                        offset: start as u32,
                    })?;
                    i += 2;
                    let v = u8::from_str_radix(h, 16).map_err(|_| LexError {
                        msg: "bad \\x escape".into(),
                        offset: start as u32,
                    })?;
                    out.push(v as char);
                }
                b'u' => {
                    if bytes.get(i) == Some(&b'{') {
                        // \u{...}
                        let close = raw[i..].find('}').ok_or_else(|| LexError {
                            msg: "bad \\u{...} escape".into(),
                            offset: start as u32,
                        })?;
                        let hex = &raw[i + 1..i + close];
                        i += close + 1;
                        let v = u32::from_str_radix(hex, 16).map_err(|_| LexError {
                            msg: "bad \\u{...} escape".into(),
                            offset: start as u32,
                        })?;
                        out.push(char::from_u32(v).unwrap_or('\u{fffd}'));
                    } else {
                        let h = raw.get(i..i + 4).ok_or_else(|| LexError {
                            msg: "bad \\u escape".into(),
                            offset: start as u32,
                        })?;
                        i += 4;
                        let v = u16::from_str_radix(h, 16).map_err(|_| LexError {
                            msg: "bad \\u escape".into(),
                            offset: start as u32,
                        })?;
                        // shortcut: surrogate pairs are not recombined; lone
                        // surrogates become U+FFFD.
                        out.push(char::from_u32(v as u32).unwrap_or('\u{fffd}'));
                    }
                }
                other => {
                    out.push('\\');
                    out.push(other as char);
                }
            }
        }
        Ok(out)
    }

    /// Read a whole backtick template into literal/expr parts.
    fn read_template(&mut self, start: usize) -> Result<Vec<TplPart>, LexError> {
        let mut parts = Vec::new();
        let mut lit = String::new();
        loop {
            match self.peek() {
                None => return self.err("unterminated template literal", start),
                Some(b'`') => {
                    self.pos += 1;
                    parts.push(TplPart::Lit(Rc::from(lit.as_str())));
                    return Ok(parts);
                }
                Some(b'$') if self.peek2() == Some(b'{') => {
                    self.pos += 2;
                    parts.push(TplPart::Lit(Rc::from(lit.as_str())));
                    lit.clear();
                    let toks = self.read_tokens_until_brace(start)?;
                    parts.push(TplPart::Expr(toks));
                }
                Some(b'\\') => {
                    // find raw run up to next escape/delimiter/interp, unescape it
                    let raw_start = self.pos;
                    while let Some(b) = self.peek() {
                        if b == b'\\' || b == b'`' || b == b'$' {
                            break;
                        }
                        self.pos += 1;
                    }
                    let raw = &self.src[raw_start..self.pos];
                    lit.push_str(&self.unescape(raw, start)?);
                    if self.peek() == Some(b'\\') {
                        let raw_start = self.pos;
                        self.pos += 2;
                        let raw = &self.src[raw_start..self.pos];
                        lit.push_str(&self.unescape(raw, start)?);
                    }
                }
                Some(_) => {
                    let raw_start = self.pos;
                    while let Some(b) = self.peek() {
                        if b == b'`' || b == b'\\' || b == b'$' {
                            break;
                        }
                        self.pos += 1;
                    }
                    lit.push_str(&self.src[raw_start..self.pos]);
                }
            }
        }
    }

    /// Lex tokens for a `${ ... }` interpolation: normal lexing with brace
    /// depth tracking; stops (without consuming) at the matching `}`.
    fn read_tokens_until_brace(&mut self, start: usize) -> Result<Vec<Token>, LexError> {
        let mut out = Vec::new();
        let mut depth = 0usize;
        loop {
            let t = self.next_token(regex_allowed(out.last().map(|t: &Token| &t.tok)))?;
            match &t.tok {
                Tok::Punct(Punct::LBrace) => depth += 1,
                Tok::Punct(Punct::RBrace) => {
                    if depth == 0 {
                        return Ok(out);
                    }
                    depth -= 1;
                }
                Tok::Eof => return self.err("unterminated ${...} in template literal", start),
                _ => {}
            }
            out.push(t);
        }
    }

    fn read_punct(&mut self, start: usize, b: u8) -> Result<Tok, LexError> {
        macro_rules! one {
            ($p:expr) => {{
                self.pos += 1;
                return Ok(Tok::Punct($p));
            }};
        }
        macro_rules! op {
            ($o:expr) => {{
                self.pos += 1;
                return Ok(Tok::Op($o));
            }};
        }
        macro_rules! op2 {
            ($o:expr) => {{
                self.pos += 2;
                return Ok(Tok::Op($o));
            }};
        }
        match b {
            b'(' => one!(Punct::LParen),
            b')' => one!(Punct::RParen),
            b'{' => one!(Punct::LBrace),
            b'}' => one!(Punct::RBrace),
            b'[' => one!(Punct::LBrack),
            b']' => one!(Punct::RBrack),
            b';' => one!(Punct::Semi),
            b',' => one!(Punct::Comma),
            b':' => one!(Punct::Colon),
            b'?' => {
                if self.peek2() == Some(b'?') {
                    op2!(Op::Coalesce)
                }
                one!(Punct::Question)
            }
            b'=' => {
                if self.peek2() == Some(b'=') {
                    self.pos += 2;
                    if self.peek() == Some(b'=') {
                        op!(Op::StrictEq)
                    }
                    return Ok(Tok::Op(Op::Eq));
                }
                if self.peek2() == Some(b'>') {
                    op2!(Op::Arrow)
                }
                op!(Op::Assign)
            }
            b'!' => {
                if self.peek2() == Some(b'=') {
                    self.pos += 2;
                    if self.peek() == Some(b'=') {
                        op!(Op::StrictNe)
                    }
                    return Ok(Tok::Op(Op::Ne));
                }
                op!(Op::Not)
            }
            b'<' => {
                if self.peek2() == Some(b'<') {
                    // <<, <<=
                    if self.src.as_bytes().get(self.pos + 2) == Some(&b'=') {
                        self.pos += 3;
                        return Ok(Tok::Op(Op::ShlAssign));
                    }
                    self.pos += 2;
                    return Ok(Tok::Op(Op::Shl));
                }
                if self.peek2() == Some(b'=') {
                    op2!(Op::Le)
                }
                op!(Op::Lt)
            }
            b'>' => {
                if self.peek2() == Some(b'>') {
                    // >>>, >>>=, >>, >>=
                    if self.src.as_bytes().get(self.pos + 2) == Some(&b'>') {
                        if self.src.as_bytes().get(self.pos + 3) == Some(&b'=') {
                            self.pos += 4;
                            return Ok(Tok::Op(Op::UShrAssign));
                        }
                        self.pos += 3;
                        return Ok(Tok::Op(Op::UShr));
                    }
                    if self.src.as_bytes().get(self.pos + 2) == Some(&b'=') {
                        self.pos += 3;
                        return Ok(Tok::Op(Op::ShrAssign));
                    }
                    self.pos += 2;
                    return Ok(Tok::Op(Op::Shr));
                }
                if self.peek2() == Some(b'=') {
                    op2!(Op::Ge)
                }
                op!(Op::Gt)
            }
            b'+' => {
                if self.peek2() == Some(b'+') {
                    op2!(Op::Inc)
                }
                if self.peek2() == Some(b'=') {
                    op2!(Op::AddAssign)
                }
                op!(Op::Add)
            }
            b'-' => {
                if self.peek2() == Some(b'-') {
                    op2!(Op::Dec)
                }
                if self.peek2() == Some(b'=') {
                    op2!(Op::SubAssign)
                }
                op!(Op::Sub)
            }
            b'*' => {
                if self.peek2() == Some(b'*') {
                    return self.err("`**` exponent operator is not supported (later wave)", start);
                }
                if self.peek2() == Some(b'=') {
                    op2!(Op::MulAssign)
                }
                op!(Op::Mul)
            }
            b'/' => {
                if self.peek2() == Some(b'=') {
                    op2!(Op::DivAssign)
                }
                op!(Op::Div)
            }
            b'%' => {
                if self.peek2() == Some(b'=') {
                    op2!(Op::ModAssign)
                }
                op!(Op::Mod)
            }
            b'&' => {
                if self.peek2() == Some(b'&') {
                    op2!(Op::AndAnd)
                }
                if self.peek2() == Some(b'=') {
                    op2!(Op::AndAssign)
                }
                op!(Op::And)
            }
            b'|' => {
                if self.peek2() == Some(b'|') {
                    op2!(Op::OrOr)
                }
                if self.peek2() == Some(b'=') {
                    op2!(Op::OrAssign)
                }
                op!(Op::Or)
            }
            b'^' => {
                if self.peek2() == Some(b'=') {
                    op2!(Op::XorAssign)
                }
                op!(Op::Xor)
            }
            b'~' => op!(Op::BitNot),
            b'.' => {
                if self.peek2() == Some(b'.') && self.src.as_bytes().get(self.pos + 2) == Some(&b'.') {
                    self.pos += 3;
                    return Ok(Tok::Spread);
                }
                one!(Punct::Dot)
            }
            _ => {
                let ch_len = (1..=4)
                    .find(|n| self.pos + n <= self.src.len() && self.src.is_char_boundary(self.pos + n))
                    .unwrap_or(1);
                let ch = &self.src[self.pos..self.pos + ch_len];
                self.pos += ch_len;
                self.err(&format!("unexpected character `{}` in source", ch), start)
            }
        }
    }
}
