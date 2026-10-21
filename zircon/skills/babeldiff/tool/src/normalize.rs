//! Normalization that makes C++ and Rust spellings of the same thing compare
//! equal: `CommitRange` and `commit_range`, `lock_` and `self.lock`,
//! `ZX_ERR_NO_MEMORY` and `Status::NO_MEMORY`.

use regex::Regex;
use std::sync::LazyLock;

/// Converts an identifier to lower snake case and drops leading and trailing
/// underscores, so `CommitRange`, `commit_range` and `commit_range_` agree.
/// The configuration names a conditional-compilation condition tests
/// (`#if __has_feature(safe_stack)`, `#[cfg(feature = "safe-stack")]`),
/// lowercase with separators dropped so `safe_stack` meets `safestack`.
pub fn cfg_words(text: &str) -> Vec<String> {
    const SYNTAX: &[&str] = &[
        "if",
        "ifdef",
        "ifndef",
        "elif",
        "elifdef",
        "elifndef",
        "else",
        "endif",
        "defined",
        "hasfeature",
        "hasinclude",
        "hasbuiltin",
        "hasattribute",
        "cfg",
        "not",
        "all",
        "any",
        "feature",
        "true",
        "false",
    ];
    static WORD: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"[A-Za-z_][A-Za-z0-9_-]*").unwrap());
    let mut out: Vec<String> = Vec::new();
    for m in WORD.find_iter(text) {
        let w: String = m
            .as_str()
            .chars()
            .filter(|c| *c != '_' && *c != '-')
            .collect::<String>()
            .to_ascii_lowercase();
        if !w.is_empty() && !SYNTAX.contains(&w.as_str()) && !out.contains(&w) {
            out.push(w);
        }
    }
    out
}

