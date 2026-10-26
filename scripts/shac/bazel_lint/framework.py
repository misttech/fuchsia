# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Core AST traversal, semantic hooks, and span emission for Bazel lint checks."""

import ast
import dataclasses
import enum
from pathlib import PurePosixPath
from typing import Sequence, TypedDict

from allowlist import Allowlist


# LINT.IfChange(file_kinds)
class FileKind(enum.Enum):
    """Supported file kinds for Bazel lint checks."""

    # BUILD.bazel, *.BUILD.bazel, BUILD, *.BUILD
    BUILD = "build"
    # *.bzl
    BZL = "bzl"
    # MODULE.bazel, *.MODULE.bazel
    MODULE = "module"
    # WORKSPACE, WORKSPACE.bazel, WORKSPACE.bzlmod, *.WORKSPACE.bazel
    WORKSPACE = "workspace"
    # *.py (Starlark load() statements do not exist in Python; check_load is disallowed)
    PYTHON = "python"


# LINT.ThenChange(//scripts/shac/bazel.star:file_globs)


def classify_file(rel_path: str) -> FileKind:
    """Classifies a repository-relative path into a `FileKind`.

    Raises `ValueError` if the path does not match a known file kind.
    """
    name = PurePosixPath(rel_path).name
    if (
        name in ("BUILD.bazel", "BUILD")
        or name.endswith(".BUILD.bazel")
        or name.endswith(".BUILD")
    ):
        return FileKind.BUILD
    if name.endswith(".bzl"):
        return FileKind.BZL
    if name == "MODULE.bazel" or name.endswith(".MODULE.bazel"):
        return FileKind.MODULE
    if name in (
        "WORKSPACE",
        "WORKSPACE.bazel",
        "WORKSPACE.bzlmod",
    ) or name.endswith(".WORKSPACE.bazel"):
        return FileKind.WORKSPACE
    if name.endswith(".py"):
        return FileKind.PYTHON
    raise ValueError(f"Unsupported file kind for path: {rel_path}")


class Finding(TypedDict, total=False):
    """JSON-serializable shac finding."""

    filepath: str
    line: int
    end_line: int
    col: int
    end_col: int
    message: str
    level: str


@dataclasses.dataclass(frozen=True)
class _RawFinding:
    finding: Finding
    targets: tuple[str, ...]


# LINT.IfChange(load_info)
@dataclasses.dataclass(frozen=True)
class LoadInfo:
    """Pre-extracted metadata for a `load()` call passed to `check_load`.

    Attributes:
        node (ast.Call): The underlying AST `Call` node for the `load()` statement.
        module_label (str): The label of the `.bzl` module being loaded (first
            argument to `load()`, e.g. `"@rules_cc//cc:defs.bzl"`).
        symbols (tuple[tuple[str, str, ast.AST], ...]): Tuple of loaded symbols,
            where each entry is `(local_name, exported_name, symbol_ast_node)`.
            `local_name` matches `exported_name` for positional string arguments,
            or the keyword argument name when aliased (e.g. `foo = "bar"`).
    """

    node: ast.Call
    module_label: str
    # Each entry is (local_name, exported_name, symbol_ast_node).
    symbols: tuple[tuple[str, str, ast.AST], ...]


# LINT.ThenChange(//scripts/shac/bazel_lint/checks/README.md:load_info)


