---
name: add-bazel-shac-check
description: >
  Add a new Python AST-based Bazel lint check to //scripts/shac/bazel_lint/
  and register it in //scripts/shac/bazel.star, including style guide anchors,
  allowlists, unit tests, and hermetic Bazel test verification.
---

# Adding a Bazel `shac` Lint Check

Use this skill when implementing a new Bazel static analysis check in
`//scripts/shac/bazel_lint/` or migrating an existing Starlark check in
`//scripts/shac/bazel.star` to the `bazel_lint` AST framework.

## 1. Read the In-Tree Reference Documentation First

Before writing code, read these files in `//scripts/shac/bazel_lint/`:

* [`//scripts/shac/bazel_lint/README.md`](../../../README.md): Architecture
  overview and test commands.
* [`//scripts/shac/bazel_lint/checks/README.md`](../../../checks/README.md):
  Step-by-step registration checklist, `BazelCheck` attributes, `FileContext`
  methods, and AST hook points (`check_module`, `check_load`, `check_call`,
  `check_string`).
* [`//scripts/shac/bazel_lint/allowlists/README.md`](../../../allowlists/README.md):
  JSON allowlist schema and field rules.

Inspect existing checks in `//scripts/shac/bazel_lint/checks/` (such as
`load_visibility.py`, `package_default_visibility.py`, and
`target_visibility.py`) and their tests in
`//scripts/shac/bazel_lint/checks/tests/` as reference examples.

---

## 2. Align with the Bazel Style Guide and Craft Messages

1.  **Locate the style guide section**: Check
    `//docs/development/languages/bazel/style/` (`common.md`, `build_files.md`,
    `bzl_files.md`, etc.) for the style guideline being enforced.
    * Verify that the target section heading defines an explicit
      `{:#anchor-name}` attribute using dashes (`-`) rather than underscores
      (`_`), e.g., `### Do not use public visibility
      {:#do-not-use-public-visibility}`.
    * If the heading lacks an explicit `{:#...}` anchor, add one in the same
      commit so the documentation URL remains stable if the heading text
      changes.
    * Pass the full
      `https://fuchsia.dev/fuchsia-src/development/languages/bazel/style/...#anchor-name`
      URL as `url=...` to `fc.report()` or `fc.report_string()`. Do not manually
      append `" See <url>."` to the message string; `fc.report*` appends it
      automatically.

2.  **Write actionable finding messages**:
    * State clearly what is disallowed or required and what the developer should
      do instead (for example, naming the Fuchsia wrapper macro or narrower
      visibility scope to use).

---

## 3. Design the Check Class (`checks/<check_name>.py`)

1.  **Choose `file_kinds` and matching `applies_to_globs`**: Every `FileKind` in
    `check.file_kinds` must match the `applies_to_globs` constant passed to
    `_make_bazel_lint_check()` in `//scripts/shac/bazel.star`:
    * `frozenset({FileKind.BUILD})` <-> `_BUILD_FILES` (use for `package()`
      checks that only apply to `BUILD.bazel` files).
    * `frozenset({FileKind.BZL})` <-> `_BZL_FILES` (use for `.bzl`-specific
      checks such as load `visibility()`).
    * `frozenset({FileKind.BUILD, FileKind.BZL})` <-> `_BUILD_AND_BZL_FILES`
      (use for target, rule-call, `load()`, and label checks, even when the
      guideline is documented in `build_files.md`, because macros in `.bzl`
      files also define targets).
    * `ALL_BAZEL_FILE_KINDS` (`frozenset({FileKind.BUILD, FileKind.BZL,
      FileKind.MODULE, FileKind.WORKSPACE})`) <-> `_ALL_BAZEL_FILES`
    * `main.py` exits with code 2 if `shac` passes a file whose `FileKind` is
      not declared in `check.file_kinds`.

