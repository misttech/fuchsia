# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Call graph and stack size parsing module."""

from collections import defaultdict
from collections.abc import Iterable, Iterator
from dataclasses import dataclass
from typing import Any

import graph

# Type aliases
# One file of llvm-readelf --elf-output-style=JSON output.
JsonData = dict[str, Any]
StackSizeMap = dict[str, int]
Address = int
FuncName = str
FunctionInfoByAddr = dict[Address, dict[str, Any]]


def single_file(readelf_output: Any) -> JsonData:
    """
    Returns the one file in llvm-readelf JSON output, which is a list with an
    object per input file. The analysis is of one linked file, whose addresses
    mean nothing in another file, so any other number is a ValueError.
    """
    if (
        isinstance(readelf_output, list)
        and len(readelf_output) == 1
        and isinstance(readelf_output[0], dict)
    ):
        return readelf_output[0]
    raise ValueError("expected llvm-readelf JSON output for exactly one file")


@dataclass(frozen=True)
class FunctionSet:
    """
    Functions named by a configuration, each by name or by address, which
    identifies the function exactly, so that a local function whose name is
    not unique can be named.
    """

    names: frozenset[FuncName] = frozenset()
    addresses: frozenset[Address] = frozenset()

    def contains(self, addr: Address, names: Iterable[FuncName]) -> bool:
        return addr in self.addresses or not self.names.isdisjoint(names)


def function_set(items: Any) -> FunctionSet:
    """
    Parses a configuration value that names functions: a list of function
    names and dictionaries with an address (and a name, for the reader).
    """
    names: set[FuncName] = set()
    addresses: set[Address] = set()
    for item in items or []:
        if isinstance(item, str):
            names.add(item)
        elif isinstance(item, dict) and "address" in item:
            addresses.add(item["address"])
        elif isinstance(item, dict) and "name" in item:
            names.add(item["name"])
    return FunctionSet(frozenset(names), frozenset(addresses))


def get_exclusions(config: dict[str, Any] | None) -> FunctionSet:
    """Parses exclude_functions, the functions left out of the analysis."""
    return function_set((config or {}).get("exclude_functions"))


def extract_type_id_mapping(
    data: JsonData, excluded: FunctionSet = FunctionSet()
) -> dict[int, list[Address]]:
    """
    Extracts a mapping from TypeID to a list of function addresses.
    """
    type_id_to_targets: dict[int, list[Address]] = defaultdict(list)
    for entry in data.get("CallGraph", []):
        func = entry.get("Function", {})
        addr = func.get("Address")
        if addr is None:
            continue
        if excluded.contains(addr, func.get("Names", [])):
            continue
        type_id = func.get("TypeID")
        if type_id:
            type_id_to_targets[type_id].append(addr)
    return type_id_to_targets


def get_display_names(data: JsonData) -> dict[Address, list[str]]:
    """
    Returns the symbol names that llvm-readelf printed for each function and
    direct callee address. This is only for presenting results to the user.
    """
    names_by_addr: dict[Address, list[str]] = {}
    for entry in data.get("CallGraph", []):
        func = entry.get("Function", {})
        for item in [func] + func.get("DirectCallees", []):
            addr = item.get("Address")
            if addr is not None and item.get("Names"):
                names_by_addr.setdefault(addr, item["Names"])
    return names_by_addr


def callees(adj: dict[str, Any]) -> Iterator[Address]:
    """
    Yields the functions in the call graph that an adjacency list entry
    calls, directly or as the targets of its indirect calls.
    """
    yield from adj.get("direct", [])
    for indirect in adj.get("indirect", []):
        yield from indirect.get("targets", [])


def _register_vertices(
    data: JsonData,
    excluded: FunctionSet,
    stack_sizes: StackSizeMap,
    call_graph: graph.Graph,
    fn_name_to_addr: dict[FuncName, Address],
    function_info_map: FunctionInfoByAddr,
    excluded_addrs: set[Address],
) -> None:
    """Registers function vertices and their stack sizes."""
    for entry in data.get("CallGraph", []):
        func = entry.get("Function", {})
        addr = func.get("Address")
        names = func.get("Names", [])

        if addr is None:
            continue

        primary_name = names[0] if names else f"<unknown_@{hex(addr)}>"

        if excluded.contains(addr, names):
            excluded_addrs.add(addr)
            continue

        fn_name_to_addr.update(dict.fromkeys(names, addr))
        stack_usage = next(
            (stack_sizes[n] for n in names if n in stack_sizes), 0
        )

        call_graph.add_vertex(addr)
        function_info_map[addr] = {
            "stack_usage": stack_usage,
            "label": primary_name,
            "names": names,
        }


