#!/usr/bin/env fuchsia-vendored-python

# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import json
import os
import re
import shlex
import subprocess
import sys
from pathlib import Path
from typing import NoReturn


def eprint(*args, **kwargs) -> None:
    print(*args, file=sys.stderr, **kwargs)


def fail(*args, **kwargs) -> NoReturn:
    eprint("\033[31mError:\033[0m ", end="")
    eprint(*args, **kwargs)
    sys.exit(1)


def warn(*args, **kwargs) -> None:
    eprint("\033[33mWarning:\033[0m ", end="")
    eprint(*args, **kwargs)


def getenv(key: str) -> str:
    value = os.getenv(key)
    if value:
        return value
    fail(
        f"{key} environment variable not set. "
        "Are you running this command from an `fx` environment?",
    )


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Dump disassembly for a binary to a file in the build directory.",
        epilog="""Each BINARY can be the name of the binary file without a directory,
e.g. "libzircon.so" or "zircon.elf"; or the name of a GN target, e.g. "zircon";
or a string of (lowercase) hex characters that's an ELF build ID.

The disassembly will be written to a file next to the binary file in
the build directory, using its name with a ".lst" suffix.""",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument(
        "--line-numbers",
        "-l",
        help="Display source line numbers (the default)",
        action="store_true",
        default=True,
    )
    parser.add_argument(
        "--no-line-numbers",
        help="Do not display source line numbers",
        dest="line_numbers",
        action="store_false",
    )
    parser.add_argument(
        "--source",
        "-S",
        help="Display source interleaved with the disassembly",
        action="store_true",
    )
    parser.add_argument(
        "--no-source",
        help="Do not display source interleaved with the disassembly (the default)",
        dest="source",
        action="store_false",
    )
    parser.add_argument(
        "--reloc",
        "-r",
        help="Display the relocation entries",
        action="store_true",
    )
    parser.add_argument(
        "--no-reloc",
        help="Do not display the relocation entries",
        dest="reloc",
        action="store_false",
    )
    parser.add_argument(
        "--demangle",
        help="Demangle symbol names (the default)",
        action="store_true",
        default=True,
    )
    parser.add_argument(
        "--no-demangle",
        "-n",
        help="Do not demangle symbol names",
        dest="demangle",
        action="store_false",
    )
    parser.add_argument(
        "-G",
        "--gnu",
        action="store_true",
        help="Use GNU objdump rather than llvm-objdump.",
    )
    parser.add_argument(
        "-P",
        "--objdump-path",
        help="Use objdump binary at the specified path.",
    )
    parser.add_argument(
        "-e",
        "--edit",
        help="Automatically open the output disassembly file, provided $EDITOR is set",
        action="store_true",
    )
    parser.add_argument(
        "binary",
        help="Name of the binary, GN target, or ELF build ID.",
    )
    args = parser.parse_args()

    if args.objdump_path:
        objdump_path = Path(args.objdump_path)
    elif args.gnu:
        prebuilt_binutils_dir = getenv("PREBUILT_BINUTILS_DIR")
        objdump_path = Path(prebuilt_binutils_dir) / "bin" / "objdump"
    else:
        prebuilt_clang_dir = getenv("PREBUILT_CLANG_DIR")
        objdump_path = Path(prebuilt_clang_dir) / "bin" / "llvm-objdump"

    if not objdump_path.exists():
        fail(f"no objdump not found at {objdump_path}")

    objdump_args = [objdump_path, "--disassemble"]
    if args.line_numbers:
        objdump_args.append("--line-numbers")
    if args.source:
        objdump_args.append("--source")
    if args.reloc:
        objdump_args.append("--reloc")
    if args.demangle:
        objdump_args.append("--demangle")

    build_dir = Path(getenv("FUCHSIA_BUILD_DIR"))
    (bin_path, bin_label) = find_binary(build_dir, args.binary)

    full_bin_path = build_dir / bin_path
    output_path = full_bin_path.with_suffix(".lst")

    print(f"Disassembling {bin_label}...")
    try:
        with output_path.open("w") as outfile:
            subprocess.run(
                [*objdump_args, bin_path],
                cwd=build_dir,
                stdout=outfile,
                stderr=subprocess.PIPE,
                text=True,
                check=True,
            )
    except FileNotFoundError:
        fail(f"Binary file not found at '{full_bin_path}'")
    except subprocess.CalledProcessError as e:
        fail(f"objdump failed for '{bin_path}': {e.stderr}")

    rel_output_path = output_path.relative_to(Path.cwd())
    print(f"Wrote '{rel_output_path}' for '{bin_label}'")

    if args.edit:
        # $EDITOR may contain other flags like `--wait` supplied in service of
        # `git commit` in one's terminal - but the base editor executable
        # followed by a file should always simply open the file for common
        # editors.
        editor = getenv("EDITOR").split()[0]
        subprocess.run(
            f"{editor} {shlex.quote(str(rel_output_path))}",
            stderr=subprocess.PIPE,
            check=True,
            shell=True,
            text=True,
        )


def find_binary(
    build_dir: Path,
    bin_spec: str,
) -> tuple[str, str]:
    """
    Finds the binary path and label in the build directory based on a binary spec.
    """
    binaries_json = build_dir / "binaries.json"
    if not binaries_json.exists():
        fail(
            f"'{binaries_json}' not found. Please run `fx build` first.",
        )

    with binaries_json.open("r") as f:
        binaries = json.load(f)

    hex_re = re.compile(r"[0-9a-f]+")
    if re.fullmatch(hex_re, bin_spec):
        for binary in binaries:
            if "elf_build_id" not in binary:
                continue
            build_id_path = build_dir / binary["elf_build_id"]
            try:
                with build_id_path.open("r") as f:
                    build_id = f.read().strip()
            except FileNotFoundError:
                # It's possible the build ID file doesn't exist; skip it.
                continue
            if build_id == bin_spec:
                return (binary["dist"], binary["label"])
        fail(f"No binary with build ID '{bin_spec}' found")

    debug_re = re.compile(f"(^|/){bin_spec}(.debug)?$")
    label_re = re.compile(rf":{bin_spec}\(")
    matches = []
    for binary in binaries:
        if "debug" not in binary:
            continue
        if re.search(debug_re, binary["debug"]) or (
            "label" in binary and re.search(label_re, binary["label"])
        ):
            matches.append((binary["dist"], binary["label"]))

    if len(matches) == 0:
        fail(f"'{bin_spec}' did not match any binaries")
    if len(matches) > 1:
        warn(
            f"'{bin_spec}' matched more than one binary (going with the first): "
            f"{json.dumps(matches, indent=2)}",
        )
    return matches[0]


if __name__ == "__main__":
    main()