2.  **Avoid false positives in AST hooks**:
    * **`check_module(self, fc)`**:
      * Prefer inspecting pre-extracted `fc.calls`, `fc.loads`, and `fc.strings`
        over manually walking `fc.tree.body` (for example, checking
        `is_load_visibility(call)` from `visibility_utils.py` to identify
        top-level `visibility(...)` declarations).
      * Omit `node` (or pass `node=None`) in `fc.report(...)` to emit a
        file-level finding.
    * **`check_string(self, fc, s)`**:
      * `check_string` visits all string constants in the file, including
        `load()` arguments (where the `.bzl` module label has `s.is_load_source
        == True`). If your check also implements `check_load` (or should not
        inspect `load()` module labels), check `if s.is_load_source: return` to
        avoid duplicate findings.
      * Check whether module/function docstrings (`s.is_docstring`), Bazel `doc
        = "..."` keyword arguments (`s.keyword_arg == "doc"`), comparisons
        (`s.in_compare`), or `fail(...)` messages (`s.in_fail`) should be
        skipped.
      * Prefer `fc.report_string(message, s.node, s.value, ...)` so the finding
        span highlights the string contents inside quotes.
      * When matching repository or root label prefixes on strings in
        `check_string`, call `normalize_repo_prefix(s.value)` (imported from
        `framework`) so canonical `@@repo//...` and `@//...` forms are matched
        automatically without duplicating `@` and `@@` constants (still pass
        `s.value` to `fc.report_string` so the span and message match the source
        text).
      * Use `s.enclosing_statement_call` (rather than `s.enclosing_call`) when
        inspecting the rule or macro call containing a string, because strings
        inside rule arguments may be nested inside helper calls like
        `select(...)` or `glob(...)` (`s.enclosing_call` returns the innermost
        call such as `select`, whereas `s.enclosing_statement_call` returns the
        outer rule/macro call). Likewise, use
        `fc.enclosing_statement_call(node)` rather than
        `fc.enclosing_call(node)` when walking AST nodes in other hooks.
    * **`check_call(self, fc, call)`**:
      * `call.func_name` normalizes `native.<rule>` to `<rule>` (e.g.
        `native.cc_library` to `cc_library`). Almost all checks should use
        `call.func_name` so they handle direct calls and `native.<func>` calls
        uniformly. Use `call.raw_func_name` only when a check explicitly needs
        to distinguish `native.<func>` from `<func>`.
      * `load.module_label` and `fc.loaded_symbols` (which maps `local_name ->
        (module_label, exported_symbol_name)` for all symbols imported via
        `load()`) automatically normalize leading `@@` to `@` and `@//` to `//`
        via `normalize_repo_prefix()`. Never duplicate `@` and `@@` module
        strings in checks; match against `load.module_label` and use
        `load.raw_module_label` when including the source label in finding
        messages or `fc.report_string(...)`.
      * Pass `call.node` (or the specific `ast.keyword` node from
        `call.keywords[...]`) to `fc.report(...)` so multi-line calls receive
        full `[line, end_line]` spans for `unchanged_lines_warning_only`
        intersection checks.
    * **Allowlist support (`supports_allowlist` and
      `supports_targets_allowlist`)**:
      * `supports_allowlist` defaults to `False` on `BazelCheck`. Authors should
        determine whether there are reasonable use cases for allowing exceptions
        to the check, such as to unblock work where the check cannot be
        reasonably resolved, or whether the check is non-negotiable:
        * Set `supports_allowlist = True` when there are reasonable use cases
          for temporary, bug-tracked exceptions in `allowlists/<name>.json` (for
          example, during migrations or to unblock work where the check cannot
          be reasonably resolved), **regardless of whether an allowlist `.json`
          file is currently needed on disk**.
        * Leave `supports_allowlist = False` for non-negotiable checks where
          violations must never be committed or allowlisted (for example,
          `bazel_vendor_paths`, where vendor subpaths must never be referenced
          in public Fuchsia code). When `False`, no allowlist is loaded and the
          linter fails if `allowlists/<name>.json` exists on disk. If you are
          unsure whether a new check should allow exceptions, ask the user.
      * Set `supports_targets_allowlist = True` (which also requires
        `supports_allowlist = True`) on the `BazelCheck` subclass when findings
        can be associated with named targets in `BUILD.bazel` files. When
        `supports_targets_allowlist = True` and `node` is passed to
        `fc.report()` or `fc.report_string()`, `FileContext` automatically
        infers which target(s) in the file own `node` so individual checks do
        not need to compute or pass `targets=...` manually:
        * Rule/macro calls and expressions inside them (including inside
          `select(...)` or `glob(...)`) are attributed to that call's `name`
          attribute.
        * Findings reported on a `load()` statement (either a loaded symbol in
          `check_load()` or the `.bzl` module label string in `check_string()`)
          are attributed to the named target(s) in the file that reference the
          loaded symbol(s), so allowlisting `"targets": ["my_target"]` in a
          `BUILD.bazel` file also suppresses `load()` findings whose loaded
          symbol(s) are only used by `"my_target"`.
        * Functions and rules without a `name` parameter (such as
          `exports_files()`, `package()`, and `expect_includes()`) are not
          targets and are not covered by target-specific allowlists.
      * Leave `supports_targets_allowlist = False` (the default) for file-wide
        or `package()`-only checks where findings never belong to a specific
        target.

