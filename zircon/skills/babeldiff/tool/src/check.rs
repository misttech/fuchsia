//! Compares aligned units and reports where the Rust does something
//! different from the C++.

use crate::align::{Pair, similarity};
use crate::model::{Function, Lang, Ret, Unit, UnitKind};
use crate::normalize::lcs_len;

/// How much a finding matters.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// A behavioral difference or something missing: a different error
    /// code, lock, control flow step, call, or a lost comment.
    Issue,
    /// A difference worth a look that is often just translation, such as
    /// reworded comments or different helper calls.
    Note,
}

impl Severity {
    pub fn marker(self) -> char {
        match self {
            Severity::Issue => '!',
            Severity::Note => '~',
        }
    }
}

/// How a row of the side-by-side view is marked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Marker {
    /// Aligned and equivalent.
    Same,
    /// Aligned, with a note.
    Note,
    /// Aligned, with an issue.
    Issue,
    /// Only in C++.
    CppOnly,
    /// Only in Rust.
    RustOnly,
}

impl Marker {
    pub fn symbol(self) -> char {
        match self {
            Marker::Same => '=',
            Marker::Note => '~',
            Marker::Issue => '!',
            Marker::CppOnly => '<',
            Marker::RustOnly => '>',
        }
    }
}

/// What a finding is about. Reviewers and agents can filter on it, and each
/// maps to the part of the porting rubric it checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Category {
    /// A comment lost, added or reworded.
    Comment,
    /// Error codes, error propagation, or success versus failure.
    ErrorPath,
    /// Locks taken or released.
    Lock,
    /// Branches, loops, returns and other control flow.
    ControlFlow,
    /// Calls one side makes and the other doesn't.
    Call,
    /// Assertions.
    Assert,
    /// Trace and debug printing.
    Trace,
    /// The same steps in a different order.
    Order,
    /// How the two sides were paired.
    Pairing,
    /// Memory ordering of atomic operations.
    Atomic,
    /// Constants, flags and masks one side uses and the other doesn't.
    Value,
    /// Unsafe code the C++ didn't need.
    Unsafe,
    /// Code compiled only in some configurations on one side.
    Conditional,
}

impl Category {
    /// Short, stable name for output formats.
    pub fn name(self) -> &'static str {
        match self {
            Category::Comment => "comment",
            Category::ErrorPath => "error-path",
            Category::Lock => "lock",
            Category::ControlFlow => "control-flow",
            Category::Call => "call",
            Category::Assert => "assert",
            Category::Trace => "trace",
            Category::Order => "order",
            Category::Pairing => "pairing",
            Category::Atomic => "atomic",
            Category::Value => "value",
            Category::Unsafe => "unsafe",
            Category::Conditional => "conditional",
        }
    }

    /// The part of Zircon's C++ to Rust porting rubric the finding relates to.
    pub fn rubric(self) -> &'static str {
        match self {
            Category::Comment => "comment parity (rubric 3.15, pitfall 22)",
            Category::ErrorPath => "behavioral parity of error paths",
            Category::Lock => "locking parity (rubric 3.4)",
            Category::ControlFlow | Category::Call | Category::Order | Category::Value => {
                "direct translation"
            }
            Category::Assert => "assertions (pitfall 23)",
            Category::Trace => "trace parity (rubric 3.9, pitfall 16)",
            Category::Pairing => "pairing",
            Category::Atomic => "atomics and memory ordering",
            Category::Unsafe => "safe facades over unsafe code",
            Category::Conditional => "conditional compilation stays conditional",
        }
    }
}

/// A finding attached to a row.
#[derive(Clone, Debug, PartialEq)]
pub struct Note {
    pub severity: Severity,
    pub category: Category,
    pub message: String,
}

impl Note {
    pub fn new(severity: Severity, category: Category, message: impl Into<String>) -> Note {
        Note { severity, category, message: message.into() }
    }
}

#[derive(Clone, Debug)]
pub struct Finding {
    pub severity: Severity,
    pub category: Category,
    pub cpp_line: Option<usize>,
    pub rust_line: Option<usize>,
    /// The file of the C++ line when it isn't the function's own, such as a
    /// doc comment in a header.
    pub cpp_file: Option<String>,
    pub message: String,
}

#[derive(Clone, Debug)]
pub struct Row {
    pub cpp: Option<usize>,
    pub rust: Option<usize>,
    pub marker: Marker,
    pub notes: Vec<Note>,
}

/// Builds the rows and findings for an aligned pair of functions.
pub fn check(cpp: &Function, rust: &Function, pairs: &[Pair]) -> (Vec<Row>, Vec<Finding>) {
    check_with(cpp, rust, pairs, &Context::default())
}

/// What else [`check_with`] may consult.
#[derive(Default)]
pub struct Context<'a> {
    /// Rust units accounted for elsewhere (by a C++ override folded into
    /// the same function), not reported as only in Rust.
    pub claimed: &'a [usize],
    /// Functions the Rust calls that code may have moved into: `cpp_*`
    /// helpers the change added, and other Rust functions. C++ found there
    /// was moved, not dropped.
    pub elsewhere: &'a [&'a Function],
    /// Constants the changed files define.
    pub values: Option<&'a crate::values::Values>,
}

/// Like [`check`], with more to go on.
pub fn check_with(
    cpp: &Function,
    rust: &Function,
    pairs: &[Pair],
    ctx: &Context,
) -> (Vec<Row>, Vec<Finding>) {
    let claimed = ctx.claimed;
    let rescued = rescue(pairs, cpp, rust);
    let pairs = rescued.as_slice();
    // Control flow of a kind that both sides have as much of was most
    // likely rewritten (a `switch` as `matches!`, a clamp as `min`), not
    // dropped or added.
    let flow_balanced = |k: UnitKind| {
        let g = |u: &Unit| flow_group(u.kind);
        let (a, b) = (
            cpp.units.iter().filter(|u| g(u) == flow_group(k)).count(),
            rust.units.iter().filter(|u| g(u) == flow_group(k)).count(),
        );
        a == b
    };
    let (cpp_names, rust_names) = (Tokens::of(cpp), Tokens::of(rust));
    let vc = ValueCtx::new(ctx);
    let mut rows = Vec::new();
    let unmatched_c: Vec<usize> =
        pairs.iter().filter(|p| p.rust.is_none()).filter_map(|p| p.cpp).collect();
    let unmatched_r: Vec<usize> =
        pairs.iter().filter(|p| p.cpp.is_none()).filter_map(|p| p.rust).collect();

    for p in pairs {
        let mut notes: Vec<Note> = Vec::new();
        let marker = match (p.cpp, p.rust) {
            (Some(i), Some(j)) => {
                let output = output_diff(i, j, cpp, rust);
                compare(&cpp.units[i], &rust.units[j], output, ctx.elsewhere, &mut notes);
                unadapted_comment_idents(&cpp.units[i], &rust.units[j], cpp, rust, &mut notes);
                condition_diff(i, j, cpp, rust, &mut notes);
                ordering_diff(
                    &unit_text(cpp, &cpp.units[i]),
                    &unit_text(rust, &rust.units[j]),
                    cpp.units[i].features.asserts,
                    &mut notes,
                );
                // Constants passed to a call only one side makes, or tested
                // by a condition already reported as different, are part
                // of that difference.
                let explained = notes.iter().any(|n| {
                    n.category == Category::Call || n.message.starts_with("condition reads")
                });
                lock_by_callback(&cpp.units[i], &rust.units[j], cpp, rust, &mut notes);
                let cases =
                    cpp.units[i].kind == UnitKind::Case || rust.units[j].kind == UnitKind::Case;
                if !cases {
                    value_diff(
                        &unit_text(cpp, &cpp.units[i]),
                        &unit_text(rust, &rust.units[j]),
                        &cpp_names,
                        &rust_names,
                        explained,
                        &vc,
                        &mut notes,
                    );
                }
                if notes.iter().any(|n| n.severity == Severity::Issue) {
                    Marker::Issue
                } else if notes.is_empty() {
                    Marker::Same
                } else {
                    Marker::Note
                }
            }
            (Some(i), None) => {
                let u = &cpp.units[i];
                let moved =
                    find_moved(u, &rust.units, &unmatched_r).map(|j| rust.units[j].start_line);
                if u.kind == UnitKind::Comment && restates_signature(u, cpp) {
                    notes.push(Note::new(
                        Severity::Note,
                        Category::Comment,
                        "comment only in C++; it restates the function's name",
                    ));
                } else if u.kind == UnitKind::Comment && u.features.still_in_cpp {
                    notes.push(Note::new(
                        Severity::Note,
                        Category::Comment,
                        "comment only in C++; it is still in the C++ after the change",
                    ));
                } else if u.kind == UnitKind::Comment && is_banner(u) {
                    notes.push(Note::new(
                        Severity::Note,
                        Category::Comment,
                        "comment only in C++; it labels a section",
                    ));
                } else if let Some(g) = (u.kind == UnitKind::Comment)
                    .then(|| ctx.elsewhere.iter().find(|g| verbatim_in(u, g)))
                    .flatten()
                {
                    notes.push(Note::new(
                        Severity::Note,
                        Category::Comment,
                        format!("comment only in C++; it is in {}, which the Rust calls", g.base),
                    ));
                } else if u.kind == UnitKind::Comment && words_kept(u, rust) {
                    notes.push(Note::new(
                        Severity::Note,
                        Category::Comment,
                        "comment only in C++, but the Rust comments carry its words (merged or reworded)",
                    ));
                } else if !u.features.plumbing && u.kind != UnitKind::Signature {
                    // C++ `status = foo(); if (status != ZX_OK) return status; return ZX_OK;`
                    // is `foo()` in Rust when returning Result.
                    let ok_after_status = u.kind == UnitKind::Return
                        && u.features.ret == Some(Ret::Ok)
                        && rows.last().is_some_and(|r: &Row| {
                            r.rust.is_some_and(|j| {
                                rust.units[j].features.ret == Some(Ret::Status)
                                    || rust.units[j].features.propagates
                            })
                        });
                    if !ok_after_status {
                        let n = soften_if_called(only_in(u, "C++", "Rust", moved), u, rust);
                        let n = soften_balanced(n, u, flow_balanced(u.kind));
                        notes.push(soften_moved(n, u, ctx.elsewhere));
                    }
                }
                // The Rust holds the same lock over a scope of its own.
                if !u.features.locks.is_empty() {
                    let same = rust
                        .units
                        .iter()
                        .find(|r| u.features.locks.iter().all(|l| r.features.locks.contains(l)));
                    if let Some(r) = same {
                        for n in notes.iter_mut().filter(|n| n.severity == Severity::Issue) {
                            n.severity = Severity::Note;
                            n.message = format!(
                                "{}; the Rust takes the same lock at line {}",
                                n.message, r.start_line
                            );
                        }
                    }
                }
                one_sided_constant(&unit_text(cpp, u), u, "C++", &rust_names, &vc, &mut notes);
                Marker::CppOnly
            }
            (None, Some(j)) => {
                let u = &rust.units[j];
                let moved =
                    find_moved(u, &cpp.units, &unmatched_c).map(|i| cpp.units[i].start_line);
                // Safety comments, doc comments on `pub` functions, a test's
                // name, and plumbing are expected additions, not findings.
                let expected = claimed.contains(&j)
                    || u.kind == UnitKind::Comment
                        && (u.features.safety
                            || rust.is_pub && new_doc(rust, j)
                            || names_test(rust, j, cpp))
                    || u.features.lock_plumbing
                    || u.features.plumbing;
                // A doc comment for a C++ function that had none is new
                // text for a reviewer to read, not a lost translation.
                let added_doc = new_doc(rust, j) && !has_doc(cpp);
                // `return Foo();` in C++ is `foo()?; Ok(())` in Rust.
                let ok_after_status = u.kind == UnitKind::Return
                    && u.features.ret == Some(Ret::Ok)
                    && rows.last().is_some_and(|r: &Row| {
                        r.cpp.is_some_and(|i| cpp.units[i].features.ret == Some(Ret::Status))
                    });
                // One note per doc block, though a blank `///` line splits
                // it into several comment units.
                let block_start = j == 0 || rust.units[j - 1].kind != UnitKind::Comment;
                // Conversions add no comments but SAFETY ones: a comment
                // whose words aren't in the C++ is new text.
                let carried = u.kind == UnitKind::Comment
                    && (words_kept(u, cpp)
                        || ctx.values.is_some_and(|v| v.in_cpp_comments(&u.features.comment)));
                if added_doc && !expected {
                    if block_start {
                        notes.push(if carried {
                            Note::new(
                                Severity::Note,
                                Category::Comment,
                                "doc comment only in Rust; its words are in a C++ comment elsewhere in the change",
                            )
                        } else {
                            Note::new(
                                Severity::Issue,
                                Category::Comment,
                                "doc comment added in Rust where the C++ function has none; a conversion adds no comments other than SAFETY",
                            )
                        });
                    }
                } else if u.kind == UnitKind::Comment
                    && !expected
                    && !carried
                    && ctx.values.is_some()
                {
                    notes.push(Note::new(
                        Severity::Issue,
                        Category::Comment,
                        "comment added in Rust; a conversion adds no comments other than SAFETY",
                    ));
                } else if !expected && !ok_after_status {
                    let n = soften_if_called(only_in(u, "Rust", "C++", moved), u, cpp);
                    notes.push(soften_balanced(n, u, flow_balanced(u.kind)));
                }
                // A plain binding (`let dr7 = X86_DR7_MASK;`) is still a
                // value the C++ may not have.
                let lost_value = !claimed.contains(&j) && u.kind != UnitKind::Comment;
                if lost_value {
                    one_sided_constant(&unit_text(rust, u), u, "Rust", &cpp_names, &vc, &mut notes);
                }
                Marker::RustOnly
            }
            (None, None) => continue,
        };
        rows.push(Row { cpp: p.cpp, rust: p.rust, marker, notes });
    }
    one_finding_per_check(&mut rows, cpp, rust);
    switch_as_if_chain(&mut rows, cpp, rust);
    merged_checks(&mut rows, cpp, rust);
    fallible_alloc(&mut rows, cpp, rust);
    assert_semantics(&mut rows, cpp, rust);
    exhaustive_match(&mut rows, cpp, rust);
    conditional_compilation(&mut rows, cpp, rust);
    dedupe_values(&mut rows);
    group_runs(&mut rows, cpp, rust);
    let findings = refresh(&mut rows, cpp, rust);
    (rows, findings)
}

