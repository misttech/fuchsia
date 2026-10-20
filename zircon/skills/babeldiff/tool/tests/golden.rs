//! End-to-end tests over the fixtures in `tests/fixtures`.
//!
//! Each test renders a report and compares it with an `expected*.txt` file
//! next to the fixture. Run with `BLESS=1` to rewrite the expected output
//! after an intentional change, and review the diff.

use babeldiff::analyze::{CppOrigin, Link, NoFinder, Options, Report};
use babeldiff::check::{Category, Severity};
use babeldiff::git::{Git, RepoFinder};
use babeldiff::input::ChangeSet;
use babeldiff::render::{Layout, RenderOptions, render};
use std::path::{Path, PathBuf};
use std::process::Command;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

fn check_golden(path: &Path, actual: &str) {
    if std::env::var_os("BLESS").is_some() {
        std::fs::write(path, actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(path)
        .unwrap_or_else(|_| panic!("missing {}; run with BLESS=1", path.display()));
    if expected != actual {
        let first = expected
            .lines()
            .zip(actual.lines())
            .position(|(a, b)| a != b)
            .unwrap_or(expected.lines().count().min(actual.lines().count()));
        panic!(
            "{} differs from the actual output starting at line {}:\n  expected: {:?}\n  actual:   {:?}\nRun with BLESS=1 to update.",
            path.display(),
            first + 1,
            expected.lines().nth(first),
            actual.lines().nth(first)
        );
    }
}

fn opts(layout: Layout) -> RenderOptions {
    RenderOptions { layout, width: 160, context: None, summary_only: false }
}

/// A synthetic port with no planted differences, read from a patch.
fn beacon_report() -> Report {
    let text = std::fs::read_to_string(fixture("beacon/beacon.patch")).unwrap();
    let files = babeldiff::patch::parse(&text);
    let cs = ChangeSet::from_patch(&files, &mut |_| None);
    babeldiff::run(&cs, &Options::default(), &mut NoFinder)
}

#[test]
fn beacon_golden() {
    let report = beacon_report();
    check_golden(&fixture("beacon/expected.txt"), &render(&report, &opts(Layout::SideBySide)));
}

#[test]
fn beacon_follows_ffi_shims_without_false_positives() {
    let report = beacon_report();
    let pair = report
        .pairs
        .iter()
        .find(|p| p.cpp.name == "BeaconDispatcher::Subscribe")
        .expect("Subscribe is paired");
    assert_eq!(pair.rust.name, "BeaconDispatcher::subscribe");
    assert!(
        matches!(&pair.link, Link::Ffi { shim, .. } if shim == "rust_beacon_dispatcher_subscribe")
    );
    // A faithful port: every function pairs and none has an issue.
    assert_eq!(report.pairs.len(), 7);
    for p in &report.pairs {
        assert_eq!(p.issues(), 0, "{}: {:#?}", p.cpp.name, p.findings);
        assert_eq!(p.summary.cpp_errors, p.summary.rust_errors, "{}", p.cpp.name);
    }
    assert!(report.unmatched_cpp.is_empty());
    assert!(report.unmatched_rust.is_empty());
    // Safety comments and `# Safety` docs are expected in Rust: no finding.
    for name in ["BeaconDispatcher::FindLocked", "BeaconDispatcher::GetSubscriber"] {
        let p = report.pairs.iter().find(|p| p.cpp.name == name).unwrap();
        let safety: Vec<usize> =
            p.rust.units.iter().filter(|u| u.features.safety).map(|u| u.start_line).collect();
        assert!(!safety.is_empty(), "{name}");
        for f in &p.findings {
            assert!(f.rust_line.is_none_or(|l| !safety.contains(&l)), "{name}: {f:?}");
        }
    }
    // ksync token plumbing has no C++ counterpart and is not a finding, and
    // the lock is still compared as a lock.
    let fc = report.pairs.iter().find(|p| p.cpp.name == "BeaconDispatcher::FlashCount").unwrap();
    assert!(fc.rust.units.iter().any(|u| u.features.lock_plumbing));
    assert!(
        fc.findings.iter().all(|f| f.severity == Severity::Note && f.category == Category::Comment)
    );
    assert_eq!(fc.summary.cpp_locks, fc.summary.rust_locks);
    // Shims are reported as shims, not as unpaired Rust.
    assert!(report.shims.iter().any(|s| s.shim.name == "rust_beacon_dispatcher_flash"
        && s.target.as_deref() == Some("BeaconDispatcher::flash")));
}

/// Builds a git repository with the fixture's `before` and `after` trees as
/// two commits.
fn two_commit_repo(fixture_name: &str, name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["-c", "user.name=test", "-c", "user.email=test@example.com"])
            .args(args)
            .output()
            .expect("git is installed");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    let copy = |from: &Path| {
        for entry in walk(from) {
            let rel = entry.strip_prefix(from).unwrap();
            let to = dir.join(rel);
            std::fs::create_dir_all(to.parent().unwrap()).unwrap();
            std::fs::copy(&entry, &to).unwrap();
        }
    };
    git(&["init", "-q"]);
    copy(&fixture(&format!("{fixture_name}/before")));
    git(&["add", "-A"]);
    git(&["commit", "-qm", "before"]);
    std::fs::remove_dir_all(dir.join("zircon")).unwrap();
    copy(&fixture(&format!("{fixture_name}/after")));
    git(&["add", "-A"]);
    git(&["commit", "-qm", "after"]);
    dir
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out.sort();
    out
}

fn git_report(fixture_name: &str, name: &str) -> Report {
    let repo = two_commit_repo(fixture_name, name);
    let git = Git::new(&repo);
    let (base, head) = Git::range("HEAD");
    let cs = git.changeset(&base, &head).unwrap();
    let mut finder = RepoFinder::new(git, base);
    let report = babeldiff::run(&cs, &Options::default(), &mut finder);
    let _ = std::fs::remove_dir_all(&repo);
    report
}

fn fifo_report(name: &str) -> Report {
    git_report("fifo", name)
}

#[test]
fn fifo_golden() {
    let report = fifo_report("fifo-golden");
    check_golden(&fixture("fifo/expected.txt"), &render(&report, &opts(Layout::SideBySide)));
    check_golden(&fixture("fifo/expected-stacked.txt"), &render(&report, &opts(Layout::Stacked)));
}

#[test]
fn fifo_finds_planted_differences() {
    let report = fifo_report("fifo-planted");
    let pair = |name: &str| report.pairs.iter().find(|p| p.cpp.name == name).unwrap();
    let has = |name: &str, needle: &str| {
        pair(name)
            .findings
            .iter()
            .any(|f| f.severity == Severity::Issue && f.message.contains(needle))
    };
    // A different error code.
    assert!(has(
        "FifoDispatcher::WriteFromUser",
        "C++ returns PEER_CLOSED, Rust returns BAD_STATE"
    ));
    // A rollback path replaced by `?`.
    assert!(has(
        "FifoDispatcher::WriteSelfLocked",
        "C++ handles this call's error in its own branch, but Rust propagates it"
    ));
    assert!(has("FifoDispatcher::WriteSelfLocked", "only in C++"));
    // The lock is taken before the argument checks in Rust, after in C++.
    assert!(has("FifoDispatcher::ReadToUser", "order may differ"));
    // Comments carried over from the header's declaration comments.
    assert_eq!(pair("FifoDispatcher::WriteFromUser").summary.comments_same, 2);
    // C++ the change left alone is found in the repository.
    let full = pair("FifoDispatcher::IsFullLocked");
    assert_eq!(full.origin, CppOrigin::Unchanged);
    assert_eq!(full.rust.name, "FifoDispatcher::is_full_locked");
    assert_eq!(full.issues(), 0);
}

#[test]
fn cli_exit_status_reflects_issues() {
    let bin = env!("CARGO_BIN_EXE_babeldiff");
    let before = fixture("fifo/before/zircon/kernel/object/fifo_dispatcher.cc");
    let after = fixture("fifo/after/zircon/kernel/object/fifo_dispatcher.rs");
    let out =
        Command::new(bin).args(["--summary", "files"]).arg(&before).arg(&after).output().unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("FifoDispatcher::WriteSelfLocked  <->  FifoDispatcher::write_self_locked")
    );

    let same = Command::new(bin).args(["files"]).arg(&before).arg(&before).output().unwrap();
    assert_eq!(same.status.code(), Some(0));
}

