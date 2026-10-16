# babeldiff

`babeldiff` is a diff tool for reviewing changes that port C++ to Rust. It was
built for the Zircon kernel's in-place conversion, where each change replaces
a C++ implementation with Rust (called through FFI) that is meant to follow
the C++ line for line.

It parses both languages, pairs each removed C++ function with the Rust
function that replaces it, lines the two up side by side, and flags places
where the Rust does not do the same thing:

- **comments** that were lost, added, or reworded
- **error returns** that differ in code or order, and failures that one side
  handles and the other propagates
- **locks** taken in a different place or on a different lock
- **control flow** (`if`, loops, `switch`/`match`, `case` arms, `return`, `break`)
  with no counterpart
- **steps** (calls) that only one side performs

The output is plain text in the style of `diff -y`, meant to be read by people
and by agents, a self-contained HTML page, or JSON.

Its job is to help reviewers find real mistakes in a conversion, so it errs
toward precision: differences that are usually just translation (a renamed
operand, a reworded comment, a helper call spelled differently, a comment that
stays in the C++) are notes, and only differences a reviewer must check are
issues.

## Install

```sh
cargo install --path .
```

This builds a single static binary; tree-sitter grammars for C++ and Rust are
compiled in.

## Usage

```sh
# In a Fuchsia checkout: compare a commit with its parent, or a range.
babeldiff git HEAD
babeldiff git origin/main..my-branch

# A Fuchsia Gerrit change (CL number, patchset, fxr/ prefix, or review URL).
# Downloads the patch from fuchsia-review.googlesource.com with full context.
babeldiff cl 1885120
babeldiff cl 1885120/5
babeldiff cl fxr/1885120
babeldiff --cl 1885120

# A patch file (git format-patch, git show, or git diff output), or stdin.
babeldiff patch change.patch
git show HEAD | babeldiff patch

# With -C, patch mode reads full files from the repository by the blob ids in
# the patch, and can look up C++ that the change did not touch.
babeldiff patch -C ~/fuchsia --base origin/main change.patch

# Compare every function in a set of files.
babeldiff files handle_table.cc handle_table.rs
```

Useful options:

| Option | Effect |
| --- | --- |
| `--cl <CL>`, `--review`, `--gerrit` | Download and compare a Fuchsia Gerrit change (e.g. `1885120`, `1885120/5`, `fxr/1885120`, or URL). |
| `--format html -o report.html` | Write a self-contained HTML page for reviewing in a browser (see below). |
| `--layout stacked` | Print each C++ unit above the Rust it aligns with, untruncated. Better for agents and narrow terminals. |
| `--format json` | Machine-readable findings for agents (see below). |
| `--issues-only` | Leave out notes, and pairs with no issues. |
| `--summary` | Only the per-function summaries and findings. |
| `-U N`, `--context N` | Only rows within N rows of a difference. |
| `--width N` | Width of the side-by-side view (default `$COLUMNS` or 160). |
| `--pair Cpp=rust` | Force a pairing the matcher missed. Repeatable. |
| `--min-changed F` | Fraction of a function's lines a change must touch for it to be compared (default 0.25). |
| `--no-search` | Don't search the repository for unchanged C++. |

The exit status is 0 when there are no issues, 1 when there are, and 2 on
error, like `diff`.

## Reading the output

