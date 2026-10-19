# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""
Library to normalize clang and lld command arguments for C, C++, Assembly, and Link actions.
"""

import os
import shlex

import path_normalizer

# Action types supported for normalization.
ACTION_C_COMPILE = "c_compile"
ACTION_CPP_COMPILE = "cpp_compile"
ACTION_ASSEMBLE = "assemble"
ACTION_CPP_LINK = "cpp_link"

ALL_ACTION_TYPES = {
    ACTION_C_COMPILE,
    ACTION_CPP_COMPILE,
    ACTION_ASSEMBLE,
    ACTION_CPP_LINK,
}

# List of whole arguments to ignore when comparing GN and Bazel commands.
_ARGS_TO_IGNORE = set(
    [
        # Output and compilation control
        "-c",
        # Dependency files
        "-MD",
        "-MMD",
        # Diagnostic / formatting options
        "-fcolor-diagnostics",
        "-fno-color-diagnostics",
        "-Wl,--color-diagnostics",
        # RBE / Remote execution flags
        "--remote-only",
        "--local-only",
    ]
)

# List of arg prefixes to ignore when comparing GN and Bazel commands.
_ARG_PREFIXES_TO_IGNORE = (
    # Output and compilation control
    "-c",
    "-o=",
    "--output=",
    # Dependency files
    "-MF=",
    "-MT=",
    "-MQ=",
    # Linker map and dependency files
    "-Wl,--Map=",
    "-Wl,-Map=",
    "-Wl,--dependency-file=",
    "-Wl,-dependency-file=",
    # Driver mode flags added explicitly by Bazel
    "--driver-mode=",
    # Environment variables
    "PATH=",
    "PWD=",
    "TMPDIR=",
    # RBE / Remote execution flags
    "--remote-flag=",
    "--remote-worker-pool=",
    # Crash diagnostic / reproducers
    "-fcrash-diagnostics-dir=",
    # ThinLTO cache directory is used by GN/Ninja for local incremental builds,
    # whereas Bazel intentionally omits it as it is incompatible with action
    # sandboxing and remote execution.
    "-Wl,--thinlto-cache-dir=",
)


def _normalize_path(
    path: str, normalizer: path_normalizer.PathNormalizer
) -> str:
    """Normalize a path using the normalizer if available, or fallback to relative stripping."""
    try:
        return normalizer.normalize_path(path)
    except ValueError:
        return path


_PREFIX_MAP_OPTIONS = {
    "-ffile-prefix-map",
    "-fdebug-prefix-map",
    "-fmacro-prefix-map",
    "-fcoverage-prefix-map",
    "-fprofile-prefix-map",
    "-ffile-compilation-dir",
    "--remap-path-prefix",
}


def normalize_prefix_map_flag(
    arg: str, normalizer: path_normalizer.PathNormalizer
) -> tuple[bool, str]:
    """Normalize a prefix-map flag into a canonical format.

    Supports:
      - -ffile-prefix-map=from=to
      - -fdebug-prefix-map=from=to
      - -fmacro-prefix-map=from=to
      - -fcoverage-prefix-map=from=to
      - -fprofile-prefix-map=from=to
      - -ffile-compilation-dir=dir
      - --remap-path-prefix=from=to
    """
    option, equal, value = arg.partition("=")
    if not equal:
        return False, arg

    if option not in _PREFIX_MAP_OPTIONS:
        return False, arg

    from_path, equal, to_path = value.partition("=")
    if equal:
        normalized_from = _normalize_path(from_path, normalizer)
        normalized_to = _normalize_path(to_path, normalizer)
        value = f"{normalized_from}={normalized_to}"
    else:
        value = _normalize_path(value, normalizer)

    return True, f"{option}={value}"


def normalize_clang_arg(
    arg: str,
    normalizer: path_normalizer.PathNormalizer,
    action_type: str = ACTION_CPP_COMPILE,
) -> str:
    """Normalize a single clang or lld argument.

    Args:
        arg: The argument string to normalize.
        action_type: One of c_compile, cpp_compile, assemble, cpp_link.
        normalizer: PathNormalizer instance.

    Returns:
        The normalized argument string, or empty string if it should be ignored.
    """
    if not arg:
        return ""

    if arg in _ARGS_TO_IGNORE:
        return ""

    if arg.startswith(_ARG_PREFIXES_TO_IGNORE):
        return ""

    # Normalize prefix-map / determinism flags
    if arg.startswith("-"):
        changed, arg = normalize_prefix_map_flag(arg, normalizer)
        if changed:
            return arg

        # Normalize include flags: -Ipath, -isystem path, -iquote path
        for inc_prefix in ("-I", "-isystem", "-iquote", "-idirafter"):
            if arg.startswith(inc_prefix):
                val = arg[len(inc_prefix) :]
                val = _normalize_path(val, normalizer)
                return f"{inc_prefix}{val}"

        # Normalize library search dirs: -Lpath, -Wl,-Lpath
        if arg.startswith("-L"):
            val = _normalize_path(arg[2:], normalizer)
            return f"-L{val}"
        if arg.startswith("-Wl,-L"):
            val = _normalize_path(arg[6:], normalizer)
            return f"-L{val}"

        # Normalize sysroot / target prefixes
        if arg.startswith("--sysroot="):
            val = _normalize_path(arg[len("--sysroot=") :], normalizer)
            return f"--sysroot={val}"

    # Normalize tool names
    else:  # not arg.startswith("-"):
        base = os.path.basename(arg)
        if base in (
            "clang",
            "clang++",
            "ld.lld",
            "lld",
            "llvm-ar",
            "llvm-strip",
        ):
            return base

        # Filter out intermediate object files in link commands
        if action_type == ACTION_CPP_LINK and arg.endswith((".o", ".obj")):
            return ""

        # Normalize source file paths
        if action_type in (
            ACTION_C_COMPILE,
            ACTION_CPP_COMPILE,
            ACTION_ASSEMBLE,
        ):
            if any(
                arg.endswith(ext)
                for ext in (
                    ".c",
                    ".cc",
                    ".cpp",
                    ".cxx",
                    ".C",
                    ".S",
                    ".s",
                    ".asm",
                )
            ):
                return _normalize_path(arg, normalizer)

        # Ignore raw output/temporary paths
        if (
            arg.startswith("obj/")
            or arg.startswith("bazel-out/")
            or arg.startswith("out/")
        ):
            return ""

        # Normalize any other file paths
        return _normalize_path(arg, normalizer)

    return arg


def normalize_clang_cmd(
    cmd: str,
    normalizer: path_normalizer.PathNormalizer,
    action_type: str = ACTION_CPP_COMPILE,
) -> list[str]:
    """Normalize a full clang / lld command.

    Args:
        cmd: The raw command string.
        action_type: One of c_compile, cpp_compile, assemble, cpp_link.
        normalizer: Optional PathNormalizer instance.

    Returns:
        A sorted list of unique normalized arguments.
    """
    # Pre-split fixups for flags where value may follow space (e.g. `-o foo`, `-MF bar`)
    tokens = shlex.split(cmd)
    merged_tokens: list[str] = []
    skip_next = False

    flags_with_values = {
        "-o": "-o=",
        "-MF": "-MF=",
        "-MT": "-MT=",
        "-MQ": "-MQ=",
        "-I": "-I",
        "-L": "-L",
        "-isystem": "-isystem",
        "-iquote": "-iquote",
        "-idirafter": "-idirafter",
        "--sysroot": "--sysroot=",
        "--target": "--target=",
    }

    for i, token in enumerate(tokens):
        if skip_next:
            skip_next = False
            continue

        if token in flags_with_values and i + 1 < len(tokens):
            merged_tokens.append(f"{flags_with_values[token]}{tokens[i + 1]}")
            skip_next = True
        elif (
            token.startswith("-o")
            and len(token) > 2
            and not token.startswith("-o=")
        ):
            merged_tokens.append(f"-o={token[2:]}")
        else:
            merged_tokens.append(token)

    normalized_args = []
    for t in merged_tokens:
        norm = normalize_clang_arg(
            t, normalizer=normalizer, action_type=action_type
        )
        if norm:
            normalized_args.append(norm)

    return sorted(set(normalized_args))
