//! Recursive-descent parser for the supported KQL subset (spec section 9).

use std::sync::Arc;
use wartlog_model::Value;

use crate::ast::*;
use crate::lexer::{lex, Tok, Token};
use crate::QueryError;

pub fn parse(src: &str) -> Result<Query, QueryError> {
    let toks = lex(src)?;
    let mut p = Parser { t: toks, i: 0, len: src.len() };
    let q = p.query()?;
    if p.i < p.t.len() {
        return Err(p.err_here("unexpected text after the query"));
    }
    Ok(q)
}

/// Parses a standalone expression (used by the viewer for filters).
pub fn parse_expr(src: &str) -> Result<Expr, QueryError> {
    let toks = lex(src)?;
    let mut p = Parser { t: toks, i: 0, len: src.len() };
    let e = p.expr()?;
    if p.i < p.t.len() {
        return Err(p.err_here("unexpected text after the expression"));
    }
    Ok(e)
}

pub const OPERATORS: &[&str] = &["where", "filter", "project", "project-away", "extend", "take", "limit", "sort", "order", "top", "summarize", "distinct", "count", "search"];

const UNSUPPORTED: &[&str] = &[
    "join", "let", "union", "make-series", "render", "materialize", "mv-expand", "parse", "evaluate", "lookup", "as", "invoke", "project-keep",
    "project-rename", "project-reorder", "sample", "serialize", "range", "print", "datatable", "facet", "fork", "getschema", "scan",
];

