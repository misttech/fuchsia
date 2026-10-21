//! Pairs removed C++ functions with added Rust functions and checks each
//! pair.

use crate::align::{self, Pair};
use crate::check::{self, Finding, Row, Severity, Summary};
use crate::layout;
use crate::model::{Function, Unit, UnitKind};
use crate::normalize::{self, seq_similarity};
use std::collections::{HashMap, HashSet};

/// How a pair was found.
#[derive(Clone, Debug, PartialEq)]
pub enum Link {
    /// The new C++ body calls an FFI shim that forwards to this Rust function.
    Ffi { shim: String, shim_location: String },
    /// The FFI shim's name follows the `rust_<class>_<method>` convention.
    FfiName { shim: String, shim_location: String },
    /// Names and bodies are similar.
    Similarity,
    /// Requested explicitly.
    Forced,
}

/// Where the C++ side came from.
#[derive(Clone, Debug, PartialEq)]
pub enum CppOrigin {
    /// Removed or rewritten by the change.
    Changed,
    /// Not touched by the change; found in the repository.
    Unchanged,
}

#[derive(Clone, Debug)]
pub struct PairReport {
    pub cpp: Function,
    pub rust: Function,
    pub link: Link,
    /// A thin Rust function that forwards to `rust`, standing in for the C++
    /// by name.
    pub forwarder: Option<Function>,
    pub origin: CppOrigin,
    pub score: f64,
    pub rows: Vec<Row>,
    pub findings: Vec<Finding>,
    pub summary: Summary,
    /// C++ overrides of the same method in related classes whose bodies the
    /// Rust function also carries, typically as arms of a `match` on an
    /// enum that replaced the class hierarchy.
    pub overrides: Vec<OverrideReport>,
    /// Why the pair was chosen, for readers who need to judge it.
    pub rationale: String,
    /// For a second Rust definition of an already paired function, where
    /// the first one is.
    pub duplicate_of: Option<(String, usize)>,
}

/// A C++ override folded into the Rust function of a pair.
#[derive(Clone, Debug)]
pub struct OverrideReport {
    pub cpp: Function,
    /// Rows of the override against the Rust units it matched. Rust units
    /// that belong to the primary C++ function or other overrides are left
    /// out.
    pub rows: Vec<Row>,
    pub findings: Vec<Finding>,
}

impl PairReport {
    /// Every finding of the pair, including those of its overrides.
    pub fn all_findings(&self) -> impl Iterator<Item = &Finding> {
        self.findings
            .iter()
            .chain(self.overrides.iter().flat_map(|o| o.findings.iter()))
    }
    pub fn issues(&self) -> usize {
        self.all_findings()
            .filter(|f| f.severity == Severity::Issue)
            .count()
    }
    pub fn notes(&self) -> usize {
        self.all_findings()
            .filter(|f| f.severity == Severity::Note)
            .count()
    }
}

/// Issue counts by category, most first.
pub fn issues_by_category(report: &Report) -> Vec<(check::Category, usize)> {
    let mut counts: HashMap<check::Category, usize> = HashMap::new();
    for f in report.pairs.iter().flat_map(PairReport::all_findings) {
        if f.severity == Severity::Issue {
            *counts.entry(f.category).or_default() += 1;
        }
    }
    let mut v: Vec<(check::Category, usize)> = counts.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    v
}

/// An FFI shim and the Rust function it forwards to.
#[derive(Clone, Debug)]
pub struct Shim {
    pub shim: Function,
    pub target: Option<String>,
    /// Functions the shim could forward to when babeldiff can't tell which.
    pub ambiguous: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Report {
    pub pairs: Vec<PairReport>,
    pub unmatched_cpp: Vec<Function>,
    /// Changed or removed C++ helpers that Rust calls through FFI
    /// (`cpp_<class>_<method>` in a `*_ffi.cc` file). They have no Rust
    /// counterpart.
    pub removed_cpp_shims: Vec<Function>,
    /// Rust functions that only call into C++ through a `cpp_*` FFI helper,
    /// with the helper's name. They wrap C++ rather than convert it.
    pub rust_facades: Vec<(Function, String)>,
    pub unmatched_rust: Vec<Function>,
    /// Rust tests with no C++ counterpart, kept apart from the other
    /// unpaired Rust.
    pub rust_tests: Vec<Function>,
    pub shims: Vec<Shim>,
    /// C++ the change modified that is not part of the port (not removed,
    /// not a forwarder into Rust, not an FFI declaration). Reviewers check
    /// these by hand.
    pub cpp_changes: Vec<crate::input::CppChange>,
    /// Rubric lints over the changed files.
    pub lints: Vec<crate::lint::Lint>,
    /// Which Rust files each C++ file's functions went to.
    pub placement: Vec<crate::placement::Placement>,
}

impl Report {
    /// Drops notes and the pairs left with no issues, for readers who only
    /// want what needs fixing.
    pub fn retain_issues(&mut self) {
        let strip = |rows: &mut Vec<Row>, findings: &mut Vec<Finding>| {
            for r in rows.iter_mut() {
                r.notes.retain(|n| n.severity == Severity::Issue);
                if r.marker == check::Marker::Note {
                    r.marker = check::Marker::Same;
                }
            }
            findings.retain(|f| f.severity == Severity::Issue);
        };
        for p in &mut self.pairs {
            strip(&mut p.rows, &mut p.findings);
            for o in &mut p.overrides {
                strip(&mut o.rows, &mut o.findings);
            }
        }
        self.pairs.retain(|p| p.issues() > 0);
        self.lints.retain(|l| l.severity == Severity::Issue);
    }

    /// Lints that are issues.
    pub fn lint_issues(&self) -> usize {
        self.lints
            .iter()
            .filter(|l| l.severity == Severity::Issue)
            .count()
    }

    pub fn issues(&self) -> usize {
        self.pairs.iter().map(PairReport::issues).sum()
    }

    /// The paired Rust functions that call an unpaired Rust function, for
    /// showing it as a helper of the code it serves.
    pub fn callers(&self, f: &Function) -> Vec<&str> {
        let mut v: Vec<&str> = self
            .pairs
            .iter()
            .filter(|p| p.rust.name != f.name && calls_function(&p.rust, f))
            .map(|p| p.rust.name.as_str())
            .collect();
        v.sort();
        v.dedup();
        v
    }
    pub fn notes(&self) -> usize {
        self.pairs.iter().map(PairReport::notes).sum()
    }
}

/// Inputs to [`analyze`].
#[derive(Default)]
pub struct Inputs {
    /// C++ functions as they were before the change.
    pub cpp: Vec<Function>,
    /// Rust functions added or rewritten by the change.
    pub rust: Vec<Function>,
    /// Every Rust function in the touched files, used to follow FFI shims.
    pub all_rust: Vec<Function>,
    /// Calls made by the post-change version of each C++ function, keyed by
    /// qualified name. This is how C++ bodies that became FFI calls are
    /// linked to the Rust that replaced them.
    pub cpp_new_calls: HashMap<String, Vec<String>>,
    /// Explicit `(C++ name, Rust name)` pairs.
    pub forced: Vec<(String, String)>,
    /// Base classes of the C++ classes defined in the changed files.
    pub cpp_bases: crate::cpp::ClassBases,
    /// Changed C++ that stays C++.
    pub cpp_changes: Vec<crate::input::CppChange>,
    /// `cpp_*` helpers in the C++ after the change, which Rust calls back
    /// into.
    pub cpp_helpers: Vec<Function>,
    /// Every C++ file the change touched.
    pub cpp_changed_paths: Vec<String>,
    /// Function-like macros the old C++ defines, as written.
    pub cpp_macros: Vec<String>,
    /// Constants the changed files define.
    pub values: crate::values::Values,
}

#[derive(Clone, Debug)]
pub struct Options {
    /// Minimum score for pairing by similarity.
    pub min_score: f64,
    /// Minimum score for pairing with C++ that the change did not touch.
    pub min_unchanged_score: f64,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            min_score: 0.35,
            min_unchanged_score: 0.5,
        }
    }
}

/// Looks up C++ functions outside the change that might correspond to a
/// Rust function. Implemented by the git layer.
pub trait CppFinder {
    fn find(&mut self, rust: &Function) -> Vec<Function>;

    /// Which of `lines` (trimmed source text) are still in C++ files the
    /// change did not touch (`changed`).
    fn still_present(&mut self, _lines: &[String], _changed: &[String]) -> HashSet<String> {
        HashSet::new()
    }
}

/// A finder that finds nothing.
pub struct NoFinder;
impl CppFinder for NoFinder {
    fn find(&mut self, _: &Function) -> Vec<Function> {
        Vec::new()
    }
}

fn operator_alias(base: &str) -> &str {
    match base.split_whitespace().collect::<String>().as_str() {
        "operator==" => "eq",
        "operator!=" => "ne",
        "operator<" => "lt",
        "operator<=" => "le",
        "operator>" => "gt",
        "operator>=" => "ge",
        "operator[]" => "index",
        "operator()" => "call",
        "operator=" => "assign",
        "operatorbool" => "is_valid",
        _ => base,
    }
}

fn name_words(f: &Function) -> Vec<String> {
    if f.lang == crate::model::Lang::Cpp {
        let class = f
            .class
            .as_deref()
            .map(|c| c.rsplit("::").next().unwrap_or(c));
        if f.base.starts_with('~') {
            // Destructors become `Drop::drop`.
            return vec!["drop".to_string()];
        }
        if class == Some(f.base.as_str()) {
            // Constructors become `new` or `init`.
            return vec!["new".to_string()];
        }
    }
    normalize::words(operator_alias(&f.base))
}

/// Similarity of two function names, tolerant of CamelCase vs snake_case.
pub fn name_similarity(a: &Function, b: &Function) -> f64 {
    let base = seq_similarity(&name_words(a), &name_words(b));
    let last = |f: &Function| {
        f.class
            .as_deref()
            .map(|c| normalize::words(c.rsplit("::").next().unwrap_or(c)))
    };
    match (last(a), last(b)) {
        (Some(ca), Some(cb)) => 0.8 * base + 0.2 * seq_similarity(&ca, &cb),
        // A method and a free function are less alike than two methods or
        // two free functions with the same name.
        (Some(_), None) | (None, Some(_)) => 0.8 * base,
        (None, None) => base,
    }
}

