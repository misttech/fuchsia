#!/usr/bin/env fuchsia-vendored-python
# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Generic utilities for working with text protobufs (without schema).
"""

import argparse
import collections
import dataclasses
import enum
import re
import sys
from collections.abc import Iterable, Iterator, Mapping, Sequence
from pathlib import Path
from typing import Any


class TokenType(enum.Enum):
    FIELD_NAME = 0  # includes trailing ':'
    START_BLOCK = 1  # '<' or '{'
    END_BLOCK = 2  # '>' or '}'
    STRING_VALUE = 3  # quoted text, e.g. "string"
    OTHER_VALUE = 4  # non-string value
    SPACE = 5  # [ \t]*
    NEWLINE = 6  # [\n\r]


@dataclasses.dataclass
class Token(object):
    text: str
    type: TokenType


_FIELD_NAME_RE = re.compile(r"[a-zA-Z_][a-zA-Z0-9_]*:")
_SPACE_RE = re.compile(r"[ \t]+")
_NEWLINE_RE = re.compile(r"\r?\n")
_STRING_RE = re.compile(r'"(?:[^\\"]++|\\.)*+"')  # Allow escaped quotes inside
_VALUE_RE = re.compile(
    r"[^ \t\r\n,;{}<>#]+"
)  # Value token excluding delimiters and comments


def _lex_line(line: str) -> Iterable[Token]:
    while line:  # is not empty
        next_char = line[0]

        if next_char == "#":
            # Comment extends to end of line
            break

        if next_char in {",", ";"}:
            # Skip optional field separators
            line = line[1:]
            continue

        if next_char in {"<", "{"}:
            yield Token(text=next_char, type=TokenType.START_BLOCK)
            line = line[1:]
            continue

        if next_char in {">", "}"}:
            yield Token(text=next_char, type=TokenType.END_BLOCK)
            line = line[1:]
            continue

        field_match = _FIELD_NAME_RE.match(line)
        if field_match:
            field_name = field_match.group(0)
            yield Token(text=field_name, type=TokenType.FIELD_NAME)
            line = line[len(field_name) :]
            continue

        string_match = _STRING_RE.match(line)
        if string_match:
            string = string_match.group(0)
            yield Token(text=string, type=TokenType.STRING_VALUE)
            line = line[len(string) :]
            continue

        value_match = _VALUE_RE.match(line)
        if value_match:
            value = value_match.group(0)
            yield Token(text=value, type=TokenType.OTHER_VALUE)
            line = line[len(value) :]
            continue

        space_match = _SPACE_RE.match(line)
        if space_match:
            space = space_match.group(0)
            yield Token(text=space, type=TokenType.SPACE)
            line = line[len(space) :]
            continue

        newline_match = _NEWLINE_RE.match(line)
        if newline_match:
            newline = newline_match.group(0)
            yield Token(text=newline, type=TokenType.NEWLINE)
            line = line[len(newline) :]
            continue

        raise ValueError(f'[textpb.lex] Unrecognized text: "{line}"')


def yield_verbose(items: Iterable[Any]) -> Iterable[Any]:
    for item in items:
        print(f"{item}")
        yield item


def _lex(lines: Iterable[str]) -> Iterable[Token]:
    """Divides proto text into tokens."""
    for line in lines:
        yield from _lex_line(line)


class ParseError(ValueError):
    def __init__(self, msg: str):
        super().__init__(msg)


def _auto_dict(values: Sequence[Any]) -> Any:
    """Convert sequences of key-value pairs to dictionaries."""
    if len(values) == 0:
        return values

    if all(
        isinstance(v, Mapping) and v.keys() == {"key", "value"} for v in values
    ):
        # assume keys are unique quoted strings
        # 'key' and 'value' should not be repeated fields
        return {v["key"][0].text.strip('"'): v["value"][0] for v in values}

    return values


def _parse_block(tokens: Iterator[Token], top: bool) -> dict[str, Any]:
    """Parse text proto tokens into a structure.

    Args:
      tokens: lexical tokens, without any spaces/newlines.

    Returns:
      dictionary representation of text proto.
    """
    # Without a schema, we cannot deduce whether a field is scalar or
    # repeated, so treat them as repeated (maybe singleton).
    result = collections.defaultdict(list)

    while True:
        try:
            field = next(tokens)
        except StopIteration:
            if top:
                break
            else:
                raise ParseError(
                    "Unexpected EOF, missing '>' or '}' end-of-block"
                )

        if field.type == TokenType.END_BLOCK:
            if top:
                raise ParseError(
                    "Unexpected end-of-block at top-level before EOF."
                )
            break

        if field.type != TokenType.FIELD_NAME:
            raise ParseError(f"Expected a field name, but got {field}.")

        key = field.text[:-1]  # removes trailing ':'
        try:
            value_or_block = next(tokens)
        except StopIteration:
            raise ParseError(
                "Unexpected EOF, expecting a value or start-of-block."
            )

        value: dict[str, Any] | Token
        if value_or_block.type == TokenType.START_BLOCK:
            value = _parse_block(tokens, top=False)
        elif value_or_block.type in {
            TokenType.STRING_VALUE,
            TokenType.OTHER_VALUE,
        }:
            value = value_or_block  # a Token
        else:
            raise ParseError(f"Unexpected token: {value_or_block}")

        result[key].append(value)

    # End of block, post-process key-value pairs into dictionaries.
    return {k: _auto_dict(v) for k, v in result.items()}


def _parse_tokens(tokens: Iterator[Token]) -> dict[str, Any]:
    return _parse_block(tokens, top=True)


def parse(lines: Iterable[str]) -> dict[str, Any]:
    """Parse a text protobuf into a recursive dictionary.

    Args:
      lines: lines of text proto (spaces and line breaks are ignored)

    Returns:
      Structured representation of the proto.
      Fields are treated as either repeated (even if the original
      schema was scalar) or as key-value dictionaries.
    """
    # ignore spaces
    return _parse_tokens(
        token
        for token in _lex(lines)
        if token.type not in {TokenType.SPACE, TokenType.NEWLINE}
    )


_WHITESPACE_OR_COMMENT_RE = re.compile(r"(?:[ \t\r\n,;]+|#[^\r\n]*)*")
_CANDIDATE_DELIM_RE = re.compile(r"""[{}<>"#]""")