#[test]
fn html_report_is_self_contained() {
    use babeldiff::html::{HtmlOptions, render_html};
    let report = fifo_report("fifo-html");
    let html = render_html(&report, &HtmlOptions { title: "fifo <test>".into() });
    assert!(html.starts_with("<!doctype html>"));
    assert!(html.contains("<title>babeldiff: fifo &lt;test&gt;</title>"));
    // Nothing is loaded from elsewhere.
    for needle in ["<link", "src=", "http://", "https://", "@import", "url("] {
        assert!(!html.contains(needle), "found {needle:?}");
    }
    // One section per pair, and a row for the planted error-code change.
    assert_eq!(html.matches("<section class=\"pair ").count(), report.pairs.len());
    assert!(html.contains("error code differs: C++ returns PEER_CLOSED, Rust returns BAD_STATE"));
    assert!(html.contains("data-k=\"e:PEER_CLOSED\""));
    assert!(html.contains("data-k=\"e:BAD_STATE\""));
    // Runs of equivalent rows fold, and functions can be filtered by what
    // they carry.
    assert!(html.contains("class=\"fold\" title=\"Show these rows\">"));
    assert!(html.contains("<option value=\"findings\">functions with issues or notes</option>"));
    // Source text is escaped.
    assert!(!html.contains("<const"));
    assert!(html.contains("&lt;<span class=\"kw\">const</span>"));
    // Each code block names its files and functions, so that a line
    // comment can say where it points.
    let p = &report.pairs[0];
    assert!(html.contains(&format!(
        "<div class=\"code\" data-cp=\"{}\" data-rp=\"{}\"",
        p.cpp.path, p.rust.path
    )));
    assert!(html.contains("id=\"comments-text\""));
}