/// Body similarity with comments weighted double, since comments are
/// supposed to carry over unchanged.
fn body_similarity(a: &Function, b: &Function, pairs: &[Pair]) -> f64 {
    let w = |u: &crate::model::Unit| match u.kind {
        UnitKind::Comment => 2.0,
        // Signatures always align; they say nothing about the bodies.
        UnitKind::Signature => 0.0,
        _ => 1.0,
    };
    let total: f64 = a.units.iter().map(w).sum::<f64>() + b.units.iter().map(w).sum::<f64>();
    if total == 0.0 {
        return 0.0;
    }
    let matched: f64 = pairs
        .iter()
        .filter_map(|p| Some((p.cpp?, p.rust?, p.score)))
        .map(|(i, j, s)| s * (w(&a.units[i]) + w(&b.units[j])))
        .sum();
    matched / total
}

/// Small bodies look alike by accident, so short functions need similar names.
/// Whether one name asks about what the other returns:
/// `supports_page_size` against `page_size`.
fn asks_instead(a: &Function, b: &Function) -> bool {
    let (wa, wb) = (
        normalize::words(&normalize::ident(&a.base)),
        normalize::words(&normalize::ident(&b.base)),
    );
    let predicate = |x: &[String], y: &[String]| {
        x.len() == y.len() + 1
            && matches!(x[0].as_str(), "supports" | "is" | "has" | "can" | "should")
            && x[1..] == *y
    };
    predicate(&wa, &wb) || predicate(&wb, &wa)
}

/// Whether unchanged C++ `c` is somewhere Rust `r`'s counterpart could be.
/// C++ outside the Rust's component (see [`layout::related`]) must be the
/// same function by class and name: the Rust may duplicate a generic
/// helper, but a merely similar method there is someone else's code.
fn expected_place(c: &Function, r: &Function) -> bool {
    if layout::arch_conflict(&c.path, &r.path) {
        return false;
    }
    layout::related(&c.path, &r.path)
        || (class_key(c) == class_key(r) && name_words(c) == name_words(r))
}

/// The code between a function's outermost braces, without comments.
fn body_text(f: &Function) -> String {
    let code: String = f
        .lines
        .iter()
        .map(|l| l.split("//").next().unwrap_or(""))
        .filter(|l| !l.trim_start().starts_with("#["))
        .collect::<Vec<_>>()
        .join("\n");
    let start = code.find("fn ").or_else(|| code.find('(')).unwrap_or(0);
    let open = code[start..].find('{').map(|i| start + i + 1);
    let close = code.rfind('}');
    match (open, close) {
        (Some(o), Some(c)) if o <= c => code[o..c].trim().to_string(),
        _ => String::new(),
    }
}

/// A body that does nothing but return a constant (or nothing at all):
/// `{ 0 }`, `{ false }`, `{ return nullptr; }`, `{ todo!() }`.
fn constant_body(f: &Function) -> bool {
    static CONST: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r#"^(?:return\s*)?(?:-?(?:0[xX][0-9a-fA-F_]+|\d[\d_]*)(?:u8|u16|u32|u64|usize|i32|i64|U|UL|ULL|u)?|true|false|None|nullptr|NULL|\(\)|Ok\(\(\)\)|Default::default\(\)|(?:core::)?(?:ptr::)?null(?:_mut)?\(\)|"[^"]*"|(?:unimplemented|todo|unreachable)!\([^)]*\))?\s*;?$"#,
        )
        .unwrap()
    });
    CONST.is_match(&body_text(f))
}

/// A Rust placeholder standing in for C++ with real logic: its body is a
/// constant, or hands a parameter back unchanged (`fn pt_phys(p) -> u64 { p }`).
/// Such a stub is never the port of the C++.
fn stub_of(c: &Function, r: &Function) -> bool {
    if r.lang != crate::model::Lang::Rust || body_len(r) > 1 || constant_body(c) {
        return false;
    }
    if constant_body(r) {
        return true;
    }
    let body = body_text(r);
    let body = body.trim_end_matches(';').trim();
    let sig = r
        .lines
        .iter()
        .find(|l| l.contains("fn "))
        .map_or("", |l| l.as_str());
    !body.is_empty()
        && body.chars().all(|ch| ch.is_alphanumeric() || ch == '_')
        && sig.contains(&format!("{body}:"))
}

/// A Rust function that only passes its arguments on to one other function
/// named like it (`fn x86_xsetbv(r, v) { unsafe { xsetbv(r, v) } }`), not
/// to a shared utility (`arch_dcache_line_size` calling
/// `cpuid_cache_line_size_bytes`).
fn passes_through(r: &Function, t: &Function) -> bool {
    let (own, theirs) = (
        normalize::words(&normalize::ident(&r.base)),
        normalize::words(&normalize::ident(&t.base)),
    );
    body_len(r) <= 1
        && r.calls.len() == 1
        && calls_function(r, t)
        && (theirs.iter().all(|w| own.contains(w)) || own.iter().all(|w| theirs.contains(w)))
}

/// Whether a Rust method with the C++ method's class and name, or its name
/// plus a suffix (`AutoVmcs::read_16` for `AutoVmcs::Read`), exists. Then
/// the port is on that type, not a same-named method elsewhere.
fn has_twin(c: &Function, rust: &[Function]) -> bool {
    let (Some(ck), words) = (class_key(c), name_words(c)) else {
        return false;
    };
    rust.iter().any(|r| {
        !r.is_ffi
            && !r.test_only
            && class_key(r).as_ref() == Some(&ck)
            && name_words(r).starts_with(&words)
    })
}

/// For overloads ported as suffixed functions (`Read(VmcsField16)` as
/// `read_16`): +1 when the Rust name's extra words all appear in the C++
/// signature, -1 when one doesn't, 0 when the names are the same.
fn suffix_fit(c: &Function, r: &Function) -> f64 {
    let (cw, rw) = (name_words(c), name_words(r));
    if !rw.starts_with(&cw) || rw.len() == cw.len() {
        return 0.0;
    }
    let sig = c
        .lines
        .iter()
        .find(|l| l.contains('('))
        .map_or(String::new(), |l| l.to_lowercase());
    if rw[cw.len()..].iter().all(|w| sig.contains(w.as_str())) {
        1.0
    } else {
        -1.0
    }
}

/// Whether a Rust function other than `r` has `c`'s class and name
/// (or its name plus a suffix, for a method).
fn twin_elsewhere(c: &Function, r: &Function, rust: &[Function]) -> bool {
    let words = name_words(c);
    let twin = |u: &Function| {
        !u.is_ffi
            && !u.test_only
            && class_key(u) == class_key(c)
            && if c.class.is_some() {
                name_words(u).starts_with(&words)
            } else {
                name_words(u) == words
            }
    };
    !twin(r)
        && rust
            .iter()
            .any(|u| (u.path != r.path || u.start_line != r.start_line) && twin(u))
}

/// Whether two functions' names, with their classes, share a word beyond
/// architecture prefixes (`ResourceDispatcher::ResourceDispatcher` and
/// `ResourceDispatcherState::init` share `resource`).
fn shares_name_word(c: &Function, r: &Function) -> bool {
    let generic = |w: &String| {
        matches!(
            w.as_str(),
            "x86" | "x64" | "arch" | "cpp" | "rust" | "get" | "set" | "is"
        )
    };
    let words = |f: &Function| {
        let mut w = name_words(f);
        if let Some(k) = f.class.as_deref() {
            w.extend(normalize::words(&normalize::ident(
                k.rsplit("::").next().unwrap_or(k),
            )));
        }
        w
    };
    let cw = words(c);
    words(r).iter().any(|w| !generic(w) && cw.contains(w))
}

fn plausible(a: &Function, b: &Function) -> bool {
    let body = |f: &Function| {
        f.units
            .iter()
            .filter(|u| !matches!(u.kind, UnitKind::Signature | UnitKind::Comment))
            .count()
    };
    if asks_instead(a, b) || layout::arch_conflict(&a.path, &b.path) || stub_of(a, b) {
        return false;
    }
    // `RealCpuId::new` constructs; only a C++ constructor ports to it.
    let ctor = |f: &Function| matches!(f.base.as_str(), "new" | "drop" | "default");
    if ctor(b) && !matches!(name_words(a).as_slice(), [w] if ctor_word(w)) {
        return false;
    }
    body(a).min(body(b)) >= 3 || name_similarity(a, b) >= 0.5
}

fn ctor_word(w: &str) -> bool {
    matches!(w, "new" | "drop" | "default" | "init" | "create")
}

fn score(a: &Function, b: &Function) -> (f64, Vec<Pair>) {
    let (pairs, _) = align::align(&a.units, &b.units);
    let s = 0.35 * name_similarity(a, b) + 0.65 * body_similarity(a, b, &pairs);
    (s, pairs)
}

/// The shim's name with the FFI decoration removed:
/// `rust_beacon_dispatcher_flash` -> `beacon_dispatcher_flash`.
fn undecorated(shim: &str) -> String {
    let n = normalize::ident(shim);
    let n = n.strip_prefix("rust_").unwrap_or(&n);
    let n = n.strip_suffix("_ffi").unwrap_or(n);
    n.to_string()
}

/// What an FFI shim forwards to.
enum Target {
    One(Box<Function>),
    /// Several functions fit equally well.
    Ambiguous(Vec<String>),
    None,
}

/// `Foo::bar` as `foo::bar`, the form of [`crate::model::Features::qcalls`].
fn qualified_key(f: &Function) -> Option<String> {
    let class = f.class.as_deref()?;
    let class = class.rsplit("::").next().unwrap_or(class);
    Some(format!(
        "{}::{}",
        normalize::ident(class),
        normalize::ident(&f.base)
    ))
}

/// Fraction of directory names two paths share, in `[0, 1]`.
fn path_affinity(a: &str, b: &str) -> f64 {
    let dirs = |p: &str| -> Vec<String> {
        let mut v: Vec<String> = p.split('/').map(str::to_string).collect();
        v.pop();
        v
    };
    let (da, db) = (dirs(a), dirs(b));
    if da.is_empty() || db.is_empty() {
        return 0.0;
    }
    let shared = da.iter().filter(|d| db.contains(d)).count();
    shared as f64 / da.len().max(db.len()) as f64
}

/// A path's file name without extension and `_ffi` suffix.
fn file_stem(path: &str) -> String {
    let base = path.rsplit('/').next().unwrap_or(path);
    let s = base.split('.').next().unwrap_or(base);
    s.strip_suffix("_ffi").unwrap_or(s).to_string()
}