3.  **Distinguish `policy_exempt_globs` vs. `allowlists/`**:
    * **`policy_exempt_globs`**: Use only for paths that are permanently exempt
      by design (e.g., `"build/bazel_sdk/bazel_rules_fuchsia/**"` or
      `"build/bazel_sdk/fuchsia_rules_common/**"`). Patterns use
      `PurePosixPath.full_match` syntax (`*` matches within a path segment, `**`
      matches across path segments, and patterns must not end with `"/"`).
      Always document why each glob is exempt in a comment above it.
    * Do **not** add `third_party/**/src/**` or `third_party/bazel_vendor/**` to
      `policy_exempt_globs`; those are already excluded at the framework level
      by `_EXCLUDED_GLOBS` in `//scripts/shac/bazel.star`.

---

## 4. Update All Synchronized Registration Files

Keep all registrations in alphabetical order across the `LINT.IfChange` /
`LINT.ThenChange` files (note that `shac` check names and allowlist filenames
use the `bazel_<check_name>` prefix, whereas files under `checks/` use
`<check_name>`):

1.  `scripts/shac/bazel_lint/checks/<check_name>.py`: Implement the
    `@register_check` class with `name = "bazel_<check_name>"`.
2.  `scripts/shac/bazel_lint/checks/__init__.py`: Import `<check_name>` in
    alphabetical order.
3.  `scripts/shac/bazel.star`:
    * Define `_bazel_<check_name> = _make_bazel_lint_check("bazel_<check_name>",
      applies_to_globs = ..., enforcement = "unchanged_lines_warning_only")`
      with a `# TODO(https://fxbug.dev/<bug_id>): Switch to "all" once rolled
      out.` comment.
    * Register `shac.register_check(_bazel_<check_name>)` in
      `register_bazel_checks()` in alphabetical order.
4.  `scripts/shac/bazel_lint/checks/BUILD.bazel`: Add `"<check_name>.py"` to
    `srcs`.
5.  `scripts/shac/bazel_lint/checks/tests/BUILD.bazel`: Add
    `"<check_name>_test.star"` to `srcs`.
6.  `scripts/shac/bazel_lint/checks/tests/checks_test.star`: Load and call
    `run_<check_name>_tests()`.

---

## 5. Scan the Tree and Handle Existing Violations

Run the new check across the entire checkout:

```sh
fx host-tool shac check --only bazel_<check_name> --all
```

*(Note: While `allowlists/README.md` prohibits AI agents from adding allowlist
entries to bypass an existing check when editing Bazel files, authoring a
**new** check requires ensuring `shac check --only bazel_<check_name> --all`
passes with zero findings before landing.)*

If existing files in the repository trigger findings:

1.  **Inspect the findings** to check whether any reveal a false positive in
    your AST logic or a directory that should be permanently exempt in
    `policy_exempt_globs`.
    * **Findings in `//build/bazel_sdk/`**: Any findings under
      `//build/bazel_sdk/` MUST be reported to the user grouped by the immediate
      subdirectory of `//build/bazel_sdk/` (such as
      `build/bazel_sdk/bazel_rules_fuchsia/`,
      `build/bazel_sdk/fuchsia_rules_common/`, or `build/bazel_sdk/tests/`). Ask
      the user whether those Bazel SDK files should be exempt from this check by
      policy (i.e., by design or functional requirement) or whether they should
      ideally conform in the future. If the former, add the relevant immediate
      subdirectory/ies to `policy_exempt_globs` (with a comment explaining why);
      otherwise, add the files (and `"targets"`, as appropriate) to the
      allowlist.