/// Re-marks rows after their notes changed and lists their findings.
pub fn refresh(rows: &mut [Row], cpp: &Function, rust: &Function) -> Vec<Finding> {
    for r in rows.iter_mut() {
        if matches!(r.marker, Marker::Same | Marker::Note | Marker::Issue) {
            r.marker = if r.notes.iter().any(|n| n.severity == Severity::Issue) {
                Marker::Issue
            } else if r.notes.is_empty() {
                Marker::Same
            } else {
                Marker::Note
            };
        }
    }
    findings_of(rows, cpp, rust)
}

/// A C++ guard on the stack against a Rust closure that runs under the same
/// lock because it is passed to a callback (`with_chain_lock(t, f)`): the
/// same lock, taken a different way.
fn lock_by_callback(a: &Unit, b: &Unit, cpp: &Function, rust: &Function, notes: &mut Vec<Note>) {
    static LET_CLOSURE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\blet\s+(?:mut\s+)?\w+\s*(?::[^=]*)?=\s*(?:move\s+)?\|").unwrap()
    });
    if a.features.locks.is_empty() || a.features.locks != b.features.locks {
        return;
    }
    let guard = unit_text(cpp, a).contains("Guard");
    if guard && LET_CLOSURE.is_match(&unit_text(rust, b)) {
        notes.push(Note::new(
            Severity::Note,
            Category::Lock,
            "same lock, but the Rust takes it by passing this closure to a callback instead of holding a guard like the C++; a guard type would keep the Rust shaped like the C++",
        ));
    }
}

/// Flags aligned Rust comments that still use C++ trailing-underscore member
/// names (`eviction_trigger_`) or `kPascalCase` constants (`kOutOfMemory`)
/// copied verbatim from the C++ comment when the Rust code uses Rust names.
fn unadapted_comment_idents(
    a: &Unit,
    b: &Unit,
    cpp: &Function,
    rust: &Function,
    notes: &mut Vec<Note>,
) {
    static CPP_IDENT: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\b([a-z][a-z0-9_]*_|k[A-Z][A-Za-z0-9]+)\b").unwrap()
    });
    if a.kind != UnitKind::Comment || b.kind != UnitKind::Comment {
        return;
    }
    let cpp_text = unit_text(cpp, a);
    let rust_text = unit_text(rust, b);
    let rust_code: String = rust
        .units
        .iter()
        .filter(|u| u.kind != UnitKind::Comment)
        .map(|u| unit_text(rust, u))
        .collect::<Vec<_>>()
        .join("\n");
    let mut unadapted: Vec<String> = Vec::new();
    for cap in CPP_IDENT.captures_iter(&rust_text) {
        let ident = &cap[1];
        let word_re = regex::Regex::new(&format!(r"\b{}\b", regex::escape(ident))).unwrap();
        if word_re.is_match(&cpp_text)
            && !word_re.is_match(&rust_code)
            && !unadapted.iter().any(|x| x == ident)
        {
            unadapted.push(ident.to_string());
        }
    }
    if !unadapted.is_empty() {
        let list = unadapted.iter().map(|s| format!("`{s}`")).collect::<Vec<_>>().join(", ");
        notes.push(Note::new(
            Severity::Issue,
            Category::Comment,
            format!(
                "comment still uses C++ identifier{} {list}; update to the Rust name",
                if unadapted.len() == 1 { "" } else { "s" }
            ),
        ));
    }
}

const EXHAUSTIVE_DEFAULT_WHY: &str = "the Rust match covers every case, so it needs no default";

/// A C++ `default:` that only panics or returns a fallback value, against a
/// Rust `match` with no wildcard arm: the compiler checks that the match
/// covers every value, so the Rust needs no default.
fn exhaustive_match(rows: &mut [Row], cpp: &Function, rust: &Function) {
    let has_match = rust.units.iter().any(|u| u.kind == UnitKind::Switch)
        && rust.lines.iter().any(|l| l.contains("match "));
    let wildcard = rust
        .units
        .iter()
        .any(|u| u.kind == UnitKind::Case && u.features.idents.iter().any(|i| i == "default"));
    if !has_match || wildcard {
        return;
    }
    let all_cases_matched = !rows.iter().any(|r| {
        r.marker == Marker::CppOnly
            && row_unit(r, cpp, true).is_some_and(|v| {
                v.kind == UnitKind::Case && !v.features.idents.iter().any(|i| i == "default")
            })
    });
    let mut k = 0;
    while k < rows.len() {
        let Some(u) =
            (rows[k].marker == Marker::CppOnly).then(|| row_unit(&rows[k], cpp, true)).flatten()
        else {
            k += 1;
            continue;
        };
        if u.kind != UnitKind::Case || !u.features.idents.iter().any(|i| i == "default") {
            k += 1;
            continue;
        }
        // The default's body: C++-only rows below it, up to the next case.
        let depth = u.depth;
        let mut end = k + 1;
        while end < rows.len()
            && rows[end].marker == Marker::CppOnly
            && row_unit(&rows[end], cpp, true)
                .is_some_and(|v| v.depth > depth || v.kind == UnitKind::Comment)
        {
            end += 1;
        }
        let unreachable_fallback = (k + 1..end).all(|m| {
            row_unit(&rows[m], cpp, true).is_some_and(|v| {
                v.kind == UnitKind::Comment
                    || v.kind == UnitKind::Break
                    || v.features.calls.iter().any(|c| c == "panic" || c == "assert")
                    || (all_cases_matched
                        && v.kind == UnitKind::Return
                        && v.features.ret == Some(Ret::Value)
                        && v.features.calls.is_empty()
                        && v.features.locks.is_empty()
                        && v.features.errors.is_empty())
            })
        });
        if unreachable_fallback {
            demote(&mut rows[k], EXHAUSTIVE_DEFAULT_WHY);
            for r in &mut rows[k + 1..end] {
                if row_unit(r, cpp, true).is_some_and(|v| v.kind != UnitKind::Comment) {
                    r.notes.clear();
                }
            }
        }
        k = end;
    }
}

/// Rows for conditional compilation. C++ `#if` code must stay conditional
/// in Rust (`#[cfg]` or `cfg!`): a `#if` with no Rust counterpart means
/// the Rust either always runs that code or never does, and a run-time
/// `if` on a build setting (`__has_feature(safe_stack)`) can't stand in
/// for it. A run-time `if` on a plain constant (`#if LK_DEBUGLEVEL > 1`)
/// compiles to the same code, so that is only a note.
fn conditional_compilation(rows: &mut [Row], cpp: &Function, rust: &Function) {
    let directive = |f: &Function, u: &Unit| f.line(u.start_line).trim().to_string();
    let is_else = |f: &Function, u: &Unit| {
        let d = directive(f, u);
        d.starts_with("#else") || d.starts_with("} else")
    };
    // A condition on the compiler or build configuration, as opposed to a
    // comparison of a named constant.
    let build_setting = |d: &str| {
        d.starts_with("#ifdef")
            || d.starts_with("#ifndef")
            || d.contains("defined")
            || d.contains("__")
    };
    // Lines a C++ directive guards: up to the next unit back at its depth.
    let span = |u: &Unit, k: usize| {
        let end = cpp.units[k + 1..]
            .iter()
            .take_while(|v| v.depth > u.depth)
            .map(|v| v.end_line)
            .max()
            .unwrap_or(u.end_line);
        if end > u.start_line + 1 {
            format!("lines {}-{}", u.start_line + 1, end)
        } else {
            format!("line {}", end)
        }
    };
    // Rust that mentions the condition's names outside any cfg: the stub
    // or run-time test the `#if` became. The one nearest `near` (the Rust
    // line reached so far) wins.
    let mention = |u: &Unit, near: usize| -> Option<(usize, String)> {
        let words: Vec<&String> = u.features.idents.iter().filter(|w| w.len() >= 4).collect();
        rust.units
            .iter()
            .filter(|v| v.kind != UnitKind::Cfg && v.kind != UnitKind::Comment)
            .filter_map(|v| {
                let t = directive(rust, v);
                let flat = t.replace('_', "").to_ascii_lowercase();
                words.iter().any(|w| flat.contains(w.as_str())).then_some((v.start_line, t))
            })
            .min_by_key(|(line, _)| line.abs_diff(near))
    };
    let mut near = rust.start_line;
    for r in rows.iter_mut() {
        if let Some(j) = r.rust {
            near = rust.units[j].start_line;
        }
        let cu = r.cpp.map(|k| (k, &cpp.units[k]));
        let ru = r.rust.map(|k| &rust.units[k]);
        match (cu, ru) {
            (Some((k, u)), None) if u.kind == UnitKind::Cfg => {
                let d = directive(cpp, u);
                r.notes = if is_else(cpp, u) {
                    vec![Note::new(
                        Severity::Note,
                        Category::Conditional,
                        format!("`{d}` branch only in C++"),
                    )]
                } else {
                    let mut msg = format!(
                        "C++ compiles {} only when `{d}`; the Rust has no `#[cfg]` or `cfg!` for it, so it runs that code unconditionally or not at all",
                        span(u, k)
                    );
                    if let Some((line, text)) = mention(u, near) {
                        let text: String = text.chars().take(60).collect();
                        msg.push_str(&format!(
                            "; Rust line {line} (`{text}`) is not a compile-time condition"
                        ));
                    }
                    vec![Note::new(Severity::Issue, Category::Conditional, msg)]
                };
            }
            (None, Some(v)) if v.kind == UnitKind::Cfg && !is_else(rust, v) => {
                r.notes = vec![Note::new(
                    Severity::Note,
                    Category::Conditional,
                    format!(
                        "Rust compiles this only when `{}`; the C++ has no `#if` for it",
                        directive(rust, v)
                    ),
                )];
            }
            (Some((_, u)), Some(v)) if u.kind == UnitKind::Cfg && v.kind == UnitKind::Cfg => {
                r.notes.clear();
            }
            (Some((_, u)), Some(v)) if u.kind == UnitKind::Cfg => {
                let d = directive(cpp, u);
                r.notes = vec![if build_setting(&d) {
                    Note::new(
                        Severity::Issue,
                        Category::Conditional,
                        format!(
                            "C++ decides this at compile time (`{d}`), but the Rust tests it at run time (`{}`); use `#[cfg]` or `cfg!`",
                            directive(rust, v)
                        ),
                    )
                } else {
                    Note::new(
                        Severity::Note,
                        Category::Conditional,
                        format!(
                            "C++ decides this at compile time (`{d}`); the Rust tests the same constant at run time, which compiles to the same code"
                        ),
                    )
                }];
            }
            (Some((_, u)), Some(v)) if v.kind == UnitKind::Cfg => {
                r.notes = vec![Note::new(
                    Severity::Note,
                    Category::Conditional,
                    format!(
                        "the Rust decides this at compile time (`{}`), where the C++ tests `{}` at run time",
                        directive(rust, v),
                        directive(cpp, u)
                    ),
                )];
            }
            _ => {}
        }
    }
}

/// Lines up a statement only in C++ with a statement only in Rust in the
/// same gap between aligned rows when both make the same call, or print
/// the same format (`fprintf` as `write!`): the alignment scored them apart
/// (a declaration split from its check, a macro spelled as a method), but
/// they are one statement, and comparing them says more than two one-sided
/// findings. Statements with an aligned row between them stay apart, so a
/// reordering is still reported.
fn rescue(pairs: &[Pair], cpp: &Function, rust: &Function) -> Vec<Pair> {
    let mut out: Vec<Pair> = pairs.to_vec();
    let generic = |c: &str| matches!(c, "assert" | "trace" | "print" | "len" | "min" | "max");
    let stmt = |u: &Unit| matches!(u.kind, UnitKind::Stmt | UnitKind::Return);
    let shares = |a: &Unit, b: &Unit| {
        stmt(a)
            && stmt(b)
            && (a
                .features
                .calls
                .iter()
                .any(|x| !generic(x) && b.features.calls.iter().any(|y| call_like(x, y)))
                || crate::printf::same_output(&unit_text(cpp, a), &unit_text(rust, b)))
    };
    let aligned = |p: &Pair| {
        p.cpp.is_some_and(|i| cpp.units[i].kind != UnitKind::Comment) && p.rust.is_some()
    };
    for i in 0..out.len() {
        let (Some(c), None) = (out[i].cpp, out[i].rust) else {
            continue;
        };
        // The gap of one-sided entries around `i`.
        let mut lo = i;
        while lo > 0 && !aligned(&out[lo - 1]) {
            lo -= 1;
        }
        let mut hi = i;
        while hi + 1 < out.len() && !aligned(&out[hi + 1]) {
            hi += 1;
        }
        let found = (lo..=hi).find(|&j| {
            out[j].cpp.is_none()
                && out[j].rust.is_some_and(|r| shares(&cpp.units[c], &rust.units[r]))
        });
        if let Some(j) = found {
            out[i].rust = out[j].rust.take();
        }
    }
    out.retain(|p| p.cpp.is_some() || p.rust.is_some());
    out
}

/// Turns a one-sided issue on a row into a note with more to say.
fn demote(r: &mut Row, why: &str) {
    for n in &mut r.notes {
        if n.severity == Severity::Issue && n.category != Category::Comment {
            n.severity = Severity::Note;
            n.message = format!("{}; {why}", n.message);
        }
    }
}

/// The unit of a row on one side.
fn row_unit<'f>(r: &Row, f: &'f Function, cpp_side: bool) -> Option<&'f Unit> {
    if cpp_side { r.cpp } else { r.rust }.map(|k| &f.units[k])
}