@dataclasses.dataclass(slots=True)
class _FieldEntry:
    """Descriptor for an indexed, unparsed field in a text protobuf.

    Analogous to a C++ `std::string_view`, submessage boundaries are recorded as
    integer offset spans `[body_start, body_end]` within the original buffer
    rather than slicing or allocating intermediate substrings.

    Attributes:
      is_message: True if this field is a submessage ({ ... } or < ... >).
      body_start: Start index of the inner body (excluding opening delimiter).
      body_end: End index of the inner body (excluding closing delimiter).
      value_text: Raw text of the scalar value (empty if is_message is True).
      token_type: TokenType classification of the scalar value.
    """

    is_message: bool
    body_start: int = 0
    body_end: int = 0
    value_text: str = ""
    token_type: TokenType = TokenType.OTHER_VALUE


def _find_block_end(
    text: str, open_delim: str, start_pos: int, end_pos: int
) -> tuple[int, int]:
    """Finds the matching closing delimiter for a block starting at start_pos.

    Scans forward by balancing opening and closing delimiters ({}, <>) while
    ignoring structural characters inside string literals and comments.

    Performance note:
      If a caller traverses deeply nested submessages down to depth D, the
      innermost characters may be scanned D times (once per ancestor message as
      each ancestor indexes its direct children). In practice, protobuf schemas
      are shallow (typically D <= 3), and unaccessed large blocks (like command)
      are scanned only once.
      While it is possible to eliminate redundant scanning by memoizing child
      delimiter spans during the first pass, that would introduce bookkeeping
      and allocation overhead for every intermediate block across the entire file,
      slowing down the common path. The stateless scanner is a deliberate
      trade-off favoring simplicity and zero allocations.
    """
    close_map = {"{": "}", "<": ">"}
    stack = [close_map[open_delim]]
    pos = start_pos
    quote_char = '"'
    while pos < end_pos:
        m = _CANDIDATE_DELIM_RE.search(text, pos, end_pos)
        if not m:
            break
        ch = m.group(0)
        idx = m.start()
        if ch == quote_char:
            pos = idx + 1
            while pos < end_pos:
                end_q = text.find(quote_char, pos, end_pos)
                if end_q == -1:
                    raise ParseError(f"Unterminated string starting at {idx}")
                bs_count = 0
                b = end_q - 1
                while b >= idx and text[b] == "\\":
                    bs_count += 1
                    b -= 1
                pos = end_q + 1
                if bs_count % 2 == 0:
                    break
        elif ch == "#":
            nl = text.find("\n", idx + 1, end_pos)
            pos = nl + 1 if nl != -1 else end_pos
        elif ch in "{<":
            stack.append(close_map[ch])
            pos = idx + 1
        elif ch in "}>":
            if not stack or ch != stack[-1]:
                raise ParseError(
                    f"Mismatched end-of-block: got '{ch}', expected '{stack[-1] if stack else None}'"
                )
            stack.pop()
            pos = idx + 1
            if not stack:
                return idx, idx + 1
        else:
            pos = idx + 1
    raise ParseError("Unexpected EOF, missing '>' or '}' end-of-block")


