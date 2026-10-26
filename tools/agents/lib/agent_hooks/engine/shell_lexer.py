# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Lexing and splitting utilities for compound shell commands.

Provides lightweight shell command decomposition for agent hook policy checks,
including heredoc body removal, compound operator splitting (`&&`, `||`, `;`,
`|`, `&`, newlines), I/O redirection stripping, and leading environment/wrapper
stripping.
"""

import codecs
import re
import shlex
from collections.abc import Sequence
from pathlib import Path

_ENV_REGEX = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*=.*$")
_WRAPPER_PROGS: frozenset[str] = frozenset(
    "env command nice nohup time stdbuf xargs eval exec watch".split()
)
_WRAPPER_ARG_FLAGS: frozenset[str] = frozenset(
    "-u --unset -C --chdir -n --adjustment --max-args --max-procs "
    "--max-lines -I -L -P -s -d -a -E".split()
)
_SHELL_KEYWORDS: frozenset[str] = frozenset(
    "do then else elif if while until done fi ! { }".split()
)
# Group 1: attached prefix before operator; Group 2: inline target after operator.
_REDIR_RE = re.compile(
    r"^([^\s<>]*?)(?:\d*(?:>>?|<<<|<<-?|<&?|>&)|&>>?|<>&?)(.*)$"
)
# Matches quoted strings/here-strings (ignored) or << / <<- heredoc headers
# (Group 1: '-', Groups 2-4: quoted/escaped delimiter, Group 5: unquoted).
_HEREDOC_RE = re.compile(
    r"""(?:'[^']*'|"[^"]*"|<<<)|"""
    r"""<<(-?)\s*(?:'([^']*)'|"([^"]*)"|\\([^\s;&|<>'"]+)|([^\s;&|<>'"]+))"""
)
_QUOTED_RE = r"""'[^']*(?:'|$)|"(?:\\[\s\S]|\$\([^()]*\)|[^"\\])*(?:"|$)"""
_SUB_RE = r"\$\((?!\()((?:[^()]|\([^()]*\))*)\)|`((?:\\[\s\S]|[^`\\])*)`"
# Group 1 captures quotes ($'...', '...', "..."), command substitutions, line
# continuations, or escapes; the ungrouped branch matches '#' comments.
_COMMENT_OR_CONT_RE = re.compile(
    rf"""(\$'(?:\\[\s\S]|[^'\\])*(?:'|$)|{_QUOTED_RE}|{_SUB_RE}|\\(?:\r?\n)|\\[\s\S])"""
    r"""|(?:^|(?<=[\s;&|()]))#[^\n]*"""
)
# Matches maximal command chunks between unquoted ;, &, |, (, ), `, or newline.
_CMD_CHUNK_RE = re.compile(
    rf"""(?:{_QUOTED_RE}|\\[\s\S]|\\$|[<>]&|&>>?|[^'"\\;&|()`\n])+"""
)
# Captures $(...) in Group 1 (excluding $((...)) arithmetic) and `...` in Group 2.
_DQ_SUB_RE = re.compile(rf"\\[\s\S]|{_SUB_RE}")
# Matches escaped backslashes (Group 1) or unescaped line continuations in "...".
_DQ_CONT_RE = re.compile(r"(\\\\)|\\\r?\n")
# Captures find -exec/-execdir/-ok/-okdir flag in Group 1 and command in Group 2.
_FIND_EXEC_RE = re.compile(
    r"""'[^']*'|"(?:\\[\s\S]|[^"\\])*"|\\[\s\S]|"""
    r"""(?<=\s)(-(?:exec|ok)(?:dir)?)\s+"""
    r"""((?:\\[\s\S]|'[^']*'|"(?:\\[\s\S]|[^"\\])*"|[^'"\\;&|()\n])+?)\s+"""
    r"""(?:\\;|';'|";"|(?:\{\}\s+)?(?:\\?\+|'\+'|"\+"))(?=\s|[;&|()\n]|$)"""
)


def _extract_substitutions(text: str) -> list[str]:
    """Extract `$(...)` and `` `...` `` command substitutions from `text`."""
    subs: list[str] = []
    for match in _DQ_SUB_RE.finditer(text):
        sub_cmd = match[1] or match[2]
        if sub_cmd:
            subs.append(sub_cmd)
    return subs


def _normalize_shell(cmd_str: str) -> tuple[str, list[str]]:
    """Fold line continuations, decode `$'...'` quotes, and strip comments.

    Also collects any `$(...)` or `` `...` `` command substitutions embedded
    inside double-quoted strings so they can be audited alongside top-level
    commands.
    """
    quoted_subs: list[str] = []

    def _repl(match: re.Match[str]) -> str:
        token = match[1]
        if token is None or token in ("\\\n", "\\\r\n"):
            return ""
        if token.startswith("$'") and token.endswith("'"):
            try:
                decoded = codecs.decode(token[2:-1], "unicode_escape")
                return shlex.quote(decoded)
            except ValueError:
                return shlex.quote(token[2:-1])
        if token.startswith("$(") or token.startswith("`"):
            quoted_subs.extend(_extract_substitutions(token))
            return "'__sub__'"
        if token.startswith('"'):
            token = _DQ_CONT_RE.sub(lambda m: m[1] or "", token)
            if "$(" in token or "`" in token:
                quoted_subs.extend(_extract_substitutions(token[1:-1]))
        return token

    return _COMMENT_OR_CONT_RE.sub(_repl, cmd_str), quoted_subs


