//! The linlua lexer: source text to tokens with absolute byte spans.
//!
//! Comments (`--` line, `--[[ ]]` block) are skipped but their spans
//! are recorded — the memory annotations (`-- @own`, `-- @ref`) are
//! recovered from these, exactly like linjs.

/// A token kind. Keyword and punctuation kinds are exact; literals
/// carry their parsed value.
#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    // literals
    Num(NumLit),
    Str(String),
    Name(String),
    // keywords
    And,
    Break,
    Do,
    Else,
    Elseif,
    End,
    False,
    For,
    Function,
    Goto,
    If,
    In,
    Local,
    Nil,
    Not,
    Or,
    Repeat,
    Return,
    Then,
    True,
    Until,
    While,
    // punctuation and operators
    Plus,
    Minus,
    Star,
    Slash,
    DSlash,
    Percent,
    Caret,
    Hash,
    Amp,
    Tilde, // binary xor only; unary not is the `not` keyword
    Pipe,
    Shl,
    Shr,
    Eq,
    NotEq,
    Le,
    Ge,
    Lt,
    Gt,
    Assign,
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Semi,
    Colon,
    DColon,
    Comma,
    Dot,
    DDot, // concat
}

/// A Lua numeric literal keeps its subtype: `1` is an integer, `1.0`
/// is a float. Lua 5.4's arithmetic and printing depend on the
/// distinction, and so does ours.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NumLit {
    Int(i64),
    Float(f64),
}

/// A token: kind plus absolute byte span in the source.
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub tok: Tok,
    pub start: usize,
    pub end: usize,
}

/// A lex error: message plus absolute byte offset.
#[derive(Debug, Clone, PartialEq)]
pub struct LexError {
    pub offset: usize,
    pub message: String,
}

pub struct Lexer<'s> {
    src: &'s [u8],
    pos: usize,
    /// Spans of comments seen so far, in order.
    pub comments: Vec<(usize, usize)>,
}

impl<'s> Lexer<'s> {
    pub fn new(src: &'s str) -> Self {
        Lexer {
            src: src.as_bytes(),
            pos: 0,
            comments: Vec::new(),
        }
    }

    /// Lexes the whole source. Stops at the first error — the parser
    /// reports it with position and recovers upstream.
    pub fn tokenize(mut self) -> Result<Vec<Token>, LexError> {
        let mut out = Vec::new();
        loop {
            self.skip_trivia()?;
            if self.pos >= self.src.len() {
                return Ok(out);
            }
            let start = self.pos;
            let tok = self.next_token()?;
            out.push(Token {
                tok,
                start,
                end: self.pos,
            });
        }
    }

    fn skip_trivia(&mut self) -> Result<(), LexError> {
        loop {
            match self.src.get(self.pos) {
                Some(b' ') | Some(b'\t') | Some(b'\r') | Some(b'\n') => self.pos += 1,
                Some(b'-') if self.peek(1) == Some(&b'-') => {
                    let start = self.pos;
                    self.pos += 2;
                    // Long comment --[[ ... ]] or --[==[ ... ]==]
                    if self.peek(0) == Some(&b'[') {
                        if let Some(len) = self.long_bracket_level() {
                            self.skip_long_bracket(len)?;
                            self.comments.push((start, self.pos));
                            continue;
                        }
                    }
                    // Line comment: to end of line.
                    while self.pos < self.src.len() && self.src[self.pos] != b'\n' {
                        self.pos += 1;
                    }
                    self.comments.push((start, self.pos));
                }
                _ => return Ok(()),
            }
        }
    }

    /// At `self.pos == b'['`: returns the long-bracket level (count of
    /// `=` between the brackets) if this opens a long bracket.
    fn long_bracket_level(&self) -> Option<usize> {
        if self.peek(0) != Some(&b'[') {
            return None;
        }
        let mut level = 0;
        let mut i = 1;
        while self.peek(i) == Some(&b'=') {
            level += 1;
            i += 1;
        }
        if self.peek(i) == Some(&b'[') {
            Some(level)
        } else {
            None
        }
    }

