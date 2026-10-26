# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Declarative command schema and parser definitions."""

import argparse
from collections.abc import Sequence
from dataclasses import dataclass
from typing import Any, ClassVar, Never

_FALSY_FLAG_VALUES: frozenset[str] = frozenset(
    {"0", "false", "never", "no", "none"}
)


@dataclass(frozen=True)
class Flag:
    """Declarative specification of a command line option flag.

    Attributes:
        flags: Option strings (e.g. `("-f", "--force")`).
        action: `argparse` action (e.g. `"store_true"`, `"append"`, or `None`).
        nargs: Number of arguments consumed (e.g. `"?"` for optional value).
    """

    flags: Sequence[str]
    action: str | None = None
    nargs: int | str | None = None


@dataclass(frozen=True)
class Subcommand:
    """Declarative specification of a CLI subcommand and its nested hierarchy.

    Attributes:
        name: Subcommand token name (e.g. `"clean"`, `"target"`).
        flags: Option flags specific to this subcommand.
        subcommands: Nested subcommands under this subcommand.
    """

    name: str
    flags: Sequence[Flag] = ()
    subcommands: Sequence[Subcommand] = ()


class _SchemaNamespace(argparse.Namespace):
    """Namespace subclass tracking the resolved CLI subcommand path."""

    __slots__ = ("subcommand_path",)
    subcommand_path: tuple[str, ...]

    def __init__(self) -> None:
        super().__init__()
        self.subcommand_path = ()


class SchemaArgumentParser(argparse.ArgumentParser):
    """Non-exiting ArgumentParser tailored for declarative CommandSchema trees.

    Extends `argparse.ArgumentParser` with four behaviors required for static
    command auditing:
    - Raises `ValueError` on parse errors instead of printing to stderr or
      exiting the process.
    - Dispatches subcommands lazily via `_LazySubParsersAction`, tolerating
      unrecognized subcommands and flags after positional arguments.
    - Allows value-taking options (e.g. `grep -e`) to consume dash-prefixed
      arguments (e.g. `-v`) without rejecting them as unexpected options.
    - Prevents `nargs="?"` options (e.g. `sed -i`) from greedily consuming the
      next positional operand, and normalizes inline `=true`/`=false` booleans.
    """

    def __init__(self, *args: Any, **kwargs: Any) -> None:
        super().__init__(*args, **kwargs)
        self.register("action", "parsers", _LazySubParsersAction)

    def parse_known_args(  # type: ignore[override]
        self,
        args: Sequence[str] | None = None,
        namespace: _SchemaNamespace | None = None,
    ) -> tuple[_SchemaNamespace, list[str]]:
        return super().parse_known_args(
            args, _SchemaNamespace() if namespace is None else namespace
        )

    def _check_value(self, action: argparse.Action, value: Any) -> None:
        if isinstance(action, argparse._SubParsersAction):
            return
        super()._check_value(action, value)

    def _match_argument(
        self, action: argparse.Action, arg_strings_pattern: str
    ) -> int:
        # Treat dash-prefixed tokens (e.g. `-C -v`) as the argument to a
        # value-taking option rather than rejecting them as another option.
        if (
            action.option_strings
            and action.nargs in (None, 1)
            and arg_strings_pattern.startswith("O")
        ):
            return 1
        return super()._match_argument(action, arg_strings_pattern)

    def _parse_optional(self, arg_string: str) -> Any:
        # Prevent optional-value flags (nargs="?") from greedily consuming the
        # next positional token, and handle inline `=bool` on 0-arg flags.
        res = super()._parse_optional(arg_string)
        if res and len(res) == 1:
            action, option_string, sep, explicit_arg = res[0]
            if action is not None:
                if action.nargs == "?" and explicit_arg is None:
                    return [(action, option_string, "=", "")]
                if action.nargs == 0:
                    if sep == "=" and explicit_arg is not None:
                        if explicit_arg.lower() in _FALSY_FLAG_VALUES:
                            return [(None, option_string, None, None)]
                        return [(action, option_string, None, None)]
                    if sep == "" and explicit_arg:
                        prefix = option_string[0]
                        for i, ch in enumerate(explicit_arg):
                            sub = self._option_string_actions.get(prefix + ch)
                            if sub is None or sub.nargs != 0:
                                if (
                                    sub is not None
                                    and sub.nargs == "?"
                                    and i == len(explicit_arg) - 1
                                ):
                                    return [
                                        (
                                            action,
                                            option_string,
                                            sep,
                                            explicit_arg + "=",
                                        )
                                    ]
                                break
        return res

    def error(self, message: str) -> Never:
        """Raise `ValueError` with the error message instead of exiting."""
        raise ValueError(message)