def strip_heredocs(cmd_str: str) -> str:
    """Strip heredoc body lines while preserving command header lines.

    Scans `cmd_str` for unquoted `<<` or `<<-` heredoc operators and removes
    subsequent body lines up through the matching closing delimiter line so
    multiline script payloads (such as `python3 - <<'EOF'`) are not lexed as
    shell commands. For unquoted heredoc delimiters (`<<EOF`), embedded `$(...)`
    and `` `...` `` command substitutions inside the body are still extracted.

    Args:
        cmd_str: Raw shell command string potentially containing heredocs.

    Returns:
        Command string with heredoc bodies and closing delimiter lines removed.
    """
    if "<<" not in cmd_str:
        return cmd_str
    out_lines: list[str] = []
    pending: list[tuple[str, bool, bool]] = []
    for line in cmd_str.splitlines():
        if pending:
            delim, strip_tabs, unquoted = pending[0]
            candidate = line.lstrip("\t") if strip_tabs else line
            if candidate == delim:
                pending.pop(0)
            elif unquoted:
                out_lines.extend(_extract_substitutions(line))
            continue
        out_lines.append(line)
        for match in _HEREDOC_RE.finditer(line):
            delim = match[2] or match[3] or match[4] or match[5]
            if delim is not None:
                is_strip_tabs = match[1] == "-"
                is_unquoted = match[5] is not None
                pending.append((delim, is_strip_tabs, is_unquoted))
    return "\n".join(out_lines)


def split_compound_commands(cmd_str: str) -> list[str]:
    """Split command string on `&&`, `||`, `;`, `|`, `&`, `(`, `)`, and newlines.

    Strips heredoc bodies and normalizes continuations/comments first, splits
    outside quotes while preserving redirections (`2>&1`, `&>/dev/null`), and
    extracts inner commands from `"$(...)"` substitutions and `find -exec`.

    Args:
        cmd_str: Full compound shell command string.

    Returns:
        List of individual trimmed subcommand strings.
    """
    cleaned, extra_cmds = _normalize_shell(strip_heredocs(cmd_str))
    if "-exec" in cleaned or "-ok" in cleaned:
        exec_cmds: list[str] = []

        def _exec_repl(match: re.Match[str]) -> str:
            if match[1]:
                exec_cmds.append(match[2])
                return f"{match[1]} ''"
            return match[0]

        cleaned = _FIND_EXEC_RE.sub(_exec_repl, cleaned)
        extra_cmds = exec_cmds + extra_cmds
    if extra_cmds:
        cleaned = f"{cleaned}\n" + "\n".join(extra_cmds)

    chunks: list[str] = []
    for match in _CMD_CHUNK_RE.finditer(cleaned):
        chunk = match.group(0).strip()
        if chunk:
            chunks.append(chunk)
    return chunks


def strip_redirections(tokens: Sequence[str]) -> list[str]:
    """Remove shell redirection tokens from a tokenized command.

    Strips output/input redirections (`>`, `>>`, `<`, `2>&1`, `&>/dev/null`),
    heredoc operators (`<<EOF`, `<<-EOF`), and here-strings (`<<<word`), along
    with their target token when separated by whitespace.

    Args:
        tokens: Sequence of shell tokens for a single subcommand.

    Returns:
        Token list with redirection operators and targets removed.
    """
    result: list[str] = []
    i = 0
    while i < len(tokens):
        match = _REDIR_RE.match(tokens[i])
        if match:
            if match[1]:
                result.append(match[1])
            # If the redirection target was attached to the operator (`>out`),
            # advance by 1 token; otherwise skip the separate target token too.
            i += 1 if match[2] else 2
        else:
            result.append(tokens[i])
            i += 1
    return result


def strip_env_assignments(tokens: Sequence[str]) -> list[str]:
    """Strip leading `KEY=VALUE` assignments, shell keywords, and wrappers.

    Unwraps prefixes such as `FOO=1`, shell control-flow keywords (`do`, `then`,
    `!`, `{`), and command wrappers (`env`, `command`, `nice`, `nohup`, `time`,
    `stdbuf`, `xargs`, `eval`, `exec`, `watch`) to expose the underlying
    executable token.

    Args:
        tokens: Sequence of shell tokens for a single subcommand.

    Returns:
        Token list starting at the underlying command executable, or an empty
        list if `tokens` only contained environment assignments.
    """
    idx = 0
    n = len(tokens)
    while idx < n:
        tok = tokens[idx]
        if _ENV_REGEX.match(tok) or tok in _SHELL_KEYWORDS:
            idx += 1
            continue

        prog_name = Path(tok).name
        if prog_name in _WRAPPER_PROGS and idx + 1 < n:
            next_idx = idx + 1
            while (
                next_idx < n
                and tokens[next_idx].startswith("-")
                and tokens[next_idx] != "-"
            ):
                flag_tok = tokens[next_idx]
                if flag_tok == "--":
                    next_idx += 1
                    break
                # `command -v <prog>` and `command -V <prog>` only inspect PATH
                # and do not execute `<prog>`.
                if (
                    prog_name == "command"
                    and not flag_tok.startswith("--")
                    and ("v" in flag_tok[1:] or "V" in flag_tok[1:])
                ):
                    return list(tokens[idx:])
                next_idx += 2 if flag_tok in _WRAPPER_ARG_FLAGS else 1
            idx = next_idx
        else:
            break

    out = list(tokens[idx:])
    while out and out[-1] == "}":
        out.pop()
    if len(out) == 1 and any(c.isspace() for c in out[0]):
        return tokenize_shell(out[0])
    return out


def tokenize_shell(cmd: str) -> list[str]:
    """Tokenize a shell command string with a whitespace-split fallback.

    Args:
        cmd: Single shell subcommand string.

    Returns:
        List of parsed tokens from `shlex.split`, falling back to `norm.split()`
        if `shlex.split` raises `ValueError` (e.g. unclosed quotes).
    """
    norm, _ = _normalize_shell(cmd)
    try:
        return shlex.split(norm)
    except ValueError:
        return norm.split()