/// Finds the function an FFI shim forwards to. When the shim calls
/// several same-named methods (`A::create` and `B::create`), the type path
/// of the call, the shim's name and its file decide; if they don't, the
/// target is ambiguous rather than guessed.
fn shim_target(shim: &Function, rust: &[Function]) -> Target {
    let und = undecorated(&shim.base);
    // How a call to `r` appears in the shim's call list: `Foo::init` is
    // recorded as a call of `foo`, like a constructor.
    let key = |r: &Function| normalize::call(&r.name).unwrap_or_else(|| normalize::ident(&r.base));
    let scored: Vec<(usize, &Function)> = rust
        .iter()
        .filter(|r| !r.is_ffi && !r.test_only && r.name != shim.name)
        .filter(|r| shim.calls.contains(&key(r)))
        .map(|r| {
            let k = key(r);
            let b = normalize::ident(&r.base);
            let q = qualified_key(r);
            let by_path = q.as_ref().is_some_and(|q| shim.qcalls.contains(q));
            let exact = q.as_ref().is_some_and(|q| q.replace("::", "_") == und);
            let same_file = file_stem(&r.path) == file_stem(&shim.path);
            // `rust_cpuid_processor_stepping` calling
            // `ProcessorId::new(r).stepping()` forwards to `stepping`, the
            // end of the chain the shim is named for, not to the receiver's
            // constructor; the type path only settles same-named methods.
            let score = (exact as usize) * 8000
                + (und.ends_with(&b) as usize) * 4000
                + (by_path as usize) * 2000
                + (und.contains(&k) as usize) * 500
                + (same_file as usize) * 100
                + k.len();
            (score, r)
        })
        .collect();
    let Some(best) = scored.iter().map(|(s, _)| *s).max() else {
        return Target::None;
    };
    let top: Vec<&Function> = scored
        .iter()
        .filter(|(s, _)| *s == best)
        .map(|(_, r)| *r)
        .collect();
    match top.as_slice() {
        [one] => Target::One(Box::new((*one).clone())),
        many => Target::Ambiguous(many.iter().map(|r| r.name.clone()).collect()),
    }
}

/// The last component of a class path, normalized.
fn class_key(f: &Function) -> Option<String> {
    let c = f.class.as_deref()?;
    Some(normalize::ident(c.rsplit("::").next().unwrap_or(c)))
}

/// The C++ class hierarchy of the change, by normalized class name.
struct Hierarchy {
    bases: HashMap<String, Vec<String>>,
}

impl Hierarchy {
    fn new(raw: &crate::cpp::ClassBases) -> Hierarchy {
        let bases = raw
            .iter()
            .map(|(k, v)| {
                (
                    normalize::ident(k),
                    v.iter().map(|b| normalize::ident(b)).collect(),
                )
            })
            .collect();
        Hierarchy { bases }
    }

    fn direct(&self, class: &str) -> &[String] {
        self.bases.get(class).map_or(&[], Vec::as_slice)
    }

    fn ancestors(&self, class: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut todo = vec![class.to_string()];
        while let Some(c) = todo.pop() {
            for b in self.direct(&c) {
                if !out.contains(b) && b != class {
                    out.push(b.clone());
                    todo.push(b.clone());
                }
            }
        }
        out
    }

    fn is_ancestor(&self, a: &str, of: &str) -> bool {
        self.ancestors(of).iter().any(|x| x == a)
    }

    /// One class derives from the other, or both derive directly from the
    /// same class.
    fn related(&self, a: &str, b: &str) -> bool {
        a == b
            || self.is_ancestor(a, b)
            || self.is_ancestor(b, a)
            || self.direct(a).iter().any(|x| self.direct(b).contains(x))
    }
}

/// The name a function goes by for matching overrides: constructors are
/// `new` and destructors `drop`, like their Rust counterparts.
fn override_key(f: &Function) -> String {
    name_words(f).join("_")
}

/// A pair with at least this many unsafe blocks gets a note.
const UNSAFE_PAIR_MIN: usize = 3;