def _index_fields(
    text: str, start: int, end: int, top: bool
) -> dict[str, list[_FieldEntry]]:
    """Indexes direct child fields within text[start:end] without descending.

    Args:
      text: The source textproto string.
      start: Starting character offset of the block to index.
      end: Ending character offset of the block to index.
      top: True if this is the root document, False if inside a submessage.

    Returns:
      Dictionary mapping field names to a list of _FieldEntry descriptors.
    """
    pos = start
    fields: dict[str, list[_FieldEntry]] = collections.defaultdict(list)
    while pos < end:
        # Skip inter-field whitespace, comments, and optional separators (, ;)
        m_ws = _WHITESPACE_OR_COMMENT_RE.match(text, pos, end)
        if m_ws:
            pos = m_ws.end()
        if pos >= end:
            break

        # Check for closing delimiter of the current block
        ch = text[pos]
        if ch in "}>":
            if top:
                raise ParseError(
                    "Unexpected end-of-block at top-level before EOF."
                )
            break

        # Match the field identifier (e.g. "field_name:")
        m_field = _FIELD_NAME_RE.match(text, pos, end)
        if not m_field:
            tok = (
                text[pos : pos + 20].split()[0]
                if text[pos : pos + 20].split()
                else text[pos]
            )
            raise ParseError(f"Expected a field name, but got '{tok}'.")

        key = m_field.group(0)[:-1]
        pos = m_field.end()

        # Skip whitespace/comments between the field name and its value or block
        m_ws = _WHITESPACE_OR_COMMENT_RE.match(text, pos, end)
        if m_ws:
            pos = m_ws.end()

        if pos >= end:
            raise ParseError(
                "Unexpected EOF, expecting a value or start-of-block."
            )

        next_ch = text[pos]
        if next_ch in "{<":
            # Submessage block: record the inner body span without descending
            # or parsing any nested fields inside it.
            body_start = pos + 1
            body_end, block_end = _find_block_end(
                text, next_ch, body_start, end
            )
            fields[key].append(
                _FieldEntry(
                    is_message=True,
                    body_start=body_start,
                    body_end=body_end,
                )
            )
            pos = block_end
        elif next_ch == '"':
            # Quoted string value
            m_str = _STRING_RE.match(text, pos, end)
            if not m_str:
                raise ParseError(f"Unterminated string starting at {pos}")
            fields[key].append(
                _FieldEntry(
                    is_message=False,
                    value_text=m_str.group(0),
                    token_type=TokenType.STRING_VALUE,
                )
            )
            pos = m_str.end()
        elif next_ch in "}>":
            raise ParseError("Unexpected token: TokenType.END_BLOCK")
        else:
            # Unquoted scalar value (enum, number, boolean identifier)
            m_val = _VALUE_RE.match(text, pos, end)
            if not m_val:
                raise ParseError(f"Unexpected character: '{next_ch}'")
            fields[key].append(
                _FieldEntry(
                    is_message=False,
                    value_text=m_val.group(0),
                    token_type=TokenType.OTHER_VALUE,
                )
            )
            pos = m_val.end()

    return fields