```
==== FifoDispatcher::WriteFromUser  <->  FifoDispatcher::write_from_user
  C++   zircon/kernel/object/fifo_dispatcher.cc:14-23
  Rust  zircon/kernel/object/fifo_dispatcher.rs:13-29
  via   C++ now calls FFI shim rust_fifo_dispatcher_write_from_user  zircon/kernel/object/fifo_dispatcher_ffi.rs:11-30
  note  C++ lines marked * are the declaration's comment in zircon/kernel/object/include/object/fifo_dispatcher.h
  similarity 0.95, 1 issue, 1 note

   17* // Writes |count| elements of |elem_size| bytes from |ptr| into the pee… =     13 /// Writes `count` elements of `elem_size` bytes from `ptr` into the pe…
   19* // Returns ZX_ERR_OUT_OF_RANGE if |elem_size| does not match the fifo.   =     15 /// Returns ZX_ERR_OUT_OF_RANGE if `elem_size` does not match the fifo.
    14 zx_status_t FifoDispatcher::WriteFromUser(size_t elem_size, user_in_ptr… =     16 pub fn write_from_user(
    16   canary_.Assert();                                                      =     22     self.canary.assert();
    18   Guard<CriticalMutex> guard{get_lock()};                                =     24     ksync::lock!(let guard = self.lock());
    19   if (!peer()) {                                                         =     25     let Some(peer) = self.peer(&guard) else {
    20     return ZX_ERR_PEER_CLOSED;                                           !     26         return Err(Status::BAD_STATE);
        ^ ! error code differs: C++ returns PEER_CLOSED, Rust returns BAD_STATE
    22   return peer()->WriteSelfLocked(elem_size, ptr, count, actual);         ~     28     peer.write_self_locked(&guard, elem_size, ptr, count)
        ^ ~ only C++ calls peer

  errors   DIFFERENT
           C++:  PEER_CLOSED
           Rust: BAD_STATE
  locks    same
           C++:  lock
           Rust: lock
  flow     if 1, return 2
  comments 2 C++ comments: 2 identical, 0 reworded, 0 missing in Rust
  findings
    ! fifo_dispatcher.cc:20 | fifo_dispatcher.rs:26  error code differs: C++ returns PEER_CLOSED, Rust returns BAD_STATE
    ~ fifo_dispatcher.cc:22 | fifo_dispatcher.rs:28  only C++ calls peer
```

Each pair has a header, the aligned source, a summary, and a findings list
with `file:line` references on both sides. The marker column is:

| Marker | Meaning |
| --- | --- |
| `=` | Aligned and equivalent. |
| `~` | Aligned, with a note: a difference that is often just translation (reworded comment, different helper calls). |
| `!` | Aligned, with an issue: different error code, lock, propagation, or success/failure. |
| `<` | Only in C++. |
| `>` | Only in Rust. |

Rust safety comments (`// SAFETY: ...` and `# Safety` doc sections) are
expected additions: they are shown, marked `>`, with no finding, and never
aligned with a C++ comment. Doc comments on `pub` Rust functions where the
C++ function had none are also expected additions.

Locking is compared through ksync's differences from C++. `ksync::lock!(let g
= self.lock.lock())`, `self.read_lock()`, `self.write_lock()` and a
`#[guarded]` struct's `self.lock_mu()` count as taking the lock that C++'s
`Guard<...> guard{&lock_}` takes. Lock tokens are treated as bookkeeping and
shown with no finding: `let token = guard.token()`, a forged
`LockToken::new()` in an FFI shim whose C++ caller holds the lock, and binding
a guarded field with `self.field.get(token)`. A guarded field read through
its `KCell` (`self.flashes.get(&token)`) matches the C++ member (`flashes_`).
`guard_<lock>(&token)` and `fields_mut()` are field access, not acquisitions.
Clang thread-safety annotations (`TA_REQ`, `TA_GUARDED`) are ignored on the
C++ side, since in Rust they become token parameters.

Findings start with `!` (issue) or `~` (note), so `grep '^    !'` lists the
issues. The first lines of the report count issues by kind. At the end the
report lists functions it could not pair (Rust tests separately, and Rust
helpers with the paired functions that call them) and the FFI shims it recognized,
including shims it could not resolve (`-> ambiguous: A or B`), which
`--pair` settles.

Each finding has a kind, which maps to the part of the porting rubric it
checks:

| Kind | What it covers |
| --- | --- |
| `comment` | A comment lost, added, reworded, or still using C++ trailing-underscore member names (`foo_`) or `kPascalCase` constants (`kOutOfMemory`) that were renamed in Rust. Each lost comment is its own finding. |
| `error-path` | Error codes, propagation, and success versus failure. |
| `lock` | Locks taken or released. |
| `control-flow` | Branches, loops, returns, and tests added to or dropped from a condition. |
| `call` | Calls one side makes and the other doesn't. |
| `assert` | Assertions. A dropped assert is an issue. |
| `trace` | Trace and debug printing. A dropped trace is an issue. |
| `order` | The same step at a different position. |
| `atomic` | Memory ordering of atomic operations. A Rust ordering weaker than the C++ one (which is `seq_cst` when unstated) is an issue. |
| `value` | Named constants, flags and masks on one side only, when the other function never mentions them: `Rust also clears X86_FLAGS_RF`, `C++ sets X86_DR6_MASK here, and the Rust doesn't`, or a statement that uses a constant the other side never does. A literal replaced by a constant is a note, since it may be the same value. |
| `pairing` | How functions were paired. A function the change defines twice (in two Rust modules) is paired with the C++ once per copy, and an issue says so when the copies differ. |
| `unsafe` | A note on a function that adds three or more `unsafe` blocks where the C++ needed none, which usually means a safe facade is missing. |