# LINT.IfChange(call_info)
@dataclasses.dataclass(frozen=True)
class CallInfo:
    """Pre-extracted metadata for a function/rule/macro call passed to `check_call`.

    All function calls, rule invocations, and macros are represented as
    `CallInfo`. The AST cannot distinguish between a rule and a macro; they are
    syntactically identical call expressions.

    Attributes:
        node (ast.Call): The underlying AST `Call` node.
        func_name (str): The normalized function name. For direct calls
            (e.g. `cc_library(...)` or `my_macro(...)`), `func_name` matches
            the function identifier. For calls via `native.<rule>` (e.g.
            `native.cc_library(...)`), `func_name` is normalized to strip
            `native.` (becoming `"cc_library"`).
        raw_func_name (str): The exact un-normalized function expression as source
            text (e.g. `"native.cc_library"` or `"my_macro"`). This is identical
            to `func_name` for all calls except those using the `native.` prefix.
        target_name (str | None): The value of the `name = "..."` keyword argument
            if present as a string literal, otherwise `None`.
        keywords (dict[str, ast.keyword]): Mapping from keyword argument name to
            its `ast.keyword` AST node.
        args (tuple[ast.expr, ...]): Positional argument AST expressions.
        forwards_kwargs (bool): True if the call passes `**kwargs` (e.g. in a macro).
        is_top_level (bool): True if the call is an expression statement at the file's
            top level (i.e. not nested inside a function definition, if, or loop).
    """

    node: ast.Call
    func_name: str
    raw_func_name: str
    target_name: str | None
    keywords: dict[str, ast.keyword]
    args: tuple[ast.expr, ...]
    forwards_kwargs: bool
    is_top_level: bool


# LINT.ThenChange(//scripts/shac/bazel_lint/checks/README.md:call_info)


# LINT.IfChange(string_info)
@dataclasses.dataclass(frozen=True)
class StringInfo:
    """Pre-extracted metadata for a string literal passed to `check_string`.

    `check_string` only inspects string literals (represented in the AST as
    `ast.Constant` with a `str` value). It cannot evaluate variables, string
    formatting (e.g. `%` formatting or `.format()`), or concatenation involving
    variables (e.g. `prefix + "/foo"`). When literal strings are concatenated
    with `+` (e.g. `"foo" + "bar"`), each operand is a distinct `ast.Constant`
    node; the AST does not perform constant folding, so `check_string` is
    invoked separately for `"foo"` and `"bar"`, and neither sees `"foobar"`.
    In contrast, adjacent string literals (e.g. `"foo" "bar"`) are merged into a
    single `ast.Constant` (`"foobar"`) by the parser at parse time. Multiline
    strings preserve newline characters (`\n`), and whitespace inside the string
    is unmodified and un-normalized.

    Attributes:
        node (ast.Constant): The underlying AST `Constant` string node.
        value (str): The decoded string value.
        enclosing_call (CallInfo | None): The nearest enclosing non-`load()` call,
            or `None` if the string is outside any function call.
        keyword_arg (str | None): The keyword argument name if this string is passed
            directly to a keyword argument of `enclosing_call` (e.g. `"visibility"`
            or `"doc"`).
        is_load_source (bool): True if this string is the module label argument of a
            `load()` statement.
        is_docstring (bool): True if this string is a module, class, or function
            docstring. Note that Bazel `doc = "..."` keyword arguments on rules,
            macros, or attributes are not docstrings (`is_docstring == False`);
            they have `keyword_arg == "doc"`.
        in_compare (bool): True if this string is part of a comparison expression
            (e.g. `if x == "//visibility:public":`).
        in_fail (bool): True if this string is an argument to `fail(...)`.
    """

    node: ast.Constant
    value: str
    enclosing_call: CallInfo | None
    keyword_arg: str | None
    is_load_source: bool
    is_docstring: bool
    in_compare: bool
    in_fail: bool


# LINT.ThenChange(//scripts/shac/bazel_lint/checks/README.md:string_info)


def _byte_offset_to_char_col(line_text: str, byte_offset: int) -> int:
    """Converts a 0-based UTF-8 byte offset into a 1-based character column."""
    if byte_offset <= 0:
        return 1
    encoded = line_text.encode("utf-8")
    prefix = encoded[:byte_offset].decode("utf-8", errors="replace")
    return len(prefix) + 1


def _collect_docstrings(tree: ast.Module) -> set[ast.AST]:
    """Returns the set of `ast.Constant` nodes that are module, class, or function docstrings."""
    docstrings: set[ast.AST] = set()
    for node in ast.walk(tree):
        if isinstance(
            node,
            (ast.Module, ast.ClassDef, ast.FunctionDef, ast.AsyncFunctionDef),
        ):
            if (
                node.body
                and isinstance(node.body[0], ast.Expr)
                and isinstance(node.body[0].value, ast.Constant)
                and isinstance(node.body[0].value.value, str)
            ):
                docstrings.add(node.body[0].value)
    return docstrings