class LazyMapping(Mapping[str, Any]):
    """Lazy, hierarchical mapping representation of a text protobuf message.

    This class provides a drop-in replacement for the nested dictionary structure
    returned by `parse()`, while deferring tokenization and AST generation until
    individual fields are accessed. Conceptually analogous to hierarchical C++
    `std::string_view`s over a single shared text buffer, each `LazyMapping`
    windows into `text[start:end]` without copying memory or creating intermediate
    substring allocations.

    Behavior and Lifecycle:
      - Instantiation: Fast and non-allocating; only records the string buffer
        and character bounds `[start, end]`.
      - Shallow Indexing: On the first field access, `len()`, or key iteration,
        `_index_fields` scans only the direct child fields of this block,
        recording `_FieldEntry` descriptors. Nested submessages (`{ ... }` or
        `< ... >`) have their boundaries detected by balancing delimiters while
        respecting string literals and comments, but their inner contents are not
        descended into or tokenized.
      - On-Demand Materialization: Accessing `mapping[key]` materializes only
        that specific field:
          * Submessages are wrapped in child `LazyMapping` instances restricted
            to their inner body spans.
          * Scalar values are converted into `Token` instances (`Token(text, type)`).
          * Repeated submessages containing `key` and `value` fields (e.g. map
            fields like `output_file_digests` or `platform`) are converted to
            dictionaries (`{k: v}`), matching eager `_auto_dict` behavior.
        Materialized values are cached so subsequent lookups are O(1).
      - Eager Conversion: Calling `.to_dict()` recursively evaluates all fields
        and child mappings into a standard Python `dict`.
      - Equality: Compares equal (`==`) to standard Python `dict`s with identical
        contents.

    Example:
      >>> msg = parse_lazy('command: { args: "echo" } status: OK')
      >>> msg["status"][0].text
      'OK'
      >>> # The "command" submessage body is only parsed if accessed:
      >>> msg["command"][0]["args"][0].text
      '"echo"'
    """

    def __init__(
        self,
        text: str,
        start: int = 0,
        end: int | None = None,
        top: bool = False,
    ):
        self._text = text
        self._start = start
        self._end = len(text) if end is None else end
        self._top = top
        self._fields: dict[str, list[_FieldEntry]] | None = None
        self._cache: dict[str, Any] = {}

    def _index(self) -> dict[str, list[_FieldEntry]]:
        if self._fields is None:
            self._fields = _index_fields(
                self._text, self._start, self._end, top=self._top
            )
        return self._fields

    def __getitem__(self, key: str) -> Any:
        # Return already-materialized field from cache if available.
        if key in self._cache:
            return self._cache[key]

        # Trigger shallow indexing of direct fields on first access.
        fields = self._index()
        if key not in fields:
            raise KeyError(key)

        # Materialize only the requested field's entries:
        # - Submessages are wrapped in child LazyMapping instances without
        #   parsing their internal contents.
        # - Scalars are converted into Token instances.
        entries = fields[key]
        values: list[Any] = []
        for entry in entries:
            if entry.is_message:
                values.append(
                    LazyMapping(
                        self._text,
                        start=entry.body_start,
                        end=entry.body_end,
                        top=False,
                    )
                )
            else:
                values.append(
                    Token(text=entry.value_text, type=entry.token_type)
                )

        # Convert repeated key-value pairs (e.g. output_file_digests, platform)
        # to dictionaries, matching eager parse() behavior.
        result = _auto_dict(values)
        self._cache[key] = result
        return result

    def __iter__(self) -> Iterator[str]:
        return iter(self._index())

    def __len__(self) -> int:
        return len(self._index())

    def __contains__(self, key: object) -> bool:
        return key in self._index()

    def __repr__(self) -> str:
        return f"LazyMapping({dict(self.items())})"

    def __eq__(self, other: Any) -> bool:
        """Compares this mapping with another mapping for equality.

        Note:
          Unless identity (`other is self`) or length checks fail early, equality
          comparison fully materializes this mapping (and any child LazyMappings)
          into standard Python dicts to perform deep recursive comparison. This is
          primarily intended for unit tests and assertions; avoid using equality
          checks on large LazyMappings in performance-critical hot paths.
        """
        if other is self:
            return True
        if not isinstance(other, Mapping) or len(self) != len(other):
            return False
        return dict(self.items()) == dict(other.items())

    def to_dict(self) -> dict[str, Any]:
        """Recursively converts this LazyMapping into a standard Python dict."""

        def _conv(v: Any) -> Any:
            if isinstance(v, LazyMapping):
                return v.to_dict()
            if isinstance(v, list):
                return [_conv(item) for item in v]
            if isinstance(v, dict):
                return {k: _conv(val) for k, val in v.items()}
            return v

        return {k: _conv(self[k]) for k in self}


def parse_lazy(text: str) -> LazyMapping:
    """Parse a text protobuf lazily into a recursive mapping.

    Blocks and fields are indexed on demand without tokenizing or parsing
    unaccessed submessages.

    Unlike `parse()`, which accepts an `Iterable[str]` of lines, `parse_lazy()`
    operates directly on the complete flat text of the protobuf. Callers reading
    from disk should pass `Path(...).read_text()` or `f.read()` to avoid the
    memory allocation and line-splitting overhead of `readlines()`.

    Args:
      text: Complete text of the text protobuf (e.g. `Path.read_text()`).

    Returns:
      Lazy mapping representation of the text proto. Fields are treated as
      repeated (lists) or as key-value dictionaries.
    """
    return LazyMapping(text, start=0, end=len(text), top=True)


def _main_arg_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Parse any text proto and show its representation.",
        argument_default=None,
    )
    parser.add_argument(
        "input",
        type=Path,
        metavar="FILE",
        help="The text proto file to parse",
    )
    return parser


_MAIN_ARG_PARSER = _main_arg_parser()


def main(argv: Sequence[str]) -> int:
    args = _MAIN_ARG_PARSER.parse_args(argv)
    with open(args.input) as f:
        data = parse(f)
    print(data)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