    /// Skips a long bracket body opened at the current `level`.
    fn skip_long_bracket(&mut self, level: usize) -> Result<(), LexError> {
        self.pos += level + 2; // past [=*[ [
                               // A newline immediately after the opening bracket is not part
                               // of the content.
        if self.src.get(self.pos) == Some(&b'\r') {
            self.pos += 1;
        }
        if self.src.get(self.pos) == Some(&b'\n') {
            self.pos += 1;
        }
        loop {
            if self.pos >= self.src.len() {
                return Err(self.err("unterminated long bracket"));
            }
            if self.src[self.pos] == b']' {
                let mut i = 1;
                while self.peek(i) == Some(&b'=') {
                    i += 1;
                }
                if self.peek(i) == Some(&b']') && i == level + 1 {
                    self.pos += i + 1;
                    return Ok(());
                }
            }
            self.pos += 1;
        }
    }

    fn next_token(&mut self) -> Result<Tok, LexError> {
        let c = self.src[self.pos];
        match c {
            b'0'..=b'9' => self.lex_number(),
            b'\"' | b'\'' => self.lex_quoted_string(),
            b'[' => {
                if let Some(level) = self.long_bracket_level() {
                    self.pos += level + 2;
                    let mut content = String::new();
                    // A newline immediately after the opening bracket
                    // is not part of the content.
                    if self.src.get(self.pos) == Some(&b'\r') {
                        self.pos += 1;
                    }
                    if self.src.get(self.pos) == Some(&b'\n') {
                        self.pos += 1;
                    }
                    loop {
                        if self.pos >= self.src.len() {
                            return Err(self.err("unterminated long string"));
                        }
                        if self.src[self.pos] == b']' {
                            let mut i = 1;
                            while self.peek(i) == Some(&b'=') {
                                i += 1;
                            }
                            if self.peek(i) == Some(&b']') && i == level + 1 {
                                self.pos += i + 1;
                                return Ok(Tok::Str(content));
                            }
                        }
                        content.push(self.src[self.pos] as char);
                        self.pos += 1;
                    }
                } else {
                    self.pos += 1;
                    Ok(Tok::LBracket)
                }
            }
            _ => self.lex_name_or_op(),
        }
    }