/// Runs the analysis.
pub fn analyze(inputs: Inputs, opts: &Options, finder: &mut dyn CppFinder) -> Report {
    let Inputs {
        cpp,
        rust,
        all_rust,
        cpp_new_calls,
        forced,
        cpp_bases,
        cpp_changes,
        cpp_helpers,
        cpp_changed_paths,
        cpp_macros,
        values,
    } = inputs;
    let mut cpp = cpp;
    mark_comments_in_untouched_cpp(&mut cpp, &cpp_changed_paths, finder);
    let mut report = Report {
        cpp_changes,
        ..Report::default()
    };
    let hierarchy = Hierarchy::new(&cpp_bases);

    // Resolve FFI shims among the Rust functions.
    let mut universe: Vec<Function> = all_rust;
    for r in &rust {
        if !universe
            .iter()
            .any(|u| u.path == r.path && u.start_line == r.start_line)
        {
            universe.push(r.clone());
        }
    }
    let shims: Vec<(Function, Target)> = universe
        .iter()
        .filter(|f| f.is_ffi)
        .map(|s| (s.clone(), shim_target(s, &universe)))
        .collect();

    // Functions a Rust function calls, where C++ it doesn't carry itself
    // may have gone: `cpp_*` helpers the change added, and other Rust.
    let universe_ref = &universe;
    let helpers_ref = &cpp_helpers;
    // Functions passed by name (`sync_exec(mask, invalidate_task, ..)`) run
    // on the caller's behalf too.
    let reached = |r: &Function| -> Vec<&Function> {
        let named: HashSet<&str> = r
            .lines
            .iter()
            .flat_map(|l| l.split(|c: char| !c.is_alphanumeric() && c != '_'))
            .collect();
        universe_ref
            .iter()
            .filter(|u| {
                u.name != r.name
                    && (!u.is_ffi && calls_function(r, u)
                        // An `extern "C"` callback handed to C++ by name.
                        || named.contains(u.base.as_str()) && !calls_function(r, u))
            })
            .collect()
    };
    // Two levels deep: a helper that hands its work to a callback.
    let elsewhere = |r: &Function| -> Vec<&Function> {
        let mut out: Vec<&Function> = helpers_ref
            .iter()
            .filter(|h| r.calls.contains(&normalize::ident(&h.base)))
            .collect();
        let first = reached(r);
        for f in &first {
            for g in reached(f) {
                if g.name != r.name && !out.iter().chain(&first).any(|x| std::ptr::eq(*x, g)) {
                    out.push(g);
                }
            }
        }
        out.extend(first);
        out
    };
    let mut cpp_used = vec![false; cpp.len()];
    let touched_rust: HashSet<(String, usize)> = rust
        .iter()
        .map(|r| (r.path.clone(), r.start_line))
        .collect();
    let is_touched = |r: &Function| touched_rust.contains(&(r.path.clone(), r.start_line));
    let mut rust_pool: Vec<Function> = rust;
    for (s, _) in &shims {
        if !rust_pool
            .iter()
            .any(|r| r.path == s.path && r.start_line == s.start_line)
        {
            rust_pool.push(s.clone());
        }
    }
    // Shim targets may live in files the change did not touch much; make
    // sure they are pairable.
    for (_, t) in &shims {
        if let Target::One(t) = t {
            if !rust_pool
                .iter()
                .any(|r| r.path == t.path && r.start_line == t.start_line)
            {
                rust_pool.push((**t).clone());
            }
        }
    }
    let mut rust_used = vec![false; rust_pool.len()];
    let find_rust = |pool: &[Function], f: &Function| {
        pool.iter()
            .position(|r| r.path == f.path && r.start_line == f.start_line)
    };

    // Types that exist on each side, to tell a port of `A::f` from a
    // same-named method of another class.
    let cpp_classes: Vec<String> = cpp
        .iter()
        .filter_map(class_key)
        .chain(hierarchy.bases.keys().cloned())
        .collect();
    let rust_classes: Vec<String> = universe.iter().filter_map(class_key).collect();

    let mut chosen: Vec<(usize, usize, Link, String)> = Vec::new();
    let mut forwarders: Vec<(usize, usize)> = Vec::new();

    // 1. Explicit pairs.
    for (c, r) in &forced {
        let ci = cpp.iter().position(|f| &f.name == c || &f.base == c);
        let ri = rust_pool.iter().position(|f| &f.name == r || &f.base == r);
        if let (Some(ci), Some(ri)) = (ci, ri) {
            if !cpp_used[ci] && !rust_used[ri] {
                cpp_used[ci] = true;
                rust_used[ri] = true;
                chosen.push((ci, ri, Link::Forced, "requested with --pair".into()));
            }
        }
    }

    // 2. FFI links: the new C++ body calls the shim, or the shim is named
    //    after the C++ function.
    for orig in 0..cpp.len() {
        if cpp_used[orig] {
            continue;
        }
        let c = &cpp[orig];
        let new_calls = cpp_new_calls.get(&c.name);
        let qualified = normalize::ident(&format!(
            "{}_{}",
            c.class
                .as_deref()
                .map(|k| k.rsplit("::").next().unwrap_or(k))
                .unwrap_or(""),
            operator_alias(&c.base)
        ));
        for (shim, target) in &shims {
            let shim_name = normalize::ident(&shim.base);
            let by_call = new_calls.is_some_and(|calls| calls.contains(&shim_name));
            let by_name = !by_call && undecorated(&shim.base) == qualified;
            if !(by_call || by_name) {
                continue;
            }
            // A Rust method with the C++ method's own class and name is its
            // port, rather than whatever part the new C++ still hands to
            // Rust (`VmxPage::alloc` over `validate_vmx_info`).
            if by_call {
                let twin = rust_pool.iter().enumerate().any(|(k, r)| {
                    !rust_used[k]
                        && !r.is_ffi
                        && !r.test_only
                        && class_key(r).is_some()
                        && class_key(r) == class_key(c)
                        && name_words(r) == name_words(c)
                });
                let target_is_twin = matches!(target, Target::One(t)
                    if class_key(t) == class_key(c) && name_words(t) == name_words(c));
                if twin && !target_is_twin {
                    continue;
                }
            }
            // An exported Rust function that does the work itself, rather
            // than forwarding, is the port. One that only passes its
            // arguments on stands in for the function it calls.
            let target = match target {
                Target::One(t) if !passes_through(shim, t) && score(c, shim).0 > score(c, t).0 => {
                    shim
                }
                Target::One(t) if stub_of(c, t) => shim,
                Target::One(t) => &**t,
                Target::None => shim,
                Target::Ambiguous(_) if body_len(shim) > 3 => shim,
                // Leave it to the similarity pass rather than guess.
                Target::Ambiguous(_) => continue,
            };
            let Some(ri) = find_rust(&rust_pool, target) else {
                continue;
            };
            if rust_used[ri] {
                continue;
            }
            // A C++ trampoline (`mask_interrupt(v)` calling
            // `manager.MaskInterrupt(v)`) shares the shim's name, but the
            // C++ it calls holds the body the Rust ports.
            let callee = (body_len(c) <= 2)
                .then(|| {
                    cpp.iter().enumerate().find(|(k, d)| {
                        *k != orig
                            && !cpp_used[*k]
                            && body_len(d) > body_len(c)
                            && c.calls.contains(&normalize::ident(&d.base))
                            && score(d, target).0 > score(&cpp[orig], target).0
                    })
                })
                .flatten()
                .map(|(k, _)| k);
            let ci = callee.unwrap_or(orig);
            let c = &cpp[ci];
            // Nothing in common at all: the name led astray.
            if body_len(c) > 2 && body_len(target) > 2 && score(c, target).0 < 0.05 {
                continue;
            }
            let (link, why) = if by_call {
                (
                    Link::Ffi {
                        shim: shim.name.clone(),
                        shim_location: shim.location(),
                    },
                    format!("the new C++ body calls FFI shim {}", shim.name),
                )
            } else {
                (
                    Link::FfiName {
                        shim: shim.name.clone(),
                        shim_location: shim.location(),
                    },
                    format!("FFI shim {} is named after the C++ function", shim.name),
                )
            };
            let why = match callee {
                Some(_) => format!("{why}, through the C++ trampoline {}", cpp[orig].name),
                None => why,
            };
            if callee.is_some() {
                cpp_used[orig] = true;
            }
            cpp_used[ci] = true;
            rust_used[ri] = true;
            chosen.push((ci, ri, link, why));
            break;
        }
    }

    // 3. Same class and method name, where exactly one of each exists: a
    //    name match that close beats body similarity.
    for ci in 0..cpp.len() {
        if cpp_used[ci] || is_cpp_shim(&cpp[ci]) {
            continue;
        }
        let c = &cpp[ci];
        let exact = |r: &Function| {
            !r.is_ffi
                && (!r.test_only || cpp_is_test(c))
                && !stub_of(c, r)
                && class_key(r) == class_key(c)
                && name_words(r) == name_words(c)
        };
        let mut hits: Vec<usize> = (0..rust_pool.len())
            .filter(|&ri| !rust_used[ri] && is_touched(&rust_pool[ri]) && exact(&rust_pool[ri]))
            .collect();
        // A trait impl's `fn read(&self) { Self::read(self) }` forwards to
        // the inherent method that holds the body.
        if hits.len() > 1 {
            let forwarding: Vec<usize> = hits
                .iter()
                .copied()
                .filter(|&a| {
                    hits.iter().any(|&b| {
                        a != b
                            && body_len(&rust_pool[a]) <= 1
                            && calls_function(&rust_pool[a], &rust_pool[b])
                    })
                })
                .collect();
            if forwarding.len() < hits.len() {
                hits.retain(|h| !forwarding.contains(h));
            }
        }
        let rivals = cpp
            .iter()
            .enumerate()
            .filter(|(k, d)| {
                *k != ci
                    && !cpp_used[*k]
                    && hits.iter().any(|&ri| {
                        class_key(d) == class_key(&rust_pool[ri])
                            && name_words(d) == name_words(&rust_pool[ri])
                    })
            })
            .count();
        if let ([ri], 0) = (hits.as_slice(), rivals) {
            cpp_used[ci] = true;
            rust_used[*ri] = true;
            chosen.push((
                ci,
                *ri,
                Link::Similarity,
                "same class and method name".into(),
            ));
        }
    }

    // 4. Similarity, greedily from the best score down.
    // C++ that is still there after the change, and doesn't call into
    // Rust, was moved or edited within C++ (say from mmu.cc into a header),
    // not ported. Only Rust named for it and close in body (a copy that
    // duplicates it, `aspace_unmap` for `Unmap`) can claim it.
    let stays = |f: &Function| {
        cpp_new_calls
            .get(&f.name)
            .is_some_and(|calls| !calls.iter().any(|c| c.starts_with("rust_")))
    };
    let mut cands: Vec<(f64, usize, usize)> = Vec::new();
    for (ci, c) in cpp.iter().enumerate() {
        if cpp_used[ci] {
            continue;
        }
        let kept = stays(c);
        let twin = has_twin(c, &universe);
        for (ri, r) in rust_pool.iter().enumerate() {
            if rust_used[ri] || r.is_ffi || !is_touched(r) || (r.test_only && !cpp_is_test(c)) {
                continue;
            }
            if twin && class_key(r) != class_key(c) {
                continue;
            }
            if !class_compatible(c, r, &cpp_classes, &rust_classes, &hierarchy) {
                continue;
            }
            // A C++ method that calls a free function named like `r` is a
            // caller of what `r` ports, not its source.
            if r.class.is_none()
                && c.class.is_some()
                && c.calls.contains(&normalize::ident(&r.base))
            {
                continue;
            }
            let (s, _) = score(c, r);
            if kept && !(s >= 0.6 && name_words(c).iter().all(|w| name_words(r).contains(w))) {
                continue;
            }
            // Bodies alike by accident: nothing in the names in common, and
            // not alike enough to say otherwise.
            if s < 0.6 && !shares_name_word(c, r) {
                continue;
            }
            if s >= opts.min_score && plausible(c, r) {
                // Among equals, prefer the C++ from the same directory
                // (`arch/riscv64` over `arch/arm64`), and the overload whose
                // parameter type the Rust name spells (`read_16` for
                // `Read(VmcsField16)`).
                let s = s + 0.01 * path_affinity(&c.path, &r.path) + 0.1 * suffix_fit(c, r);
                cands.push((s, ci, ri));
            }
        }
    }
    cands.sort_by(|a, b| b.0.total_cmp(&a.0));
    for (s, ci, ri) in cands {
        if !cpp_used[ci] && !rust_used[ri] {
            cpp_used[ci] = true;
            rust_used[ri] = true;
            chosen.push((
                ci,
                ri,
                Link::Similarity,
                format!("names and bodies are similar (score {s:.2})"),
            ));
        }
    }

    // A thin Rust method that just calls the method doing the work (say
    // `Widget::dump` calling `WidgetState::dump`) stands in
    // for the C++ only by name; compare against the method doing the work.
    for (ci, ri, _, _) in &mut chosen {
        if let Some(rj) = forwardee(&cpp[*ci], &rust_pool[*ri], &rust_pool, &rust_used) {
            rust_used[rj] = true;
            forwarders.push((rj, *ri));
            *ri = rj;
        }
    }

    // 5. Overrides: a C++ override of a method whose base or sibling version
    //    is paired, in a related class, is folded into the same Rust
    //    function (an enum and `match` replacing virtual dispatch).
    let mut overrides: HashMap<usize, Vec<usize>> = HashMap::new();
    for (ci, c) in cpp.iter().enumerate() {
        if cpp_used[ci] || is_cpp_shim(c) {
            continue;
        }
        let Some(ck) = class_key(c) else { continue };
        let key = override_key(c);
        let host = chosen.iter().position(|(pi, _, _, _)| {
            let p = &cpp[*pi];
            override_key(p) == key && class_key(p).is_some_and(|pk| hierarchy.related(&pk, &ck))
        });
        if let Some(h) = host {
            let (pi, ri, _, _) = &chosen[h];
            let rust_fn = &rust_pool[*ri];
            // Only when the Rust carries something of the override, or the
            // override is too small to say.
            let claimed = override_rows(&cpp[*pi], rust_fn, &[c]);
            let matched = claimed[0]
                .iter()
                .filter(|p| {
                    p.cpp.is_some_and(|i| {
                        !matches!(c.units[i].kind, UnitKind::Signature | UnitKind::Comment)
                    }) && p.rust.is_some()
                })
                .count();
            if matched > 0 || body_len(c) <= 1 {
                cpp_used[ci] = true;
                overrides.entry(h).or_default().push(ci);
            }
        }
    }

    for (h, (ci, ri, link, why)) in chosen.into_iter().enumerate() {
        let ovs: Vec<Function> = overrides
            .get(&h)
            .map(|v| v.iter().map(|&k| cpp[k].clone()).collect())
            .unwrap_or_default();
        let mut pair = build_pair(
            cpp[ci].clone(),
            rust_pool[ri].clone(),
            link,
            CppOrigin::Changed,
            ovs,
            &elsewhere(&rust_pool[ri]),
            &values,
        );
        pair.rationale = why;
        pair.forwarder = forwarders
            .iter()
            .find(|(t, _)| *t == ri)
            .map(|(_, f)| rust_pool[*f].clone());
        report.pairs.push(pair);
    }

    // 6. Rust with no C++ in the change: look for C++ the change left alone.
    let in_change = |c: &Function| cpp.iter().any(|x| x.path == c.path && x.name == c.name);
    // The best C++ for each Rust function, then assigned from the best
    // score down, so that `encoded_addr` doesn't take `Item::addr` from
    // `PendingTlbItem::addr` just by coming first.
    let mut cands6: Vec<(bool, f64, usize, Function)> = Vec::new();
    for (ri, r) in rust_pool.iter().enumerate() {
        if rust_used[ri] || r.is_ffi || !is_touched(r) {
            continue;
        }
        // A thin Rust wrapper that calls back into C++ wraps that C++; it
        // doesn't port it.
        if body_len(r) <= 3 && r.calls.iter().any(|c| c.starts_with("cpp_")) {
            continue;
        }
        let mut ranked: Vec<(bool, f64, Function)> = finder
            .find(r)
            .into_iter()
            .filter(|c| !in_change(c) && !asks_instead(c, r))
            // A C++ `cpp_*` FFI helper is ported only by a Rust `cpp_*`
            // function of the same name.
            .filter(|c| !is_cpp_shim(c) || c.base == r.base)
            .filter(|c| !stub_of(c, r) && (!r.test_only || cpp_is_test(c)))
            .filter(|c| expected_place(c, r))
            // A Rust function with the C++ function's own class and name
            // exists elsewhere: that one is its port.
            .filter(|c| !twin_elsewhere(c, r, &universe))
            .map(|c| {
                let ok = unchanged_class_ok(&c, r, &hierarchy);
                (ok, score(&c, r).0, c)
            })
            .filter(|(_, s, c)| {
                *s >= opts.min_unchanged_score && (*s >= 0.6 || shares_name_word(c, r))
            })
            .collect();
        ranked.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.total_cmp(&a.1)));
        let Some((ok, s, c)) = ranked.first().cloned() else {
            continue;
        };
        // Two equally good candidates in unrelated classes: don't guess.
        let tie = ranked.get(1).is_some_and(|(ok2, s2, c2)| {
            *ok2 == ok && (s - s2).abs() < 0.02 && class_key(c2) != class_key(&c)
        });
        if tie {
            continue;
        }
        cands6.extend(ranked.into_iter().map(|(ok, s, c)| (ok, s, ri, c)));
    }
    cands6.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.total_cmp(&a.1)));
    let mut found_used: Vec<(String, usize)> = Vec::new();
    let mut assigned: Vec<(usize, Function)> = Vec::new();
    for (_, _, ri, c) in cands6 {
        if rust_used[ri] || found_used.contains(&(c.path.clone(), c.start_line)) {
            continue;
        }
        rust_used[ri] = true;
        found_used.push((c.path.clone(), c.start_line));
        assigned.push((ri, c));
    }
    assigned.sort_by_key(|(ri, _)| *ri);
    for (ri, c) in assigned {
        let r = &rust_pool[ri];
        // The change left this C++ alone, so its comments are all still
        // there.
        let mut c = c;
        for u in &mut c.units {
            if u.kind == UnitKind::Comment {
                u.features.still_in_cpp = true;
            }
        }
        let mut pair = build_pair(
            c,
            r.clone(),
            Link::Similarity,
            CppOrigin::Unchanged,
            Vec::new(),
            &elsewhere(r),
            &values,
        );
        pair.rationale = format!(
            "found in the repository, not in the change (score {:.2})",
            pair.score
        );
        // A C++ test the change left alone still runs; a Rust test that
        // differs from it is new coverage, not a lost check.
        let test = pair.cpp.path.contains("test")
            || pair
                .cpp
                .lines
                .iter()
                .any(|l| l.contains("UNITTEST") || l.contains("BEGIN_TEST"));
        if test {
            for row in &mut pair.rows {
                for n in &mut row.notes {
                    n.severity = Severity::Note;
                }
            }
            let kept: Vec<Finding> = pair
                .findings
                .iter()
                .filter(|f| f.cpp_line.is_none() && f.rust_line.is_none())
                .cloned()
                .collect();
            let refreshed = check::refresh(&mut pair.rows, &pair.cpp, &pair.rust);
            pair.findings = kept.into_iter().chain(refreshed).collect();
            pair.rationale.push_str("; the C++ test still runs");
        }
        report.pairs.push(pair);
    }

    // 7. C++ with no Rust in the change: the Rust may already exist, in a
    //    file the change touched elsewhere.
    for ci in 0..cpp.len() {
        if cpp_used[ci] || is_cpp_shim(&cpp[ci]) {
            continue;
        }
        let c = &cpp[ci];
        let hits: Vec<&Function> = universe
            .iter()
            .filter(|r| !r.is_ffi && class_key(r) == class_key(c) && name_words(r) == name_words(c))
            .filter(|r| !layout::arch_conflict(&c.path, &r.path))
            .filter(|r| find_rust(&rust_pool, r).is_none_or(|ri| !rust_used[ri]))
            .collect();
        let [r] = hits.as_slice() else { continue };
        let (s, _) = score(c, r);
        if s < opts.min_unchanged_score {
            continue;
        }
        let r = (*r).clone();
        match find_rust(&rust_pool, &r) {
            Some(ri) => rust_used[ri] = true,
            None => {
                rust_pool.push(r.clone());
                rust_used.push(true);
            }
        }
        cpp_used[ci] = true;
        let ew = elsewhere(&r);
        let mut pair = build_pair(
            c.clone(),
            r,
            Link::Similarity,
            CppOrigin::Changed,
            Vec::new(),
            &ew,
            &values,
        );
        pair.rationale = format!(
            "same name; the change left this Rust function alone (score {:.2})",
            pair.score
        );
        report.pairs.push(pair);
    }

    // 8. A second Rust definition of a function already paired: the change
    //    defines it twice (in two modules), and callers may use either
    //    copy, so each is compared with the C++. A thin copy that only
    //    forwards to the other is a wrapper, not a duplicate; when the pair
    //    holds the wrapper, the copy doing the work takes its place.
    let cfg_gated = |f: &Function| {
        f.lines
            .iter()
            .take_while(|l| !l.contains("fn "))
            .any(|l| l.trim_start().starts_with("#[cfg"))
    };
    let mut extra = Vec::new();
    for ri in 0..rust_pool.len() {
        let r = rust_pool[ri].clone();
        if rust_used[ri] || cfg_gated(&r) {
            continue;
        }
        let Some(pi) = report.pairs.iter().position(|p| {
            p.rust.path != r.path
                && p.rust.base == r.base
                && class_key(&p.rust) == class_key(&r)
                && !cfg_gated(&p.rust)
        }) else {
            continue;
        };
        let p = &report.pairs[pi];
        if forwards_to(&r, &p.rust, &universe) {
            continue;
        }
        if forwards_to(&p.rust, &r, &universe) {
            if score(&p.cpp, &r).0 >= opts.min_score {
                rust_used[ri] = true;
                let ew = elsewhere(&r);
                let mut pair = build_pair(
                    p.cpp.clone(),
                    r,
                    p.link.clone(),
                    p.origin.clone(),
                    Vec::new(),
                    &ew,
                    &values,
                );
                pair.rationale = format!(
                    "{}; {} only forwards to this function",
                    p.rationale, p.rust.name
                );
                pair.forwarder = Some(p.rust.clone());
                report.pairs[pi] = pair;
            }
            continue;
        }
        if !is_touched(&r) {
            continue;
        }
        let (s, _) = score(&p.cpp, &r);
        if s < opts.min_score {
            continue;
        }
        rust_used[ri] = true;
        let ew = elsewhere(&r);
        let mut pair = build_pair(
            p.cpp.clone(),
            r.clone(),
            Link::Similarity,
            p.origin.clone(),
            Vec::new(),
            &ew,
            &values,
        );
        let other = format!("{}:{}", p.rust.path, p.rust.start_line);
        pair.rationale = format!(
            "a second Rust definition of {}; the first is at {other} (score {:.2})",
            r.name, pair.score
        );
        pair.duplicate_of = Some((p.rust.path.clone(), p.rust.start_line));
        let same = body_code(&r) == body_code(&p.rust);
        pair.findings.insert(
            0,
            Finding {
                severity: if same { Severity::Note } else { Severity::Issue },
                category: check::Category::Pairing,
                cpp_line: None,
                rust_line: Some(r.start_line),
                cpp_file: None,
                message: if same {
                    format!("the change defines {} twice, here and at {other}; the copies are the same", r.name)
                } else {
                    format!(
                        "the change defines {} twice, here and at {other}, and the copies differ; both are compared with the C++, and callers may reach either",
                        r.name
                    )
                },
            },
        );
        extra.push(pair);
    }
    report.pairs.extend(extra);

    structure_checks(&mut report.pairs, &cpp, &cpp_macros, &universe);
    for p in &mut report.pairs {
        let n = crate::lint::unsafe_blocks(&p.rust);
        if n >= UNSAFE_PAIR_MIN {
            p.findings.push(Finding {
                severity: Severity::Note,
                category: check::Category::Unsafe,
                cpp_line: None,
                rust_line: Some(p.rust.start_line),
                cpp_file: None,
                message: format!(
                    "Rust adds {n} unsafe blocks where the C++ needs none; a safe facade for what they reach into would keep it closer to the C++"
                ),
            });
        }
    }

    report.pairs.sort_by(|a, b| {
        (a.cpp.path.as_str(), a.cpp.start_line).cmp(&(b.cpp.path.as_str(), b.cpp.start_line))
    });
    let (removed_shims, unmatched): (Vec<Function>, Vec<Function>) = cpp
        .into_iter()
        .zip(cpp_used)
        .filter(|(_, u)| !u)
        .map(|(f, _)| f)
        .partition(is_cpp_shim);
    // A C++ function that is still there after the change, and doesn't call
    // into Rust, was edited rather than ported; its edits are listed with
    // the other C++ changes.
    let stays = |f: &Function| {
        cpp_new_calls
            .get(&f.name)
            .is_some_and(|calls| !calls.iter().any(|c| c.starts_with("rust_")))
    };
    report.unmatched_cpp = unmatched.into_iter().filter(|f| !stays(f)).collect();
    report.removed_cpp_shims = removed_shims;
    report.unmatched_rust = rust_pool
        .into_iter()
        .zip(rust_used)
        .filter(|(f, u)| !u && !f.is_ffi && is_touched(f))
        .map(|(f, _)| f)
        .collect();
    let (facades, unmatched): (Vec<Function>, Vec<Function>) =
        std::mem::take(&mut report.unmatched_rust)
            .into_iter()
            .partition(|f| body_len(f) <= 3 && f.calls.iter().any(|c| c.starts_with("cpp_")));
    let (tests, unmatched): (Vec<Function>, Vec<Function>) =
        unmatched.into_iter().partition(is_rust_test);
    report.unmatched_rust = unmatched;
    report.rust_tests = tests;
    // Helpers of paired functions first, so they read as part of the port.
    let helper: Vec<bool> = report
        .unmatched_rust
        .iter()
        .map(|f| !report.callers(f).is_empty())
        .collect();
    let mut tagged: Vec<(bool, Function)> = helper
        .into_iter()
        .zip(std::mem::take(&mut report.unmatched_rust))
        .collect();
    tagged.sort_by_key(|(h, _)| !*h);
    report.unmatched_rust = tagged.into_iter().map(|(_, f)| f).collect();
    report.rust_facades = facades
        .into_iter()
        .map(|f| {
            let callee = f
                .calls
                .iter()
                .find(|c| c.starts_with("cpp_"))
                .cloned()
                .unwrap_or_default();
            (f, callee)
        })
        .collect();
    let used_shims: HashSet<String> = report
        .pairs
        .iter()
        .filter_map(|p| match &p.link {
            Link::Ffi { shim, .. } | Link::FfiName { shim, .. } => Some(shim.clone()),
            _ => None,
        })
        .collect();
    report.shims = shims
        .into_iter()
        .filter(|(shim, target)| {
            is_touched(shim)
                || used_shims.contains(&shim.name)
                || matches!(target, Target::One(t) if is_touched(t))
        })
        .map(|(shim, target)| match target {
            Target::One(t) => Shim {
                shim,
                target: Some(t.name),
                ambiguous: Vec::new(),
            },
            Target::Ambiguous(names) => Shim {
                shim,
                target: None,
                ambiguous: names,
            },
            Target::None => Shim {
                shim,
                target: None,
                ambiguous: Vec::new(),
            },
        })
        .collect();
    report
}

