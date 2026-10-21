//! Formatted output on both sides: a C++ printf-family call
//! (`fprintf(&outfile, "%#" PRIx64 "\n", x)`) and the Rust formatting macro
//! that replaces it (`writeln!(outfile, "{:#x}", x)`), compared by what
//! their format strings print.

use regex::Regex;
use std::sync::LazyLock;

/// Where a call prints.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Dest {
    /// A stream the call takes: `fprintf(&f, ...)`, `write!(f, ...)`.
    Stream,
    /// The console: `printf`, `kprintln!`.
    Console,
}

impl Dest {
    /// Normalized names of the C++ calls that print here.
    pub(crate) fn cpp_calls(self) -> &'static [&'static str] {
        match self {
            Dest::Stream => &["fprintf"],
            Dest::Console => &["print"],
        }
    }

    /// Normalized names of the Rust macros that print here.
    pub(crate) fn rust_calls(self) -> &'static [&'static str] {
        match self {
            Dest::Stream => &["write", "writeln"],
            Dest::Console => &["print", "eprint"],
        }
    }
}

/// A piece of a format string: literal text, or a conversion with its
/// Rust format spec (`""` for `{}`, `"#x"` for `{:#x}`).
#[derive(Debug, PartialEq)]
enum Piece {
    Text(String),
    /// `wrapper` is set for a Rust `{}` whose argument is a wrapper call
    /// (`CAltHex(x)`), which may print as any C++ conversion.
    Arg {
        spec: String,
        wrapper: bool,
    },
}

/// How a printf-family call and a Rust formatting macro compare.
#[derive(Debug, PartialEq)]
pub(crate) enum Output {
    /// They print the same text.
    Same,
    /// Their formats differ; the message says how.
    Differs(String),
}

/// Compares the output call in C++ `cpp` with the one in Rust `rust`, or
/// `None` unless both are output calls to the same [`Dest`] whose formats
/// can be read.
pub(crate) fn compare(cpp: &str, rust: &str) -> Option<(Dest, Output)> {
    let ((dest, a), (rust_dest, b)) = (cpp_format(cpp)?, rust_format(rust)?);
    if dest != rust_dest {
        return None;
    }
    let agree = a.len() == b.len()
        && a.iter().zip(&b).all(|p| match p {
            (Piece::Text(x), Piece::Text(y)) => x == y,
            (Piece::Arg { spec: x, .. }, Piece::Arg { spec: y, wrapper }) => {
                x == y || (y.is_empty() && *wrapper)
            }
            _ => false,
        });
    Some((
        dest,
        if agree {
            Output::Same
        } else {
            Output::Differs(format!(
                "format string differs: C++ prints \"{}\", Rust prints \"{}\"",
                show(&a),
                show(&b)
            ))
        },
    ))
}

/// Whether `cpp` and `rust` are output calls that print the same text.
pub(crate) fn same_output(cpp: &str, rust: &str) -> bool {
    compare(cpp, rust).is_some_and(|(_, o)| o == Output::Same)
}

