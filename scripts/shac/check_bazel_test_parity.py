#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Enforces GN-to-Bazel test parity and loud test-deletion alerts in shac.

Detects:
1. Class 1 Silent Orphaning (unwaivable error):
   - Bazel test targets with `tags = ["manual"]` (without
     `# test-parity-allow-manual: <reason>`) or not transitively wired into a
     `bazel_test_suite(...)` in the same or an ancestor `BUILD.gn`.
   - `bazel_test_suite(...)` placed inside `if (is_host)` in `BUILD.gn` (FM2).
   - Host vs. target OS bucket mismatches in `bazel_test_suite(...)` and nested
     host `test_suite(...)` targets missing `HOST_OS_CONSTRAINTS` (FM3 / FM4).
   - Removed GN test targets, removed `with_unit_tests`, missing dual
     host+device toolchain legs, or severed `:tests` group edges whose test
     source files still exist on disk.
2. Lost `#[cfg(feature = ...)]` unit tests on `rust_test` / `rustc_test` using
   `crate = ":<lib>"` without inheriting `crate_features` (FM8, unwaivable).
3. Dual-build `# @bazel2gn:skip` on test targets (FM5): requires both adjacent
   `# Needs reviewer attention (bazel2gn skip): <reason>` and
   `Skipped-Dual-Tests:` + numeric `Bug:`, plus `Test-Drop-Verified +1`.
4. Class 4 Deleted Tests (co-deleting build target and test source files):
   - Migration CLs: unwaivable error unless attested via exact `Deleted-Tests:`
     + numeric `Bug:` (and not `TAG: planter`), which still screams loudly with
     `fail_if_missing_label = "Test-Drop-Verified"`.
   - Non-migration cleanup CLs: visible warning naming deleted test targets.
5. Narrowed `Build-Only: true` enforcement (FM1, unwaivable error): rejected
   whenever any test rule, `with_unit_tests`, `bazel_test_suite`, `test_suite`,
   or `:tests` group dependency is added, removed, or modified.
"""

from __future__ import annotations

import argparse
import ast
import dataclasses
import json
import os
import pathlib
import re
import shutil
import subprocess
import sys
from collections.abc import Sequence
from typing import Any

BAZEL2GN_SENTINEL = "## BAZEL2GN SENTINEL"

BAZEL_DEVICE_PACKAGE_RULES = frozenset(
    {
        "fuchsia_test_package",
        "fuchsia_unittest_package",
        "fx_test",
        "fx_unittest_package",
    }
)

BAZEL_HOST_ONLY_TEST_RULES = frozenset(
    {
        "host_cc_test",
        "host_go_test",
        "host_py_test",
        "host_rustc_test",
        "host_test",
        "py_test",
        "python_host_test",
        "sdk_host_test",
        "wrap_host_rust_test",
    }
)

BAZEL_AMBI_BINARY_TEST_RULES = frozenset(
    {
        "cc_test",
        "fuchsia_cc_test",
        "fx_cc_test",
        "go_test",
        "rust_test",
        "rustc_test",
    }
)

BAZEL_ALL_EXPLICIT_TEST_RULES = (
    BAZEL_DEVICE_PACKAGE_RULES
    | BAZEL_HOST_ONLY_TEST_RULES
    | BAZEL_AMBI_BINARY_TEST_RULES
)

BAZEL_RUST_UNITTEST_OWNER_RULES = frozenset(
    {
        "rust_binary",
        "rust_library",
        "rust_proc_macro",
        "rustc_binary",
        "rustc_library",
        "rustc_proc_macro",
    }
)

GN_DEVICE_PACKAGE_RULES = frozenset(
    {
        "fuchsia_test_package",
        "fuchsia_unittest_package",
    }
)

GN_DEVICE_COMPONENT_RULES = frozenset(
    {
        "bootfs_test",
        "fuchsia_test",
        "fuchsia_test_component",
        "fuchsia_unittest_component",
    }
)

GN_HOST_ONLY_TEST_RULES = frozenset(
    {
        "host_test",
        "python_build_time_tests",
        "python_host_test",
    }
)

GN_BINARY_TEST_RULES = frozenset(
    {
        "cc_test",
        "go_test",
        "rustc_test",
        "test",
    }
)

GN_EXPLICIT_TEST_RULES = (
    GN_DEVICE_PACKAGE_RULES
    | GN_DEVICE_COMPONENT_RULES
    | GN_HOST_ONLY_TEST_RULES
    | GN_BINARY_TEST_RULES
)

GN_RUST_UNITTEST_OWNER_RULES = frozenset(
    {
        "rustc_binary",
        "rustc_library",
        "rustc_proc_macro",
    }
)

TEST_GROUP_NAMES = frozenset(
    {
        "bootfs_tests",
        "e2e_tests",
        "hermetic_tests",
        "host_tests",
        "tests",
        "tests_no_e2e",
    }
)

IGNORED_PATH_PREFIXES = (
    "third_party/bazel_vendor/",
    "third_party/rust_crates/vendor/",
)

RUST_TEST_ATTR_RE = re.compile(
    r"#\[\s*(?:cfg\s*\(\s*test\b|(?:\w+::)*test\b)",
    re.MULTILINE,
)

HOST_CONSTRAINT_IDENTIFIERS = frozenset(
    {
        "HOST_CONSTRAINTS",
        "HOST_OS_CONSTRAINTS",
    }
)

HOST_CONSTRAINT_STRINGS = frozenset(
    {
        "@platforms//os:linux",
        "@platforms//os:macos",
        "@platforms//os:windows",
        "//build/bazel/platforms/os:linux",
        "//build/bazel/platforms/os:host",
    }
)

FUCHSIA_CONSTRAINT_STRINGS = frozenset(
    {
        "@platforms//os:fuchsia",
        "//build/bazel/platforms/os:fuchsia",
    }
)


@dataclasses.dataclass
class Finding:
    """Represents a single shac finding."""

    level: str
    message: str
    filepath: str = ""
    line: int = 0
    end_line: int = 0
    properties: dict[str, str] = dataclasses.field(default_factory=dict)

    def to_dict(self) -> dict[str, Any]:
        result: dict[str, Any] = {
            "level": self.level,
            "message": self.message,
        }
        if self.filepath:
            result["filepath"] = self.filepath
        if self.line > 0:
            result["line"] = self.line
        if self.end_line > 0:
            result["end_line"] = self.end_line
        if self.properties:
            result["properties"] = dict(self.properties)
        return result


@dataclasses.dataclass
class BazelTarget:
    """Parsed rule call from a BUILD.bazel file."""

    pkg_dir: str
    rule_type: str
    name: str
    lineno: int
    end_lineno: int
    string_lists: dict[str, list[str]]
    scalars: dict[str, Any]
    present_kwargs: frozenset[str]
    has_host_os_constraints: bool
    has_fuchsia_os_constraints: bool
    skip_lineno: int = 0
    has_reviewer_skip_note: bool = False
    allow_manual_reason: str = ""
    canonical_sig: str = ""


@dataclasses.dataclass
class GnTarget:
    """Parsed target block from a BUILD.gn file."""

    pkg_dir: str
    rule_type: str
    name: str
    lineno: int
    end_lineno: int
    in_post_sentinel: bool
    conditions: tuple[str, ...]
    string_lists: dict[str, list[str]]
    scalars: dict[str, Any]
    canonical_sig: str = ""


@dataclasses.dataclass
class ParsedGnFile:
    """Parsed representation of a BUILD.gn file."""

    pkg_dir: str
    has_sentinel: bool
    sentinel_lineno: int
    targets: list[GnTarget]


@dataclasses.dataclass
class ParsedBazelFile:
    """Parsed representation of a BUILD.bazel file."""

    pkg_dir: str
    targets: list[BazelTarget]


@dataclasses.dataclass
class RemovedTestRecord:
    """Tracks an unaccounted-for removed GN or Bazel test target."""

    label: str
    pkg_dir: str
    name: str
    rule_type: str
    polarities: frozenset[str]
    source_files: tuple[str, ...]
    is_inline_rust_unit_test: bool
    build_filepath: str
    lineno: int


@dataclasses.dataclass
class CommitMetadata:
    """Parsed footers and classification signals from a commit message."""

    raw_message: str
    has_build_only_true: bool
    has_tag_planter: bool
    has_migration_marker: bool
    bug_ids: tuple[str, ...]
    deleted_tests_raw: tuple[tuple[int, str], ...]
    skipped_dual_tests_raw: tuple[tuple[int, str], ...]
    renamed_tests_raw: tuple[tuple[int, str], ...]


def _normalize_rel_path(path_str: str, root: pathlib.Path) -> str:
    """Normalizes a path to a POSIX workspace-relative path."""
    candidate = pathlib.Path(path_str)
    if candidate.is_absolute():
        try:
            rel = candidate.resolve().relative_to(root.resolve())
            return rel.as_posix()
        except ValueError:
            return candidate.as_posix().lstrip("/")
    normalized = os.path.normpath(path_str).replace("\\", "/")
    if normalized == ".":
        return ""
    return normalized.lstrip("./")


def _pkg_dir_of(filepath: str) -> str:
    """Returns package directory ("" for root) of a workspace-relative path."""
    parent = os.path.dirname(filepath)
    return "" if parent == "." else parent


def _format_label(pkg_dir: str, target_name: str) -> str:
    """Formats a canonical //pkg:target label."""
    if pkg_dir:
        return f"//{pkg_dir}:{target_name}"
    return f"//:{target_name}"


def _split_label(
    raw_label: str, current_pkg: str
) -> tuple[str, str, str] | None:
    """Splits a GN or Bazel label into (pkg_dir, target_name, toolchain).

    Returns None for external repository labels (e.g. `@crates//:foo`).
    """
    cleaned = raw_label.strip()
    toolchain = ""
    if "(" in cleaned and cleaned.endswith(")"):
        base_part, _, tc_part = cleaned.partition("(")
        cleaned = base_part.strip()
        toolchain = tc_part[:-1].strip()

    cleaned = re.sub(r"^@@?//", "//", cleaned)
    if cleaned.startswith("@"):
        return None
    if cleaned.startswith("//"):
        without_slashes = cleaned[2:]
        path_part, sep, name_part = without_slashes.partition(":")
        pkg = path_part.strip("/")
        if sep:
            target = name_part.strip()
        else:
            target = pkg.rsplit("/", 1)[-1] if pkg else ""
        return (pkg, target, toolchain)
    if cleaned.startswith(":"):
        return (current_pkg, cleaned[1:].strip(), toolchain)
    if ":" in cleaned:
        sub_pkg, _, name_part = cleaned.partition(":")
        joined = (
            f"{current_pkg}/{sub_pkg.strip('/')}"
            if current_pkg
            else sub_pkg.strip("/")
        )
        return (os.path.normpath(joined).replace("\\", "/"), name_part, "")
    joined = (
        f"{current_pkg}/{cleaned.strip('/')}"
        if current_pkg
        else cleaned.strip("/")
    )
    norm_pkg = os.path.normpath(joined).replace("\\", "/")
    if norm_pkg == ".":
        norm_pkg = ""
    leaf = norm_pkg.rsplit("/", 1)[-1] if norm_pkg else ""
    return (norm_pkg, leaf, toolchain)


def _resolve_git_bin(requested_git: str) -> str:
    """Resolves a working git executable path."""
    if requested_git and os.path.isfile(requested_git):
        if os.access(requested_git, os.X_OK):
            return requested_git
    found = shutil.which("git")
    if found:
        return found
    if os.path.isfile("/usr/bin/git"):
        return "/usr/bin/git"
    return "git"


def _run_git(
    git_bin: str, root: pathlib.Path, args: Sequence[str]
) -> subprocess.CompletedProcess[str]:
    """Runs a git subcommand safely without raising on non-zero exit."""
    try:
        return subprocess.run(
            [git_bin, "-C", str(root), *args],
            capture_output=True,
            text=True,
            check=False,
        )
    except OSError as exc:
        return subprocess.CompletedProcess(
            args=[git_bin, "-C", str(root), *args],
            returncode=127,
            stdout="",
            stderr=str(exc),
        )


def _extract_ast_strings(node: ast.AST | None) -> list[str]:
    """Extracts string literals from AST lists, tuples, `+`, and `select()`."""
    if node is None:
        return []
    if isinstance(node, ast.Constant) and isinstance(node.value, str):
        return [node.value]
    if isinstance(node, (ast.List, ast.Tuple)):
        items: list[str] = []
        for elt in node.elts:
            items.extend(_extract_ast_strings(elt))
        return items
    if isinstance(node, ast.BinOp) and isinstance(node.op, ast.Add):
        return _extract_ast_strings(node.left) + _extract_ast_strings(
            node.right
        )
    if isinstance(node, ast.Call):
        func_name = ""
        if isinstance(node.func, ast.Name):
            func_name = node.func.id
        elif isinstance(node.func, ast.Attribute):
            func_name = node.func.attr
        if func_name == "select" and node.args:
            first_arg = node.args[0]
            if isinstance(first_arg, ast.Dict):
                items = []
                for val_node in first_arg.values:
                    items.extend(_extract_ast_strings(val_node))
                return items
    return []


def _extract_ast_identifiers(node: ast.AST | None) -> set[str]:
    """Extracts identifier names referenced inside an AST node."""
    if node is None:
        return set()
    names: set[str] = set()
    for child in ast.walk(node):
        if isinstance(child, ast.Name):
            names.add(child.id)
    return names


