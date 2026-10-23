#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import json
import subprocess
import sys
from pathlib import Path


def dict_key_name(record):
    return record["Names"][0]


# Returns a pair with the symbol table index of the named symbol, and the
# symbol table index of the anchor symbol for that same name.
def symbol_indices(symbols: list[dict]) -> dict[str, (int, int)]:
    def is_section(symbol: dict[str, Any]) -> bool:
        return symbol["Symbol"]["Type"]["Name"] == "Section"

    section_symbols: dict[int, int] = {
        symbol["Symbol"]["Section"]["Value"]: index
        for index, symbol in enumerate(symbols)
        if is_section(symbol)
    }

    # There are always section symbols, and usually those act as the anchor
    # symbols.  But there may instead be local label symbols acting as the
    # anchor.  In the test, each function is defined in its own group and thus
    # its own section, so there is only one such label symbol in each section
    # that's always the one used.
    def is_entry_label(symbol: dict[str, Any]) -> bool:
        return (
            symbol["Symbol"]["Type"]["Name"] == "None"
            and symbol["Symbol"]["Binding"]["Name"] == "Local"
            and symbol["Symbol"]["Name"]["Name"] == ".L._.function.entry"
        )

    entry_label_symbols: dict[int, int] = {
        symbol["Symbol"]["Section"]["Value"]: index
        for index, symbol in enumerate(symbols)
        if is_entry_label(symbol)
    }

    if entry_label_symbols:
        section_symbols = entry_label_symbols

    assert 0 not in section_symbols
    section_symbols[0] = 0
    return {
        symbol["Symbol"]["Name"]["Name"]: (
            index,
            section_symbols.get(symbol["Symbol"]["Section"]["Value"], 0),
        )
        for index, symbol in enumerate(symbols)
    }


def expected_call_graph(symbols: list[dict]) -> list[dict[str, Any]]:
    symbol_index = symbol_indices(symbols)

    def section_reloc(name: str) -> dict[str, Any]:
        name_idx, section_idx = symbol_index[name]
        assert section_idx != 0
        reloc: dict[str, int] = {"SymbolIndex": section_idx}
        symbol = symbols[section_idx]["Symbol"]
        if symbol["Type"]["Name"] != "Section":
            reloc["SymbolName"] = symbol["Name"]["Name"]
        return reloc

    def name_reloc(name: str) -> dict[str, Any]:
        name_idx, section_idx = symbol_index[name]
        return {"SymbolIndex": name_idx, "SymbolName": name}

    def callees(*names: list[str]) -> list[dict]:
        return [{"Relocation": name_reloc(name)} for name in names]

    return sorted(
        [
            {
                "Names": ["DirectAndIndirectCalls"],
                "Relocation": section_reloc("DirectAndIndirectCalls"),
                "Version": 0,
                "IsIndirectTarget": False,
                "TypeID": 0,
                "DirectCallees": callees("Callee1"),
                "IndirectTypeIDs": [0x1234, 0x5678, 0xDEADBEEFFEEDFACE],
            },
            {
                "Names": ["DirectCalls"],
                "Relocation": section_reloc("DirectCalls"),
                "Version": 0,
                "IsIndirectTarget": False,
                "TypeID": 0,
                "DirectCallees": callees("Callee1", "Callee2", "Callee3"),
                "IndirectTypeIDs": [],
            },
            {
                "Names": ["DirectNoCalls"],
                "Relocation": section_reloc("DirectNoCalls"),
                "Version": 0,
                "IsIndirectTarget": False,
                "TypeID": 0,
                "DirectCallees": [],
                "IndirectTypeIDs": [],
            },
            {
                "Names": ["Indirectable"],
                "Relocation": section_reloc("Indirectable"),
                "Version": 0,
                "IsIndirectTarget": True,
                "TypeID": 0xFEEDFACE,
                "DirectCallees": callees("Callee1"),
                "IndirectTypeIDs": [],
            },
        ],
        key=dict_key_name,
    )


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--objfile",
        type=Path,
        help="ELF file to examine",
        required=True,
    )
    parser.add_argument(
        "--depfile",
        type=argparse.FileType("w"),
        required=True,
    )
    parser.add_argument(
        "--output",
        type=argparse.FileType("w"),
        help="Output file (success stamp)",
        required=True,
    )
    parser.add_argument("readelf", help="llvm-readelf binary", nargs=1)
    args = parser.parse_args()

    elf_files: list(str) = [str(args.objfile)]

    args.depfile.write(f"{args.output.name}: {' '.join(elf_files)}\n")
    args.depfile.close()

    [file_data] = json.loads(
        subprocess.check_output(
            [
                args.readelf[0],
                "--elf-output-style=JSON",
                "--call-graph-info",
                "--symbols",
            ]
            + elf_files
        )
    )

    expected = expected_call_graph(file_data["Symbols"])
    info = sorted(
        [entry["Function"] for entry in file_data["CallGraph"]],
        key=dict_key_name,
    )
    assert info == expected, (
        json.dumps(info, indent=4) + " != " + json.dumps(expected, indent=4)
    )

    args.output.write("OK\n")
    args.output.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