class _LazySubParsersAction(
    argparse._SubParsersAction  # type: ignore[type-arg]
):
    """On-demand subparser dispatcher that tolerates unknown subcommands."""

    def __init__(
        self,
        option_strings: Sequence[str],
        prog: str,
        schema_cls: type[CommandSchema],
        path: tuple[str, ...],
        subs: Sequence[Subcommand],
        **kwargs: Any,
    ) -> None:
        kwargs.setdefault("parser_class", SchemaArgumentParser)
        kwargs.setdefault("dest", argparse.SUPPRESS)
        super().__init__(option_strings=option_strings, prog=prog, **kwargs)
        self._schema_cls = schema_cls
        self._path = path
        self._subs = {s.name: s for s in subs}

    def __call__(
        self,
        parser: argparse.ArgumentParser,
        namespace: _SchemaNamespace,  # type: ignore[override]
        values: Any,
        option_string: str | None = None,
    ) -> None:
        if not values:
            return
        parser_name = values[0]

        if parser_name in self._subs:
            # 1. Known subcommand: record the subcommand path and recursively
            # parse remaining tokens with the subcommand's parser.
            sub_path = (*self._path, parser_name)
            subparser = self._schema_cls.get_parser(sub_path)
            sub_ns, arg_strings = subparser.parse_known_args(values[1:], None)
            namespace.subcommand_path = sub_ns.subcommand_path or sub_path
            extra_out = list(arg_strings)
        elif not self._path and self._schema_cls.strict_global_prefix:
            # 2. Unknown root token under strict_global_prefix (e.g. `fx`):
            # global flags are only valid before the subcommand, so leave all
            # remaining tokens unrecognized.
            sub_ns = _SchemaNamespace()
            extra_out = list(values)
        elif parser_name.startswith("-") and len(parser_name) > 1:
            # 3. Unrecognized flag where a subcommand was expected: prefix with
            # `--` so `argparse` stops scanning and `parse_tokens` records it
            # in `unknown_flags`.
            sub_ns = _SchemaNamespace()
            extra_out = ["--", *values]
        else:
            # 4. Positional operand before trailing flags (e.g. `git push
            # origin --force`): re-parse remaining tokens against the current
            # level without subparsers so trailing flags are still captured.
            leaf = self._schema_cls.get_parser(
                self._path, _with_subparsers=False
            )
            sub_ns, arg_strings = leaf.parse_known_args(values[1:], None)
            extra_out = [parser_name, *arg_strings]

        # Merge parsed flags from the delegated parse back into `namespace`,
        # concatenating `append`-action lists when present at both levels.
        for key, value in vars(sub_ns).items():
            if hasattr(namespace, key):
                cur = getattr(namespace, key)
                if isinstance(cur, list) and isinstance(value, list):
                    setattr(namespace, key, cur + value)
                    continue
            setattr(namespace, key, value)

        if extra_out:
            vars(namespace).setdefault(argparse._UNRECOGNIZED_ARGS_ATTR, [])
            getattr(namespace, argparse._UNRECOGNIZED_ARGS_ATTR).extend(
                extra_out
            )


