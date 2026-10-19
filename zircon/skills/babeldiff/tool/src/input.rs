//! Turns files, patches and git revisions into analysis inputs.

use crate::analyze::Inputs;
use crate::cpp::DeclComments;
use crate::extract::{self, attach_decl_comments};
use crate::model::{Function, Lang, UnitKind};
use crate::patch::FilePatch;
use std::collections::BTreeSet;

/// One version of a file and the lines the change touched in it.
#[derive(Clone, Debug)]
pub struct Version {
    pub path: String,
    pub text: String,
    /// Lines removed (old side) or added (new side). `None` means every
    /// function in the file is of interest.
    pub changed: Option<BTreeSet<usize>>,
}

/// A run of changed C++ lines that stay C++.
#[derive(Clone, Debug)]
pub struct CppChange {
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
    /// The functions the lines are in.
    pub functions: Vec<String>,
    /// The first changed line, trimmed.
    pub text: String,
}

/// The files a change touches, by language and side.
#[derive(Clone, Debug, Default)]
pub struct ChangeSet {
    /// C++ files before the change.
    pub cpp_old: Vec<Version>,
    /// C++ files after the change (used to follow calls into FFI shims).
    pub cpp_new: Vec<Version>,
    /// Rust files after the change.
    pub rust_new: Vec<Version>,
}

impl ChangeSet {
    /// Whether this compares whole files (`from_files`) rather than a change,
    /// so it says nothing about what was added or deleted.
    pub fn is_file_comparison(&self) -> bool {
        self.cpp_new.is_empty()
            && self
                .cpp_old
                .iter()
                .chain(&self.rust_new)
                .all(|v| v.changed.is_none())
    }

    /// Every function of the given files.
    pub fn from_files(files: &[(String, String)]) -> ChangeSet {
        let mut cs = ChangeSet::default();
        for (path, text) in files {
            let v = Version {
                path: path.clone(),
                text: text.clone(),
                changed: None,
            };
            match Lang::from_path(path) {
                Some(Lang::Cpp) => cs.cpp_old.push(v),
                Some(Lang::Rust) => cs.rust_new.push(v),
                None => {}
            }
        }
        cs
    }

    /// Builds a change set from a parsed patch. `full_text` may supply the
    /// complete contents of a blob (by abbreviated id); otherwise the file is
    /// reconstructed from the hunks alone.
    pub fn from_patch(
        files: &[FilePatch],
        full_text: &mut dyn FnMut(&str) -> Option<String>,
    ) -> ChangeSet {
        let mut cs = ChangeSet::default();
        for f in files {
            let Some(lang) = Lang::from_path(f.path()) else {
                continue;
            };
            let old_text = |ft: &mut dyn FnMut(&str) -> Option<String>| {
                f.old_blob
                    .as_deref()
                    .and_then(&mut *ft)
                    .unwrap_or_else(|| f.sparse_old())
            };
            let new_text = |ft: &mut dyn FnMut(&str) -> Option<String>| {
                f.new_blob
                    .as_deref()
                    .and_then(&mut *ft)
                    .unwrap_or_else(|| f.sparse_new())
            };
            match lang {
                Lang::Cpp => {
                    if let Some(p) = &f.old_path {
                        cs.cpp_old.push(Version {
                            path: p.clone(),
                            text: old_text(full_text),
                            changed: Some(f.removed()),
                        });
                    }
                    if let Some(p) = &f.new_path {
                        cs.cpp_new.push(Version {
                            path: p.clone(),
                            text: new_text(full_text),
                            changed: Some(f.added()),
                        });
                    }
                }
                Lang::Rust => {
                    if let Some(p) = &f.new_path {
                        cs.rust_new.push(Version {
                            path: p.clone(),
                            text: new_text(full_text),
                            changed: Some(f.added()),
                        });
                    }
                }
            }
        }
        cs
    }
}

/// Fraction of a function's body lines that the change touched.
fn changed_fraction(f: &Function, changed: &BTreeSet<usize>) -> f64 {
    let body_start = f
        .units
        .iter()
        .find(|u| u.kind == UnitKind::Signature)
        .map_or(f.start_line, |u| u.start_line);
    let span = f.end_line.saturating_sub(body_start) + 1;
    let n = changed.range(body_start..=f.end_line).count();
    n as f64 / span as f64
}

