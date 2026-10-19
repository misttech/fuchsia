//! Rubric lints: syntax-level checks on the changed files that don't need a
//! C++/Rust pairing.
//!
//! - `extern-signature`: a Rust `extern "C"` declaration of a `cpp_*`
//!   helper, or a `rust_*` export, whose parameters or return type don't
//!   match the C++ side.
//! - `unsafe-safety`: an `unsafe` block or `unsafe impl` without a
//!   `// SAFETY:` comment, or an `unsafe fn` without a `# Safety` section.
//! - `shim-logic`: an FFI shim (`rust_*` export or `cpp_*` helper) that
//!   branches or loops instead of only forwarding and converting results.
//!   Every function in an `_ffi.cc` file counts as a shim.
//! - `mangled-symbol`: Rust that declares, calls or defines a C++ function
//!   by its mangled symbol (`_ZN6Thread...`) instead of going through a
//!   `cpp_*` helper or `rust_*` export.
//! - `stale-cpp-reference`: a comment on new Rust that describes C++ the
//!   change deletes, by naming a deleted C++ symbol or by comparing the Rust
//!   port with the C++ it replaces.

use crate::check::Severity;
use crate::input::{ChangeSet, Version};
use crate::model::{Function, Lang, UnitKind};
use crate::ts;
use regex::Regex;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::LazyLock;
use tree_sitter::Node;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LintKind {
    ExternSignature,
    UnsafeSafety,
    ShimLogic,
    FilePlacement,
    Provenance,
    InventedLifetime,
    UnsafeDensity,
    MangledSymbol,
    StaleCppReference,
}

impl LintKind {
    /// Short, stable name for output formats.
    pub fn name(self) -> &'static str {
        match self {
            LintKind::ExternSignature => "extern-signature",
            LintKind::UnsafeSafety => "unsafe-safety",
            LintKind::ShimLogic => "shim-logic",
            LintKind::FilePlacement => "file-placement",
            LintKind::Provenance => "provenance-comment",
            LintKind::InventedLifetime => "invented-lifetime",
            LintKind::UnsafeDensity => "unsafe-density",
            LintKind::MangledSymbol => "mangled-symbol",
            LintKind::StaleCppReference => "stale-cpp-reference",
        }
    }

    /// The part of Zircon's C++ to Rust porting rubric the lint checks.
    pub fn rubric(self) -> &'static str {
        match self {
            LintKind::ExternSignature => "FFI declarations match on both sides",
            LintKind::UnsafeSafety => "every unsafe block and fn documents its safety",
            LintKind::ShimLogic => "FFI shims only forward",
            LintKind::FilePlacement => "each C++ file becomes one Rust file named after it",
            LintKind::Provenance => "no comments about where the code was ported from",
            LintKind::InventedLifetime => "references borrow from what they point into",
            LintKind::UnsafeDensity => "safe facades instead of unsafe code at each use",
            LintKind::MangledSymbol => {
                "C++ and Rust call each other through cpp_ and rust_ FFI functions"
            }
            LintKind::StaleCppReference => "comments don't describe C++ that the change deletes",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Lint {
    pub kind: LintKind,
    pub severity: Severity,
    pub path: String,
    pub line: usize,
    pub message: String,
    /// The other side, for a lint that compares two places.
    pub related: Option<(String, usize)>,
}

/// Runs every lint over the change set's new C++ and Rust.
pub fn lint(cs: &ChangeSet) -> Vec<Lint> {
    let mut out = Vec::new();
    out.extend(extern_signatures(cs));
    for v in &cs.rust_new {
        out.extend(unsafe_safety(v));
        out.extend(provenance(v));
        out.extend(mangled_symbols(v));
    }
    out.extend(rust_function_lints(cs));
    out.extend(shim_logic(cs));
    out.extend(stale_cpp_references(cs));
    out.sort_by(|a, b| (&a.path, a.line, a.kind).cmp(&(&b.path, b.line, b.kind)));
    out.dedup();
    out
}

/// Whether any of `lines` changed in `v` (every line counts when the
/// version has no change information).
fn touched(v: &Version, lines: std::ops::RangeInclusive<usize>) -> bool {
    v.changed
        .as_ref()
        .is_none_or(|c: &BTreeSet<usize>| c.range(lines).next().is_some())
}

// ---------------------------------------------------------------------------
// extern "C" signatures

/// A parameter or return type reduced to what the ABI cares about.
#[derive(Clone, Debug, PartialEq)]
struct CType {
    /// Levels of pointer (or reference) indirection.
    ptrs: usize,
    /// Whether the innermost pointee is const.
    const_pointee: bool,
    /// Canonical base type: a Rust primitive name (`u32`, `usize`, `void`,
    /// `char`, `bool`), or the lowercase type name when it isn't one.
    base: String,
    /// A function pointer; only its pointer-ness is compared.
    fn_ptr: bool,
    /// Source text, for messages.
    text: String,
}

impl CType {
    fn known(&self) -> bool {
        scalar_width(&self.base).is_some() || self.base == "void"
    }
}

#[derive(Clone, Debug)]
struct Sig {
    name: String,
    path: String,
    line: usize,
    params: Vec<CType>,
    /// `None` for `void` / no return type.
    ret: Option<CType>,
    /// Defined here (not just declared).
    definition: bool,
    touched: bool,
}

/// Width in bytes and signedness of a canonical scalar.
fn scalar_width(base: &str) -> Option<(u8, bool)> {
    Some(match base {
        "u8" => (1, false),
        "i8" => (1, true),
        "char" => (1, true),
        "u16" => (2, false),
        "i16" => (2, true),
        "u32" => (4, false),
        "i32" => (4, true),
        "u64" | "usize" => (8, false),
        "i64" | "isize" => (8, true),
        "bool" => (1, false),
        "f32" => (4, true),
        "f64" => (8, true),
        _ => return None,
    })
}

/// Maps a C, C++ or Rust type name to its canonical form.
fn canonical(name: &str) -> String {
    let n: String = name.split_whitespace().collect::<Vec<_>>().join(" ");
    let last = n.rsplit("::").next().unwrap_or(&n).trim();
    let c = match last {
        "uint8_t" | "unsigned char" | "c_uchar" => "u8",
        "int8_t" | "signed char" | "c_schar" => "i8",
        "char" | "c_char" => "char",
        "uint16_t" | "unsigned short" | "c_ushort" => "u16",
        "int16_t" | "short" | "c_short" => "i16",
        "uint32_t" | "unsigned" | "unsigned int" | "c_uint" | "zx_handle_t" | "zx_rights_t"
        | "zx_signals_t" | "cpu_num_t" | "zx_obj_type_t" => "u32",
        "int32_t" | "int" | "c_int" | "zx_status_t" => "i32",
        "uint64_t" | "unsigned long" | "unsigned long long" | "c_ulong" | "c_ulonglong"
        | "zx_koid_t" | "zx_off_t" => "u64",
        "int64_t" | "long" | "long long" | "c_long" | "c_longlong" | "zx_time_t"
        | "zx_duration_t" | "zx_instant_mono_t" | "zx_instant_boot_t" | "zx_ticks_t"
        | "zx_duration_mono_t" | "zx_duration_boot_t" => "i64",
        "size_t" | "uintptr_t" | "vaddr_t" | "paddr_t" | "zx_vaddr_t" | "zx_paddr_t" => "usize",
        "ssize_t" | "intptr_t" | "ptrdiff_t" => "isize",
        "void" | "c_void" | "()" => "void",
        "float" => "f32",
        "double" => "f64",
        other => return other.to_ascii_lowercase(),
    };
    c.to_string()
}

fn mask_cpp(src: &str) -> String {
    static ATTR: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\bFFI_ALWAYS_INLINE\b|__attribute__\s*\(\((?:[^()]|\([^()]*\))*\)\)|\b(?:__)?TA_[A-Z_]+\b(?:\s*\((?:[^()]|\([^()]*\))*\))?|\b__(?:BEGIN|END)_CDECLS\b")
            .unwrap()
    });
    ATTR.replace_all(src, |c: &regex::Captures| {
        c[0].chars()
            .map(|ch| if ch == '\n' { '\n' } else { ' ' })
            .collect::<String>()
    })
    .into_owned()
}

fn is_ffi_name(n: &str) -> bool {
    n.starts_with("cpp_") || n.starts_with("rust_")
}

/// The C++ declarations and definitions of `cpp_*` and `rust_*` functions.
fn cpp_sigs(v: &Version) -> Vec<Sig> {
    let src = mask_cpp(&v.text);
    let tree = ts::parse(Lang::Cpp, &src);
    let bytes = src.as_bytes();
    let mut out = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(n) = stack.pop() {
        match n.kind() {
            "function_definition" | "declaration" | "field_declaration" => {
                if let Some(s) = cpp_sig(n, bytes, v) {
                    out.push(s);
                }
            }
            _ => stack.extend(ts::named_children(n)),
        }
    }
    out
}

fn cpp_sig(n: Node, src: &[u8], v: &Version) -> Option<Sig> {
    let ty = n.child_by_field_name("type")?;
    let mut d = n.child_by_field_name("declarator")?;
    let mut ret_ptrs = 0;
    let mut ret_const_inner = false;
    // Peel pointer declarators off the return type: `T* f(...)`.
    loop {
        match d.kind() {
            "pointer_declarator" | "reference_declarator" => {
                ret_ptrs += 1;
                d = d
                    .child_by_field_name("declarator")
                    .or_else(|| ts::named_children(d).into_iter().last())?;
            }
            "function_declarator" => break,
            _ => return None,
        }
    }
    let name_node = d.child_by_field_name("declarator")?;
    let name = ts::text(name_node, src).to_string();
    if !is_ffi_name(&name) {
        return None;
    }
    for c in ts::children(n) {
        if c.kind() == "type_qualifier" && ts::text(c, src) == "const" {
            ret_const_inner = true;
        }
    }
    let base = canonical(ts::text(ty, src));
    let ret = (ret_ptrs > 0 || base != "void").then(|| CType {
        ptrs: ret_ptrs,
        const_pointee: ret_const_inner,
        base,
        fn_ptr: false,
        text: ts::text(ty, src).to_string(),
    });
    let params_node = d.child_by_field_name("parameters")?;
    let mut params = Vec::new();
    for p in ts::named_children(params_node) {
        match p.kind() {
            "parameter_declaration" | "optional_parameter_declaration" => {
                params.push(cpp_param(p, src)?);
            }
            "variadic_parameter" | "comment" => {}
            _ => return None,
        }
    }
    // `f(void)` takes no parameters.
    if params.len() == 1 && params[0].base == "void" && params[0].ptrs == 0 {
        params.clear();
    }
    Some(Sig {
        name,
        path: v.path.clone(),
        line: ts::line(n),
        params,
        ret,
        definition: n.kind() == "function_definition",
        touched: touched(v, ts::line(n)..=ts::end_line(n)),
    })
}