#[test]
fn cli_writes_html() {
    let bin = env!("CARGO_BIN_EXE_babeldiff");
    let out_path =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("beacon-{}.html", std::process::id()));
    let out = Command::new(bin)
        .args(["patch", "--format", "html", "-o"])
        .arg(&out_path)
        .arg(fixture("beacon/beacon.patch"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let html = std::fs::read_to_string(&out_path).unwrap();
    let _ = std::fs::remove_file(&out_path);
    assert!(html.contains(
        "<title>babeldiff: [kernel] Port BeaconDispatcher subscriptions to Rust</title>"
    ));
    assert_eq!(html.matches("<section class=\"pair ").count(), 7);
}

/// A dispatcher hierarchy folded into one Rust type with an enum, two
/// same-named `create` functions behind shims, syscalls with a handle
/// lookup, status chaining and a status set in each branch, four planted
/// mistakes in the Rust, and one planted change to C++ that stays C++.
fn doorbell_report(name: &str) -> Report {
    git_report("doorbell", name)
}

#[test]
fn doorbell_golden() {
    let report = doorbell_report("doorbell-golden");
    check_golden(&fixture("doorbell/expected.txt"), &render(&report, &opts(Layout::Stacked)));
}

#[test]
fn doorbell_finds_exactly_the_planted_mistakes() {
    let report = doorbell_report("doorbell-planted");
    let pair = |name: &str| report.pairs.iter().find(|p| p.cpp.name == name).unwrap();

    // Each static Create pairs with its own Rust `create`, told apart by the
    // type path the shim calls.
    assert_eq!(
        pair("ChimeDoorbellDispatcher::Create").rust.name,
        "ChimeDoorbellDispatcher::create"
    );
    assert_eq!(
        pair("BuzzerDoorbellDispatcher::Create").rust.name,
        "BuzzerDoorbellDispatcher::create"
    );
    // Both overrides of Ring fold into the one Rust `ring`.
    let ring = pair("ChimeDoorbellDispatcher::Ring");
    assert_eq!(ring.rust.name, "DoorbellDispatcher::ring");
    let ovs: Vec<&str> = ring.overrides.iter().map(|o| o.cpp.name.as_str()).collect();
    assert_eq!(ovs, ["BuzzerDoorbellDispatcher::Ring"]);
    assert!(report.unmatched_cpp.is_empty());
    assert!(report.unmatched_rust.is_empty());

    let mut issues: Vec<String> = report
        .pairs
        .iter()
        .flat_map(|p| p.all_findings())
        .filter(|f| f.severity == Severity::Issue)
        .map(|f| format!("{} {}", f.category.name(), f.message))
        .collect();
    issues.sort();
    assert_eq!(
        issues,
        [
            "comment comment only in C++",
            "control-flow Rust's condition adds a test that C++ doesn't make: tone == 0",
            "error-path error codes differ: C++ [NO_MEMORY], Rust [NO_RESOURCES]",
            "trace trace/print statement only in C++",
        ]
    );
    // The dropped comment is the Buzzer override's.
    let lost =
        ring.overrides[0].findings.iter().find(|f| f.message == "comment only in C++").unwrap();
    assert_eq!(
        ring.overrides[0].cpp.line(lost.cpp_line.unwrap()).trim(),
        "// A buzzer rings once, however long it buzzes."
    );
    // Status chaining in the syscall is `?` in Rust.
    assert_eq!(pair("sys_doorbell_ring").issues(), 0);
    assert_eq!(pair("sys_doorbell_create").issues(), 0);

    // The one C++ change that stays C++ is listed; the forwarders into Rust
    // and the FFI declarations are not.
    let changes: Vec<(usize, &str)> =
        report.cpp_changes.iter().map(|c| (c.start_line, c.text.as_str())).collect();
    assert_eq!(
        changes,
        [(
            24,
            "ChimeDoorbellDispatcher::ChimeDoorbellDispatcher(uint32_t notes) : notes_(notes + 1) {}"
        )]
    );

    // One of each rubric lint is planted: an `extern "C"` parameter of the
    // wrong width, an unsafe block without a SAFETY comment, and a C++ FFI
    // helper that branches.
    let lints: Vec<(&str, usize, &str)> = report
        .lints
        .iter()
        .map(|l| (l.path.rsplit('/').next().unwrap(), l.line, l.kind.name()))
        .collect();
    assert_eq!(
        lints,
        [
            ("doorbell_dispatcher_ffi.cc", 17, "shim-logic"),
            ("doorbell_dispatcher_ffi.rs", 15, "extern-signature"),
            ("doorbell_dispatcher_ffi.rs", 61, "unsafe-safety"),
        ]
    );
    assert_eq!(report.lint_issues(), 3);
}

#[test]
fn json_lists_findings_with_locations() {
    let report = doorbell_report("doorbell-json");
    let json = babeldiff::json::render_json(&report, "doorbell");
    assert!(json.starts_with("{\"version\":1,\"title\":\"doorbell\""));
    assert!(json.contains("\"summary\":{\"pairs\":7,\"issues\":4,"));
    assert!(json.contains(
        "\"severity\":\"issue\",\"category\":\"error-path\",\"rubric\":\"behavioral parity of error paths\",\"message\":\"error codes differ: C++ [NO_MEMORY], Rust [NO_RESOURCES]\""
    ));
    assert!(json.contains("\"override\":\"BuzzerDoorbellDispatcher::Ring\""));
    assert!(json.contains("\"lint_issues\":3,\"lint_notes\":0"));
    assert!(json.contains("{\"kind\":\"extern-signature\",\"severity\":\"issue\",\"rubric\":\"FFI declarations match on both sides\",\"message\":\"cpp_doorbell_dispatcher_log: parameter 2: C++ `uint32_t kind`, Rust `u64` (4-byte vs 8-byte value)\",\"location\":{\"path\":\"zircon/kernel/object/doorbell_dispatcher_ffi.rs\",\"line\":15},\"related\":{\"path\":\"zircon/kernel/object/doorbell_dispatcher_ffi.cc\",\"line\":15}}"));
    assert!(json.contains("\"path\":\"zircon/kernel/object/doorbell_dispatcher.rs\",\"line\":28,\"text\":\"if tone == 0 || tone > MAX_TONE {\""));
}

#[test]
fn issues_only_drops_notes_and_clean_pairs() {
    let mut report = doorbell_report("doorbell-issues");
    report.retain_issues();
    assert_eq!(report.notes(), 0);
    assert_eq!(report.issues(), 4);
    assert!(report.pairs.iter().all(|p| p.issues() > 0));
    assert_eq!(report.pairs.len(), 2);
}

fn lantern_report(name: &str) -> Report {
    git_report("lantern", name)
}

#[test]
fn lantern_golden() {
    let report = lantern_report("lantern-golden");
    check_golden(&fixture("lantern/expected.txt"), &render(&report, &opts(Layout::Stacked)));
}

/// The lantern fixture plants the mistakes found reviewing a large
/// conversion by hand: a second copy of a function that drops a mask, a
/// constant the C++ never used, a mask the Rust adds, a macro and a helper
/// expanded inline, a call moved out of `DEBUG_ASSERT`, `ASSERT(false)`
/// turned into an error, an added check, a function in an unrelated file,
/// a reference with an invented lifetime and "Ported from" comments.
#[test]
fn lantern_finds_the_planted_mistakes() {
    let report = lantern_report("lantern-planted");
    let mut issues: Vec<String> = report
        .pairs
        .iter()
        .flat_map(|p| p.all_findings())
        .filter(|f| f.severity == Severity::Issue)
        .map(|f| format!("{} {}", f.category.name(), f.message))
        .collect();
    issues.sort();
    assert_eq!(
        issues,
        [
            "assert C++ calls is_lit_locked only inside DEBUG_ASSERT, so only in debug builds; the Rust calls it unconditionally at line 56",
            "assert C++ panics here (ASSERT(false)); the Rust returns NOT_SUPPORTED instead (line 73)",
            "call C++ calls the helper copy_color at 3 places (lines 44, 45, 46); the Rust never calls it, so its code is repeated inline",
            "call C++ macro COPY_WICKS is expanded inline in the Rust; keep it as a macro (macro_rules!) and use it where the C++ does",
            "call C++ macro COPY_WICKS is expanded inline in the Rust; keep it as a macro (macro_rules!) and use it where the C++ does",
            "error-path Rust adds a check `lantern.is_null() || out.is_null()`, returning INVALID_ARGS, that the C++ doesn't make",
            "pairing the change defines read_brightness twice, here and at zircon/kernel/arch/toy/src/lantern.rs:8, and the copies differ; both are compared with the C++, and callers may reach either",
            "value C++ sets LANTERN_BRIGHT_MASK here, and the Rust doesn't",
            "value Rust also clears LANTERN_FLAGS_RESUME, which the C++ doesn't",
            "value only Rust uses LANTERN_OFF_MASK (0x700); the C++ function never mentions it",
        ]
    );
    // Both copies of read_brightness are compared with the C++.
    let copies: Vec<&str> = report
        .pairs
        .iter()
        .filter(|p| p.cpp.name == "read_brightness")
        .map(|p| p.rust.path.rsplit('/').next().unwrap())
        .collect();
    assert_eq!(copies, ["lantern.rs", "glow.rs"]);
    // The lock the closure runs under lines up with the C++ guard.
    let state = report.pairs.iter().find(|p| p.cpp.name == "lantern_get_state").unwrap();
    assert_eq!(state.summary.cpp_locks, state.summary.rust_locks);

    let lints: Vec<(&str, usize, &str)> = report
        .lints
        .iter()
        .map(|l| (l.path.rsplit('/').next().unwrap(), l.line, l.kind.name()))
        .collect();
    assert_eq!(
        lints,
        [
            ("lantern.cc", 1, "file-placement"),
            ("glow.rs", 31, "invented-lifetime"),
            ("lantern.rs", 4, "provenance-comment"),
            ("lantern.rs", 82, "stale-cpp-reference"),
            ("lantern.rs", 85, "stale-cpp-reference"),
        ]
    );
    let placement: Vec<(&str, Vec<(&str, bool)>)> = report
        .placement
        .iter()
        .map(|p| {
            (
                p.cpp_path.rsplit('/').next().unwrap(),
                p.targets
                    .iter()
                    .map(|t| (t.rust_path.rsplit('/').next().unwrap(), t.expected))
                    .collect(),
            )
        })
        .collect();
    assert_eq!(placement, [("lantern.cc", vec![("lantern.rs", true), ("glow.rs", false)])]);
}

// The lamplog fixture trims the kernel crashlog tests: each C++ test makes a
// buffer with `make_unique` and `ASSERT_TRUE(ac.check())`, and each Rust test
// with `unwrap_some!(try_new_zeroed_slice(..).ok())`, except a planted one
// that returns on failure instead of asserting.
#[test]
fn lamplog_golden() {
    let report = git_report("lamplog", "lamplog-golden");
    check_golden(&fixture("lamplog/expected.txt"), &render(&report, &opts(Layout::Stacked)));

    let pair = |name: &str| report.pairs.iter().find(|p| p.cpp.name == name).unwrap();
    let alloc_notes = |name: &str| {
        pair(name).findings.iter().filter(|f| f.message.contains("`ac.check()`")).count()
    };
    // The C++ allocation and check and the Rust allocation are notes.
    for name in ["NoGlowTest", "FlickerTest"] {
        assert_eq!(pair(name).issues(), 0, "{name}: {:#?}", pair(name).findings);
        assert_eq!(alloc_notes(name), 3, "{name}: {:#?}", pair(name).findings);
    }
    // The Rust that returns on failure stays reported.
    let dim = pair("DimTest");
    assert!(
        dim.findings
            .iter()
            .any(|f| f.severity == Severity::Issue && f.message.contains("make_unique")),
        "{:#?}",
        dim.findings
    );
    assert_eq!(alloc_notes("DimTest"), 0, "{:#?}", dim.findings);
}

#[test]
fn chain_lock_callback_lines_up_with_a_guard() {
    let cpp = "zx_status_t lamp_get(Lamp* lamp, uint32_t* out) {\n  SingleChainLockGuard guard{IrqSaveOption, lamp->get_lock(), CLT_TAG(\"lamp_get\")};\n  // Only a lit lamp has a color.\n  if (!lamp->lit()) {\n    return ZX_ERR_BAD_STATE;\n  }\n  *out = lamp->color();\n  return ZX_OK;\n}\n";
    let rust = "pub unsafe fn lamp_get(lamp: *mut Lamp, out: &mut u32) -> zx_status_t {\n    // SAFETY: `lamp` is valid.\n    unsafe {\n        lamp::with_chain_lock(lamp, |lamp| {\n            // Only a lit lamp has a color.\n            if !lamp::lit(lamp) {\n                return ZX_ERR_BAD_STATE;\n            }\n            *out = lamp::color(lamp);\n            ZX_OK\n        })\n    }\n}\n";
    let cs =
        ChangeSet::from_files(&[("lamp.cc".into(), cpp.into()), ("lamp.rs".into(), rust.into())]);
    let report = babeldiff::run(&cs, &Options::default(), &mut NoFinder);
    assert_eq!(report.pairs.len(), 1);
    let p = &report.pairs[0];
    let issues: Vec<&str> = p
        .findings
        .iter()
        .filter(|f| f.severity == Severity::Issue)
        .map(|f| f.message.as_str())
        .collect();
    assert!(issues.is_empty(), "{issues:?}");
    assert_eq!(p.summary.cpp_locks, p.summary.rust_locks);
}

#[test]
fn unchanged_cpp_comes_from_the_same_architecture() {
    // Rust under arch/x86 with no removed C++ must not be paired with a
    // same-named method under arch/riscv64.
    let report = git_report("compass", "compass");
    let names: Vec<(&str, &str)> =
        report.pairs.iter().map(|p| (p.cpp.path.as_str(), p.rust.base.as_str())).collect();
    assert!(names.contains(&("zircon/kernel/arch/x86/compass.cc", "heading")), "{names:?}");
    assert!(!names.iter().any(|(c, _)| c.contains("riscv64")), "{names:?}");
}

fn only_issues(cpp: &str, rust: &str) -> Vec<String> {
    let cs =
        ChangeSet::from_files(&[("lamp.cc".into(), cpp.into()), ("lamp.rs".into(), rust.into())]);
    let report = babeldiff::run(&cs, &Options::default(), &mut NoFinder);
    assert_eq!(report.pairs.len(), 1);
    report.pairs[0]
        .findings
        .iter()
        .filter(|f| f.severity == Severity::Issue)
        .map(|f| f.message.clone())
        .collect()
}

const LAMP_ON_CPP: &str = "void lamp_on(Lamp* lamp) {\n  lamp->power(true);\n#if __has_feature(safe_stack)\n  lamp->reset_shadow(lamp->shadow_top());\n#else\n  lamp->reset();\n#endif\n  lamp->glow();\n}\n";

#[test]
fn conditional_compilation_must_stay_conditional() {
    // The `#if` became a run-time stub, so the guarded code always runs
    // and the `#else` branch is gone.
    let stub = "pub fn lamp_on(lamp: &mut Lamp) {\n    lamp.power(true);\n    let _ = has_feature(\"safe_stack\");\n    lamp.reset_shadow(lamp.shadow_top());\n    lamp.glow();\n}\n";
    let issues = only_issues(LAMP_ON_CPP, stub);
    assert!(
        issues.iter().any(|m| m
            .starts_with("C++ compiles line 4 only when `#if __has_feature(safe_stack)`")
            && m.contains("Rust line 3")),
        "{issues:?}"
    );

    // A `#[cfg]` on each branch keeps it conditional.
    let cfg = "pub fn lamp_on(lamp: &mut Lamp) {\n    lamp.power(true);\n    #[cfg(sanitize = \"safestack\")]\n    lamp.reset_shadow(lamp.shadow_top());\n    #[cfg(not(sanitize = \"safestack\"))]\n    lamp.reset();\n    lamp.glow();\n}\n";
    let issues = only_issues(LAMP_ON_CPP, cfg);
    assert!(issues.is_empty(), "{issues:?}");
}

fn all_findings(cpp: &str, rust: &str) -> Vec<String> {
    let cs =
        ChangeSet::from_files(&[("lamp.cc".into(), cpp.into()), ("lamp.rs".into(), rust.into())]);
    let report = babeldiff::run(&cs, &Options::default(), &mut NoFinder);
    assert_eq!(report.pairs.len(), 1);
    report.pairs[0].findings.iter().map(|f| f.message.clone()).collect()
}

#[test]
fn static_assert_matches_a_const_assert() {
    let cpp = "int lamp_pages(Lamp* lamp) {\n  static_assert(kLampSize == 3 * kPageSize);\n  return lamp->pages();\n}\n";
    let rust = "pub fn lamp_pages(lamp: &Lamp) -> i32 {\n    const {\n        assert!(LAMP_SIZE == 3 * PAGE_SIZE);\n    }\n    lamp.pages()\n}\n";
    let found = all_findings(cpp, rust);
    assert!(!found.iter().any(|m| m.contains("assert")), "{found:?}");

    // Dropping the assertion is still reported.
    let dropped = "pub fn lamp_pages(lamp: &Lamp) -> i32 {\n    lamp.pages()\n}\n";
    let found = all_findings(cpp, dropped);
    assert!(found.iter().any(|m| m.contains("assert")), "{found:?}");
}

#[test]
fn is_err_early_return_matches_cpp_is_error_check() {
    let cpp = "zx::result<> lamp_init(Lamp& lamp) {\n  auto result = lamp.SetPower(kOn, 0);\n  if (result.is_error()) {\n    return result;\n  }\n  result = lamp.SetColor(kWhite);\n  if (result.is_error()) {\n    return result;\n  }\n  return zx::ok();\n}\n";
    let rust = "pub fn lamp_init(lamp: &mut Lamp) -> Result<(), Status> {\n    let mut result = lamp.set_power(ON, 0);\n    if result.is_err() {\n        return result;\n    }\n    result = lamp.set_color(WHITE);\n    if result.is_err() {\n        return result;\n    }\n    Ok(())\n}\n";
    let found = all_findings(cpp, rust);
    assert!(found.is_empty(), "{found:?}");
}

// An `fbl::AllocChecker` allocation and its `ac.check()` match a Rust `try_*`
// allocation handled in one statement, unless the failure handling differs.
#[test]
fn alloc_checker_matches_fallible_allocation() {
    let cpp_test = concat!(
        "bool lamp_buffer_test() {\n",
        "  fbl::AllocChecker ac;\n",
        "  auto buffer = ktl::make_unique<char[]>(&ac, kLampSize);\n",
        "  ASSERT_TRUE(ac.check());\n",
        "  lamp_fill(buffer.get(), kLampSize);\n",
        "  return true;\n",
        "}\n",
    );
    let rust_test = concat!(
        "fn lamp_buffer_test() -> bool {\n",
        "    let mut buffer =\n",
        "        unwrap_some!(kalloc::Box::<[u8]>::try_new_zeroed_slice(LAMP_SIZE).ok());\n",
        "    lamp_fill(&mut buffer, LAMP_SIZE);\n",
        "    true\n",
        "}\n",
    );
    let issues = only_issues(cpp_test, rust_test);
    assert!(issues.is_empty(), "{issues:?}");

    let cpp_load = concat!(
        "zx_status_t lamp_load(Lamp* lamp, size_t size) {\n",
        "  fbl::AllocChecker ac;\n",
        "  auto buffer = ktl::unique_ptr<char[]>(new (&ac) char[size]);\n",
        "  if (!ac.check()) {\n",
        "    printf(\"lamp: no memory\\n\");\n",
        "    return ZX_ERR_NO_MEMORY;\n",
        "  }\n",
        "  return lamp->Load(buffer.get(), size);\n",
        "}\n",
    );
    let rust_load = concat!(
        "pub fn lamp_load(lamp: &mut Lamp, size: usize) -> Result<(), Status> {\n",
        "    let Ok(mut buffer) = kalloc::Box::<[u8]>::try_new_zeroed_slice(size) else {\n",
        "        kprintln!(\"lamp: no memory\");\n",
        "        return Err(Status::NO_MEMORY);\n",
        "    };\n",
        "    lamp.load(&mut buffer, size)\n",
        "}\n",
    );
    let issues = only_issues(cpp_load, rust_load);
    assert!(issues.is_empty(), "{issues:?}");

    // The C++ asserts on failure, but the Rust returns.
    let returns = concat!(
        "fn lamp_buffer_test() -> bool {\n",
        "    let Ok(mut buffer) = kalloc::Box::<[u8]>::try_new_zeroed_slice(LAMP_SIZE) else {\n",
        "        return false;\n",
        "    };\n",
        "    lamp_fill(&mut buffer, LAMP_SIZE);\n",
        "    true\n",
        "}\n",
    );
    let issues = only_issues(cpp_test, returns);
    assert!(issues.iter().any(|m| m.contains("make_unique")), "{issues:?}");

    // The C++ returns an error on failure, but the Rust panics.
    let panics = concat!(
        "pub fn lamp_load(lamp: &mut Lamp, size: usize) -> Result<(), Status> {\n",
        "    let mut buffer = kalloc::Box::<[u8]>::try_new_zeroed_slice(size).unwrap();\n",
        "    lamp.load(&mut buffer, size)\n",
        "}\n",
    );
    let issues = only_issues(cpp_load, panics);
    assert!(issues.iter().any(|m| m.contains("unique_ptr")), "{issues:?}");

    // The C++ never checks the allocation.
    let unchecked = cpp_test.replace("  ASSERT_TRUE(ac.check());\n", "");
    let issues = only_issues(&unchecked, rust_test);
    assert!(issues.iter().any(|m| m.contains("make_unique")), "{issues:?}");

    // The Rust allocates only `b`: the missing `a` stays reported.
    let cpp_pair = concat!(
        "zx_status_t lamp_pair(Lamp* lamp) {\n",
        "  fbl::AllocChecker ac;\n",
        "  auto a = ktl::make_unique<Bulb>(&ac);\n",
        "  if (!ac.check()) {\n",
        "    return ZX_ERR_NO_MEMORY;\n",
        "  }\n",
        "  auto b = ktl::make_unique<Bulb>(&ac);\n",
        "  if (!ac.check()) {\n",
        "    return ZX_ERR_NO_MEMORY;\n",
        "  }\n",
        "  return lamp->Pair(ktl::move(a), ktl::move(b));\n",
        "}\n",
    );
    let rust_pair = concat!(
        "pub fn lamp_pair(lamp: &mut Lamp) -> Result<(), Status> {\n",
        "    let Ok(b) = kalloc::Box::try_new(Bulb::new()) else {\n",
        "        return Err(Status::NO_MEMORY);\n",
        "    };\n",
        "    lamp.pair(b)\n",
        "}\n",
    );
    let issues = only_issues(cpp_pair, rust_pair);
    assert!(issues.len() == 1 && issues[0].contains("make_unique"), "{issues:?}");

    // The C++ `if` runs when the allocation succeeds.
    let succeeded = cpp_load.replace("!ac.check()", "ac.check()");
    let issues = only_issues(&succeeded, rust_load);
    assert!(issues.iter().any(|m| m.contains("unique_ptr")), "{issues:?}");

    // The Rust `if let Ok` body runs when the allocation succeeds.
    let if_ok = concat!(
        "pub fn lamp_load(lamp: &mut Lamp, size: usize) -> Result<(), Status> {\n",
        "    if let Ok(mut buffer) = kalloc::Box::<[u8]>::try_new_zeroed_slice(size) {\n",
        "        lamp.load(&mut buffer, size)?;\n",
        "        return Err(Status::NO_MEMORY);\n",
        "    }\n",
        "    Ok(())\n",
        "}\n",
    );
    let issues = only_issues(cpp_load, if_ok);
    assert!(issues.iter().any(|m| m.contains("unique_ptr")), "{issues:?}");

    // An `unwrap()` inside the arguments does not hide the `?` on the result.
    let converted = concat!(
        "pub fn lamp_load(lamp: &mut Lamp, size: u64) -> Result<(), Status> {\n",
        "    let mut buffer =\n",
        "        kalloc::Box::<[u8]>::try_new_zeroed_slice(size.try_into().unwrap())\n",
        "            .map_err(|_| Status::NO_MEMORY)?;\n",
        "    lamp.load(&mut buffer, size)\n",
        "}\n",
    );
    let issues = only_issues(cpp_load, converted);
    assert!(!issues.iter().any(|m| m.contains("try_new_zeroed_slice")), "{issues:?}");

    // `fbl::MakeRefCountedChecked` allocates through the checker too.
    let cpp_create = concat!(
        "zx_status_t lamp_create(Lamp* lamp) {\n",
        "  fbl::AllocChecker ac;\n",
        "  auto bulb = fbl::MakeRefCountedChecked<Bulb>(&ac, kBulbWatts);\n",
        "  if (!ac.check()) {\n",
        "    printf(\"lamp: no memory\\n\");\n",
        "    return ZX_ERR_NO_MEMORY;\n",
        "  }\n",
        "  return lamp->Install(ktl::move(bulb));\n",
        "}\n",
    );
    let rust_create = concat!(
        "pub fn lamp_create(lamp: &mut Lamp) -> Result<(), Status> {\n",
        "    let Ok(bulb) = kalloc::Arc::try_new(Bulb::new(BULB_WATTS)) else {\n",
        "        kprintln!(\"lamp: no memory\");\n",
        "        return Err(Status::NO_MEMORY);\n",
        "    };\n",
        "    lamp.install(bulb)\n",
        "}\n",
    );
    let issues = only_issues(cpp_create, rust_create);
    assert!(issues.is_empty(), "{issues:?}");

    // An `if (!ac.check())` that only returns folds into the allocation.
    let cpp_folded = concat!(
        "zx_status_t lamp_load(Lamp* lamp, size_t size) {\n",
        "  fbl::AllocChecker ac;\n",
        "  auto buffer = ktl::make_unique<char[]>(&ac, size);\n",
        "  if (!ac.check()) {\n",
        "    return ZX_ERR_NO_MEMORY;\n",
        "  }\n",
        "  return lamp->Load(buffer.get(), size);\n",
        "}\n",
    );
    let rust_folded = concat!(
        "pub fn lamp_load(lamp: &mut Lamp, size: usize) -> Result<(), Status> {\n",
        "    let Ok(mut buffer) = kalloc::Box::<[u8]>::try_new_uninit_slice(size) else {\n",
        "        return Err(Status::NO_MEMORY);\n",
        "    };\n",
        "    lamp.load(&mut buffer, size)\n",
        "}\n",
    );
    let issues = only_issues(cpp_folded, rust_folded);
    assert!(issues.is_empty(), "{issues:?}");

    // The folded C++ returns on failure, but the Rust `expect`s.
    let expects = concat!(
        "pub fn lamp_load(lamp: &mut Lamp, size: usize) -> Result<(), Status> {\n",
        "    let mut buffer =\n",
        "        kalloc::Box::<[u8]>::try_new_uninit_slice(size).expect(\"no memory\");\n",
        "    lamp.load(&mut buffer, size)\n",
        "}\n",
    );
    let issues = only_issues(cpp_folded, expects);
    assert!(issues.iter().any(|m| m.contains("make_unique")), "{issues:?}");

    // `try_grow` with `expect` asserts like `ASSERT_TRUE(ac.check())`.
    let grows = concat!(
        "fn lamp_buffer_test() -> bool {\n",
        "    let mut buffer = lamp_spare().try_grow(LAMP_SIZE).expect(\"lamp: no memory\");\n",
        "    lamp_fill(&mut buffer, LAMP_SIZE);\n",
        "    true\n",
        "}\n",
    );
    let issues = only_issues(cpp_test, grows);
    assert!(issues.is_empty(), "{issues:?}");

    // A `match` whose `Err` arm returns, on an `_in` allocator.
    let cpp_install = concat!(
        "zx_status_t lamp_init(Lamp* lamp) {\n",
        "  fbl::AllocChecker ac;\n",
        "  auto bulb = ktl::make_unique<Bulb>(&ac);\n",
        "  if (!ac.check()) {\n",
        "    printf(\"lamp: no memory\\n\");\n",
        "    return ZX_ERR_NO_MEMORY;\n",
        "  }\n",
        "  return lamp->Install(ktl::move(bulb));\n",
        "}\n",
    );
    let rust_match = concat!(
        "pub fn lamp_init(lamp: &mut Lamp) -> Result<(), Status> {\n",
        "    let bulb = match kalloc::Box::try_new_in(Bulb::new(), &LAMP_HEAP) {\n",
        "        Ok(bulb) => bulb,\n",
        "        Err(_) => {\n",
        "            kprintln!(\"lamp: no memory\");\n",
        "            return Err(Status::NO_MEMORY);\n",
        "        }\n",
        "    };\n",
        "    lamp.install(bulb)\n",
        "}\n",
    );
    let issues = only_issues(cpp_install, rust_match);
    assert!(issues.is_empty(), "{issues:?}");

    // `if let Err`, `if let None`, `.is_err()` and `.is_none()` bodies run on
    // failure.
    let cpp_probe = concat!(
        "zx_status_t lamp_probe() {\n",
        "  fbl::AllocChecker ac;\n",
        "  ktl::make_unique<Bulb>(&ac);\n",
        "  if (!ac.check()) {\n",
        "    printf(\"lamp: no memory\\n\");\n",
        "    return ZX_ERR_NO_MEMORY;\n",
        "  }\n",
        "  return ZX_OK;\n",
        "}\n",
    );
    for condition in [
        "if let Err(_) = kalloc::Box::try_new(Bulb::new()) {",
        "if let None = kalloc::Box::try_new(Bulb::new()).ok() {",
        "if kalloc::Box::try_new(Bulb::new()).is_err() {",
        "if kalloc::Box::try_new(Bulb::new()).ok().is_none() {",
    ] {
        let rust_probe = format!(
            "pub fn lamp_probe() -> Result<(), Status> {{\n    {condition}\n{}",
            concat!(
                "        kprintln!(\"lamp: no memory\");\n",
                "        return Err(Status::NO_MEMORY);\n",
                "    }\n",
                "    Ok(())\n",
                "}\n",
            )
        );
        let issues = only_issues(cpp_probe, &rust_probe);
        assert!(issues.is_empty(), "{condition}: {issues:?}");
    }

    // A `reset` receiver is the name a Rust assignment stores into.
    let cpp_reset = concat!(
        "bool lamp_bulb_test() {\n",
        "  Lamp lamp;\n",
        "  fbl::AllocChecker ac;\n",
        "  lamp.bulb_.reset(new (&ac) Bulb);\n",
        "  ASSERT_TRUE(ac.check());\n",
        "  EXPECT_TRUE(lamp.Light());\n",
        "  return true;\n",
        "}\n",
    );
    let rust_assign = concat!(
        "fn lamp_bulb_test() -> bool {\n",
        "    let mut lamp = Lamp::new();\n",
        "    lamp.bulb = unwrap_some!(kalloc::Box::try_new(Bulb::new()).ok());\n",
        "    expect_true!(lamp.light());\n",
        "    true\n",
        "}\n",
    );
    let issues = only_issues(cpp_reset, rust_assign);
    assert!(issues.is_empty(), "{issues:?}");

    // The Rust stores into a different field: still reported.
    let other_field = rust_assign.replace("lamp.bulb =", "lamp.lens =");
    let issues = only_issues(cpp_reset, &other_field);
    assert!(issues.iter().any(|m| m.contains("unwrap_some")), "{issues:?}");

    // A type annotation is not part of the binding.
    let typed = concat!(
        "fn lamp_buffer_test() -> bool {\n",
        "    let mut buffer: kalloc::Box<[u8]> =\n",
        "        unwrap_some!(kalloc::Box::<[u8]>::try_new_zeroed_slice(LAMP_SIZE).ok());\n",
        "    lamp_fill(&mut buffer, LAMP_SIZE);\n",
        "    true\n",
        "}\n",
    );
    let issues = only_issues(cpp_test, typed);
    assert!(issues.is_empty(), "{issues:?}");

    // The Rust allocates into a different name: still reported.
    let renamed = typed.replace("buffer", "data");
    let issues = only_issues(cpp_test, &renamed);
    assert!(issues.iter().any(|m| m.contains("make_unique")), "{issues:?}");
}

#[test]
fn implicit_result_return_matches_cpp_status_check() {
    let cpp = "zx_status_t lamp_transmit(Lamp& lamp, Packet* packet) {\n  zx_status_t status = lamp.Enter(packet);\n  if (status != ZX_OK) {\n    return status;\n  }\n  status = copy_to_user(packet);\n  if (status != ZX_OK) {\n    return status;\n  }\n  return ZX_OK;\n}\n";
    let rust = "pub fn lamp_transmit(lamp: &mut Lamp, packet: &mut Packet) -> Result<(), Status> {\n    lamp.enter(packet)?;\n    copy_to_user(packet)\n}\n";
    let found = all_findings(cpp, rust);
    assert!(found.is_empty(), "{found:?}");

    // An explicit return of a Result expression also matches.
    let rust_ret = "pub fn lamp_transmit(lamp: &mut Lamp, packet: &mut Packet) -> Result<(), Status> {\n    lamp.enter(packet)?;\n    return copy_to_user(packet);\n}\n";
    let found_ret = all_findings(cpp, rust_ret);
    assert!(found_ret.is_empty(), "{found_ret:?}");

    // Dropping the tail call is reported as an issue.
    let dropped = "pub fn lamp_transmit(lamp: &mut Lamp, packet: &mut Packet) -> Result<(), Status> {\n    lamp.enter(packet)?;\n    Ok(())\n}\n";
    let found_dropped = all_findings(cpp, dropped);
    assert!(found_dropped.iter().any(|m| m.contains("write_user")), "{found_dropped:?}");
}

#[test]
fn do_while_matches_loop_break() {
    let cpp = "void wait_for_event(Lamp* lamp, uint32_t prev_seq, uint32_t prev_idx) {\n  do {\n    lamp->poll();\n  } while (prev_seq != lamp->seq() || lamp->idx() != prev_idx);\n}\n";
    let rust = "pub fn wait_for_event(lamp: &mut Lamp, prev_seq: u32, prev_idx: u32) {\n    loop {\n        lamp.poll();\n        if prev_seq == lamp.seq() && lamp.idx() == prev_idx {\n            break;\n        }\n    }\n}\n";
    let cs =
        ChangeSet::from_files(&[("lamp.cc".into(), cpp.into()), ("lamp.rs".into(), rust.into())]);
    let report = babeldiff::run(&cs, &Options::default(), &mut NoFinder);
    assert_eq!(report.pairs.len(), 1);
    let p = &report.pairs[0];
    assert!(p.findings.is_empty(), "{:?}", p.findings);
    for (_, a, b) in &p.summary.flow {
        assert_eq!(a, b, "flow counts should match: {:?}", p.summary.flow);
    }
    let rendered =
        render(&report, &RenderOptions { layout: Layout::Stacked, ..RenderOptions::default() });
    assert!(
        rendered.contains("} while (prev_seq != lamp->seq() || lamp->idx() != prev_idx);"),
        "stacked output should include trailing while line:\n{rendered}"
    );

    // Dropping one of the loop-exit checks is still reported as an issue.
    let dropped = "pub fn wait_for_event(lamp: &mut Lamp, prev_seq: u32, prev_idx: u32) {\n    loop {\n        lamp.poll();\n        if prev_seq == lamp.seq() {\n            break;\n        }\n    }\n}\n";
    let issues = only_issues(cpp, dropped);
    assert!(issues.iter().any(|m| m.contains("condition tests")), "{issues:?}");
}

#[test]
fn exhaustive_match_demotes_default_fallback_and_flow() {
    let cpp = "const char* level_to_string(Level level) {\n  switch (level) {\n    case Level::kLow:\n      return \"Low\";\n    case Level::kHigh:\n      return \"High\";\n    default:\n      return \"Unknown\";\n  }\n}\n";
    let rust = "pub fn level_to_string(level: Level) -> &'static str {\n    match level {\n        Level::Low => \"Low\",\n        Level::High => \"High\",\n    }\n}\n";
    let cs =
        ChangeSet::from_files(&[("lamp.cc".into(), cpp.into()), ("lamp.rs".into(), rust.into())]);
    let report = babeldiff::run(&cs, &Options::default(), &mut NoFinder);
    assert_eq!(report.pairs.len(), 1);
    let p = &report.pairs[0];
    let issues: Vec<&str> = p
        .findings
        .iter()
        .filter(|f| f.severity == Severity::Issue)
        .map(|f| f.message.as_str())
        .collect();
    assert!(issues.is_empty(), "{issues:?}");
    assert_eq!(p.findings.len(), 1, "{:?}", p.findings);
    assert!(
        p.findings[0].message.contains("the Rust match covers every case, so it needs no default"),
        "{:?}",
        p.findings
    );
    for (_, a, b) in &p.summary.flow {
        assert_eq!(a, b, "flow counts should match: {:?}", p.summary.flow);
    }
    assert!(
        p.summary.flow.contains(&("switch", 1, 1)) && p.summary.flow.contains(&("case", 2, 2)),
        "expected separate switch and case counts in flow summary: {:?}",
        p.summary.flow
    );

    // If a non-default C++ case is missing in Rust, it is still an issue.
    let missing_case = "pub fn level_to_string(level: Level) -> &'static str {\n    match level {\n        Level::Low => \"Low\",\n    }\n}\n";
    let issues = only_issues(cpp, missing_case);
    assert!(!issues.is_empty(), "expected issue when a case is dropped");
}

fn version(path: &str, text: &str) -> babeldiff::input::Version {
    babeldiff::input::Version { path: path.into(), text: text.into(), changed: None }
}

const LAMP_OLD_CC: &str = r#"
uint16_t Panel::Read(Field16 field) {
  uint64_t value = vmread(field.value);
  return static_cast<uint16_t>(value);
}

uint32_t Panel::Read(Field32 field) {
  uint64_t value = vmread(field.value);
  return static_cast<uint32_t>(value);
}

uint32_t Lamp::Traps() {
  uint32_t count = traps_.count();
  return count + pending_;
}

void lamp_xsetbv(uint32_t reg, uint64_t val) {
  uint32_t lo = static_cast<uint32_t>(val);
  uint32_t hi = static_cast<uint32_t>(val >> 32);
  __asm__ volatile("xsetbv" ::"c"(reg), "a"(lo), "d"(hi));
}

void Lamp::Init(uint32_t level) {
  level_ = level;
  pending_ = 0;
  Configure(level);
}

uint8_t LampId::stepping() const {
  uint32_t eax = regs_.eax;
  return static_cast<uint8_t>(eax & 0xf);
}
"#;

const LAMP_NEW_H: &str = r#"
void Lamp::Init(uint32_t level) {
  level_ = level;
  pending_ = 0;
  Configure(level);
}

uint8_t LampId::stepping() const { return rust_lamp_id_stepping(&regs_); }
"#;

const LAMP_RS: &str = r#"
impl Panel {
    pub fn read_16(&self, field: Field16) -> u16 {
        let value = vmread(field.value);
        value as u16
    }

    pub fn read_32(&self, field: Field32) -> u32 {
        let value = vmread(field.value);
        value as u32
    }
}

fn traps() -> u32 {
    0
}

pub fn lamp_xsetbv(reg: u32, val: u64) {
    unsafe { xsetbv(reg, val) }
}

unsafe fn xsetbv(reg: u32, val: u64) {
    let lo = val as u32;
    let hi = (val >> 32) as u32;
    unsafe { core::arch::asm!("xsetbv", in("ecx") reg, in("eax") lo, in("edx") hi) };
}

impl LampId {
    pub fn new(regs: Regs) -> Self {
        LampId { regs }
    }

    pub fn stepping(&self) -> u8 {
        let eax = self.regs.eax;
        (eax & 0xf) as u8
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_lamp_id_stepping(regs: &Regs) -> u8 {
    LampId::new(*regs).stepping()
}

#[cfg(test)]
mod tests {
    struct TestLamp {
        level: u32,
        pending: u32,
    }

    impl TestLamp {
        fn init(&mut self, level: u32) {
            self.level = level;
            self.pending = 0;
            configure(level);
        }
    }
}
"#;

#[test]
fn pairs_follow_names_types_and_real_bodies() {
    let cs = ChangeSet {
        cpp_old: vec![version("lamp/lamp.cc", LAMP_OLD_CC)],
        cpp_new: vec![version("lamp/lamp.h", LAMP_NEW_H)],
        rust_new: vec![version("lamp/lamp.rs", LAMP_RS)],
    };
    let report = babeldiff::run(&cs, &Options::default(), &mut NoFinder);
    let pairs: Vec<(String, usize, String)> = report
        .pairs
        .iter()
        .map(|p| (p.cpp.name.clone(), p.cpp.start_line, p.rust.name.clone()))
        .collect();
    let has = |c: &str, r: &str| pairs.iter().any(|(pc, _, pr)| pc == c && pr == r);
    // Overloads go to the Rust function named for their parameter type.
    let read: Vec<&str> =
        pairs.iter().filter(|(c, _, _)| c == "Panel::Read").map(|(_, _, r)| r.as_str()).collect();
    assert_eq!(read, ["Panel::read_16", "Panel::read_32"], "{pairs:?}");
    // A placeholder returning a constant is not the port.
    assert!(!pairs.iter().any(|(c, _, _)| c == "Lamp::Traps"), "{pairs:?}");
    // A wrapper that only passes its arguments on stands in for its callee.
    assert!(has("lamp_xsetbv", "xsetbv"), "{pairs:?}");
    // C++ that moved into a header is not ported by a test double.
    assert!(!pairs.iter().any(|(c, _, _)| c == "Lamp::Init"), "{pairs:?}");
    // The shim calls `LampId::new(..).stepping()`: its target is `stepping`.
    assert!(has("LampId::stepping", "LampId::stepping"), "{pairs:?}");
}

#[test]
fn private_extern_c_callback_pairs_and_nested_fn_aligns() {
    let cpp = r#"
void Watchdog::EvictionTriggerCallback(Timer* timer, zx_instant_mono_t now, void* arg) {
  Watchdog* watchdog = reinterpret_cast<Watchdog*>(arg);
  watchdog->EvictionTrigger();
}

void Watchdog::EvictionTrigger() {
  trigger_eviction();
}

void Watchdog::Init() {
  auto worker_cb = [](void* arg) -> int {
    Watchdog* watchdog = reinterpret_cast<Watchdog*>(arg);
    watchdog->WorkerThread();
  };
  start_worker(worker_cb, this);
}
"#;
    let rust = r#"
unsafe extern "C" fn eviction_trigger_callback(
    _timer: *mut Timer,
    _now: i64,
    arg: *mut c_void,
) {
    // SAFETY: `arg` points to a valid `Watchdog`.
    let watchdog = unsafe { &*arg.cast::<Watchdog>() };
    watchdog.eviction_trigger();
}

impl Watchdog {
    fn eviction_trigger(&self) {
        trigger_eviction();
    }

    pub fn init(&self) {
        extern "C" fn worker_cb(arg: *mut c_void) -> i32 {
            // SAFETY: `arg` points to a valid `Watchdog`.
            let watchdog = unsafe { &*arg.cast::<Watchdog>() };
            watchdog.worker_thread();
            0
        }
        start_worker(worker_cb, self);
    }
}
"#;
    let cs = ChangeSet {
        cpp_old: vec![version("watchdog.cc", cpp)],
        cpp_new: Vec::new(),
        rust_new: vec![version("watchdog.rs", rust)],
    };
    let report = babeldiff::run(&cs, &Options::default(), &mut NoFinder);
    assert!(report.shims.is_empty(), "{:?}", report.shims);
    assert!(report.unmatched_cpp.is_empty(), "{:?}", report.unmatched_cpp);
    assert!(report.unmatched_rust.is_empty(), "{:?}", report.unmatched_rust);
    let cb = report
        .pairs
        .iter()
        .find(|p| p.cpp.name == "Watchdog::EvictionTriggerCallback")
        .expect("callback pair");
    assert_eq!(cb.rust.name, "eviction_trigger_callback");
    assert!(cb.findings.is_empty(), "{:?}", cb.findings);

    let init = report.pairs.iter().find(|p| p.cpp.name == "Watchdog::Init").expect("init pair");
    assert!(init.findings.is_empty(), "{:?}", init.findings);
}

#[test]
fn unpaired_helper_callers_distinguish_same_named_methods() {
    let cpp = r#"
void Watchdog::EvictionTrigger() {
  continuous_eviction_active_.store(true);
  trigger_eviction();
}

void Watchdog::WaitForMemChange() {
  mem_event_idx_ = idx;
  wait_event();
}
"#;
    let rust = r#"
impl RelaxedAtomicPressureLevel {
    fn store(&self, level: PressureLevel) {
        self.0.store(level as u8, Ordering::Relaxed);
    }
}

impl Watchdog {
    fn eviction_trigger(&self) {
        self.continuous_eviction_active.store(true, Ordering::SeqCst);
        trigger_eviction();
    }

    fn wait_for_mem_change(&self) {
        self.mem_event_idx.store(idx);
        wait_event();
    }
}
"#;
    let cs = ChangeSet {
        cpp_old: vec![version("watchdog.cc", cpp)],
        cpp_new: Vec::new(),
        rust_new: vec![version("watchdog.rs", rust)],
    };
    let report = babeldiff::run(&cs, &Options::default(), &mut NoFinder);
    let store_fn = report
        .unmatched_rust
        .iter()
        .find(|f| f.name == "RelaxedAtomicPressureLevel::store")
        .expect("unpaired store helper");
    assert_eq!(report.callers(store_fn), vec!["Watchdog::wait_for_mem_change"]);
}

#[test]
fn untouched_functions_and_shims_are_omitted_from_report() {
    let rust_file = r#"
impl EventDispatcher {
    pub fn create() {
        unsafe { cpp_event_dispatcher_create() };
    }

    pub fn user_signal_self(&self) {
        unsafe { cpp_event_dispatcher_user_signal_self(self) };
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_event_dispatcher_create() {
    EventDispatcher::create();
}
"#;
    let changed: std::collections::BTreeSet<usize> = (7..=9).collect();
    let cs = ChangeSet {
        cpp_old: Vec::new(),
        cpp_new: Vec::new(),
        rust_new: vec![babeldiff::input::Version {
            path: "event_dispatcher.rs".into(),
            text: rust_file.into(),
            changed: Some(changed),
        }],
    };
    let report = babeldiff::run(&cs, &Options::default(), &mut NoFinder);
    assert!(report.shims.is_empty(), "{:?}", report.shims);
    assert!(
        report.rust_facades.iter().all(|(f, _)| f.name == "EventDispatcher::user_signal_self"),
        "{:?}",
        report.rust_facades
    );
    assert!(!report.rust_facades.is_empty());
}

#[test]
fn in_class_member_initializers_pair_with_rust_new() {
    let cpp_old = r#"
class Watchdog {
 private:
  AutounsignalEvent mem_state_signal_;
  RelaxedAtomic<PressureLevel> mem_event_idx_ = PressureLevel::kNormal;
  zx_duration_mono_t hysteresis_seconds_ = ZX_SEC(10);
  Timer eviction_trigger_;
  ktl::atomic<bool> continuous_eviction_active_ = false;
  Thread* worker_thread_ = nullptr;
};
"#;
    let cpp_new = r#"
Watchdog::Watchdog() {
  rust_watchdog_construct(&opaque_storage_);
}
"#;
    let rust_new = r#"
impl RelaxedAtomicPressureLevel {
    const fn new(level: PressureLevel) -> Self {
        Self(AtomicU8::new(level as u8))
    }
}

impl WatchdogState {
    pub fn new() -> impl PinInit<Self, core::convert::Infallible> {
        pin_init!(Self {
            mem_state_signal <- AutounsignalEvent::init(false),
            mem_event_idx: RelaxedAtomicPressureLevel::new(PressureLevel::Normal),
            hysteresis_seconds: UnsafeCell::new(zx_sec(10)),
            eviction_trigger <- UnsafeCell::pin_init(Timer::init(kernel::timer::ZX_CLOCK_MONOTONIC)),
            continuous_eviction_active: AtomicBool::new(false),
            worker_thread: UnsafeCell::new(None),
        })
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_watchdog_construct(storage: *mut MaybeUninit<WatchdogState>) {
    let init = WatchdogState::new();
    let _ = unsafe { pin_init::PinInit::__pinned_init(init, storage.cast()) };
}
"#;
    let cs = ChangeSet {
        cpp_old: vec![version("watchdog.h", cpp_old)],
        cpp_new: vec![version("watchdog.cc", cpp_new)],
        rust_new: vec![version("watchdog.rs", rust_new)],
    };
    let report = babeldiff::run(&cs, &Options::default(), &mut NoFinder);
    let pair = report
        .pairs
        .iter()
        .find(|p| p.cpp.name == "Watchdog::Watchdog")
        .expect("in-class member initializers should pair with WatchdogState::new");
    assert_eq!(pair.rust.name, "WatchdogState::new");
    assert!(pair.findings.is_empty(), "{:?}", pair.findings);
    let helper = report
        .unmatched_rust
        .iter()
        .find(|f| f.name == "RelaxedAtomicPressureLevel::new")
        .expect("RelaxedAtomicPressureLevel::new should be in unmatched_rust");
    assert_eq!(report.callers(helper), vec!["WatchdogState::new"]);
}

#[test]
fn status_assignment_and_let_match_align_with_cpp_status_checks() {
    let cpp = r#"
void init_and_shutdown(zx_instant_mono_t deadline) {
  zx_status_t status = EventDispatcher::Create(0, &event, &rights);
  if (status != ZX_OK) {
    panic("create failed: %d\n", status);
  }
  status = dlog_shutdown(deadline);
  if (status != ZX_OK) {
    printf("dlog_shutdown failed: %d\n", status);
  }
}
"#;
    let rust = r#"
pub fn init_and_shutdown(deadline: zx_instant_mono_t) {
    let handle = match EventDispatcher::create(0) {
        Ok((h, _rights)) => h,
        Err(status) => {
            panic!("create failed: {}\n", status.into_raw());
        }
    };
    let status = dlog_shutdown(deadline);
    if let Err(status) = status {
        kprintln!("dlog_shutdown failed: {}", status.into_raw());
    }
}
"#;
    let cs =
        ChangeSet::from_files(&[("init.cc".into(), cpp.into()), ("init.rs".into(), rust.into())]);
    let report = babeldiff::run(&cs, &Options::default(), &mut NoFinder);
    assert_eq!(report.pairs.len(), 1);
    let p = &report.pairs[0];
    assert!(p.findings.is_empty(), "{:?}", p.findings);
    for (_, a, b) in &p.summary.flow {
        assert_eq!(a, b, "flow counts should match: {:?}", p.summary.flow);
    }
}

#[test]
fn idiomatic_zircon_rust_patterns_and_cpp_helpers_produce_no_false_notes() {
    let cpp_old = r#"
void Watchdog::CheckAndEvict(zx_instant_mono_t time_now) {
  OOM_KTRACE_DURATION();
  if (eviction_strategy_ == EvictionStrategy::Continuous) {
    continuous_eviction_active_ = true;
  }
  if (zx_time_sub_time(time_now, prev_eval_time_) >= hysteresis_ && ispow2(iterations_)) {
    uint64_t free_bytes = pmm_count_free_pages() * kPageSize;
    printf("free: %s\n", FormattedBytes(free_bytes).c_str());
    pmm_page_queues()->Dump();
    pmm_evictor()->EvictAsynchronous(min_free_target_, free_mem_target_);
  }
  for (uint8_t i = 0; i < kNumLevels; i++) {
    auto level = PressureLevel(i);
    printf("level: %s\n", PressureLevelToString(level));
  }
}
"#;
    let cpp_new = r#"
FFI_ALWAYS_INLINE void cpp_watchdog_evict_async(uint64_t min_free, uint64_t target) {
  pmm_evictor()->EvictAsynchronous(min_free, target);
}
"#;
    let rust_new = r#"
impl WatchdogState {
    pub fn check_and_evict(&self, time_now: zx_instant_mono_t) {
        let _trace = ScopedOomKtrace::new();
        if *self.eviction_strategy.get() == EvictionStrategy::Continuous {
            self.continuous_eviction_active.store(true, Ordering::SeqCst);
        }
        if time_now.saturating_sub(*self.prev_eval_time.get()) >= *self.hysteresis.get()
            && self.iterations.load().is_power_of_two()
        {
            let free_bytes = pmm::node().count_free_pages() * PAGE_SIZE;
            let mut buf = [0u8; pretty::MAX_FORMAT_SIZE_LEN];
            kprintln!("free: {:s}", pretty::format_size_rs(&mut buf, free_bytes as usize));
            pmm::page_queues().dump();
            cpp_watchdog_evict_async(*self.min_free_target.get(), *self.free_mem_target.get());
        }
        for (i, _slot) in self.slots.iter_mut().enumerate().take(NUM_LEVELS) {
            let level = PressureLevel::try_from(i as u8).unwrap();
            kprintln!("level: {:s}", pressure_level_to_string(level));
        }
    }
}
"#;
    let cs = ChangeSet {
        cpp_old: vec![version("watchdog.cc", cpp_old)],
        cpp_new: vec![version("watchdog.cc", cpp_new)],
        rust_new: vec![version("watchdog.rs", rust_new)],
    };
    let report = babeldiff::run(&cs, &Options::default(), &mut NoFinder);
    assert_eq!(report.pairs.len(), 1);
    let p = &report.pairs[0];
    assert!(p.findings.is_empty(), "expected 0 findings, got {:?}", p.findings);
}

#[test]
fn unadapted_cpp_identifiers_in_ported_comments_are_reported() {
    let cpp = r#"
void Watchdog::WaitForChange() {
  // Coming into this method we must not be in the kOutOfMemory state, and
  // eviction_trigger_ must not be active.
  DEBUG_ASSERT(level_ != PressureLevel::kOutOfMemory);
  // Adapted in Rust to `watermark_debounce`.
  uint64_t debounce = watermark_debounce_;
}
"#;
    let rust = r#"
impl WatchdogState {
    fn wait_for_change(&self) {
        // Coming into this method we must not be in the kOutOfMemory state, and
        // eviction_trigger_ must not be active.
        debug_assert_ne!(self.level.load(), PressureLevel::OutOfMemory);
        // Adapted in Rust to `watermark_debounce`.
        let _debounce = *self.watermark_debounce.get();
    }
}
"#;
    let cs = ChangeSet::from_files(&[
        ("watchdog.cc".into(), cpp.into()),
        ("watchdog.rs".into(), rust.into()),
    ]);
    let report = babeldiff::run(&cs, &Options::default(), &mut NoFinder);
    assert_eq!(report.pairs.len(), 1);
    let p = &report.pairs[0];
    let issues: Vec<&str> = p
        .findings
        .iter()
        .filter(|f| f.severity == Severity::Issue && f.category == Category::Comment)
        .map(|f| f.message.as_str())
        .collect();
    assert_eq!(
        issues,
        [
            "comment still uses C++ identifiers `kOutOfMemory`, `eviction_trigger_`; update to the Rust name"
        ],
        "{:?}",
        p.findings
    );
}

#[test]
fn pub_fn_doc_comments_are_expected_additions() {
    // 1. Free `pub fn`, `pub(crate) fn`, `pub unsafe fn`, and `impl` `pub fn`
    //    adding single- and multi-paragraph doc comments where C++ has none.
    let cpp = r#"
uint32_t lamp_level(const Lamp& lamp) {
  return lamp.level();
}

uint32_t lamp_wattage(const Lamp& lamp) {
  return lamp.wattage();
}

void lamp_raw_reset(Lamp* lamp) {
  lamp->reset();
}

void Lamp::Activate(uint32_t target) {
  target_ = target;
  enable();
}
"#;
    let rust = r#"
/// Returns the current brightness level of `lamp`.
pub fn lamp_level(lamp: &Lamp) -> u32 {
    lamp.level()
}

/// Returns the wattage of `lamp` for crate-internal callers.
pub(crate) fn lamp_wattage(lamp: &Lamp) -> u32 {
    lamp.wattage()
}

/// Resets `lamp` through a raw pointer.
///
/// # Safety
///
/// `lamp` must point to a valid, exclusively borrowed `Lamp`.
pub unsafe fn lamp_raw_reset(lamp: *mut Lamp) {
    // SAFETY: `lamp` is valid by caller contract.
    unsafe { (*lamp).reset() };
}

impl Lamp {
    /// Activates the lamp at `target`.
    ///
    /// Configures the target brightness before enabling hardware output.
    pub fn activate(&mut self, target: u32) {
        self.target = target;
        self.enable();
    }
}
"#;
    let cs =
        ChangeSet::from_files(&[("lamp.cc".into(), cpp.into()), ("lamp.rs".into(), rust.into())]);
    let report = babeldiff::run(&cs, &Options::default(), &mut NoFinder);
    assert_eq!(report.pairs.len(), 4);
    for p in &report.pairs {
        assert!(
            p.findings.is_empty(),
            "expected no findings on `{}` when adding doc comments to pub fn, got {:?}",
            p.rust.name,
            p.findings
        );
    }
    use babeldiff::html::{HtmlOptions, render_html};
    let html = render_html(&report, &HtmlOptions::default());
    assert!(
        html.contains("title=\"Doc comment, expected in Rust\""),
        "HTML output should mark pub fn doc comment rows as expected:\n{html}"
    );

    // 2. Expanding a C++ doc comment with an extra paragraph on a `pub fn` is
    //    also an expected addition.
    let cpp_with_doc = "// Returns the lamp level.\nuint32_t lamp_level(const Lamp& lamp) {\n  return lamp.level();\n}\n";
    let rust_expanded_doc = "/// Returns the lamp level.\n///\n/// The returned value is in candelas.\npub fn lamp_level(lamp: &Lamp) -> u32 {\n    lamp.level()\n}\n";
    assert!(only_issues(cpp_with_doc, rust_expanded_doc).is_empty());

    // 3. Adding a doc comment to a private `fn` where C++ has none is still an issue.
    let private_doc = "/// Returns the current brightness level.\nfn lamp_level(lamp: &Lamp) -> u32 {\n    lamp.level()\n}\n";
    assert_eq!(
        only_issues(
            "uint32_t lamp_level(const Lamp& lamp) {\n  return lamp.level();\n}\n",
            private_doc
        ),
        [
            "doc comment added in Rust where the C++ function has none; a conversion adds no comments other than SAFETY"
        ]
    );

    // 4. Adding a non-SAFETY in-body comment inside a `pub fn` is still an issue.
    let in_body_comment = "pub fn lamp_level(lamp: &Lamp) -> u32 {\n    // Query the hardware register.\n    lamp.level()\n}\n";
    assert_eq!(
        only_issues(
            "uint32_t lamp_level(const Lamp& lamp) {\n  return lamp.level();\n}\n",
            in_body_comment
        ),
        ["comment added in Rust; a conversion adds no comments other than SAFETY"]
    );

    // 5. Dropping a C++ doc comment on a `pub fn` is still an issue.
    let dropped_doc = "pub fn lamp_level(lamp: &Lamp) -> u32 {\n    lamp.level()\n}\n";
    assert_eq!(only_issues(cpp_with_doc, dropped_doc), ["comment only in C++"]);
}

#[test]
fn parse_cl_formats_in_golden() {
    assert_eq!(babeldiff::cl::parse_cl_spec("1885120"), Ok(("1885120".into(), None)));
    assert_eq!(babeldiff::cl::parse_cl_spec("1885120/5"), Ok(("1885120".into(), Some("5".into()))));
    assert_eq!(babeldiff::cl::parse_cl_spec("fxr/1885120"), Ok(("1885120".into(), None)));
    assert_eq!(
        babeldiff::cl::parse_cl_spec("fxr/1885120/5"),
        Ok(("1885120".into(), Some("5".into())))
    );
    assert_eq!(
        babeldiff::cl::parse_cl_spec("https://fuchsia-review.googlesource.com/c/fuchsia/+/1885120"),
        Ok(("1885120".into(), None))
    );
    assert_eq!(
        babeldiff::cl::parse_cl_spec(
            "https://fuchsia-review.googlesource.com/c/fuchsia/+/1885120/5"
        ),
        Ok(("1885120".into(), Some("5".into())))
    );
    assert_eq!(
        babeldiff::cl::parse_cl_spec(
            "https://fuchsia-review.git.corp.google.com/c/fuchsia/+/1885120/5"
        ),
        Ok(("1885120".into(), Some("5".into())))
    );
    assert!(babeldiff::cl::parse_cl_spec("").is_err());
    assert!(babeldiff::cl::parse_cl_spec("not-a-cl").is_err());
    assert!(babeldiff::cl::parse_cl_spec("1885120/bad").is_err());
}

#[test]
fn cli_requires_subcommand_or_cl() {
    let bin = env!("CARGO_BIN_EXE_babeldiff");
    let out = Command::new(bin).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("a subcommand (git, patch, cl, files) or --cl <CL> is required"));
}

#[test]
fn cli_cl_subcommand_and_flag() {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let patch_content = std::fs::read_to_string(fixture("beacon/beacon.patch")).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    let server_patch = patch_content.clone();
    std::thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, _) = match listener.accept() {
                Ok(conn) => conn,
                Err(_) => break,
            };
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                server_patch.len(),
                server_patch
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });

    let bin = env!("CARGO_BIN_EXE_babeldiff");
    let base_url = format!("http://127.0.0.1:{port}");

    // 1. Test `babeldiff cl 1885120/1`
    let out1 = Command::new(bin)
        .env("BABELDIFF_GERRIT_URL", &base_url)
        .args(["cl", "1885120/1", "--summary"])
        .output()
        .unwrap();
    assert_eq!(out1.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out1.stderr));
    let stdout1 = String::from_utf8_lossy(&out1.stdout);
    assert!(stdout1.contains("babeldiff: 7 pairs, 0 issues"));

    // 2. Test `babeldiff --cl 1885120/1`
    let out2 = Command::new(bin)
        .env("BABELDIFF_GERRIT_URL", &base_url)
        .args(["--cl", "1885120/1", "--summary"])
        .output()
        .unwrap();
    assert_eq!(out2.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out2.stderr));
    let stdout2 = String::from_utf8_lossy(&out2.stdout);
    assert!(stdout2.contains("babeldiff: 7 pairs, 0 issues"));
}