def parse_bazel_file(content: str, pkg_dir: str) -> ParsedBazelFile:
    """Parses a BUILD.bazel file into BazelTarget records."""
    try:
        tree = ast.parse(content)
    except SyntaxError:
        return ParsedBazelFile(pkg_dir=pkg_dir, targets=[])

    lines = content.splitlines()
    targets: list[BazelTarget] = []

    for stmt in tree.body:
        if not isinstance(stmt, ast.Expr) or not isinstance(
            stmt.value, ast.Call
        ):
            continue
        call = stmt.value
        if isinstance(call.func, ast.Name):
            rule_type = call.func.id
        elif isinstance(call.func, ast.Attribute):
            rule_type = call.func.attr
        else:
            continue

        string_lists: dict[str, list[str]] = {}
        scalars: dict[str, Any] = {}
        present_kwargs: set[str] = set()
        has_host_os = False
        has_fuchsia_os = False

        for kw in call.keywords:
            if kw.arg is None:
                continue
            present_kwargs.add(kw.arg)
            if isinstance(kw.value, ast.Constant) and isinstance(
                kw.value.value, (str, bool, int)
            ):
                scalars[kw.arg] = kw.value.value
            extracted_strs = _extract_ast_strings(kw.value)
            if extracted_strs or isinstance(kw.value, (ast.List, ast.Tuple)):
                string_lists[kw.arg] = extracted_strs
            if kw.arg == "target_compatible_with":
                ident_names = _extract_ast_identifiers(kw.value)
                if ident_names & HOST_CONSTRAINT_IDENTIFIERS or (
                    set(extracted_strs) & HOST_CONSTRAINT_STRINGS
                ):
                    has_host_os = True
                if set(extracted_strs) & FUCHSIA_CONSTRAINT_STRINGS:
                    has_fuchsia_os = True

        target_name = str(scalars.get("name", ""))
        if not target_name and rule_type != "exports_files":
            continue

        start_line = stmt.lineno
        end_line = getattr(stmt, "end_lineno", start_line) or start_line

        # Inspect contiguous comment block immediately preceding start_line
        # as well as inline comments within [start_line, end_line].
        skip_lineno = 0
        has_reviewer_skip_note = False
        allow_manual_reason = ""

        idx = start_line - 2
        preceding_comments: list[tuple[int, str]] = []
        while idx >= 0:
            stripped = lines[idx].strip()
            if not stripped:
                break
            if stripped.startswith("#"):
                preceding_comments.append((idx + 1, stripped))
                idx -= 1
                continue
            break

        for c_lineno, c_text in reversed(preceding_comments):
            if "@bazel2gn:skip" in c_text and skip_lineno == 0:
                skip_lineno = c_lineno
            m_rev = re.match(
                r"^#\s*Needs reviewer attention \(bazel2gn skip\):\s*(.+)$",
                c_text,
            )
            if m_rev and m_rev.group(1).strip():
                has_reviewer_skip_note = True
            m_man = re.search(r"#\s*test-parity-allow-manual:\s*(.+)$", c_text)
            if m_man and m_man.group(1).strip():
                allow_manual_reason = m_man.group(1).strip()

        for body_idx in range(start_line - 1, min(end_line, len(lines))):
            line_txt = lines[body_idx]
            if "@bazel2gn:skip" in line_txt and skip_lineno == 0:
                # Track inline skip only if attached to test attribute or call.
                if any(
                    tok in line_txt
                    for tok in ("with_unit_tests", "with_host_unit_tests")
                ):
                    skip_lineno = body_idx + 1
            m_man = re.search(
                r"#\s*test-parity-allow-manual:\s*(.+)$", line_txt
            )
            if m_man and m_man.group(1).strip():
                allow_manual_reason = m_man.group(1).strip()

        canonical_sig = json.dumps(
            {
                "rule": rule_type,
                "name": target_name,
                "scalars": scalars,
                "lists": {k: sorted(v) for k, v in string_lists.items()},
                "host_os": has_host_os,
                "fuchsia_os": has_fuchsia_os,
                "skip": bool(skip_lineno),
            },
            sort_keys=True,
        )

        targets.append(
            BazelTarget(
                pkg_dir=pkg_dir,
                rule_type=rule_type,
                name=target_name,
                lineno=start_line,
                end_lineno=end_line,
                string_lists=string_lists,
                scalars=scalars,
                present_kwargs=frozenset(present_kwargs),
                has_host_os_constraints=has_host_os,
                has_fuchsia_os_constraints=has_fuchsia_os,
                skip_lineno=skip_lineno,
                has_reviewer_skip_note=has_reviewer_skip_note,
                allow_manual_reason=allow_manual_reason,
                canonical_sig=canonical_sig,
            )
        )

    return ParsedBazelFile(pkg_dir=pkg_dir, targets=targets)


@dataclasses.dataclass
class _GnToken:
    kind: str  # "ident", "str", "punct"
    value: str
    lineno: int


def _tokenize_gn(content: str) -> tuple[list[_GnToken], int]:
    """Tokenizes GN source while tracking 1-based line numbers and sentinel."""
    tokens: list[_GnToken] = []
    sentinel_lineno = 0
    n = len(content)
    i = 0
    lineno = 1

    while i < n:
        ch = content[i]
        if ch == "\n":
            lineno += 1
            i += 1
            continue
        if ch in (" ", "\t", "\r"):
            i += 1
            continue
        if ch == "#":
            end_nl = content.find("\n", i)
            if end_nl == -1:
                comment_line = content[i:]
                i = n
            else:
                comment_line = content[i:end_nl]
                i = end_nl
            if BAZEL2GN_SENTINEL in comment_line and sentinel_lineno == 0:
                sentinel_lineno = lineno
            continue
        if ch == '"':
            start_line = lineno
            i += 1
            chars: list[str] = []
            while i < n:
                c = content[i]
                if c == "\\" and i + 1 < n:
                    chars.append(content[i + 1])
                    if content[i + 1] == "\n":
                        lineno += 1
                    i += 2
                    continue
                if c == '"':
                    i += 1
                    break
                if c == "\n":
                    lineno += 1
                chars.append(c)
                i += 1
            tokens.append(_GnToken("str", "".join(chars), start_line))
            continue
        if ch.isalpha() or ch == "_":
            start = i
            while i < n and (content[i].isalnum() or content[i] == "_"):
                i += 1
            tokens.append(_GnToken("ident", content[start:i], lineno))
            continue
        if i + 1 < n and content[i : i + 2] in (
            "+=",
            "-=",
            "==",
            "!=",
            "<=",
            ">=",
            "&&",
            "||",
        ):
            tokens.append(_GnToken("punct", content[i : i + 2], lineno))
            i += 2
            continue
        tokens.append(_GnToken("punct", ch, lineno))
        i += 1

    return tokens, sentinel_lineno


def _parse_gn_target_body(
    tokens: Sequence[_GnToken], start_idx: int, end_idx: int
) -> tuple[dict[str, list[str]], dict[str, Any]]:
    """Extracts assignments (`=`, `+=`, `-=`) inside a GN target block."""
    string_lists: dict[str, list[str]] = {}
    scalars: dict[str, Any] = {}
    i = start_idx
    brace_depth = 0

    while i < end_idx:
        tok = tokens[i]
        if tok.kind == "punct" and tok.value == "{":
            brace_depth += 1
            i += 1
            continue
        if tok.kind == "punct" and tok.value == "}":
            brace_depth = max(0, brace_depth - 1)
            i += 1
            continue

        # Parse `attr = ...` or `attr += ...` (including inside `if (...) {}`
        # within the target body, skipping nested `metadata = { ... }` scopes
        # except when reading bazel_test_suite's direct attributes).
        if (
            tok.kind == "ident"
            and i + 2 <= end_idx
            and tokens[i + 1].kind == "punct"
            and tokens[i + 1].value in ("=", "+=", "-=")
        ):
            attr_name = tok.value
            op = tokens[i + 1].value
            i += 2
            if i >= end_idx:
                break
            rhs = tokens[i]
            if rhs.kind == "punct" and rhs.value == "{":
                # Skip nested scope `{ ... }` such as `metadata = { ... }`.
                sub_depth = 1
                i += 1
                while i < end_idx and sub_depth > 0:
                    if tokens[i].kind == "punct" and tokens[i].value == "{":
                        sub_depth += 1
                    elif tokens[i].kind == "punct" and tokens[i].value == "}":
                        sub_depth -= 1
                    i += 1
                continue
            if rhs.kind == "str":
                scalars[attr_name] = rhs.value
                i += 1
                continue
            if rhs.kind == "ident" and rhs.value in ("true", "false"):
                scalars[attr_name] = rhs.value == "true"
                i += 1
                continue
            if rhs.kind == "punct" and rhs.value == "[":
                # Collect strings across `[ ... ] + [ ... ]` expressions.
                collected: list[str] = []
                while i < end_idx:
                    if tokens[i].kind == "punct" and tokens[i].value == "[":
                        bracket_depth = 1
                        i += 1
                        while i < end_idx and bracket_depth > 0:
                            if (
                                tokens[i].kind == "punct"
                                and tokens[i].value == "["
                            ):
                                bracket_depth += 1
                            elif (
                                tokens[i].kind == "punct"
                                and tokens[i].value == "]"
                            ):
                                bracket_depth -= 1
                            elif tokens[i].kind == "str" and bracket_depth == 1:
                                collected.append(tokens[i].value)
                            i += 1
                        if (
                            i < end_idx
                            and tokens[i].kind == "punct"
                            and tokens[i].value == "+"
                        ):
                            i += 1
                            continue
                        break
                    if tokens[i].kind == "ident":
                        i += 1
                        if (
                            i < end_idx
                            and tokens[i].kind == "punct"
                            and tokens[i].value == "+"
                        ):
                            i += 1
                            continue
                    break
                if op == "-=":
                    existing = string_lists.get(attr_name, [])
                    string_lists[attr_name] = [
                        x for x in existing if x not in set(collected)
                    ]
                elif op == "+=":
                    string_lists.setdefault(attr_name, []).extend(collected)
                else:
                    # Inside conditional branches within a target, preserve
                    # earlier unconditional items while unioning branch items.
                    if brace_depth > 0 and attr_name in string_lists:
                        string_lists[attr_name].extend(collected)
                    else:
                        string_lists[attr_name] = collected
                continue
        i += 1

    return string_lists, scalars


def parse_gn_file(content: str, pkg_dir: str) -> ParsedGnFile:
    """Parses a BUILD.gn file into GnTarget records with condition stacks."""
    tokens, sentinel_lineno = _tokenize_gn(content)
    targets: list[GnTarget] = []
    n = len(tokens)

    def _find_matching(idx: int, open_ch: str, close_ch: str) -> int:
        depth = 0
        pos = idx
        while pos < n:
            if tokens[pos].kind == "punct" and tokens[pos].value == open_ch:
                depth += 1
            elif tokens[pos].kind == "punct" and tokens[pos].value == close_ch:
                depth -= 1
                if depth == 0:
                    return pos
            pos += 1
        return n - 1

    def _walk_statements(
        start: int, end: int, conditions: tuple[str, ...]
    ) -> None:
        i = start
        while i < end:
            tok = tokens[i]
            if tok.kind == "ident" and tok.value == "if":
                if (
                    i + 1 < end
                    and tokens[i + 1].kind == "punct"
                    and tokens[i + 1].value == "("
                ):
                    close_paren = _find_matching(i + 1, "(", ")")
                    cond_str = " ".join(
                        t.value for t in tokens[i + 2 : close_paren]
                    )
                    if (
                        close_paren + 1 < end
                        and tokens[close_paren + 1].kind == "punct"
                        and tokens[close_paren + 1].value == "{"
                    ):
                        close_brace = _find_matching(close_paren + 1, "{", "}")
                        _walk_statements(
                            close_paren + 2,
                            close_brace,
                            (*conditions, cond_str),
                        )
                        i = close_brace + 1
                        # Handle `else if (...)` or `else { ... }`
                        while (
                            i < end
                            and tokens[i].kind == "ident"
                            and tokens[i].value == "else"
                        ):
                            if (
                                i + 1 < end
                                and tokens[i + 1].kind == "ident"
                                and tokens[i + 1].value == "if"
                                and i + 2 < end
                                and tokens[i + 2].kind == "punct"
                                and tokens[i + 2].value == "("
                            ):
                                cp2 = _find_matching(i + 2, "(", ")")
                                cond2 = " ".join(
                                    t.value for t in tokens[i + 3 : cp2]
                                )
                                if (
                                    cp2 + 1 < end
                                    and tokens[cp2 + 1].kind == "punct"
                                    and tokens[cp2 + 1].value == "{"
                                ):
                                    cb2 = _find_matching(cp2 + 1, "{", "}")
                                    _walk_statements(
                                        cp2 + 2,
                                        cb2,
                                        (
                                            *conditions,
                                            f"!({cond_str}) && ({cond2})",
                                        ),
                                    )
                                    cond_str = f"{cond_str} || {cond2}"
                                    i = cb2 + 1
                                    continue
                            elif (
                                i + 1 < end
                                and tokens[i + 1].kind == "punct"
                                and tokens[i + 1].value == "{"
                            ):
                                cb_else = _find_matching(i + 1, "{", "}")
                                _walk_statements(
                                    i + 2,
                                    cb_else,
                                    (*conditions, f"!({cond_str})"),
                                )
                                i = cb_else + 1
                                break
                            break
                        continue

            if tok.kind == "ident" and tok.value == "foreach":
                if (
                    i + 1 < end
                    and tokens[i + 1].kind == "punct"
                    and tokens[i + 1].value == "("
                ):
                    cp = _find_matching(i + 1, "(", ")")
                    if (
                        cp + 1 < end
                        and tokens[cp + 1].kind == "punct"
                        and tokens[cp + 1].value == "{"
                    ):
                        cb = _find_matching(cp + 1, "{", "}")
                        _walk_statements(cp + 2, cb, conditions)
                        i = cb + 1
                        continue

            # Target declaration: `<rule_type>("<name>") { ... }`
            if (
                tok.kind == "ident"
                and tok.value
                not in (
                    "import",
                    "assert",
                    "declare_args",
                    "template",
                    "set_defaults",
                )
                and i + 3 < end
                and tokens[i + 1].kind == "punct"
                and tokens[i + 1].value == "("
                and tokens[i + 2].kind == "str"
                and tokens[i + 3].kind == "punct"
                and tokens[i + 3].value == ")"
                and i + 4 < end
                and tokens[i + 4].kind == "punct"
                and tokens[i + 4].value == "{"
            ):
                rule_type = tok.value
                target_name = tokens[i + 2].value
                start_line = tok.lineno
                close_brace = _find_matching(i + 4, "{", "}")
                end_line = tokens[close_brace].lineno
                s_lists, scalars = _parse_gn_target_body(
                    tokens, i + 5, close_brace
                )
                in_post = sentinel_lineno > 0 and start_line > sentinel_lineno
                sig = json.dumps(
                    {
                        "rule": rule_type,
                        "name": target_name,
                        "conds": conditions,
                        "scalars": scalars,
                        "lists": {k: sorted(v) for k, v in s_lists.items()},
                    },
                    sort_keys=True,
                )
                targets.append(
                    GnTarget(
                        pkg_dir=pkg_dir,
                        rule_type=rule_type,
                        name=target_name,
                        lineno=start_line,
                        end_lineno=end_line,
                        in_post_sentinel=in_post,
                        conditions=conditions,
                        string_lists=s_lists,
                        scalars=scalars,
                        canonical_sig=sig,
                    )
                )
                i = close_brace + 1
                continue

            # Skip standalone `{ ... }` or `template(...) { ... }` blocks so we
            # don't treat template internals as real package targets.
            if (
                tok.kind == "ident"
                and tok.value in ("template", "declare_args", "set_defaults")
                and i + 1 < end
                and tokens[i + 1].kind == "punct"
                and tokens[i + 1].value == "("
            ):
                cp = _find_matching(i + 1, "(", ")")
                if (
                    cp + 1 < end
                    and tokens[cp + 1].kind == "punct"
                    and tokens[cp + 1].value == "{"
                ):
                    cb = _find_matching(cp + 1, "{", "}")
                    i = cb + 1
                    continue

            i += 1

    _walk_statements(0, n, ())
    return ParsedGnFile(
        pkg_dir=pkg_dir,
        has_sentinel=sentinel_lineno > 0,
        sentinel_lineno=sentinel_lineno,
        targets=targets,
    )