def _extract_func_names(func: ast.expr) -> tuple[str, str]:
    """Returns `(normalized_func_name, raw_func_name)` for a call's `func` node."""
    if isinstance(func, ast.Name):
        return func.id, func.id
    raw = ast.unparse(func)
    if (
        isinstance(func, ast.Attribute)
        and isinstance(func.value, ast.Name)
        and func.value.id == "native"
    ):
        return func.attr, raw
    return raw, raw


# LINT.IfChange(file_context)
class FileContext:
    """Per-file AST context and finding emitter passed to `BazelCheck` hooks.

    Attributes:
        path (str): Repository-relative path of the file being linted.
        kind (FileKind): The classified `FileKind` of the file (e.g. `FileKind.BUILD`).
        tree (ast.Module): Parsed Python/Starlark AST root.
        lines (list[str]): File content split into lines as strings without trailing
            newlines (1-indexed line numbers correspond to `lines[lineno - 1]`).
        loaded_symbols (dict[str, tuple[str, str]]): Map of local symbol name to
            `(module_label, exported_symbol_name)` for all symbols imported via `load()`.
        loads (list[LoadInfo]): Pre-extracted `LoadInfo` metadata for all `load()`
            calls in the file.
        calls (list[CallInfo]): Pre-extracted `CallInfo` metadata for all non-`load()`
            function/rule/macro calls.
        strings (list[StringInfo]): Pre-extracted `StringInfo` metadata for all
            string constants.
    """

    def __init__(
        self,
        path: str,
        kind: FileKind,
        tree: ast.Module,
        lines: list[str],
    ) -> None:
        self.path = path
        self.kind = kind
        self.tree = tree
        self.lines = lines
        self._raw_findings: list[_RawFinding] = []

        self._parents: dict[ast.AST, ast.AST] = {}
        for parent in ast.walk(tree):
            for child in ast.iter_child_nodes(parent):
                self._parents[child] = parent

        self._docstrings = _collect_docstrings(tree)
        self.loaded_symbols: dict[str, tuple[str, str]] = {}
        self.loads: list[LoadInfo] = []
        self.calls: list[CallInfo] = []
        self._call_by_node: dict[ast.Call, CallInfo] = {}
        self._load_source_nodes: set[ast.AST] = set()

        self._extract_loads_and_calls()
        self.strings: list[StringInfo] = self._extract_strings()

    def parent(self, node: ast.AST) -> ast.AST | None:
        """Returns the parent AST node of `node`, or `None` for the root module."""
        return self._parents.get(node)

    def enclosing_call(self, node: ast.AST) -> CallInfo | None:
        """Returns the nearest enclosing non-`load()` `CallInfo`, if any."""
        cur = self._parents.get(node)
        while cur is not None:
            if isinstance(cur, ast.Call) and cur in self._call_by_node:
                return self._call_by_node[cur]
            if isinstance(cur, (ast.FunctionDef, ast.AsyncFunctionDef)):
                break
            cur = self._parents.get(cur)
        return None

    def _extract_loads_and_calls(self) -> None:
        for node in ast.walk(self.tree):
            if not isinstance(node, ast.Call):
                continue
            func_name, raw_func_name = _extract_func_names(node.func)
            if (
                raw_func_name == "load"
                and node.args
                and isinstance(node.args[0], ast.Constant)
                and isinstance(node.args[0].value, str)
            ):
                module_label = node.args[0].value
                self._load_source_nodes.add(node.args[0])
                symbols: list[tuple[str, str, ast.AST]] = []
                for arg in node.args[1:]:
                    if isinstance(arg, ast.Constant) and isinstance(
                        arg.value, str
                    ):
                        symbols.append((arg.value, arg.value, arg))
                        self.loaded_symbols[arg.value] = (
                            module_label,
                            arg.value,
                        )
                for kw in node.keywords:
                    if (
                        kw.arg is not None
                        and isinstance(kw.value, ast.Constant)
                        and isinstance(kw.value.value, str)
                    ):
                        symbols.append((kw.arg, kw.value.value, kw))
                        self.loaded_symbols[kw.arg] = (
                            module_label,
                            kw.value.value,
                        )
                self.loads.append(
                    LoadInfo(
                        node=node,
                        module_label=module_label,
                        symbols=tuple(symbols),
                    )
                )
            else:
                keywords = {
                    kw.arg: kw for kw in node.keywords if kw.arg is not None
                }
                target_name: str | None = None
                if "name" in keywords:
                    name_val = keywords["name"].value
                    if isinstance(name_val, ast.Constant) and isinstance(
                        name_val.value, str
                    ):
                        target_name = name_val.value
                forwards_kwargs = any(kw.arg is None for kw in node.keywords)
                parent_node = self._parents.get(node)
                is_top_level = (
                    isinstance(parent_node, ast.Expr)
                    and self._parents.get(parent_node) is self.tree
                )
                call_info = CallInfo(
                    node=node,
                    func_name=func_name,
                    raw_func_name=raw_func_name,
                    target_name=target_name,
                    keywords=keywords,
                    args=tuple(node.args),
                    forwards_kwargs=forwards_kwargs,
                    is_top_level=is_top_level,
                )
                self.calls.append(call_info)
                self._call_by_node[node] = call_info

    def _extract_strings(self) -> list[StringInfo]:
        strings: list[StringInfo] = []
        for node in ast.walk(self.tree):
            if not (
                isinstance(node, ast.Constant) and isinstance(node.value, str)
            ):
                continue

            enclosing = self.enclosing_call(node)
            keyword_arg: str | None = None
            in_compare = False
            cur: ast.AST | None = node
            while cur is not None:
                if isinstance(cur, ast.Compare):
                    in_compare = True
                if (
                    isinstance(cur, ast.keyword)
                    and cur.arg is not None
                    and enclosing is not None
                    and self._parents.get(cur) is enclosing.node
                ):
                    keyword_arg = cur.arg
                if isinstance(cur, ast.stmt):
                    break
                cur = self._parents.get(cur)

            in_fail = enclosing is not None and enclosing.func_name == "fail"
            strings.append(
                StringInfo(
                    node=node,
                    value=node.value,
                    enclosing_call=enclosing,
                    keyword_arg=keyword_arg,
                    is_load_source=node in self._load_source_nodes,
                    is_docstring=node in self._docstrings,
                    in_compare=in_compare,
                    in_fail=in_fail,
                )
            )
        return strings

    def report(
        self,
        message: str,
        node: ast.AST | None = None,
        *,
        level: str,
        url: str | None,
        targets: Sequence[str] = (),
    ) -> None:
        """Emits a finding for `node` (or a file-level finding if `node` is `None`)."""
        full_message = f"{message} See {url}." if url is not None else message
        finding: Finding = {
            "filepath": self.path,
            "message": full_message,
            "level": level,
        }
        lineno = getattr(node, "lineno", None)
        col_offset = getattr(node, "col_offset", None)
        if node is not None and lineno is not None and col_offset is not None:
            end_lineno = getattr(node, "end_lineno", None) or lineno
            end_col_offset = getattr(node, "end_col_offset", None)
            start_line_text = (
                self.lines[lineno - 1] if 1 <= lineno <= len(self.lines) else ""
            )
            end_line_text = (
                self.lines[end_lineno - 1]
                if 1 <= end_lineno <= len(self.lines)
                else ""
            )
            col = _byte_offset_to_char_col(start_line_text, col_offset)
            if end_col_offset is not None:
                end_col = _byte_offset_to_char_col(
                    end_line_text, end_col_offset
                )
            else:
                end_col = col + 1

            finding["line"] = lineno
            finding["col"] = col
            if end_lineno > lineno:
                finding["end_line"] = end_lineno
            finding["end_col"] = end_col

        self._raw_findings.append(
            _RawFinding(finding=finding, targets=tuple(targets))
        )

    def report_string(
        self,
        message: str,
        node: ast.Constant,
        substring: str | None = None,
        *,
        level: str,
        url: str | None,
        targets: Sequence[str] = (),
    ) -> None:
        """Emits a finding highlighting `substring` inside a string literal `node`.

        Excludes surrounding quotes on single-line strings and falls back to the
        full `node` span for multi-line strings, implicit string concatenation,
        or escaped literals.
        """
        target_str = (
            substring
            if substring is not None
            else (node.value if isinstance(node.value, str) else "")
        )
        lineno = getattr(node, "lineno", None)
        end_lineno = getattr(node, "end_lineno", None) or lineno
        col_offset = getattr(node, "col_offset", None)

        if (
            target_str
            and lineno is not None
            and end_lineno == lineno
            and col_offset is not None
            and 1 <= lineno <= len(self.lines)
        ):
            line_text = self.lines[lineno - 1]
            start_char_idx = _byte_offset_to_char_col(line_text, col_offset) - 1
            found_idx = line_text.find(target_str, start_char_idx)
            if found_idx != -1:
                full_message = (
                    f"{message} See {url}." if url is not None else message
                )
                col = found_idx + 1
                end_col = col + len(target_str)
                self._raw_findings.append(
                    _RawFinding(
                        finding=Finding(
                            filepath=self.path,
                            line=lineno,
                            col=col,
                            end_col=end_col,
                            message=full_message,
                            level=level,
                        ),
                        targets=tuple(targets),
                    )
                )
                return

        self.report(message, node, level=level, url=url, targets=targets)