/// Where the first printf-family call in C++ `text` prints, and its
/// format, with `PRIu64`-style macros expanded and conversions in Rust's
/// syntax.
fn cpp_format(text: &str) -> Option<(Dest, Vec<Piece>)> {
    static CALL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b(f|d|k)?printf\s*\(").unwrap());
    static PRI: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^PRI([diouxX])\w*$").unwrap());
    let m = CALL.captures(text)?;
    let args = split_args(&text[m.get(0)?.end()..]);
    // `fprintf` and `dprintf` take the stream first.
    let prefix = m.get(1).map(|g| g.as_str());
    let fmt = args.get(usize::from(matches!(prefix, Some("f" | "d"))))?;
    let dest = if prefix == Some("f") { Dest::Stream } else { Dest::Console };
    let mut s = String::new();
    for token in tokens(fmt) {
        if let Some(lit) = token.strip_prefix('"') {
            s.push_str(&unescape(lit.strip_suffix('"')?, false));
        } else {
            s.push_str(&PRI.captures(&token)?[1]);
        }
    }
    Some((dest, printf_pieces(&s)?))
}

/// Where the first Rust formatting macro in `text` prints, and its format,
/// with the newline `writeln!` and friends add.
fn rust_format(text: &str) -> Option<(Dest, Vec<Piece>)> {
    static MACRO: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\b(write|writeln|print|println|kprint|kprintln|eprint|eprintln)!\s*[(\[{]")
            .unwrap()
    });
    let m = MACRO.captures(text)?;
    let name = &m[1];
    let args = split_args(&text[m.get(0)?.end()..]);
    let (dest, at) = if name.starts_with("write") { (Dest::Stream, 1) } else { (Dest::Console, 0) };
    let mut s = match args.get(at).map(|a| a.trim()) {
        Some(f) => {
            let lit = f.strip_prefix('"')?.strip_suffix('"')?;
            rust_pieces(&unescape(lit, true), &args[at + 1..])?
        }
        None if name.ends_with("ln") => Vec::new(),
        None => return None,
    };
    if name.ends_with("ln") {
        push_text(&mut s, "\n");
    }
    Some((dest, s))
}

/// The top-level, comma-separated arguments of a call whose opening
/// parenthesis was just consumed, up to its closing one. Comments are
/// blanked; string and character literals are kept whole.
fn split_args(text: &str) -> Vec<String> {
    let mut args = vec![String::new()];
    let mut depth = 0usize;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        let cur = args.last_mut().unwrap();
        match c {
            '"' => {
                cur.push(c);
                while let Some(d) = chars.next() {
                    cur.push(d);
                    if d == '\\' {
                        cur.extend(chars.next());
                    } else if d == '"' {
                        break;
                    }
                }
            }
            '\'' => {
                // A character literal, not a Rust lifetime.
                let ahead: String = chars.clone().take(4).collect();
                let len = if let Some(esc) = ahead.strip_prefix('\\') {
                    esc.find('\'').map(|i| i + 2)
                } else {
                    ahead.chars().nth(1).filter(|&e| e == '\'').map(|_| 2)
                };
                cur.push(c);
                for _ in 0..len.unwrap_or(0) {
                    cur.extend(chars.next());
                }
            }
            '/' if chars.peek() == Some(&'/') => while chars.next_if(|&d| d != '\n').is_some() {},
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut prev = ' ';
                for d in chars.by_ref() {
                    if prev == '*' && d == '/' {
                        break;
                    }
                    prev = d;
                }
            }
            '(' | '[' | '{' => {
                depth += 1;
                cur.push(c);
            }
            ')' | ']' | '}' if depth == 0 => break,
            ')' | ']' | '}' => {
                depth -= 1;
                cur.push(c);
            }
            ',' if depth == 0 => args.push(String::new()),
            _ => cur.push(c),
        }
    }
    if args.last().is_some_and(|a| a.trim().is_empty()) {
        args.pop();
    }
    args
}

/// The string literals and identifiers of a C++ format argument
/// (`"a %" PRIu64 "\n"`), in order.
fn tokens(arg: &str) -> Vec<String> {
    static TOKEN: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#""(?:[^"\\]|\\.)*"|\w+|\S"#).unwrap());
    TOKEN.find_iter(arg).map(|m| m.as_str().to_string()).collect()
}