def parse_commit_message(message: str) -> CommitMetadata:
    """Extracts test-parity footers and signals from a commit message."""
    lines = message.splitlines()
    has_build_only_true = False
    has_tag_planter = False
    has_migration_marker = False
    bug_ids: list[str] = []
    deleted_raw: list[tuple[int, str]] = []
    skipped_raw: list[tuple[int, str]] = []
    renamed_raw: list[tuple[int, str]] = []

    if lines and ("[bazel_migration]" in lines[0] or "[gn2gone]" in lines[0]):
        has_migration_marker = True

    for idx, line in enumerate(lines, start=1):
        if line != line.lstrip():
            continue
        stripped = line.rstrip()
        if not stripped or stripped.startswith((">", "#")):
            continue
        m_bo = re.match(r"^Build-Only\s*:\s*(.*)$", stripped, re.IGNORECASE)
        if m_bo and m_bo.group(1).strip().lower() == "true":
            has_build_only_true = True

        m_tag = re.match(r"^TAG\s*[:=]\s*(.+)$", stripped, re.IGNORECASE)
        if m_tag:
            tag_val = m_tag.group(1).strip().lower()
            if "planter" in tag_val:
                has_tag_planter = True
                has_migration_marker = True

        if re.match(r"^Bazel-Migration-Target\s*:", stripped, re.IGNORECASE):
            has_migration_marker = True

        m_bug = re.match(r"^(?:Bug|Fixed)\s*:\s*(.+)$", stripped, re.IGNORECASE)
        if m_bug:
            bug_ids.extend(re.findall(r"\b(\d{4,})\b", m_bug.group(1)))

        m_del = re.match(r"^Deleted-Tests\s*:\s*(.*)$", stripped, re.IGNORECASE)
        if m_del:
            deleted_raw.append((idx, m_del.group(1).strip()))

        m_skip = re.match(
            r"^Skipped-Dual-Tests\s*:\s*(.*)$", stripped, re.IGNORECASE
        )
        if m_skip:
            skipped_raw.append((idx, m_skip.group(1).strip()))

        m_ren = re.match(r"^Renamed-Tests\s*:\s*(.*)$", stripped, re.IGNORECASE)
        if m_ren:
            renamed_raw.append((idx, m_ren.group(1).strip()))

    return CommitMetadata(
        raw_message=message,
        has_build_only_true=has_build_only_true,
        has_tag_planter=has_tag_planter,
        has_migration_marker=has_migration_marker,
        bug_ids=tuple(bug_ids),
        deleted_tests_raw=tuple(deleted_raw),
        skipped_dual_tests_raw=tuple(skipped_raw),
        renamed_tests_raw=tuple(renamed_raw),
    )


def _condition_is_host_only(conditions: Sequence[str]) -> bool:
    """Returns True if any enclosing GN `if (...)` restricts to `is_host`."""
    for cond in conditions:
        compact = cond.replace(" ", "")
        if compact.startswith("!(is_host)"):
            continue
        if re.search(r"(?<!!)\bis_host\b", cond) or (
            "host_toolchain" in cond and "!=" not in cond
        ):
            return True
    return False


def _bazel_unit_test_polarities(target: BazelTarget) -> set[str]:
    """Returns {"host"}, {"device"}, or {"host", "device"} for rustc_* unit tests."""
    polarities: set[str] = set()
    if target.scalars.get("with_host_unit_tests") is True:
        polarities.add("host")
    wut = target.scalars.get("with_unit_tests")
    if wut is True or wut == "fuchsia":
        polarities.add("device")
    elif wut == "host":
        polarities.add("host")
    elif wut == "both":
        polarities.update(("device", "host"))
    return polarities


def _is_bazel_host_target(target: BazelTarget) -> bool:
    """Returns True if a Bazel target is strictly host-compatible."""
    if target.rule_type in BAZEL_HOST_ONLY_TEST_RULES:
        return True
    if target.has_host_os_constraints and not target.has_fuchsia_os_constraints:
        return True
    return False


def _is_bazel_device_package_target(target: BazelTarget) -> bool:
    """Returns True if a Bazel target is a Fuchsia device test package."""
    return target.rule_type in BAZEL_DEVICE_PACKAGE_RULES