/// A C++ `switch` written as an if/else-if chain in Rust (or a `match`
/// written as one in C++): once its cases line up with the branches, the
/// `switch` header on its own is a rewrite, not a change in behavior.
fn switch_as_if_chain(rows: &mut [Row], cpp: &Function, rust: &Function) {
    let crossed = rows.iter().any(|r| {
        let (Some(i), Some(j)) = (r.cpp, r.rust) else {
            return false;
        };
        let (a, b) = (&cpp.units[i], &rust.units[j]);
        (a.kind == UnitKind::Case) != (b.kind == UnitKind::Case)
            && crate::align::case_vs_branch(a, b).is_some()
    });
    if !crossed {
        return;
    }
    for r in rows.iter_mut() {
        let u = match r.marker {
            Marker::CppOnly => row_unit(r, cpp, true),
            Marker::RustOnly => row_unit(r, rust, false),
            _ => None,
        };
        // The branches of the chain name what the cases test.
        let (other, other_cases) =
            if r.marker == Marker::CppOnly { (rust, false) } else { (cpp, true) };
        let case_names: Vec<&String> = other
            .units
            .iter()
            .filter(|v| (v.kind == UnitKind::Case) == other_cases)
            .filter(|v| {
                v.kind == UnitKind::Case || matches!(v.kind, UnitKind::If | UnitKind::ElseIf)
            })
            .flat_map(|v| v.features.names.iter())
            .collect();
        let part_of_chain = |u: &Unit| match u.kind {
            UnitKind::Switch | UnitKind::Case => true,
            UnitKind::If | UnitKind::ElseIf => {
                u.features.names.iter().any(|n| case_names.contains(&n))
            }
            UnitKind::Else => true,
            _ => false,
        };
        if u.is_some_and(part_of_chain) {
            demote(r, "the switch and the if/else-if chain dispatch the same way");
        }
    }
}

/// The error an `if` returns: the error of the return right after it.
fn if_returns(f: &Function, k: usize) -> Option<String> {
    let next = f.units.get(k + 1..)?.iter().find(|u| u.kind != UnitKind::Comment)?;
    match (&next.kind, &next.features.ret) {
        (UnitKind::Return, Some(Ret::Error(e))) if next.depth > f.units[k].depth => Some(e.clone()),
        _ => None,
    }
}

/// Several C++ checks returning the same error, written as one Rust `if a
/// || b || c`: the C++ checks the Rust merged are not missing.
fn merged_checks(rows: &mut [Row], cpp: &Function, rust: &Function) {
    let merged: Vec<(usize, String)> = rust
        .units
        .iter()
        .enumerate()
        .filter(|(_, u)| {
            matches!(u.kind, UnitKind::If | UnitKind::ElseIf) && u.features.conjuncts.len() >= 2
        })
        .filter_map(|(j, _)| if_returns(rust, j).map(|e| (j, e)))
        .collect();
    if merged.is_empty() {
        return;
    }
    let mut folded: Vec<(usize, usize)> = Vec::new();
    for (k, r) in rows.iter().enumerate() {
        let Some(i) = r.cpp.filter(|_| r.rust.is_none()) else {
            continue;
        };
        let u = &cpp.units[i];
        if !matches!(u.kind, UnitKind::If | UnitKind::ElseIf) || u.features.conjuncts.is_empty() {
            continue;
        }
        let Some(err) = if_returns(cpp, i) else {
            continue;
        };
        let hit = merged.iter().find(|(j, e)| {
            *e == err
                && u.features.conjuncts.iter().all(|c| {
                    !c.names.is_empty()
                        && rust.units[*j]
                            .features
                            .conjuncts
                            .iter()
                            .any(|d| c.names.iter().all(|n| d.names.contains(n)))
                })
        });
        if let Some((j, _)) = hit {
            folded.push((k, rust.units[*j].start_line));
        }
    }
    for (k, line) in folded {
        let why = format!("the Rust merges this check into the `||` condition at line {line}");
        demote(&mut rows[k], &why);
        // And the C++ return that goes with it.
        if let Some(next) = rows[k + 1..].iter_mut().find(|r| r.cpp.is_some()) {
            if next.rust.is_none()
                && next.cpp.is_some_and(|i| cpp.units[i].kind == UnitKind::Return)
            {
                demote(next, &why);
            }
        }
    }
}

/// How a failed allocation is handled.
#[derive(Clone, Copy, Debug, PartialEq)]
enum OnAllocFailure {
    /// An assertion or panic: `ASSERT_TRUE(ac.check())`, `unwrap_some!`.
    Assert,
    /// An early return: `if (!ac.check()) { return ...; }`, `let-else`, `?`.
    Return,
}

/// How the body of the `if`, `let-else` or `match` arm at `k` handles the
/// failure it tests for: the first return or panic in it.
fn failure_branch(f: &Function, k: usize) -> Option<OnAllocFailure> {
    let depth = f.units[k].depth;
    for u in f.units[k + 1..].iter().take_while(|u| u.depth > depth) {
        if u.kind == UnitKind::Return {
            return Some(OnAllocFailure::Return);
        }
        if u.features.asserts || u.features.calls.iter().any(|c| c == "panic") {
            return Some(OnAllocFailure::Assert);
        }
    }
    None
}

/// The byte index of the `)` closing the `(` at byte `open` of `text`.
fn closing_paren(text: &str, open: usize) -> Option<usize> {
    let mut depth = 0;
    for (k, c) in text[open..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + k);
                }
            }
            _ => {}
        }
    }
    None
}

/// The normalized name an allocation is stored in, from the code before it:
/// the binding of a `let` pattern, an assigned variable or a `reset` receiver.
fn alloc_binding(prefix: &str) -> Option<String> {
    static LET: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"\blet\s([^=]*)=").unwrap());
    // The pattern without its type annotation.
    static PATTERN: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"^(?:[^:]|::)*").unwrap());
    static IDENT: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"\b[a-z_]\w*").unwrap());
    static ASSIGN: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(\w+)\s*(?:\[[^\]]*\]\s*)?=(?:[^=>]|$)").unwrap()
    });
    static RESET: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(\w+)\s*(?:\.|->)\s*reset\s*\(\s*$").unwrap()
    });
    let name = if let Some(c) = LET.captures_iter(prefix).last() {
        let pattern = c.get(1).and_then(|p| PATTERN.find(p.as_str())).map_or("", |m| m.as_str());
        IDENT.find_iter(pattern).map(|m| m.as_str()).filter(|w| !matches!(*w, "mut" | "ref")).last()
    } else if let Some(c) = ASSIGN.captures_iter(prefix).last() {
        c.get(1).map(|m| m.as_str())
    } else {
        RESET.captures(prefix).and_then(|c| c.get(1)).map(|m| m.as_str())
    };
    name.map(crate::normalize::ident).filter(|n| !n.is_empty())
}

/// How the Rust allocation at unit `j` handles failure, and the name it is
/// stored in.
fn rust_alloc(rust: &Function, j: usize) -> Option<(OnAllocFailure, Option<String>)> {
    // kalloc's fallible `Box` constructors.
    static RUST_ALLOC: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(concat!(
            r"\b(?:try_new|try_new_uninit|try_new_zeroed|try_new_uninit_slice",
            r"|try_new_zeroed_slice|try_new_zeroed_uninit_slice|try_grow)",
            r"(?:_in)?\s*(?:::\s*<[^>]*>\s*)?\(",
        ))
        .unwrap()
    });
    // Conversions between a `try_*` call and its failure handling.
    static ADAPTER: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"^\s*\.\s*(?:ok|map_err|ok_or|ok_or_else)\s*\(").unwrap()
    });
    // `unwrap_some!(` with the allocation as its argument.
    static UNWRAP_SOME: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\bunwrap_some!\s*\(\s*[\w:<>\[\]\s]*$").unwrap()
    });
    // `let ... else`, `if let Err(..)` and `if let None`, whose body runs on
    // failure.
    static ON_ERR: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"^(?:let\b|(?:\}\s*)?(?:else\s+)?if\s+let\s+(?:Err|None)\b)").unwrap()
    });
    static ERR_ARM: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"^\s*(?:Err|None)\b").unwrap());
    let v = &rust.units[j];
    let text = code_only(&unit_text(rust, v));
    let m = RUST_ALLOC.find(&text)?;
    let mut rest = &text[closing_paren(&text, m.end() - 1)? + 1..];
    while let Some(a) = ADAPTER.find(rest) {
        rest = &rest[closing_paren(rest, a.end() - 1)? + 1..];
    }
    let rest = rest.trim_start();
    let prefix = &text[..m.start()];
    let on_failure = if rest.starts_with('?') {
        Some(OnAllocFailure::Return)
    } else if rest.starts_with(".unwrap()")
        || rest.starts_with(".expect(")
        || (rest.starts_with(')') && UNWRAP_SOME.is_match(prefix))
    {
        Some(OnAllocFailure::Assert)
    } else if matches!(v.kind, UnitKind::If | UnitKind::ElseIf)
        && (ON_ERR.is_match(text.trim_start())
            || rest.starts_with(".is_err()")
            || rest.starts_with(".is_none()"))
    {
        failure_branch(rust, j)
    } else if v.kind == UnitKind::Switch && rest.starts_with('{') {
        let arms = rust.units[j + 1..].iter().take_while(|u| u.depth > v.depth);
        (j + 1..)
            .zip(arms)
            .find(|(_, u)| {
                u.kind == UnitKind::Case
                    && u.depth == v.depth + 1
                    && ERR_ARM.is_match(&code_only(&unit_text(rust, u)))
            })
            .and_then(|(k, _)| failure_branch(rust, k))
    } else {
        None
    };
    Some((on_failure?, alloc_binding(prefix)))
}

/// The rows of a C++ allocation, its check, and the Rust allocation that
/// replaces both.
struct Folded {
    alloc_row: usize,
    check_row: usize,
    rust_row: usize,
    checker: String,
}

/// Demotes to notes the one-sided findings for a C++ `fbl::AllocChecker`
/// allocation, its `ac.check()`, and the Rust `try_*` allocation that
/// replaces both, when the Rust stores into the same name and handles
/// failure the same way (both assert, or both return).
fn fallible_alloc(rows: &mut [Row], cpp: &Function, rust: &Function) {
    // `make_unique<T>(&ac, ...)`, `MakeRefCountedChecked<T>(&ac, ...)` and
    // `new (&ac) T`, capturing the checker.
    static CPP_ALLOC: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(concat!(
            r"\b(?:make_unique|MakeRefCountedChecked)\s*<[^;]*?>\s*\(\s*&\s*(\w+)\s*[,)]",
            r"|\bnew\s*\(\s*&\s*(\w+)\s*\)",
        ))
        .unwrap()
    });
    static DECLARED: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"\bAllocChecker\s+(\w+)\b").unwrap());
    static CHECK: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"\b(\w+)\s*\.\s*check\s*\(\s*\)").unwrap());
    static NEGATED: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"!\s*(\w+)\s*\.\s*check\s*\(\s*\)").unwrap()
    });
    // An `if` testing `!ac.check()`.
    static IF_FAILED: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"^(?:\}\s*)?(?:else\s+)?if\s*\(\s*!\s*(\w+)\s*\.\s*check\s*\(\s*\)\s*\)")
            .unwrap()
    });
    let names = |re: &regex::Regex, text: &str, ac: &str| {
        re.captures_iter(text).any(|c| c.get(1).is_some_and(|m| m.as_str() == ac))
    };
    if !cpp.lines.iter().any(|l| l.contains("AllocChecker")) {
        return;
    }
    let cpp_text = code_only(&cpp.lines.join("\n"));
    let declared: std::collections::HashSet<&str> =
        DECLARED.captures_iter(&cpp_text).filter_map(|c| c.get(1)).map(|m| m.as_str()).collect();
    let mut folded: Vec<Folded> = Vec::new();
    for (i, u) in cpp.units.iter().enumerate() {
        let text = code_only(&unit_text(cpp, u));
        let Some(cap) = CPP_ALLOC.captures(&text) else {
            continue;
        };
        let Some(m) = cap.get(1).or(cap.get(2)) else {
            continue;
        };
        let ac = m.as_str().to_string();
        if !declared.contains(ac.as_str()) {
            continue;
        }
        let name = cap.get(0).and_then(|a| alloc_binding(&text[..a.start()]));
        // The check is the next unit, or this one when it folded in an
        // `if (!ac.check()) return ...;`.
        let c = if names(&CHECK, &text, &ac) {
            i
        } else {
            match cpp.units[i + 1..].iter().position(|v| v.kind != UnitKind::Comment) {
                Some(d) => i + 1 + d,
                None => continue,
            }
        };
        let cu = &cpp.units[c];
        let check_text = code_only(&unit_text(cpp, cu));
        if c != i && !names(&CHECK, &check_text, &ac) {
            continue;
        }
        // An `if` (or folded return) runs on failure only when it tests
        // `!ac.check()`.
        let on_failure = if cu.features.asserts {
            Some(OnAllocFailure::Assert)
        } else if matches!(cu.kind, UnitKind::If | UnitKind::ElseIf) {
            if names(&IF_FAILED, check_text.trim_start(), &ac) {
                failure_branch(cpp, c)
            } else {
                None
            }
        } else if c == i && cu.features.propagates && names(&NEGATED, &text, &ac) {
            Some(OnAllocFailure::Return)
        } else {
            None
        };
        let row_of = |k: usize| rows.iter().position(|r| r.cpp == Some(k));
        let (Some(on_failure), Some(ka), Some(kc)) = (on_failure, row_of(i), row_of(c)) else {
            continue;
        };
        // The Rust allocation is in the gap of one-sided rows around the
        // C++ allocation and check, or aligned with the check.
        let one_sided = |r: &Row| r.cpp.is_none() || r.rust.is_none();
        let mut lo = ka;
        while lo > 0 && one_sided(&rows[lo - 1]) {
            lo -= 1;
        }
        let mut hi = kc;
        while hi + 1 < rows.len() && one_sided(&rows[hi + 1]) {
            hi += 1;
        }
        // Each Rust allocation stands for one C++ allocation into the same
        // name; one that handles failure differently stays reported.
        let alloc = (lo..=hi)
            .filter(|k| !folded.iter().any(|f| f.rust_row == *k))
            .filter_map(|k| rows[k].rust.and_then(|j| rust_alloc(rust, j)).map(|a| (k, a)))
            .find(|(_, (_, binding))| *binding == name);
        if let Some((kr, _)) = alloc.filter(|(_, (rust_failure, _))| *rust_failure == on_failure) {
            folded.push(Folded { alloc_row: ka, check_row: kc, rust_row: kr, checker: ac });
        }
    }
    for Folded { alloc_row, check_row, rust_row, checker: ac } in folded {
        let (Some(i), Some(j)) = (rows[alloc_row].cpp, rows[rust_row].rust) else {
            continue;
        };
        let (cpp_line, rust_line) = (cpp.units[i].start_line, rust.units[j].start_line);
        let why = format!(
            "it is a fallible allocation checked by `{ac}.check()`, which the Rust makes and \
             checks in one statement at line {rust_line}"
        );
        for k in [alloc_row, check_row] {
            if rows[k].marker == Marker::CppOnly {
                demote(&mut rows[k], &why);
            }
        }
        if rows[rust_row].marker == Marker::RustOnly {
            let why = format!(
                "it is the fallible allocation the C++ makes at line {cpp_line} and checks with \
                 `{ac}.check()`"
            );
            demote(&mut rows[rust_row], &why);
        }
    }
}