    fn lex_number(&mut self) -> Result<Tok, LexError> {
        let start = self.pos;
        let mut is_float = false;

        // Hex literal: 0x / 0X, digits and hex digits, optional
        // fraction and binary exponent (p/P).
        if self.src[self.pos] == b'0' && matches!(self.peek(1), Some(b'x') | Some(b'X')) {
            self.pos += 2;
            let hex_start = self.pos;
            while matches!(
                self.peek(0),
                Some(b'0'..=b'9') | Some(b'a'..=b'f') | Some(b'A'..=b'F')
            ) {
                self.pos += 1;
            }
            if self.peek(0) == Some(&b'.') {
                is_float = true;
                self.pos += 1;
                while matches!(
                    self.peek(0),
                    Some(b'0'..=b'9') | Some(b'a'..=b'f') | Some(b'A'..=b'F')
                ) {
                    self.pos += 1;
                }
            }
            if matches!(self.peek(0), Some(b'p') | Some(b'P')) {
                is_float = true;
                self.pos += 1;
                if matches!(self.peek(0), Some(b'+') | Some(b'-')) {
                    self.pos += 1;
                }
                while matches!(self.peek(0), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            let text = std::str::from_utf8(&self.src[start..self.pos])
                .unwrap()
                .replace('_', "");
            if self.pos == hex_start {
                return Err(self.err("malformed hex number"));
            }
            // Lua: hex without fraction/exponent is an integer, and
            // hex literals wrap at 64 bits (0x8000000000000000 is
            // math.mininteger, not a float).
            if !is_float {
                if let Ok(v) = i64::from_str_radix(&text[2..], 16) {
                    return Ok(Tok::Num(NumLit::Int(v)));
                }
                if let Ok(v) = u64::from_str_radix(&text[2..], 16) {
                    return Ok(Tok::Num(NumLit::Int(v as i64)));
                }
            }
            let f = parse_hex_float(&text).ok_or_else(|| self.err("malformed hex number"))?;
            return Ok(Tok::Num(NumLit::Float(f)));
        }

        while matches!(self.peek(0), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        if self.peek(0) == Some(&b'.') {
            is_float = true;
            self.pos += 1;
            while matches!(self.peek(0), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(0), Some(b'e') | Some(b'E')) {
            let save = self.pos;
            self.pos += 1;
            if matches!(self.peek(0), Some(b'+') | Some(b'-')) {
                self.pos += 1;
            }
            if matches!(self.peek(0), Some(b'0'..=b'9')) {
                is_float = true;
                while matches!(self.peek(0), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            } else {
                self.pos = save; // not an exponent; `e` ends the number
            }
        }
        let text = std::str::from_utf8(&self.src[start..self.pos]).unwrap();
        if is_float {
            let f: f64 = text.parse().map_err(|_| self.err("malformed number"))?;
            Ok(Tok::Num(NumLit::Float(f)))
        } else {
            match text.parse::<i64>() {
                Ok(v) => Ok(Tok::Num(NumLit::Int(v))),
                // Integer overflow promotes to float, like Lua.
                Err(_) => {
                    let f: f64 = text.parse().map_err(|_| self.err("malformed number"))?;
                    Ok(Tok::Num(NumLit::Float(f)))
                }
            }
        }
    }

    fn lex_quoted_string(&mut self) -> Result<Tok, LexError> {
        let quote = self.src[self.pos];
        self.pos += 1;
        let mut out = String::new();
        loop {
            match self.src.get(self.pos) {
                None => return Err(self.err("unterminated string")),
                Some(&b'\n') => return Err(self.err("unterminated string")),
                Some(&c) if c == quote => {
                    self.pos += 1;
                    return Ok(Tok::Str(out));
                }
                Some(&b'\\') => {
                    self.pos += 1;
                    self.lex_escape(&mut out)?;
                }
                Some(&c) => {
                    // Lua strings are bytes; copy through as Latin-1
                    // into the Rust string — the v1 dialect keeps
                    // source text ASCII-safe.
                    out.push(c as char);
                    self.pos += 1;
                }
            }
        }
    }

    fn lex_escape(&mut self, out: &mut String) -> Result<(), LexError> {
        let c = *self
            .src
            .get(self.pos)
            .ok_or_else(|| self.err("unterminated escape"))?;
        self.pos += 1;
        match c {
            b'a' => out.push('\u{7}'),
            b'b' => out.push('\u{8}'),
            b'f' => out.push('\u{c}'),
            b'n' | b'\n' => out.push('\n'),
            b'r' => out.push('\r'),
            b't' => out.push('\t'),
            b'v' => out.push('\u{b}'),
            b'\\' => out.push('\\'),
            b'\"' => out.push('\"'),
            b'\'' => out.push('\''),
            b'x' => {
                let h1 = self.hex_digit()? as u32;
                let h2 = self.hex_digit()? as u32;
                out.push(char::from_u32(h1 * 16 + h2).unwrap_or('\u{fffd}'));
            }
            b'0'..=b'9' => {
                // Decimal escape: up to three digits.
                let mut v = (c - b'0') as u32;
                for _ in 0..2 {
                    match self.peek(0) {
                        Some(d @ b'0'..=b'9') => {
                            v = v * 10 + (d - b'0') as u32;
                            self.pos += 1;
                        }
                        _ => break,
                    }
                }
                out.push(char::from_u32(v).unwrap_or('\u{fffd}'));
            }
            b'z' => {
                // \z skips following whitespace.
                while matches!(
                    self.peek(0),
                    Some(b' ') | Some(b'\t') | Some(b'\r') | Some(b'\n')
                ) {
                    self.pos += 1;
                }
            }
            b'u' => {
                if self.peek(0) != Some(&b'{') {
                    return Err(self.err("expected { after \\u"));
                }
                self.pos += 1;
                let mut v: u32 = 0;
                while let Some(d) = self.hex_digit_opt() {
                    v = v * 16 + d as u32;
                    self.pos += 1;
                }
                if self.peek(0) != Some(&b'}') {
                    return Err(self.err("missing } after \\u{"));
                }
                self.pos += 1;
                out.push(char::from_u32(v).unwrap_or('\u{fffd}'));
            }
            other => {
                return Err(self.err(&format!("invalid escape \\{}", other as char)));
            }
        }
        Ok(())
    }

    fn hex_digit(&mut self) -> Result<u8, LexError> {
        self.hex_digit_opt()
            .ok_or_else(|| self.err("expected hex digit"))
    }

    fn hex_digit_opt(&self) -> Option<u8> {
        match self.peek(0) {
            Some(d @ b'0'..=b'9') => Some(d - b'0'),
            Some(d @ b'a'..=b'f') => Some(d - b'a' + 10),
            Some(d @ b'A'..=b'F') => Some(d - b'A' + 10),
            _ => None,
        }
    }

    fn lex_name_or_op(&mut self) -> Result<Tok, LexError> {
        let c = self.src[self.pos];
        if c == b'_' || c.is_ascii_alphabetic() {
            let start = self.pos;
            while matches!(self.peek(0), Some(d) if *d == b'_' || d.is_ascii_alphanumeric()) {
                self.pos += 1;
            }
            let text = std::str::from_utf8(&self.src[start..self.pos]).unwrap();
            return Ok(match text {
                "and" => Tok::And,
                "break" => Tok::Break,
                "do" => Tok::Do,
                "else" => Tok::Else,
                "elseif" => Tok::Elseif,
                "end" => Tok::End,
                "false" => Tok::False,
                "for" => Tok::For,
                "function" => Tok::Function,
                "goto" => Tok::Goto,
                "if" => Tok::If,
                "in" => Tok::In,
                "local" => Tok::Local,
                "nil" => Tok::Nil,
                "not" => Tok::Not,
                "or" => Tok::Or,
                "repeat" => Tok::Repeat,
                "return" => Tok::Return,
                "then" => Tok::Then,
                "true" => Tok::True,
                "until" => Tok::Until,
                "while" => Tok::While,
                _ => Tok::Name(text.to_string()),
            });
        }

        // Multi-char operators first.
        match (c, self.peek(1).copied()) {
            (b'/', Some(b'/')) => {
                self.pos += 2;
                return Ok(Tok::DSlash);
            }
            (b'=', Some(b'=')) => {
                self.pos += 2;
                return Ok(Tok::Eq);
            }
            (b'~', Some(b'=')) => {
                self.pos += 2;
                return Ok(Tok::NotEq);
            }
            (b'<', Some(b'=')) => {
                self.pos += 2;
                return Ok(Tok::Le);
            }
            (b'>', Some(b'=')) => {
                self.pos += 2;
                return Ok(Tok::Ge);
            }
            (b'<', Some(b'<')) => {
                self.pos += 2;
                return Ok(Tok::Shl);
            }
            (b'>', Some(b'>')) => {
                self.pos += 2;
                return Ok(Tok::Shr);
            }
            (b'.', Some(b'.')) => {
                self.pos += 2;
                return Ok(Tok::DDot);
            }
            (b':', Some(b':')) => {
                self.pos += 2;
                return Ok(Tok::DColon);
            }
            _ => {}
        }
        self.pos += 1;
        Ok(match c {
            b'+' => Tok::Plus,
            b'-' => Tok::Minus,
            b'*' => Tok::Star,
            b'/' => Tok::Slash,
            b'%' => Tok::Percent,
            b'^' => Tok::Caret,
            b'#' => Tok::Hash,
            b'&' => Tok::Amp,
            b'~' => Tok::Tilde,
            b'|' => Tok::Pipe,
            b'<' => Tok::Lt,
            b'>' => Tok::Gt,
            b'=' => Tok::Assign,
            b'(' => Tok::LParen,
            b')' => Tok::RParen,
            b'{' => Tok::LBrace,
            b'}' => Tok::RBrace,
            b'[' => Tok::LBracket,
            b']' => Tok::RBracket,
            b';' => Tok::Semi,
            b':' => Tok::Colon,
            b',' => Tok::Comma,
            b'.' => Tok::Dot,
            other => {
                return Err(self.err(&format!("unexpected character `{}`", other as char)));
            }
        })
    }

    fn peek(&self, ahead: usize) -> Option<&u8> {
        self.src.get(self.pos + ahead)
    }

    fn err(&self, message: &str) -> LexError {
        LexError {
            offset: self.pos,
            message: message.to_string(),
        }
    }
}

/// Parses Lua's hex float syntax (`0x1p4`, `0xA.8p-2`) — `f64`
/// `FromStr` does not know it.
fn parse_hex_float(text: &str) -> Option<f64> {
    let rest = text.strip_prefix("0x")?;
    let (mantissa, exponent) = match rest.find(['p', 'P']) {
        Some(p) => (&rest[..p], &rest[p + 1..]),
        None => (rest, ""),
    };
    let (int_part, frac_part) = match mantissa.find('.') {
        Some(d) => (&mantissa[..d], &mantissa[d + 1..]),
        None => (mantissa, ""),
    };
    let mut m: f64 = 0.0;
    for c in int_part.chars() {
        m = m * 16.0 + c.to_digit(16)? as f64;
    }
    let mut scale = 1.0 / 16.0;
    for c in frac_part.chars() {
        m += c.to_digit(16)? as f64 * scale;
        scale /= 16.0;
    }
    let exp: i32 = if exponent.is_empty() {
        0
    } else {
        exponent.parse().ok()?
    };
    Some(m * 2f64.powi(exp))
}