class WorkspaceAnalyzer:
    """Caches parsed BUILD.gn / BUILD.bazel files and analyzes test parity."""

    def __init__(
        self,
        root: pathlib.Path,
        git_bin: str,
        base_ref: str | None,
        changed_status: dict[str, str],
        commit_meta: CommitMetadata,
    ) -> None:
        self.root = root
        self.git_bin = git_bin
        self.base_ref = base_ref
        self.changed_status = changed_status
        self.commit_meta = commit_meta
        self._after_bazel_cache: dict[str, ParsedBazelFile | None] = {}
        self._before_bazel_cache: dict[str, ParsedBazelFile | None] = {}
        self._after_gn_cache: dict[str, ParsedGnFile | None] = {}
        self._before_gn_cache: dict[str, ParsedGnFile | None] = {}
        self._before_text_cache: dict[str, str | None] = {}

    def read_after_text(self, rel_path: str) -> str | None:
        full_path = self.root / rel_path
        try:
            return full_path.read_text(encoding="utf-8", errors="replace")
        except OSError:
            return None

    def read_before_text(self, rel_path: str) -> str | None:
        if rel_path in self._before_text_cache:
            return self._before_text_cache[rel_path]
        if not self.base_ref:
            self._before_text_cache[rel_path] = None
            return None
        proc = _run_git(
            self.git_bin, self.root, ["show", f"{self.base_ref}:{rel_path}"]
        )
        text = proc.stdout if proc.returncode == 0 else None
        self._before_text_cache[rel_path] = text
        return text

    def get_after_bazel(self, pkg_dir: str) -> ParsedBazelFile | None:
        if pkg_dir in self._after_bazel_cache:
            return self._after_bazel_cache[pkg_dir]
        for fname in ("BUILD.bazel", "BUILD"):
            rel = f"{pkg_dir}/{fname}" if pkg_dir else fname
            text = self.read_after_text(rel)
            if text is not None:
                parsed = parse_bazel_file(text, pkg_dir)
                self._after_bazel_cache[pkg_dir] = parsed
                return parsed
        self._after_bazel_cache[pkg_dir] = None
        return None

    def get_before_bazel(self, pkg_dir: str) -> ParsedBazelFile | None:
        if pkg_dir in self._before_bazel_cache:
            return self._before_bazel_cache[pkg_dir]
        for fname in ("BUILD.bazel", "BUILD"):
            rel = f"{pkg_dir}/{fname}" if pkg_dir else fname
            text = self.read_before_text(rel)
            if text is not None:
                parsed = parse_bazel_file(text, pkg_dir)
                self._before_bazel_cache[pkg_dir] = parsed
                return parsed
        self._before_bazel_cache[pkg_dir] = None
        return None

    def get_after_gn(self, pkg_dir: str) -> ParsedGnFile | None:
        if pkg_dir in self._after_gn_cache:
            return self._after_gn_cache[pkg_dir]
        rel = f"{pkg_dir}/BUILD.gn" if pkg_dir else "BUILD.gn"
        text = self.read_after_text(rel)
        parsed = parse_gn_file(text, pkg_dir) if text is not None else None
        self._after_gn_cache[pkg_dir] = parsed
        return parsed

    def get_before_gn(self, pkg_dir: str) -> ParsedGnFile | None:
        if pkg_dir in self._before_gn_cache:
            return self._before_gn_cache[pkg_dir]
        rel = f"{pkg_dir}/BUILD.gn" if pkg_dir else "BUILD.gn"
        text = self.read_before_text(rel)
        parsed = parse_gn_file(text, pkg_dir) if text is not None else None
        self._before_gn_cache[pkg_dir] = parsed
        return parsed

    def is_migration_cl(self) -> bool:
        """Determines whether this CL is a Bazel migration or dual-build CL."""
        if self.commit_meta.has_migration_marker:
            return True
        if (
            self.commit_meta.deleted_tests_raw
            or self.commit_meta.skipped_dual_tests_raw
            or self.commit_meta.renamed_tests_raw
        ):
            return True

        bazel_pkgs: set[str] = set()
        gn_pkgs: set[str] = set()
        for rel_path in self.changed_status:
            base = os.path.basename(rel_path)
            pkg = _pkg_dir_of(rel_path)
            if base in ("BUILD.bazel", "BUILD"):
                bazel_pkgs.add(pkg)
            elif base == "BUILD.gn":
                gn_pkgs.add(pkg)
                after_gn = self.get_after_gn(pkg)
                before_gn = self.get_before_gn(pkg)
                if (after_gn and after_gn.has_sentinel) or (
                    before_gn and before_gn.has_sentinel
                ):
                    return True
        if bazel_pkgs & gn_pkgs:
            return True
        for b_pkg in bazel_pkgs:
            after_gn = self.get_after_gn(b_pkg)
            if after_gn and after_gn.has_sentinel:
                return True
        return False

    def ancestor_chain(self, pkg_dir: str) -> list[str]:
        """Returns [pkg_dir, parent, ..., ''] walking up to workspace root."""
        if not pkg_dir:
            return [""]
        parts = [p for p in pkg_dir.split("/") if p]
        chain = ["/".join(parts[:i]) for i in range(len(parts), 0, -1)]
        chain.append("")
        return chain

    def _gn_suite_is_wired(
        self, gn_file: ParsedGnFile, suite: GnTarget
    ) -> bool:
        """Checks that a bazel_test_suite is wired into group('tests') or parent."""
        if _condition_is_host_only(suite.conditions):
            return False
        all_local_groups: dict[str, list[GnTarget]] = {}
        for t in gn_file.targets:
            if t.rule_type == "group" and not _condition_is_host_only(
                t.conditions
            ):
                all_local_groups.setdefault(t.name, []).append(t)
        local_groups = [
            t
            for t in gn_file.targets
            if t.rule_type == "group" and t.name in TEST_GROUP_NAMES
        ]
        if not local_groups:
            return True
        work = [
            grp
            for grp in local_groups
            if not _condition_is_host_only(grp.conditions)
        ]
        seen_names: set[str] = {grp.name for grp in work}
        while work:
            grp = work.pop()
            deps = grp.string_lists.get("deps", []) + grp.string_lists.get(
                "public_deps", []
            )
            for dep in deps:
                parsed = _split_label(dep, gn_file.pkg_dir)
                if not parsed or parsed[0] != gn_file.pkg_dir:
                    continue
                if parsed[1] == suite.name:
                    return True
                if (
                    parsed[1] in all_local_groups
                    and parsed[1] not in seen_names
                ):
                    seen_names.add(parsed[1])
                    work.extend(all_local_groups[parsed[1]])
        # Also allow an ancestor BUILD.gn directly depending on //pkg:suite_name.
        for anc in self.ancestor_chain(gn_file.pkg_dir)[1:]:
            anc_gn = self.get_after_gn(anc)
            if not anc_gn:
                continue
            for grp in anc_gn.targets:
                if grp.rule_type != "group":
                    continue
                deps = grp.string_lists.get("deps", []) + grp.string_lists.get(
                    "public_deps", []
                )
                for dep in deps:
                    parsed = _split_label(dep, anc)
                    if (
                        parsed
                        and parsed[0] == gn_file.pkg_dir
                        and parsed[1] == suite.name
                    ):
                        return True
        return False

    def compute_reachable_bazel_tests(
        self, seed_pkgs: Sequence[str]
    ) -> dict[tuple[str, str], set[str]]:
        """Computes `(pkg_dir, target_name) -> {'host'|'device'}` reachability.

        Walks all valid `bazel_test_suite(...)` targets in `seed_pkgs` and their
        ancestors, expanding `test_suite(...)` targets and intra-package binary
        / component wrappers.
        """
        checked_gn_pkgs: set[str] = set()
        for pkg in seed_pkgs:
            checked_gn_pkgs.update(self.ancestor_chain(pkg))

        reachable: dict[tuple[str, str], set[str]] = {}
        queue: list[tuple[str, str, str]] = []

        for gn_pkg in sorted(checked_gn_pkgs):
            gn_file = self.get_after_gn(gn_pkg)
            if not gn_file:
                continue
            for target in gn_file.targets:
                if target.rule_type == "bazel_test_suite":
                    if not self._gn_suite_is_wired(gn_file, target):
                        continue
                    for lbl in target.string_lists.get("host_tests", []):
                        parsed = _split_label(lbl, gn_pkg)
                        if parsed:
                            queue.append((parsed[0], parsed[1], "host"))
                    for lbl in target.string_lists.get("target_tests", []):
                        parsed = _split_label(lbl, gn_pkg)
                        if parsed:
                            queue.append((parsed[0], parsed[1], "device"))
                elif target.rule_type == "bazel_fuchsia_test_package":
                    if not self._gn_suite_is_wired(gn_file, target):
                        continue
                    raw_bt = target.scalars.get("bazel_target")
                    bt_lbl = (
                        raw_bt
                        if isinstance(raw_bt, str) and raw_bt
                        else f":{target.name}"
                    )
                    parsed = _split_label(bt_lbl, gn_pkg)
                    if parsed:
                        queue.append((parsed[0], parsed[1], "device"))

        visited_queue: set[tuple[str, str, str]] = set()
        while queue:
            b_pkg, b_name, bucket = queue.pop()
            state_key = (b_pkg, b_name, bucket)
            if state_key in visited_queue:
                continue
            visited_queue.add(state_key)
            reachable.setdefault((b_pkg, b_name), set()).add(bucket)

            b_file = self.get_after_bazel(b_pkg)
            if not b_file:
                continue
            by_name = {t.name: t for t in b_file.targets if t.name}
            tgt = by_name.get(b_name)
            if tgt is None:
                continue

            if tgt.rule_type == "test_suite":
                if "tests" in tgt.present_kwargs:
                    for sub_lbl in tgt.string_lists.get("tests", []):
                        parsed = _split_label(sub_lbl, b_pkg)
                        if parsed:
                            queue.append((parsed[0], parsed[1], bucket))
                else:
                    # Implicit package test_suite collects non-manual test rules
                    # in `b_pkg`.
                    for cand in b_file.targets:
                        if cand.name == tgt.name:
                            continue
                        tags = cand.string_lists.get("tags", [])
                        if "manual" in tags:
                            continue
                        if cand.rule_type in BAZEL_ALL_EXPLICIT_TEST_RULES:
                            queue.append((b_pkg, cand.name, bucket))
                        elif cand.rule_type in BAZEL_RUST_UNITTEST_OWNER_RULES:
                            if _bazel_unit_test_polarities(cand):
                                queue.append(
                                    (b_pkg, f"{cand.name}_test", bucket)
                                )
                continue

            # Propagate reachability down intra-package test wrappers.
            if bucket == "device":
                for attr in (
                    "unit_tests",
                    "test_components",
                    "components",
                    "deps",
                ):
                    for ref in tgt.string_lists.get(attr, []):
                        parsed = _split_label(ref, b_pkg)
                        if parsed and parsed[0] == b_pkg:
                            queue.append((b_pkg, parsed[1], "device"))
                for scalar_attr in ("package", "binary", "test_component"):
                    ref_val = tgt.scalars.get(scalar_attr)
                    if isinstance(ref_val, str):
                        parsed = _split_label(ref_val, b_pkg)
                        if parsed and parsed[0] == b_pkg:
                            queue.append((b_pkg, parsed[1], "device"))
            elif bucket == "host":
                for scalar_attr in ("binary", "test_binary"):
                    ref_val = tgt.scalars.get(scalar_attr)
                    if isinstance(ref_val, str):
                        parsed = _split_label(ref_val, b_pkg)
                        if parsed and parsed[0] == b_pkg:
                            queue.append((b_pkg, parsed[1], "host"))
                bin_name = tgt.scalars.get("binary_name")
                if isinstance(bin_name, str) and bin_name:
                    queue.append((b_pkg, bin_name, "host"))

        return reachable

    def _intra_package_wrapped_binaries(
        self, b_file: ParsedBazelFile
    ) -> tuple[set[str], set[str]]:
        """Returns `(device_wrapped_names, host_wrapped_names)` in `b_file`."""
        by_name = {t.name: t for t in b_file.targets if t.name}
        device_wrapped: set[str] = set()
        host_wrapped: set[str] = set()

        def _collect_pkg_binaries(target_name: str, seen: set[str]) -> None:
            if target_name in seen:
                return
            seen.add(target_name)
            obj = by_name.get(target_name)
            if obj is None:
                return
            for attr in ("unit_tests", "test_components", "components", "deps"):
                for ref in obj.string_lists.get(attr, []):
                    parsed = _split_label(ref, b_file.pkg_dir)
                    if parsed and parsed[0] == b_file.pkg_dir:
                        device_wrapped.add(parsed[1])
                        _collect_pkg_binaries(parsed[1], seen)
            for scalar_attr in ("package", "binary", "test_component"):
                val = obj.scalars.get(scalar_attr)
                if isinstance(val, str):
                    parsed = _split_label(val, b_file.pkg_dir)
                    if parsed and parsed[0] == b_file.pkg_dir:
                        device_wrapped.add(parsed[1])
                        _collect_pkg_binaries(parsed[1], seen)

        for t in b_file.targets:
            if t.rule_type in BAZEL_DEVICE_PACKAGE_RULES or (
                t.rule_type == "fx_package"
                and bool(t.string_lists.get("test_components"))
            ):
                _collect_pkg_binaries(t.name, set())
            elif t.rule_type in ("host_test", "wrap_host_rust_test"):
                b_ref = t.scalars.get("binary")
                if isinstance(b_ref, str):
                    parsed = _split_label(b_ref, b_file.pkg_dir)
                    if parsed and parsed[0] == b_file.pkg_dir:
                        host_wrapped.add(parsed[1])
                b_name = t.scalars.get("binary_name")
                if isinstance(b_name, str) and b_name:
                    host_wrapped.add(b_name)

        return device_wrapped, host_wrapped

    def check_touched_gn_suites(
        self, touched_gn_pkgs: Sequence[str]
    ) -> list[Finding]:
        """Checks FM2 (`if (is_host)`) and FM3/FM4 bucket mismatches in GN."""
        findings: list[Finding] = []
        for pkg in touched_gn_pkgs:
            gn_file = self.get_after_gn(pkg)
            if not gn_file:
                continue
            gn_rel = f"{pkg}/BUILD.gn" if pkg else "BUILD.gn"

            for target in gn_file.targets:
                if target.rule_type != "bazel_test_suite":
                    continue
                suite_label = _format_label(pkg, target.name)
                if _condition_is_host_only(target.conditions):
                    findings.append(
                        Finding(
                            level="error",
                            filepath=gn_rel,
                            line=target.lineno,
                            end_line=target.end_lineno,
                            message=(
                                "🚨 [TEST PARITY ALERT \u2014 CLASS 1 SILENT ORPHANING"
                                " (FM2: bazel_test_suite INSIDE if (is_host))] "
                                f"Target `{suite_label}` in `{gn_rel}` (line "
                                f"{target.lineno}) is wrapped inside "
                                "`if (is_host)`, which prevents `$default_toolchain`"
                                " metadata collection and silently drops ALL "
                                "referenced Bazel tests from `tests.json`.\n"
                                "Remediation: Move `bazel_test_suite("
                                f'"{target.name}")` outside `if (is_host)` so it '
                                "is evaluated unconditionally in "
                                "`$default_toolchain`, and depend on "
                                f'`:{target.name}` from `group("tests")`.'
                            ),
                        )
                    )
                elif not self._gn_suite_is_wired(gn_file, target):
                    findings.append(
                        Finding(
                            level="error",
                            filepath=gn_rel,
                            line=target.lineno,
                            end_line=target.end_lineno,
                            message=(
                                "🚨 [TEST PARITY ALERT \u2014 CLASS 1 SILENT ORPHANING"
                                " (UNWIRED bazel_test_suite)] "
                                f"Target `{suite_label}` in `{gn_rel}` (line "
                                f"{target.lineno}) is not depended upon by "
                                '`group("tests")`, so its tests never enter '
                                "`tests.json`.\n"
                                f'Remediation: Add `":{target.name}"` to `deps` '
                                f'of `group("tests")` in `{gn_rel}`.'
                            ),
                        )
                    )

                # Validate FM3 / FM4 bucket polarity for target_tests & host_tests.
                self._check_suite_bucket_entries(
                    gn_rel, pkg, target, "target_tests", findings
                )
                self._check_suite_bucket_entries(
                    gn_rel, pkg, target, "host_tests", findings
                )

        return findings

    def _check_suite_bucket_entries(
        self,
        gn_rel: str,
        gn_pkg: str,
        suite_target: GnTarget,
        bucket_attr: str,
        findings: list[Finding],
    ) -> None:
        """Checks FM3/FM4 host vs. target OS bucket mismatches recursively."""
        entries = suite_target.string_lists.get(bucket_attr, [])
        before_gn = self.get_before_gn(gn_pkg)
        before_suite_entries: set[str] = set()
        if before_gn is not None:
            for bt in before_gn.targets:
                if bt.rule_type == "bazel_test_suite":
                    for raw_b in bt.string_lists.get(bucket_attr, []):
                        parsed_b = _split_label(raw_b, gn_pkg)
                        if parsed_b:
                            before_suite_entries.add(
                                _format_label(parsed_b[0], parsed_b[1])
                            )
        seen: set[tuple[str, str]] = set()

        def _inspect(b_pkg: str, b_name: str, via_suite: bool) -> None:
            if (b_pkg, b_name) in seen:
                return
            seen.add((b_pkg, b_name))
            b_file = self.get_after_bazel(b_pkg)
            if not b_file:
                return
            b_rel = f"{b_pkg}/BUILD.bazel" if b_pkg else "BUILD.bazel"
            by_name = {t.name: t for t in b_file.targets if t.name}
            tgt = by_name.get(b_name)
            if tgt is None:
                return
            lbl = _format_label(b_pkg, b_name)

            if tgt.rule_type == "test_suite":
                sub_labels = tgt.string_lists.get("tests", [])
                if bucket_attr == "host_tests":
                    is_newly_wired = (
                        bool(self.base_ref)
                        and not via_suite
                        and lbl not in before_suite_entries
                    )
                    if not tgt.has_host_os_constraints and (
                        b_rel in self.changed_status or is_newly_wired
                    ):
                        report_file = (
                            b_rel if b_rel in self.changed_status else gn_rel
                        )
                        report_line = (
                            tgt.lineno
                            if b_rel in self.changed_status
                            else suite_target.lineno
                        )
                        report_end = (
                            tgt.end_lineno
                            if b_rel in self.changed_status
                            else suite_target.end_lineno
                        )
                        findings.append(
                            Finding(
                                level="error",
                                filepath=report_file,
                                line=report_line,
                                end_line=report_end,
                                message=(
                                    "🚨 [TEST PARITY ALERT \u2014 CLASS 1 SILENT "
                                    "ORPHANING (FM3: HOST test_suite MISSING "
                                    "HOST_OS_CONSTRAINTS)] "
                                    f"`test_suite` `{lbl}` in `{b_rel}` (line "
                                    f"{tgt.lineno}) is referenced via "
                                    f"`host_tests` of `bazel_test_suite` in "
                                    f"`{gn_rel}`, but omits "
                                    "`target_compatible_with = "
                                    "HOST_OS_CONSTRAINTS`.\n"
                                    "Remediation: Load `HOST_OS_CONSTRAINTS` "
                                    "from "
                                    '`"//build/bazel/platforms:constraints.bzl"`'
                                    " and add "
                                    "`target_compatible_with = "
                                    "HOST_OS_CONSTRAINTS` to "
                                    f'`test_suite(name = "{b_name}")`.'
                                ),
                            )
                        )
                elif (
                    bucket_attr == "target_tests"
                    and tgt.has_host_os_constraints
                ):
                    findings.append(
                        Finding(
                            level="error",
                            filepath=gn_rel,
                            line=suite_target.lineno,
                            end_line=suite_target.end_lineno,
                            message=(
                                "🚨 [TEST PARITY ALERT \u2014 CLASS 1 SILENT "
                                "ORPHANING (FM4: HOST TARGET IN target_tests)] "
                                f"Host-only `test_suite` `{lbl}` (`{b_rel}`:"
                                f"{tgt.lineno}) is listed in `target_tests` of "
                                f'`bazel_test_suite("{suite_target.name}")` in '
                                f"`{gn_rel}`, where `--config=fuchsia_platform` "
                                "silently filters it out to 0 tests!\n"
                                f'Remediation: Move `"{lbl}"` from '
                                "`target_tests = [...]` to `host_tests = [...]`"
                                f" in `{gn_rel}`."
                            ),
                        )
                    )
                for sub in sub_labels:
                    parsed = _split_label(sub, b_pkg)
                    if parsed:
                        _inspect(parsed[0], parsed[1], True)
                return

            if bucket_attr == "target_tests" and _is_bazel_host_target(tgt):
                findings.append(
                    Finding(
                        level="error",
                        filepath=gn_rel,
                        line=suite_target.lineno,
                        end_line=suite_target.end_lineno,
                        message=(
                            "🚨 [TEST PARITY ALERT \u2014 CLASS 1 SILENT ORPHANING "
                            "(FM4: HOST TEST IN target_tests)] "
                            f"Host test `{lbl}` (`{tgt.rule_type}` in `{b_rel}`:"
                            f"{tgt.lineno}) is wired into `target_tests` of "
                            f'`bazel_test_suite("{suite_target.name}")` in '
                            f"`{gn_rel}`; `bazel cquery --config=fuchsia_platform`"
                            " silently filters host tests out to 0!\n"
                            f"Remediation: Move `{lbl}` into "
                            f"`host_tests = [...]` (or a host `test_suite` with "
                            "`target_compatible_with = HOST_OS_CONSTRAINTS`)."
                        ),
                    )
                )
            elif (
                bucket_attr == "host_tests"
                and _is_bazel_device_package_target(tgt)
            ):
                findings.append(
                    Finding(
                        level="error",
                        filepath=gn_rel,
                        line=suite_target.lineno,
                        end_line=suite_target.end_lineno,
                        message=(
                            "🚨 [TEST PARITY ALERT \u2014 CLASS 1 SILENT ORPHANING "
                            "(FM4: DEVICE TEST PACKAGE IN host_tests)] "
                            f"Fuchsia device test `{lbl}` (`{tgt.rule_type}` in "
                            f"`{b_rel}`:{tgt.lineno}) is wired into `host_tests`"
                            f' of `bazel_test_suite("{suite_target.name}")` in '
                            f"`{gn_rel}`, where `--config=host` silently drops "
                            "device packages!\n"
                            f"Remediation: Move `{lbl}` into "
                            "`target_tests = [...]`."
                        ),
                    )
                )

        for raw in entries:
            parsed = _split_label(raw, gn_pkg)
            if parsed:
                _inspect(parsed[0], parsed[1], False)

    def check_touched_bazel_packages(
        self,
        touched_bazel_pkgs: Sequence[str],
        reachable: dict[tuple[str, str], set[str]],
    ) -> list[Finding]:
        """Checks Class 1 unwired/`manual` Bazel tests, FM8 features, and FM5 skip."""
        findings: list[Finding] = []
        (
            skipped_footer_set,
            skipped_valid,
            skipped_err,
        ) = self._parse_skipped_dual_tests()
        if skipped_err:
            findings.append(skipped_err)

        for pkg in touched_bazel_pkgs:
            b_file = self.get_after_bazel(pkg)
            if not b_file:
                continue
            b_rel = f"{pkg}/BUILD.bazel" if pkg else "BUILD.bazel"
            by_name = {t.name: t for t in b_file.targets if t.name}
            dev_wrapped, host_wrapped = self._intra_package_wrapped_binaries(
                b_file
            )
            after_gn = self.get_after_gn(pkg)
            is_dual_pkg = bool(after_gn and after_gn.has_sentinel)
            before_b = self.get_before_bazel(pkg)
            before_gn = self.get_before_gn(pkg)
            before_is_dual_pkg = bool(before_gn and before_gn.has_sentinel)
            before_skipped_names: set[str] = (
                {
                    bt.name
                    for bt in before_b.targets
                    if bt.name and bt.skip_lineno > 0
                }
                if (
                    self.base_ref
                    and before_is_dual_pkg
                    and before_b is not None
                )
                else set()
            )
            kept_above_sentinel_names = self._kept_gn_test_names_above_sentinel(
                after_gn
            )

            # Check `test_suite` rules in touched BUILD.bazel that aggregate
            # host targets without `target_compatible_with = HOST_OS_CONSTRAINTS`
            # (FM3, fxrev.dev/1927506).
            for target in b_file.targets:
                if target.rule_type != "test_suite":
                    continue
                sub_labels = target.string_lists.get("tests", [])
                if not sub_labels or target.has_host_os_constraints:
                    continue
                sub_host_targets: list[str] = []
                all_sub_are_host = True
                resolved_any = False
                for sub_lbl in sub_labels:
                    parsed = _split_label(sub_lbl, pkg)
                    if not parsed:
                        continue
                    sub_b = self.get_after_bazel(parsed[0])
                    if not sub_b:
                        continue
                    sub_map = {t.name: t for t in sub_b.targets if t.name}
                    sub_tgt = sub_map.get(parsed[1])
                    if sub_tgt is None:
                        continue
                    resolved_any = True
                    if _is_bazel_host_target(sub_tgt):
                        sub_host_targets.append(
                            _format_label(parsed[0], parsed[1])
                        )
                    else:
                        all_sub_are_host = False
                suite_reach = reachable.get((pkg, target.name), set())
                if resolved_any and all_sub_are_host and sub_host_targets:
                    if "host" not in suite_reach:
                        # Avoid double-reporting if already caught by
                        # _check_suite_bucket_entries via host_tests.
                        findings.append(
                            Finding(
                                level="error",
                                filepath=b_rel,
                                line=target.lineno,
                                end_line=target.end_lineno,
                                message=(
                                    "🚨 [TEST PARITY ALERT \u2014 CLASS 1 SILENT "
                                    "ORPHANING (FM3: HOST test_suite MISSING "
                                    "HOST_OS_CONSTRAINTS)] "
                                    f"`test_suite` `{_format_label(pkg, target.name)}`"
                                    f" in `{b_rel}` (line {target.lineno}) "
                                    "aggregates host-only test target(s) "
                                    f"({', '.join(sub_host_targets[:3])}) "
                                    "without `target_compatible_with = "
                                    "HOST_OS_CONSTRAINTS`.\n"
                                    "Remediation: Add `target_compatible_with ="
                                    " HOST_OS_CONSTRAINTS` to "
                                    f'`test_suite(name = "{target.name}")` and '
                                    "wire it into `bazel_test_suite(host_tests "
                                    "= [...])`."
                                ),
                            )
                        )

            for target in b_file.targets:
                lbl = _format_label(pkg, target.name)
                srcs = target.string_lists.get("srcs", [])

                # Rule 2 (FM8): `rust_test` / `rustc_test` with `crate = ":lib"`
                # dropping `crate_features`.
                if target.rule_type in (
                    "rust_test",
                    "rustc_test",
                    "host_rustc_test",
                ):
                    crate_ref = target.scalars.get("crate")
                    if isinstance(crate_ref, str):
                        parsed_crate = _split_label(crate_ref, pkg)
                        if parsed_crate and parsed_crate[0] == pkg:
                            lib_tgt = by_name.get(parsed_crate[1])
                            if lib_tgt is not None:
                                lib_feats = set(
                                    lib_tgt.string_lists.get(
                                        "crate_features", []
                                    )
                                )
                                test_feats = set(
                                    target.string_lists.get(
                                        "crate_features", []
                                    )
                                )
                                missing_feats = sorted(lib_feats - test_feats)
                                if missing_feats:
                                    merged = sorted(lib_feats | test_feats)
                                    feats_str = json.dumps(merged)
                                    findings.append(
                                        Finding(
                                            level="error",
                                            filepath=b_rel,
                                            line=target.lineno,
                                            end_line=target.end_lineno,
                                            message=(
                                                "🚨 [TEST PARITY ALERT \u2014 FM8 "
                                                "LOST #[cfg(feature)] UNIT "
                                                f"TESTS] `{lbl}` (`{b_rel}`:"
                                                f"{target.lineno}) sets "
                                                f'`crate = "{crate_ref}"` '
                                                "without inheriting "
                                                f"`crate_features` "
                                                f"{missing_feats} from "
                                                f"`{_format_label(pkg, lib_tgt.name)}`."
                                                " `rules_rust` does NOT inherit"
                                                " `crate_features` from `crate`,"
                                                " silently compiling out "
                                                "feature-gated unit tests!\n"
                                                "Remediation: Add "
                                                f"`crate_features = {feats_str}`"
                                                f" to `{target.rule_type}"
                                                f'(name = "{target.name}")`.'
                                            ),
                                        )
                                    )

                is_explicit_test = (
                    target.rule_type in BAZEL_ALL_EXPLICIT_TEST_RULES
                )
                implicit_polarities = (
                    _bazel_unit_test_polarities(target)
                    if target.rule_type in BAZEL_RUST_UNITTEST_OWNER_RULES
                    else set()
                )

                if not is_explicit_test and not implicit_polarities:
                    continue

                # Rule 1a: `tags = ["manual"]` on any explicit test rule.
                tags = target.string_lists.get("tags", [])
                if (
                    is_explicit_test
                    and "manual" in tags
                    and not target.allow_manual_reason
                ):
                    src_note = (
                        f" (sources: {', '.join(srcs[:3])})" if srcs else ""
                    )
                    findings.append(
                        Finding(
                            level="error",
                            filepath=b_rel,
                            line=target.lineno,
                            end_line=target.end_lineno,
                            message=(
                                "🚨 [TEST PARITY ALERT \u2014 CLASS 1 SILENT "
                                'ORPHANING (tags = ["manual"] ON TEST TARGET)] '
                                f"Test target `{lbl}`{src_note} in `{b_rel}` "
                                f"(line {target.lineno}) sets "
                                '`tags = ["manual"]`, excluding it from '
                                "wildcard queries and `test_suite` expansion.\n"
                                'Remediation: Remove `"manual"` from `tags` and'
                                " wire the test into `bazel_test_suite(...)`, "
                                "or if strictly required on an inner helper, "
                                "document `# test-parity-allow-manual: "
                                "<reason>` directly above the target."
                            ),
                        )
                    )

                # Rule 3 (FM5): `# @bazel2gn:skip` on test targets in dual-build
                # packages.
                if (
                    is_dual_pkg
                    and target.skip_lineno > 0
                    and target.name not in before_skipped_names
                ):
                    # Skip reporting inner binary if its outer wrapper in the
                    # same BUILD.bazel also has `# @bazel2gn:skip`, or if a
                    # hand-written GN copy exists above the sentinel.
                    is_kept_in_gn = target.name in kept_above_sentinel_names
                    outer_also_skipped = target.name in (
                        dev_wrapped | host_wrapped
                    ) and any(
                        w.skip_lineno > 0
                        for w in b_file.targets
                        if w.rule_type in BAZEL_DEVICE_PACKAGE_RULES
                        and target.name in w.string_lists.get("unit_tests", [])
                    )
                    if not is_kept_in_gn and not outer_also_skipped:
                        in_footer = lbl in skipped_footer_set
                        has_bug = bool(self.commit_meta.bug_ids)
                        if (
                            not target.has_reviewer_skip_note
                            or not in_footer
                            or not has_bug
                            or not skipped_valid
                            or self.commit_meta.has_tag_planter
                        ):
                            missing_parts: list[str] = []
                            if not target.has_reviewer_skip_note:
                                missing_parts.append(
                                    "`# Needs reviewer attention (bazel2gn "
                                    "skip): <reason>` comment above "
                                    "`# @bazel2gn:skip`"
                                )
                            if not in_footer:
                                missing_parts.append(
                                    f"`Skipped-Dual-Tests: {lbl}` footer in "
                                    "commit message"
                                )
                            if not has_bug:
                                missing_parts.append(
                                    "numeric `Bug: <id>` footer in commit "
                                    "message"
                                )
                            if self.commit_meta.has_tag_planter:
                                missing_parts.append(
                                    "human author required (`TAG: planter` "
                                    "agents may never skip dual-build tests)"
                                )
                            findings.append(
                                Finding(
                                    level="error",
                                    filepath=b_rel,
                                    line=target.skip_lineno,
                                    end_line=target.end_lineno,
                                    message=(
                                        "🚨 [TEST PARITY ALERT \u2014 UNATTESTED "
                                        "DUAL-BUILD TEST SKIP (FM5)] "
                                        f"`# @bazel2gn:skip` on `{lbl}` in "
                                        f"`{b_rel}` (line {target.skip_lineno})"
                                        " drops the GN dual-build copy of this "
                                        "test without required attestation "
                                        f"(missing: {'; '.join(missing_parts)})."
                                        "\nRemediation: Remove "
                                        "`# @bazel2gn:skip` and run "
                                        "`fx bazel2gn` so the test runs in both"
                                        " Bazel and GN, or add BOTH "
                                        "`# Needs reviewer attention (bazel2gn "
                                        "skip): <reason>` above "
                                        "`# @bazel2gn:skip` AND commit footers "
                                        f"`Skipped-Dual-Tests: {lbl}` + "
                                        "`Bug: <id>` (and obtain human "
                                        "`Test-Drop-Verified +1`)."
                                    ),
                                )
                            )
                        else:
                            findings.append(
                                Finding(
                                    level="error",
                                    filepath=b_rel,
                                    line=target.skip_lineno,
                                    end_line=target.end_lineno,
                                    properties={
                                        "fail_if_missing_label": (
                                            "Test-Drop-Verified"
                                        )
                                    },
                                    message=(
                                        "🚨 [TEST PARITY ALERT \u2014 DUAL-BUILD "
                                        "TEST SKIP REQUIRES Test-Drop-Verified "
                                        f"+1] `{lbl}` in `{b_rel}` (line "
                                        f"{target.skip_lineno}) skips GN "
                                        "dual-build test generation via "
                                        "`# @bazel2gn:skip` (Bug: "
                                        f"{self.commit_meta.bug_ids[0]}).\n"
                                        "Remediation: A human reviewer must "
                                        "verify the skip justification and vote"
                                        " `Test-Drop-Verified +1` on Gerrit to "
                                        "clear this gate."
                                    ),
                                )
                            )

                # Rule 1b: Transitive `bazel_test_suite` reachability.
                if is_explicit_test:
                    if target.rule_type in BAZEL_AMBI_BINARY_TEST_RULES and (
                        target.name in dev_wrapped
                        or target.name in host_wrapped
                    ):
                        # Inner binary wrapped by a package/host_test in the
                        # same BUILD.bazel; reachability is checked on the outer
                        # wrapper target.
                        continue
                    needed_bucket = (
                        "host" if _is_bazel_host_target(target) else "device"
                    )
                    got_buckets = reachable.get((pkg, target.name), set())
                    if needed_bucket not in got_buckets:
                        # Check if a legacyambi binary (cc_test/go_test) without
                        # explicit OS constraints was wired into host_tests.
                        if (
                            target.rule_type in BAZEL_AMBI_BINARY_TEST_RULES
                            and not target.has_fuchsia_os_constraints
                            and "host" in got_buckets
                        ):
                            continue
                        bucket_field = (
                            "host_tests"
                            if needed_bucket == "host"
                            else "target_tests"
                        )
                        src_note = (
                            f" (sources: {', '.join(srcs[:3])})" if srcs else ""
                        )
                        findings.append(
                            Finding(
                                level="error",
                                filepath=b_rel,
                                line=target.lineno,
                                end_line=target.end_lineno,
                                message=(
                                    "🚨 [TEST PARITY ALERT \u2014 CLASS 1 SILENT "
                                    "ORPHANING (UNWIRED BAZEL TEST TARGET)] "
                                    f"Bazel test `{lbl}` (`{target.rule_type}`"
                                    f"{src_note}) in `{b_rel}` (line "
                                    f"{target.lineno}) is NOT transitively "
                                    "referenced by any `bazel_test_suite("
                                    f"{bucket_field} = [...])` in `{pkg or '.'}`"
                                    " or any ancestor `BUILD.gn`! Unwired Bazel"
                                    " tests never enter `tests.json` and never "
                                    "run on CQ.\n"
                                    f"Remediation: Wire `{lbl}` into an "
                                    "existing ancestor `test_suite()` / "
                                    f"`bazel_test_suite({bucket_field} = [...])`"
                                    ', or define `bazel_test_suite("bazel_tests"'
                                    f") {{ {bucket_field} = "
                                    f'["{lbl}"] }}` wired into `group("tests")`.'
                                ),
                            )
                        )
                elif implicit_polarities:
                    synth_name = f"{target.name}_test"
                    synth_lbl = _format_label(pkg, synth_name)
                    got_buckets = reachable.get((pkg, synth_name), set())
                    for pol in sorted(implicit_polarities):
                        if pol == "device" and synth_name in dev_wrapped:
                            # Reaching the outer fx_unittest_package already
                            # propagates "device" to synth_name when wired; if
                            # the outer package itself is unwired, the outer
                            # package emits the error.
                            continue
                        if pol not in got_buckets:
                            bucket_field = (
                                "host_tests"
                                if pol == "host"
                                else "target_tests"
                            )
                            findings.append(
                                Finding(
                                    level="error",
                                    filepath=b_rel,
                                    line=target.lineno,
                                    end_line=target.end_lineno,
                                    message=(
                                        "🚨 [TEST PARITY ALERT \u2014 CLASS 1 SILENT"
                                        " ORPHANING (UNWIRED IMPLICIT RUST UNIT"
                                        f" TEST)] Implicit `{pol}` unit test "
                                        f"`{synth_lbl}` generated by "
                                        f"`{target.rule_type}(name = "
                                        f'"{target.name}")` in `{b_rel}` (line '
                                        f"{target.lineno}) is NOT wired into "
                                        f"`bazel_test_suite({bucket_field} = "
                                        "[...])`!\n"
                                        f"Remediation: Package/wire `{synth_lbl}`"
                                        f" into `bazel_test_suite({bucket_field}"
                                        " = [...])` so it runs in `tests.json`."
                                    ),
                                )
                            )

        return findings

    def _kept_gn_test_names_above_sentinel(
        self, gn_file: ParsedGnFile | None
    ) -> set[str]:
        """Returns GN test target names defined above BAZEL2GN SENTINEL."""
        if not gn_file:
            return set()
        kept: set[str] = set()
        for t in gn_file.targets:
            if t.in_post_sentinel:
                continue
            if t.rule_type in GN_EXPLICIT_TEST_RULES:
                kept.add(t.name)
                pkg_name = t.scalars.get("package_name")
                if isinstance(pkg_name, str) and pkg_name.endswith("-gn"):
                    kept.add(pkg_name[:-3])
        return kept

    def _parse_skipped_dual_tests(
        self,
    ) -> tuple[set[str], bool, Finding | None]:
        """Parses and validates `Skipped-Dual-Tests:` footers."""
        labels: set[str] = set()
        for lineno, raw_val in self.commit_meta.skipped_dual_tests_raw:
            if not raw_val or "*" in raw_val:
                return (
                    labels,
                    False,
                    Finding(
                        level="error",
                        message=(
                            "🚨 [TEST PARITY ALERT \u2014 INVALID "
                            "Skipped-Dual-Tests: FOOTER] Commit message line "
                            f"{lineno} has invalid `Skipped-Dual-Tests: "
                            f"{raw_val}` (wildcards `*` and empty values are "
                            "forbidden).\n"
                            "Remediation: List exact canonical `//<pkg>:<target>`"
                            " labels, e.g. `Skipped-Dual-Tests: "
                            "//src/foo:foo_tests`."
                        ),
                    ),
                )
            for item in [x.strip() for x in raw_val.split(",") if x.strip()]:
                if not re.match(r"^//[\w./\-]*:[\w.\-]+$", item):
                    return (
                        labels,
                        False,
                        Finding(
                            level="error",
                            message=(
                                "🚨 [TEST PARITY ALERT \u2014 INVALID "
                                "Skipped-Dual-Tests: FOOTER] Entry "
                                f"`{item}` on commit message line {lineno} must"
                                " be an exact `//<pkg>:<target>` label."
                            ),
                        ),
                    )
                labels.add(item)
        return labels, True, None

    def _parse_deleted_tests_footer(
        self,
    ) -> tuple[set[str], bool, str]:
        """Parses `Deleted-Tests:` footers into `(labels, is_valid, error_reason)`."""
        labels: set[str] = set()
        if not self.commit_meta.deleted_tests_raw:
            return (
                labels,
                False,
                "missing `Deleted-Tests: //<pkg>:<target>` footer",
            )
        for lineno, raw_val in self.commit_meta.deleted_tests_raw:
            if not raw_val:
                return (
                    labels,
                    False,
                    f"empty `Deleted-Tests:` footer on line {lineno}",
                )
            if "*" in raw_val:
                return (
                    labels,
                    False,
                    f"wildcards (`*`) are forbidden in `Deleted-Tests:` (line {lineno})",
                )
            for item in [x.strip() for x in raw_val.split(",") if x.strip()]:
                if not re.match(r"^//[\w./\-]*:[\w.\-]+$", item):
                    return (
                        labels,
                        False,
                        f"entry `{item}` on line {lineno} is not an exact `//<pkg>:<target>` label",
                    )
                labels.add(item)
        return labels, True, ""

    def _parse_renamed_tests_footer(
        self,
    ) -> tuple[dict[str, list[str]], list[Finding]]:
        """Parses `Renamed-Tests: <old> -> <new>` footers."""
        mapping: dict[str, list[str]] = {}
        errors: list[Finding] = []
        reverse_owners: dict[str, list[str]] = {}

        for lineno, raw_val in self.commit_meta.renamed_tests_raw:
            if not raw_val or "*" in raw_val:
                errors.append(
                    Finding(
                        level="error",
                        message=(
                            "🚨 [TEST PARITY ALERT \u2014 INVALID Renamed-Tests: "
                            f"FOOTER] Commit message line {lineno} has invalid "
                            f"`Renamed-Tests: {raw_val}` (wildcards `*` and "
                            "empty entries are forbidden).\n"
                            "Remediation: Write `Renamed-Tests: "
                            "//old/pkg:old_test -> //new/pkg:new_test`."
                        ),
                    )
                )
                continue
            clauses = [c.strip() for c in raw_val.split(",") if c.strip()]
            for clause in clauses:
                m = re.match(
                    r"^(//[\w./\-]*:[\w.\-]+)\s*->\s*(//[\w./\-]*:[\w.\-]+)$",
                    clause,
                )
                if not m:
                    errors.append(
                        Finding(
                            level="error",
                            message=(
                                "🚨 [TEST PARITY ALERT \u2014 INVALID Renamed-Tests:"
                                f" SYNTAX] Entry `{clause}` on commit message "
                                f"line {lineno} does not match "
                                "`//<old_pkg>:<old_name> -> "
                                "//<new_pkg>:<new_name>`.\n"
                                "Remediation: Specify exact `//pkg:target` "
                                "labels on both sides of `->`."
                            ),
                        )
                    )
                    continue
                old_lbl, new_lbl = m.group(1), m.group(2)
                mapping.setdefault(old_lbl, []).append(new_lbl)
                reverse_owners.setdefault(new_lbl, []).append(old_lbl)

        for new_lbl, old_list in sorted(reverse_owners.items()):
            unique_olds = list(dict.fromkeys(old_list))
            if len(unique_olds) > 1:
                errors.append(
                    Finding(
                        level="error",
                        message=(
                            "🚨 [TEST PARITY ALERT \u2014 NON-BIJECTIVE "
                            "Renamed-Tests: COLLAPSE] Multiple removed tests "
                            f"({', '.join(unique_olds)}) map to the same Bazel "
                            f"target `{new_lbl}` in `Renamed-Tests:`, which "
                            "collapses distinct test targets!\n"
                            "Remediation: Map each removed test target 1:1 to "
                            "its own wired Bazel test target."
                        ),
                    )
                )

        return mapping, errors

    def _gn_test_polarities_and_wrappers(
        self, gn_file: ParsedGnFile
    ) -> tuple[dict[str, set[str]], set[str], dict[str, list[str]]]:
        """Analyzes a GN file's test targets, polarities, and inner binaries.

        Returns:
          polarities_by_name: Map from GN test target name (or `<lib>_test` for
            `with_unit_tests = true`) to `{"device"}`, `{"host"}`, or both.
          inner_wrapped_only: Set of binary/component target names wrapped by a
            package in the same BUILD.gn and NOT independently wired for host.
          resolved_sources: Map from test target name to workspace-relative
            source file paths.
        """
        by_name = {t.name: t for t in gn_file.targets}
        pkg = gn_file.pkg_dir

        # Find binaries/components referenced inside device test packages.
        packaged_deps: dict[str, set[str]] = {}
        for t in gn_file.targets:
            if t.rule_type in (
                GN_DEVICE_PACKAGE_RULES | GN_DEVICE_COMPONENT_RULES
            ):
                for attr in ("deps", "test_components", "unit_tests"):
                    for dep in t.string_lists.get(attr, []):
                        parsed = _split_label(dep, pkg)
                        if parsed and parsed[0] == pkg:
                            packaged_deps.setdefault(t.name, set()).add(
                                parsed[1]
                            )

        # Find direct host/default wiring in `group("tests")` etc.
        group_host_refs: set[str] = set()
        group_default_refs: set[str] = set()
        for t in gn_file.targets:
            if t.rule_type != "group" or t.name not in TEST_GROUP_NAMES:
                continue
            grp_is_host = _condition_is_host_only(t.conditions)
            for attr in ("deps", "public_deps"):
                for dep in t.string_lists.get(attr, []):
                    parsed = _split_label(dep, pkg)
                    if not parsed or parsed[0] != pkg:
                        continue
                    t_name, tc = parsed[1], parsed[2]
                    if grp_is_host or "host_toolchain" in tc:
                        group_host_refs.add(t_name)
                    else:
                        group_default_refs.add(t_name)

        all_wrapped: set[str] = set()
        for deps_set in packaged_deps.values():
            all_wrapped.update(deps_set)

        polarities: dict[str, set[str]] = {}
        resolved_sources: dict[str, list[str]] = {}

        def _direct_sources_of(tgt: GnTarget) -> list[str]:
            raw_srcs = list(tgt.string_lists.get("sources", []))
            raw_srcs.extend(tgt.string_lists.get("tests", []))
            for s_key in ("main_source", "crate_root"):
                val = tgt.scalars.get(s_key)
                if isinstance(val, str) and val:
                    raw_srcs.append(val)
            out: list[str] = []
            for s in raw_srcs:
                if s.startswith("//"):
                    out.append(s[2:].lstrip("/"))
                else:
                    joined = f"{pkg}/{s}" if pkg else s
                    out.append(os.path.normpath(joined).replace("\\", "/"))
            return list(dict.fromkeys(out))

        for t in gn_file.targets:
            if t.rule_type in GN_EXPLICIT_TEST_RULES:
                pols: set[str] = set()
                if t.rule_type in (
                    GN_DEVICE_PACKAGE_RULES | GN_DEVICE_COMPONENT_RULES
                ):
                    pols.add("device")
                elif (
                    t.rule_type in GN_HOST_ONLY_TEST_RULES
                    or _condition_is_host_only(t.conditions)
                ):
                    pols.add("host")
                else:
                    if t.name in all_wrapped or t.name in group_default_refs:
                        pols.add("device")
                    if t.name in group_host_refs:
                        pols.add("host")
                    if not pols:
                        pols.add("device")
                polarities[t.name] = pols
                srcs = _direct_sources_of(t)
                if not srcs and t.name in packaged_deps:
                    # Collect sources transitively through local component/bin.
                    queue = list(packaged_deps[t.name])
                    seen_d: set[str] = set()
                    while queue:
                        sub_n = queue.pop(0)
                        if sub_n in seen_d:
                            continue
                        seen_d.add(sub_n)
                        if sub_n in by_name:
                            srcs.extend(_direct_sources_of(by_name[sub_n]))
                            queue.extend(packaged_deps.get(sub_n, ()))
                        elif sub_n.endswith("_test") and sub_n[:-5] in by_name:
                            srcs.extend(_direct_sources_of(by_name[sub_n[:-5]]))
                resolved_sources[t.name] = list(dict.fromkeys(srcs))

            elif t.rule_type in GN_RUST_UNITTEST_OWNER_RULES:
                has_wut = t.scalars.get("with_unit_tests") is True
                has_whut = t.scalars.get("with_host_unit_tests") is True
                if has_wut or has_whut:
                    synth = f"{t.name}_test"
                    pols = set()
                    if (
                        has_whut
                        or synth in group_host_refs
                        or _condition_is_host_only(t.conditions)
                    ):
                        pols.add("host")
                    if has_wut and (
                        synth in all_wrapped
                        or synth in group_default_refs
                        or not pols
                    ):
                        pols.add("device")
                    polarities[synth] = pols
                    srcs = _direct_sources_of(t)
                    if not srcs:
                        default_rs = (
                            "src/main.rs"
                            if t.rule_type == "rustc_binary"
                            else "src/lib.rs"
                        )
                        joined = f"{pkg}/{default_rs}" if pkg else default_rs
                        srcs = [joined]
                    resolved_sources[synth] = srcs

        inner_wrapped_only = {
            name
            for name in all_wrapped
            if name not in group_host_refs
            and (
                name in by_name
                and by_name[name].rule_type
                in (GN_BINARY_TEST_RULES | GN_DEVICE_COMPONENT_RULES)
                or (
                    name.endswith("_test")
                    and name[:-5] in by_name
                    and by_name[name[:-5]].rule_type
                    in GN_RUST_UNITTEST_OWNER_RULES
                )
            )
        }
        return polarities, inner_wrapped_only, resolved_sources

    def check_removed_tests_and_severed_edges(
        self,
        touched_gn_pkgs: Sequence[str],
        touched_bazel_pkgs: Sequence[str],
        reachable: dict[tuple[str, str], set[str]],
    ) -> list[Finding]:
        """Checks removed GN/Bazel tests (Class 1 vs Class 2/3/4) & `:tests` edges."""
        findings: list[Finding] = []
        renamed_map, rename_errors = self._parse_renamed_tests_footer()
        findings.extend(rename_errors)
        used_renames: set[str] = set()
        skipped_dual_set, _, _ = self._parse_skipped_dual_tests()

        removed_records: list[RemovedTestRecord] = []

        all_pkgs = sorted(set(touched_gn_pkgs) | set(touched_bazel_pkgs))
        for pkg in all_pkgs:
            before_gn = self.get_before_gn(pkg)
            after_gn = self.get_after_gn(pkg)
            after_bazel = self.get_after_bazel(pkg)
            gn_rel = f"{pkg}/BUILD.gn" if pkg else "BUILD.gn"
            report_file = (
                gn_rel
                if after_gn is not None
                else (
                    f"{pkg}/BUILD.bazel"
                    if after_bazel is not None and pkg
                    else ("BUILD.bazel" if after_bazel is not None else gn_rel)
                )
            )

            # 1. Check severed `:tests` group edges in BUILD.gn.
            if before_gn is not None:
                self._check_severed_group_edges(
                    pkg,
                    gn_rel,
                    report_file,
                    before_gn,
                    after_gn,
                    reachable,
                    findings,
                )

            # 2. Compare before_gn test targets against after_gn + after_bazel.
            if before_gn is not None:
                (
                    before_pols,
                    before_inner,
                    before_srcs,
                ) = self._gn_test_polarities_and_wrappers(before_gn)
                after_pols: dict[str, set[str]] = {}
                if after_gn is not None:
                    after_pols, _, _ = self._gn_test_polarities_and_wrappers(
                        after_gn
                    )

                is_dual_after = bool(after_gn and after_gn.has_sentinel)
                before_by_name: dict[str, GnTarget] = {}
                inline_rust_synth: set[str] = set()
                for t in before_gn.targets:
                    if t.rule_type in GN_EXPLICIT_TEST_RULES:
                        before_by_name[t.name] = t
                    elif t.rule_type in GN_RUST_UNITTEST_OWNER_RULES and (
                        t.scalars.get("with_unit_tests") is True
                        or t.scalars.get("with_host_unit_tests") is True
                    ):
                        synth = f"{t.name}_test"
                        before_by_name[synth] = t
                        inline_rust_synth.add(synth)

                for t_name, req_pols in sorted(before_pols.items()):
                    if t_name in before_inner:
                        # Inner binary/component wrapped by a GN test package in
                        # the same BUILD.gn; parity is tracked on the outer
                        # package target so standard GN binary+package -> single
                        # Bazel package consolidation never false-positives.
                        continue

                    tgt_obj = before_by_name[t_name]
                    old_lbl = _format_label(pkg, t_name)
                    still_in_gn = after_pols.get(t_name, set())
                    missing_from_gn = set(req_pols) - still_in_gn
                    if not missing_from_gn:
                        continue

                    # Check if satisfied in Bazel (directly, via same-package
                    # wrapper, via Class 2 gn2gone, or via Class 3 Renamed-Tests).
                    unsatisfied_pols: set[str] = set()
                    for pol in sorted(missing_from_gn):
                        if self._is_polarity_satisfied_in_bazel(
                            pkg,
                            t_name,
                            pol,
                            old_lbl,
                            renamed_map,
                            used_renames,
                            reachable,
                        ):
                            # In Case 2 dual-build (sentinel added/present),
                            # dropping the GN copy while keeping sentinel is
                            # ONLY valid if attested via Skipped-Dual-Tests
                            # (checked in Rule 3) or if the rule is not
                            # dual-built; if BUILD.bazel lacks `# @bazel2gn:skip`
                            # and GN still dropped it, flag if sentinel was not
                            # retired.
                            if (
                                is_dual_after
                                and not before_gn.has_sentinel
                                and pol == "device"
                                and old_lbl not in skipped_dual_set
                                and not self._has_skip_in_bazel(pkg, t_name)
                            ):
                                # Check if a `-gn` package with another target
                                # name exists in after_gn.
                                if not any(
                                    gt.rule_type in GN_DEVICE_PACKAGE_RULES
                                    for gt in (
                                        after_gn.targets if after_gn else []
                                    )
                                ):
                                    unsatisfied_pols.add(pol)
                            continue
                        unsatisfied_pols.add(pol)

                    if unsatisfied_pols:
                        removed_records.append(
                            RemovedTestRecord(
                                label=old_lbl,
                                pkg_dir=pkg,
                                name=t_name,
                                rule_type=tgt_obj.rule_type,
                                polarities=frozenset(unsatisfied_pols),
                                source_files=tuple(before_srcs.get(t_name, [])),
                                is_inline_rust_unit_test=(
                                    t_name in inline_rust_synth
                                ),
                                build_filepath=report_file,
                                lineno=1,
                            )
                        )

            # 3. Also check test targets removed directly from BUILD.bazel!
            before_bazel = self.get_before_bazel(pkg)
            if before_bazel is not None:
                after_b_names = {
                    t.name for t in (after_bazel.targets if after_bazel else [])
                }
                b_dev_wrap, b_host_wrap = self._intra_package_wrapped_binaries(
                    before_bazel
                )
                b_rel = f"{pkg}/BUILD.bazel" if pkg else "BUILD.bazel"
                for bt in before_bazel.targets:
                    if bt.rule_type not in BAZEL_ALL_EXPLICIT_TEST_RULES:
                        continue
                    if bt.name in after_b_names:
                        continue
                    if bt.rule_type in BAZEL_AMBI_BINARY_TEST_RULES and (
                        bt.name in b_dev_wrap or bt.name in b_host_wrap
                    ):
                        continue
                    old_lbl = _format_label(pkg, bt.name)
                    pol = "host" if _is_bazel_host_target(bt) else "device"
                    if self._is_polarity_satisfied_in_bazel(
                        pkg,
                        bt.name,
                        pol,
                        old_lbl,
                        renamed_map,
                        used_renames,
                        reachable,
                    ):
                        continue
                    raw_srcs = bt.string_lists.get("srcs", [])
                    norm_srcs = tuple(
                        (
                            s[2:].lstrip("/")
                            if s.startswith("//")
                            else os.path.normpath(
                                f"{pkg}/{s}" if pkg else s
                            ).replace("\\", "/")
                        )
                        for s in raw_srcs
                        if not s.startswith(":")
                    )
                    removed_records.append(
                        RemovedTestRecord(
                            label=old_lbl,
                            pkg_dir=pkg,
                            name=bt.name,
                            rule_type=bt.rule_type,
                            polarities=frozenset({pol}),
                            source_files=norm_srcs,
                            is_inline_rust_unit_test=False,
                            build_filepath=b_rel,
                            lineno=1,
                        )
                    )

        # Validate that every `Renamed-Tests:` entry actually matched a removed
        # test and a wired Bazel test!
        for old_lbl, new_lbls in sorted(renamed_map.items()):
            for new_lbl in new_lbls:
                parsed_new = _split_label(new_lbl, "")
                new_reach = (
                    reachable.get((parsed_new[0], parsed_new[1]), set())
                    if parsed_new
                    else set()
                )
                if not new_reach:
                    findings.append(
                        Finding(
                            level="error",
                            message=(
                                "🚨 [TEST PARITY ALERT \u2014 UNWIRED OR MISSING "
                                "Renamed-Tests: TARGET] `Renamed-Tests: "
                                f"{old_lbl} -> {new_lbl}` names `{new_lbl}`, "
                                "which does not exist or is NOT wired into any "
                                "`bazel_test_suite(...)`!\n"
                                f"Remediation: Define `{new_lbl}` in "
                                "`BUILD.bazel` and wire it into "
                                "`bazel_test_suite(...)`."
                            ),
                        )
                    )
                elif old_lbl not in used_renames:
                    findings.append(
                        Finding(
                            level="error",
                            message=(
                                "🚨 [TEST PARITY ALERT \u2014 STALE OR MISMATCHED "
                                "Renamed-Tests: ENTRY] `Renamed-Tests: "
                                f"{old_lbl} -> {new_lbl}` does not match any "
                                "removed test target with matching host/device "
                                "polarity in this CL.\n"
                                "Remediation: Remove the stale `Renamed-Tests:`"
                                " footer or fix the old target label / OS "
                                "bucket polarity."
                            ),
                        )
                    )

        # Partition `removed_records` into Class 1 (sources still exist ->
        # silent orphaning) vs Class 4 (sources also deleted -> test deletion).
        class4_records: list[RemovedTestRecord] = []
        for rec in removed_records:
            still_exists, existing_paths = self._test_sources_still_exist(rec)
            if still_exists:
                pol_desc = "/".join(sorted(rec.polarities))
                srcs_desc = (
                    ", ".join(existing_paths[:4])
                    if existing_paths
                    else f"test sources under `{rec.pkg_dir or '.'}/`"
                )
                findings.append(
                    Finding(
                        level="error",
                        filepath=rec.build_filepath,
                        line=rec.lineno,
                        message=(
                            "🚨 [TEST PARITY ALERT \u2014 CLASS 1 SILENT ORPHANING "
                            "(REMOVED TEST TARGET WITH EXISTING SOURCES)] "
                            f"Test target `{rec.label}` (`{rec.rule_type}`, "
                            f"{pol_desc} leg) was removed from "
                            f"`{rec.build_filepath}`, but its test source "
                            f"file(s) STILL EXIST at HEAD ({srcs_desc}) and "
                            "are NOT wired into any matching Bazel test target!"
                            "\nRemediation: Migrate `{rec.label}` to "
                            "`BUILD.bazel` and wire its "
                            f"{pol_desc} leg into `bazel_test_suite(...)` "
                            "(if renamed, declare `Renamed-Tests: "
                            f"{rec.label} -> //<pkg>:<new_target>`)."
                        ).replace("{rec.label}", rec.label),
                    )
                )
            else:
                class4_records.append(rec)

        findings.extend(self._emit_class4_findings(class4_records))
        return findings

    def _has_skip_in_bazel(self, pkg: str, name: str) -> bool:
        b_file = self.get_after_bazel(pkg)
        if not b_file:
            return False
        return any(
            t.skip_lineno > 0
            and (t.name == name or t.rule_type in BAZEL_DEVICE_PACKAGE_RULES)
            for t in b_file.targets
        )

    def _is_polarity_satisfied_in_bazel(
        self,
        pkg: str,
        t_name: str,
        pol: str,
        old_lbl: str,
        renamed_map: dict[str, list[str]],
        used_renames: set[str],
        reachable: dict[tuple[str, str], set[str]],
    ) -> bool:
        """Checks whether `old_lbl` (`pol` leg) is wired in Bazel."""
        # 1. Exact same name in same package (or normalized `-`/`_` package
        # wrapper, or `<name>_host_test` host wrapper in same package).
        candidate_names = [t_name]
        if pol == "host":
            candidate_names.extend([f"{t_name}_host_test", f"{t_name}_host"])
            if t_name.endswith("_bin_test"):
                stem = t_name[: -len("_bin_test")]
                candidate_names.extend([f"{stem}_test", f"{stem}_host_test"])
        for cand in candidate_names:
            if pol in reachable.get((pkg, cand), set()):
                return True

        # Check if `b_file` has a single wired device package wrapping `:t_name`
        # (e.g. `rustc_library(with_unit_tests=true)` -> `fx_unittest_package`
        # wrapping `:lib_test`).
        b_file = self.get_after_bazel(pkg)
        if b_file and pol == "device":
            for bt in b_file.targets:
                if bt.rule_type in BAZEL_DEVICE_PACKAGE_RULES and (
                    "device" in reachable.get((pkg, bt.name), set())
                ):
                    for u_ref in bt.string_lists.get("unit_tests", []):
                        parsed = _split_label(u_ref, pkg)
                        if parsed and parsed[0] == pkg and parsed[1] == t_name:
                            return True

        # 2. Explicit `Renamed-Tests: <old_lbl> -> <new_lbl>` bijection.
        for new_lbl in renamed_map.get(old_lbl, []):
            parsed_new = _split_label(new_lbl, pkg)
            if not parsed_new:
                continue
            if pol in reachable.get((parsed_new[0], parsed_new[1]), set()):
                used_renames.add(old_lbl)
                return True

        return False

    def _check_severed_group_edges(
        self,
        pkg: str,
        gn_rel: str,
        report_file: str,
        before_gn: ParsedGnFile,
        after_gn: ParsedGnFile | None,
        reachable: dict[tuple[str, str], set[str]],
        findings: list[Finding],
    ) -> None:
        """Detects removed `sub:tests` group deps that orphan child tests."""
        before_child_pkgs: dict[str, str] = {}
        for grp in before_gn.targets:
            if grp.rule_type != "group" or grp.name not in TEST_GROUP_NAMES:
                continue
            for attr in ("deps", "public_deps"):
                for dep in grp.string_lists.get(attr, []):
                    parsed = _split_label(dep, pkg)
                    if (
                        parsed
                        and parsed[0] != pkg
                        and parsed[1] in TEST_GROUP_NAMES
                    ):
                        before_child_pkgs[parsed[0]] = dep

        after_child_pkgs: set[str] = set()
        if after_gn is not None:
            for grp in after_gn.targets:
                if grp.rule_type != "group" or grp.name not in TEST_GROUP_NAMES:
                    continue
                for attr in ("deps", "public_deps"):
                    for dep in grp.string_lists.get(attr, []):
                        parsed = _split_label(dep, pkg)
                        if (
                            parsed
                            and parsed[0] != pkg
                            and parsed[1] in TEST_GROUP_NAMES
                        ):
                            after_child_pkgs.add(parsed[0])

        for child_pkg, raw_dep in sorted(before_child_pkgs.items()):
            if child_pkg in after_child_pkgs:
                continue
            # Was child_pkg re-homed to an ancestor GN group?
            rehomed_in_anc = False
            for anc in self.ancestor_chain(pkg)[1:]:
                anc_gn = self.get_after_gn(anc)
                if not anc_gn:
                    continue
                for grp in anc_gn.targets:
                    if grp.rule_type != "group":
                        continue
                    for dep in grp.string_lists.get(
                        "deps", []
                    ) + grp.string_lists.get("public_deps", []):
                        parsed = _split_label(dep, anc)
                        if (
                            parsed
                            and parsed[0] == child_pkg
                            and parsed[1] in TEST_GROUP_NAMES
                        ):
                            rehomed_in_anc = True
                            break
            if rehomed_in_anc:
                continue

            # Check whether `child_pkg` still has active GN tests or unwired
            # Bazel tests / existing test sources!
            child_gn = self.get_after_gn(child_pkg)
            child_bazel = self.get_after_bazel(child_pkg)
            still_has_gn_tests = False
            if child_gn is not None:
                c_pols, _, _ = self._gn_test_polarities_and_wrappers(child_gn)
                has_suite = any(
                    t.rule_type == "bazel_test_suite" for t in child_gn.targets
                )
                has_sub_tests_grp = any(
                    t.rule_type == "group" and t.name in TEST_GROUP_NAMES
                    for t in child_gn.targets
                )
                if c_pols or has_suite or has_sub_tests_grp:
                    still_has_gn_tests = True

            unwired_child_bazel = False
            if child_bazel is not None:
                c_dev_w, c_host_w = self._intra_package_wrapped_binaries(
                    child_bazel
                )
                for bt in child_bazel.targets:
                    if bt.rule_type in BAZEL_ALL_EXPLICIT_TEST_RULES:
                        if bt.rule_type in BAZEL_AMBI_BINARY_TEST_RULES and (
                            bt.name in c_dev_w or bt.name in c_host_w
                        ):
                            continue
                        if not reachable.get((child_pkg, bt.name)):
                            unwired_child_bazel = True
                            break

            if still_has_gn_tests or unwired_child_bazel:
                findings.append(
                    Finding(
                        level="error",
                        filepath=report_file,
                        line=1,
                        message=(
                            "🚨 [TEST PARITY ALERT \u2014 CLASS 1 SILENT ORPHANING "
                            "(SEVERED :tests GROUP EDGE)] Dependency "
                            f'`"{raw_dep}"` was removed from `group("tests")` '
                            f"in `{gn_rel}`, severing `{child_pkg}` from the "
                            "GN test graph while tests still exist in "
                            f"`//{child_pkg}`!\n"
                            f'Remediation: Restore `"{raw_dep}"` in '
                            f'`group("tests")` of `{gn_rel}`, or if '
                            f"`//{child_pkg}` migrated fully to Bazel, delete "
                            f"`{child_pkg}/BUILD.gn` and wire all `//{child_pkg}`"
                            " Bazel tests into an ancestor `bazel_test_suite`."
                        ),
                    )
                )

    def _test_sources_still_exist(
        self, rec: RemovedTestRecord
    ) -> tuple[bool, list[str]]:
        """Returns `(still_exists, existing_paths)` for a removed test target."""
        if rec.is_inline_rust_unit_test:
            # Inline Rust unit tests live inside `.rs` library/binary sources.
            # They still exist iff any source `.rs` file exists on disk AND
            # contains `#[cfg(test)]` / `#[test]` / `#[fuchsia::test]`.
            existing_with_tests: list[str] = []
            for src in rec.source_files:
                after_txt = self.read_after_text(src)
                if after_txt is not None and RUST_TEST_ATTR_RE.search(
                    after_txt
                ):
                    existing_with_tests.append(src)
            return bool(existing_with_tests), existing_with_tests

        if rec.source_files:
            existing = [
                src for src in rec.source_files if (self.root / src).is_file()
            ]
            return bool(existing), existing

        # Fallback when `sources` was populated via a `.gni` variable: inspect
        # `*test*` source files in `rec.pkg_dir` on disk vs deleted in git diff.
        pkg_abs = self.root / rec.pkg_dir if rec.pkg_dir else self.root
        existing_test_files: list[str] = []
        if pkg_abs.is_dir():
            for child in sorted(pkg_abs.rglob("*")):
                if not child.is_file():
                    continue
                if child.name in ("BUILD.gn", "BUILD.bazel", "BUILD"):
                    continue
                if "test" in child.name.lower() and child.suffix in (
                    ".cc",
                    ".cpp",
                    ".cxx",
                    ".rs",
                    ".py",
                    ".go",
                ):
                    existing_test_files.append(
                        child.relative_to(self.root).as_posix()
                    )
        if existing_test_files:
            return True, existing_test_files

        prefix = f"{rec.pkg_dir}/" if rec.pkg_dir else ""
        deleted_in_pkg = [
            p
            for p, st in self.changed_status.items()
            if st.startswith("D") and p.startswith(prefix)
        ]
        if deleted_in_pkg:
            return False, []
        return True, []

    def _emit_class4_findings(
        self, records: Sequence[RemovedTestRecord]
    ) -> list[Finding]:
        """Emits Class 4 Deleted-Test findings (migration vs feature cleanup)."""
        findings: list[Finding] = []
        is_mig = self.is_migration_cl()
        (
            declared_deleted,
            del_valid,
            del_err_reason,
        ) = self._parse_deleted_tests_footer()

        # If `Deleted-Tests:` was explicitly written even when zero tests were
        # deleted (or with invalid syntax), validate it strictly!
        if self.commit_meta.deleted_tests_raw and not records:
            findings.append(
                Finding(
                    level="error",
                    message=(
                        "🚨 [TEST PARITY ALERT \u2014 INVALID Deleted-Tests: FOOTER]"
                        " Commit message declares `Deleted-Tests:`, but no "
                        f"co-deleted test targets matched ({del_err_reason or '0 deleted tests'})."
                        "\nRemediation: Remove the bogus `Deleted-Tests:` "
                        "footer (note: orphaned tests whose source files still "
                        "exist are Class 1 errors and CANNOT be waived with "
                        "`Deleted-Tests:`)."
                    ),
                )
            )
            return findings

        if not records:
            return findings

        deleted_labels = [r.label for r in records]
        primary_file = records[0].build_filepath
        srcs_summary = ", ".join(
            s for r in records for s in (r.source_files or (r.build_filepath,))
        )

        if is_mig or self.commit_meta.deleted_tests_raw:
            missing_attestation = sorted(set(deleted_labels) - declared_deleted)
            extra_attestation = sorted(declared_deleted - set(deleted_labels))
            has_bug = bool(self.commit_meta.bug_ids)

            problems: list[str] = []
            if not del_valid:
                problems.append(del_err_reason)
            if missing_attestation:
                problems.append(
                    "undeclared in `Deleted-Tests:`: "
                    + ", ".join(missing_attestation)
                )
            if extra_attestation:
                problems.append(
                    "unmatched `Deleted-Tests:` entries: "
                    + ", ".join(extra_attestation)
                )
            if not has_bug:
                problems.append("missing numeric `Bug: <id>` footer")
            if self.commit_meta.has_tag_planter:
                problems.append(
                    "automated `TAG: planter` migration agents are FORBIDDEN "
                    "from deleting tests or emitting `Deleted-Tests:`"
                )

            if problems:
                footer_lines = "\n".join(
                    f"  Deleted-Tests: {lbl}" for lbl in deleted_labels
                )
                findings.append(
                    Finding(
                        level="error",
                        filepath=primary_file,
                        line=1,
                        message=(
                            "🚨 [TEST PARITY ALERT \u2014 UNATTESTED MIGRATION TEST "
                            "DELETION (CLASS 4)] GN-to-Bazel migration / "
                            "dual-build CL deletes test target(s) "
                            f"{', '.join(deleted_labels)} and source(s) "
                            f"({srcs_summary}) without valid attestation "
                            f"({'; '.join(problems)})!\n"
                            "Remediation: Restore and migrate the deleted "
                            "test(s) to Bazel. If a human engineer is "
                            "intentionally retiring dead test code, remove any "
                            "agent `TAG: planter` footer, add:\n"
                            f"{footer_lines}\n"
                            "  Bug: <fxbug_id>\n"
                            "and obtain a human `Test-Drop-Verified +1` vote."
                        ),
                    )
                )
            else:
                findings.append(
                    Finding(
                        level="error",
                        filepath=primary_file,
                        line=1,
                        properties={
                            "fail_if_missing_label": "Test-Drop-Verified"
                        },
                        message=(
                            "🚨 [TEST PARITY ALERT \u2014 ATTESTED ESCAPE-HATCH "
                            "TEST DELETION (REQUIRES Test-Drop-Verified +1)] "
                            "This CL permanently deletes test target(s): "
                            f"{', '.join(deleted_labels)} (sources: "
                            f"{srcs_summary}; Bug: "
                            f"{self.commit_meta.bug_ids[0]}).\n"
                            "Remediation: A human committer / reviewer must "
                            "audit every deleted test listed above and vote "
                            "`Test-Drop-Verified +1` on Gerrit to allow "
                            "submission."
                        ),
                    )
                )
        else:
            for rec in records:
                rec_srcs = (
                    ", ".join(rec.source_files)
                    if rec.source_files
                    else rec.build_filepath
                )
                findings.append(
                    Finding(
                        level="warning",
                        filepath=rec.build_filepath,
                        line=rec.lineno,
                        message=(
                            "⚠️ [TEST DELETED \u2014 FEATURE/CODE CLEANUP] "
                            f"`{rec.label}` (`{rec.rule_type}`, sources: "
                            f"{rec_srcs}) and its source files were deleted in "
                            "this CL. Reviewers: confirm this test coverage "
                            "retirement is intentional."
                        ),
                    )
                )

        return findings

    def check_build_only_footer(self) -> list[Finding]:
        """Enforces Rule 5 (FM1: narrowed `Build-Only: true` restriction)."""
        if not self.commit_meta.has_build_only_true:
            return []

        violations: list[str] = []
        primary_file = ""

        if self.commit_meta.deleted_tests_raw:
            violations.append(
                "commit message contains `Deleted-Tests:` alongside "
                "`Build-Only: true`"
            )
        if self.commit_meta.skipped_dual_tests_raw:
            violations.append(
                "commit message contains `Skipped-Dual-Tests:` alongside "
                "`Build-Only: true`"
            )

        is_bazel_mig_subj = "[bazel_migration]" in self.commit_meta.raw_message

        for rel_path in sorted(self.changed_status):
            base = os.path.basename(rel_path)
            pkg = _pkg_dir_of(rel_path)
            if base == "BUILD.gn":
                before_sig = self._gn_test_footprint(self.get_before_gn(pkg))
                after_sig = self._gn_test_footprint(self.get_after_gn(pkg))
                if before_sig != after_sig:
                    diff_keys = sorted(
                        set(before_sig.keys()) ^ set(after_sig.keys())
                        | {
                            k
                            for k in (
                                set(before_sig.keys()) & set(after_sig.keys())
                            )
                            if before_sig[k] != after_sig[k]
                        }
                    )
                    if not primary_file:
                        primary_file = rel_path
                    violations.append(
                        f"`{rel_path}` adds/removes/modifies test or `:tests` "
                        f"target(s): {', '.join(diff_keys[:4])}"
                    )
            elif base in ("BUILD.bazel", "BUILD"):
                before_sig = self._bazel_test_footprint(
                    self.get_before_bazel(pkg)
                )
                after_sig = self._bazel_test_footprint(
                    self.get_after_bazel(pkg)
                )
                if before_sig != after_sig:
                    diff_keys = sorted(
                        set(before_sig.keys()) ^ set(after_sig.keys())
                        | {
                            k
                            for k in (
                                set(before_sig.keys()) & set(after_sig.keys())
                            )
                            if before_sig[k] != after_sig[k]
                        }
                    )
                    if not primary_file:
                        primary_file = rel_path
                    violations.append(
                        f"`{rel_path}` adds/removes/modifies test or "
                        f"`test_suite` target(s): {', '.join(diff_keys[:4])}"
                    )
            elif is_bazel_mig_subj:
                if not primary_file:
                    primary_file = rel_path
                violations.append(
                    f"`[bazel_migration]` CL modifies non-BUILD file `{rel_path}`"
                )

        if not violations:
            return []

        return [
            Finding(
                level="error",
                filepath=primary_file,
                line=1 if primary_file else 0,
                message=(
                    "🚨 [TEST PARITY ALERT \u2014 ILLEGAL Build-Only: true ON "
                    "TEST-TOUCHING CL (FM1)] `Build-Only: true` bypasses "
                    "post-build test execution on CQ and is STRICTLY FORBIDDEN "
                    "whenever any test target, `with_unit_tests`, "
                    "`bazel_test_suite`, `test_suite`, or `:tests` group "
                    f"dependency is touched ({'; '.join(violations)})!\n"
                    "Remediation: Remove `Build-Only: true` from the commit "
                    "message so CQ executes full test suites and verifies "
                    "`tests.json` runtime parity."
                ),
            )
        ]

    def _gn_test_footprint(
        self, gn_file: ParsedGnFile | None
    ) -> dict[str, str]:
        """Extracts canonical signatures of test & test-group targets in GN."""
        if gn_file is None:
            return {}
        out: dict[str, str] = {}
        for t in gn_file.targets:
            if (
                t.rule_type in GN_EXPLICIT_TEST_RULES
                or t.rule_type
                in ("bazel_test_suite", "bazel_fuchsia_test_package")
                or (t.rule_type == "group" and t.name in TEST_GROUP_NAMES)
            ):
                out[f"{t.rule_type}:{t.name}"] = t.canonical_sig
            elif t.rule_type in GN_RUST_UNITTEST_OWNER_RULES:
                wut = bool(t.scalars.get("with_unit_tests"))
                whut = bool(t.scalars.get("with_host_unit_tests"))
                if wut or whut:
                    out[f"{t.rule_type}:{t.name}:unit_tests"] = json.dumps(
                        {
                            "wut": wut,
                            "whut": whut,
                            "test_deps": sorted(
                                t.string_lists.get("test_deps", [])
                            ),
                        },
                        sort_keys=True,
                    )
        return out

    def _bazel_test_footprint(
        self, b_file: ParsedBazelFile | None
    ) -> dict[str, str]:
        """Extracts canonical signatures of test & test_suite targets in Bazel."""
        if b_file is None:
            return {}
        out: dict[str, str] = {}
        for t in b_file.targets:
            if (
                t.rule_type in BAZEL_ALL_EXPLICIT_TEST_RULES
                or t.rule_type == "test_suite"
            ):
                out[f"{t.rule_type}:{t.name}"] = t.canonical_sig
            elif t.rule_type in BAZEL_RUST_UNITTEST_OWNER_RULES:
                pols = _bazel_unit_test_polarities(t)
                if pols:
                    out[f"{t.rule_type}:{t.name}:unit_tests"] = json.dumps(
                        {
                            "pols": sorted(pols),
                            "test_deps": sorted(
                                t.string_lists.get("test_deps", [])
                            ),
                        },
                        sort_keys=True,
                    )
        return out