/// The condition of an `if` as written, without `if`, parentheses and the
/// opening brace.
fn condition_text(f: &Function, u: &Unit) -> String {
    let t = unit_text(f, u);
    let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    let t = t.trim_start_matches("} ").trim_start_matches("else ");
    let t = t.strip_prefix("if").unwrap_or(t).trim();
    let t = t.trim_end_matches('{').trim();
    let t = if t.starts_with('(') && t.ends_with(')') { &t[1..t.len() - 1] } else { t };
    let t = t.trim();
    if t.chars().count() > 80 {
        format!("{}...", t.chars().take(77).collect::<String>())
    } else {
        t.to_string()
    }
}

/// An `if` on one side only whose body just returns: one finding for the
/// check, "Rust adds a check `x.is_null()` returning INVALID_ARGS", instead
/// of one for the `if` and one for the `return`.
fn one_finding_per_check(rows: &mut [Row], cpp: &Function, rust: &Function) {
    for k in 0..rows.len() {
        let marker = rows[k].marker;
        let (f, cpp_side) = match marker {
            Marker::CppOnly => (cpp, true),
            Marker::RustOnly => (rust, false),
            _ => continue,
        };
        let Some(u) = row_unit(&rows[k], f, cpp_side) else {
            continue;
        };
        if !matches!(u.kind, UnitKind::If | UnitKind::ElseIf)
            || !rows[k].notes.iter().any(|n| n.severity == Severity::Issue)
        {
            continue;
        }
        let idx = if cpp_side { rows[k].cpp } else { rows[k].rust }.unwrap();
        // The return must be the next unit of the `if`, on the same side
        // and one-sided too.
        let Some(next) = (k + 1..rows.len()).find(|&m| {
            row_unit(&rows[m], f, cpp_side).is_some_and(|v| v.kind != UnitKind::Comment)
        }) else {
            continue;
        };
        let Some(v) = row_unit(&rows[next], f, cpp_side) else {
            continue;
        };
        let vi = if cpp_side { rows[next].cpp } else { rows[next].rust }.unwrap();
        if rows[next].marker != marker
            || v.kind != UnitKind::Return
            || v.depth <= u.depth
            || vi != idx + 1 && f.units[idx + 1..vi].iter().any(|w| w.kind != UnitKind::Comment)
        {
            continue;
        }
        let what = match &v.features.ret {
            Some(Ret::Error(e)) => format!("returning {e}"),
            Some(r) => format!("returning {r}"),
            None => "returning".to_string(),
        };
        let cond = condition_text(f, u);
        let msg = if cpp_side {
            format!("C++ checks `{cond}` here, {what}; the Rust doesn't")
        } else {
            format!("Rust adds a check `{cond}`, {what}, that the C++ doesn't make")
        };
        let category = if matches!(v.features.ret, Some(Ret::Error(_)) | Some(Ret::Status)) {
            Category::ErrorPath
        } else {
            Category::ControlFlow
        };
        rows[k].notes.retain(|n| n.severity != Severity::Issue);
        rows[k].notes.insert(0, Note::new(Severity::Issue, category, msg));
        rows[next].notes.retain(|n| n.severity != Severity::Issue);
    }
}

/// Calls a function makes, by the words of their names, for matching
/// `IsUserStateSavedLocked` against `is_user_state_saved`.
pub(crate) fn call_like(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let (wa, wb) = (crate::normalize::words(a), crate::normalize::words(b));
    let within = |x: &[String], y: &[String]| x.len() >= 2 && x.iter().all(|w| y.contains(w));
    within(&wa, &wb) || within(&wb, &wa)
}

/// Assertions whose meaning changed:
/// - a call the C++ makes only inside `DEBUG_ASSERT` (debug builds only)
///   that the Rust makes unconditionally;
/// - a C++ `ASSERT(false)` or `PANIC` where the Rust returns an error.
fn assert_semantics(rows: &mut [Row], cpp: &Function, rust: &Function) {
    static PANIC: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\b(?:(?:ZX_)?(?:DEBUG_)?ASSERT(?:_MSG)?\s*\(\s*false\b|(?:ZX_)?PANIC(?:_UNIMPLEMENTED)?\b|__UNREACHABLE\b|__builtin_unreachable\b|panic\s*\()").unwrap()
    });
    static RUST_PANIC: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\b(?:panic|unreachable|todo|unimplemented)!|\bassert!\s*\(\s*false\b|\.expect\(|\.unwrap\(\)").unwrap()
    });
    let rust_code: Vec<(usize, String)> =
        rust.units.iter().enumerate().map(|(j, u)| (j, unit_text(rust, u))).collect();
    let rust_panics = rust_code.iter().any(|(_, t)| RUST_PANIC.is_match(&code_only(t)));
    for k in 0..rows.len() {
        let Some(i) = rows[k].cpp else { continue };
        let u = &cpp.units[i];
        if !u.features.asserts {
            continue;
        }
        let text = code_only(&unit_text(cpp, u));
        if text.trim_start().starts_with("DEBUG_ASSERT") {
            for c in u.features.calls.iter().filter(|c| c.as_str() != "assert") {
                let elsewhere_in_cpp =
                    cpp.units.iter().any(|v| !v.features.asserts && v.features.calls.contains(c));
                let in_rust_assert = rust.units.iter().any(|v| {
                    v.features.asserts && v.features.calls.iter().any(|d| call_like(c, d))
                });
                let unconditional = rust.units.iter().position(|v| {
                    !v.features.asserts && v.features.calls.iter().any(|d| call_like(c, d))
                });
                if let (false, false, Some(j)) = (elsewhere_in_cpp, in_rust_assert, unconditional) {
                    let v = &rust.units[j];
                    // The Rust statement making the call is this finding,
                    // not one of its own.
                    if let Some(m) = rows.iter().position(|r| r.rust == Some(j) && r.cpp.is_none())
                    {
                        demote(
                            &mut rows[m],
                            &format!(
                                "it makes the call C++ makes only inside DEBUG_ASSERT at line {}",
                                u.start_line
                            ),
                        );
                    }
                    rows[k].notes.retain(|n| !n.message.starts_with("only C++ asserts"));
                    rows[k].notes.push(Note::new(
                        Severity::Issue,
                        Category::Assert,
                        format!(
                            "C++ calls {c} only inside DEBUG_ASSERT, so only in debug builds; the Rust calls it unconditionally at line {}",
                            v.start_line
                        ),
                    ));
                    break;
                }
            }
        }
        if PANIC.is_match(&text) && !rust_panics {
            // The Rust error return nearest after the aligned position.
            let after = rows[k..].iter().filter_map(|r| r.rust).next().unwrap_or(0);
            let before = rows[..k].iter().filter_map(|r| r.rust).next_back().unwrap_or(0);
            let ret = rows
                .iter()
                .enumerate()
                .filter(|(_, r)| r.cpp.is_none())
                .filter_map(|(m, r)| r.rust.map(|j| (m, j)))
                .filter(|(_, j)| matches!(rust.units[*j].features.ret, Some(Ret::Error(_))))
                .min_by_key(|(_, j)| (*j as isize - after.max(before) as isize).unsigned_abs());
            if let Some((m, j)) = ret {
                let e = match &rust.units[j].features.ret {
                    Some(Ret::Error(e)) => e.clone(),
                    _ => String::new(),
                };
                let line = rust.units[j].start_line;
                rows[k].notes.retain(|n| n.severity != Severity::Issue);
                rows[k].notes.push(Note::new(
                    Severity::Issue,
                    Category::Assert,
                    format!(
                        "C++ panics here ({}); the Rust returns {e} instead (line {line})",
                        text.split_whitespace().collect::<Vec<_>>().join(" ").trim_end_matches(';')
                    ),
                ));
                demote(&mut rows[m], "it stands in for a C++ panic");
            }
        }
    }
}

/// Whether the unit at `j` is part of the function's leading doc comment.
pub(crate) fn new_doc(f: &Function, j: usize) -> bool {
    f.units[j].kind == UnitKind::Comment
        && f.units[j + 1..]
            .iter()
            .find(|u| u.kind != UnitKind::Comment)
            .is_some_and(|u| u.kind == UnitKind::Signature)
}

/// Whether a function has a leading comment, on its definition or on its
/// declaration.
fn has_doc(f: &Function) -> bool {
    f.units
        .iter()
        .take_while(|u| u.kind != UnitKind::Signature)
        .any(|u| u.kind == UnitKind::Comment)
}

/// Whether the unit at `j` is the whole doc comment of a `#[test]`
/// function and is just the C++ test's name, which unittest-rs shows as
/// the test's name.
fn names_test(rust: &Function, j: usize, cpp: &Function) -> bool {
    let (Some(name), Some(sig)) = (&cpp.test_name, rust.units.get(j + 1)) else {
        return false;
    };
    let u = &rust.units[j];
    let alone = (j == 0 || rust.units[j - 1].kind != UnitKind::Comment)
        && sig.kind == UnitKind::Signature
        && u.start_line == u.end_line;
    // Only attributes, one of them `#[test]` (perhaps followed by the `fn`
    // on its line), between the doc and the `fn`.
    let attrs: Vec<&str> = (u.end_line + 1..=sig.start_line)
        .map(|l| rust.line(l).trim())
        .take_while(|l| l.starts_with("#["))
        .collect();
    alone
        && attrs.iter().any(|l| l.split_whitespace().next() == Some("#[test]"))
        && rust.line(u.start_line).trim().strip_prefix("///").map(str::trim) == Some(name.trim())
}

/// The findings of a list of rows, in row order.
pub fn findings_of(rows: &[Row], cpp: &Function, rust: &Function) -> Vec<Finding> {
    let mut findings = Vec::new();
    for r in rows {
        for n in &r.notes {
            findings.push(Finding {
                severity: n.severity,
                category: n.category,
                cpp_line: r.cpp.map(|i| cpp.units[i].start_line),
                rust_line: r.rust.map(|j| rust.units[j].start_line),
                cpp_file: r.cpp.and_then(|i| cpp.units[i].file.clone()),
                message: n.message.clone(),
            });
        }
    }
    findings
}

/// Collapses runs of three or more consecutive one-sided rows into a single
/// note on the first row, so a block of code with no counterpart reads as
/// one finding rather than one per statement. Comments are left out of the
/// runs: each lost comment is its own finding, because it is what a
/// reviewer has to carry over.
fn group_runs(rows: &mut [Row], cpp: &Function, rust: &Function) {
    let unit = |r: &Row, m: Marker| -> Option<&Unit> {
        if m == Marker::CppOnly {
            r.cpp.map(|k| &cpp.units[k])
        } else {
            r.rust.map(|k| &rust.units[k])
        }
    };
    let mut i = 0;
    while i < rows.len() {
        let m = rows[i].marker;
        if !matches!(m, Marker::CppOnly | Marker::RustOnly) {
            i += 1;
            continue;
        }
        let mut j = i;
        while j < rows.len() && rows[j].marker == m {
            j += 1;
        }
        // Rows with no note (expected additions such as safety comments)
        // stay in the run but don't count toward it, and neither do
        // comments, traces, asserts and moved steps, which are reported
        // alone.
        let noted: Vec<usize> = (i..j)
            .filter(|&k| {
                !rows[k].notes.is_empty()
                    && rows[k].notes.iter().all(|n| {
                        !n.message.starts_with("Rust adds a check")
                            && !n.message.starts_with("C++ checks")
                    })
                    && rows[k].notes.iter().all(|n| {
                        !matches!(
                            n.category,
                            Category::Comment
                                | Category::Trace
                                | Category::Assert
                                | Category::Order
                                | Category::Conditional
                        )
                    })
            })
            .collect();
        if noted.len() >= 3 {
            let (side, other) =
                if m == Marker::CppOnly { ("C++", "Rust") } else { ("Rust", "C++") };
            let units: Vec<&Unit> = noted.iter().filter_map(|&k| unit(&rows[k], m)).collect();
            let mut counts: Vec<(&'static str, usize)> = Vec::new();
            for u in &units {
                let name = match u.kind {
                    UnitKind::Stmt => "statement",
                    k => k.name(),
                };
                match counts.iter_mut().find(|(n, _)| *n == name) {
                    Some(c) => c.1 += 1,
                    None => counts.push((name, 1)),
                }
            }
            let worst = noted
                .iter()
                .flat_map(|&k| rows[k].notes.iter())
                .min_by_key(|n| (n.severity, n.category))
                .map(|n| (n.severity, n.category))
                .unwrap_or((Severity::Note, Category::ControlFlow));
            let first = units.first().map_or(0, |u| u.start_line);
            let last = units.last().map_or(0, |u| u.end_line);
            let what: Vec<String> = counts.iter().map(|(n, c)| format!("{c} {n}")).collect();
            let span = if first == last {
                format!("line {first}")
            } else {
                format!("lines {first}-{last}")
            };
            // Keep what each statement did: the calls and errors are what a
            // reviewer checks for.
            let mut did: Vec<String> = Vec::new();
            for u in &units {
                for c in u.features.calls.iter().chain(u.features.errors.iter()) {
                    if !did.contains(c) {
                        did.push(c.clone());
                    }
                }
            }
            let mut msg =
                format!("{span} only in {side}, with no {other} counterpart: {}", what.join(", "));
            if !did.is_empty() {
                did.truncate(8);
                msg.push_str(&format!(" ({})", did.join(", ")));
            }
            for &k in &noted {
                rows[k].notes.clear();
            }
            rows[noted[0]].notes.push(Note::new(worst.0, worst.1, msg));
        }
        i = j;
    }
}

pub(crate) fn find_moved(u: &Unit, others: &[Unit], candidates: &[usize]) -> Option<usize> {
    let trivial = u.kind != UnitKind::Comment
        && u.features.calls.is_empty()
        && u.features.errors.is_empty()
        && u.features.locks.is_empty();
    if trivial {
        return None;
    }
    candidates
        .iter()
        .map(|&j| (j, similarity(u, &others[j])))
        .filter(|(_, s)| *s >= 0.7)
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(j, _)| j)
}

