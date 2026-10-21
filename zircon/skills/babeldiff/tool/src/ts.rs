//! Thin helpers over tree-sitter shared by the C++ and Rust extractors.

use crate::model::{Features, Lang, Ret, Unit, UnitKind};
use crate::normalize;
use regex::Regex;
use std::sync::LazyLock;
use tree_sitter::{Node, Parser, Tree};

pub fn parse(lang: Lang, src: &str) -> Tree {
    let mut parser = Parser::new();
    let language = match lang {
        Lang::Cpp => tree_sitter_cpp::LANGUAGE.into(),
        Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
    };
    parser.set_language(&language).expect("tree-sitter grammar version mismatch");
    parser.parse(src, None).expect("tree-sitter parse cancelled")
}

pub fn text<'a>(n: Node, src: &'a [u8]) -> &'a str {
    n.utf8_text(src).unwrap_or("")
}

pub fn named_children<'t>(n: Node<'t>) -> Vec<Node<'t>> {
    let mut cursor = n.walk();
    n.named_children(&mut cursor).collect()
}

pub fn children<'t>(n: Node<'t>) -> Vec<Node<'t>> {
    let mut cursor = n.walk();
    n.children(&mut cursor).collect()
}

/// 1-based first line of a node.
pub fn line(n: Node) -> usize {
    n.start_position().row + 1
}

/// 1-based last line of a node.
pub fn end_line(n: Node) -> usize {
    let end = n.end_position();
    // A node ending at column 0 ends on the previous line.
    if end.column == 0 && end.row > n.start_position().row { end.row } else { end.row + 1 }
}

pub fn is_comment(n: Node) -> bool {
    matches!(n.kind(), "comment" | "line_comment" | "block_comment")
}

