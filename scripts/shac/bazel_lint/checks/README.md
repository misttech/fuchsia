# Adding a Bazel Lint Check

Each check is implemented as a small module in this directory that subclasses
`BazelCheck` and registers itself with `@register_check`.

## Step-by-Step Guide

1. **Create a check module** in `scripts/shac/bazel_lint/checks/<rule>.py`
   subclassing `BazelCheck` from `framework` (see
   [`BazelCheck` Attributes](#bazelcheck-attributes) below) and decorating it
   with `@register_check`.
2. **Import the module** in [`__init__.py`](./__init__.py) so the check is
   registered when `bazel_lint` loads.
3. **Register the `shac` check** in
   [`//scripts/shac/bazel.star`](../../bazel.star) using `_make_bazel_lint_check`
   and `shac.register_check(...)` (in alphabetical order). Pass the file globs
   matching your check's `file_kinds` to `applies_to_globs` (e.g.
   `_make_bazel_lint_check("my_check", applies_to_globs = _BUILD_FILES)`).
   Leave `enforcement` at its default (`"all"`) during development and verification.
4. **Add tests and build targets** (kept in sync with
   [`__init__.py`](./__init__.py) via `LINT.IfChange`):
   * Add a Starlark check test file in
     `scripts/shac/bazel_lint/checks/tests/<rule>_test.star` and invoke its
     test runner function in [`tests/checks_test.star`](./tests/checks_test.star)
     (which is executed by
     [`//scripts/shac/tests/bazel_test.star`](../../tests/bazel_test.star)).
   * Update build targets so the new files are staged into the runfiles of
     `//scripts/shac/tests:bazel_test`:
     * Add `<rule>.py` to [`BUILD.bazel`](./BUILD.bazel).
     * Add `<rule>_test.star` to [`tests/BUILD.bazel`](./tests/BUILD.bazel).
   * Structure tests with clear separation between passing and failing cases:
     * Separate tests that produce findings from tests that assert no findings
       (`asserts.eq(res.findings, ())`).
     * In tests producing findings, keep the test file narrow so each line produces
       a finding on consecutive line numbers, making it easy to spot if a finding
       is missed. Avoid mixing passing lines into a multi-finding file.
     * Use dedicated test functions for each category: allowed syntax, policy
       exemptions, and unsupported file types. (Framework-level exclusions such
       as `third_party/**/src/**` are tested in framework test suites rather
       than individual check tests.)
5. **Verify the check**:
   Whenever adding or modifying a check, run the following tests:
   * **Starlark check tests (for fast iteration)**:
     To test only your check:
     ```sh
     fx host-tool shac test scripts/shac/bazel_lint/checks/tests/<rule>_test.star
     ```
     To test all checks, run one of the following (the latter also runs the other Bazel shac tests):
     ```sh
     fx host-tool shac test scripts/shac/bazel_lint/checks/tests/checks_test.star
     fx host-tool shac test scripts/shac/tests/bazel_test.star
     ```
   * **Hermetic Bazel test (required final validation)**:
     ```sh
     fx test --host @//scripts/shac/tests:bazel_test
     ```
     This runs `bazel_test.star` inside Bazel's hermetic sandbox. While individual
     `shac test` commands above are faster for iteration, this test MUST pass
     before landing to ensure all necessary Python modules, AST rules, and test
     files are declared in `BUILD.bazel` files and staged in runfiles.
     If the target is not recognized by `fx test`, register it first by running:
     ```sh
     fx add-host-test //scripts/shac/tests
     ```
   * **Full-workspace check (`--all`)** (also run this whenever modifying an
     allowlist):
     ```sh
     fx host-tool shac check --only <check_name> --all
     ```
     A new check MUST have no findings for this check (errors, warnings or
     notices) across the entire workspace before landing so that developers
     modifying an existing file do not encounter unrelated failures. Any
     existing violations in the tree must either be fixed first or added to
     [`allowlists/<check_name>.json`](../allowlists/README.md) with a tracking
     bug.
6. **Configure rollout enforcement**:
   Before uploading the CL, update the `_make_bazel_lint_check(...)` declaration
   in [`//scripts/shac/bazel.star`](../../bazel.star) to set:
   ```starlark
   enforcement = "unchanged_lines_warning_only"
   ```
   This ensures that pre-existing `"error"` findings on unchanged lines and
   untouched files are reported as `"warning"` rather than `"error"`, preventing
   CQ/CI failures or closing the tree due to race conditions with in-flight CLs
   that passed CQ before your check lands.

   When committing the new check, use the hermetic `fx test` and `--all`
   commands from Step 5 as `Test:` footers in the commit message:
   ```
   Test: fx test --host @//scripts/shac/tests:bazel_test
   Test: fx host-tool shac check --only <check_name> --all
   ```
7. **Promote to full enforcement after rollout**:
   After landing a new or modified check, allow the period for which CQ results
   can be reused (currently 24 hours) then:
   * Sync your checkout (`jiri update`).
   * Remove `enforcement = "unchanged_lines_warning_only"` from the check's
     `_make_bazel_lint_check(...)` call in
     [`//scripts/shac/bazel.star`](../../bazel.star) (restoring the default
     `"all"`).
   * Run the full-workspace check:
     ```sh
     fx host-tool shac check --all --only <check_name>
     ```
   * If new violations are found, fix them or update
     [`allowlists/<check_name>.json`](../allowlists/README.md).
   * Land the change. Multiple checks can be updated at once.

## `BazelCheck` Attributes {:#bazelcheck-attributes}

* `name: str` - Must match the `shac` check name in `bazel.star` (e.g.
  `"bazel_target_visibility"`). Also determines the optional allowlist filename
  (`allowlists/<name>.json`).
* `file_kinds: frozenset[FileKind]` - The file kinds this check applies to
  (`FileKind.BUILD`, `FileKind.BZL`, `FileKind.MODULE`, `FileKind.WORKSPACE`,
  `FileKind.PYTHON`). **Important**: Files matching each declared `FileKind`
  must be included in the globs passed to
  `_make_bazel_lint_check(..., applies_to_globs=...)` when registering the check
  in `bazel.star`. If a glob is omitted in `bazel.star`, `shac` will never pass
  those files to the linter, even if the check includes the `FileKind`.
* `policy_exempt_globs: tuple[str, ...]` - Optional repository-relative glob
  patterns using Bazel `glob()` syntax (evaluated via
  `PurePosixPath.full_match`, where `*` matches within a single path segment and
  `**` matches zero or more path segments, e.g.
  `"build/bazel_sdk/bazel_rules_fuchsia/**"` or `"build/config/**"`; patterns
  must not end with `"/"`) for paths that are permanently exempt by policy (no
  tracking bug required). Authors should clearly document the reason for each
  exemption in comments. Use [`allowlists/`](../allowlists/README.md) instead
  for temporary exceptions that require a tracking bug.
* `supports_targets_allowlist: bool` - Controls whether entries in
  `allowlists/<name>.json` that specify a single file `"path"` may also specify
  a `"targets"` list to exempt only specific rule/macro targets in that file
  rather than the entire file:
  * Set to `True` when a check's findings are associated with named targets
    (e.g., `call.target_name` in `check_call()` or `s.enclosing_call.target_name`
    in `check_string()`) and the check passes `targets=(...)` to `fc.report()`
    or `fc.report_string()`.
  * Leave as `False` (the default) for file-wide or load-level checks where
    findings do not belong to a specific target (such as `package()` checks or
    `.bzl` `visibility()` checks). When `False`, `load_allowlist()` rejects
    `"targets"` in the JSON allowlist so ineffective target-scoped entries
    cannot be added.

## Semantic Hook Points & Examples

`FileContext` parses each file once and invokes up to four hooks on your check:

* `check_module(self, fc: FileContext)`
* `check_load(self, fc: FileContext, load: LoadInfo)`
* `check_call(self, fc: FileContext, call: CallInfo)`
* `check_string(self, fc: FileContext, s: StringInfo)`

*Note*: Checks that include `FileKind.PYTHON` in `file_kinds` may not implement
`check_load`. Doing so raises a `TypeError` during `@register_check` (because
Python files do not use Starlark `load()` statements).

### 1. `check_module(self, fc: FileContext)`

Called once per file for whole-file assertions (such as requiring a top-level
statement).

```python
def check_module(self, fc: FileContext) -> None:
    has_visibility = any(
        isinstance(stmt, ast.Expr)
        and isinstance(stmt.value, ast.Call)
        and isinstance(stmt.value.func, ast.Name)
        and stmt.value.func.id == "visibility"
        for stmt in fc.tree.body
    )
    if not has_visibility:
        fc.report(
            "All .bzl files must declare load visibility() at the top level.",
            level="error",
            url="https://fuchsia.dev/fuchsia-src/development/languages/bazel/style/bzl_files#load-visibility",
        )
```

### 2. `check_load(self, fc: FileContext, load: LoadInfo)`

Called for every `load()` statement. `load.module_label` is the source `.bzl`
label, and `load.symbols` is a tuple of `(local_name, exported_name, ast_node)`.

```python
def check_load(self, fc: FileContext, load: LoadInfo) -> None:
    if load.module_label == "@platforms//host:constraints.bzl":
        for _local, exported, sym_node in load.symbols:
            if exported == "HOST_CONSTRAINTS":
                fc.report(
                    "Do not use HOST_CONSTRAINTS; load HOST_OS_CONSTRAINTS from "
                    "//build/bazel/platforms:constraints.bzl instead.",
                    sym_node,
                    level="error",
                    url=None,
                )
```

### 3. `check_call(self, fc: FileContext, call: CallInfo)`

Called for every non-`load()` function, rule, or macro call. `call.func_name`
normalizes `native.<rule>` to `<rule>`, `call.keywords` maps keyword names to
`ast.keyword` nodes, `call.forwards_kwargs` is `True` when `**kwargs` is
forwarded, and `call.target_name` holds the string value of `name = "..."` when
present.

```python
def check_call(self, fc: FileContext, call: CallInfo) -> None:
    # Inspect a keyword argument value and choose finding severity:
    if call.func_name == "package" and "default_visibility" in call.keywords:
        kw = call.keywords["default_visibility"]
        is_broad = _has_broad_visibility(kw.value)
        fc.report(
            "Do not set default_visibility in package(); specify visibility on individual targets.",
            kw,
            level="error" if is_broad else "warning",
            url="https://fuchsia.dev/fuchsia-src/development/languages/bazel/style/build_files#avoid-package-default-visibility",
        )

    # Require a keyword argument unless a macro forwards **kwargs:
    if call.func_name in ("config_setting", "exports_files"):
        if "visibility" not in call.keywords and not call.forwards_kwargs:
            fc.report(
                f"{call.func_name}() must explicitly specify 'visibility'.",
                call.node,
                level="error",
                url="https://fuchsia.dev/fuchsia-src/development/languages/bazel/style/common#visibility",
                targets=(call.target_name,) if call.target_name else (),
            )
```

### 4. `check_string(self, fc: FileContext, s: StringInfo)`

Called for every string literal in the file:

* **String literals only**: Only checks string constants (`ast.Constant` with a
  `str` value). Variables, string formatting, or concatenation involving
  variables (e.g. `prefix + "/foo"`) cannot be inspected.
* **Concatenation and formatting**: When string literals are concatenated with
  `+` (e.g. `"foo" + "bar"`), each operand is visited as a separate
  `ast.Constant` node (the AST does not perform constant folding). For example,
  `check_string()` is called separately for `"foo"` and `"bar"`, and neither
  call sees `"foobar"`. In contrast, adjacent string literals (e.g.
  `"foo" "bar"`) are merged into a single `ast.Constant` by the parser at parse
  time, so `check_string()` is called once with `"foobar"`. Multiline strings
  preserve newline characters (`\n`), and whitespace inside strings is
  unmodified and un-normalized.
* **Included strings**: Every string constant in the parsed AST, except
  module-level `load()` arguments (which are passed to `check_load()` instead).
* **Docstrings**: Module, class, and function docstrings are included and have
  `s.is_docstring == True`. Checks that do not want to inspect docstrings should
  return early when `s.is_docstring` is `True`. Note that Bazel `doc` parameter
  strings (such as `doc = "..."` on `rule()`, `macro()`, `aspect()`, or
  `attr.*()`) are *not* docstrings; they are normal string literals with
  `s.is_docstring == False` and `s.keyword_arg == "doc"`. Checks that wish to
  ignore or inspect Bazel documentation attributes can check
  `if s.keyword_arg == "doc": ...`.
* **Comments**: `#` comments are stripped by `ast.parse()` during tokenization
  and are never passed to `check_string()` or any other hook point function.

`s.enclosing_call` is the nearest enclosing `CallInfo`, `s.keyword_arg` is the
enclosing keyword name (if any), `s.in_compare` is `True` inside comparison
expressions (`==`, `!=`, `in`), and `s.in_fail` is `True` inside `fail(...)`.

```python
def check_string(self, fc: FileContext, s: StringInfo) -> None:
    if s.is_docstring:
        return
    if s.value == "//visibility:public" and not (s.in_compare or s.in_fail):
        targets = (
            (s.enclosing_call.target_name,)
            if (s.enclosing_call and s.enclosing_call.target_name)
            else ()
        )
        fc.report_string(
            'Do not use "//visibility:public"; restrict visibility to narrow package groups.',
            s.node,
            s.value,
            level="error",
            url="https://fuchsia.dev/fuchsia-src/development/languages/bazel/style/common#do-not-use-public-visibility",
            targets=targets,
        )
```

## Emitting Findings

* `fc.report(message, node=None, *, level, url, targets=())`: Emits a finding
  spanning `node` (converting UTF-8 byte offsets to 1-based character columns),
  or a file-level finding when `node=None`.
* `fc.report_string(message, node, substring=None, *, level, url, targets=())`:
  Highlights `substring` (defaulting to `node.value`) inside a single-line
  string literal excluding quotes, falling back to `fc.report` for multi-line or
  concatenated strings.

### Documentation URLs (`url`)

Both `fc.report()` and `fc.report_string()` require a keyword-only `url`
argument (`str | None`):

* **Provide a URL whenever documentation exists**: If the check enforces a
  section of the Fuchsia Bazel style guide (under
  `//docs/development/languages/bazel/style/`) or another documentation page,
  pass its `https://fuchsia.dev/fuchsia-src/...` URL. `fc.report*` automatically
  appends `" See <url>."` to `message`. Pass `url=None` only when no relevant
  documentation exists.
* **Verify explicit Markdown anchors**: When linking to a section heading in a
  Markdown file under `//docs/`, ensure the heading in the `.md` file defines an
  explicit `{:#anchor-name}` attribute and use that exact anchor to help ensure
  the anchor remains stable if the heading text changes. If the heading does not
  yet have an explicit anchor, you can add one in the same commit. Note that
  explicit anchors typically use dashes (`-`) rather than underscores (`_`) (for
  example,
  `### Do not use public visibility {:#do-not-use-public-visibility}`).

<!-- LINT.IfChange(file_context) -->
## `FileContext` Reference

Each hook receives a `FileContext` instance (`fc`) providing AST metadata and
traversal helpers:

### Attributes

* `fc.path: str` - Repository-relative path of the file being linted.
* `fc.kind: FileKind` - Classified `FileKind` (`BUILD`, `BZL`, `MODULE`,
  `WORKSPACE`, or `PYTHON`).
* `fc.tree: ast.Module` - Parsed Python/Starlark root AST node.
* `fc.lines: list[str]` - File contents split into lines without trailing
  newlines (1-indexed line numbers correspond to `fc.lines[lineno - 1]`).
* `fc.loaded_symbols: dict[str, tuple[str, str]]` - Mapping from local symbol
  name to `(module_label, exported_symbol_name)` for all symbols imported via
  `load()`, where `module_label` is normalized via `normalize_repo_prefix()`
  (converting `@@repo//...` to `@repo//...` and `@//...` to `//...`).
* `fc.loads: list[LoadInfo]` - Pre-extracted `LoadInfo` metadata for all
  `load()` statements in file order.
* `fc.calls: list[CallInfo]` - Pre-extracted `CallInfo` metadata for all
  non-`load()` function, rule, or macro calls in file order.
* `fc.strings: list[StringInfo]` - Pre-extracted `StringInfo` metadata for all
  string constants in file order.

### Traversal Methods

* `fc.parent(node: ast.AST) -> ast.AST | None`: Returns the parent AST node of
  `node`, or `None` if `node` is the root module.
* `fc.enclosing_statement_call(node: ast.AST) -> CallInfo | None`: Returns the
  outermost non-`load()` `CallInfo` enclosing `node` within the current
  statement, or `None` if `node` is not inside a call. Most checks should prefer
  `enclosing_statement_call` over `enclosing_call` because arguments inside a
  rule or macro call may be wrapped in helper calls such as `select(...)` or
  `glob(...)`; `enclosing_statement_call` returns the outer rule/macro call
  (which carries `target_name` and the rule's `func_name`), whereas
  `enclosing_call` returns the innermost call (`select` or `glob`). (`load()`
  statements are represented as `LoadInfo` in `fc.loads` rather than `CallInfo`
  in `fc.calls`, so nodes inside `load(...)` return `None`.)
* `fc.enclosing_call(node: ast.AST) -> CallInfo | None`: Returns the nearest
  (innermost) enclosing non-`load()` `CallInfo`, or `None` if `node` is not
  inside a call. Use this only when inspecting the immediate call directly
  wrapping `node` (such as checking whether `node` is an argument to
  `select(...)` or `glob(...)`); most checks should use
  `fc.enclosing_statement_call(node)` instead.
<!-- LINT.ThenChange(//scripts/shac/bazel_lint/framework.py:file_context) -->

## AST Metadata Classes (`*Info`)

<!-- LINT.IfChange(load_info) -->
### `LoadInfo`

Passed to `check_load(self, fc, load)`:

* `load.node: ast.Call` - Underlying AST `Call` node for the `load(...)`
  statement.
* `load.module_label: str` - Normalized first argument to `load()`, with leading
  `@@` normalized to `@` and `@//` normalized to `//` via
  `normalize_repo_prefix()` (e.g. `"@rules_cc//cc:defs.bzl"`). Almost all checks
  should match against `load.module_label` so they handle `@` and `@@` forms
  uniformly.
* `load.raw_module_label: str` - Exact un-normalized module label string as
  written in the `load()` statement (e.g. `"@@rules_cc//cc:defs.bzl"`). Use this
  when formatting finding messages or calling `fc.report_string(...)` so the
  emitted message and span match the source text.
* `load.symbols: tuple[tuple[str, str, ast.AST], ...]` - Tuple of loaded
  symbols where each entry is `(local_name, exported_name, symbol_ast_node)`.
  When aliasing is used (`local = "exported"`), `local_name` holds the alias.
<!-- LINT.ThenChange(//scripts/shac/bazel_lint/framework.py:load_info) -->

<!-- LINT.IfChange(call_info) -->
### `CallInfo`

Passed to `check_call(self, fc, call)`:

* `call.node: ast.Call` - Underlying AST `Call` node.
* `call.func_name: str` - Normalized function name. For direct calls
  (e.g. `cc_library(...)` or `my_macro(...)`), `func_name` matches the
  function identifier. For calls via `native.<rule>` (e.g.
  `native.cc_library(...)`), `func_name` is normalized to strip `native.`
  (becoming `"cc_library"`).
* `call.raw_func_name: str` - Exact function expression string (e.g.
  `"native.cc_library"` or `"my_macro"`). This is identical to `func_name` for
  all calls except those using the `native.` prefix.
* `call.target_name: str | None` - String value of `name = "..."` keyword
  argument if present as a string literal, otherwise `None`.
* `call.keywords: dict[str, ast.keyword]` - Mapping of keyword argument names
  to their `ast.keyword` AST nodes.
* `call.args: tuple[ast.expr, ...]` - Positional argument AST expressions.
* `call.forwards_kwargs: bool` - `True` if the call forwards `**kwargs`.
* `call.is_top_level: bool` - `True` if the call is an expression statement at
  the file's top level (not nested in a function, if-branch, or loop).
<!-- LINT.ThenChange(//scripts/shac/bazel_lint/framework.py:call_info) -->

<!-- LINT.IfChange(string_info) -->
### `StringInfo`

Passed to `check_string(self, fc, s)`:

* `s.node: ast.Constant` - Underlying AST `Constant` string node. Only string
  literals are inspected; variables, formatting, and variable concatenations
  cannot be checked. Operands of `+` literal concatenation appear as separate
  AST nodes (e.g. `"foo" + "bar"` is checked as separate `"foo"` and `"bar"`
  calls); adjacent string literals (e.g. `"foo" "bar"`) are merged into a single
  string (`"foobar"`) by the parser. Multiline strings preserve `\n` and
  whitespace is un-normalized.
* `s.value: str` - Decoded string value.
* `s.enclosing_statement_call: CallInfo | None` - Outermost non-`load()`
  `CallInfo` enclosing this string within the current statement, if any. Most
  checks should use `enclosing_statement_call` rather than `enclosing_call` so
  strings nested inside helper calls like `select(...)` or `glob(...)` still
  resolve to the enclosing rule/macro call (and its `target_name`).
* `s.enclosing_call: CallInfo | None` - Nearest (innermost) enclosing
  non-`load()` `CallInfo`, if any. Use this only when checking the immediate
  call wrapping the string (e.g., `select(...)`); most checks should use
  `s.enclosing_statement_call` instead. (`load()` statements are represented as
  `LoadInfo` rather than `CallInfo`, so strings inside `load(...)` have
  `enclosing_call = None` and `enclosing_statement_call = None`.)
* `s.keyword_arg: str | None` - Keyword argument name if this string is
  passed directly as or inside a keyword argument to
  `enclosing_statement_call` (or `enclosing_call` if different, e.g.
  `"visibility"` or `"doc"`).
* `s.is_load_source: bool` - `True` if this string is the module label of a
  `load()` statement.
* `s.is_docstring: bool` - `True` if this string is a Python/Starlark module,
  class, or function docstring. Note that Bazel `doc = "..."` keyword arguments
  on rules, macros, or attributes have `is_docstring == False` and
  `keyword_arg == "doc"`.
* `s.in_compare: bool` - `True` if inside a comparison expression (e.g.
  `x == "//visibility:public"`).
* `s.in_fail: bool` - `True` if inside a `fail(...)` call.
<!-- LINT.ThenChange(//scripts/shac/bazel_lint/framework.py:string_info) -->