class CommandSchema:
    """Declarative base class for CLI command schemas.

    Attributes:
        prog: Canonical executable name (e.g. `"git"`, `"fx"`).
        strict_global_prefix: When `True`, global flags are only recognized
            before the subcommand token (as required by `fx`).
        global_flags: Top-level flags accepted by the command.
        subcommands: Top-level subcommand specifications.
    """

    prog: str = ""
    strict_global_prefix: bool = False
    global_flags: Sequence[Flag] = ()
    subcommands: Sequence[Subcommand] = ()
    _parser_cache: ClassVar[dict[tuple[Any, ...], SchemaArgumentParser]] = {}

    @classmethod
    def get_parser(
        cls,
        subcommand_path: Sequence[str] = (),
        *,
        _with_subparsers: bool = True,
    ) -> SchemaArgumentParser:
        """Return a cached `SchemaArgumentParser` for the given subcommand path.

        Args:
            subcommand_path: Sequence of active subcommand names from root.
            _with_subparsers: Internal toggle to omit nested subparsers when
                re-parsing leaf flags after an unknown positional token.

        Returns:
            Configured `SchemaArgumentParser` instance for the requested
            hierarchy level.
        """
        path_tuple = tuple(subcommand_path)
        key = (cls, path_tuple, _with_subparsers)
        if key not in cls._parser_cache:
            prog = " ".join((cls.prog, *path_tuple)).strip()
            parser = SchemaArgumentParser(
                prog=prog,
                add_help=False,
                allow_abbrev=False,
                conflict_handler="resolve",
            )
            layers: list[Sequence[Flag]] = []
            if not (path_tuple and cls.strict_global_prefix):
                layers.append(cls.global_flags)

            subs = cls.subcommands
            for part in path_tuple:
                matched_sub = next(s for s in subs if s.name == part)
                layers.append(matched_sub.flags)
                subs = matched_sub.subcommands

            for layer in layers:
                for flag_spec in layer:
                    kwargs: dict[str, Any] = {"default": argparse.SUPPRESS}
                    if flag_spec.action is not None:
                        kwargs["action"] = flag_spec.action
                    if flag_spec.nargs is not None:
                        kwargs["nargs"] = flag_spec.nargs
                    parser.add_argument(*flag_spec.flags, **kwargs)

            if _with_subparsers and (
                subs or (not path_tuple and cls.strict_global_prefix)
            ):
                parser.add_subparsers(  # type: ignore[call-overload]
                    schema_cls=cls,
                    path=path_tuple,
                    subs=subs,
                )
            cls._parser_cache[key] = parser
        return cls._parser_cache[key]

    @classmethod
    def parse_tokens(
        cls,
        cmd_tokens: Sequence[str],
        *,
        allow_dangling_flag_value: bool = False,
    ) -> CommandMatch | None:
        """Parse tokenized command arguments against this schema.

        Args:
            cmd_tokens: Sequence of command tokens starting with executable.
            allow_dangling_flag_value: When `True` (used for permission rule
                definitions like `command(git config -f)`), retry with an empty
                string argument if a trailing flag expects a value.

        Returns:
            Populated `CommandMatch`, or `None` if argument syntax is invalid.
        """
        args = list(cmd_tokens[1:])
        try:
            ns, extra = cls.get_parser().parse_known_args(args)
        except ValueError:
            if allow_dangling_flag_value and args and args[-1].startswith("-"):
                try:
                    ns, extra = cls.get_parser().parse_known_args([*args, ""])
                except ValueError:
                    return None
            else:
                return None

        unknown_flags: list[str] = []
        positionals: list[str] = []
        after_double_dash = False
        for i, token in enumerate(extra):
            if not after_double_dash and token == "--":
                after_double_dash = True
            elif (
                not after_double_dash
                and token.startswith("-")
                and len(token) > 1
            ):
                unknown_flags.append(token)
                # Also record `--flag=val` when followed by a non-flag token so
                # space-separated `--flag val` matches `--flag=val` rules.
                if (
                    token.startswith("--")
                    and "=" not in token
                    and i + 1 < len(extra)
                    and not extra[i + 1].startswith("-")
                ):
                    unknown_flags.append(f"{token}={extra[i + 1]}")
            else:
                positionals.append(token)

        return CommandMatch(
            flags=vars(ns),
            unknown_flags=tuple(unknown_flags),
            positionals=tuple(positionals),
            subcommand_path=ns.subcommand_path,
        )


@dataclass(frozen=True)
class CommandMatch:
    """Parsed command invocation produced by `CommandSchema.parse_tokens`.

    Attributes:
        flags: Mapping of recognized flag `dest` names to parsed values.
        unknown_flags: Unrecognized flag tokens preceding any `--` separator.
        positionals: Positional arguments (including all tokens after `--`).
        subcommand_path: Resolved subcommand hierarchy tuple.
    """

    flags: dict[str, Any]
    unknown_flags: tuple[str, ...] = ()
    positionals: tuple[str, ...] = ()
    subcommand_path: tuple[str, ...] = ()

    def matches_flags_of(self, required: CommandMatch) -> bool:
        """Return `True` if this match satisfies `required` subcommand/flags.

        Verifies that this invocation's subcommand path starts with `required`'s
        subcommand path, all recognized flags and explicit string values in
        `required.flags` are present on `self.flags`, and any unrecognized flags
        in `required.unknown_flags` are present (supporting short-flag bundles).

        Args:
            required: Parsed rule `CommandMatch` whose constraints must be met.

        Returns:
            `True` if `self` includes all subcommands and flags of `required`.
        """
        req_sub = required.subcommand_path
        if req_sub and self.subcommand_path[: len(req_sub)] != req_sub:
            return False
        if not req_sub and required.positionals and self.subcommand_path:
            return False

        for dest, req_val in required.flags.items():
            cand = self.flags.get(dest)
            if cand is None:
                return False
            req_vals = req_val if isinstance(req_val, list) else [req_val]
            cand_vals = cand if isinstance(cand, list) else [cand]
            req_strs = [v for v in req_vals if isinstance(v, str) and v]
            if req_strs:
                if not all(rv in cand_vals for rv in req_strs):
                    return False
            elif (
                isinstance(cand_vals[-1], str)
                and cand_vals[-1].lower() in _FALSY_FLAG_VALUES
            ):
                return False

        short_chars: set[str] = set()
        for tok in self.unknown_flags:
            if not tok.startswith("--"):
                short_chars.update(tok[1:])

        for ftok in required.unknown_flags:
            if ftok.startswith("--"):
                has_exact = ftok in self.unknown_flags
                has_eq_prefix = "=" not in ftok and any(
                    c.startswith(f"{ftok}=") for c in self.unknown_flags
                )
                if not (has_exact or has_eq_prefix):
                    return False
            elif not set(ftok[1:]).issubset(short_chars):
                return False
        return True
