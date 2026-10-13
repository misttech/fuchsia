#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Language-family definitions and syntax scanners for direct prebuilt detection."""

from __future__ import annotations

import dataclasses
import pathlib
import re
from collections.abc import Callable, Sequence

# LINT.IfChange
_EXCLUDED_PATHS = frozenset(
    {
        "build/bazel/rules/cipd_prebuilt.bzl",
        "scripts/shac/prebuilts.star",
        "scripts/shac/tests/prebuilts_test.star",
    }
)

_EXCLUDED_DIR_PREFIXES = (
    "build/prebuilt/",
    "third_party/",
    "docs/",
    "manifests/",
    "tools/check-licenses/assets/readmes/",
)

_EXCLUDED_BASENAMES = frozenset(
    {
        "OWNERS",
        "LICENSE",
        "AUTHORS",
        "PATENTS",
        "README",
        "NOTICE",
        "COPYING",
        "CONTRIBUTORS",
        "CODEOWNERS",
    }
)
# LINT.ThenChange(//scripts/shac/prebuilts.star)

_SHELL_RE = re.compile(
    r"//prebuilts?/|"
    r"(?<!\$\{cipd_prebuilt_root\})(?<!llvm)/prebuilts?/[A-Za-z0-9_$]|"
    r"(?<![/A-Za-z0-9_])prebuilts?/[A-Za-z0-9_$]|"
    r"\$\{?PREBUILT_"
)
_CODE_RE = re.compile(
    r"""(?:["'`]|(?:\$\{?(?!cipd_prebuilt_root\b)[A-Za-z0-9_]+\}?/))(?!\$\{cipd_prebuilt_root\})(?:/?\.\.?/)*(?://:?)?prebuilts?/[A-Za-z0-9_$]|(?:\$\{?PREBUILT_)"""
)
_CIPD_PREBUILT_CALL_RE = re.compile(
    r"\bcipd_prebuilt(?:_file|_directory)?\s*\("
)
_STRING_OR_HASH_COMMENT_RE = re.compile(
    r'"(?:\\.|[^"\\])*"|\'(?:\\.|[^\'\\])*\'|(?P<comment>#.*)'
)
_STRING_OR_C_COMMENT_RE = re.compile(
    r'(?P<block_comment>/\*[\s\S]*?\*/)|(?P<line_comment>//[^\n]*)|"(?:\\.|[^"\\])*"|\'(?:\\.|[^\'\\\n])\'|`[^`]*`'
)


@dataclasses.dataclass(frozen=True)
class LanguageFamily:
    """Syntax scanning and remediation rules for a family of source/build files.

    Attributes:
        name: Short identifier for this syntax family (e.g., `"gn"`, `"c_family"`).
        matches_path: Predicate returning `True` if a repository-relative file
            path belongs to this language family.
        find_matches: Scanner returning 1-indexed `(line_num, col_num)` pairs
            for unmigrated direct prebuilt accesses in file content.
        remediation_suffix: File-type-specific remediation guidance appended to
            violation messages.
    """

    name: str
    matches_path: Callable[[str], bool]
    find_matches: Callable[[str], Sequence[tuple[int, int]]]
    remediation_suffix: str


def _is_excluded_path(path: str) -> bool:
    """Returns True if `path` is excluded from all prebuilt ratchet scanning."""
    if path in _EXCLUDED_PATHS:
        return True
    if any(path.startswith(prefix) for prefix in _EXCLUDED_DIR_PREFIXES):
        return True
    if path.startswith("testdata/") or "/testdata/" in path:
        return True
    return pathlib.PurePosixPath(path).name in _EXCLUDED_BASENAMES


def _strip_hash_comments(line: str) -> str:
    """Strips trailing `#` comments outside string literals."""
    return _STRING_OR_HASH_COMMENT_RE.sub(
        lambda match: "" if match.group("comment") else match.group(0), line
    )


def _strip_strings_and_hash_comments(line: str) -> str:
    """Strips both string literals and `#` comments to inspect structural delimiters."""
    return _STRING_OR_HASH_COMMENT_RE.sub("", line)


def _strip_c_comments(content: str) -> str:
    """Strips `//` and `/* ... */` comments outside strings while preserving line numbers."""

    def _replace(match: re.Match[str]) -> str:
        if match.group("block_comment") or match.group("line_comment"):
            return "\n" * match.group(0).count("\n")
        return match.group(0)

    return _STRING_OR_C_COMMENT_RE.sub(_replace, content)


def _find_shell_prebuilts(content: str) -> Sequence[tuple[int, int]]:
    """Finds direct prebuilt references in shell and devshell scripts."""
    return tuple(
        (line_num, found.start() + 1)
        for line_num, line in enumerate(content.splitlines(), start=1)
        if (code := _strip_hash_comments(line)).strip()
        for found in _SHELL_RE.finditer(code)
    )


def _find_block_filtered_prebuilts(
    content: str, open_delimiter: str, close_delimiter: str
) -> Sequence[tuple[int, int]]:
    """Finds direct prebuilt references while skipping `cipd_prebuilt*` blocks."""
    matches: list[tuple[int, int]] = []
    block_depth = 0
    awaiting_open_delimiter = False
    for line_num, line in enumerate(content.splitlines(), start=1):
        code = _strip_hash_comments(line)
        if not code.strip():
            continue

        unquoted = _strip_strings_and_hash_comments(line)
        if awaiting_open_delimiter:
            if open_delimiter in unquoted:
                awaiting_open_delimiter = False
                delta = unquoted.count(open_delimiter) - unquoted.count(
                    close_delimiter
                )
                block_depth = max(delta, 0)
            continue

        if block_depth == 0 and _CIPD_PREBUILT_CALL_RE.search(unquoted):
            if open_delimiter in unquoted:
                delta = unquoted.count(open_delimiter) - unquoted.count(
                    close_delimiter
                )
                block_depth = max(delta, 0)
            else:
                awaiting_open_delimiter = True
            continue

        if block_depth > 0:
            block_depth += unquoted.count(open_delimiter) - unquoted.count(
                close_delimiter
            )
            block_depth = max(block_depth, 0)
            continue

        matches.extend(
            (line_num, found.start() + 1) for found in _CODE_RE.finditer(code)
        )
    return tuple(matches)