fn cpp_param(p: Node, src: &[u8]) -> Option<CType> {
    let ty = p.child_by_field_name("type")?;
    let mut const_pointee = ts::children(p)
        .iter()
        .any(|c| c.kind() == "type_qualifier" && ts::text(*c, src) == "const");
    // Type constructors from the base type out to the name; the last one
    // is the parameter's own type.
    let mut ctors: Vec<&str> = Vec::new();
    let mut fn_ptr = false;
    let mut d = p.child_by_field_name("declarator");
    while let Some(n) = d {
        match n.kind() {
            "array_declarator" | "abstract_array_declarator" => {
                ctors.push("array");
                d = n.child_by_field_name("declarator");
            }
            "pointer_declarator"
            | "abstract_pointer_declarator"
            | "reference_declarator"
            | "abstract_reference_declarator" => {
                ctors.push("ptr");
                d = n.child_by_field_name("declarator").or_else(|| {
                    ts::named_children(n)
                        .into_iter()
                        .find(|c| c.kind().contains("declarator"))
                });
            }
            "function_declarator" | "abstract_function_declarator" => {
                fn_ptr = true;
                break;
            }
            "parenthesized_declarator" | "abstract_parenthesized_declarator" => {
                d = ts::named_children(n).into_iter().next();
            }
            _ => break,
        }
    }
    // `T* const p` is a const pointer, not a pointer to const.
    if ts::text(ty, src).starts_with("const ") {
        const_pointee = true;
    }
    // An array parameter is a pointer to its first element; an array
    // further in is compared only as an array.
    let decays = ctors.last() == Some(&"array");
    let ptrs = ctors.iter().filter(|c| **c == "ptr").count() + usize::from(decays);
    let nested_array = ctors.iter().filter(|c| **c == "array").count() > usize::from(decays);
    Some(CType {
        ptrs,
        const_pointee,
        base: if nested_array {
            "array".into()
        } else {
            canonical(ts::text(ty, src))
        },
        fn_ptr,
        text: ts::text(p, src).to_string(),
    })
}

/// Rust `extern "C"` block declarations and `extern "C"` function
/// definitions named `cpp_*` or `rust_*`.
fn rust_sigs(v: &Version) -> Vec<Sig> {
    let tree = ts::parse(Lang::Rust, &v.text);
    let src = v.text.as_bytes();
    let mut out = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(n) = stack.pop() {
        match n.kind() {
            "foreign_mod_item" => {
                for body in ts::named_children(n) {
                    if body.kind() == "declaration_list" {
                        for f in ts::named_children(body) {
                            if f.kind() == "function_signature_item" {
                                out.extend(rust_sig(f, src, v, false));
                            }
                        }
                    }
                }
            }
            "function_item" => {
                let ext = ts::children(n).iter().any(|c| {
                    c.kind() == "function_modifiers" && ts::text(*c, src).contains("extern")
                });
                if ext {
                    out.extend(rust_sig(n, src, v, true));
                }
                stack.extend(ts::named_children(n));
            }
            _ => stack.extend(ts::named_children(n)),
        }
    }
    out
}

fn rust_sig(n: Node, src: &[u8], v: &Version, definition: bool) -> Option<Sig> {
    let name = ts::text(n.child_by_field_name("name")?, src).to_string();
    if !is_ffi_name(&name) {
        return None;
    }
    let mut params = Vec::new();
    for p in ts::named_children(n.child_by_field_name("parameters")?) {
        match p.kind() {
            "parameter" => params.push(rust_type(p.child_by_field_name("type")?, src)),
            "variadic_parameter" | "line_comment" | "block_comment" | "attribute_item" => {}
            _ => return None,
        }
    }
    let ret = n
        .child_by_field_name("return_type")
        .map(|t| rust_type(t, src))
        .filter(|t| !(t.ptrs == 0 && (t.base == "void" || t.base == "!")));
    Some(Sig {
        name,
        path: v.path.clone(),
        line: ts::line(n),
        params,
        ret,
        definition,
        touched: touched(v, ts::line(n)..=ts::end_line(n)),
    })
}

fn rust_type(t: Node, src: &[u8]) -> CType {
    let text = ts::text(t, src).to_string();
    let mut ptrs = 0;
    let mut const_pointee = false;
    let mut cur = t;
    loop {
        match cur.kind() {
            "pointer_type" | "reference_type" => {
                ptrs += 1;
                let mutable = ts::children(cur)
                    .iter()
                    .any(|c| c.kind() == "mutable_specifier");
                const_pointee = !mutable;
                match cur.child_by_field_name("type") {
                    Some(inner) => cur = inner,
                    None => break,
                }
            }
            // `Option<&T>`, `Option<NonNull<T>>` and `NonNull<T>` are
            // pointers in the ABI.
            "generic_type" => {
                let head = cur
                    .child_by_field_name("type")
                    .map(|h| ts::text(h, src))
                    .unwrap_or("");
                let head = head.rsplit("::").next().unwrap_or(head);
                let arg = cur
                    .child_by_field_name("type_arguments")
                    .and_then(|a| ts::named_children(a).into_iter().next());
                match (head, arg) {
                    ("Option", Some(a)) => cur = a,
                    ("NonNull", Some(a)) => {
                        ptrs += 1;
                        const_pointee = false;
                        cur = a;
                    }
                    _ => break,
                }
            }
            _ => break,
        }
    }
    let fn_ptr = cur.kind() == "function_type";
    CType {
        ptrs,
        const_pointee,
        base: match cur.kind() {
            "array_type" => "array".into(),
            "never_type" => "!".into(),
            _ => canonical(ts::text(cur, src)),
        },
        fn_ptr,
        text,
    }
}

/// How two types differ, as (severity, description), or `None` when they
/// agree as far as can be told. `cpp_receives` says which side gets the
/// value (and may write through it, if it is a pointer).
fn type_diff(c: &CType, r: &CType, cpp_receives: bool) -> Option<(Severity, String)> {
    // A function pointer is a pointer; nothing more is compared.
    let c_ptrs = c.ptrs + usize::from(c.fn_ptr);
    let r_ptrs = r.ptrs + usize::from(r.fn_ptr);
    if c.fn_ptr || r.fn_ptr {
        // The other side may name the function pointer type with a typedef
        // or alias; only a plain scalar is plainly wrong.
        let other = if c.fn_ptr { r } else { c };
        let wrong = other.ptrs == 0 && !other.fn_ptr && other.known();
        return wrong.then(|| (Severity::Issue, "function pointer vs a value".to_string()));
    }
    if c_ptrs != r_ptrs {
        // `void*` stands for any pointer, including a pointer to a pointer.
        let void_ptr = |t: &CType| t.base == "void" && t.ptrs >= 1;
        if (void_ptr(c) && r_ptrs >= 1) || (void_ptr(r) && c_ptrs >= 1) {
            return None;
        }
        return Some((
            Severity::Issue,
            format!("{c_ptrs} level(s) of pointer vs {r_ptrs}"),
        ));
    }
    if c_ptrs > 0 {
        if c.base == "void" || r.base == "void" {
            return None;
        }
        let (cw, rw) = (scalar_width(&c.base), scalar_width(&r.base));
        if cw.is_some() && rw.is_some() && cw != rw && !chars_match(&c.base, &r.base) {
            return Some((
                Severity::Issue,
                format!("points to {} vs {}", c.base, r.base),
            ));
        }
        // The side receiving a pointer writing through what the sender
        // treats as const is worth a look; the reverse is harmless.
        if cpp_receives && !c.const_pointee && r.const_pointee {
            return Some((
                Severity::Note,
                "C++ may write through a pointer Rust treats as const".to_string(),
            ));
        }
        if !cpp_receives && c.const_pointee && !r.const_pointee {
            return Some((
                Severity::Note,
                "Rust may write through a pointer C++ treats as const".to_string(),
            ));
        }
        return None;
    }
    if (c.base == "void") != (r.base == "void") {
        return Some((Severity::Issue, "void vs a value".to_string()));
    }
    let (Some((cw, cs)), Some((rw, rs))) = (scalar_width(&c.base), scalar_width(&r.base)) else {
        return None;
    };
    if c.base == r.base || chars_match(&c.base, &r.base) {
        return None;
    }
    if cw != rw {
        return Some((Severity::Issue, format!("{}-byte vs {}-byte value", cw, rw)));
    }
    if cs != rs || c.base == "bool" || r.base == "bool" {
        return Some((Severity::Note, format!("{} vs {}", c.base, r.base)));
    }
    None
}

/// C `char` is `u8` or `i8` depending on the target; either Rust type
/// matches it.
fn chars_match(a: &str, b: &str) -> bool {
    matches!((a, b), ("char", "u8" | "i8") | ("u8" | "i8", "char"))
}