Some checks look at the structure the Rust keeps:

- A C++ function-like macro used as a statement (`COPY_COMMON_REGS(out, in);`)
  that the Rust expands inline is one issue per function: keep it as a
  `macro_rules!`.
- A local lambda or a helper in the same C++ file that the C++ calls at two
  or more places and the Rust never calls is one issue ("inlined at 3
  places"). A converted helper called once whose Rust port the aligned Rust
  statement doesn't call is an issue too.
- A call the C++ makes only inside `DEBUG_ASSERT` that the Rust makes
  unconditionally, and a C++ `ASSERT(false)` or `PANIC` that the Rust turns
  into an error return, are `assert` issues.
- An `if` on one side only whose body returns is one finding, "Rust adds a
  check `x.is_null()` returning INVALID_ARGS", not one for the `if` and one
  for the `return`. Several C++ checks merged into one Rust `if a || b` are
  not missing.
- A C++ `switch` lines up with a Rust if/else-if chain case by case
  (`default` with `else`).
- A closure passed to a function that takes a lock around it
  (`let f = |t| {...}; with_chain_lock(t, f)`) lines up with the C++ guard
  and the statements under it, with a note that the lock is taken through
  a callback.

Separately, babeldiff runs rubric lints over the changed files. They need
no pairing, and they are listed at the end of the report, under
`lints` in JSON, and in their own HTML section:

| Lint | What it checks |
| --- | --- |
| `extern-signature` | A Rust `extern "C"` declaration of a `cpp_*` helper, or a `rust_*` export, has the parameters and return type of the C++ side: the same count, pointer depth and scalar widths, and each `cpp_*` helper defined in a `.cc` file has an `extern "C"` prototype declaration in a C++ header. A mutable pointer the receiving side may write through while the other treats it as const is a note. |
| `unsafe-safety` | Every `unsafe` block and `unsafe impl` has a `// SAFETY:` comment, and every `unsafe fn` a `# Safety` doc section. Blocks are reported once per function. |
| `shim-logic` | A `rust_*` shim or `cpp_*` helper only forwards. Branches, loops and `match` arms are reported unless they only convert a result, a status, or a null pointer or empty optional. |
| `file-placement` | Each C++ file becomes one Rust file named after it (`foo.cc` to `foo.rs`, `dir_foo.rs` for a flattened directory, or a part named `foo_*.rs`). A C++ file split across several Rust files, and functions that landed in a file not named after their C++ file, are issues; a Rust file that collects three or more C++ files is a note. Test files and `_ffi` companions don't count. The report also maps each C++ file to the Rust files its functions went to, under `File placement`. |
| `provenance-comment` | Comments that say where code was ported from ("Ported from foo.cc", "(`foo.cc:12-34`)", "Mirrors the C++ version"), reported once per file. Consecutive comment lines are matched as one comment, so wording wrapped across lines is found. `// SAFETY:` comments, keep-in-sync comments, and TODO or "must match" paragraphs are exempt. |
| `invented-lifetime` | A reference made from a raw pointer that was taken from a place still in scope (`let p = buf.as_mut_ptr(); ... &mut *p`), or a `&mut *self.field.get()` borrow held across a `self.method()` call or another `self.field.get()` access and used again afterwards, whose lifetime the compiler can't check. |
| `unsafe-density` | A note on a file whose changed functions have ten or more `unsafe` blocks between them. |

A lint issue counts toward the exit status like any other issue.

`tests/fixtures/fifo/expected.txt` is a complete example; its Rust contains
four planted mistakes (a different error code, a rollback replaced by `?`, a
lock moved ahead of the argument checks, and a dropped comment).
`tests/fixtures/doorbell` is a class hierarchy folded into one Rust type with
an enum, with four more (a test added to a condition, a dropped trace, a
dropped comment in one override, and a different error code); babeldiff
reports exactly those four as issues. It also plants one of each rubric
lint. `tests/fixtures/lantern` plants the structural mistakes: a function
defined twice with one copy dropping a mask, constants and masks on one
side only, a macro and a lambda expanded inline, a call moved out of
`DEBUG_ASSERT`, `ASSERT(false)` turned into an error, an added check, code
in a file not named after its C++ file, an invented lifetime, and "Ported
from" comments.

## JSON output

`--format json` prints one object for agents and scripts. Each finding has a
stable id (`<C++ function>#<n>`), its severity, kind and rubric reference, the
message, and `path`, `line` and source `text` on each side. Each pair says how
it was found (`link`: `ffi`, `ffi-name`, `similarity` or `forced`), why
(`rationale`), and which C++ overrides it folds in. Unpaired functions, FFI
shims (with `ambiguous` candidates when babeldiff could not choose) and C++
helpers called from Rust are listed at the end. `version` is bumped only when
a field changes meaning.

```sh
babeldiff git HEAD --format json --issues-only > findings.json
```

## HTML report

`--format html` writes one self-contained HTML file, with no external scripts,
styles or fonts. You can mail it, attach it to a review, or open it from disk.

```sh
babeldiff git HEAD --format html -o review.html
```

The page is laid out so that a reviewer's attention goes to what differs:

- A sidebar lists every function pair with a red, amber or green dot and its
  issue and note counts, and tracks which pairs you have marked reviewed.
  That state is kept in your browser's local storage.
- Each pair opens with cards for the four checks (error returns in order,
  locks, control flow and comments). The cards that differ come first and are
  red. After them comes the list of findings, which link to their rows.
- The aligned code has C++ on the left and Rust on the right. Matching rows
  stay plain, notes get a faint amber tint, and issues are red with the
  message spelled out. Code on only one side is set against a hatched blank.
  Closing braces and other lines the alignment skips are dimmed.
- Hovering an identifier highlights it on both sides under either spelling
  (`subscriber_count_`, `subscriber_count`, `kMaxSubscribers` and
  `MAX_SUBSCRIBERS`), and error codes (`ZX_ERR_NO_MEMORY` and
  `Status::NO_MEMORY`) highlight together.
- "Hide equivalent rows" collapses every run of two or more rows with nothing
  to review (equivalent lines and expected Rust additions such as `SAFETY:`
  comments) into a thin bar you can click to open. The "Show" menu picks all
  functions, those with issues or notes, or those with issues.
- Keyboard: `j`/`k` next and previous difference, `n`/`p` next and previous
  function, `x` mark reviewed, `f` hide equivalent rows, `i` cycle which
  functions are shown, `?` help.

The page follows the system's light or dark setting, stacks the two languages
on narrow screens, and prints cleanly.

## How it works

1. **Inputs.** The change is turned into the C++ files as they were before and
   the Rust files after, with the lines the change touched. In `git` mode
   files are read at both revisions; in `patch` mode they are rebuilt from the
   hunks (or read by blob id with `-C`).
2. **Units.** Each function is parsed with tree-sitter and flattened into
   units: comments, statements, and the headers of `if`/`else`/loops/`switch`/
   `match`/`case`, plus `return`/`break`/`continue`. Each unit gets
   language-neutral features: normalized calls and identifiers
   (`CommitRange` = `commit_range`, `lock_` = `self.lock`, `kFoo` = `FOO`),
   error codes (`ZX_ERR_NO_MEMORY` = `Status::NO_MEMORY`), locks
   (`Guard<Mutex> guard{&lock_}` = `self.lock.lock()` = `ksync::lock!(...)`),
   whether it propagates an error, and comment words.
   Common idioms are normalized so they line up:
   `if (status != ZX_OK) return status;` reads as `?`, and so do a status
   chained through `if (status == ZX_OK) status = B();` to a final
   `return status;` and a status set in each branch and checked once;
   `status = foo(); if (status != ZX_OK) { ... }` lines up with
   `let status = foo(); if let Err(status) = status { ... }` and with
   `let h = match foo() { Ok(h) => h, Err(status) => { ... } };`;
   `if (!ac.check()) return ZX_ERR_NO_MEMORY;` after an allocation reads as
   `try_new(..).ok_or(NO_MEMORY)?`;
   `return c ? A : B;` as `if c { A } else { B }`,
   `*out = x; return ZX_OK;` as `Ok(x)`, `do { ... } while (c);` as
   `loop { ... if !c { break; } }`, `case A: case B:` as `A | B =>`, and
   nested `extern "C" fn` callback bodies as C++ lambda bodies. A C++ class
   with no explicit constructor that initializes fields via in-class default
   member initializers synthesizes a `ClassName::ClassName` constructor so its
   fields align with a Rust constructor returning `Self { ... }` or
   `pin_init!(Self { ... })`.
   Declarations with no initializer, pure bindings such as
   `let state = self.state();`, out-parameter writes and thread-safety
   assertions are bookkeeping, not steps.
3. **Pairing.** C++ functions are paired with Rust functions by, in order:
   explicit `--pair`s; the FFI shim pattern (the new C++ body calls
   `rust_<class>_<method>`, whose `#[no_mangle]` or `pub extern "C"` definition
   forwards to the Rust method, or the shim's name spells the C++ function's;
   private `extern "C"` callbacks without `#[no_mangle]` pair as ordinary
   functions); the same class and method name; then a score combining name
   similarity and body similarity, where comments count double. When a shim
   calls several same-named methods (`A::create` and `B::create`), the type path
   decides, and if nothing does the shim is reported as ambiguous rather than
   guessed. A `#[no_mangle]` function that does the work itself is the port,
   and a C++ one-line trampoline hands the shim to the C++ it calls.
   C++ overrides of a method in related classes (found from the class
   hierarchy) are folded into the one Rust function that replaced virtual
   dispatch with an enum and a `match`, and each override is aligned against
   the arm that carries it. Rust functions still unpaired are looked up in
   the repository (files named like the Rust file, then `git grep` for the
   CamelCase and snake_case names), so Rust that ports C++ the change did not
   delete is still compared, and C++ still unpaired is matched with an
   untouched Rust function of the same name. Doc comments on C++
   declarations in headers are attached to the definitions, since that is
   where Rust doc comments come from (an override without one takes its base
   class's). A comment that is still in the C++ after the change, in a
   changed file or word for word in one the change did not touch, is not
   reported as lost.
4. **Alignment.** Units are aligned with an order-preserving weighted LCS over
   unit similarity. Units left between two aligned rows are then lined up by
   position when each side has as many of a kind, so a renamed condition or
   a `case` turned `match` arm is one changed step. Aligned units are
   compared feature by feature, and `if` conditions test by test; unaligned
   ones are reported, and a unit that resembles one on the other side at a
   different position is reported as possibly reordered. A C++ `default:`
   arm that only panics, breaks or returns a fallback value is noted rather
   than flagged when paired with an exhaustive Rust `match` that covers
   every case.

## Library

The crate is a library with a thin CLI on top:

```rust
use babeldiff::{analyze, input::ChangeSet, render};

let cs = ChangeSet::from_files(&[(cpp_path, cpp_text), (rust_path, rust_text)]);
let report = babeldiff::run(&cs, &analyze::Options::default(), &mut analyze::NoFinder);
for pair in &report.pairs {
    for finding in &pair.findings {
        println!("{:?} {}", finding.severity, finding.message);
    }
}
print!("{}", render::render(&report, &render::RenderOptions::default()));
let page = babeldiff::html::render_html(&report, &Default::default());
```

`babeldiff::git::Git` and `babeldiff::git::RepoFinder` provide the git
integration; implement `analyze::CppFinder` to look up C++ some other way.

## Limitations

- A C++ function split across several Rust functions (or the reverse) is
  paired with its best match only; the rest shows as unpaired.
- Macros are compared by name and the calls inside their arguments, not
  expanded; a macro the Rust expands inline is reported, but its expansion
  is not compared with the macro's body.
- Similarity thresholds are tuned on about 55 Zircon conversion changes; use
  `--pair` when the matcher gets a pairing wrong.
- It compares functions. C++ that stays C++ but changes, FFI declarations,
  and class and field comments are not checked.

## Development

```sh
cargo test          # unit tests and golden tests over tests/fixtures
BLESS=1 cargo test  # rewrite expected outputs after an intentional change
```