/// A statement that only traces or prints.
fn trace_only(f: &crate::model::Features) -> bool {
    !f.calls.is_empty()
        && f.calls.iter().all(|c| matches!(c.as_str(), "trace" | "print"))
        && f.locks.is_empty()
        && f.errors.is_empty()
        && !f.propagates
        && !f.unlocks
}

/// A statement that only asserts.
fn assert_only(f: &crate::model::Features) -> bool {
    f.calls.iter().any(|c| c == "assert")
        && f.locks.is_empty()
        && f.errors.is_empty()
        && !f.propagates
        && !f.unlocks
}

/// A comment that only names the function, such as `// zx_status_t
/// zx_port_wait` above `sys_port_wait`, carries nothing a reader would miss.
fn restates_signature(u: &Unit, f: &Function) -> bool {
    // Comment words are identifiers with underscores removed.
    const TYPE_WORDS: &[&str] = &["zxstatust", "void", "int", "bool", "static", "const"];
    let strip = |w: &str| -> String {
        let w = w.strip_prefix("zx").unwrap_or(w);
        w.strip_prefix("sys").unwrap_or(w).to_string()
    };
    let name = strip(&crate::normalize::ident(&f.base).replace('_', ""));
    let words = &u.features.comment;
    if words.is_empty() || words.len() > 4 || name.is_empty() {
        return false;
    }
    let mut named = false;
    for w in words {
        // `// zx_status_t zx_thread_start` also heads `sys_thread_start_regs`.
        if strip(w) == name
            || (w.starts_with("zx") && w.len() > 4 && !TYPE_WORDS.contains(&w.as_str()))
        {
            named = true;
        } else if !TYPE_WORDS.contains(&w.as_str()) {
            return false;
        }
    }
    named
}

/// Kinds of control flow counted together when judging balance.
fn flow_group(k: UnitKind) -> u8 {
    match k {
        UnitKind::If | UnitKind::ElseIf => 1,
        UnitKind::Loop => 2,
        UnitKind::Switch => 3,
        UnitKind::Case => 4,
        _ => 0,
    }
}

/// A one-sided `if`, loop, `switch` or `case` when both functions have as
/// many of them: a rewrite, so a note.
fn soften_balanced(n: Note, u: &Unit, balanced: bool) -> Note {
    if n.severity != Severity::Issue || flow_group(u.kind) == 0 || !balanced {
        return n;
    }
    Note::new(
        Severity::Note,
        n.category,
        format!("{}; both sides have as many, so it was probably rewritten", n.message),
    )
}

/// A C++ step that now lives in a function the Rust calls (a `cpp_*`
/// helper, or another Rust function) was moved, not dropped.
fn soften_moved(n: Note, u: &Unit, elsewhere: &[&Function]) -> Note {
    if n.severity != Severity::Issue || u.kind == UnitKind::Comment {
        return n;
    }
    // A bare `else` or a generic status check matches too much to prove
    // anything moved; a test must name something specific.
    const GENERIC: &[&str] = &["status", "result", "ok", "err", "res", "rc"];
    let specific: Vec<&String> =
        u.features.names.iter().filter(|n| !GENERIC.contains(&n.as_str())).collect();
    let test = matches!(u.kind, UnitKind::If | UnitKind::ElseIf | UnitKind::Else);
    if test && specific.is_empty() {
        return n;
    }
    let found = elsewhere.iter().find(|f| {
        f.units.iter().any(|v| {
            v.kind != UnitKind::Signature
                && similarity(u, v) >= 0.6
                && (!test || specific.iter().all(|s| v.features.names.contains(s)))
                && sorted(&u.features.errors) == sorted(&v.features.errors)
        })
    });
    match found {
        Some(f) => Note::new(
            Severity::Note,
            n.category,
            format!("{}; it appears in {}, which the Rust calls", n.message, f.name),
        ),
        None => n,
    }
}

/// A statement that only makes calls the other function also makes
/// somewhere was most likely split, merged or moved, not dropped: a note.
fn soften_if_called(n: Note, u: &Unit, other: &Function) -> Note {
    if n.severity != Severity::Issue
        || n.category != Category::Call
        || u.kind != UnitKind::Stmt
        || u.features.calls.is_empty()
        || !u.features.calls.iter().all(|c| other.calls.contains(c))
    {
        return n;
    }
    Note::new(
        Severity::Note,
        Category::Call,
        format!("{}; the other side makes the same calls elsewhere", n.message),
    )
}

/// A section label such as `// Socket methods.` or `/* external api */`.
fn is_banner(u: &Unit) -> bool {
    const LAST: &[&str] = &["methods", "implementation", "api", "functions", "helpers"];
    let w = &u.features.comment;
    !w.is_empty() && w.len() <= 4 && w.last().is_some_and(|l| LAST.contains(&l.as_str()))
}

/// Whether most of a C++ comment's words appear in the Rust function's
/// comments, as when several comments were merged into one Rust block.
/// Whether a comment's words appear, in order, in a comment anywhere in
/// `f`, including inside an expression or a pattern.
fn verbatim_in(u: &Unit, f: &Function) -> bool {
    static COMMENT: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"//[^\n]*|/\*(?s:.*?)\*/").unwrap());
    let words = &u.features.comment;
    if words.is_empty() {
        return false;
    }
    // Consecutive `//` lines read as one comment.
    let text = f.lines.join("\n");
    let theirs: Vec<String> = COMMENT
        .find_iter(&text)
        .flat_map(|m| crate::normalize::comment_words(m.as_str()))
        .collect();
    theirs.windows(words.len()).any(|w| w == words.as_slice())
}

fn words_kept(u: &Unit, rust: &Function) -> bool {
    let words = &u.features.comment;
    if words.is_empty() {
        return false;
    }
    // A short comment (`/* Skylake H/S */`) kept word for word.
    if verbatim_in(u, rust) {
        return true;
    }
    if words.len() < 4 {
        return false;
    }
    let rust_words: std::collections::HashSet<&str> = rust
        .units
        .iter()
        .filter(|r| r.kind == UnitKind::Comment)
        .flat_map(|r| r.features.comment.iter().map(String::as_str))
        .collect();
    let kept = words.iter().filter(|w| rust_words.contains(w.as_str())).count();
    kept as f64 >= 0.8 * words.len() as f64
}

fn only_in(u: &Unit, here: &str, there: &str, moved: Option<usize>) -> Note {
    let f = &u.features;
    let what = match u.kind {
        UnitKind::Comment => "comment".to_string(),
        UnitKind::Stmt if trace_only(f) => "trace/print statement".to_string(),
        UnitKind::Stmt => {
            let mut parts = Vec::new();
            if !f.locks.is_empty() {
                parts.push(format!("acquires {}", f.locks.join(", ")));
            }
            if !f.errors.is_empty() {
                parts.push(format!("uses {}", f.errors.join(", ")));
            }
            if f.propagates {
                parts.push("propagates an error".to_string());
            }
            if !f.calls.is_empty() {
                parts.push(format!("calls {}", f.calls.join(", ")));
            }
            if parts.is_empty() {
                "statement".to_string()
            } else {
                format!("statement ({})", parts.join("; "))
            }
        }
        UnitKind::Return => match &f.ret {
            Some(r) => format!("return of {r}"),
            None => "return".to_string(),
        },
        k => k.name().to_string(),
    };
    let from_cpp = here == "C++";
    let (significant, category) = match u.kind {
        // A comment lost in translation matters; an added one is worth a look.
        UnitKind::Comment => (from_cpp, Category::Comment),
        // Returning a plain value (often a tail expression) is how Rust ends
        // constructors and accessors; success and error returns still count.
        UnitKind::Return => (
            !matches!(f.ret, Some(Ret::Value) | None),
            if matches!(f.ret, Some(Ret::Error(_)) | Some(Ret::Status)) {
                Category::ErrorPath
            } else {
                Category::ControlFlow
            },
        ),
        // The rubric asks for every trace statement to be ported; one the
        // Rust adds is worth a look.
        UnitKind::Stmt if trace_only(f) => (from_cpp, Category::Trace),
        // A check the Rust adds is worth a look, but one it drops is an issue.
        UnitKind::Stmt if assert_only(f) => (from_cpp, Category::Assert),
        UnitKind::Stmt => {
            let cat = if !f.locks.is_empty() || f.unlocks {
                Category::Lock
            } else if !f.errors.is_empty() || f.propagates {
                Category::ErrorPath
            } else {
                Category::Call
            };
            (
                !f.calls.is_empty()
                    || !f.locks.is_empty()
                    || !f.errors.is_empty()
                    || f.propagates
                    || f.unlocks,
                cat,
            )
        }
        UnitKind::Signature => (false, Category::ControlFlow),
        // An `else`, `break`, `goto` or label on its own is a change of shape
        // (`goto done` became `break`, an `else` became an early return);
        // the flow counts show any real imbalance.
        UnitKind::Else
        | UnitKind::Break
        | UnitKind::Continue
        | UnitKind::Goto
        | UnitKind::Label => (false, Category::ControlFlow),
        // Rust's exhaustive `match` needs arms C++'s `switch` doesn't.
        UnitKind::Case => (from_cpp, Category::ControlFlow),
        _ => (true, Category::ControlFlow),
    };
    let sev = if significant { Severity::Issue } else { Severity::Note };
    let mut msg = format!("{what} only in {here}");
    let mut category = category;
    if let Some(line) = moved {
        msg.push_str(&format!("; resembles {there} line {line}, so the order may differ"));
        if category != Category::Comment {
            category = Category::Order;
        }
    }
    Note::new(sev, category, msg)
}