/// Decodes the common escapes of a string literal's body. A Rust `\` at
/// the end of a line also skips the next line's indentation.
fn unescape(lit: &str, rust: bool) -> String {
    let mut out = String::new();
    let mut chars = lit.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('0') => out.push('\0'),
            Some(e @ ('\\' | '"' | '\'')) => out.push(e),
            Some('\n') if rust => while chars.next_if(|d| d.is_whitespace()).is_some() {},
            Some('\n') => {}
            Some(e) => {
                out.push('\\');
                out.push(e);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Appends literal text, merging it with text before it.
fn push_text(pieces: &mut Vec<Piece>, s: &str) {
    match pieces.last_mut() {
        _ if s.is_empty() => {}
        Some(Piece::Text(t)) => t.push_str(s),
        _ => pieces.push(Piece::Text(s.to_string())),
    }
}

/// Splits a printf format into text and conversions, each conversion
/// spelled as the Rust spec that prints the same: `%u` and `%s` are `{}`,
/// `%#lx` is `{:#x}`, `%-8s` is `{:<8}`.
fn printf_pieces(s: &str) -> Option<Vec<Piece>> {
    static CONV: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"^%([-+ #0]*)(\d+|\*)?(?:\.(\d+|\*))?(?:hh|h|ll|l|j|z|t|L|q)?([diouxXeEfFgGcsp%])",
        )
        .unwrap()
    });
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(i) = rest.find('%') {
        push_text(&mut out, &rest[..i]);
        let c = CONV.captures(&rest[i..])?;
        rest = &rest[i + c[0].len()..];
        let conv = &c[4];
        if conv == "%" {
            push_text(&mut out, "%");
            continue;
        }
        let flags = &c[1];
        let mut spec = String::new();
        for (flag, rust) in [('-', "<"), ('+', "+"), ('#', "#"), ('0', "0")] {
            if flags.contains(flag) {
                spec.push_str(rust);
            }
        }
        spec.push_str(c.get(2).map_or("", |w| w.as_str()));
        if let Some(p) = c.get(3) {
            spec.push('.');
            spec.push_str(p.as_str());
        }
        if matches!(conv, "o" | "x" | "X" | "e" | "E" | "p") {
            spec.push_str(conv);
        }
        out.push(Piece::Arg { spec, wrapper: false });
    }
    push_text(&mut out, rest);
    Some(out)
}

/// Splits a Rust format string into text and placeholders, each with its
/// spec: `{}` and `{name}` are `""`, and a right alignment, the default for
/// numbers, is dropped. `kprintln!` also takes printf's types (`{:u}`,
/// `{:s}`, `{:cs}` for a C string); those that print as `{}` are dropped.
/// `args` are the positional arguments after the format string.
fn rust_pieces(s: &str, args: &[String]) -> Option<Vec<Piece>> {
    static WRAPPER: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^\s*(?:\w+::)*[A-Z]\w*\s*\(").unwrap());
    let mut out = Vec::new();
    let mut next = 0;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.next_if_eq(&'{').is_some() => push_text(&mut out, "{"),
            '}' if chars.next_if_eq(&'}').is_some() => push_text(&mut out, "}"),
            '{' => {
                let inner: String = chars.by_ref().take_while(|&d| d != '}').collect();
                let (name, spec) = inner.split_once(':').unwrap_or((&inner, ""));
                // `{}` takes the next positional argument, `{0}` the first;
                // an inline `{name}` is never a wrapper.
                let arg = match name.trim() {
                    "" => {
                        next += 1;
                        args.get(next - 1)
                    }
                    n => n.parse::<usize>().ok().and_then(|n| args.get(n)),
                };
                let wrapper = arg.is_some_and(|a| WRAPPER.is_match(a));
                let spec = spec.strip_prefix('>').unwrap_or(spec);
                let spec = ["cs", "z", "d", "i", "u", "s", "c", "f"]
                    .iter()
                    .find_map(|t| spec.strip_suffix(t))
                    .unwrap_or(spec);
                out.push(Piece::Arg { spec: spec.to_string(), wrapper });
            }
            '}' => return None,
            _ => push_text(&mut out, &c.to_string()),
        }
    }
    Some(out)
}