def _add_edges(
    data: JsonData,
    type_id_to_targets: dict[int, list[Address]],
    call_graph: graph.Graph,
    function_info_map: FunctionInfoByAddr,
    adj_list: dict[Address, dict[str, Any]],
    excluded_addrs: set[Address],
) -> None:
    """Resolves direct/indirect calls into graph edges and adjacency list."""
    for entry in data.get("CallGraph", []):
        func = entry.get("Function", {})
        caller_addr = func.get("Address")

        if caller_addr is None or caller_addr in excluded_addrs:
            continue

        if caller_addr not in adj_list:
            adj_list[caller_addr] = {
                "direct": [],
                "indirect": [],
                "uninstrumented": [],
            }

        for callee in func.get("DirectCallees", []):
            callee_addr = callee.get("Address")
            # Calls to excluded functions are ignored.
            if callee_addr is None or callee_addr in excluded_addrs:
                continue
            if callee_addr in function_info_map:
                call_graph.add_edge(caller_addr, callee_addr)
                adj_list[caller_addr]["direct"].append(callee_addr)
            elif callee_addr not in adj_list[caller_addr]["uninstrumented"]:
                # The callee has no call graph information. In linked
                # files, calls to functions in other modules have callee
                # address 0.
                adj_list[caller_addr]["uninstrumented"].append(callee_addr)

        for indirect_type_id in func.get("IndirectTypeIDs", []):
            if not indirect_type_id:
                continue
            targets = []
            for target_addr in type_id_to_targets.get(indirect_type_id, []):
                if target_addr in function_info_map:
                    call_graph.add_edge(caller_addr, target_addr)
                    targets.append(target_addr)
            if targets:
                adj_list[caller_addr]["indirect"].append(
                    {"typeId": indirect_type_id, "targets": targets}
                )


def build_call_graph(
    data: JsonData,
    stack_sizes: StackSizeMap,
    config: dict[str, Any] | None = None,
) -> tuple[
    graph.Graph,
    dict[FuncName, Address],
    FunctionInfoByAddr,
    dict[Address, dict[str, Any]],
]:
    """
    Constructs a directed graph from call graph metadata and stack sizes.

    Algorithm overview:
    1. Parses the excluded functions from the configuration so we can ignore
       undesired nodes early.
    2. Passes over the JSON data twice:
       - First pass: Registers all function vertices, resolves primary names,
         and maps their associated stack usages.
       - Second pass: Resolves call edges. Direct calls are mapped directly via
         addresses. Indirect calls are resolved by matching IndirectTypeIDs
         to target addresses.

    Invariants:
    - Nodes (vertices) are strictly unique integer addresses.
    - An edge is added only if both the caller and the callee exist in the
      first pass of vertex registration. Other direct callees are recorded
      as "uninstrumented".
    - Excluded functions are skipped entirely, generating no vertices or edges.
      Calls to them are ignored.
    """
    call_graph = graph.Graph()
    fn_name_to_addr: dict[FuncName, Address] = {}
    function_info_map: FunctionInfoByAddr = {}

    excluded = get_exclusions(config)
    # The functions excluded by address need no record to be excluded.
    excluded_addrs: set[Address] = set(excluded.addresses)

    # Map TypeID -> List[Address]
    type_id_to_targets = extract_type_id_mapping(data, excluded)

    _register_vertices(
        data,
        excluded,
        stack_sizes,
        call_graph,
        fn_name_to_addr,
        function_info_map,
        excluded_addrs,
    )

    adj_list: dict[Address, dict[str, Any]] = {}

    _add_edges(
        data,
        type_id_to_targets,
        call_graph,
        function_info_map,
        adj_list,
        excluded_addrs,
    )

    return call_graph, fn_name_to_addr, function_info_map, adj_list


def parse_stack_sizes(data: JsonData) -> StackSizeMap:
    """
    Parses the stack size JSON data into a simple dictionary.
    """
    sizes: StackSizeMap = {}
    # llvm-readelf --stack-sizes --elf-output-style=JSON output has a
    # "StackSizes" list in the file's object:
    # { "StackSizes": [ { "Entry": { "Functions": [...], "Size": 10 } } ] }
    for entry_item in data.get("StackSizes", []):
        entry = entry_item.get("Entry", {})
        size = entry.get("Size", 0)
        for func_name in entry.get("Functions", []):
            sizes[func_name] = size
    return sizes
