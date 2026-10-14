# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Skill Linter tool for parsing and formatting SKILL.md files.
"""

import argparse
import fnmatch
import json
import logging
import os
import re
import sys
import textwrap
from dataclasses import dataclass, field
from typing import Any, TypedDict

import yaml

# Constants for skill validation and formatting.
MAX_NAME_LENGTH = 64
MAX_DESCRIPTION_LENGTH = 1024
LINE_LENGTH = 80


@dataclass
class BlockedWordRule:
    """Represents a rule for restricted or blocked terminology."""

    term: str
    pattern: re.Pattern[str]
    raw_pattern: str | None = None
    reason: str = "Prohibited internal or unannounced term."
    suggestion: str | None = None
    scope: str = "public_only"  # "public_only" or "all"
    severity: str = "error"  # "error" or "warning"
    case_sensitive: bool = False
    check_code_blocks: bool = True
    allowed_paths: list[str] = field(default_factory=list)
    exemptions: list[str] = field(default_factory=list)


DEFAULT_GOOGLE3_HEAD_GCL_PATH = "/google/src/files/head/depot/google3/devtools/gerritcodereview/banned_words/fuchsia/common.gcl"

_LINK_REFERENCE_RE = re.compile(r"^\s{0,3}\[[^\]]+\]:\s*")

DEFAULT_POLICY_REL_PATHS = [
    os.path.join("vendor", "google", ".agents", "blocked_words.json"),
    os.path.join("vendor", "google", ".agents", "blocked_words.yaml"),
    os.path.join(
        "vendor", "google", "scripts", "skill_linter", "blocked_words.json"
    ),
    os.path.join(
        "vendor", "google", "scripts", "skill_linter", "blocked_words.yaml"
    ),
]


def find_fuchsia_root() -> str | None:
    """Discovers the root of the Fuchsia repository or checkout.

    Returns:
      Absolute path to the root directory, or None if not found.
    """
    if "FUCHSIA_DIR" in os.environ and os.path.isdir(os.environ["FUCHSIA_DIR"]):
        return os.path.abspath(os.environ["FUCHSIA_DIR"])

    curr = os.path.abspath(__file__)
    while True:
        parent = os.path.dirname(curr)
        if parent == curr:
            break
        if os.path.exists(os.path.join(parent, ".jiri_root")) or os.path.exists(
            os.path.join(parent, ".fx-root")
        ):
            return parent
        curr = parent

    script_dir = os.path.dirname(os.path.abspath(__file__))
    candidate = os.path.abspath(os.path.join(script_dir, "..", ".."))
    if os.path.isdir(candidate):
        return candidate
    return None


def find_blocked_words_policy_path(
    custom_path: str | None = None,
) -> str | None:
    """Locates the blocked words policy file if available.

    Checks:
    1. custom_path (if provided)
    2. FUCHSIA_SKILL_LINTER_BLOCKED_WORDS environment variable
    3. Canonical Google3 HEAD GCL path (/google/src/files/head/depot/google3/...)
    4. Auto-detected vendor policy paths relative to Fuchsia root.

    Args:
      custom_path: Optional explicit file path.

    Returns:
      Absolute path to the policy file, or None if not found.
    """
    if custom_path:
        if os.path.isfile(custom_path):
            return os.path.abspath(custom_path)
        logging.warning(
            "Specified blocked words policy '%s' not found.", custom_path
        )
        return None

    env_path = os.environ.get("FUCHSIA_SKILL_LINTER_BLOCKED_WORDS")
    if env_path and os.path.isfile(env_path):
        return os.path.abspath(env_path)

    # Directly check canonical Google3 HEAD GCL path
    if os.path.isfile(DEFAULT_GOOGLE3_HEAD_GCL_PATH):
        return DEFAULT_GOOGLE3_HEAD_GCL_PATH

    fuchsia_root = find_fuchsia_root()
    if fuchsia_root:
        for rel_path in DEFAULT_POLICY_REL_PATHS:
            candidate = os.path.join(fuchsia_root, rel_path)
            if os.path.isfile(candidate):
                return candidate

    return None


def _compile_blocked_word_rule(
    term: str,
    pattern: str | None = None,
    reason: str = "Prohibited internal or unannounced term.",
    suggestion: str | None = None,
    scope: str = "public_only",
    severity: str = "error",
    case_sensitive: bool = False,
    check_code_blocks: bool = True,
    allowed_paths: list[str] | None = None,
    exemptions: list[str] | None = None,
) -> BlockedWordRule | None:
    """Constructs and compiles a BlockedWordRule."""
    flags = 0 if case_sensitive else re.IGNORECASE
    if pattern:
        raw_pat = pattern
        if not (
            raw_pat.startswith(r"(_|\b)")
            or raw_pat.startswith(r"(?<!")
            or raw_pat.startswith(r"\b")
        ):
            raw_pat = rf"(?:_|\b)(?:{raw_pat})(?:_|\b)"
        try:
            compiled_pattern = re.compile(raw_pat, flags)
        except re.error as e:
            logging.error(
                "Invalid regex pattern '%s' for term '%s': %s", pattern, term, e
            )
            return None
    else:
        escaped = re.escape(term)
        try:
            compiled_pattern = re.compile(
                rf"(?<![a-zA-Z0-9]){escaped}(?![a-zA-Z0-9])", flags
            )
        except re.error as e:
            logging.error(
                "Failed to compile pattern for term '%s': %s", term, e
            )
            return None
    return BlockedWordRule(
        term=term,
        pattern=compiled_pattern,
        raw_pattern=pattern,
        reason=reason,
        suggestion=suggestion,
        scope=scope,
        severity=severity,
        case_sensitive=case_sensitive,
        check_code_blocks=check_code_blocks,
        allowed_paths=allowed_paths or [],
        exemptions=exemptions or [],
    )


def _parse_gcl_policy(fuchsia_gcl_path: str) -> list[BlockedWordRule]:
    """Parses Gerrit banned words directly from a Fuchsia common.gcl file.

    Args:
      fuchsia_gcl_path: Path to the Fuchsia common.gcl file.

    Returns:
      A list of compiled BlockedWordRule instances.
    """
    if not os.path.isfile(fuchsia_gcl_path):
        return []

    try:
        with open(fuchsia_gcl_path, "r", encoding="utf-8") as f:
            fuchsia_content = f.read()
    except OSError as e:
        logging.error("Failed to read GCL policy '%s': %s", fuchsia_gcl_path, e)
        return []

    rules: list[BlockedWordRule] = []

    # 1. Exemptions block (e.g., keyword exemptions)
    for match_exemptions in re.finditer(
        r"blocked_keyword_pattern\s*=\s*['\"]([^'\"]+)['\"][^}]*?allowed_keyword_pattern\s*=\s*\[(.*?)\]",
        fuchsia_content,
        re.DOTALL,
    ):
        pat = match_exemptions.group(1).strip()
        exemptions = []
        for line in match_exemptions.group(2).splitlines():
            line = re.sub(r"//.*", "", line).strip()
            item = line.strip("',\" ").strip()
            if item:
                exemptions.append(item)
        clean_pat = re.sub(r"\(_\|\\b\)|\(\?:_\|\\b\)|\\[bB]", "", pat)
        m_label = re.search(r"[a-zA-Z0-9_-]+", clean_pat)
        term_label = m_label.group(0).rstrip("?") if m_label else pat
        rule = _compile_blocked_word_rule(
            term=term_label,
            pattern=pat,
            reason="Official Fuchsia banned keyword with exemptions from Gerrit common.gcl.",
            suggestion="Use generic terms or allowlisted identifiers.",
            scope="public_only",
            exemptions=sorted(list(set(exemptions))),
        )
        if rule:
            rules.append(rule)

    # 2. Fuchsia only common banned words
    m_fuchsia = re.search(
        r"fuchsia_only_common_banned_words\s*=\s*.*?\[(.*?)\]\)",
        fuchsia_content,
        re.DOTALL,
    )
    if m_fuchsia:
        for line in m_fuchsia.group(1).splitlines():
            line = re.sub(r"//.*", "", line).strip()
            word = line.strip("',\" ").strip()
            if not word:
                continue
            has_regex = any(
                c in word
                for c in ["?", "(", ")", "|", "\\", "[", "]", "<", ">"]
            )
            if has_regex:
                rule = _compile_blocked_word_rule(
                    term=word,
                    pattern=f"(_|\\b){word}(_|\\b)",
                    reason="Official Fuchsia banned word from Gerrit common.gcl.",
                    scope="public_only",
                )
            else:
                rule = _compile_blocked_word_rule(
                    term=word,
                    reason="Official Fuchsia banned word from Gerrit common.gcl.",
                    scope="public_only",
                )
            if rule:
                rules.append(rule)

    # 3. Android shared common banned words (if android/common.gcl exists in peer directory)
    android_path = os.path.normpath(
        os.path.join(
            os.path.dirname(fuchsia_gcl_path), "..", "android", "common.gcl"
        )
    )
    if os.path.isfile(android_path):
        try:
            with open(android_path, "r", encoding="utf-8") as f:
                android_content = f.read()
            m_android = re.search(
                r"android_shared_common_banned_words\s*=\s*\[(.*?)\]",
                android_content,
                re.DOTALL,
            )
            if m_android:
                for line in m_android.group(1).splitlines():
                    line = re.sub(r"//.*", "", line).strip()
                    word = line.strip("',\" ").strip()
                    if not word:
                        continue
                    clean_word = re.sub(r"\\+b", "", word).strip()
                    has_regex = any(
                        c in clean_word
                        for c in [
                            "?",
                            "(",
                            ")",
                            "|",
                            "\\",
                            "[",
                            "]",
                            "<",
                            ">",
                            "*",
                            "+",
                        ]
                    )
                    if has_regex:
                        rule = _compile_blocked_word_rule(
                            term=clean_word,
                            pattern=f"(_|\\b){clean_word}(_|\\b)",
                            reason="Shared Android/Chromium/Fuchsia banned word from Gerrit common.gcl.",
                            scope="public_only",
                        )
                    else:
                        rule = _compile_blocked_word_rule(
                            term=clean_word,
                            reason="Shared Android/Chromium/Fuchsia banned word from Gerrit common.gcl.",
                            scope="public_only",
                        )
                    if rule:
                        rules.append(rule)
        except OSError:
            pass

    return rules


def load_blocked_words_policy(policy_path: str) -> list[BlockedWordRule]:
    """Loads blocked word rules from a GCL, JSON, or YAML policy file.

    Args:
      policy_path: Path to the GCL, JSON, or YAML policy file.

    Returns:
      A list of compiled BlockedWordRule instances.
    """
    if not os.path.isfile(policy_path):
        logging.warning(
            "Blocked words policy file '%s' not found.", policy_path
        )
        return []

    if policy_path.endswith(".gcl"):
        return _parse_gcl_policy(policy_path)

    try:
        with open(policy_path, "r", encoding="utf-8") as f:
            data = yaml.safe_load(f)
    except Exception as e:
        logging.error(
            "Failed to parse blocked words policy '%s': %s", policy_path, e
        )
        return []

    if not data:
        return []

    rules: list[BlockedWordRule] = []
    default_scope = "public_only"

    raw_items: list[Any] = []
    if isinstance(data, list):
        raw_items = data
    elif isinstance(data, dict):
        default_scope = str(data.get("default_scope") or "public_only")
        if "blocked_words" in data and isinstance(data["blocked_words"], list):
            raw_items = data["blocked_words"]
        elif "terms" in data and isinstance(data["terms"], list):
            raw_items = data["terms"]
        elif "rules" in data and isinstance(data["rules"], list):
            raw_items = data["rules"]

    for item in raw_items:
        if isinstance(item, str):
            term = item.strip()
            if term:
                rule = _compile_blocked_word_rule(
                    term=term, scope=default_scope
                )
                if rule:
                    rules.append(rule)
        elif isinstance(item, dict):
            raw_term = (
                item.get("term")
                if item.get("term") is not None
                else item.get("word")
            )
            term = str(raw_term).strip() if raw_term is not None else ""
            raw_pattern = item.get("pattern")
            pattern = (
                str(raw_pattern).strip() if raw_pattern is not None else None
            )
            if not pattern:
                pattern = None
            if not term and not pattern:
                continue
            if not term:
                term = str(pattern)
            raw_exemptions = (
                item.get("exemptions")
                or item.get("allowed_keyword_pattern")
                or []
            )
            if isinstance(raw_exemptions, str):
                exemptions = [raw_exemptions]
            elif isinstance(raw_exemptions, (list, tuple, set)):
                exemptions = [str(x) for x in raw_exemptions if x is not None]
            else:
                exemptions = []

            raw_allowed_paths = item.get("allowed_paths") or []
            if isinstance(raw_allowed_paths, str):
                allowed_paths = [raw_allowed_paths]
            elif isinstance(raw_allowed_paths, (list, tuple, set)):
                allowed_paths = [
                    str(x) for x in raw_allowed_paths if x is not None
                ]
            else:
                allowed_paths = []

            rule = _compile_blocked_word_rule(
                term=term,
                pattern=pattern,
                reason=str(
                    item.get("reason")
                    or "Prohibited internal or unannounced term."
                ),
                suggestion=item.get("suggestion") or item.get("replacement"),
                scope=str(item.get("scope") or default_scope),
                severity=str(item.get("severity") or "error"),
                case_sensitive=bool(item.get("case_sensitive", False)),
                check_code_blocks=bool(item.get("check_code_blocks", True)),
                allowed_paths=allowed_paths,
                exemptions=exemptions,
            )
            if rule:
                rules.append(rule)

    return rules


def is_vendor_path(path: str) -> bool:
    """Checks whether a file path is within a vendor repository.

    Args:
      path: Path to check.

    Returns:
      True if the path resides inside a 'vendor' directory.
    """
    abs_path = os.path.abspath(path)
    parts = abs_path.split(os.sep)
    return "vendor" in parts


def is_path_allowed(path: str, allowed_patterns: list[str]) -> bool:
    """Checks whether a file path matches any pattern in allowed_paths.

    Args:
      path: The file path.
      allowed_patterns: List of glob patterns.

    Returns:
      True if the path matches an allowed pattern.
    """
    if not allowed_patterns:
        return False
    normalized = os.path.normpath(path)
    basename = os.path.basename(normalized)
    for pattern in allowed_patterns:
        if not pattern or not pattern.strip():
            continue
        norm_pat = os.path.normpath(pattern)
        if norm_pat.endswith(f"{os.sep}**"):
            prefix = norm_pat[: -len(f"{os.sep}**")]
            if (
                normalized == prefix
                or normalized.startswith(f"{prefix}{os.sep}")
                or fnmatch.fnmatch(normalized, f"*{os.sep}{prefix}{os.sep}*")
                or fnmatch.fnmatch(normalized, f"*{os.sep}{prefix}")
            ):
                return True
        if fnmatch.fnmatch(normalized, norm_pat) or fnmatch.fnmatch(
            basename, norm_pat
        ):
            return True
        if (
            fnmatch.fnmatch(normalized, f"*{os.sep}{norm_pat}")
            or fnmatch.fnmatch(normalized, f"{norm_pat}{os.sep}*")
            or fnmatch.fnmatch(normalized, f"*{os.sep}{norm_pat}{os.sep}*")
        ):
            return True
    return False


def is_match_exempt(matched_text: str, exemptions: list[str]) -> bool:
    """Checks if a matched string is permitted under an exemption list.

    Args:
      matched_text: The substring matched by the rule.
      exemptions: List of allowed keyword patterns or strings.

    Returns:
      True if the matched text is permitted.
    """
    clean_matched = matched_text.strip("_ ")
    for ex in exemptions:
        if ex.lower() == clean_matched.lower():
            return True
        try:
            if re.fullmatch(ex, clean_matched, re.IGNORECASE):
                return True
        except re.error:
            pass
    return False


def _format_rule_finding(
    matched_term: str,
    location: str,
    rule: BlockedWordRule,
    snippet: str | None = None,
) -> str:
    """Formats a diagnostic message for a blocked word violation."""
    msg = f'Prohibited term "{matched_term}" found in {location}.'
    if snippet:
        clean_snippet = snippet.strip()
        if len(clean_snippet) > 80:
            clean_snippet = clean_snippet[:77] + "..."
        msg += f"\n  Context: {clean_snippet}"
    if rule.reason:
        msg += f"\n  Reason: {rule.reason}"
    if rule.suggestion:
        msg += f"\n  Suggestion: {rule.suggestion}"
    return msg


def _validate_blocked_words(
    skill_file: str,
    meta: dict[str, Any],
    raw_content: str,
    rules: list[BlockedWordRule],
) -> tuple[list[str], list[str]]:
    """Validates that skill metadata and body do not contain blocked terms.

    Args:
      skill_file: Path to the skill file being linted.
      meta: Frontmatter metadata dictionary.
      raw_content: Complete original file content.
      rules: List of BlockedWordRule instances.

    Returns:
      A tuple of (errors, warnings).
    """
    errors: list[str] = []
    warnings: list[str] = []

    is_vendor = is_vendor_path(skill_file)

    for rule in rules:
        if rule.scope == "public_only" and is_vendor:
            continue
        if rule.scope == "vendor_only" and not is_vendor:
            continue
        if is_path_allowed(skill_file, rule.allowed_paths):
            continue

        def add_violation(
            location: str,
            snippet: str | None = None,
            matched_term: str | None = None,
        ) -> None:
            formatted = _format_rule_finding(
                matched_term=matched_term
                if matched_term is not None
                else rule.term,
                location=location,
                rule=rule,
                snippet=snippet,
            )
            if rule.severity == "warning":
                warnings.append(formatted)
            else:
                errors.append(formatted)

        # 1. Frontmatter fields
        def check_field_value(field_name: str, val: Any) -> bool:
            if isinstance(val, str):
                for match in rule.pattern.finditer(val):
                    if rule.exemptions and is_match_exempt(
                        match.group(0), rule.exemptions
                    ):
                        continue
                    matched_token = match.group(0).strip("_ ")
                    add_violation(
                        f'frontmatter field "{field_name}"',
                        val,
                        matched_term=matched_token,
                    )
                    return True
            elif isinstance(val, list):
                for item in val:
                    if check_field_value(field_name, item):
                        return True
            elif isinstance(val, dict):
                matched = False
                for sub_k, sub_v in val.items():
                    if isinstance(sub_k, str) and check_field_value(
                        field_name, sub_k
                    ):
                        matched = True
                    if check_field_value(f"{field_name}.{sub_k}", sub_v):
                        matched = True
                return matched
            return False

        if "name" in meta:
            check_field_value("name", meta["name"])

        if "description" in meta:
            check_field_value("description", meta["description"])

        for k, v in meta.items():
            if k not in ("name", "description"):
                if isinstance(k, str):
                    check_field_value(k, k)
                check_field_value(str(k), v)

        # 2. Markdown Body lines
        lines = raw_content.split("\n")
        dash_count = 0
        in_code_block = False
        occurrence_count = 0
        max_occurrences_to_report = 5

        for idx, line in enumerate(lines, start=1):
            stripped = line.strip()
            if stripped == "---":
                dash_count += 1
                continue
            if dash_count < 2:
                continue

            if stripped.startswith("```"):
                in_code_block = not in_code_block
                if not rule.check_code_blocks:
                    continue

            if in_code_block and not rule.check_code_blocks:
                continue

            for match in rule.pattern.finditer(line):
                if rule.exemptions and is_match_exempt(
                    match.group(0), rule.exemptions
                ):
                    continue
                occurrence_count += 1
                if occurrence_count <= max_occurrences_to_report:
                    matched_token = match.group(0).strip("_ ")
                    add_violation(
                        f"markdown body (line {idx})",
                        line,
                        matched_term=matched_token,
                    )
                break

        if occurrence_count > max_occurrences_to_report:
            extra = occurrence_count - max_occurrences_to_report
            msg = (
                f'Prohibited term "{rule.term}" also appeared {extra} more'
                " times in the file."
            )
            if rule.severity == "warning":
                warnings.append(msg)
            else:
                errors.append(msg)

    return errors, warnings


class Finding(TypedDict, total=False):
    filepath: str
    message: str
    level: str
    replacements: list[str]


def _get_skill_metadata(
    content: str,
) -> tuple[dict[str, Any], str] | None:
    """Parses metadata from SKILL.md content.

    Args:
      content: The content of the skill file to parse.

    Returns:
      A tuple containing the metadata dict and the remaining file content.
    """
    if not content.startswith("---"):
        return None
    parts = content.split("---", 2)
    if len(parts) < 3:
        return None

    frontmatter = parts[1]
    try:
        meta = yaml.safe_load(frontmatter)
    except yaml.YAMLError:
        return None

    if not isinstance(meta, dict):
        return None

    return meta, parts[2]


def _suggest_valid_name(name: str) -> str:
    """Suggests a valid name by replacing invalid characters.

    Converts to lowercase, replaces underscores with hyphens, removes
    non-alphanumeric characters (except hyphens), and truncates to
    MAX_NAME_LENGTH.

    Args:
      name: The original name string.

    Returns:
      A suggested valid name string.
    """
    suggested = name.lower()
    suggested = suggested.replace("_", "-")
    suggested = re.sub(r"[^a-z0-9-]", "", suggested)
    return suggested[:MAX_NAME_LENGTH].strip("-")


def _pre_process(text: str, tab_size: int) -> str:
    """Pre-processes text to standardize list item indentation.

    Fixes bullet points and numbered lists that have excessive spacing.
    Skips lines within code blocks.

    Args:
      text: The input markdown text.
      tab_size: The indentation size to use for numbered lists.

    Returns:
      The processed text with standardized indentation.
    """
    lines = text.split("\n")
    processed = []
    in_code_block = False
    for line in lines:
        stripped = line.strip()
        if stripped.startswith("```"):
            in_code_block = not in_code_block
            processed.append(line)
            continue

        if in_code_block:
            processed.append(line)
            continue

        # Match bullet points (-, *, +) at the start of the line with excessive spacing.
        # Capture group 1: indentation and bullet.
        # Capture group 2: extra spaces.
        match = re.search(r"^(\s*[-*+])( +)(?=\S)", line)
        if match:
            repl = " "
            line = match.group(1) + repl + line[match.end() :]
        else:
            # Match numbered lists (e.g., 1.) at the start of the line.
            match = re.search(r"^(\s*\d+\.)( +)(?=\S)", line)
            if match:
                repl = " " if tab_size == 2 else "  "
                line = match.group(1) + repl + line[match.end() :]
        processed.append(line)
    return "\n".join(processed)


def _post_process(text: str) -> str:
    """Post-processes text to clean up whitespace.

    Removes trailing whitespace while preserving Markdown hard line breaks
    (at least two spaces at the end of a line), collapses multiple empty
    lines, and ensures the file ends with a newline.

    Args:
      text: The input markdown text.

    Returns:
      The processed text with clean whitespace.
    """
    # Remove trailing whitespace, but preserve Markdown hard line breaks (two spaces).
    text = re.sub(
        r"[ \t]+$",
        lambda m: "  " if m.group(0).endswith("  ") else "",
        text,
        flags=re.MULTILINE,
    )
    # Collapse triple newlines or more into double newlines.
    text = re.sub(r"\n{3,}", "\n\n", text)
    if not text.endswith("\n"):
        text += "\n"
    return text


def _format_paragraph(paragraph: list[str], width: int) -> list[str]:
    """Formats a single paragraph of markdown text.

    Handles list items by indenting subsequent lines to match the marker.
    Preserves hard line breaks in normal paragraphs.

    Args:
      paragraph: A list of lines forming a paragraph.
      width: The maximum line width for wrapping.

    Returns:
      A list of wrapped and formatted lines.
    """
    if not paragraph:
        return []

    first_line = paragraph[0]
    # Check if this is a list item.
    list_match = re.search(r"^(\s*)([-*+]|\d+\.)\s+", first_line)

    if list_match:
        marker = list_match.group(0)
        marker_len = len(marker)
        subsequent_indent = " " * marker_len

        first_line_text = first_line[marker_len:].strip()
        other_lines_text = " ".join([line.strip() for line in paragraph[1:]])
        full_text = (first_line_text + " " + other_lines_text).strip()

        wrapper = textwrap.TextWrapper(
            width=width,
            initial_indent=marker,
            subsequent_indent=subsequent_indent,
            break_long_words=False,
            break_on_hyphens=False,
        )
        return wrapper.wrap(full_text)
    else:
        # Normal paragraph.
        indent_match = re.search(r"^\s*", first_line)
        indent = indent_match.group(0) if indent_match else ""

        # Preserve Markdown hard line breaks (two spaces at end of line).
        has_hard_break = paragraph[-1].endswith("  ")
        content = " ".join([line.strip() for line in paragraph])
        if has_hard_break:
            content = content.rstrip() + "  "

        wrapper = textwrap.TextWrapper(
            width=width,
            initial_indent=indent,
            subsequent_indent=indent,
            break_long_words=False,
            break_on_hyphens=False,
        )
        wrapped_lines = wrapper.wrap(content)
        if has_hard_break and wrapped_lines:
            wrapped_lines[-1] = wrapped_lines[-1].rstrip() + "  "
        return wrapped_lines


def _is_table_separator(line: str) -> bool:
    """Checks if a line is a markdown table separator (e.g., |---|).

    Args:
      line: The line to check.

    Returns:
      True if the line is a table separator, False otherwise.
    """
    stripped = line.strip()
    return "|" in line and bool(re.fullmatch(r"[|\-\s:]+", stripped))


def _get_table_line_indices(lines: list[str]) -> set[int]:
    """Identifies all lines that are part of a markdown table.

    Args:
      lines: The markdown lines to evaluate.

    Returns:
      A set of integer indices representing table lines.
    """
    table_indices = set()
    in_table = False
    for idx, line in enumerate(lines):
        stripped = line.strip()
        if not stripped:
            in_table = False
            continue

        if (
            idx + 1 < len(lines)
            and "|" in line
            and _is_table_separator(lines[idx + 1])
        ):
            table_indices.add(idx)

        if _is_table_separator(line):
            in_table = True
            table_indices.add(idx)
            continue

        if in_table:
            if "|" not in line:
                in_table = False
            else:
                table_indices.add(idx)

    return table_indices


def format_markdown_with_issues(
    text: str, *, tab_size: int = 4, width: int = LINE_LENGTH
) -> tuple[str, list[str]]:
    """Formats markdown text and identifies specific linter issues.

    Identifies paragraphs, lists, code blocks, and tables to apply
    appropriate formatting while preserving structure. Tracks specific
    issues like long lines, trailing whitespace, etc.

    Args:
      text: The input markdown text.
      tab_size: Indentation size for lists.
      width: Maximum line length (defaults to LINE_LENGTH).

    Returns:
      A tuple containing the formatted markdown string and a list of detected issues.
    """
    issues = []

    raw_lines = text.split("\n")

    in_code_block = False
    has_consecutive_empty_lines = False
    has_trailing_whitespace = False
    consecutive_empty_count = 0

    for line in raw_lines:
        stripped = line.strip()
        if stripped.startswith("```"):
            in_code_block = not in_code_block
            consecutive_empty_count = 0
            continue

        if in_code_block:
            continue

        if not stripped:
            consecutive_empty_count += 1
            if consecutive_empty_count >= 2:
                has_consecutive_empty_lines = True
        else:
            consecutive_empty_count = 0

        stripped_len = len(line.rstrip(" \t"))
        trailing_len = len(line) - stripped_len
        if trailing_len > 0 and trailing_len != 2:
            has_trailing_whitespace = True

    if has_consecutive_empty_lines:
        issues.append("consecutive empty lines")
    if has_trailing_whitespace:
        issues.append("trailing whitespace")

    preprocessed_text = _pre_process(text, tab_size)
    if preprocessed_text != text:
        issues.append("excessive list item spacing")

    lines = preprocessed_text.split("\n")
    formatted_lines = []
    code_blocks: list[str] = []
    current_code_block: list[str] = []

    in_code_block = False
    current_paragraph: list[str] = []
    in_table = False

    def flush_paragraph() -> None:
        nonlocal current_paragraph
        formatted_lines.extend(_format_paragraph(current_paragraph, width))
        current_paragraph[:] = []

    for line in lines:
        stripped_line = line.strip()

        if stripped_line.startswith("```"):
            flush_paragraph()
            if in_code_block:
                current_code_block.append(line)
                placeholder = f"__SKILL_LINTER_CODE_BLOCK_{len(code_blocks)}__"
                code_blocks.append("\n".join(current_code_block))
                formatted_lines.append(placeholder)
                current_code_block = []
            else:
                current_code_block.append(line)
            in_code_block = not in_code_block
            continue

        if in_code_block:
            current_code_block.append(line)
            continue

        if not stripped_line:
            flush_paragraph()
            in_table = False
            formatted_lines.append(line)
            continue

        if in_table:
            if "|" not in line:
                in_table = False
            else:
                formatted_lines.append(line)
                continue

        if _is_table_separator(line):
            if current_paragraph:
                header_line = current_paragraph.pop()
                flush_paragraph()
                formatted_lines.append(header_line)
            else:
                flush_paragraph()
            formatted_lines.append(line)
            in_table = True
            continue

        if stripped_line.startswith("#") or stripped_line.startswith(">"):
            flush_paragraph()
            formatted_lines.append(line)
            continue

        if _LINK_REFERENCE_RE.match(stripped_line):
            flush_paragraph()
            formatted_lines.append(line)
            continue

        if re.search(r"^\s*([-*+]|\d+\.)\s+", line):
            flush_paragraph()
            current_paragraph.append(line)
            continue

        current_paragraph.append(line)
        if line.endswith("  "):
            flush_paragraph()

    flush_paragraph()

    # If a file ends inside a code block, preserve it.
    if in_code_block and current_code_block:
        placeholder = f"__SKILL_LINTER_CODE_BLOCK_{len(code_blocks)}__"
        code_blocks.append("\n".join(current_code_block))
        formatted_lines.append(placeholder)

    formatted_text = _post_process("\n".join(formatted_lines))

    for idx, code_block in enumerate(code_blocks):
        placeholder = f"__SKILL_LINTER_CODE_BLOCK_{idx}__"
        formatted_text = formatted_text.replace(placeholder, code_block)

    prep_table_indices = _get_table_line_indices(lines)

    in_code_block = False
    has_long_lines = False
    for idx, line in enumerate(lines):
        if idx in prep_table_indices:
            continue
        stripped = line.strip()
        if stripped.startswith("```"):
            in_code_block = not in_code_block
            continue
        if in_code_block:
            continue
        if not stripped:
            continue
        if _LINK_REFERENCE_RE.match(stripped):
            continue
        if len(line) > width:
            has_long_lines = True
            break

    if has_long_lines:
        issues.append(f"lines exceeding {width} characters")

    if formatted_text != text and not issues:
        issues.append("formatting inconsistencies")

    return formatted_text, issues


def format_markdown(
    text: str, *, tab_size: int = 4, width: int = LINE_LENGTH
) -> str:
    """Formats markdown text to fit within the specified width.

    Args:
      text: The input markdown text.
      tab_size: Indentation size for lists.
      width: Maximum line length (defaults to LINE_LENGTH).

    Returns:
      The formatted markdown string.
    """
    formatted, _ = format_markdown_with_issues(
        text, tab_size=tab_size, width=width
    )
    return formatted


def _validate_name(
    meta: dict[str, Any], fixit: bool
) -> tuple[list[str], list[str]]:
    """Validates the 'name' field in the skill metadata.

    Checks for presence, type, length, and valid characters. If fixit is
    True, attempts to automatically fix issues.

    Args:
      meta: The metadata dictionary.
      fixit: Whether to apply fixes.

    Returns:
      A tuple of (errors, fixes_applied).
    """
    errors = []
    fixes_applied: list[str] = []
    if "name" not in meta:
        errors.append('Missing required field "name" in frontmatter.')
        return errors, fixes_applied

    name = meta["name"]
    if not isinstance(name, str):
        errors.append('Field "name" must be a string.')
        return errors, fixes_applied

    has_length_error = len(name) > MAX_NAME_LENGTH
    has_char_error = not re.fullmatch(r"^[a-z0-9-]+$", name)

    if has_length_error and not fixit:
        errors.append(f'Field "name" exceeds {MAX_NAME_LENGTH} characters.')

    if has_char_error and not fixit:
        errors.append(
            'Field "name" must contain only lowercase letters, numbers, and'
            " hyphens."
        )

    if fixit and (has_length_error or has_char_error):
        suggested_name = _suggest_valid_name(name)
        if suggested_name != name:
            meta["name"] = suggested_name
            fixes_applied.append(f'Fix name to "{suggested_name}"')

    if "<" in name or ">" in name:
        if not fixit:
            errors.append('Field "name" cannot contain XML tags.')
    return errors, fixes_applied


def _validate_description(
    meta: dict[str, Any], fixit: bool
) -> tuple[list[str], list[str]]:
    """Validates the 'description' field in the skill metadata.

    Checks for presence, emptiness, length, and XML tags. If fixit is
    True, attempts to automatically fix issues by stripping tags.

    Args:
      meta: The metadata dictionary.
      fixit: Whether to apply fixes.

    Returns:
      A tuple of (errors, fixes_applied).
    """
    errors = []
    fixes_applied: list[str] = []
    if "description" not in meta:
        errors.append('Missing required field "description" in frontmatter.')
        return errors, fixes_applied

    description = meta["description"]
    if not isinstance(description, str) or not description.strip():
        errors.append('Field "description" cannot be empty.')
        return errors, fixes_applied

    if len(description) > MAX_DESCRIPTION_LENGTH:
        errors.append(
            f'Field "description" exceeds {MAX_DESCRIPTION_LENGTH} characters.'
        )

    if "<" in description or ">" in description:
        if not fixit:
            errors.append('Field "description" cannot contain XML tags.')
        else:
            cleaned_description = re.sub(r"<[^>]+>", "", description)
            cleaned_description = re.sub(
                r" +", " ", cleaned_description
            ).strip()
            if cleaned_description != description:
                meta["description"] = cleaned_description
                fixes_applied.append("Strip XML tags from description.")
            else:
                errors.append(
                    'Field "description" cannot contain angle brackets (< or >).'
                )
    return errors, fixes_applied


def _generate_frontmatter_lines(
    key: str, val: Any, indent: int = 0
) -> list[str]:
    """Generates lines for the YAML frontmatter.

    Handles lists, dicts, None values, and wraps long descriptions using scalar
    blocks if necessary.

    Args:
      key: The metadata key.
      val: The metadata value.
      indent: Current indentation level in spaces.

    Returns:
      A list of lines for the frontmatter.
    """
    lines = []
    prefix = " " * indent
    if isinstance(val, list):
        lines.append(f"{prefix}{key}:")
        for item in val:
            if isinstance(item, dict):
                first = True
                for sub_k, sub_v in item.items():
                    sub_lines = _generate_frontmatter_lines(
                        str(sub_k), sub_v, indent + 4
                    )
                    if first and sub_lines:
                        sub_lines[0] = f"{prefix}  - " + sub_lines[0].lstrip()
                        first = False
                    lines.extend(sub_lines)
            else:
                lines.append(f"{prefix}  - {item}")
    elif isinstance(val, dict):
        lines.append(f"{prefix}{key}:")
        for sub_k, sub_v in val.items():
            lines.extend(
                _generate_frontmatter_lines(str(sub_k), sub_v, indent + 2)
            )
    elif val is None:
        lines.append(f"{prefix}{key}:")
    elif (
        indent == 0
        and key == "description"
        and isinstance(val, str)
        and len(val) > LINE_LENGTH
    ):
        lines.append(f"{key}: >")
        wrapper = textwrap.TextWrapper(
            width=LINE_LENGTH - 2, initial_indent="  ", subsequent_indent="  "
        )
        lines.extend(wrapper.wrap(val))
    else:
        lines.append(f"{prefix}{key}: {val}")
    return lines


def _build_finding(
    rel_path: str,
    errors: list[str],
    warnings: list[str],
    fixes_applied: list[str],
    new_content: str,
    original_content: str,
) -> Finding | None:
    """Builds a Finding dict if there are errors, warnings, or suggested fixes.

    Args:
      rel_path: Relative path to the file.
      errors: List of error messages.
      warnings: List of warning messages.
      fixes_applied: List of fixes applied.
      new_content: The newly formatted content.
      original_content: The original file content.

    Returns:
      A Finding dict or None if no findings.
    """
    if not (new_content != original_content or errors or warnings):
        return None

    level = "error" if errors else "warning"
    messages = []
    if errors:
        messages.append("Errors:\n" + "\n".join(f"- {e}" for e in errors))
    if warnings:
        messages.append("Warnings:\n" + "\n".join(f"- {w}" for w in warnings))
    if fixes_applied and not messages:
        messages.append(
            "Suggested fixes:\n" + "\n".join(f"- {f}" for f in fixes_applied)
        )

    finding_message = "\n\n".join(messages) or "Skill linter findings."
    if fixes_applied or (new_content != original_content):
        finding_message += "\n\nYou can run fx format-code to fix these issues."

    finding: Finding = {
        "filepath": rel_path,
        "message": finding_message,
        "level": level,
    }
    if new_content != original_content:
        finding["replacements"] = [new_content]
    return finding


def lint_single_skill(
    skill_file: str,
    fixit: bool,
    stdout_mode: bool,
    suggest_fix_in_json_mode: bool = False,
    blocked_word_rules: list[BlockedWordRule] | None = None,
) -> tuple[int, list[Finding]]:
    """Lints a single SKILL.md file.

    Reads the file, parses metadata, validates fields, formats content,
    and writes back or outputs findings depending on the mode.

    Args:
      skill_file: Path to the SKILL.md file.
      fixit: Whether to apply fixes in-place.
      stdout_mode: Whether to output fixed content to stdout.
      suggest_fix_in_json_mode: Whether to return findings as JSON.
      blocked_word_rules: Optional list of BlockedWordRule instances to enforce.

    Returns:
      A tuple of (exit_code, findings).
    """
    findings = []
    rel_path = os.path.relpath(skill_file)

    try:
        with open(skill_file, "r") as f:
            original_content = f.read()
    except OSError as e:
        msg = f"Error reading '{skill_file}': {e}"
        if suggest_fix_in_json_mode:
            findings.append(
                Finding(filepath=rel_path, message=msg, level="error")
            )
            return 0, findings
        else:
            logging.error(msg)
            return 1, findings

    res = _get_skill_metadata(original_content)
    if not res:
        msg = f"Error: Could not find or parse YAML frontmatter in '{skill_file}'."
        if suggest_fix_in_json_mode:
            findings.append(
                Finding(filepath=rel_path, message=msg, level="error")
            )
            return 0, findings
        else:
            logging.error(msg)
            return 1, findings

    meta, content = res
    content = content.lstrip("\n")
    errors = []
    warnings = []
    fixes_applied: list[str] = []

    if blocked_word_rules:
        bw_errors, bw_warnings = _validate_blocked_words(
            skill_file=skill_file,
            meta=meta,
            raw_content=original_content,
            rules=blocked_word_rules,
        )
        errors.extend(bw_errors)
        warnings.extend(bw_warnings)

    name_errors, name_fixes = _validate_name(
        meta, fixit or suggest_fix_in_json_mode
    )
    errors.extend(name_errors)
    fixes_applied.extend(name_fixes)

    description_errors, description_fixes = _validate_description(
        meta, fixit or suggest_fix_in_json_mode
    )
    errors.extend(description_errors)
    fixes_applied.extend(description_fixes)

    formatted_content, markdown_issues = format_markdown_with_issues(content)
    if formatted_content != content:
        issues_str = ", ".join(markdown_issues)
        if not fixit and not suggest_fix_in_json_mode:
            warnings.append(f"Markdown body contains {issues_str}.")
        else:
            fixes_applied.append(f"Address markdown body: {issues_str}.")
            content = formatted_content

    skill_name = os.path.basename(os.path.dirname(skill_file))

    ordered_keys = ["name", "description"]
    frontmatter_lines = []

    for key in ordered_keys:
        if key in meta:
            frontmatter_lines.extend(
                _generate_frontmatter_lines(key, meta[key])
            )

    for key in meta:
        if key not in ordered_keys:
            frontmatter_lines.extend(
                _generate_frontmatter_lines(key, meta[key])
            )

    new_frontmatter = "\n".join(frontmatter_lines) + "\n"
    new_content = f"---\n{new_frontmatter}---\n\n{content}"

    if suggest_fix_in_json_mode:
        finding = _build_finding(
            rel_path,
            errors,
            warnings,
            fixes_applied,
            new_content,
            original_content,
        )
        if finding:
            findings.append(finding)
        return 0, findings

    if warnings:
        for warn in warnings:
            logging.warning(f"[{skill_name}] Warning: {warn}")

    if errors:
        for err in errors:
            logging.error(f"[{skill_name}] Error: {err}")

    has_autofixes = bool(
        fixes_applied
        or (formatted_content != content)
        or (new_content != original_content)
    )
    if has_autofixes and not suggest_fix_in_json_mode:
        logging.warning(
            f"[{skill_name}] You can run fx format-code to fix these issues."
        )

    if stdout_mode:
        print(new_content, end="")
    elif not suggest_fix_in_json_mode:
        if fixit and new_content != original_content:
            try:
                with open(skill_file, "w") as f:
                    f.write(new_content)
                logging.info(f"[{skill_name}] Fixed in-place.")
            except OSError as e:
                logging.error(f"Error writing to '{skill_file}': {e}")
                return 1, findings
        elif not errors and not warnings and new_content == original_content:
            logging.info(f"PASSED: '{skill_name}' is valid.")
    return (1 if errors else 0), findings


def do_lint(
    root_path: str,
    fixit: bool,
    stdout_mode: bool,
    suggest_fix_in_json_mode: bool = False,
    blocked_word_rules: list[BlockedWordRule] | None = None,
) -> tuple[int, list[Finding]]:
    """Orchestrates the linting process for a file or directory.

    Args:
      root_path: Path to a file or directory to lint.
      fixit: Whether to apply fixes in-place.
      stdout_mode: Whether to output fixed content to stdout.
      suggest_fix_in_json_mode: Whether to return findings as JSON.
      blocked_word_rules: Optional list of BlockedWordRule instances to enforce.

    Returns:
      A tuple of (max_exit_code, all_findings).
    """
    if os.path.isfile(root_path):
        return lint_single_skill(
            root_path,
            fixit,
            stdout_mode,
            suggest_fix_in_json_mode,
            blocked_word_rules=blocked_word_rules,
        )
    elif os.path.exists(os.path.join(root_path, "SKILL.md")):
        return lint_single_skill(
            os.path.join(root_path, "SKILL.md"),
            fixit,
            stdout_mode,
            suggest_fix_in_json_mode,
            blocked_word_rules=blocked_word_rules,
        )
    else:
        all_findings: list[Finding] = []
        try:
            subdirs = [
                os.path.join(root_path, d)
                for d in os.listdir(root_path)
                if os.path.isdir(os.path.join(root_path, d))
            ]
        except OSError as e:
            msg = f"Error reading directory '{root_path}': {e}"
            if suggest_fix_in_json_mode:
                all_findings.append(
                    Finding(
                        filepath=os.path.relpath(root_path),
                        message=msg,
                        level="error",
                    )
                )
            else:
                logging.error(msg)
            return 0 if suggest_fix_in_json_mode else 1, all_findings

        max_code = 0
        for subdir in subdirs:
            if os.path.exists(os.path.join(subdir, "SKILL.md")):
                code, findings = lint_single_skill(
                    os.path.join(subdir, "SKILL.md"),
                    fixit,
                    stdout_mode,
                    suggest_fix_in_json_mode,
                    blocked_word_rules=blocked_word_rules,
                )
                all_findings.extend(findings)
                max_code = max(max_code, code)

        return max_code, all_findings


def main() -> None:
    """Main entry point for the skill linter.

    Parses command line arguments, configures logging, and runs the
    linting process on specified paths.
    """
    parser = argparse.ArgumentParser(description="Skill validation checker.")
    parser.add_argument(
        "--fixit",
        action="store_true",
        help="Suggest and autofix validation errors.",
    )
    parser.add_argument(
        "--suggest-fix",
        action="store_true",
        help="Applies fixes and outputs the result directly to stdout.",
    )
    parser.add_argument(
        "--suggest-fix-in-json",
        action="store_true",
        help="Output findings as a JSON array.",
    )
    parser.add_argument(
        "--blocked-words-file",
        type=str,
        default=None,
        help="Path to a blocked words policy file (JSON or YAML).",
    )
    parser.add_argument(
        "--no-blocked-words",
        action="store_true",
        help="Disable blocked words scanning.",
    )
    parser.add_argument(
        "paths",
        nargs="+",
        help="Paths to SKILL.md files or directories containing them.",
    )

    args = parser.parse_args()

    logging.basicConfig(
        level=logging.INFO,
        format="%(message)s",
        stream=sys.stderr,
    )

    blocked_word_rules: list[BlockedWordRule] = []
    if not args.no_blocked_words:
        if args.blocked_words_file:
            if not os.path.isfile(args.blocked_words_file):
                msg = f"Specified blocked words policy '{args.blocked_words_file}' not found."
                logging.error(msg)
                if args.suggest_fix_in_json:
                    print(
                        json.dumps(
                            [
                                {
                                    "filepath": args.blocked_words_file,
                                    "message": msg,
                                    "level": "error",
                                }
                            ],
                            indent=2,
                        )
                    )
                    sys.exit(0)
                sys.exit(1)
            blocked_word_rules = load_blocked_words_policy(
                args.blocked_words_file
            )
        else:
            policy_path = find_blocked_words_policy_path()
            if policy_path:
                blocked_word_rules = load_blocked_words_policy(policy_path)

    all_findings = []
    any_error = False
    for path in args.paths:
        code, findings = do_lint(
            path,
            args.fixit or args.suggest_fix,
            args.suggest_fix,
            args.suggest_fix_in_json,
            blocked_word_rules=blocked_word_rules,
        )
        all_findings.extend(findings)
        if code != 0:
            any_error = True

    if args.suggest_fix_in_json:
        print(json.dumps(all_findings, indent=2))

    sys.exit(1 if any_error else 0)


if __name__ == "__main__":
    main()