fn extern_signatures(cs: &ChangeSet) -> Vec<Lint> {
    let mut cpp: HashMap<String, Vec<Sig>> = HashMap::new();
    for v in &cs.cpp_new {
        for s in cpp_sigs(v) {
            cpp.entry(s.name.clone()).or_default().push(s);
        }
    }
    let mut had_problem: BTreeSet<String> = BTreeSet::new();
    let mut out = Vec::new();
    for v in &cs.rust_new {
        for r in rust_sigs(v) {
            // A Rust declaration imports a C++ definition; a Rust
            // definition exports to a C++ declaration.
            let Some(cands) = cpp.get(&r.name) else {
                continue;
            };
            let Some(c) = cands
                .iter()
                .find(|c| c.definition != r.definition)
                .or_else(|| cands.first())
            else {
                continue;
            };
            if !(r.touched || c.touched) {
                continue;
            }
            let mut problems: Vec<(Severity, String)> = Vec::new();
            if c.params.len() != r.params.len() {
                problems.push((
                    Severity::Issue,
                    format!(
                        "C++ takes {} parameter{}, Rust {}",
                        c.params.len(),
                        if c.params.len() == 1 { "" } else { "s" },
                        r.params.len()
                    ),
                ));
            } else {
                for (i, (a, b)) in c.params.iter().zip(&r.params).enumerate() {
                    // Rust calls a C++ definition, or C++ calls a Rust one.
                    if let Some((sev, why)) = type_diff(a, b, !r.definition) {
                        problems.push((
                            sev,
                            format!(
                                "parameter {}: C++ `{}`, Rust `{}` ({why})",
                                i + 1,
                                a.text.trim(),
                                b.text.trim()
                            ),
                        ));
                    }
                }
            }
            let void = CType {
                ptrs: 0,
                const_pointee: false,
                base: "void".into(),
                fn_ptr: false,
                text: "void".into(),
            };
            let (cr, rr) = (
                c.ret.as_ref().unwrap_or(&void),
                r.ret.as_ref().unwrap_or(&void),
            );
            if let Some((sev, why)) = type_diff(cr, rr, r.definition) {
                let show = |t: &CType| {
                    if t.ptrs > 0 && !t.text.contains('*') {
                        format!("{}{}", t.text.trim(), "*".repeat(t.ptrs))
                    } else {
                        t.text.trim().to_string()
                    }
                };
                problems.push((
                    sev,
                    format!(
                        "returns C++ `{}`, Rust `{}` ({why})",
                        show(cr),
                        rr.text.trim()
                    ),
                ));
            }
            if !problems.is_empty() {
                had_problem.insert(r.name.clone());
            }
            for (sev, msg) in problems {
                out.push(Lint {
                    kind: LintKind::ExternSignature,
                    severity: sev,
                    path: r.path.clone(),
                    line: r.line,
                    message: format!("{}: {msg}", r.name),
                    related: Some((c.path.clone(), c.line)),
                });
            }
        }
    }

    // Check that touched `cpp_*` definitions in `.cc` files have a matching
    // `extern "C"` prototype declaration (Rubric Section 3.13 / 4.18).
    static INCLUDE_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"(?m)^\s*#\s*include\s*[<"]([^>"]+)[>"]"#).unwrap());
    let stem = |path: &str| -> String {
        let base = path.rsplit('/').next().unwrap_or(path);
        let s = base.split('.').next().unwrap_or(base);
        s.strip_suffix("_ffi").unwrap_or(s).to_string()
    };
    let has_header_context = cs.cpp_new.iter().any(|v| {
        v.path.ends_with(".h") || v.path.ends_with(".hpp") || INCLUDE_RE.is_match(&v.text)
    });
    if has_header_context {
        for cands in cpp.values() {
            let Some(c) = cands
                .iter()
                .find(|s| s.definition && s.touched && s.name.starts_with("cpp_"))
            else {
                continue;
            };
            if had_problem.contains(&c.name) {
                continue;
            }
            let is_ffi_cc = c.path.ends_with("_ffi.cc") || c.path.ends_with("_ffi.cpp");
            let has_decl = cands.iter().any(|s| {
                !s.definition && (!is_ffi_cc || s.path.ends_with(".h") || s.path.ends_with(".hpp"))
            });
            if has_decl {
                continue;
            }
            let want = stem(&c.path);
            let hdr = cs
                .cpp_new
                .iter()
                .find(|v| v.path == c.path)
                .and_then(|v| {
                    INCLUDE_RE
                        .captures_iter(&v.text)
                        .map(|cap| cap[1].to_string())
                        .find(|inc| stem(inc) == want)
                })
                .or_else(|| {
                    cs.cpp_new
                        .iter()
                        .find(|v| {
                            (v.path.ends_with(".h") || v.path.ends_with(".hpp"))
                                && stem(&v.path) == want
                        })
                        .map(|v| v.path.clone())
                })
                .map(|p| p.rsplit('/').next().unwrap_or(&p).to_string());
            let where_decl = match hdr {
                Some(h) => format!("in {h}"),
                None => "in a C++ header".to_string(),
            };
            out.push(Lint {
                kind: LintKind::ExternSignature,
                severity: Severity::Issue,
                path: c.path.clone(),
                line: c.line,
                message: format!(
                    "{}: defined without an `extern \"C\"` prototype declaration {where_decl}",
                    c.name
                ),
                related: None,
            });
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Invented lifetimes and unsafe density

/// Code of a line without its comment.
fn code_part(line: &str) -> &str {
    line.split("//").next().unwrap_or("")
}

/// Lints over each changed Rust function.
fn rust_function_lints(cs: &ChangeSet) -> Vec<Lint> {
    let mut out = Vec::new();
    for v in &cs.rust_new {
        let functions = crate::extract::extract(Lang::Rust, &v.path, &v.text).functions;
        let mut per_file: Vec<(usize, String)> = Vec::new();
        for f in &functions {
            if !touched(v, f.start_line..=f.end_line) {
                continue;
            }
            out.extend(invented_lifetime(v, f));
            // Thin FFI shims and tests need their unsafe; a function that
            // needs a lot of it points at a missing safe facade.
            let test = f
                .lines
                .iter()
                .take_while(|l| !l.contains("fn "))
                .any(|l| l.trim_start().starts_with("#[") && l.contains("test"));
            let n = unsafe_blocks(f);
            let thin = f
                .units
                .iter()
                .filter(|u| !matches!(u.kind, UnitKind::Signature | UnitKind::Comment))
                .count()
                <= 3;
            if n >= UNSAFE_FN_MIN && !(f.is_ffi && thin) && !test {
                per_file.push((n, f.name.clone()));
            }
        }
        let total: usize = per_file.iter().map(|(n, _)| n).sum();
        if total >= UNSAFE_FILE_MIN {
            per_file.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
            let top: Vec<String> = per_file
                .iter()
                .take(3)
                .map(|(n, name)| format!("{name} {n}"))
                .collect();
            out.push(Lint {
                kind: LintKind::UnsafeDensity,
                severity: Severity::Note,
                path: v.path.clone(),
                line: 1,
                message: format!(
                    "{total} unsafe blocks in {} changed function{} (most in {}); a safe facade for what they reach into would remove most of them",
                    per_file.len(),
                    if per_file.len() == 1 { "" } else { "s" },
                    top.join(", ")
                ),
                related: None,
            });
        }
    }
    out
}

/// Files with at least this many unsafe blocks in changed functions that
/// each have at least [`UNSAFE_FN_MIN`] get a note.
const UNSAFE_FILE_MIN: usize = 10;
const UNSAFE_FN_MIN: usize = 3;

/// Number of `unsafe { ... }` blocks in a function's body.
pub fn unsafe_blocks(f: &Function) -> usize {
    static UNSAFE_BLOCK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\bunsafe\s*\{").unwrap());
    f.lines
        .iter()
        .map(|l| UNSAFE_BLOCK.find_iter(code_part(l)).count())
        .sum()
}

/// A reference made from a raw pointer that was taken from a place still
/// in scope (`let p = buf.as_mut_ptr(); ... &mut *p`): the reference's
/// lifetime is invented rather than borrowed from `buf`, so the compiler
/// can't check it. Borrowing `buf` (or a part of it) says the same thing
/// safely.
fn invented_lifetime(v: &Version, f: &Function) -> Vec<Lint> {
    static FROM_PLACE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\blet\s+(?:mut\s+)?([a-z_]\w*)\s*(?::[^=]*)?=\s*&?(?:mut\s+)?([a-z_][\w.]*(?:\[[^\]]*\])?)\s*\.\s*as_(?:mut_)?ptr\s*\(\s*\)").unwrap()
    });
    static MUT_GET: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\blet\s+(?:mut\s+)?([a-z_]\w*)\s*(?::[^=]*)?=\s*(?:unsafe\s*\{\s*)?&\s*mut\s*\*\s*([a-z_][\w.]*)\s*\.\s*get\s*\(\s*\)").unwrap()
    });
    static SELF_CALL: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\bself\s*\.\s*([a-z_]\w*)\s*\(").unwrap());
    static LET: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\blet\s+(?:mut\s+)?([a-z_]\w*)\s*(?::[^=]*)?=(.*)").unwrap());
    static DEREF: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"&\s*(?:mut\s+)?\*\s*\(?\s*([a-z_]\w*)\b").unwrap());
    let mut out = Vec::new();
    let code: Vec<&str> = f.lines.iter().map(|l| code_part(l).trim()).collect();

    // Check for `&mut *place.get()` held across a `self.method()` call or
    // another `place.get()` access and used again afterwards.
    for (k, text) in code.iter().enumerate() {
        if !LET.is_match(text) {
            continue;
        }
        let end = (k..code.len().min(k + 8))
            .find(|&m| code[m].ends_with(';'))
            .unwrap_or(k);
        let whole = code[k..=end].join(" ");
        let Some(c) = MUT_GET.captures(&whole) else {
            continue;
        };
        let (var, place) = (&c[1], &c[2]);
        let bind_line = f.start_line + k;
        let var_re = Regex::new(&format!(r"\b{}\b", regex::escape(var))).unwrap();
        let rebind_re =
            Regex::new(&format!(r"\blet\s+(?:mut\s+)?{}\b", regex::escape(var))).unwrap();
        let place_get_re =
            Regex::new(&format!(r"\b{}\s*\.\s*get\s*\(", regex::escape(place))).unwrap();
        let mut last_use = None;
        let mut scan_end = code.len();
        for (m, line_code) in code.iter().enumerate().skip(end + 1) {
            if rebind_re.is_match(line_code) {
                scan_end = m;
                break;
            }
            if var_re.is_match(line_code) {
                last_use = Some(m);
            }
        }
        let Some(use_idx) = last_use else {
            continue;
        };
        let use_line = f.start_line + use_idx;
        if !touched(v, bind_line..=use_line) {
            continue;
        }
        let is_self_place = place.starts_with("self.");
        for m in (end + 1)..=use_idx.min(scan_end.saturating_sub(1)) {
            let line_code = code[m];
            let call_line = f.start_line + m;
            if place_get_re.is_match(line_code) && m < use_idx {
                out.push(Lint {
                    kind: LintKind::InventedLifetime,
                    severity: Severity::Issue,
                    path: v.path.clone(),
                    line: bind_line,
                    message: format!(
                        "`{var}` borrows `&mut *{place}.get()` (line {bind_line}) across `{place}.get()` at line {call_line} and is used again at line {use_line}, risking aliasing UB"
                    ),
                    related: None,
                });
                break;
            }
            if is_self_place {
                if let Some(sc) = SELF_CALL.captures(line_code) {
                    let method = &sc[1];
                    if method != "get" && m < use_idx {
                        out.push(Lint {
                            kind: LintKind::InventedLifetime,
                            severity: Severity::Issue,
                            path: v.path.clone(),
                            line: bind_line,
                            message: format!(
                                "`{var}` borrows `&mut *{place}.get()` (line {bind_line}) across `self.{method}()` at line {call_line} and is used again at line {use_line}, risking aliasing UB"
                            ),
                            related: None,
                        });
                        break;
                    }
                }
            }
        }
    }

    // Code that reaches everything through one base pointer on purpose,
    // because other raw aliases (`NonNull`s stored elsewhere) must stay
    // valid, says so; a borrow there would invalidate those aliases.
    let all = f.lines.join("\n").to_ascii_lowercase();
    if ["stacked borrows", "provenance", "nonnull"]
        .iter()
        .any(|k| all.contains(k))
    {
        return out;
    }
    // Pointer name -> the place it came from, and the line.
    let mut derived: Vec<(String, String, usize)> = Vec::new();
    for (k, text) in code.iter().enumerate() {
        let line = f.start_line + k;
        // A `let` can span lines (a call that returns a derived pointer):
        // its value runs to the `;`, within a few lines.
        let stmt = || -> String {
            let end = (k..code.len().min(k + 8))
                .find(|&m| code[m].ends_with(';'))
                .unwrap_or(k);
            code[k..=end].join(" ")
        };
        if let Some(c) = FROM_PLACE.captures(text) {
            derived.push((c[1].to_string(), c[2].to_string(), line));
            continue;
        }
        for c in DEREF.captures_iter(text) {
            let Some((name, place, at)) = derived.iter().find(|(n, _, _)| *n == c[1]).cloned()
            else {
                continue;
            };
            if !touched(v, line..=line) {
                continue;
            }
            out.push(Lint {
                kind: LintKind::InventedLifetime,
                severity: Severity::Issue,
                path: v.path.clone(),
                line,
                message: format!(
                    "a reference is made from `{name}`, a raw pointer into `{place}` (line {at}), so its lifetime is invented rather than borrowed from `{place}`; borrow `{place}` instead"
                ),
                related: None,
            });
        }
        // A pointer computed from a derived pointer is derived too.
        if LET.is_match(text) {
            let whole = stmt();
            let Some(c) = LET.captures(&whole) else {
                continue;
            };
            let rhs = c[2].to_string();
            let from = derived.iter().find(|(n, _, _)| {
                Regex::new(&format!(r"\b{}\b", regex::escape(n))).is_ok_and(|r| r.is_match(&rhs))
            });
            if let Some((_, place, at)) = from.cloned() {
                let pointerish = ["*mut", "*const", "ptr", ".add(", ".cast", ".offset("];
                if pointerish.iter().any(|p| rhs.contains(p)) && !rhs.trim_start().starts_with('&')
                {
                    derived.push((c[1].to_string(), place, at));
                }
            }
        }
    }
    out.dedup_by(|a, b| a.line == b.line);
    out
}

// ---------------------------------------------------------------------------
// Provenance comments

static PROVENANCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?ix)
        \b(?:ported|translated|converted|transliterated|carried\s+over|adapted)\s+(?:directly\s+|verbatim\s+)?from\b
        | \bport\s+of\s+(?:the\s+)?(?:C\+\+|`)
        | \b[\w./-]+\.(?:cc|cpp|h|hh|S)\s*:\s*\d+
        | \b(?:mirrors|matches|follows)\s+(?:the\s+)?(?:C\+\+\s+)?`?[\w./-]+\.(?:cc|cpp|h)\b
        | \b(?:the|its)\s+C\+\+\s+(?:version|implementation|original|counterpart)\b
        | \bC\+\+\s+original\b",
    )
    .unwrap()
});

/// The most line numbers a lint lists in its message.
const MAX_LISTED_LINES: usize = 8;

/// The most characters of a comment a lint quotes as an example.
const EXAMPLE_WIDTH: usize = 80;