/// Compares aligned units, with how their formats compare (see
/// [`output_diff`]).
fn compare(a: &Unit, b: &Unit, output: OutputCmp, elsewhere: &[&Function], notes: &mut Vec<Note>) {
    let fa = &a.features;
    let fb = &b.features;
    if crate::align::is_handled_vs_propagated(a, b) {
        let msg = if a.features.propagates {
            "C++ propagates this call's error, but Rust handles it in its own branch"
        } else {
            "C++ handles this call's error in its own branch, but Rust propagates it"
        };
        notes.push(Note::new(Severity::Issue, Category::ErrorPath, msg));
        return;
    }
    if a.kind != b.kind
        && !equivalent_kinds(a.kind, b.kind)
        && !crate::align::is_return_vs_propagated(a, b)
    {
        notes.push(Note::new(
            Severity::Note,
            Category::ControlFlow,
            format!("C++ {} vs Rust {}", a.kind.name(), b.kind.name()),
        ));
    }
    if a.kind == UnitKind::Comment {
        if fa.comment != fb.comment {
            // Rewording is fine; losing a TODO or a negation is not.
            const WEIGHTY: &[&str] = &[
                "todo", "not", "never", "no", "must", "cannot", "can't", "don't", "doesn't",
                "only", "always",
            ];
            let lost: Vec<&str> = WEIGHTY
                .iter()
                .copied()
                .filter(|w| fa.comment.iter().any(|x| x == w) != fb.comment.iter().any(|x| x == w))
                .collect();
            // A comment still in the C++ is not lost, however Rust words it.
            let (sev, msg) = if lost.is_empty() || fa.still_in_cpp {
                (Severity::Note, comment_diff(&fa.comment, &fb.comment))
            } else {
                (
                    Severity::Issue,
                    format!(
                        "{}; only one side says {}",
                        comment_diff(&fa.comment, &fb.comment),
                        lost.iter().map(|w| format!("\"{w}\"")).collect::<Vec<_>>().join(", ")
                    ),
                )
            };
            notes.push(Note::new(sev, Category::Comment, msg));
        }
        return;
    }
    // A trace aligned with a statement that doesn't trace was dropped.
    if trace_only(fa) && !trace_only(fb) && !fb.calls.iter().any(|c| c == "trace" || c == "print") {
        notes.push(Note::new(
            Severity::Issue,
            Category::Trace,
            "trace/print statement only in C++",
        ));
    }
    match (&fa.ret, &fb.ret) {
        (Some(Ret::Error(x)), Some(Ret::Error(y))) if x != y => {
            notes.push(Note::new(
                Severity::Issue,
                Category::ErrorPath,
                format!("error code differs: C++ returns {x}, Rust returns {y}"),
            ));
        }
        // Passing a callee's status on reads as a value in Rust, whose tail
        // expression returns the callee's `Result`.
        (Some(Ret::Status), Some(Ret::Value)) | (Some(Ret::Value), Some(Ret::Status))
            if !fa.calls.is_empty() && !fb.calls.is_empty() => {}
        (Some(ra), Some(rb)) if ra != rb => {
            // Success versus failure is a behavior change; the other
            // mismatches (a status variable, a value, `Ok`) are usually
            // just the two languages' ways of saying the same thing.
            let error = |r: &Ret| matches!(r, Ret::Error(_));
            let success = |r: &Ret| matches!(r, Ret::Ok | Ret::Value);
            // C++ mapping a failure to a specific code where Rust passes
            // the callee's status on changes what the caller sees.
            let remapped = error(ra) && *rb == Ret::Status;
            // `return ZX_ERR_NEXT` with an out-parameter is a second kind of
            // success, which Rust returns as a variant: `Ok(Action::Trap)`.
            let next_as_variant = *ra == Ret::Error("NEXT".into())
                && matches!(rb, Ret::Ok | Ret::Value)
                && b.features.idents.len() + b.features.calls.len() > 1;
            let sev = if next_as_variant {
                Severity::Note
            } else if (error(ra) && success(rb)) || (success(ra) && error(rb)) || remapped {
                Severity::Issue
            } else {
                Severity::Note
            };
            let msg = if next_as_variant {
                format!(
                    "C++ returns {ra} with an out-parameter, Rust returns a value in its place; check that callers treat it as NEXT"
                )
            } else {
                format!("C++ returns {ra}, Rust returns {rb}")
            };
            notes.push(Note::new(sev, Category::ErrorPath, msg));
        }
        _ => {}
    }
    if fa.ret.is_none() || fb.ret.is_none() {
        let (ea, eb) = (sorted(&fa.errors), sorted(&fb.errors));
        if ea != eb {
            notes.push(Note::new(
                Severity::Issue,
                Category::ErrorPath,
                format!("error codes differ: C++ [{}], Rust [{}]", ea.join(", "), eb.join(", ")),
            ));
        }
    }
    if fa.locks != fb.locks {
        // One lock on each side under different names is usually the same
        // lock spelled two ways; a lock taken on one side only is not.
        let renamed = fa.locks.len() == fb.locks.len();
        notes.push(Note::new(
            if renamed { Severity::Note } else { Severity::Issue },
            Category::Lock,
            format!(
                "locks differ: C++ acquires [{}], Rust acquires [{}]",
                fa.locks.join(", "),
                fb.locks.join(", ")
            ),
        ));
    }
    if fa.propagates != fb.propagates && !propagation_is_implied(a, b) {
        let (who, other) = if fa.propagates { ("C++", "Rust") } else { ("Rust", "C++") };
        notes.push(Note::new(
            Severity::Issue,
            Category::ErrorPath,
            format!("{who} propagates an error here but {other} does not"),
        ));
    }
    if fa.unlocks != fb.unlocks {
        let who = if fa.unlocks { "C++" } else { "Rust" };
        notes.push(Note::new(
            Severity::Note,
            Category::Lock,
            format!("only {who} releases a lock here"),
        ));
    }
    if fa.asserts != fb.asserts {
        let who = if fa.asserts { "C++" } else { "Rust" };
        notes.push(Note::new(
            if fa.asserts { Severity::Issue } else { Severity::Note },
            Category::Assert,
            format!("only {who} asserts here"),
        ));
    }
    let mut only_a: Vec<&String> = uniq(&fa.calls)
        .into_iter()
        .filter(|c| !fb.calls.contains(c) && !name_matches(c, fb))
        .collect();
    let mut only_b: Vec<&String> = uniq(&fb.calls)
        .into_iter()
        .filter(|c| !fa.calls.contains(c) && !name_matches(c, fa))
        .collect();
    // If Rust calls a `cpp_*` FFI helper defined in the same change (`elsewhere`),
    // verify that the helper's body makes the C++ calls on this line.
    only_b.retain(|y| {
        if !y.starts_with("cpp_") {
            return true;
        }
        let Some(h) = elsewhere.iter().find(|h| crate::normalize::ident(&h.base) == **y) else {
            return true;
        };
        if !h.calls.is_empty()
            && h.calls.iter().all(|hc| fa.calls.contains(hc) || name_matches(hc, fa))
        {
            only_a.retain(|x| !h.calls.contains(x));
            return false;
        }
        true
    });
    // Rust calling back into C++ through `cpp_<class>_<method>` makes the
    // call C++ made directly.
    let via_ffi = |x: &str, y: &str| y.starts_with("cpp_") && y.ends_with(&format!("_{x}"));
    while let Some((i, j)) = only_a
        .iter()
        .enumerate()
        .find_map(|(i, x)| only_b.iter().position(|y| via_ffi(x, y)).map(|j| (i, j)))
    {
        only_a.remove(i);
        only_b.remove(j);
    }
    // A C++ scope macro (`OOM_KTRACE_DURATION()`) and a Rust RAII guard
    // (`ScopedOomKtrace::new()`) share the same stem once `scoped_`/`auto_`
    // and `_duration`/`_guard`/`_scope` are stripped.
    fn guard_stem(s: &str) -> &str {
        let s = s.strip_prefix("scoped_").or_else(|| s.strip_prefix("auto_")).unwrap_or(s);
        s.strip_suffix("_duration")
            .or_else(|| s.strip_suffix("_guard"))
            .or_else(|| s.strip_suffix("_scope"))
            .unwrap_or(s)
    }
    while let Some((i, j)) = only_a.iter().enumerate().find_map(|(i, x)| {
        let gx = guard_stem(x);
        (!gx.is_empty())
            .then(|| {
                only_b.iter().position(|y| {
                    let gy = guard_stem(y);
                    !gy.is_empty() && (gx != x.as_str() || gy != y.as_str()) && gx == gy
                })
            })
            .flatten()
            .map(|j| (i, j))
    }) {
        only_a.remove(i);
        only_b.remove(j);
    }
    // Functional-style C++ type cast passed to a helper named after the type
    // (`PressureLevelToString(PressureLevel(i))`).
    only_a.retain(|x| {
        !fb.calls.iter().any(|c| fa.calls.contains(c) && c.starts_with(&format!("{x}_")))
    });
    for (x, y) in crate::normalize::EQUIVALENT_CALLS {
        let (i, j) = (
            only_a.iter().position(|c| c.as_str() == *x),
            only_b.iter().position(|c| c.as_str() == *y),
        );
        if let (Some(i), Some(j)) = (i, j) {
            only_a.remove(i);
            only_b.remove(j);
        }
    }
    // A printf-family call and a Rust formatting macro printing to the same
    // place are the same call; what they print is compared instead.
    if let OutputCmp::Same(dest) | OutputCmp::Differs(dest, _) = output {
        only_a.retain(|c| !dest.cpp_calls().contains(&c.as_str()));
        only_b.retain(|c| !dest.rust_calls().contains(&c.as_str()));
    }
    if let OutputCmp::Differs(_, note) = output {
        notes.push(note);
    }
    if !only_a.is_empty() || !only_b.is_empty() {
        let mut parts = Vec::new();
        if !only_a.is_empty() {
            parts.push(format!("only C++ calls {}", join(&only_a)));
        }
        if !only_b.is_empty() {
            parts.push(format!("only Rust calls {}", join(&only_b)));
        }
        notes.push(Note::new(Severity::Note, Category::Call, parts.join("; ")));
    }
}

/// How the formats of aligned units compare.
enum OutputCmp {
    /// Not both a printf-family call and a Rust formatting macro printing
    /// to the same [`Dest`](crate::printf::Dest).
    NotOutput,
    /// Both print the same text, or were reordered across a closure.
    Same(crate::printf::Dest),
    /// Both print, differently.
    Differs(crate::printf::Dest, Note),
}

/// How the formats of aligned units `i` and `j` compare. When each side
/// prints the other's format elsewhere, the statements were reordered.
fn output_diff(i: usize, j: usize, cpp: &Function, rust: &Function) -> OutputCmp {
    use crate::printf::{Output, same_output};
    let (a, b) = (unit_text(cpp, &cpp.units[i]), unit_text(rust, &rust.units[j]));
    let (dest, msg) = match crate::printf::compare(&a, &b) {
        None => return OutputCmp::NotOutput,
        Some((dest, Output::Same)) => return OutputCmp::Same(dest),
        Some((dest, Output::Differs(msg))) => (dest, msg),
    };
    let moved = rust.units.iter().any(|u| same_output(&a, &unit_text(rust, u)))
        && cpp.units.iter().any(|u| same_output(&unit_text(cpp, u), &b));
    // A print in a Rust closure runs where the closure is called, not where
    // it is written, so its place says nothing about the order.
    let closure_moved = moved
        && (in_closure(rust, j)
            || (0..rust.units.len())
                .any(|k| in_closure(rust, k) && same_output(&a, &unit_text(rust, &rust.units[k]))));
    if closure_moved {
        return OutputCmp::Same(dest);
    }
    OutputCmp::Differs(
        dest,
        if moved {
            Note::new(
                Severity::Issue,
                Category::Order,
                format!(
                    "{msg}; each side prints the other's format elsewhere, so they were reordered"
                ),
            )
        } else {
            Note::new(Severity::Issue, Category::Call, msg)
        },
    )
}

/// Whether unit `k` of a Rust function is in a closure's body or defines a
/// closure. A multi-line closure's statements follow the statement that
/// defines it, one level deeper.
fn in_closure(f: &Function, k: usize) -> bool {
    static CLOSURE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"[=(,]\s*(?:move\s+)?\|[^|]*\|").unwrap());
    if CLOSURE.is_match(&code_only(&unit_text(f, &f.units[k]))) {
        return true;
    }
    let mut depth = f.units[k].depth;
    for u in f.units[..k].iter().rev() {
        if u.depth < depth {
            if u.kind == UnitKind::Stmt {
                return true;
            }
            depth = u.depth;
        }
    }
    false
}

/// Kinds that are the same step spelled differently: a C++ `if` that
/// returns early and Rust's `let ... else`, or `else { if }` and `else if`.
fn equivalent_kinds(a: UnitKind, b: UnitKind) -> bool {
    use UnitKind::{Case, Else, ElseIf, If};
    matches!(
        (a, b),
        (If, ElseIf) | (ElseIf, If) | (Case, If | ElseIf | Else) | (If | ElseIf | Else, Case)
    )
}

/// Whether a call on one side is a field or accessor on the other: C++
/// `allocation()` and Rust `self.allocation`, or `set_key(k)` and `key_ = k`.
fn name_matches(call: &str, other: &crate::model::Features) -> bool {
    let stripped = call.strip_prefix("set_").or_else(|| call.strip_prefix("get_")).unwrap_or(call);
    let bare = stripped.replace('_', "");
    other.names.contains(&bare)
        || other.idents.contains(&bare)
        || other.idents.iter().any(|id| id == stripped)
}

/// One side propagates with `?` where the other returns the status it
/// checked in the same unit, which is the same thing.
fn propagation_is_implied(a: &Unit, b: &Unit) -> bool {
    let returns_status = |u: &Unit| {
        matches!(u.features.ret, Some(Ret::Status) | Some(Ret::Value))
            && !u.features.calls.is_empty()
    };
    (a.features.propagates && returns_status(b)) || (b.features.propagates && returns_status(a))
}

/// Every condition operand a function tests.
/// Conjuncts of the conditions near unit `k`: a C++ `if (a && b)` split
/// into nested Rust `if`s (or the reverse) keeps its tests close by.
fn nearby_conjuncts(f: &Function, k: usize) -> Vec<&crate::model::Conjunct> {
    let lo = k.saturating_sub(3);
    let hi = (k + 4).min(f.units.len());
    f.units[lo..hi].iter().flat_map(|u| u.features.conjuncts.iter()).collect()
}

/// Whether two normalized names are the same, or one contains the other
/// (`modewrite` and `write`).
fn name_like(x: &str, y: &str) -> bool {
    x == y || (x.len() >= 3 && y.len() >= 3 && (x.contains(y) || y.contains(x)))
}

/// Reports tests one side's `if` makes and the other's doesn't, operand by
/// operand. A test the other function makes in a condition close by (a
/// C++ `if (a && b)` split into nested Rust `if`s) doesn't count. When both
/// conditions have as many tests but they read differently, the operands
/// were probably renamed or hoisted into locals: that is a note. A test
/// added or dropped outright is an issue.
fn condition_diff(i: usize, j: usize, cpp: &Function, rust: &Function, notes: &mut Vec<Note>) {
    let (a, b) = (&cpp.units[i], &rust.units[j]);
    let conds = |u: &Unit| matches!(u.kind, UnitKind::If | UnitKind::ElseIf);
    let (fa, fb) = (&a.features, &b.features);
    if !conds(a) || !conds(b) || fa.checks_error || fb.checks_error {
        return;
    }
    if fa.conjuncts.is_empty() || fb.conjuncts.is_empty() {
        return;
    }
    // Status tests (`status != ZX_OK`) have no names left once noise is
    // dropped; they are compared as error checks instead.
    if fa.conjuncts.iter().chain(&fb.conjuncts).any(|c| c.names.is_empty()) {
        return;
    }
    // A C++ `do { ... } while (cond);` continues when `cond` holds, while
    // Rust's `loop { ... if !cond { break; } }` breaks when `!cond` holds;
    // De Morgan's law flips each comparison unless `!(` wraps the whole test.
    let do_while =
        unit_text(cpp, a).trim_start().trim_start_matches('}').trim_start().starts_with("while");
    let breaks =
        rust.units.get(j + 1).is_some_and(|u| u.kind == UnitKind::Break && u.depth > b.depth);
    let flip = do_while && breaks && !condition_text(rust, b).starts_with("!(");
    // Tests of the same names with different comparisons (`x == 0` and
    // `x > MAX`) are different tests.
    let same = |x: &crate::model::Conjunct, y: &crate::model::Conjunct| {
        let covered =
            |p: &[String], q: &[String]| p.iter().all(|w| q.iter().any(|v| name_like(w, v)));
        let names = crate::normalize::jaccard(&x.names, &y.names) >= 0.5
            || covered(&x.names, &y.names)
            || covered(&y.names, &x.names);
        let ox = comparison(&x.text).map(|op| if flip { negate_cmp(op) } else { op });
        let oy = comparison(&y.text);
        names && (ox.is_none() || oy.is_none() || ox == oy)
    };
    let (near_a, near_b) = (nearby_conjuncts(cpp, i), nearby_conjuncts(rust, j));
    let missing =
        |xs: &[crate::model::Conjunct], near: &[&crate::model::Conjunct]| -> Vec<String> {
            xs.iter().filter(|x| !near.iter().any(|y| same(x, y))).map(|x| x.text.clone()).collect()
        };
    let only_b = missing(&fb.conjuncts, &near_a);
    let only_a = missing(&fa.conjuncts, &near_b);
    if only_a.is_empty() && only_b.is_empty() {
        return;
    }
    let (na, nb) = (fa.conjuncts.len(), fb.conjuncts.len());
    if na == nb && !only_a.is_empty() && !only_b.is_empty() {
        notes.push(Note::new(
            Severity::Note,
            Category::ControlFlow,
            format!(
                "condition reads differently: C++ tests {}, Rust tests {}",
                only_a.join("; "),
                only_b.join("; ")
            ),
        ));
        return;
    }
    if !only_b.is_empty() {
        notes.push(Note::new(
            if nb > na { Severity::Issue } else { Severity::Note },
            Category::ControlFlow,
            format!("Rust's condition adds a test that C++ doesn't make: {}", only_b.join("; ")),
        ));
    }
    if !only_a.is_empty() {
        notes.push(Note::new(
            if na > nb { Severity::Issue } else { Severity::Note },
            Category::ControlFlow,
            format!("C++'s condition tests {}, which the Rust doesn't", only_a.join("; ")),
        ));
    }
}