/// Source text of `n` with the byte ranges of `skip` nodes blanked out.
pub fn text_without(n: Node, src: &[u8], skip: &[Node]) -> String {
    let start = n.start_byte();
    let mut bytes = src[start..n.end_byte()].to_vec();
    for s in skip {
        let (a, b) = (s.start_byte().max(start), s.end_byte().min(n.end_byte()));
        for byte in bytes.iter_mut().take(b.saturating_sub(start)).skip(a.saturating_sub(start)) {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Accumulates features while walking a syntax subtree.
#[derive(Default)]
pub struct FeatureAcc {
    pub calls: Vec<String>,
    pub idents: Vec<String>,
    pub text: String,
    pub propagates: bool,
    /// Names of called functions as written, so they are not also counted
    /// as identifiers.
    pub callees: Vec<String>,
    /// Calls through a type path, as `type::method` (`Foo::create`).
    pub qcalls: Vec<String>,
}

impl FeatureAcc {
    pub fn call(&mut self, name: &str) {
        let last = name.rsplit(['.', ':', '>']).next().unwrap_or(name);
        self.callees.push(normalize::ident(last.trim_end_matches('!')));
        if let Some(c) = normalize::call(name) {
            self.calls.push(c);
        }
        if let Some(q) = normalize::qualified_call(name) {
            self.qcalls.push(q);
        }
    }

    pub fn ident(&mut self, name: &str) {
        if let Some(i) = normalize::ident_feature(name) {
            self.idents.push(i);
        }
    }

    pub fn finish(self, lang: Lang) -> Features {
        let mut idents = self.idents;
        idents.sort();
        idents.dedup();
        // Called names are compared as calls, not identifiers.
        let calls = &self.calls;
        let callees = &self.callees;
        idents.retain(|i| {
            !calls.contains(i)
                && !callees.contains(i)
                && normalize::call(i).is_none_or(|c| !calls.contains(&c))
        });
        let errors = normalize::error_codes(&self.text);
        let locks = match lang {
            Lang::Cpp => cpp_locks(&self.text),
            Lang::Rust => rust_locks(&self.text),
        };
        let mut calls = self.calls;
        // Inline assembly: the registers and instructions in the template
        // are what the statement does, and Rust's operand keywords
        // (`in(reg)`, `options(...)`) are not calls.
        if ASM.is_match(&self.text) {
            calls.retain(|c| !ASM_OPERANDS.contains(&c.as_str()));
            if !calls.iter().any(|c| c == "asm") {
                calls.push("asm".to_string());
            }
            for lit in ASM_STR.captures_iter(&self.text) {
                for w in ASM_WORD.captures_iter(&lit[1]) {
                    let w = w[1].to_ascii_lowercase();
                    if !idents.contains(&w) {
                        idents.push(w);
                    }
                }
            }
        }
        // An atomic static mirroring a C++ global (`XCR0_BITMAP_ATOMIC.store(v)`
        // or `G_WIDTH.load()` for `xcr0_bitmap = v` or `g_width`) reads and
        // writes the global.
        if lang == Lang::Rust {
            for c in ATOMIC_MIRROR.captures_iter(&self.text) {
                let name = c[1].strip_suffix("_ATOMIC").unwrap_or(&c[1]);
                let name = name.strip_prefix("G_").unwrap_or(name);
                if let Some(i) = normalize::ident_feature(&name.to_ascii_lowercase()) {
                    for n in [i.clone(), format!("g_{i}")] {
                        if !idents.contains(&n) {
                            idents.push(n);
                        }
                    }
                }
                calls.retain(|x| x != &c[2]);
            }
        }
        if !locks.is_empty() {
            // Acquisitions are compared as locks, not calls.
            const LOCK_CALLS: &[&str] = &[
                "lock",
                "read_lock",
                "write_lock",
                "lock_read",
                "lock_write",
                "lock_irqsave",
                "lock_irq",
                "try_lock",
                "acquire",
                "acquire_irq_save",
            ];
            calls.retain(|c| !LOCK_CALLS.contains(&c.as_str()) && !c.contains("lock"));
            idents.retain(|i| !i.contains("lock"));
        }
        // Names are compared without underscores, so a C++ enumerator
        // `NEEDACK` matches the Rust variant `NeedAck`.
        let mut names: Vec<String> = calls
            .iter()
            .chain(idents.iter())
            .map(|n| {
                let n = n.strip_prefix("set_").unwrap_or(n);
                n.strip_prefix("get_").unwrap_or(n).replace('_', "")
            })
            .collect();
        let idents: Vec<String> = idents.iter().map(|i| i.replace('_', "")).collect();
        names.sort();
        names.dedup();
        let asserts = calls.iter().any(|c| c == "assert");
        let unlocks = calls.iter().any(|c| c == "release")
            && (self.text.contains("guard") || self.text.contains("lock"));
        Features {
            calls,
            idents,
            names,
            errors,
            locks,
            unlocks,
            propagates: self.propagates,
            asserts,
            checks_error: false,
            ret: None,
            comment: Vec::new(),
            safety: false,
            lock_plumbing: false,
            plumbing: false,
            conjuncts: Vec::new(),
            qcalls: self.qcalls,
            still_in_cpp: false,
        }
    }
}

static ASM: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\basm!\s*\(|\b__asm__\b|\basm\s+(?:volatile\s*)?\(|\basm\s*\(").unwrap()
});
static ASM_STR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#""((?:[^"\\]|\\.)*)""#).unwrap());
/// Words in an assembly template, without operand placeholders (`%0`,
/// `{}`, `{val}`).
static ASM_WORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|[^%{\w])%{0,2}([A-Za-z][A-Za-z0-9]*)\b").unwrap());
const ASM_OPERANDS: &[&str] = &[
    "in",
    "out",
    "inout",
    "lateout",
    "inlateout",
    "options",
    "reg",
    "sym",
    "const",
    "clobber_abi",
];

static CPP_GUARD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\b(\w*Guard|AutoLock|AutoSpinLock\w*|lock_guard|unique_lock|scoped_lock|shared_lock)\s*(<(?:[^<>]|<[^<>]*>)*>)?\s+\w+\s*(?:[{(]\s*([^;]*?)\s*[})])?\s*;",
    )
    .unwrap()
});
static CPP_LOCK_CALL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"([A-Za-z_][\w]*(?:\s*(?:\.|->|::)\s*[A-Za-z_]\w*(?:\(\))?)*)\s*(?:\.|->)\s*(Acquire|AcquireIrqSave|Lock|ReadLock|WriteLock|lock|lock_shared)\s*\(")
        .unwrap()
});
static RUST_LOCK_CALL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"([A-Za-z_][\w]*(?:\s*(?:\.|::)\s*[A-Za-z_]\w*(?:\(\))?)*)\s*\.\s*(lock|lock_irqsave|lock_irq|try_lock|read_lock|write_lock|lock_read|lock_write|acquire|lock_[a-z]\w*)\s*(?:::\s*<(?:[^<>]|<[^<>]*>)*>\s*)?\(")
        .unwrap()
});
/// A lock acquired through an associated function of a lock type rather
/// than a method on a value: `QcomRngLock::lock()` as generated by
/// `ksync::declare_singleton_mutex!` (the C++ `Guard<Mutex> guard(
/// QcomRngLock::Get())`), or `ThreadLockGuard::lock(thread, tag)`.
static RUST_TYPE_LOCK_CALL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b((?:[A-Za-z_]\w*\s*::\s*)*[A-Z]\w*)\s*::\s*(lock|lock_policy|lock_irqsave|lock_irq|try_lock|read_lock|write_lock|lock_read|lock_write|acquire)\s*(?:::\s*<(?:[^<>]|<[^<>]*>)*>\s*)?\(\s*&?(?:mut\s+)?((?:[^,()]|\([^)]*\))*)")
        .unwrap()
});
/// Lock types whose associated `lock(x)` locks the argument, not a lock
/// named after the type.
const GENERIC_LOCK_TYPES: &[&str] = &[
    "Mutex",
    "KMutex",
    "SpinLock",
    "Spinlock",
    "KSpinlock",
    "RwLock",
    "BrwLockPi",
    "CriticalMutex",
    "MonitoredSpinLock",
    "MonitoredSpinlock",
    "KMonitoredSpinlock",
    "ChainLock",
];
static ATOMIC_MIRROR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b([A-Z][A-Z0-9_]*)\s*(?:\[[^\]]*\])?\s*\.\s*(load|store|swap|fetch_[a-z]+)\s*\(")
        .unwrap()
});