/// What a blank line between paragraphs becomes in a `Comment`'s text.
const PARAGRAPH_BREAK: &str = "\n\n";

/// A comment: a run of consecutive `//`, `///` or `//!` lines of the same
/// kind, or the comment after code on a line. A `SAFETY:` line starts a new
/// comment, so a safety comment is never joined to the comment above it.
struct Comment {
    /// The line the comment starts on. Its lines are consecutive.
    first_line: usize,
    /// The byte offset in `text` where each of its lines starts; the first is
    /// always 0.
    line_starts: Vec<usize>,
    /// The comment's text without comment markers, lines joined with spaces.
    /// A blank comment line between paragraphs becomes `PARAGRAPH_BREAK`.
    text: String,
    /// The comment marker (`//`, `///` or `//!`), or `None` after code.
    marker: Option<&'static str>,
}

impl Comment {
    fn new(first_line: usize, text: &str, marker: Option<&'static str>) -> Self {
        Comment {
            first_line,
            line_starts: vec![0],
            text: text.to_string(),
            marker,
        }
    }

    /// Appends the text of the comment's next line.
    fn push_line(&mut self, text: &str) {
        if !self.text.is_empty() && !self.text.ends_with(PARAGRAPH_BREAK) {
            self.text.push_str(if text.is_empty() {
                PARAGRAPH_BREAK
            } else {
                " "
            });
        }
        self.line_starts.push(self.text.len());
        self.text.push_str(text);
    }

    fn last_line(&self) -> usize {
        self.first_line + self.line_starts.len() - 1
    }

    /// The line that byte `offset` of `text` falls on.
    fn line_at(&self, offset: usize) -> usize {
        self.first_line + self.line_starts.partition_point(|&start| start <= offset) - 1
    }

    fn is_safety(&self) -> bool {
        self.text.starts_with("SAFETY")
    }
}

/// The comments in `v` that include a changed line. Matching a whole comment
/// rather than each line finds wording that a comment wraps across lines.
fn comments(v: &Version) -> Vec<Comment> {
    let mut comments: Vec<Comment> = Vec::new();
    // Whether the previous line was a whole-line comment the next can join.
    let mut joinable = false;
    for (i, line) in v.text.lines().enumerate() {
        let trimmed = line.trim_start();
        let (marker, rest) = if trimmed.starts_with("//") {
            // `////` and longer are plain comments (often dividers), not docs.
            let marker = if trimmed.starts_with("///") && !trimmed.starts_with("////") {
                "///"
            } else if trimmed.starts_with("//!") {
                "//!"
            } else {
                "//"
            };
            (Some(marker), &trimmed[marker.len()..])
        } else if let Some(pos) = line.find("//") {
            (None, &line[pos + 2..])
        } else {
            joinable = false;
            continue;
        };
        let text = rest.trim_start_matches(['/', '!']).trim();
        let joins = joinable
            && !text.starts_with("SAFETY")
            && comments
                .last()
                .is_some_and(|comment| comment.marker == marker);
        match comments.last_mut() {
            Some(comment) if joins => comment.push_line(text),
            _ => comments.push(Comment::new(i + 1, text, marker)),
        }
        joinable = marker.is_some();
    }
    comments.retain(|comment| touched(v, comment.first_line..=comment.last_line()));
    comments
}

/// The paragraphs of a comment's text, with the byte offset each starts at.
fn paragraphs(text: &str) -> impl Iterator<Item = (usize, &str)> {
    let mut start = 0;
    text.split(PARAGRAPH_BREAK).map(move |paragraph| {
        let at = start;
        start += paragraph.len() + PARAGRAPH_BREAK.len();
        (at, paragraph)
    })
}

/// Whether the part of `comment` at byte `offset` stays relevant after the change
/// even though it talks about C++: the comment is a keep-in-sync note about
/// C++ that still exists, or the paragraph is a TODO or says something must
/// match or agree.
fn stays_relevant(comment: &Comment, offset: usize) -> bool {
    let paragraph = paragraphs(&comment.text)
        .take_while(|&(start, _)| start <= offset)
        .last()
        .map_or("", |(_, paragraph)| paragraph);
    let comment_lower = comment.text.to_ascii_lowercase();
    let paragraph_lower = paragraph.to_ascii_lowercase();
    ["keep in sync", "kept in sync"]
        .iter()
        .any(|k| comment_lower.contains(k))
        || paragraph.contains("TODO")
        || ["must match", "must agree"]
            .iter()
            .any(|k| paragraph_lower.contains(k))
}

/// Where `provenance-comment` matches in `comment`, if it does. Safety comments are
/// exempt, as are keep-in-sync comments and TODO paragraphs.
fn provenance_match(comment: &Comment) -> Option<usize> {
    if comment.is_safety() {
        return None;
    }
    PROVENANCE
        .find_iter(&comment.text)
        .map(|m| m.start())
        .find(|&at| !stays_relevant(comment, at))
}

/// Up to `EXAMPLE_WIDTH` characters of `text` around byte offset `at`,
/// starting on a word boundary.
fn excerpt(text: &str, at: usize) -> String {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let centre = chars.iter().position(|&(i, _)| i >= at).unwrap_or(0);
    let mut start = centre.saturating_sub(EXAMPLE_WIDTH / 4);
    while start > 0 && !chars[start - 1].1.is_whitespace() {
        start -= 1;
    }
    let end = (start + EXAMPLE_WIDTH).min(chars.len());
    let quoted: String = chars[start..end]
        .iter()
        .map(|&(_, ch)| if ch == '\n' { ' ' } else { ch })
        .collect();
    format!(
        "{}{quoted}{}",
        if start > 0 { "..." } else { "" },
        if end < chars.len() { "..." } else { "" }
    )
}

/// `1 comment` or `3 comments`.
fn count_noun(n: usize, noun: &str) -> String {
    format!("{n} {noun}{}", if n == 1 { "" } else { "s" })
}

/// `line 4` or `lines 4, 44`, listing at most `MAX_LISTED_LINES`.
fn line_list(lines: &[usize]) -> String {
    let shown: Vec<String> = lines
        .iter()
        .take(MAX_LISTED_LINES)
        .map(|n| n.to_string())
        .collect();
    format!(
        "line{} {}{}",
        if lines.len() == 1 { "" } else { "s" },
        shown.join(", "),
        if lines.len() > MAX_LISTED_LINES {
            ", ..."
        } else {
            ""
        }
    )
}

/// Comments that point back at the C++ a function was ported from
/// ("Ported from foo.cc", "(`foo.cc:12-34`)"). They stop being true, or
/// useful, once the change lands. Reported once per file.
fn provenance(v: &Version) -> Vec<Lint> {
    let hits: Vec<(usize, String)> = comments(v)
        .into_iter()
        .filter_map(|comment| {
            provenance_match(&comment).map(|at| (comment.line_at(at), excerpt(&comment.text, at)))
        })
        .collect();
    let Some((first, example)) = hits.first() else {
        return Vec::new();
    };
    let n = hits.len();
    let lines: Vec<usize> = hits.iter().map(|(line, _)| *line).collect();
    vec![Lint {
        kind: LintKind::Provenance,
        severity: Severity::Issue,
        path: v.path.clone(),
        line: *first,
        message: format!(
            "{} say{} where the code was ported from ({}), such as \"{example}\"; drop {}, \
             since {} stop{} being relevant once the change lands",
            count_noun(n, "comment"),
            if n == 1 { "s" } else { "" },
            line_list(&lines),
            if n == 1 { "it" } else { "them" },
            if n == 1 { "it" } else { "they" },
            if n == 1 { "s" } else { "" },
        ),
        related: None,
    }]
}

// ---------------------------------------------------------------------------
// References to deleted C++

/// Wording that compares Rust with the C++ it replaces, or describes that C++
/// as it was: "like the C++ global", "unlike the C++", "the C++ enum's",
/// "was defined in C++".
static CPP_COMPARISON: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?ix)
        \b(?:like|unlike|as\s+in|as\s+with)\s+(?:the\s+)?C\+\+
        | \bthe\s+C\+\+\s+(?:\w+\s+)?
          (?:enum|class|struct|global|version|implementation|original)(?:'s)?\b
        | \b(?:was|were)\s+(?:\w+\s+){0,3}in\s+C\+\+",
    )
    .unwrap()
});

/// A backticked span in a comment.
static BACKTICKED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"`([^`]+)`").unwrap());

/// An identifier.
static IDENT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[A-Za-z_][A-Za-z0-9_]*").unwrap());

/// A C++ class, struct or union declaration's name, up to its opening brace,
/// allowing annotations before the name (`class TA_CAP("mutex") Mutex {`,
/// `alignas(64)`, `[[nodiscard]]`). Only the head is matched, so declarations
/// nested in a class body are found too, and a template parameter
/// (`template <class T>`) is not mistaken for one.
static CPP_TYPE_HEAD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?x)
        \b(?:class|struct|union)\s+
        (?:(?:[A-Z_][A-Z0-9_]*(?:\([^)]*\))?|alignas\([^)]*\)|\[\[[^\]]*\]\])\s+)*
        ([A-Za-z_]\w*)\s*(?:final\s*)?(?::[^{;]*)?\{",
    )
    .unwrap()
});

/// A C++ enum declaration up to its opening brace, with its name if it has
/// one (`enum : uint32_t {` doesn't).
static CPP_ENUM_HEAD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\benum(?:\s+(?:class|struct))?(?:\s+([A-Za-z_]\w*))?\s*(?::[^{;()]*)?\{").unwrap()
});

/// A C++ `/* ... */` comment.
static CPP_BLOCK_COMMENT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)/\*.*?\*/").unwrap());

/// The name a Rust item declares.
static RUST_ITEM: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\b(?:fn|struct|enum|union|trait|type|const|static|mod)\s+(?:mut\s+)?([A-Za-z_]\w*)",
    )
    .unwrap()
});