/// The kind of comparison a condition makes, with `<` and `>` (and `<=`
/// and `>=`) alike, since either can be written with its operands swapped.
fn comparison(text: &str) -> Option<&'static str> {
    static OP: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"\s(==|!=|<=|>=|<|>)\s").unwrap());
    let op = OP.captures(text)?.get(1)?.as_str();
    Some(match op {
        "==" => "eq",
        "!=" => "ne",
        "<" | ">" => "lt",
        _ => "le",
    })
}

fn negate_cmp(op: &'static str) -> &'static str {
    match op {
        "eq" => "ne",
        "ne" => "eq",
        "lt" => "le",
        _ => "lt",
    }
}

fn sorted(v: &[String]) -> Vec<String> {
    let mut v = v.to_vec();
    v.sort();
    v.dedup();
    v
}

fn uniq(v: &[String]) -> Vec<&String> {
    let mut out: Vec<&String> = Vec::new();
    for x in v {
        if !out.contains(&x) {
            out.push(x);
        }
    }
    out
}

fn join(v: &[&String]) -> String {
    v.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
}

/// Describes how two comments' words differ.
fn comment_diff(a: &[String], b: &[String]) -> String {
    // Walk the LCS to find words unique to each side.
    let (n, m) = (a.len(), b.len());
    let mut dp = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] =
                if a[i] == b[j] { dp[i + 1][j + 1] + 1 } else { dp[i + 1][j].max(dp[i][j + 1]) };
        }
    }
    let (mut i, mut j) = (0, 0);
    let (mut only_a, mut only_b) = (Vec::new(), Vec::new());
    while i < n || j < m {
        if i < n && j < m && a[i] == b[j] {
            i += 1;
            j += 1;
        } else if j >= m || (i < n && dp[i + 1][j] >= dp[i][j + 1]) {
            only_a.push(a[i].as_str());
            i += 1;
        } else {
            only_b.push(b[j].as_str());
            j += 1;
        }
    }
    let common = lcs_len(a, b);
    let clip = |v: &[&str]| {
        if v.len() > 12 { format!("{} ...", v[..12].join(" ")) } else { v.join(" ") }
    };
    let mut parts = vec![format!("comment differs ({common} words shared)")];
    if !only_a.is_empty() {
        parts.push(format!("C++ only: \"{}\"", clip(&only_a)));
    }
    if !only_b.is_empty() {
        parts.push(format!("Rust only: \"{}\"", clip(&only_b)));
    }
    parts.join("; ")
}

/// Per-function summaries of the facts a reviewer checks first.
#[derive(Clone, Debug, Default)]
pub struct Summary {
    pub cpp_errors: Vec<String>,
    pub rust_errors: Vec<String>,
    pub cpp_locks: Vec<String>,
    pub rust_locks: Vec<String>,
    pub comments_total: usize,
    pub comments_same: usize,
    pub comments_changed: usize,
    /// (kind, C++ count, Rust count)
    pub flow: Vec<(&'static str, usize, usize)>,
}

pub fn summarize(cpp: &Function, rust: &Function, rows: &[Row]) -> Summary {
    let errs = |f: &Function| -> Vec<String> {
        f.units
            .iter()
            .filter(|u| !u.features.plumbing)
            .filter_map(|u| match (&u.features.ret, u.features.propagates) {
                (Some(Ret::Error(e)), _) => Some(e.clone()),
                // `ok_or(X)?` and `if (!x) return ZX_ERR_X;` return X.
                (_, true) if !u.features.errors.is_empty() => {
                    Some(format!("{}?", u.features.errors.join("|")))
                }
                (_, true) => Some("?".to_string()),
                _ => None,
            })
            .collect()
    };
    let locks = |f: &Function| -> Vec<String> {
        f.units.iter().flat_map(|u| u.features.locks.clone()).collect()
    };
    let mut s = Summary {
        cpp_errors: errs(cpp),
        rust_errors: errs(rust),
        cpp_locks: locks(cpp),
        rust_locks: locks(rust),
        ..Summary::default()
    };
    for r in rows {
        if let Some(i) = r.cpp {
            if cpp.units[i].kind == UnitKind::Comment {
                s.comments_total += 1;
                match r.marker {
                    Marker::Same => s.comments_same += 1,
                    Marker::Note | Marker::Issue => s.comments_changed += 1,
                    _ => {}
                }
            }
        }
    }
    let mut skipped_cpp = Vec::new();
    let mut k = 0;
    while k < rows.len() {
        if rows[k].notes.iter().any(|n| n.message.ends_with(EXHAUSTIVE_DEFAULT_WHY)) {
            if let Some(i) = rows[k].cpp {
                let depth = cpp.units[i].depth;
                skipped_cpp.push(i);
                k += 1;
                while k < rows.len()
                    && rows[k].marker == Marker::CppOnly
                    && row_unit(&rows[k], cpp, true)
                        .is_some_and(|v| v.depth > depth || v.kind == UnitKind::Comment)
                {
                    if let Some(ci) = rows[k].cpp {
                        skipped_cpp.push(ci);
                    }
                    k += 1;
                }
                continue;
            }
        }
        k += 1;
    }
    let kinds: [(&'static str, &[UnitKind]); 7] = [
        ("if", &[UnitKind::If, UnitKind::ElseIf]),
        ("else", &[UnitKind::Else]),
        ("loop", &[UnitKind::Loop]),
        ("switch", &[UnitKind::Switch]),
        ("case", &[UnitKind::Case]),
        ("return", &[UnitKind::Return]),
        ("break/continue", &[UnitKind::Break, UnitKind::Continue]),
    ];
    for (name, ks) in kinds {
        let a = cpp
            .units
            .iter()
            .enumerate()
            .filter(|(i, u)| !skipped_cpp.contains(i) && ks.contains(&u.kind))
            .count();
        let b = rust.units.iter().filter(|u| ks.contains(&u.kind)).count();
        if a + b > 0 {
            s.flow.push((name, a, b));
        }
    }
    s
}

/// The source text of a unit.
fn unit_text(f: &Function, u: &Unit) -> String {
    if u.file.is_some() {
        return u.ext_lines.join("\n");
    }
    (u.start_line..=u.end_line).map(|l| f.line(l)).collect::<Vec<_>>().join("\n")
}

/// Source text with comments and string literals blanked, so only code is
/// searched.
fn code_only(text: &str) -> String {
    static STR: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r#"//[^\n]*|/\*(?s:.)*?\*/|"(?:[^"\\\n]|\\.)*"|'(?:[^'\\\n]|\\.)'"#)
            .unwrap()
    });
    STR.replace_all(text, " ").into_owned()
}

/// A named constant a piece of code uses, with the words of its name
/// (`X86_FLAGS_RF`, or C++ `kMaxSize` as `MAX_SIZE`).
#[derive(Clone, Debug, PartialEq)]
struct Constant {
    name: String,
    words: Vec<String>,
}

/// The named constants in `text`. Macros, error codes (compared as
/// errors), limits such as `UINT32_MAX` and `u32::MAX`, and `NULL` are
/// left out.
fn constants(text: &str) -> Vec<Constant> {
    static CONST: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r"\b([A-Z][A-Z0-9]*(?:_[A-Z0-9]+)+|[A-Z]{3,}[0-9]*|k[A-Z][A-Za-z0-9]*)\b(\s*[(!]|<)?",
        )
        .unwrap()
    });
    // Log levels and test macros are not values.
    const SKIP: &[&str] = &[
        "NULL",
        "TRUE",
        "FALSE",
        "MAX",
        "MIN",
        "BITS",
        "EOF",
        "LTRACE",
        "LOCAL_TRACE",
        "ZX_OK",
        "SAFETY",
        "TODO",
        "FIXME",
        "NOTE",
        "XXX",
        "CRITICAL",
        "ALWAYS",
        "INFO",
        "SPEW",
        "INFO_VERBOSE",
        "BEGIN_TEST",
        "END_TEST",
        "FFI_ALWAYS_INLINE",
        "DEBUG_ASSERT_IMPLEMENTED",
        "ZX_CLOCK_MONOTONIC",
        "MAX_FORMAT_SIZE_LEN",
    ];
    let code = code_only(text);
    let mut out: Vec<Constant> = Vec::new();
    for c in CONST.captures_iter(&code) {
        if c.get(2).is_some() {
            continue;
        }
        let raw = &c[1];
        let name = match raw.strip_prefix('k') {
            Some(rest) => crate::normalize::ident(rest).to_ascii_uppercase(),
            None => raw.to_string(),
        };
        if SKIP.contains(&name.as_str())
            || name.starts_with("ZX_ERR_")
            || name.starts_with("ERR_")
            || name.ends_with("_MAX")
            || name.ends_with("_MIN")
            || name.starts_with("TA_")
            || name.starts_with("__")
        {
            continue;
        }
        // `Status::INVALID_ARGS` is an error code.
        let before = &code[..c.get(1).unwrap().start()];
        if before.trim_end().ends_with("::")
            && before
                .trim_end()
                .trim_end_matches("::")
                .rsplit(|ch: char| !ch.is_alphanumeric() && ch != '_')
                .next()
                .is_some_and(|seg| seg.ends_with("Status") || seg.ends_with("Error") || seg == "zx")
        {
            continue;
        }
        let words = crate::normalize::words(&name);
        // A one-word name (`DEFAULT`, `MASK`, `IRQ`) says too little to
        // tell whether the other side spells it differently.
        if words.len() < 2 {
            continue;
        }
        if !out.iter().any(|o| o.name == name) {
            out.push(Constant { name, words });
        }
    }
    out
}

/// The words of every identifier a function's code mentions, to tell
/// whether a constant one side uses appears anywhere in the other.
pub(crate) struct Tokens(Vec<Vec<String>>);

impl Tokens {
    fn of(f: &Function) -> Tokens {
        static ID: std::sync::LazyLock<regex::Regex> =
            std::sync::LazyLock::new(|| regex::Regex::new(r"[A-Za-z_][A-Za-z0-9_]*").unwrap());
        let mut v: Vec<Vec<String>> = Vec::new();
        for u in &f.units {
            if u.kind == UnitKind::Comment {
                continue;
            }
            let text = code_only(&unit_text(f, u));
            for m in ID.find_iter(&text) {
                let t = m.as_str();
                let t = match t.strip_prefix('k') {
                    Some(rest) if rest.starts_with(|c: char| c.is_ascii_uppercase()) => rest,
                    _ => t,
                };
                let w = crate::normalize::words(t);
                if !w.is_empty() && !v.contains(&w) {
                    v.push(w);
                }
            }
        }
        Tokens(v)
    }

    /// Whether the constant, or a name ending the same way (`CR0_WP` for
    /// `X86_CR0_WP`, the enum variant `GeneralRegs` for
    /// `ZX_THREAD_STATE_GENERAL_REGS`), appears.
    fn has(&self, c: &Constant) -> bool {
        let w = &c.words;
        let n = w.len();
        self.0.iter().any(|t| {
            t == w
                || (t.len() >= 2 && w.ends_with(t))
                || (n >= 2 && t.ends_with(w))
                // A global `xsave_supported` became `XSAVE_SUPPORTED_ATOMIC`.
                || (t.len() >= 2 && n == t.len() + 1 && w.starts_with(t))
        }) || (n >= 2 && {
            // An enum: `X86_VENDOR_INTEL` is `X86Vendor::Intel`, and
            // `DELIVERY_MODE_INIT` is `DeliveryMode::Init`.
            let (head, last) = w.split_at(n - 1);
            self.0.iter().any(|t| t.as_slice() == last)
                && self.0.iter().any(|t| {
                    !t.is_empty()
                        && (head.ends_with(t) || t.ends_with(head))
                        && (t.len() >= 2 || t[0].len() >= 4)
                })
        })
    }
}

/// Integer literals in code, by value.
fn literals(text: &str) -> Vec<u128> {
    static LIT: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r"\b(0[xX][0-9A-Fa-f_]+|[0-9][0-9_]*)(?:[uUlLzZ]+|[ui](?:8|16|32|64|128|size))?\b",
        )
        .unwrap()
    });
    let code = code_only(text);
    let mut out: Vec<u128> = LIT
        .captures_iter(&code)
        .filter_map(|c| {
            let t = c[1].replace('_', "");
            match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
                Some(h) => u128::from_str_radix(h, 16).ok(),
                None => t.parse().ok(),
            }
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// How `code` combines the constant with a mask: `|`, `&`, or `& !`/`& ~`.
fn mask_op(code: &str, c: &Constant) -> Option<&'static str> {
    let re = regex::Regex::new(&format!(
        r"(\|=?|&=?|\^=?)\s*([!~])?\s*\(?\s*(?:[A-Za-z_][A-Za-z0-9_]*\s*::\s*)*{0}\b|\b{0}\s*\)?\s*(\||&|\^)",
        regex::escape(&c.name)
    ))
    .ok()?;
    let m = re.captures(code)?;
    // `& !(A | B)` clears B as well as A.
    let group =
        regex::Regex::new(&format!(r"&=?\s*[!~]\s*\([^()]*\b{}\b", regex::escape(&c.name))).ok()?;
    if group.is_match(code) {
        return Some("clears");
    }
    let op = m.get(1).or(m.get(3)).map_or("", |o| o.as_str());
    Some(match (op.chars().next(), m.get(2).is_some()) {
        (Some('&'), true) => "clears",
        (Some('&'), false) => "masks with",
        (Some('|'), _) => "sets",
        _ => "toggles",
    })
}

/// What a value check knows beyond the two functions: the constants the
/// changed files define, and the functions the Rust calls, where a value
/// may have moved.
struct ValueCtx<'a> {
    values: Option<&'a crate::values::Values>,
    elsewhere: Vec<Tokens>,
}