2.  **Create `scripts/shac/bazel_lint/allowlists/bazel_<check_name>.json`** for
    remaining pre-existing violations that cannot be fixed trivially in advance:
    * Format with 4-space indentation and keep `"entries"` sorted strictly
      alphabetically by `"path"` or `"path_prefix"`.
    * Order keys within each entry object with `"path"` or `"path_prefix"`
      first, followed by `"bug"`, then optional `"comment"` and `"targets"` (do
      not pass `sort_keys=True` to `json.dumps()`, which sorts `"bug"` first).
    * When `"targets"` is present, keep the target names sorted alphabetically.
    * Every entry must include `"bug": "https://fxbug.dev/<id>"` (use the bug ID
      provided by the user, or ask the user if none was provided).
    * When `supports_targets_allowlist = True`:
      * Specify `"targets"` when named targets in a file violate the check.
        Remember that functions and rules without a `name` parameter (such as
        `exports_files()`, `package()`, and `expect_includes()`) are not targets
        and are not covered by target-specific `"targets"` allowlists; files
        with violations in those constructs require a whole-file `"path"` entry
        (without `"targets"`).
      * **More than 10 `"targets"` in a single entry**: If `"targets"` are
        specified in an allowlist entry and there are more than 10 targets in
        that entry, present this information to the user and ask whether to
        remove `"targets"` (converting the entry to a whole-file `"path"`
        exemption). Note to the user that removing `"targets"` could have
        unintended consequences if the check has multiple conditions and not all
        apply to targets, or if non-target constructs in the file (such as
        `exports_files()`, `package()`, `expect_includes()`, or module-wide
        `load()` statements) would also become suppressed.
    * Use a directory `"path_prefix"` entry (ending with `"/"`, e.g.,
      `"sdk/fidl/"` or `"tools/"`) when requested by the user or when a subtree
      contains a large, homogeneous cluster of violating files under a single
      bug.
    * **IMPORTANT**: Always report any uses of `"path_prefix"` to the user, and
      identify any common subtrees where `"path_prefix"` allowlisting or
      permanent policy exemptions (`policy_exempt_globs`) may be appropriate
      (remembering that `policy_exempt_globs` must only be used for permanent,
      by-design exceptions).
    * **Vendor path privacy**: Never add specific paths under `vendor/` to an
      allowlist in `fuchsia.git`. `allowlist.py` only permits
      `"vendor/BUILD.bazel"`, `"vendor/google/BUILD.bazel"`, `"vendor/"`, or
      `"vendor/google/"`.
3.  Verify allowlist ordering (entries sorted by `"path"`/`"path_prefix"`,
    `"path"`/`"path_prefix"` first in each entry, and `"targets"` sorted) and
    re-run `fx host-tool shac check --only bazel_<check_name> --all` to confirm
    it exits 0 with zero findings (including zero warnings on untouched files):

    ```sh
    python3 -c '
    import json
    entries = json.load(open("scripts/shac/bazel_lint/allowlists/bazel_<check_name>.json"))["entries"]
    paths = [e.get("path") or e.get("path_prefix") for e in entries]
    assert paths == sorted(paths), "Entries not sorted"
    assert all(next(iter(e)) in ("path", "path_prefix") for e in entries), "path/path_prefix must be first key"
    assert all(e["targets"] == sorted(e["targets"]) for e in entries if "targets" in e), "targets not sorted"
    '
    fx host-tool shac check --only bazel_<check_name> --all
    ```

---

## 6. Author Starlark Check Tests (`checks/tests/<check_name>_test.star`)

Follow these conventions when writing
`scripts/shac/bazel_lint/checks/tests/<check_name>_test.star`:

1.  **Use `run_lint_test` to isolate from disk allowlists**:
    * Load `run_lint_test` from `./base_test.star` and define a file-local
      `_run_<check_name>_test(files)` helper at the top of
      `<check_name>_test.star`:

      ```python
      load("./base_test.star", "run_lint_test")

      def _run_<check_name>_test(files):
          return run_lint_test(
              _bazel_<check_name>,
              "bazel_<check_name>",
              files,
          )
      ```

    * Always call `_run_<check_name>_test({...})` in test functions. Never call
      `testing.run()` directly. `run_lint_test` automatically removes
      `scripts/shac/bazel_lint/allowlists/bazel_<check_name>.json` in the
      virtual test root so virtual test paths are never suppressed by the
      check's real allowlist on disk.

2.  **Separate passing and failing test functions**:
    * Never mix passing cases into a test function that expects findings (unless
      testing a narrow boundary between two lines).
    * Passing test functions must assert `asserts.eq(res.findings, ())`.

