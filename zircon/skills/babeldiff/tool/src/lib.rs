//! babeldiff lines up C++ code removed by a change with the Rust code that
//! replaces it, and checks that the two do the same things in the same order:
//! the same comments, error returns, locks, calls, and control flow.
//!
//! The pipeline is:
//!
//! 1. [`input`] and [`git`] turn files, a patch, or a git revision range into
//!    a [`input::ChangeSet`]: C++ before the change, Rust after it.
//! 2. [`extract`] parses both languages with tree-sitter and flattens each
//!    function into [`model::Unit`]s (statements, comments, and the headers
//!    of compound statements) with language-neutral [`model::Features`].
//! 3. [`analyze`] pairs C++ functions with Rust functions, following Zircon's
//!    FFI shims (`rust_<class>_<method>`) where it can, and [`align`] lines up
//!    each pair's units.
//! 4. [`check`] flags differences, [`render`] prints a side-by-side,
//!    diff-style report, [`html`] writes a self-contained HTML page, and
//!    [`json`] writes findings for agents and scripts.
//!
//! ```
//! use babeldiff::{analyze, input::ChangeSet, render};
//!
//! let cpp = "zx_status_t Foo::Bar(int x) {\n  // Check x.\n  if (x < 0) {\n    return ZX_ERR_INVALID_ARGS;\n  }\n  return ZX_OK;\n}\n";
//! let rust = "impl Foo {\n    fn bar(&self, x: i32) -> Result<(), Status> {\n        // Check x.\n        if x < 0 {\n            return Err(Status::INVALID_ARGS);\n        }\n        Ok(())\n    }\n}\n";
//! let cs = ChangeSet::from_files(&[("foo.cc".into(), cpp.into()), ("foo.rs".into(), rust.into())]);
//! let report = babeldiff::run(&cs, &analyze::Options::default(), &mut analyze::NoFinder);
//! assert_eq!(report.pairs.len(), 1);
//! assert_eq!(report.issues(), 0);
//! println!("{}", render::render(&report, &render::RenderOptions::default()));
//! ```

pub mod align;
pub mod analyze;
pub mod check;
pub mod cl;
pub mod cpp;
pub mod extract;
pub mod git;
pub mod html;
pub mod input;
pub mod json;
pub mod layout;
pub mod lint;
pub mod model;
pub mod normalize;
pub mod patch;
pub mod placement;
pub mod render;
pub mod rust;
mod ts;
pub mod values;

/// Default minimum fraction of a function's lines that a change must touch
/// for the function to count as converted.
pub const DEFAULT_MIN_CHANGED: f64 = 0.25;

/// Extracts, pairs and checks the functions in a change set.
pub fn run(
    cs: &input::ChangeSet,
    opts: &analyze::Options,
    finder: &mut dyn analyze::CppFinder,
) -> analyze::Report {
    run_with(cs, opts, finder, DEFAULT_MIN_CHANGED, Vec::new())
}

/// Like [`run`], with a custom changed-lines threshold and forced pairs.
pub fn run_with(
    cs: &input::ChangeSet,
    opts: &analyze::Options,
    finder: &mut dyn analyze::CppFinder,
    min_changed: f64,
    forced: Vec<(String, String)>,
) -> analyze::Report {
    let mut inputs = input::build_inputs(cs, min_changed);
    inputs.forced = forced;
    let mut report = analyze::analyze(inputs, opts, finder);
    report.lints = lint::lint(cs);
    let (placement, lints) = placement::placement(&report.pairs);
    report.placement = placement;
    report.lints.extend(lints);
    report.lints.sort_by(|a, b| (&a.path, a.line, a.kind).cmp(&(&b.path, b.line, b.kind)));
    report
}