/// Checks that the Rust keeps the C++'s structure, which needs every pair:
/// - a C++ function-like macro expanded inline instead of kept as a macro;
/// - a C++ helper (a function the change converts, or a local lambda)
///   called at several places that the Rust never calls, so its code is
///   repeated inline;
/// - a converted C++ helper called once whose port the Rust doesn't call.
fn structure_checks(
    pairs: &mut [PairReport],
    cpp: &[Function],
    macros: &[String],
    universe: &[Function],
) {
    static LAMBDA: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\bauto\s+([A-Za-z_]\w*)\s*=\s*(?:\[|$)").unwrap()
    });
    let key = |base: &str| normalize::call(base).unwrap_or_else(|| normalize::ident(base));
    let macros: Vec<(String, String)> = macros.iter().map(|m| (key(m), m.clone())).collect();
    // Converted C++ functions, by call name, with where their ports are.
    let mut ports: HashMap<String, Vec<(Option<String>, String, String)>> = HashMap::new();
    // Their classes: a call `Create()` in a method of `Foo` is taken to mean
    // a free function or `Foo::Create`, not another class's `Create`.
    // Only helpers in the same C++ file count: a local helper the Rust
    // stops using is a change of structure, while code from elsewhere may
    // have its own Rust route.
    let mut classes: HashMap<String, Vec<(Option<String>, String)>> = HashMap::new();
    for f in cpp {
        ports.entry(key(&f.base)).or_default();
        classes
            .entry(key(&f.base))
            .or_default()
            .push((class_key(f), f.path.clone()));
    }
    for p in pairs.iter() {
        ports.entry(key(&p.cpp.base)).or_default().push((
            class_key(&p.cpp),
            key(&p.rust.base),
            format!("{}:{}", file_name(&p.rust.path), p.rust.start_line),
        ));
    }
    // A call to a class's name constructs it; it is not a helper.
    let class_names: HashSet<String> = cpp.iter().filter_map(class_key).collect();
    // Rust functions by call name, to follow the Rust through a wrapper to
    // a helper's port.
    let mut by_name: HashMap<String, Vec<&Function>> = HashMap::new();
    for f in universe {
        by_name.entry(key(&f.base)).or_default().push(f);
    }
    for p in pairs.iter_mut() {
        // A Rust function the change left alone didn't inline anything.
        if p.rationale.contains("left this Rust function alone") {
            continue;
        }
        let own = key(&p.cpp.base);
        // What the Rust calls, directly or through up to two other Rust
        // functions.
        let mut reach: Vec<String> = p.rust.calls.clone();
        let mut frontier: Vec<String> = p.rust.calls.clone();
        for _ in 0..2 {
            let mut next = Vec::new();
            for c in &frontier {
                for f in by_name.get(c).into_iter().flatten() {
                    for d in &f.calls {
                        if !reach.contains(d) {
                            reach.push(d.clone());
                            next.push(d.clone());
                        }
                    }
                }
            }
            frontier = next;
        }
        let rust_calls = |h: &str| reach.iter().any(|d| check::call_like(h, d));
        let rust_text = normalize::words(&p.rust.lines.join(" "));
        let lambdas: Vec<String> = p
            .cpp
            .lines
            .iter()
            .filter_map(|l| LAMBDA.captures(l).map(|c| key(&c[1])))
            .collect();
        let mut notes: Vec<(usize, check::Note)> = Vec::new();
        let mut demoted: Vec<(usize, String)> = Vec::new();
        // Macros.
        for (m, raw) in &macros {
            // A macro with a function port (`WRITE_PERCPU_FIELD` as
            // `write_percpu_u32`), a compiler intrinsic (`__wfi`), or a
            // shorthand the function defines for itself.
            let words = normalize::words(raw);
            let fn_port = words.len() >= 2 && {
                // Every word but a generic last one (`FIELD`), or all of
                // them: `riscv64_csr_read` is not a port of
                // `RISCV64_CSR_CLEAR`.
                let (last, head) = words.split_last().unwrap();
                let generic = matches!(last.as_str(), "field" | "value" | "var" | "member" | "reg");
                p.rust.calls.iter().any(|d| {
                    let dw = normalize::words(d);
                    head.iter().all(|w| dw.contains(w)) && (generic || dw.contains(last))
                })
            };
            let local = p
                .cpp
                .lines
                .iter()
                .any(|l| l.trim_start().starts_with("#") && l.contains(raw.as_str()));
            let direct = p.rust.calls.iter().any(|d| check::call_like(m, d));
            if direct || fn_port || local || raw.starts_with("__") {
                continue;
            }
            // Only a macro used as a statement of its own, like a function
            // call; one inside an expression may well be a Rust `const`.
            let uses: Vec<usize> = (0..p.rows.len())
                .filter(|&k| {
                    p.rows[k].cpp.is_some_and(|i| {
                        let u = &p.cpp.units[i];
                        u.kind == UnitKind::Stmt
                            && u.features.calls.contains(m)
                            && p.cpp
                                .line(u.start_line)
                                .trim_start()
                                .starts_with(raw.as_str())
                    })
                })
                .collect();
            let Some(&first) = uses.first() else { continue };
            let why = format!("the C++ macro {raw} is expanded inline");
            notes.push((
                first,
                check::Note::new(
                    Severity::Issue,
                    check::Category::Call,
                    match by_name.get(m).and_then(|v| v.first()) {
                        Some(f) => format!(
                            "C++ macro {raw} is expanded inline in the Rust, though Rust has {} ({}:{}); call it where the C++ uses the macro",
                            f.base,
                            file_name(&f.path),
                            f.start_line
                        ),
                        None => format!("C++ macro {raw} is expanded inline in the Rust; keep it as a macro (macro_rules!) and use it where the C++ does"),
                    },
                ),
            ));
            demoted.extend(uses.iter().map(|&k| (k, why.clone())));
        }
        // Helpers.
        let mut helpers: Vec<String> = Vec::new();
        for u in &p.cpp.units {
            for c in &u.features.calls {
                let mine = classes.get(c).is_some_and(|v| {
                    v.iter().any(|(k, path)| {
                        (k.is_none() || *k == class_key(&p.cpp)) && *path == p.cpp.path
                    })
                });
                let noise =
                    matches!(c.as_str(), "assert" | "trace" | "print") || class_names.contains(c);
                let known = !noise && (lambdas.contains(c) || mine);
                if known && *c != own && !helpers.contains(c) && !macros.iter().any(|(m, _)| m == c)
                {
                    helpers.push(c.clone());
                }
            }
        }
        for h in helpers {
            let cls = class_key(&p.cpp);
            let ported: Vec<&(Option<String>, String, String)> = ports
                .get(&h)
                .map(|v| {
                    v.iter()
                        .filter(|(k, _, _)| k.is_none() || *k == cls)
                        .collect()
                })
                .unwrap_or_default();
            if rust_calls(&h) || ported.iter().any(|(_, r, _)| rust_calls(r)) {
                continue;
            }
            // Calls on another object (`a.Update()`, `b.Update()`) are not a
            // local helper; nor is a call inside a trace statement, which the
            // Rust may drop with the trace.
            let receiver = |line: &str| {
                let re = regex::Regex::new(&format!(
                    r"(\w+)\s*(?:\.|->)\s*\w*\b{}\s*\(",
                    regex::escape(&h)
                ));
                re.ok()
                    .and_then(|re| re.captures(line).map(|c| c[1].to_string()))
                    .filter(|r| r != "this")
            };
            let uses: Vec<usize> = (0..p.rows.len())
                .filter(|&k| {
                    p.rows[k].cpp.is_some_and(|i| {
                        let u = &p.cpp.units[i];
                        u.kind != UnitKind::Signature && u.features.calls.contains(&h)
                        // The lambda's own definition.
                        && !LAMBDA.is_match(p.cpp.line(u.start_line))
                        && !u.features.calls.iter().any(|c| c == "trace" || c == "print")
                    })
                })
                .collect();
            let on_objects = uses.iter().any(|&k| {
                p.rows[k]
                    .cpp
                    .is_some_and(|i| receiver(p.cpp.line(p.cpp.units[i].start_line)).is_some())
            });
            if on_objects {
                continue;
            }
            // The Rust has the helper's code in its place only if it does
            // what the helper does: makes its calls, or names most of what
            // it names. Otherwise the helper's work was done another way.
            let body = helper_body(&h, &p.cpp, cpp);
            let Some(body) = body else { continue };
            let copied = copied_into(&body, &reach, &rust_text);
            if copied.is_none() {
                continue;
            }
            let lines: Vec<String> = uses
                .iter()
                .filter_map(|&k| p.rows[k].cpp.map(|i| p.cpp.units[i].start_line.to_string()))
                .collect();
            match uses.as_slice() {
                [] => {}
                // Where the Rust has a statement in its place; a C++ line
                // with no Rust counterpart is reported as such already.
                [only] if !ported.is_empty() && p.rows[*only].rust.is_some() => {
                    let (_, _, at) = ported[0];
                    notes.push((
                        *only,
                        check::Note::new(
                            Severity::Issue,
                            check::Category::Call,
                            format!("C++ calls {h}, which the change ports to Rust ({at}); the Rust here doesn't call it, so its code was inlined or dropped"),
                        ),
                    ));
                }
                [_] => {}
                [first, ..] => {
                    let why = format!("the C++ helper {h} is inlined");
                    notes.push((
                        *first,
                        check::Note::new(
                            Severity::Issue,
                            check::Category::Call,
                            format!(
                                "C++ calls the helper {h} at {} places (lines {}); the Rust never calls it{}, so its code is repeated inline",
                                uses.len(),
                                lines.join(", "),
                                if ported.is_empty() { "" } else { " or its port" }
                            ),
                        ),
                    ));
                    demoted.extend(uses.iter().map(|&k| (k, why.clone())));
                }
            }
        }
        if notes.is_empty() {
            continue;
        }
        for (k, why) in demoted {
            for n in &mut p.rows[k].notes {
                if n.severity == Severity::Issue
                    && n.category != check::Category::Comment
                    && n.category != check::Category::Value
                {
                    n.severity = Severity::Note;
                    n.message = format!("{}; {why}", n.message);
                }
            }
        }
        for (k, n) in notes {
            p.rows[k].notes.insert(0, n);
        }
        let kept: Vec<Finding> = p
            .findings
            .iter()
            .filter(|f| f.category == check::Category::Pairing && f.cpp_line.is_none())
            .cloned()
            .collect();
        let refreshed = check::refresh(&mut p.rows, &p.cpp, &p.rust);
        p.findings = kept.into_iter().chain(refreshed).collect();
    }
}