impl ValueCtx<'_> {
    fn new<'a>(ctx: &Context<'a>) -> ValueCtx<'a> {
        ValueCtx {
            values: ctx.values,
            elsewhere: ctx.elsewhere.iter().map(|f| Tokens::of(f)).collect(),
        }
    }

    fn value(&self, c: &Constant) -> Option<u64> {
        self.values?.value(&c.name)
    }

    fn canonical(&self, c: &Constant) -> String {
        self.values.map_or_else(|| c.name.clone(), |v| v.canonical(&c.name))
    }

    fn defined(&self, c: &Constant) -> bool {
        self.values.is_some_and(|v| v.defined(&c.name))
    }

    /// Whether the constant appears in a function the Rust calls, so it
    /// moved rather than went missing.
    fn moved(&self, c: &Constant) -> bool {
        self.elsewhere.iter().any(|t| t.has(c))
    }

    /// Whether two constants stand for the same thing: one aliases the
    /// other, or both evaluate to the same number.
    fn same(&self, a: &Constant, b: &Constant) -> bool {
        a.words.ends_with(&b.words)
            || b.words.ends_with(&a.words)
            || self.canonical(a) == self.canonical(b)
            || self.value(a).is_some() && self.value(a) == self.value(b)
    }

    /// The constants in a unit's code that are values: not statics, not
    /// macros used as statements (`PANIC_UNIMPLEMENTED;`), and not in an
    /// assertion, which is compared as one.
    fn constants(&self, text: &str) -> Vec<Constant> {
        // A match arm's pattern (`PML4_L => panic!()`) is compared as a case.
        let text = text.split_once("=>").map_or(text, |(_, r)| r);
        let code = code_only(text);
        let t = code.trim_start();
        let assert = ["ASSERT", "DEBUG_ASSERT", "ZX_ASSERT", "ZX_DEBUG_ASSERT", "assert"]
            .iter()
            .any(|a| t.starts_with(a))
            || t.starts_with("debug_assert");
        if assert {
            return Vec::new();
        }
        static UNINIT_TEMPLATE_DECL: std::sync::LazyLock<regex::Regex> =
            std::sync::LazyLock::new(|| {
                regex::Regex::new(r"^\s*[\w:]+\s*<[^=;()]*>\s+\w+\s*;\s*$").unwrap()
            });
        if UNINIT_TEMPLATE_DECL.is_match(t) {
            return Vec::new();
        }
        constants(text)
            .into_iter()
            .filter(|c| !self.values.is_some_and(|v| v.is_static(&c.name)))
            .filter(|c| {
                let stmt = t.trim_end().trim_end_matches(';').trim_end();
                let path = stmt.chars().all(|ch| ch.is_alphanumeric() || ch == '_' || ch == ':');
                !(stmt == c.name || path && stmt.ends_with(&format!("::{}", c.name)))
            })
            .collect()
    }
}

/// Whether `code` extracts a bit field with the constant (`(v & MASK) >>
/// SHIFT`), which Rust often writes as an accessor.
fn extracts_field(code: &str, c: &Constant) -> bool {
    let re = regex::Regex::new(&format!(
        r"&\s*{0}\s*\)\s*>>|>>\s*{0}\b|>>[^&|;]*\)\s*&\s*{0}\b",
        regex::escape(&c.name)
    ));
    re.is_ok_and(|re| re.is_match(code))
}

fn show(v: u64) -> String {
    if v > 9 { format!("{v:#x}") } else { v.to_string() }
}

/// A constant with its value, when it is known: `X86_DR7_MASK (0x700)`.
fn with_value(c: &Constant, vc: &ValueCtx) -> String {
    match vc.value(c) {
        Some(v) => format!("{} ({})", c.name, show(v)),
        None => c.name.clone(),
    }
}

/// How one constant on only one side is reported. A mask or flag
/// operation is an issue, and so is a constant the change's own files
/// define; a constant from elsewhere (a header, which may name the same
/// value differently) is a note.
fn one_sided(
    c: &Constant,
    code: &str,
    other_literals: &[u128],
    side: &str,
    vc: &ValueCtx,
) -> Option<Note> {
    // A C++ value found in a function the Rust calls moved there.
    if side == "C++" && vc.moved(c) {
        return None;
    }
    let other_side = if side == "C++" { "Rust" } else { "C++" };
    if let Some(op) = mask_op(code, c) {
        let msg = if side == "C++" {
            format!("C++ {op} {} here, and the Rust doesn't", c.name)
        } else {
            format!("Rust also {op} {}, which the C++ doesn't", c.name)
        };
        let severity = if extracts_field(code, c) { Severity::Note } else { Severity::Issue };
        return Some(Note::new(severity, Category::Value, msg));
    }
    let literal = vc.value(c).is_some_and(|v| other_literals.contains(&u128::from(v)));
    let severity = if vc.defined(c) && !literal { Severity::Issue } else { Severity::Note };
    Some(Note::new(
        severity,
        Category::Value,
        format!(
            "only {side} uses {}; the {other_side} function never mentions it",
            with_value(c, vc)
        ),
    ))
}

/// Reports named constants, flags and masks on only one side of an aligned
/// pair of units, when the other function never mentions them: `C++ writes
/// 0, Rust writes X86_DR7_MASK`, or `Rust also clears X86_FLAGS_RF`.
/// Constants are compared by value where the changed files define them, so
/// a renamed constant is not a difference.
#[allow(clippy::too_many_arguments)]
fn value_diff(
    a: &str,
    b: &str,
    cpp_names: &Tokens,
    rust_names: &Tokens,
    explained: bool,
    vc: &ValueCtx,
    notes: &mut Vec<Note>,
) {
    let (ca, cb) = (vc.constants(a), vc.constants(b));
    let only_a: Vec<&Constant> = ca
        .iter()
        .filter(|c| !cb.iter().any(|d| vc.same(c, d)))
        .filter(|c| !rust_names.has(c))
        .collect();
    let only_b: Vec<&Constant> = cb
        .iter()
        .filter(|c| !ca.iter().any(|d| vc.same(c, d)))
        .filter(|c| !cpp_names.has(c))
        .collect();
    if only_a.is_empty() && only_b.is_empty() {
        return;
    }
    let (la, lb) = (literals(a), literals(b));
    let lit_only = |x: &[u128], y: &[u128]| -> Vec<u128> {
        x.iter().filter(|v| !y.contains(v)).copied().collect()
    };
    let (lit_a, lit_b) = (lit_only(&la, &lb), lit_only(&lb, &la));
    let shown = |v: &[u128]| {
        v.iter()
            .map(|&v| u64::try_from(v).map_or_else(|_| v.to_string(), show))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let names =
        |v: &[&Constant]| v.iter().map(|c| with_value(c, vc)).collect::<Vec<_>>().join(", ");
    let (code_a, code_b) = (masks_only(&code_only(a)), masks_only(&code_only(b)));
    if explained {
        let masked = only_a.iter().any(|c| mask_op(&code_a, c).is_some())
            || only_b.iter().any(|c| mask_op(&code_b, c).is_some());
        if !masked {
            return;
        }
    }
    let values = |v: &[&Constant]| -> Option<Vec<u64>> {
        let mut out: Vec<u64> = v.iter().map(|c| vc.value(c)).collect::<Option<_>>()?;
        out.sort();
        out.dedup();
        Some(out)
    };
    // One constant replaced by another: a difference when both are known
    // to stand for different numbers.
    if !only_a.is_empty() && !only_b.is_empty() {
        let severity = match (values(&only_a), values(&only_b)) {
            (Some(x), Some(y)) if x == y => return,
            (Some(_), Some(_)) => Severity::Issue,
            _ => Severity::Note,
        };
        notes.push(Note::new(
            severity,
            Category::Value,
            format!("C++ uses {} where Rust uses {}", names(&only_a), names(&only_b)),
        ));
        return;
    }
    // A literal and a constant: the same number spelled two ways, or a
    // different number.
    let literal_vs = |lits: &[u128], consts: &[&Constant]| -> Option<Severity> {
        let v = values(consts)?;
        if v.iter().any(|x| lits.contains(&u128::from(*x))) { None } else { Some(Severity::Issue) }
    };
    if !only_b.is_empty() && !lit_a.is_empty() {
        let severity = match values(&only_b) {
            Some(_) => match literal_vs(&lit_a, &only_b) {
                Some(s) => s,
                None => return,
            },
            None => Severity::Note,
        };
        notes.push(Note::new(
            severity,
            Category::Value,
            format!("C++ uses {} where Rust uses {}", shown(&lit_a), names(&only_b)),
        ));
        return;
    }
    if !only_a.is_empty() && !lit_b.is_empty() {
        let severity = literal_vs(&lit_b, &only_a).unwrap_or(Severity::Note);
        notes.push(Note::new(
            severity,
            Category::Value,
            format!("C++ uses {} where Rust uses {}", names(&only_a), shown(&lit_b)),
        ));
        return;
    }
    for c in &only_a {
        notes.extend(one_sided(c, &code_a, &lb, "C++", vc));
    }
    for c in &only_b {
        notes.extend(one_sided(c, &code_b, &la, "Rust", vc));
    }
}

/// Code with the logical operators `||` and `&&` spelled out, so they are
/// not read as masks.
fn masks_only(code: &str) -> String {
    code.replace("||", " or ").replace("&&", " and ")
}

/// A statement on one side only that uses a constant the other function
/// never mentions: the value is new or lost, whatever else the statement
/// does.
fn one_sided_constant(
    text: &str,
    u: &Unit,
    side: &str,
    other: &Tokens,
    vc: &ValueCtx,
    notes: &mut Vec<Note>,
) {
    if !matches!(u.kind, UnitKind::Stmt | UnitKind::Return)
        || notes.iter().any(|n| n.severity == Severity::Issue)
    {
        return;
    }
    let code = masks_only(&code_only(text));
    let found: Vec<Note> = vc
        .constants(text)
        .into_iter()
        .filter(|c| !other.has(c))
        .filter_map(|c| one_sided(&c, &code, &[], side, vc))
        .collect();
    // One note per statement: a mask operation if there is one, else one
    // note naming every constant, as severe as the most severe.
    if let Some(n) = found.iter().find(|n| !n.message.starts_with("only ")) {
        notes.push(n.clone());
        return;
    }
    let Some(first) = found.first() else {
        return;
    };
    let names: Vec<&str> = found
        .iter()
        .filter_map(|n| {
            n.message.strip_prefix(&format!("only {side} uses ")).and_then(|m| m.split(';').next())
        })
        .collect();
    let rest = first.message.split_once(';').map_or("", |(_, r)| r);
    let severity = if found.iter().any(|n| n.severity == Severity::Issue) {
        Severity::Issue
    } else {
        Severity::Note
    };
    notes.push(Note::new(
        severity,
        Category::Value,
        format!("only {side} uses {};{rest}", names.join(", ")),
    ));
}

/// The same value finding on several rows of one function says one thing:
/// keep the first.
fn dedupe_values(rows: &mut [Row]) {
    let mut seen: Vec<String> = Vec::new();
    for r in rows.iter_mut() {
        r.notes.retain(|n| {
            if n.category != Category::Value || n.severity != Severity::Issue {
                return true;
            }
            if seen.contains(&n.message) {
                return false;
            }
            seen.push(n.message.clone());
            true
        });
    }
}

/// Rank of a memory ordering, weakest first. Acquire and release are
/// ranked together: each is weaker than acq_rel, neither than the other.
fn ordering_rank(o: &str) -> u8 {
    match o.to_ascii_lowercase().as_str() {
        "relaxed" => 0,
        "consume" | "acquire" | "release" => 1,
        "acq_rel" | "acqrel" => 2,
        _ => 3,
    }
}

/// The memory orderings of the atomic operations in C++ or Rust source. A
/// C++ operation with no explicit order is sequentially consistent.
fn orderings(text: &str, lang: Lang) -> Vec<String> {
    static CPP_OP: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?:\.|->)(?:load|store|exchange|fetch_\w+|compare_exchange_\w+)\s*\(")
            .unwrap()
    });
    static CPP_ORDER: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"memory_order(?:_|::)(relaxed|consume|acquire|release|acq_rel|seq_cst)")
            .unwrap()
    });
    static RUST_ORDER: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\bOrdering::(Relaxed|Acquire|Release|AcqRel|SeqCst)\b").unwrap()
    });
    let re = match lang {
        Lang::Rust => &RUST_ORDER,
        Lang::Cpp => &CPP_ORDER,
    };
    let mut out: Vec<String> = re.captures_iter(text).map(|c| c[1].to_ascii_lowercase()).collect();
    if lang == Lang::Cpp && out.is_empty() && CPP_OP.is_match(text) {
        out.push("seq_cst".to_string());
    }
    out
}

/// Rust atomics with a different memory ordering from the C++ they replace:
/// an issue when weaker, a note when stronger.
/// An assertion's ordering barely matters, so there it is only a note.
fn ordering_diff(cpp: &str, rust: &str, asserts: bool, notes: &mut Vec<Note>) {
    let (a, b) = (orderings(cpp, Lang::Cpp), orderings(rust, Lang::Rust));
    if a.is_empty() || b.is_empty() {
        return;
    }
    let weakest = |v: &[String]| v.iter().map(|o| ordering_rank(o)).min().unwrap_or(3);
    let show = |v: &[String]| {
        let mut v = v.to_vec();
        v.dedup();
        v.join(", ")
    };
    let (ra, rb) = (weakest(&a), weakest(&b));
    let (severity, word) = match rb.cmp(&ra) {
        std::cmp::Ordering::Less if !asserts => (Severity::Issue, "weaker"),
        std::cmp::Ordering::Less => (Severity::Note, "weaker"),
        std::cmp::Ordering::Greater => (Severity::Note, "stronger"),
        std::cmp::Ordering::Equal => return,
    };
    notes.push(Note::new(
        severity,
        Category::Atomic,
        format!("Rust uses a {word} memory ordering ({}) than C++ ({})", show(&b), show(&a)),
    ));
}

#[cfg(test)]
mod ordering_tests {
    use super::*;

    fn diff(cpp: &str, rust: &str) -> Vec<Note> {
        let mut notes = Vec::new();
        ordering_diff(cpp, rust, false, &mut notes);
        notes
    }

    #[test]
    fn default_cpp_ordering_is_seq_cst() {
        let n = diff("state_.exchange(0);", "self.state.swap(0, Ordering::Relaxed);");
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].severity, Severity::Issue);
        assert!(n[0].message.contains("weaker"), "{}", n[0].message);
    }

    #[test]
    fn same_or_stronger_ordering() {
        assert!(diff("x.load(ktl::memory_order_acquire);", "x.load(Ordering::Acquire)").is_empty());
        let n = diff("x.store(1, std::memory_order_relaxed);", "x.store(1, Ordering::SeqCst)");
        assert_eq!(n[0].severity, Severity::Note);
    }

    #[test]
    fn no_atomics_no_finding() {
        assert!(diff("count_ = 0;", "self.count = 0;").is_empty());
    }
}
