//! Hand-rolled lexer for the JS subset (see js.rs module docs for scope).
//!
//! Tokens carry byte spans (`Span`) so parser errors can report line/col.
//!
//! shortcuts (v1, all honest):
//! - regex literal vs division is decided with the standard lexical
//!   previous-token heuristic (a `/` after a value-ish token — number, str,
//!   template, identifier, `)`, `]`, prefix/postfix `++`/`--` — reads as
//!   division; anything else starts a regex literal). This cannot see parser
//!   state, so pathological spacing like `var r = {a:1} /2` (object literal
//!   then division) would mislex as a regex — such shapes are vanishingly
//!   rare in real code and produce a regex-parse error, not silent misbehavior.
//! - identifiers are `A-Za-z_$` + digits plus any non-ASCII byte (UTF-8 is
//!   consumed whole), with `\uXXXX` / `\u{...}` escapes decoded. This is
//!   deliberately looser than Unicode ID_Start/ID_Continue (symbols pass too);
//!   it keeps real bundles lexing without a Unicode tables dependency.
//! - keywords are strict (no contextual keywords): `let`/`of`/`in` etc. can
//!   never be used as identifiers.
//! - BigInt literals (`123n`) are a syntax error, not silently misparsed.
//! - `**` is exponentiation (right-associative, `**=` too). `-2 ** 2`
//!   parses as `(-2) ** 2` where the spec makes it a SyntaxError: the
//!   parser does not track parenthesis level, so it cannot tell the two
//!   apart — real code writes `-(2 ** 2)` or `0 - 2 ** 2` instead.

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
    /// integer literal with the `n` suffix (`123n`), stored as normalized
    /// decimal digits so the parser does not need the source slice
    BigInt(Rc<str>),
    /// regex literal `/pattern/flags` — pattern and flags kept raw (escapes
    /// unescaped later by the regex engine, which owns them)
    Regex(Rc<str>, Rc<str>),
    /// `...` — spread in calls/arrays/objects, rest in params/patterns
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
    With,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Op {
    Assign,
    AddAssign,
    SubAssign,
    MulAssign,
    DivAssign,
    ModAssign,
    PowAssign,
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
    Pow,
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
    /// `&&=`, `||=`, `??=` (short-circuit assignment; the value is only
    /// evaluated when the operator actually writes)
    LogicalAndAssign,
    LogicalOrAssign,
    CoalesceAssign,
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
/// after `}` we choose regex — statement boundaries are the common real-world
/// reading.
fn regex_allowed(prev: Option<&Tok>) -> bool {
    !matches!(
        prev,
        Some(Tok::Num(_))
            | Some(Tok::BigInt(_))
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
        // `of` is contextual: a plain identifier everywhere except the head of
        // a `for (... of ...)`. Bundles use it as a variable (`var of = ...`).
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
        "with" => Kw::With,
        // contextual keywords promoted to reserved (matches the strict-keyword
        // convention of the rest of this lexer): `async`, `await`, `yield`
        "async" => Kw::Async,
        "await" => Kw::Await,
        "yield" => Kw::Yield,
        _ => return None,
    })
}

/// Identifier characters are read as raw bytes: every non-ASCII byte is
/// accepted, so a UTF-8 identifier (`Ä`, `é`, …) is consumed whole and the
/// slice stays on a char boundary (a multibyte sequence contains no ASCII
/// byte). This is deliberately looser than Unicode ID_Start/ID_Continue —
/// shortcut: emoji and symbols also pass — which costs nothing at runtime
/// and keeps the lexer allocation-free.
fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b == b'$' || b >= 0x80
}

fn is_ident_char(b: u8) -> bool {
    is_ident_start(b) || b.is_ascii_digit()
}

/// Identifier code points that arrive through a `\u` escape. Mirrors the byte
/// rule above: ASCII letters/`_`/`$` at the start, digits after, and any
/// non-ASCII non-space (the spec is stricter — ID_Start/ID_Continue — but the
/// loose rule costs nothing here and needs no Unicode tables).
fn is_ident_start_char(c: char) -> bool {
    c.is_ascii_alphabetic()
        || c == '_'
        || c == '$'
        || (!c.is_ascii() && !c.is_whitespace() && !c.is_control())
}