/// A format in Rust's syntax, for messages.
fn show(pieces: &[Piece]) -> String {
    pieces
        .iter()
        .map(|p| match p {
            Piece::Text(t) => t.escape_default().to_string().replace('{', "{{").replace('}', "}}"),
            Piece::Arg { spec, .. } if spec.is_empty() => "{}".to_string(),
            Piece::Arg { spec, .. } => format!("{{:{spec}}}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // printf conversions, `PRIu64` macros and a trailing newline agree with
    // the Rust placeholders and `writeln!`.
    #[test]
    fn translated_formats_agree() {
        let same = |c: &str, r: &str| assert!(same_output(c, r), "{c} / {r}");
        same(
            r#"fprintf(&outfile, "REASON (%s)\n\n", reason_str);"#,
            r#"let _ = write!(outfile, "REASON ({reason_str})\n\n");"#,
        );
        same(
            r#"fprintf(&outfile, "UPTIME (ms)\n%" PRIi64 "\n\n", t);"#,
            r#"let _ = write!(outfile, "UPTIME (ms)\n{}\n\n", t);"#,
        );
        same(
            r#"fprintf(&f, "n=%zu %lu %d%% flags %#lx mask %x\n", n, a, b, c, d);"#,
            r#"let _ = writeln!(f, "n={} {} {}% flags {:#x} mask {:x}", n, a, b, c, d);"#,
        );
        same(r#"fprintf(&outfile, "\n");"#, "let _ = writeln!(outfile);");
        same(r#"printf("free: %s\n", s.c_str());"#, r#"kprintln!("free: {:s}", s);"#);
        // A wrapper printed with `{}` stands for `%#x`.
        same(
            "fprintf(&f,\n  \"counter:%#\" PRIx64 \" boot\\n\",\n  // A comment, too.\n  x);",
            "let _ = write!(\n    f,\n    \"counter:{} \\\n     boot\\n\",\n    CAltHex(x),\n);",
        );
        same(r#"printf("v=%#x\n", v);"#, r#"eprintln!("v={}", CAltHex(v));"#);
        same(r#"printf("%d %#x", n, v);"#, r#"eprint!("{1} {0}", CAltHex(v), n);"#);
    }

    // Changed wording, a lost newline, a dropped conversion or a different
    // radix is still reported.
    #[test]
    fn different_formats_are_reported() {
        let differs = |c: &str, r: &str| {
            let Some((_, Output::Differs(msg))) = compare(c, r) else {
                panic!("expected a difference: {c} / {r}");
            };
            msg
        };
        let msg = differs(
            r#"fprintf(&f, "panic buffer: %s\n", buf);"#,
            r#"let _ = writeln!(f, "panic buf: {}", buf);"#,
        );
        assert_eq!(
            msg,
            "format string differs: C++ prints \"panic buffer: {}\\n\", \
             Rust prints \"panic buf: {}\\n\""
        );
        differs(r#"fprintf(&f, "DLOG\n");"#, r#"let _ = write!(f, "DLOG");"#);
        differs(r#"printf("a=%d b=%d\n", a, b);"#, r#"kprintln!("a={} b=", a);"#);
        differs(r#"printf("mask %#x\n", m);"#, r#"kprintln!("mask {:x}", m);"#);
        // Only a wrapper call printed with `{}` may stand for another radix
        // or a width.
        differs(r#"printf("mask %#x\n", m);"#, r#"kprintln!("mask {}", m);"#);
        differs(r#"printf("%08x %-8s", m, s);"#, r#"print!("{} {}", m, s);"#);
        differs(r#"printf("%#x", m);"#, r#"print!("{m}");"#);
        differs(r#"printf("%#x %d", m, CAltHex(n));"#, r#"print!("{} {}", m, CAltHex(n));"#);
    }

    // Only a printf-family call and a Rust formatting macro printing to the
    // same place are compared.
    #[test]
    fn other_calls_are_not_output() {
        assert_eq!(compare(r#"printf("a\n");"#, r#"writeln!(f, "a");"#), None);
        assert_eq!(compare(r#"fprintf(&f, "a\n");"#, r#"kprintln!("a");"#), None);
        assert_eq!(compare(r#"puts("a");"#, r#"kprintln!("a");"#), None);
        assert_eq!(compare(r#"printf("a\n");"#, r#"outfile.write(b"a\n");"#), None);
        assert_eq!(compare(r#"printf("%s", x);"#, r#"kprintln!(concat!("{}", "\n"), x);"#), None);
    }
}
