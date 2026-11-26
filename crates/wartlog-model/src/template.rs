//! Message template rendering: positional `{}` and .NET/Serilog templates
//! (`{Name}`, `{Name:format}`, `{Name,alignment}`, `{@Name}`, `{$Name}`). `{{` and `}}` are literal braces.
//! The same function runs at write time (for the term index) and read time (for `Body`).

use crate::Value;

/// One hole in a template.
#[derive(Debug, Clone, PartialEq)]
pub struct Hole {
    /// empty for positional `{}`
    pub name: String,
    pub format: Option<String>,
    pub alignment: Option<i32>,
    /// byte range of the hole in the template, braces included
    pub span: (usize, usize),
}

/// Parses holes; text between holes is everything else.
pub fn holes(t: &str) -> Vec<Hole> {
    let b = t.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'{' if b.get(i + 1) == Some(&b'{') => i += 2,
            b'}' if b.get(i + 1) == Some(&b'}') => i += 2,
            b'{' => match t[i + 1..].find('}') {
                Some(rel) => {
                    let inner = &t[i + 1..i + 1 + rel];
                    if inner.contains('{') {
                        i += 1;
                        continue;
                    }
                    let inner = inner.trim_start_matches(['@', '$']);
                    let (head, format) = match inner.split_once(':') {
                        Some((h, f)) => (h, Some(f.to_string())),
                        None => (inner, None),
                    };
                    let (name, alignment) = match head.split_once(',') {
                        Some((n, a)) => (n, a.trim().parse().ok()),
                        None => (head, None),
                    };
                    let end = i + 1 + rel + 1;
                    out.push(Hole { name: name.trim().to_string(), format, alignment, span: (i, end) });
                    i = end;
                }
                None => break,
            },
            _ => i += 1,
        }
    }
    out
}

/// Renders a template with args in hole order. Missing args render as the hole text.
pub fn render(t: &str, args: &[Value]) -> String {
    render_with(t, args, |_, s| s)
}

/// Renders and also returns the byte ranges of each rendered arg (for highlighting).
pub fn render_spans(t: &str, args: &[Value]) -> (String, Vec<(usize, usize)>) {
    let mut spans = Vec::new();
    let s = render_with(t, args, |start, s| {
        spans.push((start, start + s.len()));
        s
    });
    (s, spans)
}

fn render_with(t: &str, args: &[Value], mut on_arg: impl FnMut(usize, String) -> String) -> String {
    let hs = holes(t);
    let mut out = String::with_capacity(t.len() + 16);
    let mut last = 0;
    for (n, h) in hs.iter().enumerate() {
        push_literal(&mut out, &t[last..h.span.0]);
        match args.get(n) {
            Some(v) => {
                let mut s = format_value(v, h.format.as_deref());
                if let Some(a) = h.alignment {
                    let w = a.unsigned_abs() as usize;
                    let len = s.chars().count();
                    if len < w {
                        let pad = " ".repeat(w - len);
                        s = if a < 0 { s + &pad } else { pad + &s };
                    }
                }
                let start = out.len();
                let s = on_arg(start, s);
                out.push_str(&s);
            }
            None => out.push_str(&t[h.span.0..h.span.1]),
        }
        last = h.span.1;
    }
    push_literal(&mut out, &t[last..]);
    out
}

fn push_literal(out: &mut String, s: &str) {
    out.push_str(&s.replace("{{", "{").replace("}}", "}"));
}

/// Best effort .NET format strings: F/N/E/X/D with precision, and custom 0.00 patterns for numbers.
fn format_value(v: &Value, f: Option<&str>) -> String {
    let Some(f) = f else { return v.to_text() };
    let num = match v {
        Value::I64(_) | Value::F64(_) => v.as_f64(),
        _ => None,
    };
    let Some(x) = num else { return v.to_text() };
    let (c, rest) = f.split_at(f.chars().next().map(|c| c.len_utf8()).unwrap_or(0));
    let prec: Option<usize> = rest.parse().ok();
    match c {
        "F" | "f" => format!("{:.*}", prec.unwrap_or(2), x),
        "N" | "n" => group_thousands(&format!("{:.*}", prec.unwrap_or(2), x)),
        "E" | "e" => format!("{:.*e}", prec.unwrap_or(6), x),
        "D" | "d" if x.fract() == 0.0 => format!("{:0width$}", x as i64, width = prec.unwrap_or(0)),
        "X" | "x" if x.fract() == 0.0 && x >= 0.0 => {
            let s = format!("{:0width$x}", x as u64, width = prec.unwrap_or(0));
            if c == "X" { s.to_uppercase() } else { s }
        }
        "P" | "p" => format!("{:.*} %", prec.unwrap_or(2), x * 100.0),
        _ if f.chars().all(|c| c == '0' || c == '#' || c == '.' || c == ',') => {
            let decimals = f.split_once('.').map(|(_, d)| d.len()).unwrap_or(0);
            let s = format!("{:.*}", decimals, x);
            if f.contains(',') { group_thousands(&s) } else { s }
        }
        _ => v.to_text(),
    }
}

fn group_thousands(s: &str) -> String {
    let (sign, s) = s.strip_prefix('-').map(|r| ("-", r)).unwrap_or(("", s));
    let (int, frac) = s.split_once('.').map(|(a, b)| (a, Some(b))).unwrap_or((s, None));
    let mut g = String::new();
    for (i, ch) in int.chars().enumerate() {
        if i > 0 && (int.len() - i) % 3 == 0 {
            g.push(',');
        }
        g.push(ch);
    }
    match frac {
        Some(f) => format!("{sign}{g}.{f}"),
        None => format!("{sign}{g}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positional_and_named() {
        assert_eq!(render("Resolved {} to {}", &[Value::Str("db".into()), Value::Str("10.0.0.1".into())]), "Resolved db to 10.0.0.1");
        assert_eq!(render("GET {Path} took {Elapsed:F1} ms", &[Value::Str("/x".into()), Value::F64(3.14159)]), "GET /x took 3.1 ms");
        assert_eq!(render("{@User} {$Id,5}|", &[Value::Str("tim".into()), Value::I64(42)]), "tim    42|");
        assert_eq!(render("{{literal}} {A}", &[Value::I64(1)]), "{literal} 1");
        assert_eq!(render("missing {A} {B}", &[Value::I64(1)]), "missing 1 {B}");
        assert_eq!(render("{N:N0} items", &[Value::I64(1234567)]), "1,234,567 items");
    }

    #[test]
    fn spans_cover_args() {
        let (s, sp) = render_spans("a {X} b {Y}", &[Value::I64(10), Value::Str("yy".into())]);
        assert_eq!(s, "a 10 b yy");
        assert_eq!(sp, vec![(2, 4), (7, 9)]);
        assert_eq!(holes("{A} {B:x}")[1].name, "B");
    }
}
