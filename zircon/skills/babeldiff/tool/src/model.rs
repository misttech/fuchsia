//! Core data types shared by the extractors, the aligner and the renderer.

use std::fmt;

/// Source language of a file or function.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Lang {
    Cpp,
    Rust,
}

impl Lang {
    /// Guesses the language from a file path's extension.
    pub fn from_path(path: &str) -> Option<Lang> {
        let ext = path.rsplit('.').next()?.to_ascii_lowercase();
        match ext.as_str() {
            "rs" => Some(Lang::Rust),
            "cc" | "cpp" | "cxx" | "c++" | "c" | "h" | "hh" | "hpp" | "hxx" | "inc" | "ipp" => {
                Some(Lang::Cpp)
            }
            _ => None,
        }
    }
}

impl fmt::Display for Lang {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Lang::Cpp => "C++",
            Lang::Rust => "Rust",
        })
    }
}

/// What a unit is, independent of the language it came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UnitKind {
    /// The function signature, up to the opening brace of the body.
    Signature,
    /// One comment, or a run of adjacent line comments.
    Comment,
    /// A plain statement or declaration.
    Stmt,
    /// The header of an `if` (condition up to the opening brace). Rust
    /// `let ... else` produces one of these too.
    If,
    /// `else if` header.
    ElseIf,
    /// `else` header.
    Else,
    /// Header of `for`, `while`, `do`, or `loop`.
    Loop,
    /// Header of `switch` or `match`.
    Switch,
    /// A `case` label or a `match` arm pattern.
    Case,
    Return,
    Break,
    Continue,
    Goto,
    Label,
    /// A conditionally compiled branch: C++ `#if`, `#ifdef`, `#elif` or
    /// `#else`, or Rust `#[cfg(..)]` on a statement or `if cfg!(..)`. The
    /// code it guards follows one level deeper.
    Cfg,
}

impl UnitKind {
    pub fn name(self) -> &'static str {
        match self {
            UnitKind::Signature => "signature",
            UnitKind::Comment => "comment",
            UnitKind::Stmt => "statement",
            UnitKind::If => "if",
            UnitKind::ElseIf => "else if",
            UnitKind::Else => "else",
            UnitKind::Loop => "loop",
            UnitKind::Switch => "switch/match",
            UnitKind::Case => "case/arm",
            UnitKind::Return => "return",
            UnitKind::Break => "break",
            UnitKind::Continue => "continue",
            UnitKind::Goto => "goto",
            UnitKind::Label => "label",
            UnitKind::Cfg => "conditional compilation",
        }
    }

    /// Whether this unit changes control flow.
    pub fn is_control_flow(self) -> bool {
        matches!(
            self,
            UnitKind::If
                | UnitKind::ElseIf
                | UnitKind::Else
                | UnitKind::Loop
                | UnitKind::Switch
                | UnitKind::Case
                | UnitKind::Return
                | UnitKind::Break
                | UnitKind::Continue
                | UnitKind::Goto
                | UnitKind::Label
        )
    }
}

/// What a return statement (or Rust tail expression) produces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ret {
    /// An error code, normalized without the `ZX_ERR_` prefix, e.g. `INVALID_ARGS`.
    Error(String),
    /// `ZX_OK`, `Ok(())`, `zx::ok(...)`, `Ok(value)`.
    Ok,
    /// Returns a status variable (e.g. `return status;` after a check).
    Status,
    /// Any other value, or nothing.
    Value,
}

impl fmt::Display for Ret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Ret::Error(e) => write!(f, "error {e}"),
            Ret::Ok => f.write_str("ok"),
            Ret::Status => f.write_str("a status variable"),
            Ret::Value => f.write_str("a value"),
        }
    }
}

