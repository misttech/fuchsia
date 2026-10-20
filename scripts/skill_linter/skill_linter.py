# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Skill Linter tool for parsing and formatting SKILL.md files.
"""

import argparse
import json
import logging
import os
import re
import sys
import textwrap
from typing import Any, TypedDict

import yaml

# Constants for skill validation and formatting.
MAX_NAME_LENGTH = 64
MAX_DESCRIPTION_LENGTH = 1024
LINE_LENGTH = 80

_LINK_REFERENCE_RE = re.compile(r"^\s{0,3}\[[^\]]+\]:\s*")
_LIST_ITEM_RE = re.compile(r"^\s*([-*+]|\d+\.)\s+")


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
    Standardizes child list items, code blocks, and continuation paragraphs
    under numbered list items to match the 4-space list indentation.
    Skips lines within code blocks.

    Args:
      text: The input markdown text.
      tab_size: The indentation size to use for numbered lists.

    Returns:
      The processed text with standardized indentation.
    """
    lines = text.split("\n")
    processed = []
    in_top_level_code_block = False
    in_nested_code_block = False
    nested_code_block_shifted = False
    active_num_indent: int | None = None

    for line in lines:
        stripped = line.strip()

        # Handle top level code block
        if in_top_level_code_block:
            if stripped.startswith("```"):
                in_top_level_code_block = False
            processed.append(line)
            continue

        # Handle nested code block
        if in_nested_code_block:
            if stripped.startswith("```"):
                if nested_code_block_shifted and active_num_indent is not None:
                    target_3 = " " * (active_num_indent + 3)
                    if line.startswith(
                        target_3 + "```"
                    ) and not line.startswith(target_3 + " ```"):
                        line = " " + line
                in_nested_code_block = False
                nested_code_block_shifted = False
            else:
                if nested_code_block_shifted and active_num_indent is not None:
                    target_3 = " " * (active_num_indent + 3)
                    if line.startswith(target_3):
                        line = " " + line
            processed.append(line)
            continue

        if not stripped:
            processed.append(line)
            continue

        if stripped.startswith("#") or re.match(r"^([-*_]\s*){3,}$", stripped):
            active_num_indent = None

        # Check for numbered list item
        num_match = re.match(r"^(\s*)(\d+\.)( +)(.*)$", line)
        if num_match:
            indent_str = num_match.group(1)
            marker_str = num_match.group(2)
            rest = num_match.group(4)
            repl = " " if tab_size == 2 else "  "
            line = f"{indent_str}{marker_str}{repl}{rest}"
            if tab_size == 4:
                active_num_indent = len(indent_str)
                in_nested_code_block = False
                nested_code_block_shifted = False
            processed.append(line)
            continue

        # Check if non-empty line terminates active_num_indent
        if active_num_indent is not None:
            indent_len = len(line) - len(line.lstrip(" "))
            if indent_len <= active_num_indent:
                active_num_indent = None

        # Code block opening
        if stripped.startswith("```"):
            if active_num_indent is not None and tab_size == 4:
                target_3 = " " * (active_num_indent + 3)
                if line.startswith(target_3 + "```") and not line.startswith(
                    target_3 + " ```"
                ):
                    line = " " + line
                    nested_code_block_shifted = True
                in_nested_code_block = True
            else:
                in_top_level_code_block = True
            processed.append(line)
            continue

        if active_num_indent is not None and tab_size == 4:
            target_3 = " " * (active_num_indent + 3)
            target_5 = " " * (active_num_indent + 5)
            # 3-space bullet point
            bullet_3_match = re.match(
                r"^" + target_3 + r"([-*+])( +)(.*)$", line
            )
            if bullet_3_match:
                marker = bullet_3_match.group(1)
                rest = bullet_3_match.group(3)
                line = " " + target_3 + marker + " " + rest
                processed.append(line)
                continue
            # 5-space sub-bullet
            bullet_5_match = re.match(
                r"^" + target_5 + r"([-*+])( +)(.*)$", line
            )
            if bullet_5_match:
                marker = bullet_5_match.group(1)
                rest = bullet_5_match.group(3)
                line = " " + target_5 + marker + " " + rest
                processed.append(line)
                continue
            # 5-space continuation
            if line.startswith(target_5) and not line.startswith(
                target_5 + " "
            ):
                line = " " + line
                processed.append(line)
                continue
            # 3-space continuation or paragraph
            if line.startswith(target_3) and not line.startswith(
                target_3 + " "
            ):
                line = " " + line
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
            if "|" in line or stripped.startswith(":"):
                table_indices.add(idx)
            else:
                in_table = False

    return table_indices