/// The code of a helper `h` a C++ function calls: a lambda it defines,
/// or a function of the change. `None` for a helper too small to be worth
/// keeping (one statement that makes no calls, like a getter).
fn helper_body(h: &str, f: &Function, cpp: &[Function]) -> Option<String> {
    let key = |base: &str| normalize::call(base).unwrap_or_else(|| normalize::ident(base));
    let text = if let Some(start) = f.lines.iter().position(|l| {
        let t = l.trim_start();
        t.starts_with("auto ")
            && t.contains('=')
            && key(t[5..].split('=').next().unwrap_or("").trim()) == h
    }) {
        let mut depth = 0i32;
        let mut out = Vec::new();
        for l in &f.lines[start..] {
            out.push(l.as_str());
            depth += l.matches('{').count() as i32 - l.matches('}').count() as i32;
            if depth <= 0 && l.contains('}') {
                break;
            }
        }
        out.join("\n")
    } else {
        let g = cpp.iter().find(|g| key(&g.base) == h && g.path == f.path)?;
        g.lines.join("\n")
    };
    let statements = text.matches(';').count();
    let calls = text.matches('(').count();
    (statements > 1 || calls > 1).then_some(text)
}

/// How much of a helper's code the Rust has in its place: the helper's
/// calls it makes, or the helper's words it uses. `None` when too little
/// to call the helper inlined.
fn copied_into(body: &str, rust_calls: &[String], rust_words: &[String]) -> Option<f64> {
    static CALL: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"\b([A-Za-z_]\w*)\s*\(").unwrap());
    const KEYWORDS: &[&str] = &[
        "if",
        "for",
        "while",
        "switch",
        "return",
        "sizeof",
        "auto",
        "const",
        "static",
        "cast",
        "reinterpret",
        "uint",
        "int",
        "size",
        "void",
        "bool",
        "true",
        "false",
        "nullptr",
        "this",
    ];
    let calls: Vec<String> = CALL
        .captures_iter(body)
        .filter_map(|c| normalize::call(&c[1]))
        .filter(|c| !KEYWORDS.contains(&c.as_str()))
        .collect();
    let made = calls
        .iter()
        .filter(|c| rust_calls.iter().any(|d| check::call_like(c, d)))
        .count();
    let words: Vec<String> = normalize::words(body)
        .into_iter()
        .filter(|w| w.len() > 2 && !KEYWORDS.contains(&w.as_str()))
        .collect();
    let named = words.iter().filter(|w| rust_words.contains(w)).count();
    let share = if words.is_empty() {
        0.0
    } else {
        named as f64 / words.len() as f64
    };
    (made * 2 >= calls.len().max(1) || share >= 0.6).then_some(share)
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Whether C++ `c` and Rust `r` may be the same method, judging by their
/// classes. `A::f` doesn't port to `B::f` when the change has both a C++
/// class named like `B` and a Rust type named like `A`, unless one class
/// derives from the other.
fn class_compatible(
    c: &Function,
    r: &Function,
    cpp_classes: &[String],
    rust_classes: &[String],
    h: &Hierarchy,
) -> bool {
    let (Some(ca), Some(rb)) = (class_key(c), class_key(r)) else {
        return true;
    };
    if ca == rb || h.is_ancestor(&ca, &rb) || h.is_ancestor(&rb, &ca) {
        return true;
    }
    !(rust_classes.contains(&ca) && cpp_classes.contains(&rb))
}