# LINT.ThenChange(//scripts/shac/bazel_lint/checks/README.md:file_context)


class BazelCheck:
    """Base class for pluggable AST-based Bazel lint checks."""

    # Name of the check (e.g. "bazel_target_visibility"). Must match the check
    # registration in //scripts/shac/bazel.star and allowlist filename.
    name: str = ""
    # The file kinds this check applies to (e.g. FileKind.BUILD, FileKind.BZL).
    # NOTE: Files of each declared FileKind must be included in the globs
    # specified in `_make_bazel_lint_check(..., applies_to_globs=...)` when
    # registering the check in `//scripts/shac/bazel.star`. If a glob is omitted
    # in `bazel.star`, shac will never pass those files to the linter.
    file_kinds: frozenset[FileKind] = frozenset()
    # Optional repository-relative glob patterns in Bazel `glob()` syntax (for
    # example, `"build/bazel_sdk/bazel_rules_fuchsia/**"` or `"build/config/**"`)
    # for paths that are permanently exempt by policy (no tracking bug
    # required). Use `allowlists/<name>.json` instead for temporary exceptions
    # that require a tracking bug.
    policy_exempt_globs: tuple[str, ...] = ()
    supports_targets_allowlist: bool = False

    def is_path_exempt_by_policy(self, rel_path: str) -> bool:
        """Returns True if `rel_path` matches any glob in `policy_exempt_globs`.

        Patterns use Bazel `glob()` syntax (evaluated via
        `PurePosixPath.full_match`), where `*` matches within a single path
        segment and `**` matches zero or more path segments (for example,
        `"build/bazel_sdk/*rules*/**"` or `"build/bazel/rules/**"`).
        """
        path = PurePosixPath(rel_path)
        return any(
            path.full_match(pattern) for pattern in self.policy_exempt_globs
        )

    def check_module(self, fc: FileContext) -> None:
        """Hook 1: Invoked once per file for whole-module assertions."""

    def check_load(self, fc: FileContext, load: LoadInfo) -> None:
        """Hook 2: Invoked for every `load()` call in the file."""

    def check_call(self, fc: FileContext, call: CallInfo) -> None:
        """Hook 3: Invoked for every non-`load()` call in the file."""

    def check_string(self, fc: FileContext, s: StringInfo) -> None:
        """Hook 4: Invoked for every string constant in the file.

        Only checks string literals (`ast.Constant` with a `str` value).
        Variables, string formatting, or concatenation involving variables
        (e.g. `prefix + "/foo"`) cannot be inspected. String literals
        concatenated with `+` (e.g. `"foo" + "bar"`) appear as separate AST
        nodes without constant folding (checked separately as `"foo"` and
        `"bar"`), whereas adjacent string literals (e.g. `"foo" "bar"`) are
        merged into a single node (`"foobar"`) by the parser at parse time.
        Multiline strings preserve newline characters (`\n`), and whitespace is
        unmodified and un-normalized.

        Includes all string literals in the AST except module-level `load()`
        arguments (which are passed to `check_load()`). Module, class, and
        function docstrings are included and have `s.is_docstring == True`,
        so checks that do not want to inspect docstrings can return early
        (`if s.is_docstring: return`). Note that Bazel `doc = "..."` keyword
        arguments on rules, macros, or attributes are regular string literals
        with `s.is_docstring == False` and `s.keyword_arg == "doc"`. Comments
        (`# ...`) are stripped by `ast.parse()` and are never passed to
        `check_string()` or any other hook point function.
        """