def _detect_git_context(
    root: pathlib.Path,
    git_bin: str,
    explicit_base_ref: str | None,
    explicit_commit_msg_file: str | None,
    passed_files: Sequence[str],
) -> tuple[str | None, dict[str, str], str]:
    """Determines `(base_ref, changed_status, commit_message)` from git."""
    norm_passed = [
        _normalize_rel_path(p, root)
        for p in passed_files
        if _normalize_rel_path(p, root) and p != "/COMMIT_MSG"
    ]

    git_check = _run_git(git_bin, root, ["rev-parse", "--git-dir"])
    if git_check.returncode != 0:
        status_map = {
            p: ("A" if (root / p).is_file() else "D") for p in norm_passed
        }
        msg = ""
        if explicit_commit_msg_file:
            msg = pathlib.Path(explicit_commit_msg_file).read_text(
                encoding="utf-8", errors="replace"
            )
        return None, status_map, msg

    # Check whether working tree has uncommitted tracked changes or untracked
    # BUILD/test files passed on CLI.
    porcelain = _run_git(
        git_bin, root, ["status", "--porcelain", "--untracked-files=no"]
    )
    is_dirty = bool(porcelain.stdout.strip())
    if not is_dirty and norm_passed:
        for p in norm_passed:
            if (root / p).is_file():
                ls_res = _run_git(
                    git_bin, root, ["ls-files", "--error-unmatch", "--", p]
                )
                if ls_res.returncode != 0:
                    is_dirty = True
                    break

    if explicit_base_ref is not None:
        base_ref: str | None = explicit_base_ref or None
    elif is_dirty:
        has_head = _run_git(git_bin, root, ["rev-parse", "--verify", "HEAD"])
        base_ref = "HEAD" if has_head.returncode == 0 else None
    else:
        has_parent = _run_git(git_bin, root, ["rev-parse", "--verify", "HEAD^"])
        base_ref = "HEAD^" if has_parent.returncode == 0 else None

    changed_status: dict[str, str] = {}
    if base_ref:
        diff_args = (
            ["diff", "--name-status", base_ref]
            if is_dirty or explicit_base_ref is not None
            else ["diff", "--name-status", base_ref, "HEAD"]
        )
        diff_proc = _run_git(git_bin, root, diff_args)
        if diff_proc.returncode == 0:
            for line in diff_proc.stdout.splitlines():
                parts = line.split("\t")
                if len(parts) >= 2:
                    st = parts[0].strip()
                    if st.startswith("R") and len(parts) >= 3:
                        changed_status[parts[1].strip()] = "D"
                        changed_status[parts[2].strip()] = "A"
                    else:
                        changed_status[parts[1].strip()] = st

    if is_dirty:
        untracked_proc = _run_git(
            git_bin, root, ["ls-files", "--others", "--exclude-standard"]
        )
        if untracked_proc.returncode == 0:
            for u_path in untracked_proc.stdout.splitlines():
                u_clean = u_path.strip()
                if u_clean:
                    changed_status.setdefault(u_clean, "A")

    for p in norm_passed:
        if p not in changed_status:
            changed_status[p] = "M" if (root / p).is_file() else "D"

    commit_msg = ""
    if explicit_commit_msg_file:
        commit_msg = pathlib.Path(explicit_commit_msg_file).read_text(
            encoding="utf-8", errors="replace"
        )
    elif not is_dirty:
        log_proc = _run_git(git_bin, root, ["log", "-1", "--format=%B", "HEAD"])
        if log_proc.returncode == 0:
            commit_msg = log_proc.stdout

    return base_ref, changed_status, commit_msg