fn is_ident_char_char(c: char) -> bool {
    is_ident_start_char(c) || c.is_ascii_digit()
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
            // HashbangComment: `#!...` is a comment only at the very start of
            // the source (node-style launchers; test262 `hashbang` cases).
            if self.pos == 0 && self.peek() == Some(b'#') && self.peek2() == Some(b'!') {
                while let Some(b) = self.peek() {
                    if b == b'\n' || b == b'\r' {
                        break;
                    }
                    self.pos += 1;
                }
                continue;
            }
            match self.peek() {
                Some(b'\n') | Some(b'\r') => {
                    nl = true;
                    self.pos += 1;
                }
                Some(b' ') | Some(b'\t') | Some(0x0b) | Some(0x0c) => {
                    self.pos += 1
                }
                // Non-ASCII white space (NBSP, ZWNBSP/BOM, the Zs block) and
                // the U+2028/U+2029 line terminators. Any other non-ASCII
                // byte starts an identifier, so leave it to `next_token`.
                Some(b) if b >= 0x80 => {
                    let ch = self.src[self.pos..].chars().next().unwrap_or('\u{fffd}');
                    if ch == '\u{2028}' || ch == '\u{2029}' {
                        nl = true;
                        self.pos += ch.len_utf8();
                    } else if ch == '\u{feff}' || (ch.is_whitespace() && !ch.is_ascii()) {
                        self.pos += ch.len_utf8();
                    } else {
                        return nl;
                    }
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
            self.read_ident()?
        } else if b == b'\\' && self.peek2() == Some(b'u') {
            // `\uXXXX` / `\u{...}` at an identifier position: bundles write
            // property names like `v.id.\u0275prov` (Angular's ɵ prefix)
            self.read_ident()?
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

    /// Read an identifier, decoding `\uXXXX` / `\u{...}` escapes. A name that
    /// used an escape is never a keyword: reserved words match on literal
    /// source characters, so `\u0069f` is an identifier, not `if`.
    fn read_ident(&mut self) -> Result<Tok, LexError> {
        let (name, escaped) = self.read_ident_name()?;
        if !escaped {
            if let Some(k) = kw_of(&name) {
                return Ok(Tok::Kw(k));
            }
        }
        Ok(Tok::Ident(Rc::from(name.as_str())))
    }

    /// The decoded text of an identifier (escapes resolved) plus whether an
    /// escape was used (an escaped name never matches a keyword). Split out so
    /// a private name (`#ident`) can reuse it.
    fn read_ident_name(&mut self) -> Result<(String, bool), LexError> {
        let mut name = String::new();
        let mut escaped = false;
        let mut first = true;
        loop {
            let b = match self.peek() {
                Some(b) => b,
                None => break,
            };
            if b == b'\\' {
                if self.peek2() != Some(b'u') {
                    break;
                }
                let (c, n) = self.ident_escape(self.pos).ok_or_else(|| LexError {
                    msg: "bad identifier escape".into(),
                    offset: self.pos as u32,
                })?;
                let ok = if first {
                    is_ident_start_char(c)
                } else {
                    is_ident_char_char(c)
                };
                if !ok {
                    return self.err("invalid identifier escape", self.pos);
                }
                name.push(c);
                escaped = true;
                self.pos += n;
            } else if if first { is_ident_start(b) } else { is_ident_char(b) } {
                // copy one whole char: the byte rule accepts any byte >= 0x80,
                // so a UTF-8 identifier must advance by its full width
                let n = (1..=4)
                    .find(|n| self.src.is_char_boundary(self.pos + n))
                    .unwrap_or(1);
                name.push_str(&self.src[self.pos..self.pos + n]);
                self.pos += n;
            } else {
                break;
            }
            first = false;
        }
        Ok((name, escaped))
    }

    /// Decode a `\uXXXX` or `\u{...}` escape at `at`; returns the code point
    /// and the number of source bytes it occupies.
    fn ident_escape(&self, at: usize) -> Option<(char, usize)> {
        let rest = self.src.get(at + 2..)?;
        if let Some(body) = rest.strip_prefix('{') {
            let close = body.find('}')?;
            let v = u32::from_str_radix(&body[..close], 16).ok()?;
            Some((char::from_u32(v)?, 4 + close))
        } else {
            let h = rest.get(..4)?;
            let v = u16::from_str_radix(h, 16).ok()?;
            Some((char::from_u32(v as u32)?, 6))
        }
    }

    /// Consume digits of one radix, allowing `_` separators only between two
    /// digits of that radix (`1_000`, `0x1_0`) when `allow_sep` is set.
    fn scan_digits(
        &mut self,
        ok: impl Fn(u8) -> bool,
        allow_sep: bool,
        start: usize,
    ) -> Result<(), LexError> {
        let mut last_digit = false;
        while let Some(c) = self.peek() {
            if ok(c) {
                last_digit = true;
                self.pos += 1;
            } else if c == b'_' {
                if !allow_sep || !last_digit || !matches!(self.peek2(), Some(n) if ok(n)) {
                    return self.err("invalid numeric separator", start);
                }
                last_digit = false;
                self.pos += 1;
            } else {
                break;
            }
        }
        Ok(())
    }

    fn digits_text(&self, start: usize) -> String {
        self.src[start..self.pos].chars().filter(|c| *c != '_').collect()
    }

    /// A numeric literal may not be immediately followed by an identifier
    /// start or a digit (`3in []`, `1foo`, `0x1g` are SyntaxErrors).
    fn bad_number_suffix(&self) -> bool {
        match self.peek() {
            Some(b) if b.is_ascii_alphabetic() || b == b'_' || b == b'$' || b.is_ascii_digit() => {
                true
            }
            Some(b) if b >= 0x80 => {
                let ch = self.src[self.pos..].chars().next().unwrap_or('\u{fffd}');
                is_ident_start_char(ch)
            }
            _ => false,
        }
    }

    fn read_number(&mut self, start: usize) -> Result<Tok, LexError> {
        let b0 = self.peek();
        if b0 == Some(b'0') {
            if let Some(x) = self.peek2() {
                let (radix, ok): (u32, fn(u8) -> bool) = match x {
                    b'x' | b'X' => (16, |c| c.is_ascii_hexdigit()),
                    b'b' | b'B' => (2, |c| c == b'0' || c == b'1'),
                    b'o' | b'O' => (8, |c| (b'0'..=b'7').contains(&c)),
                    _ => (0, |_| false),
                };
                if radix != 0 {
                    self.pos += 2;
                    let dstart = self.pos;
                    self.scan_digits(ok, true, start)?;
                    let digits = self.digits_text(dstart);
                    if self.peek() == Some(b'n') {
                        self.pos += 1;
                        if self.bad_number_suffix() {
                            return self.err("identifier directly after number", start);
                        }
                        return match i128::from_str_radix(&digits, radix) {
                            Ok(v) => Ok(Tok::BigInt(Rc::from(v.to_string().as_str()))),
                            Err(_) => self.err("BigInt literal too large", start),
                        };
                    }
                    if self.bad_number_suffix() {
                        return self.err("identifier directly after number", start);
                    }
                    return match u64::from_str_radix(&digits, radix) {
                        Ok(v) => Ok(Tok::Num(v as f64)),
                        Err(_) => self.err("malformed numeric literal", start),
                    };
                }
            }
        }
        // decimal: digits [. digits] [eE [+-] digits]
        // A leading zero followed by another digit (or `_`) is a legacy
        // octal / non-octal decimal form, where separators are not allowed.
        let allow_sep = !(b0 == Some(b'0')
            && matches!(self.peek2(), Some(c) if c.is_ascii_digit() || c == b'_'));
        self.scan_digits(|c| c.is_ascii_digit(), allow_sep, start)?;
        let mut had_dot = false;
        if self.peek() == Some(b'.') {
            had_dot = true;
            self.pos += 1;
            self.scan_digits(|c| c.is_ascii_digit(), allow_sep, start)?;
        }
        let mut had_exp = false;
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            let save = self.pos;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.pos += 1;
            }
            if matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                had_exp = true;
                self.scan_digits(|c| c.is_ascii_digit(), true, start)?;
            } else {
                self.pos = save; // `1e` is just `1` followed by ident `e`
            }
        }
        if self.peek() == Some(b'n') {
            // `123n` is a BigInt literal; a fraction or exponent before the
            // suffix is a syntax error (`1.5n`, `1e3n`)
            let raw = &self.src[start..self.pos];
            if had_dot || had_exp {
                return self.err("invalid BigInt literal", start);
            }
            self.pos += 1; // `n`
            if self.bad_number_suffix() {
                return self.err("identifier directly after number", start);
            }
            let text = raw.chars().filter(|c| *c != '_').collect::<String>();
            // a decimal BigInt may not use a leading zero (`00n`, `08n`):
            // those are legacy-octal/non-octal forms, which have no BigInt
            if text.len() > 1 && text.starts_with('0') {
                return self.err("invalid BigInt literal", start);
            }
            return match text.parse::<i128>() {
                Ok(v) => Ok(Tok::BigInt(Rc::from(v.to_string().as_str()))),
                Err(_) => self.err("BigInt literal too large", start),
            };
        }
        if self.bad_number_suffix() {
            return self.err("identifier directly after number", start);
        }
        let s = self.digits_text(start);
        // Annex B legacy octal: `070` is 56 in sloppy mode (the `0` prefix
        // followed only by octal digits). `08`/`09` stay decimal.
        if !had_dot
            && !had_exp
            && b0 == Some(b'0')
            && self.pos - start > 1
            && s.bytes().all(|c| (b'0'..=b'7').contains(&c))
        {
            if let Ok(v) = u64::from_str_radix(&s, 8) {
                return Ok(Tok::Num(v as f64));
            }
        }
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
                    // skip the backslash and the escaped char; a CRLF line
                    // continuation is two bytes and must be skipped whole
                    self.pos += 1;
                    if self.peek() == Some(b'\r')
                        && self.src.as_bytes().get(self.pos + 1) == Some(&b'\n')
                    {
                        self.pos += 2;
                    } else {
                        self.pos += 1; // may over-shoot on EOF; caught next loop
                    }
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
    /// Pattern text is kept raw — the regex engine unescapes it, because it
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
                    // `d` (indices), `u` (unicode) and `v` (unicode sets) are
                    // accepted; the regex engine treats them as no-ops for
                    // pattern matching (see js/regex.rs).
                    while matches!(
                        self.peek(),
                        Some(b'd' | b'g' | b'i' | b'm' | b's' | b'u' | b'v' | b'y')
                    ) {
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
                    if other >= 0x80 {
                        // Identity escape of a non-ASCII character: drop the
                        // backslash and copy the whole char. U+2028/U+2029 are
                        // line continuations, so they contribute nothing.
                        let at = i - 1;
                        let ch = raw[at..].chars().next().unwrap_or('\u{fffd}');
                        if ch != '\u{2028}' && ch != '\u{2029}' {
                            out.push(ch);
                        }
                        i = at + ch.len_utf8();
                    } else {
                        out.push('\\');
                        out.push(other as char);
                    }
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
                // a literal `$` NOT starting an interpolation (`$${x}`,
                // `$5`, `$ `): consume it as literal text. Without this arm
                // it fell into the raw-run arm below, which breaks AT the
                // `$` without advancing -> infinite loop (hung a real page
                // whose template was `` `$${r}` ``)
                Some(b'$') => {
                    lit.push('$');
                    self.pos += 1;
                }
                Some(b'\\') => {
                    // consume exactly one escape sequence, then decode it.
                    // (Previously this took a fixed 2 bytes, so `\xHH` /
                    // `\uHHHH` in a template were cut in half and reported
                    // as a bogus "bad \x escape" at the template start.)
                    let raw_start = self.pos;
                    self.pos += 1; // `\`
                    match self.peek() {
                        Some(b'x') => self.pos += 3, // \xHH
                        Some(b'u') => {
                            if self.peek2() == Some(b'{') {
                                self.pos += 2; // `u{`
                                while let Some(b) = self.peek() {
                                    if b == b'`' || b == b'$' {
                                        break;
                                    }
                                    self.pos += 1;
                                    if b == b'}' {
                                        break;
                                    }
                                }
                            } else {
                                self.pos += 5; // \uHHHH
                            }
                        }
                        Some(b'\r') => {
                            self.pos += 1;
                            if self.peek() == Some(b'\n') {
                                self.pos += 1;
                            }
                        }
                        Some(_) => {
                            // The escaped character may be multi-byte (e.g.
                            // `\` + U+2028 line separator). Advancing one byte
                            // would slice inside the char and panic.
                            let n = self.src[self.pos..]
                                .chars()
                                .next()
                                .map(|c| c.len_utf8())
                                .unwrap_or(1);
                            self.pos += n;
                        }
                        None => {}
                    }
                    let end = self.pos.min(self.src.len());
                    let Some(raw) = self.src.get(raw_start..end) else {
                        return self.err(
                            "malformed escape sequence in template literal",
                            raw_start,
                        );
                    };
                    lit.push_str(&self.unescape(raw, start)?);
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
                    if self.src.as_bytes().get(self.pos + 2) == Some(&b'=') {
                        self.pos += 3;
                        return Ok(Tok::Op(Op::CoalesceAssign));
                    }
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
                    // `**` / `**=` (right-associative exponentiation)
                    if self.src.as_bytes().get(self.pos + 2) == Some(&b'=') {
                        self.pos += 3;
                        return Ok(Tok::Op(Op::PowAssign));
                    }
                    op2!(Op::Pow)
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
                    if self.src.as_bytes().get(self.pos + 2) == Some(&b'=') {
                        self.pos += 3;
                        return Ok(Tok::Op(Op::LogicalAndAssign));
                    }
                    op2!(Op::AndAnd)
                }
                if self.peek2() == Some(b'=') {
                    op2!(Op::AndAssign)
                }
                op!(Op::And)
            }
            b'|' => {
                if self.peek2() == Some(b'|') {
                    if self.src.as_bytes().get(self.pos + 2) == Some(&b'=') {
                        self.pos += 3;
                        return Ok(Tok::Op(Op::LogicalOrAssign));
                    }
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
            // private class name: `#field` / `#method`. Kept as an ordinary
            // identifier with a `#` prefix so member access, class members
            // and `this.#x` reuse the normal property machinery. The name may
            // use `\u` escapes (`#\u{6F}_`), so decode it like an identifier.
            // shortcut: private names are per-property, not per-class, so
            // same-named privates of a base and a derived class collide.
            b'#' if matches!(self.peek2(), Some(c) if is_ident_start(c) || c == b'\\') => {
                self.pos += 1; // `#`
                let (name, _) = self.read_ident_name()?;
                return Ok(Tok::Ident(Rc::from(format!("#{name}").as_str())));
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