/// Language-neutral facts about a unit, used both to align units and to
/// check that aligned units do the same thing.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Features {
    /// Normalized names of functions, methods and macros called.
    pub calls: Vec<String>,
    /// Normalized identifiers mentioned.
    pub idents: Vec<String>,
    /// Calls and identifiers together, with accessor prefixes removed, so a
    /// C++ field `head_` matches a Rust accessor `head()` or `set_head()`.
    pub names: Vec<String>,
    /// Error codes mentioned, normalized (e.g. `NO_MEMORY`).
    pub errors: Vec<String>,
    /// Locks acquired, normalized (e.g. `lock` for `&lock_` and `self.lock`).
    pub locks: Vec<String>,
    /// Whether the unit releases a lock explicitly.
    pub unlocks: bool,
    /// Whether the unit propagates an error to the caller (`?` in Rust,
    /// `if (status != ZX_OK) return status;` in C++).
    pub propagates: bool,
    /// Whether the unit is an assertion.
    pub asserts: bool,
    /// For `if` units: whether the condition tests a call for failure
    /// (`status != ZX_OK`, `.is_err()`, `let Err(..)`).
    pub checks_error: bool,
    /// For return units: what is returned.
    pub ret: Option<Ret>,
    /// For comment units: normalized words.
    pub comment: Vec<String>,
    /// For comment units: a Rust safety comment (`// SAFETY: ...` or a
    /// `# Safety` doc section), which conversions add as a matter of course.
    pub safety: bool,
    /// For Rust statements: ksync lock bookkeeping with no C++ counterpart,
    /// such as taking a lock token from a guard (`let token =
    /// guard.token();`), asserting a held lock (`LockToken::new()`), or
    /// binding a guarded field (`let list = self.list.get_mut(token);`).
    pub lock_plumbing: bool,
    /// Bookkeeping that one language needs and the other doesn't, with no
    /// effect of its own: Rust binding a dispatcher's state struct (`let
    /// state = self.state();`), or C++ fetching the current process for a
    /// handle lookup that Rust does in one call.
    pub plumbing: bool,
    /// For `if` units: the top-level `&&`/`||` operands of the condition,
    /// so an added or dropped test can be reported.
    pub conjuncts: Vec<Conjunct>,
    /// Calls through a type path, as `type::method` (`Foo::create`), to tell
    /// same-named methods of different types apart.
    pub qcalls: Vec<String>,
    /// For C++ doc comments: the same comment is still in the C++ after the
    /// change (on the declaration, or on the function that now calls into
    /// Rust), so it was not lost.
    pub still_in_cpp: bool,
}

/// One operand of a condition's top-level `&&` or `||`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Conjunct {
    /// Source text, whitespace collapsed.
    pub text: String,
    /// Normalized names it mentions.
    pub names: Vec<String>,
}

/// One aligned step of a function body: a statement, a comment, or the
/// header of a compound statement.
#[derive(Clone, Debug)]
pub struct Unit {
    pub kind: UnitKind,
    /// First source line (1-based, file coordinates).
    pub start_line: usize,
    /// Last source line, inclusive.
    pub end_line: usize,
    /// Nesting depth within the function body.
    pub depth: usize,
    pub features: Features,
    /// Set when the unit comes from a different file than its function, such
    /// as a C++ doc comment on the declaration in a header.
    pub file: Option<String>,
    /// The unit's source lines when `file` is set.
    pub ext_lines: Vec<String>,
}

/// A function extracted from a source file.
#[derive(Clone, Debug)]
pub struct Function {
    pub lang: Lang,
    pub path: String,
    /// Display name, e.g. `VmObject::CommitRange` or `VmObject::commit_range`.
    pub name: String,
    /// The unqualified name.
    pub base: String,
    /// The enclosing class, or the `impl` type in Rust.
    pub class: Option<String>,
    /// First line, including leading comments and attributes.
    pub start_line: usize,
    pub end_line: usize,
    /// Source lines `start_line..=end_line`, tabs expanded.
    pub lines: Vec<String>,
    pub units: Vec<Unit>,
    /// Normalized names of everything the body calls.
    pub calls: Vec<String>,
    /// Calls through a type path, as `type::method`.
    pub qcalls: Vec<String>,
    /// Rust `extern "C"` or `#[no_mangle]` function.
    pub is_ffi: bool,
    /// Rust function with a `pub` visibility modifier (`pub`, `pub(crate)`, etc.).
    pub is_pub: bool,
    /// Rust inside a `#[cfg(test)]` (or `ktest`) or `tests` module: test
    /// code, never the port of production C++.
    pub test_only: bool,
    /// C++ test: the name the test runner shows for it, the first argument
    /// of `UNITTEST("name", Fn)` or `Name` in `TEST(Suite, Name)`.
    pub test_name: Option<String>,
}

impl Function {
    /// The text of a source line in file coordinates.
    pub fn line(&self, line: usize) -> &str {
        line.checked_sub(self.start_line)
            .and_then(|i| self.lines.get(i))
            .map(String::as_str)
            .unwrap_or("")
    }

    /// `path:start-end`.
    pub fn location(&self) -> String {
        format!("{}:{}-{}", self.path, self.start_line, self.end_line)
    }
}