def format_markdown_with_issues(
    text: str, *, tab_size: int = 4, width: int = LINE_LENGTH
) -> tuple[str, list[str]]:
    """Formats markdown text and identifies specific linter issues.

    Identifies paragraphs, lists, code blocks, and tables to apply
    appropriate formatting while preserving structure. Tracks specific
    issues like long lines, trailing whitespace, missing empty lines around
    lists and code blocks, etc.

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
    in_list = False
    has_missing_empty_line_before_list = False
    has_missing_empty_line_after_list = False
    has_missing_empty_line_before_code_block = False
    has_missing_empty_line_after_code_block = False
    just_closed_code_block = False
    # Whether the last emitted non-empty block is a list item paragraph. A
    # list item may directly follow another list item (tight list), but must
    # be separated by an empty line from any other preceding block.
    last_block_is_list_item = False

    def flush_paragraph() -> None:
        nonlocal current_paragraph, last_block_is_list_item
        if current_paragraph:
            last_block_is_list_item = bool(
                _LIST_ITEM_RE.match(current_paragraph[0])
            )
        formatted_lines.extend(_format_paragraph(current_paragraph, width))
        current_paragraph[:] = []

    def emit(line: str) -> None:
        nonlocal last_block_is_list_item
        if line.strip():
            last_block_is_list_item = False
        formatted_lines.append(line)

    def needs_empty_line() -> bool:
        return bool(formatted_lines) and bool(formatted_lines[-1].strip())

    def end_list_with_empty_line() -> None:
        # A non-indented block (heading, blockquote, table, or link reference
        # definition) directly after a list must be separated by an empty line.
        nonlocal in_list, has_missing_empty_line_after_list
        if in_list and needs_empty_line():
            formatted_lines.append("")
            has_missing_empty_line_after_list = True
        in_list = False

    for line in lines:
        stripped_line = line.strip()
        is_indented = line.startswith((" ", "\t"))

        if in_code_block:
            if stripped_line.startswith("```"):
                current_code_block.append(line)
                placeholder = f"__SKILL_LINTER_CODE_BLOCK_{len(code_blocks)}__"
                code_blocks.append("\n".join(current_code_block))
                emit(placeholder)
                current_code_block = []
                in_code_block = False
                if not is_indented:
                    in_list = False
                just_closed_code_block = True
            else:
                current_code_block.append(line)
            continue

        if just_closed_code_block:
            just_closed_code_block = False
            if stripped_line:
                formatted_lines.append("")
                has_missing_empty_line_after_code_block = True

        if stripped_line.startswith("```"):
            flush_paragraph()
            if needs_empty_line():
                formatted_lines.append("")
                has_missing_empty_line_before_code_block = True
            current_code_block.append(line)
            in_code_block = True
            # Code blocks indented under a list item belong to that item.
            if not is_indented:
                in_list = False
            continue

        if not stripped_line:
            flush_paragraph()
            in_table = False
            # A list stays active across empty lines; it only ends at the
            # next non-indented block that is not a list item.
            formatted_lines.append(line)
            continue

        if in_table:
            if "|" in line or stripped_line.startswith(":"):
                emit(line)
                continue
            else:
                in_table = False

        if _is_table_separator(line):
            if current_paragraph:
                header_line = current_paragraph.pop()
                flush_paragraph()
                if not header_line.startswith((" ", "\t")):
                    end_list_with_empty_line()
                emit(header_line)
            else:
                flush_paragraph()
            emit(line)
            in_table = True
            in_list = False
            continue

        if stripped_line.startswith("#") or stripped_line.startswith(">"):
            flush_paragraph()
            if not is_indented:
                end_list_with_empty_line()
            emit(line)
            continue

        if _LINK_REFERENCE_RE.match(stripped_line):
            flush_paragraph()
            end_list_with_empty_line()
            emit(line)
            continue

        if _LIST_ITEM_RE.match(line):
            flush_paragraph()
            if needs_empty_line() and not last_block_is_list_item:
                formatted_lines.append("")
                has_missing_empty_line_before_list = True
            in_list = True
            current_paragraph.append(line)
            continue

        # Non-indented text directly following a list item is a lazy
        # continuation of that item, so the list remains active. After an
        # empty line, non-indented text starts a new paragraph that ends the
        # list.
        if not is_indented and not current_paragraph and not needs_empty_line():
            in_list = False
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

    if has_missing_empty_line_before_list:
        issues.append("missing empty line before list")

    if has_missing_empty_line_after_list:
        issues.append("missing empty line after list")

    if has_missing_empty_line_before_code_block:
        issues.append("missing empty line before code block")

    if has_missing_empty_line_after_code_block:
        issues.append("missing empty line after code block")

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
) -> tuple[int, list[Finding]]:
    """Lints a single SKILL.md file.

    Reads the file, parses metadata, validates fields, formats content,
    and writes back or outputs findings depending on the mode.

    Args:
      skill_file: Path to the SKILL.md file.
      fixit: Whether to apply fixes in-place.
      stdout_mode: Whether to output fixed content to stdout.
      suggest_fix_in_json_mode: Whether to return findings as JSON.

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
) -> tuple[int, list[Finding]]:
    """Orchestrates the linting process for a file or directory.

    Args:
      root_path: Path to a file or directory to lint.
      fixit: Whether to apply fixes in-place.
      stdout_mode: Whether to output fixed content to stdout.
      suggest_fix_in_json_mode: Whether to return findings as JSON.

    Returns:
      A tuple of (max_exit_code, all_findings).
    """
    if os.path.isfile(root_path):
        return lint_single_skill(
            root_path, fixit, stdout_mode, suggest_fix_in_json_mode
        )
    elif os.path.exists(os.path.join(root_path, "SKILL.md")):
        return lint_single_skill(
            os.path.join(root_path, "SKILL.md"),
            fixit,
            stdout_mode,
            suggest_fix_in_json_mode,
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

    all_findings = []
    any_error = False
    for path in args.paths:
        code, findings = do_lint(
            path,
            args.fixit or args.suggest_fix,
            args.suggest_fix,
            args.suggest_fix_in_json,
        )
        all_findings.extend(findings)
        if code != 0:
            any_error = True

    if args.suggest_fix_in_json:
        print(json.dumps(all_findings, indent=2))

    sys.exit(1 if any_error else 0)


if __name__ == "__main__":
    main()