def analyze_repository(
    root: pathlib.Path,
    git_bin: str = "",
    base_ref: str | None = None,
    commit_msg_file: str | None = None,
    passed_files: Sequence[str] = (),
) -> list[Finding]:
    """Runs all Bazel test parity checks on the repository changes."""
    resolved_git = _resolve_git_bin(git_bin)
    eff_base, raw_changed, commit_msg = _detect_git_context(
        root, resolved_git, base_ref, commit_msg_file, passed_files
    )

    filtered_changed = {
        p: st
        for p, st in raw_changed.items()
        if not p.startswith(IGNORED_PATH_PREFIXES)
    }

    commit_meta = parse_commit_message(commit_msg)
    analyzer = WorkspaceAnalyzer(
        root=root,
        git_bin=resolved_git,
        base_ref=eff_base,
        changed_status=filtered_changed,
        commit_meta=commit_meta,
    )

    touched_gn_pkgs: set[str] = set()
    touched_bazel_pkgs: set[str] = set()
    for rel_path in filtered_changed:
        base = os.path.basename(rel_path)
        pkg = _pkg_dir_of(rel_path)
        if base == "BUILD.gn":
            touched_gn_pkgs.add(pkg)
            if analyzer.get_after_bazel(pkg) is not None:
                touched_bazel_pkgs.add(pkg)
        elif base in ("BUILD.bazel", "BUILD"):
            touched_bazel_pkgs.add(pkg)
            if analyzer.get_after_gn(pkg) is not None:
                touched_gn_pkgs.add(pkg)

    seed_pkgs = sorted(touched_gn_pkgs | touched_bazel_pkgs)
    reachable = analyzer.compute_reachable_bazel_tests(seed_pkgs)

    findings: list[Finding] = []
    findings.extend(analyzer.check_touched_gn_suites(sorted(touched_gn_pkgs)))
    findings.extend(
        analyzer.check_touched_bazel_packages(
            sorted(touched_bazel_pkgs), reachable
        )
    )
    findings.extend(
        analyzer.check_removed_tests_and_severed_edges(
            sorted(touched_gn_pkgs), sorted(touched_bazel_pkgs), reachable
        )
    )
    findings.extend(analyzer.check_build_only_footer())

    # Deduplicate identical findings deterministically.
    deduped: list[Finding] = []
    seen_keys: set[tuple[str, str, int, str]] = set()
    for f in findings:
        key = (f.level, f.filepath, f.line, f.message)
        if key not in seen_keys:
            seen_keys.add(key)
            deduped.append(f)
    return deduped