pub fn ident(s: &str) -> String {
    let s = s.strip_prefix("r#").unwrap_or(s);
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len() + 4);
    for (i, &c) in chars.iter().enumerate() {
        if c.is_ascii_uppercase() {
            let prev = if i > 0 { chars[i - 1] } else { '_' };
            let next = chars.get(i + 1).copied().unwrap_or('_');
            let boundary = prev.is_ascii_lowercase()
                || prev.is_ascii_digit()
                || (prev.is_ascii_uppercase() && next.is_ascii_lowercase());
            if boundary && !out.ends_with('_') && !out.is_empty() {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else if c.is_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    out.trim_matches('_').to_string()
}

/// Words of an identifier after normalization.
pub fn words(s: &str) -> Vec<String> {
    ident(s).split('_').filter(|w| !w.is_empty()).map(str::to_string).collect()
}

/// Calls that carry no meaning across the two languages (ownership and
/// conversion plumbing), dropped from the call lists.
const NOISE_CALLS: &[&str] = &[
    // A dispatcher's state struct (`self.state()`) holds what were C++
    // fields; newtype unwrapping and `size_of` (an operator in C++) do
    // nothing a reviewer compares.
    "state",
    "raw_value",
    "size_of",
    "result_into",
    "enumerate",
    // `fbl::AllocChecker ac;` only carries the result of a `new`.
    "alloc_checker",
    // Branch-prediction hints.
    "likely",
    "unlikely",
    // ksync reaches guarded state through the guard or a `KCell`.
    "fields",
    "fields_mut",
    "get_inner",
    "into_inner",
    // ksync's token guards re-borrow a lock that is already held.
    "guard_read_lock",
    "guard_write_lock",
    "token",
    "token_mut",
    "move",
    "forward",
    "get",
    "unwrap",
    "expect",
    "as_ref",
    "as_mut",
    "as_ptr",
    "as_mut_ptr",
    "clone",
    "into",
    "from",
    "try_into",
    "try_from",
    "map_err",
    "ok_or",
    "ok_or_else",
    "ok",
    "err",
    "some",
    "none",
    "box",
    "new",
    "default",
    "deref",
    "deref_mut",
    "borrow",
    "borrow_mut",
    "unwrap_or",
    "unwrap_or_default",
    "addr_of",
    "addr_of_mut",
    "cast",
    "static_cast",
    "reinterpret_cast",
    "const_cast",
    "unsafe",
    "is_ok",
    "is_err",
    "is_error",
    "status_value",
    "take_error",
    "value",
    "error_value",
    "get_mut",
    "index",
    "index_mut",
    "as_slice",
    "as_mut_slice",
    "iter",
    "iter_mut",
    "into_iter",
    "guard",
    // Option and pointer tests, which the other language writes as a
    // truthiness test or a null comparison.
    "is_some",
    "is_none",
    "is_null",
    "has_value",
    "null",
    // Cell and pin-init wrappers used for Rust interior mutability and
    // in-place initialization.
    "unsafe_cell",
    "k_cell",
    "pin_init",
    "pinned_init",
    "write_pin_init",
    // C++ atomics use implicit conversion/assignment operators where Rust
    // requires explicit `.load()` and `.store()`; memory ordering is checked
    // separately by `check::ordering_diff`.
    "load",
    "store",
    // Zircon singleton accessors and pointer/string/event conversions.
    "node",
    "get_halt_token",
    "as",
    "as_event",
    "c_str",
    "to_str",
    "to_string_lossy",
    "as_str",
    "from_ptr",
    "builtin_unreachable",
    // Zircon C++ saturating time helpers and Rust time newtype wrappers.
    "zx_time_add_duration",
    "zx_time_sub_duration",
    "zx_time_sub_time",
    "zx_duration_add_duration",
    "zx_duration_sub_duration",
    "duration_mono",
    "instant_mono",
    "instant_unknown",
    "duration_boot",
    "instant_boot",
    "timer_slack",
    // `zx::error(s)` / `zx::ok(v)` are C++'s `Err(s)` / `Ok(v)`.
    "error",
    "success",
    // Zircon's handle table is how C++ reaches a handle's dispatcher; Rust
    // looks the handle up in one call.
    "handle_table",
];

/// Normalizes the name of a called function, method or macro. Returns `None`
/// for calls that are language plumbing rather than program logic.
pub fn call(name: &str) -> Option<String> {
    let name = name.trim().trim_end_matches('!');
    // Drop template arguments and take the last path or member segment.
    let name = strip_generics(name);
    let mut segs = name.rsplit([':', '.', '>', '-']).filter(|s| !s.trim().is_empty());
    let last = segs.next().unwrap_or(&name);
    let mut n = ident(last);
    // `Type::get(...)` looks up a Type, like C++'s `GetType(...)`.
    if n == "get" {
        if let Some(ty) = name
            .rsplit("::")
            .nth(1)
            .map(str::trim)
            .filter(|s| s.starts_with(|c: char| c.is_ascii_uppercase()))
        {
            let get_name = format!("get_{}", ident(ty));
            if NOISE_CALLS.contains(&get_name.as_str()) {
                return None;
            }
            return Some(alias(&get_name).to_string());
        }
    }
    // `Foo::new(...)` constructs a Foo, like C++ `Foo foo{...}`.
    if matches!(n.as_str(), "new" | "default" | "init" | "create_in_place" | "try_new") {
        if let Some(ty) = segs
            .next()
            .map(str::trim)
            .filter(|s| s.starts_with(|c: char| c.is_ascii_uppercase()) && !s.starts_with("Atomic"))
        {
            n = ident(ty);
        }
    }
    // ksync's `guard_<lock>(&token)` gives field access under a lock that
    // is already held; it acquires nothing.
    if NOISE_CALLS.contains(&n.as_str())
        || n.starts_with("wrapping_")
        || n.starts_with("saturating_")
        || n.starts_with("guard_")
    {
        // Rust's wrapping and saturating arithmetic replaces C++ unsigned
        // and `zx_time_*` arithmetic.
        return None;
    }
    // Rust spells variants of the same operation with suffixes.
    for suffix in ["_mut", "_raw", "_unchecked"] {
        if let Some(stripped) = n.strip_suffix(suffix) {
            if !stripped.is_empty() {
                n = stripped.to_string();
            }
        }
    }
    if n.is_empty() || NOISE_CALLS.contains(&n.as_str()) {
        return None;
    }
    // C's `arch_zero_page()` is Rust's `arch::zero_page()`, `pmm_foo()` is
    // `pmm::node().foo()` / `pmm::foo()`, and an architecture's `x86_foo()`
    // is `foo()` inside its own crate.
    for prefix in ["arch_", "x86_", "arm64_", "riscv64_", "pmm_"] {
        if let Some(rest) = n.strip_prefix(prefix) {
            if !rest.is_empty() {
                n = rest.to_string();
            }
        }
    }
    // Test checks: zxtest's `EXPECT_EQ` and `ASSERT_OK` are Rust's
    // `assert_eq!` and `assert!`.
    if (n.starts_with("expect_") || n.starts_with("assert_")) && n != "assert_held" {
        return Some("assert".to_string());
    }
    Some(alias(&n).to_string())
}

/// A call through a type path as `type::method` (`Foo::Create` and
/// `Foo::create` both give `foo::create`), or `None` for other calls.
pub fn qualified_call(name: &str) -> Option<String> {
    let name = strip_generics(name.trim().trim_end_matches('!'));
    let segs: Vec<&str> = name.split("::").map(str::trim).collect();
    if segs.len() < 2 {
        return None;
    }
    let (ty, m) = (segs[segs.len() - 2], segs[segs.len() - 1]);
    if !ty.starts_with(|c: char| c.is_ascii_uppercase()) || m.is_empty() {
        return None;
    }
    Some(format!("{}::{}", ident(ty), ident(m)))
}

/// Whether a zero-argument call is likely to change state, so binding its
/// result is not mere bookkeeping.
pub fn is_mutating(name: &str) -> bool {
    let last = name.rsplit(['.', ':', '>']).next().unwrap_or(name).trim_end_matches('!');
    let w = ident(last);
    if matches!(w.as_str(), "try_from" | "try_into" | "into" | "into_inner") {
        return false;
    }
    const VERBS: &[&str] = &[
        "take",
        "pop",
        "reset",
        "release",
        "clear",
        "lock",
        "unlock",
        "acquire",
        "cancel",
        "drain",
        "next",
        "commit",
        "create",
        "alloc",
        "allocate",
        "new",
        "make",
        "destroy",
        "close",
        "open",
        "start",
        "stop",
        "wait",
        "signal",
        "trigger",
        "ack",
        "flush",
        "sync",
        "leak",
        "detach",
        "swap",
        "replace",
        "remove",
        "insert",
        "push",
        "increment",
        "decrement",
        "bind",
        "unbind",
        "init",
        "initialize",
        "try",
        "fetch",
        "read",
        "write",
        "update",
        "set",
        "run",
        "join",
        "enable",
        "disable",
        "mask",
        "unmask",
        "register",
        "unregister",
        "adopt",
        "into",
    ];
    words(&w).first().is_some_and(|first| VERBS.contains(&first.as_str()))
}

/// Maps equivalent C++ and Rust spellings onto one name.
/// Calls that do the same thing but can't share a name everywhere, because
/// one of them is too generic: `out.copy_to_user(v)` is `out.write(v)` on a
/// user pointer, but not every `write` is a user copy. They are equivalent
/// when both sides of an aligned pair make them.
pub const EQUIVALENT_CALLS: &[(&str, &str)] = &[
    ("write_user", "write"),
    ("read_user", "read"),
    ("down_cast_dispatcher", "downcast"),
    ("down_cast_dispatcher", "downcast_ref"),
];

fn alias(n: &str) -> &str {
    match n {
        "debug_assert"
        | "zx_debug_assert"
        | "debug_assert_msg"
        | "zx_debug_assert_msg"
        | "assert"
        | "zx_assert"
        | "zx_assert_msg"
        | "assert_msg"
        | "debug_assert_eq"
        | "debug_assert_ne"
        | "assert_eq"
        | "assert_ne"
        | "static_assert"
        | "const_assert" => "assert",
        "printf" | "dprintf" | "println" | "print" | "kprintf" | "kprint" | "kprintln"
        | "eprintln" => "print",
        "ltracef" | "ltracef_level" | "tracef" | "ltrace_entry" | "ltrace_exit"
        | "ltrace_entry_obj" | "ltrace_exit_obj" | "trace_duration" | "ktrace" => "trace",
        "panic" | "zx_panic" | "platform_panic_start" => "panic",
        "size" | "len" => "len",
        "empty" | "is_empty" => "is_empty",
        "min" | "ktl_min" => "min",
        "max" | "ktl_max" => "max",
        "memcpy" | "copy_from_slice" | "copy_nonoverlapping" => "memcpy",
        "memset" | "fill" | "write_bytes" => "memset",
        "push_back" | "push" => "push",
        "kcounter_add" => "add",
        // `ProcessDispatcher::GetCurrent()` and `with_current(|up| ...)`.
        "with_current" => "get_current",
        "pop_back" | "pop" => "pop",
        "roundup" | "round_up" | "align_up" | "next_multiple_of" => "roundup",
        "rounddown" | "round_down" | "align_down" => "rounddown",
        "push_front" => "push_front",
        "pop_front" => "pop_front",
        "reset" | "take" | "swap" => "take",
        "exchange" | "replace" => "replace",
        // Volatile device memory access.
        "mmio_read8" | "mmio_read16" | "mmio_read32" | "mmio_read64" | "readb" | "readw"
        | "readl" | "readq" | "read_volatile" => "mmio_read",
        "mmio_write8" | "mmio_write16" | "mmio_write32" | "mmio_write64" | "writeb" | "writew"
        | "writel" | "writeq" | "write_volatile" => "mmio_write",
        // Zircon's handle lookups: `up->handle_table().GetDispatcherWithRights(...)`
        // and `Dispatcher::get_with_rights::<T>(...)`.
        "get_dispatcher_with_rights" | "get_with_rights" => "get_with_rights",
        "get_dispatcher" => "get_dispatcher",
        // User memory: `out.copy_to_user(v)` and `out.write(v)`.
        "copy_to_user" | "copy_array_to_user" => "write_user",
        "copy_from_user" | "copy_array_from_user" => "read_user",
        "add_overflow" | "checked_add" => "checked_add",
        "sub_overflow" | "checked_sub" => "checked_sub",
        "mul_overflow" | "checked_mul" => "checked_mul",
        "adopt_ref" | "make_ref_counted" => "adopt_ref",
        "formatted_bytes" | "format_size_rs" => "format_size",
        "ispow2" | "is_power_of_two" => "is_power_of_two",
        "create_with_priority" => "create",
        // Memory barriers.
        "mb"
        | "rmb"
        | "wmb"
        | "smp_mb"
        | "smp_rmb"
        | "smp_wmb"
        | "fence"
        | "compiler_fence"
        | "atomic_thread_fence"
        | "atomic_signal_fence" => "fence",
        "release" | "unlock" | "drop" => "release",
        other => other,
    }
}

fn strip_generics(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth = 0usize;
    for c in s.chars() {
        match c {
            '<' => depth += 1,
            '>' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

/// Identifiers that do not help match C++ against Rust.
const NOISE_IDENTS: &[&str] = &[
    "self", "this", "auto", "let", "mut", "const", "unsafe", "ok", "err", "some", "none",
    "nullptr", "null", "true", "false", "status", "result", "zx_ok", "ret", "rc", "res", "guard",
    "usize", "u32", "u64", "size_t", "uint32_t", "uint64_t",
];

/// Normalizes an identifier for similarity purposes; `None` for noise.
pub fn ident_feature(s: &str) -> Option<String> {
    // Google-style constants: `kMaxSize` is `MAX_SIZE` in Rust.
    let mut c = s.chars();
    let screaming =
        s.chars().any(|c| c.is_ascii_uppercase()) && !s.chars().any(|c| c.is_ascii_lowercase());
    let s = match (c.next(), c.next()) {
        (Some('k'), Some(u)) if u.is_ascii_uppercase() => &s[1..],
        // A constant ported by mechanically upper-casing `kMaxSize`.
        (Some('K'), Some('_')) if screaming => &s[2..],
        _ => s,
    };
    let n = ident(s);
    if n.is_empty() || NOISE_IDENTS.contains(&n.as_str()) || n.len() < 2 {
        return None;
    }
    // ksync's lock tokens and guards (`token`, `list_token`, `state_guard`,
    // `LockToken`, `TableWriteTokenGuard`, `FooLockClass`) are
    // plumbing; the lock itself is compared as a lock.
    // An all-caps `TOKEN` is a constant or enum variant, not a lock token.
    if (n == "token" && !screaming)
        || n.ends_with("_token")
        || n.ends_with("_guard")
        || n.ends_with("_lock_class")
    {
        return None;
    }
    Some(n)
}

static ZX_ERR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\bZX_ERR_([A-Z0-9_]+)\b").unwrap());
static PATH_ERR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\b(?:[A-Za-z_]*(?:Status|Error|Err|status|error))\s*::\s*(?:ERR_)?([A-Z][A-Z0-9_]+)\b",
    )
    .unwrap()
});

/// Error codes mentioned in a piece of source, in order, normalized to the
/// bare name (`ZX_ERR_NO_MEMORY` and `Status::NO_MEMORY` both give
/// `NO_MEMORY`).
pub fn error_codes(text: &str) -> Vec<String> {
    let mut found: Vec<(usize, String)> = Vec::new();
    for c in ZX_ERR.captures_iter(text) {
        found.push((c.get(0).unwrap().start(), c[1].to_string()));
    }
    for c in PATH_ERR.captures_iter(text) {
        let code = &c[1];
        if code != "OK" {
            found.push((c.get(0).unwrap().start(), code.to_string()));
        }
    }
    found.sort_by_key(|(pos, _)| *pos);
    found.into_iter().map(|(_, c)| c).collect()
}

/// Whether the text names the success status.
pub fn mentions_ok(text: &str) -> bool {
    static OK: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\bZX_OK\b|\bOk\s*\(|\bzx::ok\s*\(|\bfit::ok\s*\(|::OK\b").unwrap()
    });
    OK.is_match(text)
}

/// Normalizes the expression naming a lock to the name of the lock itself,
/// so `&lock_`, `get_lock()`, `self.lock`, `&handle_table_->lock_` and
/// `table.write_lock()` all give `lock`, and `ThreadLock::Get()` gives
/// `thread_lock`. `method` is the Rust method that acquired it, if any.
pub fn lock_key(receiver: &str, method: Option<&str>) -> String {
    if let Some(m) = method {
        if matches!(m, "read_lock" | "write_lock" | "lock_read" | "lock_write") {
            return "lock".to_string();
        }
        // An accessor that locks a named lock: `self.lock_timer_lock()`.
        // ksync's `#[guarded]` generates `lock_<field>()` and its
        // `_policy` and `_aliased` variants for a mutex field.
        if let Some(name) = m.strip_prefix("lock_") {
            let name = name.trim_end_matches("_policy").trim_end_matches("_aliased");
            if !matches!(name, "irqsave" | "irq" | "read" | "write" | "shared" | "policy") {
                return name.to_string();
            }
        }
    }
    const DROP: &[&str] = &["this", "self", "get", "mut", "ref", "as", "deref", "borrow", "cast"];
    let cleaned: String =
        receiver.chars().map(|c| if c.is_alphanumeric() || c == '_' { c } else { ' ' }).collect();
    for comp in cleaned.split_whitespace().rev() {
        let w: Vec<String> =
            words(comp).into_iter().filter(|w| !DROP.contains(&w.as_str())).collect();
        if !w.is_empty() {
            return w.join("_");
        }
    }
    "lock".to_string()
}

/// Normalizes comment text to a list of words. Comment markers, Rust doc
/// backticks, Zircon `|name|` references and punctuation are dropped, and
/// identifiers are normalized so `CommitRange()` matches `commit_range`.
pub fn comment_words(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let mut l = line.trim();
        for prefix in ["///", "//!", "//", "/**", "/*!", "/*"] {
            if let Some(rest) = l.strip_prefix(prefix) {
                l = rest;
                break;
            }
        }
        let l = l.trim_end_matches("*/").trim();
        let l = l.strip_prefix('*').unwrap_or(l);
        for raw in l.split_whitespace() {
            let w = raw.trim_matches(|c: char| !c.is_alphanumeric() && c != '_');
            if w.is_empty() {
                continue;
            }
            let looks_like_ident = w.contains('_')
                || w.chars().skip(1).any(|c| c.is_ascii_uppercase())
                || raw.contains("()");
            if looks_like_ident {
                // C++ constants are `kMaxTone`; Rust's are `MAX_TONE`.
                let w = match w.strip_prefix('k') {
                    Some(r) if r.starts_with(|c: char| c.is_ascii_uppercase()) => r,
                    _ => w,
                };
                out.push(ident(w).replace('_', ""));
            } else {
                out.push(w.to_lowercase());
            }
        }
    }
    out
}

/// Length of the longest common subsequence of two sequences.
pub fn lcs_len<T: PartialEq>(a: &[T], b: &[T]) -> usize {
    if a.is_empty() || b.is_empty() {
        return 0;
    }
    let mut prev = vec![0usize; b.len() + 1];
    let mut cur = vec![0usize; b.len() + 1];
    for x in a {
        for (j, y) in b.iter().enumerate() {
            cur[j + 1] = if x == y { prev[j] + 1 } else { prev[j + 1].max(cur[j]) };
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Similarity of two word sequences in `[0, 1]`, order-sensitive.
pub fn seq_similarity<T: PartialEq>(a: &[T], b: &[T]) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    2.0 * lcs_len(a, b) as f64 / (a.len() + b.len()) as f64
}

/// Jaccard similarity of two lists treated as sets, in `[0, 1]`.
pub fn jaccard(a: &[String], b: &[String]) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let mut sa: Vec<&String> = a.iter().collect();
    sa.sort();
    sa.dedup();
    let mut union: Vec<&String> = a.iter().chain(b.iter()).collect();
    union.sort();
    union.dedup();
    let inter = sa.iter().filter(|x| b.contains(x)).count();
    inter as f64 / union.len() as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idents() {
        assert_eq!(ident("CommitRange"), "commit_range");
        assert_eq!(ident("commit_range_"), "commit_range");
        assert_eq!(ident("HTTPServer"), "http_server");
        assert_eq!(ident("VmObjectPaged"), "vm_object_paged");
        assert_eq!(ident("kMaxSize"), "k_max_size");
    }

    #[test]
    fn calls() {
        assert_eq!(call("vmo->CommitRangeLocked").as_deref(), Some("commit_range_locked"));
        assert_eq!(call("self.commit_range_locked").as_deref(), Some("commit_range_locked"));
        assert_eq!(call("ktl::move"), None);
        assert_eq!(call("DEBUG_ASSERT").as_deref(), Some("assert"));
        assert_eq!(call("debug_assert!").as_deref(), Some("assert"));
        assert_eq!(call("fbl::AdoptRef<VmObject>").as_deref(), Some("adopt_ref"));
        assert_eq!(
            call("AutoExpiringPreemptDisabler::new").as_deref(),
            Some("auto_expiring_preempt_disabler")
        );
        assert_eq!(call("list.push_front_raw").as_deref(), Some("push_front"));
        assert_eq!(call("guard.as_mut"), None);
        assert_eq!(call("self.flag.load"), None);
        assert_eq!(call("self.flag.store"), None);
        assert_eq!(call("time.saturating_sub"), None);
        assert_eq!(call("zx_time_add_duration"), None);
        assert_eq!(call("pmm_count_free_pages").as_deref(), Some("count_free_pages"));
        assert_eq!(call("pmm::node().count_free_pages").as_deref(), Some("count_free_pages"));
        assert_eq!(call("pretty::format_size_rs").as_deref(), Some("format_size"));
        assert_eq!(call("FormattedBytes").as_deref(), Some("format_size"));
        assert_eq!(call("ispow2").as_deref(), Some("is_power_of_two"));
        assert!(!is_mutating("PressureLevel::try_from"));
    }

    #[test]
    fn errors() {
        assert_eq!(error_codes("return ZX_ERR_INVALID_ARGS;"), vec!["INVALID_ARGS"]);
        assert_eq!(error_codes("return Err(Status::NO_MEMORY);"), vec!["NO_MEMORY"]);
        assert_eq!(error_codes("Err(zx::Status::OUT_OF_RANGE)"), vec!["OUT_OF_RANGE"]);
        assert_eq!(error_codes("Err(ZxError::ERR_BAD_STATE)"), vec!["BAD_STATE"]);
        assert!(error_codes("Ok(())").is_empty());
    }

    #[test]
    fn locks() {
        assert_eq!(lock_key("&lock_", None), "lock");
        assert_eq!(lock_key("self.lock", None), "lock");
        assert_eq!(lock_key("get_lock()", None), "lock");
        assert_eq!(lock_key("ThreadLock::Get()", None), "thread_lock");
        assert_eq!(lock_key("&self.page_lock", None), "page_lock");
        assert_eq!(lock_key("&handle_table_->lock_", None), "lock");
        assert_eq!(lock_key("up->handle_table().get_lock()", None), "lock");
        // ksync accessors name the lock field.
        assert_eq!(lock_key("self", Some("lock_mu")), "mu");
        assert_eq!(lock_key("self", Some("lock_mu_policy")), "mu");
        assert_eq!(lock_key("thread.cast()", None), "thread");
        assert_eq!(lock_key("&self.inner.get()", None), "inner");
        assert_eq!(lock_key("&mu_", None), "mu");
        assert_eq!(lock_key("self.spare_list_lock", Some("lock")), "spare_list_lock");
        assert_eq!(lock_key("&spare_list_lock_", None), "spare_list_lock");
        assert_eq!(lock_key("table", Some("write_lock")), "lock");
    }

    #[test]
    fn comments() {
        assert_eq!(
            comment_words("// Call |CommitRange()| with the lock held."),
            comment_words("/// Call `commit_range` with the lock held.")
        );
    }
}