/// A Rust enum declaration up to its opening brace.
static RUST_ENUM_HEAD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\benum\s+[A-Za-z_]\w*[^{;]*\{").unwrap());

/// The shortest deleted C++ name worth matching in a comment.
const MIN_DELETED_NAME_LEN: usize = 3;

/// C++ words too generic to tie a comment to a deleted declaration.
const GENERIC_CPP_WORDS: &[&str] = &[
    "bool", "char", "const", "int", "size_t", "uint32_t", "uint64_t", "int64_t", "void", "this",
    "true", "false", "nullptr", "std", "ktl", "fbl", "final",
];

/// Rust prelude names that a deleted C++ name may share; a comment naming
/// one is about the Rust.
const RUST_PRELUDE_NAMES: &[&str] = &[
    "None", "Some", "Ok", "Err", "Option", "Result", "Self", "Box", "Vec", "String", "Default",
];

/// The paths of the C++ files the change deletes.
fn deleted_cpp_paths(cs: &ChangeSet) -> BTreeSet<&str> {
    let new_paths: BTreeSet<&str> = cs.cpp_new.iter().map(|v| v.path.as_str()).collect();
    cs.cpp_old
        .iter()
        .map(|v| v.path.as_str())
        .filter(|path| !new_paths.contains(path))
        .collect()
}

/// The enumerator names in the body of a C++ enum, which starts at `body`.
fn enumerators(body: &str) -> Vec<String> {
    let body = &body[..body.find('}').unwrap_or(body.len())];
    let body = CPP_BLOCK_COMMENT.replace_all(body, " ");
    let code: Vec<&str> = body
        .lines()
        .map(|line| line.split("//").next().unwrap_or(""))
        .collect();
    code.join("\n")
        .split(',')
        .filter_map(|item| IDENT.find(item.trim_start()).filter(|m| m.start() == 0))
        .map(|m| m.as_str().to_string())
        .collect()
}

/// The variant names of a Rust enum whose body starts at `body`: the
/// identifiers that open its top-level items, skipping attributes and the
/// fields of struct and tuple variants.
fn rust_enum_variants(body: &str) -> Vec<String> {
    let mut depth = 1;
    let mut top_level = String::new();
    for ch in body.chars() {
        match ch {
            '{' | '(' | '[' => depth += 1,
            '}' | ')' | ']' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            '#' => {}
            _ if depth == 1 => top_level.push(ch),
            _ => {}
        }
    }
    top_level
        .split(',')
        .filter_map(|item| IDENT.find(item.trim_start()).filter(|m| m.start() == 0))
        .map(|m| m.as_str().to_string())
        .collect()
}

/// Names the new Rust declares, outside its comments: items, enum variants,
/// and the prelude.
fn rust_declared_names(cs: &ChangeSet) -> HashSet<String> {
    let mut names: HashSet<String> = RUST_PRELUDE_NAMES.iter().map(|n| n.to_string()).collect();
    for v in &cs.rust_new {
        let code: Vec<&str> = v
            .text
            .lines()
            .map(|line| line.split("//").next().unwrap_or(""))
            .collect();
        let code = code.join("\n");
        names.extend(
            RUST_ITEM
                .captures_iter(&code)
                .map(|captures| captures[1].to_string()),
        );
        for head in RUST_ENUM_HEAD.find_iter(&code) {
            names.extend(rust_enum_variants(&code[head.end()..]));
        }
    }
    names
}

/// Names that C++ declared before the change and that nothing after it uses:
/// functions, methods and types (with enumerators) from deleted files, and
/// functions removed from files that stay. Names still mentioned in C++ the
/// change leaves behind, or declared by the new Rust, are left out, since a
/// comment naming them may be about live code.
fn deleted_cpp_names(cs: &ChangeSet, deleted_paths: &BTreeSet<&str>) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for v in &cs.cpp_old {
        let deleted_file = deleted_paths.contains(v.path.as_str());
        for f in crate::extract::extract(Lang::Cpp, &v.path, &v.text).functions {
            if deleted_file || touched(v, f.start_line..=f.end_line) {
                names.insert(f.base.clone());
            }
        }
        if deleted_file {
            names.extend(
                CPP_TYPE_HEAD
                    .captures_iter(&v.text)
                    .map(|captures| captures[1].to_string()),
            );
            for captures in CPP_ENUM_HEAD.captures_iter(&v.text) {
                names.extend(captures.get(1).map(|name| name.as_str().to_string()));
                let body_start = captures.get(0).expect("group 0 always matches").end();
                names.extend(enumerators(&v.text[body_start..]));
            }
        }
    }
    let cpp_still_mentions: HashSet<&str> = cs
        .cpp_new
        .iter()
        .flat_map(|v| IDENT.find_iter(&v.text).map(|m| m.as_str()))
        .collect();
    let rust_declares = rust_declared_names(cs);
    names.retain(|name| {
        name.len() >= MIN_DELETED_NAME_LEN
            && !GENERIC_CPP_WORDS.contains(&name.as_str())
            && !cpp_still_mentions.contains(name.as_str())
            && !rust_declares.contains(name)
    });
    names
}

/// A comment `stale_cpp_references` found.
struct StaleComment {
    line: usize,
    /// The deleted C++ name the comment gives, if it names one.
    deleted_name: Option<String>,
    excerpt: String,
}

/// Comments on new Rust that describe C++ the change deletes. Such comments
/// are accurate during review but stop being true once the change lands, so
/// the comparison belongs in the commit message instead. C++ that stays in
/// the tree (an FFI counterpart, a type the Rust binds to) is fine to name,
/// and so is a C++ name the new Rust declares too (`struct Lamp` for a deleted
/// `class Lamp`), since such a comment may be about the Rust.
///
/// - Naming a deleted C++ symbol in a paragraph that mentions C++ is an issue.
/// - In a Rust file named after a deleted C++ file (as `file-placement`
///   expects), comparison wording ("like the C++ global", "the C++ enum's")
///   is a note, unless the comment is a keep-in-sync note or the paragraph a
///   TODO.
///
/// Unlike `provenance-comment`, this checks `// SAFETY:` comments too, since
/// a safety argument that leans on the deleted C++ goes stale with it. A
/// comment `provenance-comment` already reports gets no note here. Each is
/// reported once per file.
fn stale_cpp_references(cs: &ChangeSet) -> Vec<Lint> {
    // Comparing files (`babeldiff files`) says nothing about what was deleted.
    if cs.is_file_comparison() {
        return Vec::new();
    }
    let deleted_paths = deleted_cpp_paths(cs);
    let deleted_names = deleted_cpp_names(cs, &deleted_paths);
    if deleted_names.is_empty() && deleted_paths.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for v in &cs.rust_new {
        let port_of_deleted = deleted_paths
            .iter()
            .any(|cpp| crate::placement::expected(cpp, &v.path));
        let mut named = Vec::new();
        let mut compared = Vec::new();
        for comment in comments(v) {
            // Leave what `provenance-comment` already reports to it.
            let reported_by_provenance = provenance_match(&comment).is_some();
            let mut comparison = None;
            let mut names_deleted = false;
            for (at, paragraph) in paragraphs(&comment.text) {
                if !paragraph.contains("C++") {
                    continue;
                }
                let deleted_name = BACKTICKED
                    .captures_iter(paragraph)
                    .filter_map(|code| code.get(1))
                    .flat_map(|span| {
                        IDENT
                            .find_iter(span.as_str())
                            .map(move |m| (span.start() + m.start(), m.as_str()))
                    })
                    .find(|(_, name)| deleted_names.contains(*name));
                if let Some((offset, name)) = deleted_name {
                    let at = at + offset;
                    named.push(StaleComment {
                        line: comment.line_at(at),
                        deleted_name: Some(name.to_string()),
                        excerpt: excerpt(&comment.text, at),
                    });
                    names_deleted = true;
                    break;
                }
                if comparison.is_some() || !port_of_deleted || reported_by_provenance {
                    continue;
                }
                if let Some(m) = CPP_COMPARISON.find(paragraph) {
                    let at = at + m.start();
                    if stays_relevant(&comment, at) {
                        continue;
                    }
                    comparison = Some(StaleComment {
                        line: comment.line_at(at),
                        deleted_name: None,
                        excerpt: excerpt(&comment.text, at),
                    });
                }
            }
            // A comment that names deleted C++ is reported as that issue only.
            if !names_deleted {
                compared.extend(comparison);
            }
        }
        out.extend(stale_cpp_lint(v, Severity::Issue, &named));
        out.extend(stale_cpp_lint(v, Severity::Note, &compared));
    }
    out
}

/// The one `stale-cpp-reference` lint for the comments `found` in `v`.
fn stale_cpp_lint(v: &Version, severity: Severity, found: &[StaleComment]) -> Option<Lint> {
    let first = found.first()?;
    let n = found.len();
    let lines: Vec<usize> = found.iter().map(|stale| stale.line).collect();
    let comments = count_noun(n, "comment");
    let excerpt = &first.excerpt;
    let message = match &first.deleted_name {
        Some(name) => format!(
            "{comments} name{} C++ that this change deletes ({}), such as `{name}` in \
             \"{excerpt}\"; describe the Rust as it is and move the comparison to the commit \
             message",
            if n == 1 { "s" } else { "" },
            line_list(&lines),
        ),
        None => format!(
            "{comments} compare{} the Rust with C++ this change deletes ({}), such as \
             \"{excerpt}\"; {} stop{} being true once the change lands",
            if n == 1 { "s" } else { "" },
            line_list(&lines),
            if n == 1 { "it" } else { "they" },
            if n == 1 { "s" } else { "" },
        ),
    };
    Some(Lint {
        kind: LintKind::StaleCppReference,
        severity,
        path: v.path.clone(),
        line: first.line,
        message,
        related: None,
    })
}

// ---------------------------------------------------------------------------
// mangled C++ symbols

/// An Itanium C++ mangled name: `_Z` and then a nested name, a length or a
/// special name.
static MANGLED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b_Z[NLSTZGD0-9][A-Za-z0-9_$.]*").unwrap());

/// Readable form of a mangled name, or the name itself.
fn demangle(sym: &str) -> String {
    cpp_demangle::Symbol::new(sym)
        .ok()
        .and_then(|s| s.demangle().ok())
        .unwrap_or_else(|| sym.to_string())
}

/// Rust that binds to a C++ function by its mangled name: an `extern`
/// declaration named `_Z...` or given `#[link_name = "_Z..."]` (Rust
/// calling C++), or a definition named `_Z...` or given
/// `#[export_name = "_Z..."]` (Rust standing in for a C++ function). Both
/// skip the `cpp_`/`rust_` FFI layer, so nothing checks the two sides
/// agree, and they break as soon as the C++ signature changes.
fn mangled_symbols(v: &Version) -> Vec<Lint> {
    let lines: Vec<&str> = v.text.lines().collect();
    let code = |l: &str| code_part(l).to_string();
    let mut seen: BTreeSet<(bool, String)> = BTreeSet::new();
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let n = i + 1;
        let text = code(line);
        let t = text.trim();
        if t.is_empty() || !touched(v, n..=n) {
            continue;
        }
        // (symbol, exported, name the Rust code uses for it)
        let found: Option<(String, bool, String)> = if t.starts_with("#[") {
            let attr = |key: &str| {
                let at = t.find(key)?;
                let m = MANGLED.find(&t[at..])?;
                Some(m.as_str().to_string())
            };
            let item = || -> String {
                lines[i + 1..]
                    .iter()
                    .map(|l| code(l))
                    .find(|l| !l.trim().is_empty() && !l.trim_start().starts_with("#["))
                    .and_then(|l| {
                        let after = l.split_once("fn ")?.1;
                        Some(
                            after
                                .split(|c: char| !c.is_alphanumeric() && c != '_')
                                .next()?
                                .to_string(),
                        )
                    })
                    .unwrap_or_default()
            };
            if let Some(sym) = attr("link_name") {
                Some((sym, false, item()))
            } else {
                attr("export_name").map(|sym| (sym, true, item()))
            }
        } else if let Some(after) = t.split_once("fn ").map(|(_, a)| a) {
            MANGLED.find(after).filter(|m| m.start() == 0).map(|m| {
                // A body makes it a definition; a `;` a declaration.
                let rest: String = std::iter::once(t.to_string())
                    .chain(lines[i + 1..].iter().take(12).map(|l| code(l)))
                    .collect::<Vec<_>>()
                    .join(" ");
                let body = rest.find('{');
                let semi = rest.find(';');
                let exported = match (body, semi) {
                    (Some(b), Some(s)) => b < s,
                    (Some(_), None) => true,
                    _ => false,
                };
                (m.as_str().to_string(), exported, m.as_str().to_string())
            })
        } else {
            None
        };
        let Some((sym, exported, local)) = found else {
            continue;
        };
        if !seen.insert((exported, sym.clone())) {
            continue;
        }
        let readable = demangle(&sym);
        let message = if exported {
            format!(
                "Rust defines C++ `{readable}` under its mangled symbol `{sym}`; export a `rust_*` function instead and call it from C++"
            )
        } else {
            let calls: Vec<String> = if local.is_empty() {
                Vec::new()
            } else {
                let word = Regex::new(&format!(r"\b{}\s*\(", regex::escape(&local))).unwrap();
                lines
                    .iter()
                    .enumerate()
                    .filter(|(j, l)| {
                        let c = code(l);
                        word.is_match(&c) && !c.contains("fn ") && *j != i
                    })
                    .map(|(j, _)| (j + 1).to_string())
                    .collect()
            };
            let used = match calls.len() {
                0 => String::new(),
                1 => format!(" (called at line {})", calls[0]),
                _ => format!(
                    " (called at lines {}{})",
                    calls.iter().take(6).cloned().collect::<Vec<_>>().join(", "),
                    if calls.len() > 6 { ", ..." } else { "" }
                ),
            };
            format!(
                "Rust calls C++ `{readable}` through its mangled symbol `{sym}`{used}; call a `cpp_*` FFI function declared in a C++ header instead"
            )
        };
        out.push(Lint {
            kind: LintKind::MangledSymbol,
            severity: Severity::Issue,
            path: v.path.clone(),
            line: n,
            message,
            related: None,
        });
    }
    out
}

