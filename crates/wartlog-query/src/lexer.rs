//! KQL lexer. Tokens carry byte spans for error messages and completion.

use crate::QueryError;

#[derive(Clone, Debug, PartialEq)]
pub enum Tok {
    Ident(String),
    Str(String),
    Long(i64),
    Real(f64),
    /// timespan literal in ns, like `5m`
    Span(i64),
    /// raw text of `datetime(...)`
    DateTime(String),
    Pipe,
    Comma,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Dot,
    DotDot,
    Eq,
    EqEq,
    NotEq,
    TildeEq,
    NotTilde,
    Lt,
    Le,
    Gt,
    Ge,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Colon,
    Semi,
    Bang,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Token {
    pub tok: Tok,
    pub start: usize,
    pub end: usize,
}

fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_' || c == '$' || c == '@'
}
fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$' || c == '@'
}

/// Names that may contain `-` or start with `!` (operators and keywords).
const DASHED: &[&str] = &["project-away", "project-keep", "project-rename", "project-reorder", "mv-expand", "make-series"];

pub fn lex(src: &str) -> Result<Vec<Token>, QueryError> {
    let mut out = Vec::new();
    let b: Vec<(usize, char)> = src.char_indices().collect();
    let mut i = 0;
    let at = |i: usize| b.get(i).map(|x| x.1);
    let pos = |i: usize| b.get(i).map(|x| x.0).unwrap_or(src.len());
    while i < b.len() {
        let (start, c) = b[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        // comments
        if c == '/' && at(i + 1) == Some('/') {
            while i < b.len() && b[i].1 != '\n' {
                i += 1;
            }
            continue;
        }
        let single = |t: Tok, n: usize| (t, n);
        let two = |a: char, bb: char| c == a && at(i + 1) == Some(bb);
        let (tok, n) = if two('=', '=') {
            single(Tok::EqEq, 2)
        } else if two('!', '=') {
            single(Tok::NotEq, 2)
        } else if two('=', '~') {
            single(Tok::TildeEq, 2)
        } else if two('!', '~') {
            single(Tok::NotTilde, 2)
        } else if two('<', '=') {
            single(Tok::Le, 2)
        } else if two('>', '=') {
            single(Tok::Ge, 2)
        } else if two('<', '>') {
            single(Tok::NotEq, 2)
        } else if two('.', '.') {
            single(Tok::DotDot, 2)
        } else {
            match c {
                '|' => single(Tok::Pipe, 1),
                ',' => single(Tok::Comma, 1),
                '(' => single(Tok::LParen, 1),
                ')' => single(Tok::RParen, 1),
                '[' => single(Tok::LBracket, 1),
                ']' => single(Tok::RBracket, 1),
                '=' => single(Tok::Eq, 1),
                '<' => single(Tok::Lt, 1),
                '>' => single(Tok::Gt, 1),
                '+' => single(Tok::Plus, 1),
                '-' => single(Tok::Minus, 1),
                '*' => single(Tok::Star, 1),
                '/' => single(Tok::Slash, 1),
                '%' => single(Tok::Percent, 1),
                ':' => single(Tok::Colon, 1),
                ';' => single(Tok::Semi, 1),
                '.' if !at(i + 1).is_some_and(|d| d.is_ascii_digit()) => single(Tok::Dot, 1),
                '!' if at(i + 1).is_some_and(is_ident_start) => {
                    // !has, !contains, !in, !between, ...
                    let mut j = i + 1;
                    while j < b.len() && is_ident(b[j].1) {
                        j += 1;
                    }
                    if at(j) == Some('~') {
                        j += 1;
                    }
                    let word: String = src[start..pos(j)].to_string();
                    out.push(Token { tok: Tok::Ident(word), start, end: pos(j) });
                    i = j;
                    continue;
                }
                '!' => single(Tok::Bang, 1),
                '"' | '\'' => {
                    let (s, j) = lex_string(src, &b, i, false)?;
                    out.push(Token { tok: Tok::Str(s), start, end: pos(j) });
                    i = j;
                    continue;
                }
                '@' if matches!(at(i + 1), Some('"') | Some('\'')) => {
                    let (s, j) = lex_string(src, &b, i + 1, true)?;
                    out.push(Token { tok: Tok::Str(s), start, end: pos(j) });
                    i = j;
                    continue;
                }
                c if c.is_ascii_digit() || c == '.' => {
                    let mut j = i;
                    while j < b.len() && (b[j].1.is_ascii_digit() || b[j].1 == '.' && at(j + 1) != Some('.')) {
                        j += 1;
                    }
                    // exponent
                    if matches!(at(j), Some('e') | Some('E')) && at(j + 1).is_some_and(|d| d.is_ascii_digit() || d == '-' || d == '+') {
                        j += 2;
                        while j < b.len() && b[j].1.is_ascii_digit() {
                            j += 1;
                        }
                    }
                    let num = &src[start..pos(j)];
                    // timespan unit suffix
                    let mut k = j;
                    while k < b.len() && b[k].1.is_ascii_alphabetic() {
                        k += 1;
                    }
                    if k > j {
                        let lit = &src[start..pos(k)];
                        let ns = wartlog_model::time::parse_duration(lit)
                            .ok_or_else(|| QueryError::at(start, pos(k), format!("bad timespan literal `{lit}`")))?;
                        out.push(Token { tok: Tok::Span(ns), start, end: pos(k) });
                        i = k;
                        continue;
                    }
                    let tok = if num.contains(['.', 'e', 'E']) {
                        Tok::Real(num.parse().map_err(|_| QueryError::at(start, pos(j), format!("bad number `{num}`")))?)
                    } else {
                        Tok::Long(num.parse().map_err(|_| QueryError::at(start, pos(j), format!("bad number `{num}`")))?)
                    };
                    out.push(Token { tok, start, end: pos(j) });
                    i = j;
                    continue;
                }
                c if is_ident_start(c) => {
                    let mut j = i;
                    while j < b.len() && is_ident(b[j].1) {
                        j += 1;
                    }
                    // dashed operator names
                    for d in DASHED {
                        if src[start..].starts_with(d) && !src[start + d.len()..].starts_with(is_ident) {
                            j = b.iter().position(|x| x.0 >= start + d.len()).unwrap_or(b.len());
                            break;
                        }
                    }
                    // `_cs` style suffixes and `in~` / `has_any` are plain identifier chars; `in~` needs the tilde
                    if at(j) == Some('~') && &src[start..pos(j)] == "in" {
                        j += 1;
                    }
                    let word = &src[start..pos(j)];
                    if (word == "datetime" || word == "todatetime") && at(j) == Some('(') {
                        // raw datetime literal up to the closing paren
                        if let Some(close) = src[pos(j)..].find(')') {
                            let inner = src[pos(j) + 1..pos(j) + close].trim();
                            if !inner.starts_with(['"', '\'']) || word == "datetime" {
                                let inner = inner.trim_matches(['"', '\'']);
                                let end = pos(j) + close + 1;
                                out.push(Token { tok: Tok::DateTime(inner.to_string()), start, end });
                                i = b.iter().position(|x| x.0 >= end).unwrap_or(b.len());
                                continue;
                            }
                        }
                    }
                    out.push(Token { tok: Tok::Ident(word.to_string()), start, end: pos(j) });
                    i = j;
                    continue;
                }
                other => return Err(QueryError::at(start, start + other.len_utf8(), format!("unexpected character `{other}`"))),
            }
        };
        out.push(Token { tok, start, end: start + n });
        i += n;
    }
    Ok(out)
}

fn lex_string(src: &str, b: &[(usize, char)], i: usize, verbatim: bool) -> Result<(String, usize), QueryError> {
    let q = b[i].1;
    let start = b[i].0;
    let mut s = String::new();
    let mut j = i + 1;
    while j < b.len() {
        let c = b[j].1;
        if c == q {
            return Ok((s, j + 1));
        }
        if c == '\\' && !verbatim {
            j += 1;
            match b.get(j).map(|x| x.1) {
                Some('n') => s.push('\n'),
                Some('t') => s.push('\t'),
                Some('r') => s.push('\r'),
                Some('0') => s.push('\0'),
                Some(x) => s.push(x),
                None => break,
            }
        } else {
            s.push(c);
        }
        j += 1;
    }
    Err(QueryError::at(start, src.len(), "unterminated string literal"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(s: &str) -> Vec<Tok> {
        lex(s).unwrap().into_iter().map(|t| t.tok).collect()
    }

    #[test]
    fn basics() {
        assert_eq!(
            toks("logs | where Body !has 'x' and n >= 5m"),
            vec![
                Tok::Ident("logs".into()),
                Tok::Pipe,
                Tok::Ident("where".into()),
                Tok::Ident("Body".into()),
                Tok::Ident("!has".into()),
                Tok::Str("x".into()),
                Tok::Ident("and".into()),
                Tok::Ident("n".into()),
                Tok::Ge,
                Tok::Span(300_000_000_000),
            ]
        );
        assert_eq!(toks("project-away a"), vec![Tok::Ident("project-away".into()), Tok::Ident("a".into())]);
        assert_eq!(toks("datetime(2026-11-26 10:00)"), vec![Tok::DateTime("2026-11-26 10:00".into())]);
        assert_eq!(toks(r#"@"c:\x" "a\"b""#), vec![Tok::Str(r"c:\x".into()), Tok::Str("a\"b".into())]);
        assert_eq!(toks("1 .. 2.5"), vec![Tok::Long(1), Tok::DotDot, Tok::Real(2.5)]);
        assert_eq!(toks("x in~ (1)"), vec![Tok::Ident("x".into()), Tok::Ident("in~".into()), Tok::LParen, Tok::Long(1), Tok::RParen]);
        assert!(lex("'open").is_err());
    }
}