def _find_gn_prebuilts(content: str) -> Sequence[tuple[int, int]]:
    """Finds direct prebuilt references in GN files (`*.gn`, `*.gni`)."""
    return _find_block_filtered_prebuilts(
        content, open_delimiter="{", close_delimiter="}"
    )


def _find_bazel_prebuilts(content: str) -> Sequence[tuple[int, int]]:
    """Finds direct prebuilt references in Bazel files (`*.bazel`, `*.bzl`)."""
    return _find_block_filtered_prebuilts(
        content, open_delimiter="(", close_delimiter=")"
    )


def _find_hash_comment_prebuilts(content: str) -> Sequence[tuple[int, int]]:
    """Finds direct prebuilt references in `#`-comment source files (`*.py`, `*.star`)."""
    return tuple(
        (line_num, found.start() + 1)
        for line_num, line in enumerate(content.splitlines(), start=1)
        if (code := _strip_hash_comments(line)).strip()
        for found in _CODE_RE.finditer(code)
    )


def _find_c_family_prebuilts(content: str) -> Sequence[tuple[int, int]]:
    """Finds direct prebuilt references in C-family source files (`*.cc`, `*.rs`, `*.go`)."""
    cleaned = _strip_c_comments(content)
    return tuple(
        (line_num, found.start() + 1)
        for line_num, line in enumerate(cleaned.splitlines(), start=1)
        if line.strip()
        for found in _CODE_RE.finditer(line)
    )


# LINT.IfChange
def _is_devshell_path(path: str) -> bool:
    if not (
        path.startswith("tools/devshell/")
        or path.startswith("scripts/devshell/")
    ):
        return False
    return pathlib.PurePosixPath(path).suffix in {"", ".fx", ".sh"}


def _is_shell_path(path: str) -> bool:
    posix_path = pathlib.PurePosixPath(path)
    return posix_path.suffix in {".sh", ".bash", ""}


def _is_hash_comment_path(path: str) -> bool:
    posix_path = pathlib.PurePosixPath(path)
    return (
        posix_path.suffix in {".py", ".star", ".cmake"}
        or posix_path.name == "CMakeLists.txt"
    )


def _is_c_family_path(path: str) -> bool:
    return pathlib.PurePosixPath(path).suffix in {
        ".c",
        ".cc",
        ".cpp",
        ".h",
        ".hpp",
        ".rs",
        ".go",
        ".dart",
    }


LANGUAGE_FAMILIES: Sequence[LanguageFamily] = (
    LanguageFamily(
        name="gn",
        matches_path=lambda path: path.endswith(".gn") or path.endswith(".gni"),
        find_matches=_find_gn_prebuilts,
        remediation_suffix=(
            "Use `cipd_prebuilt()` from //build/prebuilt/cipd_prebuilt.gni instead."
        ),
    ),
    LanguageFamily(
        name="bazel",
        matches_path=lambda path: (
            path.endswith(".bazel") or path.endswith(".bzl")
        ),
        find_matches=_find_bazel_prebuilts,
        remediation_suffix=(
            "Use `cipd_prebuilt_file()` or `cipd_prebuilt_directory()` "
            "from //build/bazel/rules/cipd_prebuilt.bzl instead."
        ),
    ),
    LanguageFamily(
        name="devshell",
        matches_path=_is_devshell_path,
        find_matches=_find_shell_prebuilts,
        remediation_suffix=(
            "Declare `#### PREBUILTS=` or `#### EXECUTABLE=` metadata headers instead."
        ),
    ),
    LanguageFamily(
        name="shell",
        matches_path=_is_shell_path,
        find_matches=_find_shell_prebuilts,
        remediation_suffix=(
            "Pass prebuilt paths via build/tool arguments or declare `#### PREBUILTS=` "
            "metadata headers in devshell scripts instead."
        ),
    ),
    LanguageFamily(
        name="hash_comment",
        matches_path=_is_hash_comment_path,
        find_matches=_find_hash_comment_prebuilts,
        remediation_suffix=(
            "Declare the dependency via `cipd_prebuilt*()` in GN/Bazel and pass prebuilt "
            "paths via build/tool arguments instead."
        ),
    ),
    LanguageFamily(
        name="c_family",
        matches_path=_is_c_family_path,
        find_matches=_find_c_family_prebuilts,
        remediation_suffix=(
            "Declare the dependency via `cipd_prebuilt*()` in GN/Bazel and pass staged "
            "paths via `include_dirs`, `rustenv`, or CLI flags instead."
        ),
    ),
)
# LINT.ThenChange(//scripts/shac/prebuilts.star)


def match_language_family(path: str) -> LanguageFamily | None:
    """Returns the matching `LanguageFamily` for `path`, or `None` if unchecked."""
    if _is_excluded_path(path):
        return None
    for family in LANGUAGE_FAMILIES:
        if family.matches_path(path):
            return family
    return None