// ---------------------------------------------------------------------------
// unsafe without SAFETY

fn unsafe_safety(v: &Version) -> Vec<Lint> {
    let tree = ts::parse(Lang::Rust, &v.text);
    let src = v.text.as_bytes();
    let lines: Vec<&str> = v.text.lines().collect();
    let mut out = Vec::new();
    // Undocumented unsafe blocks by enclosing function, reported once per
    // function.
    let mut blocks: Vec<(String, Vec<usize>)> = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(n) = stack.pop() {
        match n.kind() {
            "unsafe_block" => {
                let stmt = statement_of(n);
                let from = comment_start(&lines, ts::line(stmt));
                let covered = has_safety(&lines, from, ts::line(n))
                    // `unsafe { // SAFETY: ... }`
                    || has_safety(&lines, ts::line(n), ts::line(n) + 1);
                if !covered && touched(v, ts::line(n)..=ts::line(n)) {
                    let within = enclosing_fn(n, src);
                    match blocks.iter_mut().find(|(f, _)| *f == within) {
                        Some((_, lines)) => lines.push(ts::line(n)),
                        None => blocks.push((within, vec![ts::line(n)])),
                    }
                }
            }
            "function_item" => {
                let is_unsafe = ts::children(n).iter().any(|c| {
                    c.kind() == "function_modifiers"
                        && ts::children(*c)
                            .iter()
                            .any(|m| ts::text(*m, src) == "unsafe")
                });
                if is_unsafe && touched(v, ts::line(n)..=ts::line(n)) {
                    let from = comment_start(&lines, ts::line(n));
                    let doc = lines[from.saturating_sub(1)..ts::line(n).saturating_sub(1)]
                        .iter()
                        .any(|l| l.contains("# Safety"));
                    if !doc {
                        let name = n
                            .child_by_field_name("name")
                            .map_or("", |x| ts::text(x, src));
                        out.push(Lint {
                            kind: LintKind::UnsafeSafety,
                            severity: Severity::Issue,
                            path: v.path.clone(),
                            line: ts::line(n),
                            message: format!("unsafe fn {name} without a # Safety doc section"),
                            related: None,
                        });
                    }
                }
            }
            "impl_item" => {
                let is_unsafe = ts::children(n)
                    .iter()
                    .any(|c| ts::text(*c, src) == "unsafe");
                if is_unsafe && touched(v, ts::line(n)..=ts::line(n)) {
                    let from = comment_start(&lines, ts::line(n));
                    if !has_safety(&lines, from, ts::line(n)) {
                        out.push(Lint {
                            kind: LintKind::UnsafeSafety,
                            severity: Severity::Issue,
                            path: v.path.clone(),
                            line: ts::line(n),
                            message: "unsafe impl without a // SAFETY: comment".into(),
                            related: None,
                        });
                    }
                }
            }
            _ => {}
        }
        stack.extend(ts::named_children(n));
    }
    for (within, mut lines) in blocks {
        lines.sort();
        let place = if within.is_empty() {
            String::new()
        } else {
            format!(" in {within}")
        };
        let message = match lines.len() {
            1 => format!("unsafe block{place} without a // SAFETY: comment"),
            n => format!(
                "{n} unsafe blocks{place} without a // SAFETY: comment (lines {})",
                lines
                    .iter()
                    .map(|l| l.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
        out.push(Lint {
            kind: LintKind::UnsafeSafety,
            severity: Severity::Issue,
            path: v.path.clone(),
            line: lines[0],
            message,
            related: None,
        });
    }
    out
}

/// The name of the function a node is in, or "" at the top level.
fn enclosing_fn(n: Node, src: &[u8]) -> String {
    let mut cur = n.parent();
    while let Some(p) = cur {
        if p.kind() == "function_item" {
            return p
                .child_by_field_name("name")
                .map_or(String::new(), |x| ts::text(x, src).to_string());
        }
        cur = p.parent();
    }
    String::new()
}

/// The statement or item a node is part of: its ancestor just inside a
/// block, a file or an `impl`.
fn statement_of(n: Node) -> Node {
    let mut cur = n;
    while let Some(p) = cur.parent() {
        if matches!(
            p.kind(),
            "block" | "source_file" | "declaration_list" | "match_block"
        ) {
            return cur;
        }
        // A closure's body is its own statement for this purpose.
        if p.kind() == "closure_expression" {
            return cur;
        }
        cur = p;
    }
    cur
}

/// The first line of the comments and attributes directly above `line`.
fn comment_start(lines: &[&str], line: usize) -> usize {
    let mut from = line;
    while from > 1 {
        let t = lines.get(from - 2).map_or("", |l| l.trim());
        if t.starts_with("//") || t.starts_with("#[") || t.starts_with("/*") || t.starts_with('*') {
            from -= 1;
        } else {
            break;
        }
    }
    from
}

/// Whether a comment in lines `from..=to` says `SAFETY:`.
fn has_safety(lines: &[&str], from: usize, to: usize) -> bool {
    (from..=to).any(|n| {
        lines.get(n - 1).is_some_and(|l| {
            l.find("//")
                .or_else(|| l.find("/*"))
                .is_some_and(|i| l[i..].to_ascii_uppercase().contains("SAFETY:"))
        })
    })
}

// ---------------------------------------------------------------------------
// logic in shims

fn shim_logic(cs: &ChangeSet) -> Vec<Lint> {
    let mut out = Vec::new();
    let versions = cs
        .rust_new
        .iter()
        .map(|v| (v, Lang::Rust))
        .chain(cs.cpp_new.iter().map(|v| (v, Lang::Cpp)));
    for (v, lang) in versions {
        let functions = crate::extract::extract(lang, &v.path, &v.text).functions;
        for f in &functions {
            let shim = match lang {
                Lang::Rust => f.is_ffi && f.base.starts_with("rust_"),
                // Everything in an `_ffi.cc` file is glue, including C++
                // methods that now call into Rust.
                Lang::Cpp => {
                    f.base.starts_with("cpp_")
                        || v.path.ends_with("_ffi.cc")
                        || v.path.ends_with("_ffi.cpp")
                }
            };
            if !shim || !touched(v, f.start_line..=f.end_line) {
                continue;
            }
            let logic = logic_units(f);
            if logic.is_empty() {
                continue;
            }
            let mut kinds: Vec<String> = Vec::new();
            for (k, many) in [
                ("branch", "branches"),
                ("loop", "loops"),
                ("match/switch arm", "match/switch arms"),
            ] {
                let n = logic.iter().filter(|(x, _)| *x == k).count();
                if n > 0 {
                    kinds.push(format!("{n} {}", if n == 1 { k } else { many }));
                }
            }
            let mut at: Vec<String> = logic.iter().take(6).map(|(_, l)| l.to_string()).collect();
            if logic.len() > 6 {
                at.push("…".into());
            }
            out.push(Lint {
                kind: LintKind::ShimLogic,
                severity: Severity::Issue,
                path: v.path.clone(),
                line: logic[0].1,
                message: format!(
                    "FFI shim {} does more than forward: {} (line{} {})",
                    f.name,
                    kinds.join(", "),
                    if logic.len() == 1 { "" } else { "s" },
                    at.join(", ")
                ),
                related: None,
            });
        }
    }
    out
}

/// Branches and loops in a shim that aren't result or null conversions.
fn logic_units(f: &Function) -> Vec<(&'static str, usize)> {
    static CONVERSION: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?i)\b(?:null(?:ptr)?|is_null|is_none|is_some|is_ok|is_err|has_value|some|none|ok|err|zx_ok|status|result)\b",
        )
        .unwrap()
    });
    static CONVERSION_ARM: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^\s*(?:Ok|Err|Some|None|_|Status::OK)\b").unwrap());
    let text = |u: &crate::model::Unit| -> String {
        (u.start_line..=u.end_line)
            .map(|l| f.line(l))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let mut out: Vec<(&'static str, usize)> = Vec::new();
    for u in &f.units {
        let kind = match u.kind {
            UnitKind::Loop => "loop",
            UnitKind::If | UnitKind::ElseIf => {
                if u.features.checks_error
                    || u.features.propagates
                    || CONVERSION.is_match(&text(u))
                    || truthiness(&u.features.conjuncts)
                {
                    continue;
                }
                "branch"
            }
            UnitKind::Case => {
                if f.lang == Lang::Rust && CONVERSION_ARM.is_match(&text(u)) {
                    continue;
                }
                "match/switch arm"
            }
            _ => continue,
        };
        out.push((kind, u.start_line));
    }
    out
}

/// A condition that only tests pointers or optionals for presence
/// (`p`, `!p`, `a && a->b`): converting a value, not deciding anything.
fn truthiness(conjuncts: &[crate::model::Conjunct]) -> bool {
    static PRESENCE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^!?\s*[A-Za-z_][\w.]*(?:->[\w.]+)*$").unwrap());
    !conjuncts.is_empty()
        && conjuncts.iter().all(|c| {
            let t = c
                .text
                .trim()
                .trim_start_matches('(')
                .trim_end_matches(')')
                .trim();
            PRESENCE.is_match(t)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(path: &str, text: &str) -> Version {
        Version {
            path: path.into(),
            text: text.into(),
            changed: None,
        }
    }

    fn cs(cpp: &str, rust: &str) -> ChangeSet {
        ChangeSet {
            cpp_old: Vec::new(),
            cpp_new: vec![version("foo_ffi.cc", cpp)],
            rust_new: vec![version("foo.rs", rust)],
        }
    }

    fn messages(l: &[Lint], kind: LintKind) -> Vec<String> {
        l.iter()
            .filter(|l| l.kind == kind)
            .map(|l| l.message.clone())
            .collect()
    }

    #[test]
    fn invented_lifetimes_are_reported() {
        let rust = "fn f(arch: &mut Arch) {\n    let buf_ptr = arch.buffer.as_mut_ptr();\n    let save_ptr = unsafe {\n        component(buf_ptr, 0)\n    } as *mut Area;\n    let save = unsafe { &mut *save_ptr };\n    let ok = unsafe { &*other };\n}\n";
        let v = version("foo.rs", rust);
        let f = &crate::extract::extract(Lang::Rust, "foo.rs", rust).functions[0];
        let l = invented_lifetime(&v, f);
        assert_eq!(l.len(), 1, "{l:?}");
        assert_eq!(l[0].line, 6);
        assert!(l[0].message.contains("`arch.buffer`"), "{}", l[0].message);
    }

    #[test]
    fn mut_cell_ref_held_across_self_call_is_reported() {
        let rust = "impl State {\n    pub fn init(&self) {\n        unsafe {\n            let events = &mut *self.events.get();\n            events[0] = 1;\n            let watermarks = &mut *self.mem_watermarks.get();\n            watermarks[0] = 10;\n            let _ = NonNull::from(self.signal.as_event());\n            if self.is_imminent_oom_enabled() {\n                kprintln!(\"{}\", watermarks[1]);\n            }\n        }\n    }\n}\n";
        let v = version("memory_watchdog.rs", rust);
        let f = &crate::extract::extract(Lang::Rust, "memory_watchdog.rs", rust).functions[0];
        let l = invented_lifetime(&v, f);
        assert_eq!(l.len(), 1, "{l:?}");
        assert_eq!(l[0].line, 6);
        assert!(
            l[0].message.contains("`watermarks`")
                && l[0].message.contains("`self.is_imminent_oom_enabled()`")
                && l[0].message.contains("line 10"),
            "{}",
            l[0].message
        );
    }

    #[test]
    fn mangled_symbols_are_reported() {
        let v = version(
            "foo.rs",
            "unsafe extern \"C\" {\n    fn _ZNK4Lamp10brightnessEv(this: *const c_void) -> u32;\n    #[link_name = \"_Z9lamp_initv\"]\n    fn lamp_init();\n    fn cpp_lamp_off(lamp: *mut c_void);\n}\n\n/// Mirrors `_ZN4Lamp2onEv`.\n#[unsafe(export_name = \"_Z8lamp_dimj\")]\npub extern \"C\" fn lamp_dim(level: u32) {}\n\nfn use_it(l: *const c_void) -> u32 {\n    unsafe { lamp_init() };\n    unsafe { _ZNK4Lamp10brightnessEv(l) }\n}\n",
        );
        let l = mangled_symbols(&v);
        let got: Vec<(usize, &str)> = l.iter().map(|l| (l.line, l.message.as_str())).collect();
        assert_eq!(l.len(), 3, "{got:?}");
        assert_eq!(l[0].line, 2);
        assert!(
            l[0].message.contains("`Lamp::brightness() const`")
                && l[0].message.contains("called at line 14"),
            "{}",
            l[0].message
        );
        assert_eq!(l[1].line, 3);
        assert!(l[1].message.contains("`lamp_init()`") && l[1].message.contains("line 13"));
        assert_eq!(l[2].line, 9);
        assert!(l[2]
            .message
            .starts_with("Rust defines C++ `lamp_dim(unsigned int)`"));
    }

    #[test]
    fn provenance_comments_are_reported_once_per_file() {
        let v = version(
            "foo.rs",
            "//! Ported from `kernel/foo.cc`.\n\n/// Frees the page (`foo.cc:120-134`).\nfn free() {}\n\n// Mirrors the C++ behavior of checking twice.\n// SAFETY: from foo.cc:12 as well.\nfn g() {}\n",
        );
        let l = provenance(&v);
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].line, 1);
        assert!(
            l[0].message.starts_with("2 comments say"),
            "{}",
            l[0].message
        );
    }

    /// The `(first line, last line, text)` of each comment in `src`.
    fn comment_spans(src: &str) -> Vec<(usize, usize, String)> {
        comments(&version("foo.rs", src))
            .into_iter()
            .map(|comment| (comment.first_line, comment.last_line(), comment.text))
            .collect()
    }

    /// The line each provenance match in `src` is reported on.
    fn provenance_lines(src: &str) -> Vec<usize> {
        comments(&version("foo.rs", src))
            .iter()
            .filter_map(|comment| provenance_match(comment).map(|at| comment.line_at(at)))
            .collect()
    }

    #[test]
    fn comments_split_into_runs_of_one_kind() {
        let src = concat!(
            "//! Module.\n",
            "/// Doc one,\n",
            "/// doc two.\n",
            "///\n",
            "/// Next paragraph.\n",
            "// Inline\n",
            "// SAFETY: sound.\n",
            "// More safety.\n",
            "x(); // trailing\n",
            "// After code.\n",
            "fn main() { // no args\n",
            "} // end\n",
            "//////////\n",
            "/// Doc.\n",
        );
        let spans = comment_spans(src);
        let expected = [
            (1, 1, "Module."),
            (2, 5, "Doc one, doc two.\n\nNext paragraph."),
            (6, 6, "Inline"),
            (7, 8, "SAFETY: sound. More safety."),
            (9, 9, "trailing"),
            (10, 10, "After code."),
            (11, 11, "no args"),
            (12, 12, "end"),
            (13, 13, ""),
            (14, 14, "Doc."),
        ];
        let got: Vec<(usize, usize, &str)> = spans
            .iter()
            .map(|(first, last, text)| (*first, *last, text.as_str()))
            .collect();
        assert_eq!(got, expected);

        let doc = &comments(&version("foo.rs", src))[1];
        assert_eq!(doc.line_at(doc.text.find("doc two").unwrap()), 3);
        assert_eq!(doc.line_at(doc.text.find("Next").unwrap()), 5);
    }

    #[test]
    fn paragraphs_split_at_blank_comment_lines() {
        let got: Vec<(usize, &str)> = paragraphs("One. Two.\n\nThree.").collect();
        assert_eq!(got, [(0, "One. Two."), (11, "Three.")]);
    }

    #[test]
    fn provenance_wrapped_across_lines_is_reported() {
        let v = version(
            "foo.rs",
            concat!(
                "/// Checks the bounds in the same order as the C++\n",
                "/// implementation.\n",
                "fn check() {}\n",
                "// SAFETY: ported from foo.cc; safety comments are exempt.\n",
                "unsafe fn h() {}\n",
            ),
        );
        let l = provenance(&v);
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].line, 1);
        assert!(
            l[0].message.starts_with("1 comment says"),
            "{}",
            l[0].message
        );
    }

    #[test]
    fn provenance_exemptions() {
        let src = concat!(
            // A keep-in-sync note exempts its whole comment.
            "/// Keep in sync with foo.h. The layout mirrors the C++ version.\n",
            "fn a() {}\n",
            // A TODO exempts its paragraph, across a `;`.
            "// TODO(b/1): drop this; it mirrors the C++ version until then.\n",
            "fn b() {}\n",
            // ... but not an earlier paragraph.
            "/// Ported from foo.cc.\n",
            "///\n",
            "/// TODO(b/1): make it faster.\n",
            "fn c() {}\n",
            // "must match" exempts only its own paragraph.
            "/// Ported from lamp.cc.\n",
            "///\n",
            "/// The two lengths must match.\n",
            "fn d() {}\n",
            "/// The two lengths must match lamp.cc:12.\n",
            "fn e() {}\n",
        );
        assert_eq!(provenance_lines(src), [5, 9]);
    }

    #[test]
    fn provenance_example_quotes_the_match() {
        let v = version(
            "foo.rs",
            concat!(
                "/// Does a lot of things, and this first line of the comment is quite long.\n",
                "/// It goes on for a while before it says that it was\n",
                "/// ported from foo.cc.\n",
                "fn f() {}\n",
            ),
        );
        let l = provenance(&v);
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].line, 3);
        assert!(
            l[0].message.contains("ported from foo.cc"),
            "{}",
            l[0].message
        );
    }

    /// A change that deletes `lamp.cc` (and its `Lamp` class and `Mode` enum),
    /// keeps `switch.h`, and adds `lamp.rs` and `switch.rs`.
    fn lamp_port(lamp_rs: &str, switch_rs: &str) -> ChangeSet {
        let lamp_cc = concat!(
            "enum class Mode : uint32_t {\n",
            "  // clang-format off\n",
            "  Off = 0,\n",
            "  Dim = 1,\n",
            "  // clang-format on\n",
            "};\n",
            "\n",
            "class Lamp {\n",
            " public:\n",
            "  void TurnOn() { on_ = true; }\n",
            "\n",
            " private:\n",
            "  bool on_ = false;\n",
            "};\n",
        );
        let switch_h = "struct Switch {\n  void Flip();\n};\n";
        ChangeSet {
            cpp_old: vec![
                version("kernel/lamp.cc", lamp_cc),
                version("kernel/switch.h", switch_h),
            ],
            cpp_new: vec![version("kernel/switch.h", switch_h)],
            rust_new: vec![
                version("kernel/src/lamp.rs", lamp_rs),
                version("kernel/src/switch.rs", switch_rs),
            ],
        }
    }

    #[test]
    fn stale_cpp_references_are_reported() {
        let lamp_rs = concat!(
            "/// Turns the lamp on, like the C++ `TurnOn`.\n",
            "fn turn_on() {}\n",
            "\n",
            "// There is no `Off` flag, unlike the C++\n",
            "// enum. Use `empty()` instead.\n",
            "fn modes() {}\n",
            "\n",
            "/// Statically initialized, like the C++ global was.\n",
            "static LAMP: u32 = 0;\n",
            "\n",
            "// SAFETY: written once at boot, the same as when the lamp was defined in C++.\n",
            "fn g() {}\n",
        );
        let l = stale_cpp_references(&lamp_port(lamp_rs, ""));
        let got: Vec<(usize, Severity, &str)> = l
            .iter()
            .map(|lint| (lint.line, lint.severity, lint.message.as_str()))
            .collect();
        assert_eq!(l.len(), 2, "{got:?}");
        // One issue for the file, covering both named symbols; the second
        // spans two comment lines.
        assert_eq!((l[0].line, l[0].severity), (1, Severity::Issue));
        assert!(
            l[0].message
                .starts_with("2 comments name C++ that this change deletes (lines 1, 4)")
                && l[0].message.contains("`TurnOn`"),
            "{}",
            l[0].message
        );
        // One note for the comparisons, including the one in a SAFETY comment.
        assert_eq!((l[1].line, l[1].severity), (8, Severity::Note));
        assert!(
            l[1].message.starts_with(
                "2 comments compare the Rust with C++ this change deletes \
                 (lines 8, 11)"
            ),
            "{}",
            l[1].message
        );
    }

    #[test]
    fn provenance_comments_are_left_to_provenance() {
        let lamp_rs = concat!(
            "// Same check order as the C++ implementation.\n",
            "fn a() {}\n",
            "\n",
            "// SAFETY: the C++\n",
            "// implementation shared it under the same lock.\n",
            "unsafe impl Send for A {}\n",
        );
        let l = stale_cpp_references(&lamp_port(lamp_rs, ""));
        let got: Vec<(usize, Severity)> = l.iter().map(|lint| (lint.line, lint.severity)).collect();
        assert_eq!(got, [(4, Severity::Note)]);
    }

    #[test]
    fn keep_in_sync_comments_are_not_reported() {
        let lamp_rs = concat!(
            "// Must match the layout of the C++ global, like the C++ struct.\n",
            "static LAYOUT: u32 = 0;\n",
            "// TODO: drop this once nothing reads it like the C++ global did.\n",
            "static OLD: u32 = 0;\n",
        );
        let l = stale_cpp_references(&lamp_port(lamp_rs, ""));
        assert!(l.is_empty(), "{l:?}");
    }

    #[test]
    fn references_to_cpp_that_stays_are_not_reported() {
        // `Switch` is still declared in C++, and `switch.rs` isn't named after
        // a deleted C++ file, so comparisons there describe live C++.
        let lamp_rs = "/// Binds to the C++ `Switch::Flip`, which stays in C++.\nfn flip() {}\n";
        let switch_rs = concat!(
            "/// Flips the switch, like the C++ `Flip`.\n",
            "fn flip() {}\n",
            "\n",
            "/// Statically initialized, like the C++ global.\n",
            "static S: u32 = 0;\n",
        );
        let l = stale_cpp_references(&lamp_port(lamp_rs, switch_rs));
        assert!(l.is_empty(), "{l:?}");
    }

    #[test]
    fn nothing_is_reported_when_no_cpp_is_deleted() {
        let v = version(
            "foo.rs",
            "/// Like the C++ global was.\nstatic X: u32 = 0;\n",
        );
        let cs = ChangeSet {
            cpp_old: Vec::new(),
            cpp_new: Vec::new(),
            rust_new: vec![v],
        };
        assert!(stale_cpp_references(&cs).is_empty());
    }

    #[test]
    fn nested_cpp_declarations_are_deleted_names() {
        let lamp_cc = concat!(
            "class Lamp {\n",
            " public:\n",
            "  enum class State { kOff, /* dark */ kDim };\n",
            "  int enum_count_ = 0;\n",
            "};\n",
        );
        let cs = ChangeSet {
            cpp_old: vec![version("kernel/lamp.cc", lamp_cc)],
            cpp_new: Vec::new(),
            rust_new: vec![version("kernel/src/lamp.rs", "fn on() {}\n")],
        };
        let names = deleted_cpp_names(&cs, &deleted_cpp_paths(&cs));
        let got: Vec<&str> = names.iter().map(String::as_str).collect();
        assert_eq!(got, ["Lamp", "State", "kDim", "kOff"]);
    }

    #[test]
    fn names_the_rust_declares_are_not_deleted() {
        // `None` is the Rust prelude's, and the Rust keeps the `Lamp` name.
        let lamp_rs = concat!(
            "/// Returns `None` if the C++ caller passes null.\n",
            "fn get() -> Option<u32> { None }\n",
            "\n",
            "/// Exported to C++ as an opaque `Lamp` pointer.\n",
            "pub struct Lamp;\n",
        );
        let mut cs = lamp_port(lamp_rs, "");
        cs.cpp_old[0].text.push_str("enum Status { None, Busy };\n");
        let l = stale_cpp_references(&cs);
        assert!(
            l.iter().all(|lint| lint.severity != Severity::Issue),
            "{l:?}"
        );
    }

    #[test]
    fn rust_enum_variants_are_declared_names() {
        let lamp_rs = concat!(
            "enum Mode {\n",
            "    Dim { level: u8 },\n",
            "    #[default]\n",
            "    Off,\n",
            "    Lit(u64),\n",
            "}\n",
            "// Unlike the C++ `Off` or `Dim`, it stays lit.\n",
            "static S: u32 = 0;\n",
        );
        let cs = lamp_port(lamp_rs, "");
        let names = rust_declared_names(&cs);
        assert!(
            ["Mode", "Dim", "Off", "Lit"]
                .iter()
                .all(|n| names.contains(*n)),
            "{names:?}"
        );
        assert!(
            !names.contains("level") && !names.contains("u64"),
            "{names:?}"
        );
        let l = stale_cpp_references(&cs);
        assert!(
            l.iter().all(|lint| lint.severity != Severity::Issue),
            "{l:?}"
        );
    }

    #[test]
    fn a_deleted_name_after_a_comparison_is_an_issue() {
        let lamp_rs = concat!(
            "// Unlike the C++ global, this is static. The C++ `TurnOn` did it.\n",
            "static ON: bool = false;\n",
            "\n",
            "// Same as the C++ `lamp.TurnOn()` call.\n",
            "static ALSO: bool = false;\n",
        );
        let l = stale_cpp_references(&lamp_port(lamp_rs, ""));
        let got: Vec<(usize, Severity)> = l.iter().map(|lint| (lint.line, lint.severity)).collect();
        assert_eq!(got, [(1, Severity::Issue)]);
        assert!(
            l[0].message.starts_with("2 comments name"),
            "{}",
            l[0].message
        );
    }

    #[test]
    fn a_deleted_name_anywhere_in_a_paragraph_about_cpp_is_an_issue() {
        // "C++" is on the first line and the deleted name on the next.
        let lamp_rs = concat!(
            "// In the original C++ implementation, the fields were unguarded.\n",
            "// `TurnOn` was intentionally unannotated because it runs unlocked.\n",
            "static ON: bool = false;\n",
        );
        let l = stale_cpp_references(&lamp_port(lamp_rs, ""));
        let got: Vec<(usize, Severity)> = l.iter().map(|lint| (lint.line, lint.severity)).collect();
        assert_eq!(got, [(2, Severity::Issue)]);
    }

    #[test]
    fn comparing_files_reports_nothing() {
        let cs = ChangeSet::from_files(&[
            (
                "kernel/lamp.cc".to_string(),
                "class Lamp { void TurnOn(); };\n".to_string(),
            ),
            (
                "kernel/lamp.rs".to_string(),
                "/// Like the C++ `TurnOn`.\nfn turn_on() {}\n".to_string(),
            ),
        ]);
        assert!(stale_cpp_references(&cs).is_empty());
    }

    #[test]
    fn templates_annotations_and_anonymous_enums_are_parsed() {
        let lamp_cc = concat!(
            "template <class T> struct Holder { T t; };\n",
            "template <typename U> class Bar {};\n",
            "class TA_CAP(\"mutex\") LampLock {};\n",
            "struct alignas(64) Padded {};\n",
            "struct [[nodiscard]] Res final {};\n",
            "enum : uint32_t { kAnonA = 1, kAnonB };\n",
        );
        let cs = ChangeSet {
            cpp_old: vec![version("kernel/lamp.cc", lamp_cc)],
            cpp_new: Vec::new(),
            rust_new: vec![version("kernel/src/lamp.rs", "fn on() {}\n")],
        };
        let names = deleted_cpp_names(&cs, &deleted_cpp_paths(&cs));
        let got: Vec<&str> = names.iter().map(String::as_str).collect();
        assert_eq!(
            got,
            ["Bar", "Holder", "LampLock", "Padded", "Res", "kAnonA", "kAnonB"]
        );
    }

    #[test]
    fn matching_extern_signatures_pass() {
        let cpp = r#"
extern "C" {
FFI_ALWAYS_INLINE zx_status_t cpp_foo_create(uint32_t count, Foo** out) { return Foo::Create(count, out); }
FFI_ALWAYS_INLINE const block_t* cpp_foo_block(const Foo* f) { return &f->block(); }
FFI_ALWAYS_INLINE void cpp_foo_with_lock(Foo* f, void (*cb)(void*), void* ctx) { cb(ctx); }
void rust_foo_init(const Foo* f, size_t len);
}
"#;
        let rust = r#"
unsafe extern "C" {
    fn cpp_foo_create(count: u32, out: *mut *mut Foo) -> zx_status_t;
    fn cpp_foo_block(f: *const Foo) -> *const Block;
    fn cpp_foo_with_lock(f: *mut Foo, cb: extern "C" fn(*mut u8), ctx: *mut u8);
}
/// # Safety
///
/// `f` is valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_foo_init(f: *const Foo, len: usize) {}
"#;
        let l = lint(&cs(cpp, rust));
        assert!(messages(&l, LintKind::ExternSignature).is_empty(), "{l:?}");
    }

    #[test]
    fn mismatched_extern_signatures_are_reported() {
        let cpp = r#"
extern "C" {
zx_status_t cpp_foo_reserve(Foo* f, uint32_t id) { return f->Reserve(id); }
void cpp_foo_poke(Foo* f) { f->Poke(); }
void rust_foo_set(Foo* f, uint64_t v);
}
"#;
        let rust = r#"
unsafe extern "C" {
    fn cpp_foo_reserve(f: *mut Foo, id: u64) -> zx_status_t;
    fn cpp_foo_poke(f: *mut Foo, extra: u32);
}
#[unsafe(no_mangle)]
pub extern "C" fn rust_foo_set(f: *mut Foo, v: u64) -> zx_status_t { 0 }
"#;
        let l = lint(&cs(cpp, rust));
        let m = messages(&l, LintKind::ExternSignature);
        assert_eq!(m.len(), 3, "{m:?}");
        assert!(
            m.iter().any(|x| x.contains("cpp_foo_reserve: parameter 2")),
            "{m:?}"
        );
        assert!(
            m.iter()
                .any(|x| x.contains("C++ takes 1 parameter, Rust 2")),
            "{m:?}"
        );
        assert!(
            m.iter().any(|x| x.contains("rust_foo_set: returns")),
            "{m:?}"
        );
    }

    #[test]
    fn missing_cpp_header_prototype_is_reported() {
        let hdr = r#"
extern "C" {
void cpp_event_dispatcher_declared(EventDispatcher* e);
}
"#;
        let ffi_cc = r#"
#include <object/event_dispatcher.h>

extern "C" {
void cpp_event_dispatcher_declared(EventDispatcher* e) { e->Declared(); }
FFI_ALWAYS_INLINE zx_status_t cpp_event_dispatcher_user_signal_self(EventDispatcher* e, uint32_t clear_mask, uint32_t set_mask) {
  return e->user_signal_self(clear_mask, set_mask);
}
}
"#;
        let rust = r#"
unsafe extern "C" {
    fn cpp_event_dispatcher_declared(e: *mut EventDispatcher);
    fn cpp_event_dispatcher_user_signal_self(e: *mut EventDispatcher, clear_mask: u32, set_mask: u32) -> zx_status_t;
}
"#;
        let cs = ChangeSet {
            cpp_old: Vec::new(),
            cpp_new: vec![
                version(
                    "zircon/kernel/object/include/object/event_dispatcher.h",
                    hdr,
                ),
                version("zircon/kernel/object/event_dispatcher_ffi.cc", ffi_cc),
            ],
            rust_new: vec![version("zircon/kernel/object/event_dispatcher.rs", rust)],
        };
        let l = lint(&cs);
        let m = messages(&l, LintKind::ExternSignature);
        assert_eq!(
            m,
            ["cpp_event_dispatcher_user_signal_self: defined without an `extern \"C\"` prototype declaration in event_dispatcher.h"],
            "{l:?}"
        );
    }

    #[test]
    fn unsafe_needs_safety_comments() {
        let rust = r#"
fn a(p: *const u8) -> u8 {
    // SAFETY: `p` is valid.
    let x = unsafe { *p };
    let y = unsafe { *p };
    x + y
}
unsafe fn b() {}
/// Does c.
///
/// # Safety
///
/// Never.
unsafe fn c() {}
unsafe impl Send for Foo {}
// SAFETY: Bar is only touched under its lock.
unsafe impl Sync for Bar {}
"#;
        let l = lint(&cs("", rust));
        let m = messages(&l, LintKind::UnsafeSafety);
        assert_eq!(
            m,
            [
                "unsafe block in a without a // SAFETY: comment",
                "unsafe fn b without a # Safety doc section",
                "unsafe impl without a // SAFETY: comment"
            ],
            "{l:?}"
        );
        assert_eq!(
            l.iter()
                .find(|l| l.kind == LintKind::UnsafeSafety)
                .unwrap()
                .line,
            5
        );
    }

    #[test]
    fn shims_that_branch_are_reported() {
        let cpp = r#"
extern "C" {
zx_status_t cpp_foo_create(Foo** out) {
  fbl::RefPtr<Foo> f;
  zx_status_t status = Foo::Create(&f);
  if (status != ZX_OK) {
    return status;
  }
  *out = fbl::ExportToRawPtr(&f);
  return ZX_OK;
}
void cpp_foo_print(const Foo* f, uint32_t kind) {
  if (kind == 1) {
    f->PrintA();
  } else if (kind == 2) {
    f->PrintB();
  }
}
}
"#;
        let rust = r#"
#[unsafe(no_mangle)]
pub extern "C" fn rust_foo_get(f: &Foo, out: &mut u32) -> zx_status_t {
    match f.get() {
        Ok(v) => {
            *out = v;
            0
        }
        Err(e) => e.into_raw(),
    }
}
#[unsafe(no_mangle)]
pub extern "C" fn rust_foo_sum(f: &Foo) -> u32 {
    let mut s = 0;
    for x in f.items() {
        s += x;
    }
    s
}
"#;
        let l = lint(&cs(cpp, rust));
        let m = messages(&l, LintKind::ShimLogic);
        assert_eq!(m.len(), 2, "{m:?}");
        assert!(m
            .iter()
            .any(|x| x.contains("cpp_foo_print does more than forward: 2 branches")));
        assert!(m
            .iter()
            .any(|x| x.contains("rust_foo_sum does more than forward: 1 loop")));
    }
}
