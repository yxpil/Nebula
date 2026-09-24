//! 词法分析:把 SQL 文本切成 token。
//! 关键字大小写不敏感;字符串用单引号,内部 '' 转义。

use nebula_core::{Error, Result};

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    /// 标识符/关键字(保留小写形式)。
    Word(String),
    /// 单引号字符串(已反转义)。
    Str(String),
    Int(i64),
    Float(f32),
    Eq,
    NotEq,
    Lt,
    Gt,
    Le,
    Ge,
    Star,
    Comma,
    LParen,
    RParen,
    Percent,
    Semi,
    Eof,
}

pub struct Lexer<'a> {
    src: &'a [u8],
    pos: usize,
}

impl<'a> Lexer<'a> {
    pub fn new(src: &'a str) -> Self {
        Lexer {
            src: src.as_bytes(),
            pos: 0,
        }
    }

    pub fn tokenize(mut self) -> Result<Vec<Token>> {
        let mut out = Vec::new();
        loop {
            let t = self.next_token()?;
            let is_eof = t == Token::Eof;
            out.push(t);
            if is_eof {
                break;
            }
        }
        Ok(out)
    }

    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.peek();
        if b.is_some() {
            self.pos += 1;
        }
        b
    }

    fn skip_ws(&mut self) {
        while let Some(b) = self.peek() {
            if b == b' ' || b == b'\t' || b == b'\n' || b == b'\r' {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn next_token(&mut self) -> Result<Token> {
        self.skip_ws();
        let Some(b) = self.peek() else {
            return Ok(Token::Eof);
        };
        match b {
            b'\'' => self.read_string(),
            b'0'..=b'9' => self.read_number(),
            b'a'..=b'z' | b'A'..=b'Z' | b'_' => {
                let start = self.pos;
                while let Some(c) = self.peek() {
                    if c.is_ascii_alphanumeric() || c == b'_' {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                let word = String::from_utf8_lossy(&self.src[start..self.pos]).to_lowercase();
                Ok(Token::Word(word))
            }
            b'=' => {
                self.bump();
                Ok(Token::Eq)
            }
            b'!' => {
                self.bump();
                if self.peek() == Some(b'=') {
                    self.bump();
                    Ok(Token::NotEq)
                } else {
                    Err(Error::Sql("unexpected '!'".into()))
                }
            }
            b'<' => {
                self.bump();
                match self.peek() {
                    Some(b'=') => {
                        self.bump();
                        Ok(Token::Le)
                    }
                    Some(b'>') => {
                        self.bump();
                        Ok(Token::NotEq)
                    }
                    _ => Ok(Token::Lt),
                }
            }
            b'>' => {
                self.bump();
                if self.peek() == Some(b'=') {
                    self.bump();
                    Ok(Token::Ge)
                } else {
                    Ok(Token::Gt)
                }
            }
            b'*' => {
                self.bump();
                Ok(Token::Star)
            }
            b',' => {
                self.bump();
                Ok(Token::Comma)
            }
            b'(' => {
                self.bump();
                Ok(Token::LParen)
            }
            b')' => {
                self.bump();
                Ok(Token::RParen)
            }
            b'%' => {
                self.bump();
                Ok(Token::Percent)
            }
            b';' => {
                self.bump();
                Ok(Token::Semi)
            }
            other => Err(Error::Sql(format!(
                "unexpected character '{}' at position {}",
                other as char, self.pos
            ))),
        }
    }

    fn read_string(&mut self) -> Result<Token> {
        self.bump(); // '
        let mut s = String::new();
        loop {
            match self.bump() {
                None => return Err(Error::Sql("unterminated string literal".into())),
                Some(b'\'') => {
                    if self.peek() == Some(b'\'') {
                        self.bump(); // '' -> '
                        s.push('\'');
                    } else {
                        break;
                    }
                }
                Some(b) => {
                    // 支持多字节 UTF-8:逐字节收集后整体解析
                    if b < 0x80 {
                        s.push(b as char);
                    } else {
                        let start = self.pos - 1;
                        while let Some(c) = self.peek() {
                            if c & 0xC0 == 0x80 {
                                self.pos += 1;
                            } else {
                                break;
                            }
                        }
                        let chunk = &self.src[start..self.pos];
                        match std::str::from_utf8(chunk) {
                            Ok(t) => s.push_str(t),
                            Err(_) => return Err(Error::Sql("invalid utf-8 in string".into())),
                        }
                    }
                }
            }
        }
        Ok(Token::Str(s))
    }

    fn read_number(&mut self) -> Result<Token> {
        let start = self.pos;
        let mut is_float = false;
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                self.pos += 1;
            } else if c == b'.' && !is_float {
                is_float = true;
                self.pos += 1;
            } else {
                break;
            }
        }
        let text = String::from_utf8_lossy(&self.src[start..self.pos]).to_string();
        if is_float {
            text.parse::<f32>()
                .map(Token::Float)
                .map_err(|_| Error::Sql(format!("invalid number '{text}'")))
        } else {
            text.parse::<i64>()
                .map(Token::Int)
                .map_err(|_| Error::Sql(format!("invalid number '{text}'")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lex(s: &str) -> Result<Vec<Token>> {
        Lexer::new(s).tokenize()
    }

    #[test]
    fn basic_tokens() {
        let t = lex("SELECT * FROM memories WHERE id = 42").unwrap();
        assert_eq!(t[0], Token::Word("select".into()));
        assert_eq!(t[1], Token::Star);
        assert_eq!(t[4], Token::Word("where".into()));
        assert_eq!(t[5], Token::Word("id".into()));
        assert_eq!(t[6], Token::Eq);
        assert_eq!(t[7], Token::Int(42));
        assert_eq!(*t.last().unwrap(), Token::Eof);
    }

    #[test]
    fn string_with_escape_and_chinese() {
        let t = lex("INSERT INTO memories VALUES ('它''s 记忆', 'rust')").unwrap();
        assert_eq!(t[5], Token::Str("它's 记忆".into()));
        assert_eq!(t[7], Token::Str("rust".into()));
    }

    #[test]
    fn operators() {
        let t = lex("a <> b != c <= d >= e < f > g").unwrap();
        assert_eq!(t[1], Token::NotEq);
        assert_eq!(t[3], Token::NotEq);
        assert_eq!(t[5], Token::Le);
        assert_eq!(t[7], Token::Ge);
        assert_eq!(t[9], Token::Lt);
        assert_eq!(t[11], Token::Gt);
    }

    #[test]
    fn float_and_percent() {
        let t = lex("importance >= 0.75 AND content LIKE '%x%'").unwrap();
        assert_eq!(t[1], Token::Ge);
        assert_eq!(t[2], Token::Float(0.75));
        assert_eq!(t[4], Token::Word("content".into()));
        assert_eq!(t[5], Token::Word("like".into()));
        assert_eq!(t[6], Token::Str("%x%".into()));
    }

    #[test]
    fn errors() {
        assert!(lex("SELECT 'unterminated").is_err());
        assert!(lex("SELECT # comment").is_err());
        assert!(lex("SELECT a ! b").is_err());
    }
}