/// Whether unchanged C++ `c` can stand for Rust `r` by class: the same
/// class, or a base of the C++ class named like the Rust type.
fn unchanged_class_ok(c: &Function, r: &Function, h: &Hierarchy) -> bool {
    match (class_key(c), class_key(r)) {
        (Some(ca), Some(rb)) => ca == rb || h.is_ancestor(&ca, &rb),
        _ => true,
    }
}

/// A Rust test: `#[test]` (or a test attribute of another framework), or a
/// function in a test file.
fn is_rust_test(f: &Function) -> bool {
    let attrs = f.lines.iter().take_while(|l| !l.contains("fn ")).any(|l| {
        let t = l.trim_start();
        t.starts_with("#[") && t.contains("test")
    });
    let file = file_name(&f.path);
    attrs
        || f.base.starts_with("test_")
        || f.path.contains("/tests/")
        || file.ends_with("_test.rs")
        || file.ends_with("_tests.rs")
        || file == "tests.rs"
}

/// C++ test code.
fn cpp_is_test(c: &Function) -> bool {
    c.path.contains("test")
        || c.lines
            .iter()
            .any(|l| l.contains("UNITTEST") || l.contains("BEGIN_TEST"))
}

/// A C++ function that exists for Rust to call through FFI.
fn is_cpp_shim(f: &Function) -> bool {
    f.base.starts_with("cpp_") && (f.path.ends_with("_ffi.cc") || f.path.ends_with("_ffi.cpp"))
}

/// Number of units in a function's body that do something.
fn body_len(f: &Function) -> usize {
    f.units
        .iter()
        .filter(|u| !matches!(u.kind, UnitKind::Signature | UnitKind::Comment))
        .count()
}

/// If `r` only forwards to another Rust function in `pool` that is named
/// like it and resembles `c` better, that function's index.
fn forwardee(c: &Function, r: &Function, pool: &[Function], used: &[bool]) -> Option<usize> {
    if body_len(r) > 5 {
        return None;
    }
    let own = normalize::words(&normalize::ident(&r.base));
    let current = score(c, r).0;
    pool.iter()
        .enumerate()
        .filter(|(j, t)| {
            !used[*j]
                && !t.is_ffi
                && (t.path != r.path || t.start_line != r.start_line)
                && body_len(t) > body_len(r)
                && calls_function(r, t)
                && {
                    // `dump` and `dump`, or `create` and `create_locked`.
                    let theirs = normalize::words(&normalize::ident(&t.base));
                    theirs.iter().all(|w| own.contains(w)) || own.iter().all(|w| theirs.contains(w))
                }
        })
        .map(|(j, t)| (j, score(c, t).0, passes_through(r, t)))
        .filter(|(_, s, through)| *through || *s > current)
        .map(|(j, s, _)| (j, s))
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(j, _)| j)
}

/// Whether `a` is a thin wrapper that reaches `b`, directly or through
/// another thin wrapper (`arch_foo` calling `rust_arch_foo` calling `foo`).
fn forwards_to(a: &Function, b: &Function, universe: &[Function]) -> bool {
    let thin = |f: &Function| body_len(f) <= 3;
    let key = |f: &Function| normalize::call(&f.base).unwrap_or_else(|| normalize::ident(&f.base));
    let target = key(b);
    // An arch-generic facade picks an implementation per `cfg`.
    let facade = a.lines.iter().any(|l| l.contains("cfg")) && body_len(a) <= 10;
    (thin(a) || facade && a.calls.contains(&target))
        && (a.calls.contains(&target)
            || universe.iter().any(|m| {
                thin(m)
                    && m.base != a.base
                    && m.base != b.base
                    && a.calls.contains(&key(m))
                    && m.calls.contains(&target)
            }))
}

/// A function's code without comments or whitespace, to tell identical
/// copies apart from diverging ones.
fn body_code(f: &Function) -> Vec<String> {
    f.units
        .iter()
        .filter(|u| !matches!(u.kind, UnitKind::Signature | UnitKind::Comment))
        .flat_map(|u| (u.start_line..=u.end_line).map(|l| f.line(l)))
        .map(|l| {
            let l = l.split("//").next().unwrap_or("");
            l.split_whitespace().collect::<String>()
        })
        .filter(|l| !l.is_empty())
        .collect()
}

/// Whether `r` calls `t`, allowing for `Type::init()` normalizing to the
/// type's name.
fn calls_function(r: &Function, t: &Function) -> bool {
    let by_type = matches!(t.base.as_str(), "new" | "init")
        && t.class
            .as_deref()
            .is_some_and(|c| r.calls.contains(&normalize::ident(c)));
    if by_type {
        return true;
    }
    if r.calls.contains(&normalize::ident(&t.base)) {
        return rust_call_matches(r, t, false);
    }
    !matches!(t.base.as_str(), "new" | "init" | "default")
        && normalize::call(&t.base).is_none()
        && rust_call_matches(r, t, true)
}

/// Checks that at least one call site of `callee.base` in `caller` matches
/// `callee`'s parameter count (accounting for `self` receiver vs. associated
/// call) and optional type qualifier, so e.g. `AtomicBool::store(val, order)`
/// is not mistaken for `RelaxedAtomicPressureLevel::store(level)`.
fn rust_call_matches(caller: &Function, callee: &Function, require_site: bool) -> bool {
    if caller.lang != crate::model::Lang::Rust || callee.lang != crate::model::Lang::Rust {
        return !require_site;
    }
    let Some((has_self, total_params)) = rust_fn_params(callee) else {
        return !require_site;
    };
    let code = body_text(caller);
    let base = &callee.base;
    if base.is_empty() {
        return !require_site;
    }
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let mut found_site = false;
    for (idx, _) in code.match_indices(base.as_str()) {
        let before = code[..idx].chars().next_back();
        let after = code[idx + base.len()..].chars().next();
        if before.is_some_and(is_word) || after.is_some_and(is_word) {
            continue;
        }
        let mut rest = code[idx + base.len()..].trim_start();
        if let Some(after_colons) = rest.strip_prefix("::") {
            let after_colons = after_colons.trim_start();
            if let Some(end_turbo) = balanced_delim(after_colons, '<', '>') {
                rest = after_colons[end_turbo..].trim_start();
            }
        }
        let Some(end_paren) = balanced_delim(rest, '(', ')') else {
            continue;
        };
        found_site = true;
        let args_str = &rest[1..end_paren - 1];
        let call_args = count_top_level_items(args_str);
        let prefix = code[..idx].trim_end();
        if prefix.ends_with('.') {
            if has_self && call_args + 1 == total_params {
                return true;
            }
        } else if let Some(before_colons) = prefix.strip_suffix("::") {
            let qual: String = before_colons
                .trim_end()
                .chars()
                .rev()
                .take_while(|&c| is_word(c))
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            let qual_ok = match callee.class.as_deref() {
                Some(cls) if !qual.is_empty() && qual != "Self" => {
                    normalize::ident(&qual) == normalize::ident(cls)
                }
                _ => true,
            };
            if qual_ok && call_args == total_params {
                return true;
            }
        } else if !has_self && call_args == total_params {
            return true;
        }
    }
    !require_site && !found_site
}