static RUST_WITH_LOCK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\bwith_(?:chain_)?lock(?:_irqsave|_irq)?\s*\(\s*&?(?:mut\s+)?([A-Za-z_][\w.]*)\s*,",
    )
    .unwrap()
});
static RUST_GUARD_NEW: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(\w*Guard)\s*(?:::\s*<[^>]*>)?\s*::\s*new\s*\(\s*([^,)]*)").unwrap()
});

fn cpp_locks(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for c in CPP_GUARD.captures_iter(text) {
        let ty = &c[1];
        let targs = c.get(2).map_or("", |m| m.as_str());
        let args = c.get(3).map_or("", |m| m.as_str());
        // Chain-lock guards take options and a tag besides the lock:
        // `SingleChainLockGuard guard{IrqSaveOption, thread->get_lock(),
        // CLT_TAG("...")}`.
        let arg = args
            .split(',')
            .map(str::trim)
            .find(|a| !a.ends_with("Option") && !a.starts_with("CLT_TAG") && !a.starts_with('"'))
            .unwrap_or("");
        let arg = arg
            .strip_suffix("get_lock()")
            .map(|r| r.trim_end_matches("->").trim_end_matches('.'))
            .filter(|r| !r.is_empty())
            .unwrap_or(arg);
        let name =
            if arg.is_empty() { guard_type_name(ty) } else { normalize::lock_key(arg, None) };
        let mode = if targs.contains("Reader") || targs.contains("Shared") || ty == "shared_lock" {
            " (read)"
        } else if targs.contains("Writer") {
            " (write)"
        } else {
            ""
        };
        out.push(format!("{name}{mode}"));
    }
    for c in CPP_LOCK_CALL.captures_iter(text) {
        let method = &c[2];
        let mode = match method {
            "ReadLock" | "lock_shared" => " (read)",
            "WriteLock" => " (write)",
            _ => "",
        };
        out.push(format!("{}{mode}", normalize::lock_key(&c[1], None)));
    }
    out
}

fn rust_locks(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for c in RUST_LOCK_CALL.captures_iter(text) {
        let method = &c[2];
        let mode = match method {
            "read_lock" | "lock_read" => " (read)",
            "write_lock" | "lock_write" => " (write)",
            _ => "",
        };
        out.push(format!("{}{mode}", normalize::lock_key(&c[1], Some(method))));
    }
    // A lock type's associated function: `QcomRngLock::lock()` names the
    // lock itself; a guard type's (`ThreadLockGuard::lock(thread, tag)`)
    // or a generic lock type's (`Mutex::lock(&self.mu)`) locks its first
    // argument, like `Guard::new(lock)`.
    for c in RUST_TYPE_LOCK_CALL.captures_iter(text) {
        let method = &c[2];
        let mode = match method {
            "read_lock" | "lock_read" => " (read)",
            "write_lock" | "lock_write" => " (write)",
            _ => "",
        };
        let ty = c[1].rsplit("::").next().unwrap_or(&c[1]).trim();
        let arg = c[3].trim();
        let is_guard = ty.ends_with("Guard");
        let name = if !arg.is_empty() && (is_guard || GENERIC_LOCK_TYPES.contains(&ty)) {
            normalize::lock_key(arg, None)
        } else if is_guard {
            guard_type_name(ty)
        } else {
            normalize::lock_key(ty, None)
        };
        out.push(format!("{name}{mode}"));
    }
    // A lock taken for the length of a callback: `with_chain_lock(thread, f)`.
    for c in RUST_WITH_LOCK.captures_iter(text) {
        out.push(normalize::lock_key(c[1].trim(), None));
    }
    for c in RUST_GUARD_NEW.captures_iter(text) {
        let arg = c[2].trim();
        let name =
            if arg.is_empty() { guard_type_name(&c[1]) } else { normalize::lock_key(arg, None) };
        out.push(name);
    }
    out
}