3.  **Format virtual file strings, use consecutive line numbers, and explain
    non-obvious cases**:
    * For multi-line virtual file strings using `"""`, always start the actual
      file content on a new line after the opening `"""` (never on the same line
      as `"""`), so the first line of content is unindented and begins on `line
      = 2`. Single-line file contents can use a single-line string literal (e.g.
      `'...\n'` on `line = 1`).
    * In test functions that expect line-scoped findings, structure each virtual
      file so every line of content produces a finding on consecutive line
      numbers (`line = 2`, `line = 3`, `line = 4`, ... for multi-line `"""`
      strings). Omit unrelated lines (like `package(...)` calls when not under
      test) so a missed finding is immediately visible from the line numbers.
    * For file-level checks (`node=None` in `check_module`), test each failure
      case in a separate virtual file in `files = {...}` and omit `line`, `col`,
      and `end_col` in `testing.finding(...)`.
    * Use descriptive, meaningful target, function/macro, symbol, and file names
      in test cases (e.g., `direct_host_constraints`, `aliased_cc_bin`) rather
      than single-letter or generic placeholders (`a`, `b`, `f`, `g`, `t1`).
    * Explain non-obvious test cases and expected results using comments or
      descriptive string literals in the test content and assertions.

4.  **Reuse a constant for violating content shared across positive and negative
    tests**:
    * When testing that a violating construct is suppressed by
      `policy_exempt_globs` or file-type filtering, define a module-level
      constant for the violating snippet and use it in both the positive test
      (which asserts a finding) and the negative tests (which assert `()`). This
      prevents a typo in a negative test from passing accidentally without
      triggering the check.

5.  **Cover each test category in dedicated `test_*` functions**:
    * Violation tests for each error/warning condition, asserting full
      `testing.finding(...)` structs (`level`, `message`, `filepath`, `line`,
      `col`, `end_col`).
    * `test_allowed_*`: Valid usages and AST exemptions (`in_compare`,
      `in_fail`, docstrings, `doc = "..."` keyword arguments).
    * `test_policy_exempt_*`: (When `policy_exempt_globs` is non-empty) Verifies
      that paths matching each policy-exempt glob produce no findings. Do not
      test `.json` allowlist matching (`"path"`, `"path_prefix"`, or
      `"targets"`) in per-check tests; `.json` allowlisting is tested at the
      framework level in `//scripts/shac/tests/bazel_test.star` and
      `//scripts/shac/tests:bazel_lint_test`.
    * `test_ignored_file_types`: (When the check only applies to a subset of
      Bazel files, such as `_BUILD_FILES` or `_BZL_FILES`) Verifies that
      violations in non-applicable file types are ignored. Do not re-test
      framework-level `_EXCLUDED_GLOBS` (`third_party/**/src/**`), which are
      tested in `//scripts/shac/tests/bazel_test.star`.

6.  **Export `run_<check_name>_tests()`**:
    * Define `run_<check_name>_tests()` at the bottom of
      `<check_name>_test.star` calling all `test_*` functions in the file.

---

## 7. Verify Everything Before Finishing

Before running the test suite, verify that:

* Every `test_*` function defined in `<check_name>_test.star` is called in
  `run_<check_name>_tests()`.
* `run_<check_name>_tests()` is loaded and called in
  `scripts/shac/bazel_lint/checks/tests/checks_test.star`.

Then run all of the following checks and tests (in non-interactive shells where
`fx` is not on `PATH`, run `prebuilt/tools/shac/shac` directly for `shac`
commands and `scripts/fx` for `fx` commands):

1.  **Starlark check tests**:

    ```sh
    fx host-tool shac test scripts/shac/bazel_lint/checks/tests/<check_name>_test.star
    ```

2.  **Framework Python unit tests** (only necessary if `bazel_lint` files
    outside `checks/` and `allowlists/` were modified):

    ```sh
    fx test --host //scripts/shac/tests:bazel_lint_test
    ```

3.  **Hermetic Bazel test** (required to verify `BUILD.bazel` runfiles and run
    all Starlark tests via `bazel_test.star`):

    ```sh
    fx test --host @//scripts/shac/tests:bazel_test
    ```

    *(If `fx test` does not recognize the target, run `fx add-host-test
    //scripts/shac/tests` first.)*

4.  **Full-workspace check**:

    ```sh
    fx host-tool shac check --only bazel_<check_name> --all
    ```

    Verify that this command exits with code 0 and outputs no findings
    (including no warnings).

5.  **Format and lint modified files**:

    ```sh
    fx format-code
    ```

Include only these two `Test:` footers in the commit message when adding a check
(only include `Test: fx test --host //scripts/shac/tests:bazel_lint_test` if you
modified framework files such as `framework.py`, `allowlist.py`, `main.py`, or
`bazel_lint_test.py`):

```
Test: fx test --host @//scripts/shac/tests:bazel_test
Test: fx host-tool shac check --only bazel_<check_name> --all
```