fn rust_fn_params(f: &Function) -> Option<(bool, usize)> {
    let code: String = f
        .lines
        .iter()
        .map(|l| l.split("//").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join(" ");
    let fn_pos = code.find("fn ")?;
    let after_fn = &code[fn_pos + 3..];
    let open = after_fn.find('(')?;
    let paren_slice = &after_fn[open..];
    let end = balanced_delim(paren_slice, '(', ')')?;
    let inner = paren_slice[1..end - 1].trim();
    if inner.is_empty() {
        return Some((false, 0));
    }
    let items = split_top_level_commas(inner);
    let first = items.first().map(|s| s.trim()).unwrap_or("");
    let first_name = first.split(':').next().unwrap_or(first).trim();
    let has_self =
        first_name == "self" || first_name.ends_with(" self") || first_name.ends_with("&self");
    Some((has_self, items.len()))
}

fn balanced_delim(s: &str, open: char, close: char) -> Option<usize> {
    if !s.starts_with(open) {
        return None;
    }
    let mut depth = 0usize;
    for (i, ch) in s.char_indices() {
        if ch == open {
            depth += 1;
        } else if ch == close {
            depth -= 1;
            if depth == 0 {
                return Some(i + ch.len_utf8());
            }
        }
    }
    None
}

fn split_top_level_commas(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut p, mut b, mut c, mut a) = (0i32, 0i32, 0i32, 0i32);
    let mut start = 0usize;
    let bytes = s.as_bytes();
    for (i, ch) in s.char_indices() {
        match ch {
            '(' => p += 1,
            ')' => p = (p - 1).max(0),
            '[' => b += 1,
            ']' => b = (b - 1).max(0),
            '{' => c += 1,
            '}' => c = (c - 1).max(0),
            '<' if i == 0 || !matches!(bytes.get(i.wrapping_sub(1)), Some(b'-' | b'=')) => a += 1,
            '>' if a > 0 && !matches!(bytes.get(i.wrapping_sub(1)), Some(b'-' | b'=')) => a -= 1,
            ',' if p == 0 && b == 0 && c == 0 && a == 0 => {
                let item = s[start..i].trim();
                if !item.is_empty() {
                    out.push(item);
                }
                start = i + 1;
            }
            _ => {}
        }
    }
    let tail = s[start..].trim();
    if !tail.is_empty() {
        out.push(tail);
    }
    out
}

fn count_top_level_items(s: &str) -> usize {
    let s = s.trim();
    if s.is_empty() {
        0
    } else {
        split_top_level_commas(s).len()
    }
}

/// Aligns the primary C++ function, then each override in turn against the
/// Rust units still unmatched. Returns the primary's alignment followed by
/// one alignment per override, in Rust unit indices; the override
/// alignments leave out Rust-only rows.
/// How alike an override's unit and the primary's Rust must be to count as
/// shared code.
const SHARED_MIN: f64 = 0.6;

fn override_rows(primary: &Function, rust: &Function, ovs: &[&Function]) -> Vec<Vec<Pair>> {
    let (main, _) = align::align(&primary.units, &rust.units);
    let mut taken: Vec<bool> = vec![false; rust.units.len()];
    for p in &main {
        if let (Some(_), Some(j)) = (p.cpp, p.rust) {
            taken[j] = true;
        }
    }
    let mut out = Vec::new();
    for o in ovs {
        let free: Vec<usize> = (0..rust.units.len())
            .filter(|&j| !taken[j] && rust.units[j].kind != UnitKind::Signature)
            .collect();
        let units: Vec<crate::model::Unit> = free.iter().map(|&j| rust.units[j].clone()).collect();
        let (pairs, _) = align::align(&o.units, &units);
        let mapped: Vec<Pair> = pairs
            .into_iter()
            .filter(|p| p.cpp.is_some())
            .map(|p| Pair {
                cpp: p.cpp,
                rust: p.rust.map(|j| free[j]),
                score: p.score,
            })
            .collect();
        for p in &mapped {
            if let Some(j) = p.rust {
                taken[j] = true;
            }
        }
        // What the override shares with the primary (an argument check
        // hoisted above the `match`, the final `Ok(())`) matches the Rust
        // the primary matched.
        let mut mapped = mapped;
        let left: Vec<usize> = mapped
            .iter()
            .enumerate()
            .filter(|(_, p)| p.rust.is_none())
            .map(|(k, _)| k)
            .collect();
        let shared: Vec<usize> = main.iter().filter_map(|p| p.cpp.and(p.rust)).collect();
        if !left.is_empty() && !shared.is_empty() {
            let cu: Vec<crate::model::Unit> = left
                .iter()
                .map(|&k| o.units[mapped[k].cpp.unwrap()].clone())
                .collect();
            let ru: Vec<crate::model::Unit> =
                shared.iter().map(|&j| rust.units[j].clone()).collect();
            let (pairs, _) = align::align(&cu, &ru);
            for p in pairs {
                if let (Some(a), Some(b)) = (p.cpp, p.rust) {
                    if p.score >= SHARED_MIN {
                        mapped[left[a]].rust = Some(shared[b]);
                        mapped[left[a]].score = p.score;
                    }
                }
            }
        }
        out.push(mapped);
    }
    out
}

fn build_pair(
    cpp: Function,
    rust: Function,
    link: Link,
    origin: CppOrigin,
    overrides: Vec<Function>,
    elsewhere: &[&Function],
    values: &crate::values::Values,
) -> PairReport {
    let (s, pairs) = score(&cpp, &rust);
    let ov_refs: Vec<&Function> = overrides.iter().collect();
    let ov_rows = if overrides.is_empty() {
        Vec::new()
    } else {
        override_rows(&cpp, &rust, &ov_refs)
    };
    // Rust units an override accounts for are not "only in Rust", nor is
    // the `match` that replaced virtual dispatch.
    let mut claimed: Vec<usize> = ov_rows
        .iter()
        .flatten()
        .filter_map(|p| p.cpp.and(p.rust))
        .collect();
    if !overrides.is_empty() {
        claimed.extend(
            pairs
                .iter()
                .filter(|p| p.cpp.is_none())
                .filter_map(|p| p.rust)
                .filter(|&j| matches!(rust.units[j].kind, UnitKind::Switch | UnitKind::Case)),
        );
    }
    let ctx = check::Context {
        claimed: &claimed,
        elsewhere,
        values: Some(values),
    };
    let (rows, findings) = check::check_with(&cpp, &rust, &pairs, &ctx);
    let mut summary = check::summarize(&cpp, &rust, &rows);
    let overrides: Vec<OverrideReport> = overrides
        .into_iter()
        .zip(ov_rows)
        .map(|(o, pairs)| {
            let ctx = check::Context {
                claimed: &[],
                elsewhere,
                values: Some(values),
            };
            let (mut rows, mut ov_findings) = check::check_with(&o, &rust, &pairs, &ctx);
            // Code the override shares with the primary has been reported
            // once already.
            let reported = |line: Option<usize>, message: &str| {
                findings
                    .iter()
                    .any(|g| g.rust_line.is_some() && g.rust_line == line && g.message == message)
            };
            ov_findings.retain(|f| !reported(f.rust_line, &f.message));
            for r in &mut rows {
                let line = r.rust.map(|j| rust.units[j].start_line);
                r.notes.retain(|n| !reported(line, &n.message));
                if matches!(r.marker, check::Marker::Note | check::Marker::Issue) {
                    r.marker = if r.notes.is_empty() {
                        check::Marker::Same
                    } else if r.notes.iter().any(|n| n.severity == check::Severity::Issue) {
                        check::Marker::Issue
                    } else {
                        check::Marker::Note
                    };
                }
            }
            OverrideReport {
                cpp: o,
                rows,
                findings: ov_findings,
            }
        })
        .collect();
    if !overrides.is_empty() {
        merge_override_summaries(&mut summary, &rust, &overrides);
    }
    PairReport {
        cpp,
        rust,
        link,
        forwarder: None,
        origin,
        score: s,
        rows,
        findings,
        summary,
        overrides,
        rationale: String::new(),
        duplicate_of: None,
    }
}

/// Folds the overrides' facts into a pair's summary. The Rust function does
/// what the primary and each override do, once per arm, so errors and locks
/// are compared as sets, control flow by the busiest C++ version, and the
/// `match` that replaced virtual dispatch is not a difference.
fn merge_override_summaries(s: &mut Summary, rust: &Function, ovs: &[OverrideReport]) {
    for o in ovs {
        let os = check::summarize(&o.cpp, rust, &o.rows);
        s.cpp_errors.extend(os.cpp_errors);
        s.cpp_locks.extend(os.cpp_locks);
        s.comments_total += os.comments_total;
        s.comments_same += os.comments_same;
        s.comments_changed += os.comments_changed;
        for (name, a, _) in os.flow {
            match s.flow.iter_mut().find(|(n, _, _)| *n == name) {
                Some(e) => e.1 = e.1.max(a),
                None => {
                    let b = s
                        .flow
                        .iter()
                        .find(|(n, _, _)| *n == name)
                        .map_or(0, |e| e.2);
                    s.flow.push((name, a, b));
                }
            }
        }
    }
    for v in [
        &mut s.cpp_errors,
        &mut s.rust_errors,
        &mut s.cpp_locks,
        &mut s.rust_locks,
    ] {
        v.sort();
        v.dedup();
    }
    s.flow
        .retain(|(name, a, _)| !(matches!(*name, "switch" | "case") && *a == 0));
}

/// The text of a comment line without its markers, if it is long enough to
/// be distinctive.
fn comment_line_text(l: &str) -> Option<String> {
    let t = l.trim();
    let t = ["///", "//!", "//", "/**", "/*", "*/", "*"]
        .iter()
        .find_map(|m| t.strip_prefix(m))
        .unwrap_or(t)
        .trim_end_matches("*/")
        .trim();
    (t.len() >= 16).then(|| t.to_string())
}

/// Marks C++ doc comments that are still, word for word, in C++ the change
/// didn't touch (usually the declaration's comment in an unchanged header):
/// not carrying them into Rust loses nothing.
fn mark_comments_in_untouched_cpp(
    cpp: &mut [Function],
    changed: &[String],
    finder: &mut dyn CppFinder,
) {
    let lines_of = |f: &Function, u: &Unit| -> Vec<String> {
        let raw: Vec<&str> = if u.file.is_some() {
            u.ext_lines.iter().map(String::as_str).collect()
        } else {
            (u.start_line..=u.end_line).map(|l| f.line(l)).collect()
        };
        raw.into_iter().filter_map(comment_line_text).collect()
    };
    let leading = |f: &Function| {
        f.units
            .iter()
            .take_while(|u| u.kind != UnitKind::Signature)
            .count()
    };
    let mut wanted: Vec<String> = Vec::new();
    for f in cpp.iter() {
        for u in &f.units[..leading(f)] {
            if u.kind == UnitKind::Comment && !u.features.still_in_cpp {
                wanted.extend(lines_of(f, u));
            }
        }
    }
    wanted.sort();
    wanted.dedup();
    if wanted.is_empty() {
        return;
    }
    let found = finder.still_present(&wanted, changed);
    if found.is_empty() {
        return;
    }
    for f in cpp.iter_mut() {
        let n = leading(f);
        let marks: Vec<bool> = f.units[..n]
            .iter()
            .map(|u| {
                let ls = lines_of(f, u);
                u.kind == UnitKind::Comment
                    && ls.iter().map(String::len).sum::<usize>() >= 30
                    && ls.iter().all(|l| found.contains(l))
            })
            .collect();
        for (u, m) in f.units.iter_mut().zip(marks) {
            if m {
                u.features.still_in_cpp = true;
            }
        }
    }
}