REGISTRY: dict[str, BazelCheck] = {}


def register_check(check_cls: type[BazelCheck]) -> type[BazelCheck]:
    """Decorator that instantiates and registers a `BazelCheck` subclass."""
    instance = check_cls()
    if not instance.name:
        raise ValueError(f"{check_cls.__name__} must define a non-empty 'name'")
    if not instance.file_kinds:
        raise ValueError(
            f"{check_cls.__name__} must define non-empty 'file_kinds'"
        )
    for pattern in instance.policy_exempt_globs:
        if pattern.endswith("/"):
            raise ValueError(
                f"{check_cls.__name__}: policy_exempt_globs pattern '{pattern}' "
                "must use Bazel glob() syntax (e.g. 'dir/**' instead of 'dir/')"
            )
    if (
        FileKind.PYTHON in instance.file_kinds
        and check_cls.check_load is not BazelCheck.check_load
    ):
        raise TypeError(
            f"{check_cls.__name__}: checks accepting FileKind.PYTHON cannot "
            "override check_load (Python does not support Starlark load() statements)"
        )
    if instance.name in REGISTRY:
        raise ValueError(f"Duplicate check registration for '{instance.name}'")
    REGISTRY[instance.name] = instance
    return check_cls


def run_check_on_contents(
    check: BazelCheck,
    content: str,
    rel_path: str,
    *,
    allowlist: Allowlist | None = None,
) -> list[Finding]:
    """Runs `check` on in-memory `content` for `rel_path`."""
    kind = classify_file(rel_path)
    if kind not in check.file_kinds:
        raise ValueError(
            f"Check '{check.name}' does not accept {kind} ({rel_path})"
        )
    if check.is_path_exempt_by_policy(rel_path):
        return []
    if allowlist is not None and allowlist.is_file_allowlisted(rel_path):
        return []

    try:
        tree = ast.parse(content, filename=rel_path)
    except SyntaxError as e:
        line = e.lineno or 1
        col = e.offset or 1
        end_col = (
            e.end_offset if (e.end_offset and e.end_offset > col) else col + 1
        )
        return [
            Finding(
                filepath=rel_path,
                line=line,
                col=col,
                end_col=end_col,
                message=f"Syntax error: {e.msg}",
                level="error",
            )
        ]
    except RecursionError:
        return [
            Finding(
                filepath=rel_path,
                line=1,
                col=1,
                end_col=2,
                message="Syntax error: maximum AST recursion depth exceeded",
                level="error",
            )
        ]

    fc = FileContext(
        path=rel_path,
        kind=kind,
        tree=tree,
        lines=content.splitlines(),
    )
    check.check_module(fc)
    for load in fc.loads:
        check.check_load(fc, load)
    for call in fc.calls:
        check.check_call(fc, call)
    for s in fc.strings:
        check.check_string(fc, s)

    results: list[Finding] = []
    for raw in fc._raw_findings:
        if allowlist is not None and allowlist.is_finding_allowlisted(
            rel_path, raw.targets
        ):
            continue
        results.append(raw.finding)

    results.sort(key=lambda f: (f.get("line", 0), f.get("col", 0)))
    return results