def main(argv: Sequence[str] | None = None) -> int:
    """CLI entry point for `check_bazel_test_parity.py`."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--root",
        default=".",
        help="Path to the Fuchsia git checkout root.",
    )
    parser.add_argument(
        "--git-bin",
        default="",
        help="Path to the git executable (falls back to PATH / /usr/bin/git).",
    )
    parser.add_argument(
        "--base-ref",
        default=None,
        help="Explicit git base revision to diff against.",
    )
    parser.add_argument(
        "--commit-msg-file",
        default=None,
        help="Optional file containing the commit message to validate.",
    )
    parser.add_argument(
        "--files-from",
        default=None,
        help="Optional newline-delimited file of affected relative paths.",
    )
    parser.add_argument(
        "--output",
        default=None,
        help="Optional path to write JSON findings to instead of stdout.",
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="Format findings as JSON.",
    )
    parser.add_argument(
        "files",
        nargs="*",
        help="Optional workspace-relative affected file paths.",
    )
    args = parser.parse_args(argv)

    passed_files = list(args.files)
    if args.files_from:
        extra = pathlib.Path(args.files_from).read_text(encoding="utf-8")
        passed_files.extend(
            line.strip() for line in extra.splitlines() if line.strip()
        )

    findings = analyze_repository(
        root=pathlib.Path(args.root).resolve(),
        git_bin=args.git_bin,
        base_ref=args.base_ref,
        commit_msg_file=args.commit_msg_file,
        passed_files=passed_files,
    )

    payload = [f.to_dict() for f in findings]
    if args.output:
        pathlib.Path(args.output).write_text(
            json.dumps(payload, indent=2), encoding="utf-8"
        )
    elif args.json:
        print(json.dumps(payload, indent=2))
    else:
        for f in findings:
            loc = (
                f"{f.filepath}:{f.line}: "
                if f.filepath and f.line
                else (f"{f.filepath}: " if f.filepath else "")
            )
            print(f"[{f.level.upper()}] {loc}{f.message}")

    return 1 if any(f.level == "error" for f in findings) else 0


if __name__ == "__main__":
    sys.exit(main())