struct Parser {
    t: Vec<Token>,
    i: usize,
    len: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.t.get(self.i).map(|t| &t.tok)
    }
    fn peek_at(&self, n: usize) -> Option<&Tok> {
        self.t.get(self.i + n).map(|t| &t.tok)
    }
    fn next(&mut self) -> Option<Tok> {
        let t = self.t.get(self.i).map(|t| t.tok.clone());
        self.i += 1;
        t
    }
    fn span_here(&self) -> (usize, usize) {
        match self.t.get(self.i) {
            Some(t) => (t.start, t.end),
            None => (self.len, self.len),
        }
    }
    fn err_here(&self, msg: impl Into<String>) -> QueryError {
        let (s, e) = self.span_here();
        QueryError::at(s, e, msg)
    }
    fn is_ident(&self, w: &str) -> bool {
        matches!(self.peek(), Some(Tok::Ident(x)) if x.eq_ignore_ascii_case(w))
    }
    fn eat_ident(&mut self, w: &str) -> bool {
        if self.is_ident(w) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn expect(&mut self, t: Tok, what: &str) -> Result<(), QueryError> {
        if self.peek() == Some(&t) {
            self.i += 1;
            Ok(())
        } else {
            Err(self.err_here(format!("expected {what}")))
        }
    }
    fn ident(&mut self, what: &str) -> Result<String, QueryError> {
        match self.peek() {
            Some(Tok::Ident(s)) => {
                let s = s.clone();
                self.i += 1;
                Ok(s)
            }
            _ => Err(self.err_here(format!("expected {what}"))),
        }
    }
    fn number(&mut self, what: &str) -> Result<u64, QueryError> {
        match self.peek() {
            Some(Tok::Long(n)) if *n >= 0 => {
                let n = *n as u64;
                self.i += 1;
                Ok(n)
            }
            _ => Err(self.err_here(format!("expected {what}"))),
        }
    }

    fn query(&mut self) -> Result<Query, QueryError> {
        let mut table = Table::Logs;
        match self.peek() {
            Some(Tok::Ident(w)) if w == "logs" || w == "spans" => {
                table = if w == "spans" { Table::Spans } else { Table::Logs };
                self.i += 1;
                if self.peek().is_some() && self.peek() != Some(&Tok::Pipe) {
                    return Err(self.err_here("expected `|` after the table name"));
                }
            }
            Some(Tok::Ident(w)) if !OPERATORS.contains(&w.as_str()) && !UNSUPPORTED.contains(&w.as_str()) => {
                return Err(self.err_here(format!("unknown table `{w}`; the tables are `logs` and `spans`")));
            }
            _ => {}
        }
        let mut ops = Vec::new();
        let mut first = true;
        while self.peek().is_some() {
            if self.peek() == Some(&Tok::Pipe) {
                self.i += 1;
            } else if !first {
                return Err(self.err_here("expected `|`"));
            }
            first = false;
            if self.peek().is_none() {
                return Err(self.err_here("expected an operator after `|`"));
            }
            ops.push(self.op()?);
        }
        Ok(Query { table, ops })
    }

    fn op(&mut self) -> Result<Op, QueryError> {
        let (s, e) = self.span_here();
        let name = self.ident("an operator such as where, project, take, sort, summarize")?;
        Ok(match name.as_str() {
            "where" | "filter" => Op::Where(self.expr()?),
            "project" => Op::Project(self.named_list(false)?),
            "project-away" => {
                let mut v = vec![self.col_name()?];
                while self.peek() == Some(&Tok::Comma) {
                    self.i += 1;
                    v.push(self.col_name()?);
                }
                Op::ProjectAway(v)
            }
            "extend" => Op::Extend(self.named_list(false)?),
            "take" | "limit" => Op::Take(self.number("a row count")?),
            "sort" | "order" => {
                if !self.eat_ident("by") {
                    return Err(self.err_here("expected `by`"));
                }
                let mut keys = vec![self.sort_key()?];
                while self.peek() == Some(&Tok::Comma) {
                    self.i += 1;
                    keys.push(self.sort_key()?);
                }
                Op::Sort(keys)
            }
            "top" => {
                let n = self.number("a row count")?;
                if !self.eat_ident("by") {
                    return Err(self.err_here("expected `by`"));
                }
                Op::Top(n, self.sort_key()?)
            }
            "summarize" => self.summarize()?,
            "distinct" => Op::Distinct(self.named_list(false)?),
            "count" => Op::Count,
            "search" => {
                // optional `in (logs)` and `kind=` are not supported; accept `search "x"` and `search x`
                match self.next() {
                    Some(Tok::Str(s)) => Op::Search(s),
                    Some(Tok::Ident(s)) => Op::Search(s),
                    _ => return Err(self.err_here("expected a search term")),
                }
            }
            other if UNSUPPORTED.contains(&other) => {
                return Err(QueryError::at(s, e, format!("`{other}` is not supported yet in WartLog's KQL subset")))
            }
            other => return Err(QueryError::at(s, e, format!("unknown operator `{other}`"))),
        })
    }

    fn col_name(&mut self) -> Result<String, QueryError> {
        let mut n = self.ident("a column name")?;
        while self.peek() == Some(&Tok::Dot) {
            self.i += 1;
            n.push('.');
            n.push_str(&self.ident("a name after `.`")?);
        }
        Ok(n)
    }

    /// `name = expr` or `expr`, comma separated.
    fn named_list(&mut self, _aggs: bool) -> Result<Vec<(String, Expr)>, QueryError> {
        let mut v = Vec::new();
        loop {
            let n = v.len();
            if matches!(self.peek(), Some(Tok::Ident(_))) && self.peek_at(1) == Some(&Tok::Eq) {
                let name = self.ident("a name")?;
                self.i += 1;
                v.push((name, self.expr()?));
            } else {
                let e = self.expr()?;
                v.push((e.default_name(n + 1), e));
            }
            if self.peek() == Some(&Tok::Comma) {
                self.i += 1;
            } else {
                return Ok(v);
            }
        }
    }

    fn sort_key(&mut self) -> Result<SortKey, QueryError> {
        let expr = self.expr()?;
        let mut desc = true;
        if self.eat_ident("asc") {
            desc = false;
        } else if self.eat_ident("desc") {
            desc = true;
        }
        let mut nulls_first = !desc;
        if self.eat_ident("nulls") {
            if self.eat_ident("first") {
                nulls_first = true;
            } else if self.eat_ident("last") {
                nulls_first = false;
            } else {
                return Err(self.err_here("expected `first` or `last`"));
            }
        }
        Ok(SortKey { expr, desc, nulls_first })
    }

    fn summarize(&mut self) -> Result<Op, QueryError> {
        let mut aggs = Vec::new();
        if !self.is_ident("by") {
            loop {
                let (name, start) = if matches!(self.peek(), Some(Tok::Ident(_))) && self.peek_at(1) == Some(&Tok::Eq) {
                    let n = self.ident("a name")?;
                    self.i += 1;
                    (Some(n), self.span_here())
                } else {
                    (None, self.span_here())
                };
                let e = self.expr()?;
                let agg = match e {
                    Expr::Call(f, args) => {
                        let func = match f.as_str() {
                            "count" => AggFunc::Count,
                            "countif" => AggFunc::CountIf,
                            "dcount" => AggFunc::DCount,
                            "min" => AggFunc::Min,
                            "max" => AggFunc::Max,
                            "avg" => AggFunc::Avg,
                            "sum" => AggFunc::Sum,
                            other => return Err(QueryError::at(start.0, start.1, format!("`{other}` is not a supported aggregation (count, countif, dcount, min, max, avg, sum)"))),
                        };
                        let want = if func == AggFunc::Count { 0 } else { 1 };
                        if args.len() != want {
                            return Err(QueryError::at(start.0, start.1, format!("{f}() takes {want} argument(s)")));
                        }
                        Agg { func, arg: args.into_iter().next() }
                    }
                    _ => return Err(QueryError::at(start.0, start.1, "expected an aggregation such as count()")),
                };
                let name = name.unwrap_or_else(|| match &agg.arg {
                    Some(Expr::Col(c)) => format!("{}_{}", agg.func.name(), c.name()),
                    _ => format!("{}_", agg.func.name()),
                });
                aggs.push((name, agg));
                if self.peek() == Some(&Tok::Comma) {
                    self.i += 1;
                } else {
                    break;
                }
            }
        }
        let by = if self.eat_ident("by") { self.named_list(false)? } else { vec![] };
        if aggs.is_empty() && by.is_empty() {
            return Err(self.err_here("summarize needs an aggregation or `by`"));
        }
        Ok(Op::Summarize { aggs, by })
    }

    // ---------- expressions ----------

    pub fn expr(&mut self) -> Result<Expr, QueryError> {
        self.or()
    }

    fn or(&mut self) -> Result<Expr, QueryError> {
        let mut l = self.and()?;
        while self.eat_ident("or") {
            let r = self.and()?;
            l = Expr::Bin(BinOp::Or, Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn and(&mut self) -> Result<Expr, QueryError> {
        let mut l = self.cmp()?;
        while self.eat_ident("and") {
            let r = self.cmp()?;
            l = Expr::Bin(BinOp::And, Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn cmp(&mut self) -> Result<Expr, QueryError> {
        let l = self.add()?;
        let op = match self.peek() {
            Some(Tok::EqEq) => Some(BinOp::Eq),
            Some(Tok::NotEq) => Some(BinOp::Ne),
            Some(Tok::TildeEq) => Some(BinOp::EqCi),
            Some(Tok::NotTilde) => Some(BinOp::NeCi),
            Some(Tok::Lt) => Some(BinOp::Lt),
            Some(Tok::Le) => Some(BinOp::Le),
            Some(Tok::Gt) => Some(BinOp::Gt),
            Some(Tok::Ge) => Some(BinOp::Ge),
            Some(Tok::Eq) => return Err(self.err_here("use `==` to compare (a single `=` names a column)")),
            _ => None,
        };
        if let Some(op) = op {
            self.i += 1;
            let r = self.add()?;
            return Ok(Expr::Bin(op, Box::new(l), Box::new(r)));
        }
        let word = match self.peek() {
            Some(Tok::Ident(w)) => w.clone(),
            _ => return Ok(l),
        };
        let (neg, base) = match word.strip_prefix('!') {
            Some(b) => (true, b.to_string()),
            None => (false, word.clone()),
        };
        let (base, cs) = match base.strip_suffix("_cs") {
            Some(b) => (b.to_string(), true),
            None => (base, false),
        };
        let strop = match base.as_str() {
            "has" => Some(StrOp::Has),
            "contains" => Some(StrOp::Contains),
            "startswith" => Some(StrOp::StartsWith),
            "endswith" => Some(StrOp::EndsWith),
            "hasprefix" => Some(StrOp::HasPrefix),
            _ => None,
        };
        if let Some(op) = strop {
            self.i += 1;
            let r = self.add()?;
            return Ok(Expr::Str { op, cs, neg, lhs: Box::new(l), rhs: Box::new(r) });
        }
        match base.as_str() {
            "in" | "in~" => {
                self.i += 1;
                self.expect(Tok::LParen, "`(` after in")?;
                let mut list = Vec::new();
                if self.peek() != Some(&Tok::RParen) {
                    loop {
                        list.push(self.expr()?);
                        if self.peek() == Some(&Tok::Comma) {
                            self.i += 1;
                        } else {
                            break;
                        }
                    }
                }
                self.expect(Tok::RParen, "`)`")?;
                Ok(Expr::In { lhs: Box::new(l), list, neg, ci: base == "in~" })
            }
            "between" => {
                self.i += 1;
                self.expect(Tok::LParen, "`(` after between")?;
                let lo = self.add()?;
                self.expect(Tok::DotDot, "`..`")?;
                let hi = self.add()?;
                self.expect(Tok::RParen, "`)`")?;
                Ok(Expr::Between { lhs: Box::new(l), lo: Box::new(lo), hi: Box::new(hi), neg })
            }
            "matches" if !neg => {
                self.i += 1;
                if !self.eat_ident("regex") {
                    return Err(self.err_here("expected `regex` after matches"));
                }
                let (s, e) = self.span_here();
                let pat = match self.next() {
                    Some(Tok::Str(p)) => p,
                    _ => return Err(QueryError::at(s, e, "expected a regex string")),
                };
                let re = regex::Regex::new(&pat).map_err(|x| QueryError::at(s, e, format!("bad regex: {x}")))?;
                Ok(Expr::Regex { lhs: Box::new(l), re: Re(Arc::new(re)) })
            }
            _ => Ok(l),
        }
    }

    fn add(&mut self) -> Result<Expr, QueryError> {
        let mut l = self.mul()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Plus) => BinOp::Add,
                Some(Tok::Minus) => BinOp::Sub,
                _ => return Ok(l),
            };
            self.i += 1;
            let r = self.mul()?;
            l = Expr::Bin(op, Box::new(l), Box::new(r));
        }
    }

    fn mul(&mut self) -> Result<Expr, QueryError> {
        let mut l = self.unary()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Star) => BinOp::Mul,
                Some(Tok::Slash) => BinOp::Div,
                Some(Tok::Percent) => BinOp::Mod,
                _ => return Ok(l),
            };
            self.i += 1;
            let r = self.unary()?;
            l = Expr::Bin(op, Box::new(l), Box::new(r));
        }
    }

    fn unary(&mut self) -> Result<Expr, QueryError> {
        if self.peek() == Some(&Tok::Minus) {
            self.i += 1;
            let e = self.unary()?;
            return Ok(match e {
                Expr::Lit(Value::I64(n)) => Expr::Lit(Value::I64(-n)),
                Expr::Lit(Value::F64(n)) => Expr::Lit(Value::F64(-n)),
                Expr::Lit(Value::Duration(n)) => Expr::Lit(Value::Duration(-n)),
                e => Expr::Neg(Box::new(e)),
            });
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Expr, QueryError> {
        let (s, e) = self.span_here();
        match self.next() {
            Some(Tok::Str(v)) => Ok(Expr::Lit(Value::Str(v))),
            Some(Tok::Long(n)) => Ok(Expr::Lit(Value::I64(n))),
            Some(Tok::Real(n)) => Ok(Expr::Lit(Value::F64(n))),
            Some(Tok::Span(n)) => Ok(Expr::Lit(Value::Duration(n))),
            Some(Tok::DateTime(t)) => match t.trim() {
                "" => Err(QueryError::at(s, e, "datetime() needs a value")),
                t => wartlog_model::time::parse_datetime(t)
                    .map(|n| Expr::Lit(Value::Timestamp(n)))
                    .ok_or_else(|| QueryError::at(s, e, format!("bad datetime `{t}`"))),
            },
            Some(Tok::LParen) => {
                let x = self.expr()?;
                self.expect(Tok::RParen, "`)`")?;
                Ok(x)
            }
            Some(Tok::Ident(name)) => {
                match name.as_str() {
                    "true" => return Ok(Expr::Lit(Value::Bool(true))),
                    "false" => return Ok(Expr::Lit(Value::Bool(false))),
                    _ => {}
                }
                if self.peek() == Some(&Tok::LParen) {
                    self.i += 1;
                    let mut args = Vec::new();
                    if self.peek() != Some(&Tok::RParen) {
                        loop {
                            args.push(self.expr()?);
                            if self.peek() == Some(&Tok::Comma) {
                                self.i += 1;
                            } else {
                                break;
                            }
                        }
                    }
                    self.expect(Tok::RParen, "`)`")?;
                    return match name.as_str() {
                        "not" => {
                            if args.len() != 1 {
                                return Err(QueryError::at(s, e, "not() takes one argument"));
                            }
                            Ok(Expr::Not(Box::new(args.pop().unwrap())))
                        }
                        "dynamic" if args.len() == 1 => Ok(args.pop().unwrap()),
                        "timespan" | "totimespan" if args.len() == 1 => match &args[0] {
                            Expr::Lit(Value::Duration(_)) => Ok(args.pop().unwrap()),
                            Expr::Lit(Value::Str(x)) => wartlog_model::time::parse_duration(x)
                                .map(|n| Expr::Lit(Value::Duration(n)))
                                .ok_or_else(|| QueryError::at(s, e, format!("bad timespan `{x}`"))),
                            _ => Ok(Expr::Call(name, args)),
                        },
                        f if crate::eval::is_function(f) || crate::eval::is_aggregate(f) => Ok(Expr::Call(name, args)),
                        f => Err(QueryError::at(s, e, format!("unknown function `{f}`"))),
                    };
                }
                if name == "null" {
                    return Ok(Expr::Lit(Value::Null));
                }
                // Attributes["k"], Resource["k"], Attributes.k
                if (name == "Attributes" || name == "Resource") && matches!(self.peek(), Some(Tok::LBracket) | Some(Tok::Dot)) {
                    let key = if self.peek() == Some(&Tok::LBracket) {
                        self.i += 1;
                        let k = match self.next() {
                            Some(Tok::Str(k)) => k,
                            _ => return Err(self.err_here("expected a quoted key")),
                        };
                        self.expect(Tok::RBracket, "`]`")?;
                        k
                    } else {
                        let mut k = String::new();
                        while self.peek() == Some(&Tok::Dot) {
                            self.i += 1;
                            if !k.is_empty() {
                                k.push('.');
                            }
                            k.push_str(&self.ident("a key")?);
                        }
                        k
                    };
                    return Ok(Expr::Col(if name == "Attributes" { Column::Attr(key) } else { Column::Resource(key) }));
                }
                let mut full = name;
                while self.peek() == Some(&Tok::Dot) && matches!(self.peek_at(1), Some(Tok::Ident(_))) {
                    self.i += 1;
                    full.push('.');
                    full.push_str(&self.ident("a name")?);
                }
                Ok(Expr::Col(Column::from_name(&full)))
            }
            Some(_) | None => Err(QueryError::at(s, e, "expected a value, column or function")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_queries() {
        let q = parse(r#"logs | where Unit == "foo.service" and Level == "error" | take 50"#).unwrap();
        assert_eq!(q.ops.len(), 2);
        let q = parse(r#"where Body has "timeout" | summarize count() by bin(Timestamp, 5m), Source | sort by count_ desc"#).unwrap();
        match &q.ops[1] {
            Op::Summarize { aggs, by } => {
                assert_eq!(aggs[0].0, "count_");
                assert_eq!(by[0].0, "bin_Timestamp");
                assert_eq!(by[1].0, "Source");
            }
            o => panic!("{o:?}"),
        }
        parse("spans | where Duration > 100ms | project Name, Duration, d = Duration / 1ms").unwrap();
        parse(r#"logs | where Attributes["http.status"] in (500, 503) and not(Body contains "health")"#).unwrap();
        parse(r#"where Timestamp between (ago(1h) .. now()) | top 10 by Timestamp asc"#).unwrap();
        parse(r#"where Body matches regex "id=[0-9]+" | project-away Template | distinct Source"#).unwrap();
        parse(r#"search "deadlock" | count"#).unwrap();
        parse(r#"where Timestamp > datetime(2026-11-26T00:00:00Z) and _PID == 123 and service.name =~ "API""#).unwrap();
    }

    #[test]
    fn helpful_errors() {
        let e = parse("logs | join x").unwrap_err();
        assert!(e.message.contains("not supported"), "{}", e.message);
        assert_eq!(&"logs | join x"[e.start..e.end], "join");
        assert!(parse("where a = 1").unwrap_err().message.contains("=="));
        assert!(parse("events | take 1").unwrap_err().message.contains("unknown table"));
        assert!(parse("where frob(1)").unwrap_err().message.contains("unknown function"));
        assert!(parse("logs | where").is_err());
        assert!(parse("logs where x").is_err());
    }
}