/// `InterruptDisableGuard` -> `interrupt_disable`.
fn guard_type_name(ty: &str) -> String {
    let n = normalize::ident(ty);
    let n = n.trim_end_matches("guard").trim_end_matches('_');
    if n.is_empty() { "guard".to_string() } else { n.to_string() }
}

static STATUS_VAR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\(?\s*(?:[a-z_]*status|st|rc|ret|res|result|err|e|error|r)\s*\)?$|take_error\s*\(|error_value\s*\(|status_value\s*\(|^(?:zx::error|fit::error|zx::error_result|Err)\s*\(\s*[a-z_]+\s*\)$")
        .unwrap()
});

/// Classifies the expression of a return statement or tail expression.
pub fn classify_return(expr: &str) -> Ret {
    let expr = expr.trim();
    let errors = normalize::error_codes(expr);
    if let Some(e) = errors.into_iter().next() {
        Ret::Error(e)
    } else if normalize::mentions_ok(expr) {
        Ret::Ok
    } else if STATUS_VAR.is_match(expr) {
        Ret::Status
    } else {
        Ret::Value
    }
}

/// Whether the source lines of `u` mention `name` as a whole word.
pub fn mentions(lines: &[String], u: &Unit, name: &str) -> bool {
    (u.start_line..=u.end_line).any(|l| {
        lines.get(l - 1).is_some_and(|t| {
            t.match_indices(name).any(|(i, _)| {
                let before = t[..i].chars().next_back();
                let after = t[i + name.len()..].chars().next();
                let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
                !word(before) && !word(after)
            })
        })
    })
}

/// Builds the flat list of units for a function body.
pub struct UnitBuilder {
    pub units: Vec<Unit>,
}

impl UnitBuilder {
    pub fn new() -> Self {
        UnitBuilder { units: Vec::new() }
    }

    pub fn push(
        &mut self,
        kind: UnitKind,
        start: usize,
        end: usize,
        depth: usize,
        features: Features,
    ) {
        self.units.push(Unit {
            kind,
            start_line: start,
            end_line: end.max(start),
            depth,
            features,
            file: None,
            ext_lines: Vec::new(),
        });
    }

    /// Adds a comment, merging it into the previous comment unit when both
    /// are line comments on adjacent lines at the same depth.
    pub fn comment(&mut self, raw: &str, start: usize, end: usize, depth: usize) {
        let words = normalize::comment_words(raw);
        if words.is_empty() || words == ["static"] {
            // Empty comments and Zircon's `// static` marker carry no meaning,
            // but a blank `///` line doesn't end a `# Safety` section.
            if let Some(last) = self.units.last_mut() {
                if words.is_empty()
                    && last.kind == UnitKind::Comment
                    && last.features.safety
                    && last.depth == depth
                    && last.end_line + 1 == start
                    && last.file.is_none()
                {
                    last.end_line = end;
                }
            }
            return;
        }
        let is_line = raw.trim_start().starts_with("//");
        let body = raw.trim_start().trim_start_matches(['/', '!', '*']).trim_start();
        // Safety comments start their own unit; lines that follow join it.
        let starts_safety = body.starts_with("SAFETY:") || body.starts_with("# Safety");
        if let Some(last) = self.units.last_mut() {
            if is_line
                && (!starts_safety || last.features.safety)
                && last.kind == UnitKind::Comment
                && last.depth == depth
                && last.end_line + 1 == start
                && last.file.is_none()
            {
                last.end_line = end;
                last.features.comment.extend(words);
                return;
            }
        }
        let features = Features { comment: words, safety: starts_safety, ..Features::default() };
        self.push(UnitKind::Comment, start, end, depth, features);
    }
}

impl Default for UnitBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_locks_through_methods() {
        assert_eq!(rust_locks("ksync::lock!(let guard = self.lock.lock());"), ["lock"]);
        assert_eq!(
            rust_locks("ksync::lock!(let mut guard = self.lock_list_lock());"),
            ["list_lock"]
        );
        assert_eq!(
            rust_locks("ksync::lock!(let guard = state.lock_lock(source_tag!()));"),
            ["lock"]
        );
        assert_eq!(
            rust_locks("ksync::lock!(let mut guard = PendingListLock::get().lock());"),
            ["pending_list_lock"]
        );
        assert_eq!(
            rust_locks("ksync::lock!(let guard = handle_table.read_lock());"),
            ["lock (read)"]
        );
        assert_eq!(rust_locks("let g = self.spin.lock_irqsave();"), ["spin"]);
        // A policy turbofish does not hide the lock.
        assert_eq!(
            rust_locks(
                "ksync::lock!(let mut guard = self.lock_mtx_policy::<ksync::IrqSavePolicy>());"
            ),
            ["mtx"]
        );
        assert_eq!(
            rust_locks(
                "ksync::lock!(let mut guard = self.lock_mtx_policy::<ksync::IrqSavePolicy<Tag>>());"
            ),
            ["mtx"]
        );
    }

    #[test]
    fn rust_locks_through_associated_functions() {
        // `ksync::declare_singleton_mutex!(QcomRngLock)` generates
        // `QcomRngLock::lock()`, the C++ `Guard<Mutex> guard(QcomRngLock::Get())`.
        assert_eq!(
            rust_locks("ksync::lock!(let _guard = QcomRngLock::lock());"),
            ["qcom_rng_lock"]
        );
        assert_eq!(
            rust_locks("ksync::lock!(let _guard = QcomRngLock::lock_policy::<IrqSavePolicy>());"),
            ["qcom_rng_lock"]
        );
        assert_eq!(rust_locks("ksync::lock!(Domain0Lock::lock());"), ["domain0_lock"]);
        assert_eq!(
            rust_locks("ksync::lock!(let mut guard = vm::DiscardableVmosLock::lock());"),
            ["discardable_vmos_lock"]
        );
        // A guard type locks its first argument, like a C++ chain-lock guard.
        assert_eq!(
            rust_locks("ThreadLockGuard::lock(current_thread, clt_tag!(\"halt\"))"),
            ["current_thread"]
        );
        assert_eq!(
            rust_locks("ThreadLockGuard::lock(thread.cast(), clt_tag!(\"halt\"))"),
            ["thread"]
        );
        assert_eq!(rust_locks("Mutex::lock(&self.mu)"), ["mu"]);
        assert_eq!(rust_locks("Mutex::lock(&self.inner.get())"), ["inner"]);
        assert_eq!(rust_locks("Spinlock::lock(&self.mu)"), ["mu"]);
        assert_eq!(rust_locks("KMutex::lock(&self.inner)"), ["inner"]);
        assert_eq!(rust_locks("KSpinlock::lock(&self.mu)"), ["mu"]);
        assert_eq!(rust_locks("KMonitoredSpinlock::lock(&self.mu)"), ["mu"]);
        assert_eq!(rust_locks("BrwLockPi::read_lock(&self.rw)"), ["rw (read)"]);
        // Not a lock: the `lock!` macro itself, a `::get()` accessor, or an
        // unrelated `Type::lock_foo()` associated function.
        assert!(rust_locks("ksync::lock!(let guard = lock);").is_empty());
        assert!(rust_locks("let l = QcomRngLock::get();").is_empty());
        assert!(rust_locks("Foo::lock_count()").is_empty());
        // Each acquisition counts once.
        assert_eq!(
            rust_locks("ksync::lock!(let g = QcomRngLock::get().lock());"),
            ["qcom_rng_lock"]
        );
    }

    #[test]
    fn cpp_singleton_mutex_guard_pairs_with_rust_associated_lock() {
        assert_eq!(cpp_locks("Guard<Mutex> guard(QcomRngLock::Get());"), ["qcom_rng_lock"]);
        assert_eq!(
            cpp_locks("Guard<Mutex> guard(QcomRngLock::Get());"),
            rust_locks("ksync::lock!(let _guard = QcomRngLock::lock());")
        );
    }
}