/// Changed C++ lines that are not part of the port: not FFI forwarders, FFI
/// declarations, `*_ffi.cc` helpers or includes. What is left is C++ that
/// stays C++ but behaves differently, which function-by-function comparison
/// never shows.
fn stays_cpp(v: &Version, functions: &[Function]) -> Vec<CppChange> {
    let Some(changed) = &v.changed else {
        return Vec::new();
    };
    if v.path.ends_with("_ffi.cc") || v.path.ends_with("_ffi.cpp") {
        return Vec::new();
    }
    let lines = extract::split_lines(&v.text);
    // A body that now only calls into Rust (and unwraps what comes back) is
    // the port's other half.
    let forwards = |f: &Function| {
        f.base.starts_with("cpp_")
            || f.calls.iter().any(|c| c.starts_with("rust_"))
                && f.calls
                    .iter()
                    .all(|c| c.starts_with("rust_") || c == "uninitialized")
    };
    let skip_fn: Vec<(usize, usize)> = functions
        .iter()
        .filter(|f| forwards(f))
        .map(|f| (f.start_line, f.end_line))
        .collect();
    static FFI: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(concat!(
            r#"\b(?:rust|cpp)_\w+\b|^extern\s+"C"|^#|^(?:class|struct)\s+\w+;$|^__(?:BEGIN|END)_CDECLS"#,
            // Layout constants and checks for state shared with Rust.
            r#"|^static_assert\s*\(\s*(?:sizeof|alignof|offsetof)|\bk\w*(?:Size|Align|Alignment|Offset)\s*="#,
            r#"|^// Copyright"#,
        ))
        .unwrap()
    });
    // Statements can span lines; an FFI declaration's continuation lines
    // are FFI too.
    let mut ffi = vec![false; lines.len() + 1];
    let mut start = 1;
    for n in 1..=lines.len() {
        let t = lines[n - 1].trim();
        let prev = (1..n)
            .rev()
            .map(|k| lines[k - 1].trim())
            .find(|l| !l.is_empty());
        let begins = prev.is_none_or(|p| {
            p.ends_with([';', '{', '}', ':']) || p.starts_with("//") || p.starts_with('#')
        });
        if begins {
            start = n;
        }
        if FFI.is_match(t) {
            ffi[start..=n].fill(true);
        } else if ffi[start] && !begins {
            ffi[n] = true;
        }
    }
    // A comment directly above FFI plumbing is about the plumbing.
    for n in (1..lines.len()).rev() {
        if ffi[n + 1] && lines[n - 1].trim().starts_with("//") {
            ffi[n] = true;
        }
    }
    let keep = |n: usize| {
        let t = lines.get(n - 1).map_or("", |l| l.trim());
        !t.is_empty()
            && !t.starts_with("}")
            && !matches!(t, "{" | "public:" | "private:" | "protected:")
            && !ffi.get(n).copied().unwrap_or(false)
            && !skip_fn.iter().any(|&(a, b)| (a..=b).contains(&n))
    };
    let mut out: Vec<CppChange> = Vec::new();
    for &n in changed.iter().filter(|&&n| keep(n)) {
        let within = functions
            .iter()
            .find(|f| (f.start_line..=f.end_line).contains(&n))
            .map(|f| f.name.clone());
        let c = match out.last_mut() {
            Some(c) if n <= c.end_line + 2 => {
                c.end_line = n;
                c
            }
            _ => {
                out.push(CppChange {
                    path: v.path.clone(),
                    start_line: n,
                    end_line: n,
                    functions: Vec::new(),
                    text: lines
                        .get(n - 1)
                        .map_or(String::new(), |l| l.trim().to_string()),
                });
                out.last_mut().unwrap()
            }
        };
        if let Some(f) = within {
            if !c.functions.contains(&f) {
                c.functions.push(f);
            }
        }
    }
    out
}

/// The comments before a function's signature.
fn leading_comments(f: &Function) -> impl Iterator<Item = &crate::model::Unit> {
    f.units
        .iter()
        .take_while(|u| u.kind != UnitKind::Signature)
        .filter(|u| u.kind == UnitKind::Comment)
}

