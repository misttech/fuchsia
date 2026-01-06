# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import shlex
import typing as T

# Default separators to split shell commands by.
_DEFAULT_SPLIT_SEPARATORS = {"&&", "||"}


def _split_by_separators(
    cmd_str: str, separators: set[str] = _DEFAULT_SPLIT_SEPARATORS
) -> list[str]:
    """Splits the command string by unquoted separators.

    Returns a list of strings (the chunks) and separators.

    Example:
        _split_by_separators("echo a && echo b") == ['echo a', '&&', 'echo b']
    """

    # Use shlex in non-posix mode to preserve quotes, ensuring we can distinguish
    # '&&' (quoted string) from && (operator).
    #
    # punctuation_chars=True makes it split even without whitespace (e.g. a&&b).
    lexer = shlex.shlex(cmd_str, posix=False, punctuation_chars=True)
    lexer.whitespace_split = True

    chunks = []
    current_chunk: list[str] = []

    for token in lexer:
        if token in separators:
            if current_chunk:
                chunks.append(" ".join(current_chunk))
                current_chunk = []
            chunks.append(token)
        else:
            current_chunk.append(token)

    if current_chunk:
        chunks.append(" ".join(current_chunk))

    return chunks


class ShellCommand:
    """Represents a shell command."""

    def __init__(self, command: T.Union[str, list[str]]):
        if isinstance(command, list):
            self._str = shlex.join(command)
        else:
            self._str = command

    def __str__(self) -> str:
        return self._str

    def __repr__(self) -> str:
        return f"ShellCommand({self._str!r})"

    def __eq__(self, other: object) -> bool:
        if not isinstance(other, ShellCommand):
            return NotImplemented
        return self._str == other._str

    def split(
        self, separators: set[str] = _DEFAULT_SPLIT_SEPARATORS
    ) -> list["ShellCommand"]:
        """Splits the command by top-level && and || separators.

        Returns separated subcommands as ShellCommand objects. Separators are dropped.
        """
        chunks = _split_by_separators(self._str, separators)
        result: list["ShellCommand"] = []
        for chunk in chunks:
            if chunk in separators:
                continue
            else:
                result.append(ShellCommand(chunk))
        return result

    def unwrap(self) -> "ShellCommand":
        """Unwraps the command by stripping the first wrapper script.

        It looks for the first occurrence of `--` (surrounded by spaces)
        and returns everything after it.
        """
        tokens = shlex.split(self._str)

        try:
            idx = tokens.index("--")
        except ValueError:
            raise ValueError(f"No '--' separator found in command: {self._str}")

        inner_tokens = tokens[idx + 1 :]
        if not inner_tokens:
            return ShellCommand("")

        return ShellCommand(inner_tokens)