/// Extracts functions and selects those the change rewrote.
pub fn build_inputs(cs: &ChangeSet, min_changed: f64) -> Inputs {
    let mut inputs = Inputs::default();

    let mut decls: DeclComments = DeclComments::new();
    let mut cpp_all: Vec<(Function, Option<&BTreeSet<usize>>)> = Vec::new();
    for v in &cs.cpp_old {
        let e = extract::extract(Lang::Cpp, &v.path, &v.text);
        for (k, u) in e.decl_comments {
            decls.entry(k).or_insert(u);
        }
        for (k, b) in e.bases {
            inputs.cpp_bases.entry(k).or_insert(b);
        }
        cpp_all.extend(e.functions.into_iter().map(|f| (f, v.changed.as_ref())));
    }
    let mut cpp: Vec<Function> = cpp_all
        .into_iter()
        .filter(|(f, ch)| ch.is_none_or(|c| changed_fraction(f, c) >= min_changed))
        .map(|(f, _)| f)
        .collect();
    attach_decl_comments(&mut cpp, &decls, &inputs.cpp_bases);

    // Doc comments that are still in the C++ after the change.
    let mut kept: std::collections::HashSet<Vec<String>> = std::collections::HashSet::new();
    let mut cpp_new = Vec::new();
    for v in &cs.cpp_new {
        let e = extract::extract(Lang::Cpp, &v.path, &v.text);
        for units in e.decl_comments.values() {
            kept.extend(units.iter().map(|u| u.features.comment.clone()));
        }
        for f in &e.functions {
            kept.extend(leading_comments(f).map(|u| u.features.comment.clone()));
        }
        cpp_new.push(e);
    }
    for f in &mut cpp {
        let n = leading_comments(f).count();
        for u in f.units.iter_mut().take(n) {
            if !u.features.comment.is_empty() && kept.contains(&u.features.comment) {
                u.features.still_in_cpp = true;
            }
        }
    }
    inputs.cpp = cpp;
    // Function-like macros the old C++ defines, which a conversion should
    // keep as macros rather than expand at each use.
    static DEFINE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?m)^[ \t]*#[ \t]*define[ \t]+([A-Za-z_]\w*)\(").unwrap()
    });
    for v in cs.cpp_old.iter().chain(&cs.cpp_new) {
        inputs.values.add_cpp(&v.text);
    }
    for v in &cs.rust_new {
        inputs.values.add_rust(&v.text);
    }
    for v in &cs.cpp_old {
        for c in DEFINE.captures_iter(&v.text) {
            let name = c[1].to_string();
            if !inputs.cpp_macros.contains(&name) {
                inputs.cpp_macros.push(name);
            }
        }
    }
    inputs.cpp_changed_paths = cs
        .cpp_old
        .iter()
        .chain(&cs.cpp_new)
        .filter(|v| v.changed.as_ref().is_none_or(|c| !c.is_empty()))
        .map(|v| v.path.clone())
        .collect();

    for (v, e) in cs.cpp_new.iter().zip(&cpp_new) {
        inputs.cpp_changes.extend(stays_cpp(v, &e.functions));
    }

    if cs.cpp_new.is_empty() {
        for f in &inputs.cpp {
            if f.base.starts_with("cpp_") {
                inputs.cpp_helpers.push(f.clone());
            }
        }
    }
    for (v, e) in cs.cpp_new.iter().zip(cpp_new) {
        let has_changes = v.changed.as_ref().is_none_or(|c| !c.is_empty());
        for (k, b) in e.bases {
            inputs.cpp_bases.entry(k).or_insert(b);
        }
        for f in e.functions {
            if has_changes && f.base.starts_with("cpp_") {
                inputs.cpp_helpers.push(f.clone());
            }
            inputs
                .cpp_new_calls
                .entry(f.name.clone())
                .or_default()
                .extend(f.calls);
        }
    }

    for v in &cs.rust_new {
        for f in extract::extract(Lang::Rust, &v.path, &v.text).functions {
            let selected = v
                .changed
                .as_ref()
                .is_none_or(|c| changed_fraction(&f, c) >= min_changed);
            if selected {
                inputs.rust.push(f.clone());
            }
            inputs.all_rust.push(f);
        }
    }
    inputs
}
