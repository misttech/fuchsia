# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Graph algorithms and utilities."""
from collections.abc import KeysView
from typing import Any


class Graph:
    """A simple directed graph implementation."""

    def __init__(self) -> None:
        # Maps each vertex to its successors. Vertices are kept in insertion
        # order, so that traversals, and the numbering of the SCCs they
        # produce, are the same from one run to the next.
        self.graph: dict[Any, list[Any]] = {}

    @property
    def vertices(self) -> KeysView[Any]:
        """The vertices of the graph, in insertion order."""
        return self.graph.keys()

    def add_vertex(self, u: Any) -> None:
        """Adds a vertex to the graph."""
        self.graph.setdefault(u, [])

    def add_edge(self, u: Any, v: Any) -> None:
        """Adds a directed edge from u to v."""
        self.add_vertex(u)
        self.add_vertex(v)
        self.graph[u].append(v)

    def get_sccs(self) -> list[list[Any]]:
        """
        Finds Strongly Connected Components using Tarjan's algorithm.

        The depth-first search is iterative, so that call chains deeper than
        Python's recursion limit are handled.
        """
        index_counter = 0
        stack: list[Any] = []
        on_stack: set[Any] = set()
        lowlink: dict[Any, int] = {}
        index: dict[Any, int] = {}
        result: list[list[Any]] = []

        for root in self.vertices:
            if root in index:
                continue
            # Each work item is a node and an iterator over its successors,
            # which records how far the node's successors have been visited.
            work = [(root, iter(self.graph[root]))]
            index[root] = lowlink[root] = index_counter
            index_counter += 1
            stack.append(root)
            on_stack.add(root)
            while work:
                node, successors = work[-1]
                descended = False
                for successor in successors:
                    if successor not in index:
                        index[successor] = lowlink[successor] = index_counter
                        index_counter += 1
                        stack.append(successor)
                        on_stack.add(successor)
                        work.append((successor, iter(self.graph[successor])))
                        descended = True
                        break
                    if successor in on_stack:
                        lowlink[node] = min(lowlink[node], index[successor])
                if descended:
                    continue

                # All successors of node have been visited.
                work.pop()
                if work:
                    parent = work[-1][0]
                    lowlink[parent] = min(lowlink[parent], lowlink[node])
                if lowlink[node] == index[node]:
                    # SCC found. The current node is the root of the SCC.
                    # Nodes belonging to this SCC are currently on the stack.
                    connected_component = []
                    while True:
                        member = stack.pop()
                        on_stack.discard(member)
                        connected_component.append(member)
                        if member == node:
                            break
                    result.append(connected_component)

        return result

    def topological_sort(self) -> list[Any]:
        """
        Returns vertices in topological order (u before v if u->v).
        Note: This is only valid for DAGs. For cyclic graphs, this returns
        *some* order compatible with the condensation DAG if used on SCCs,
        but here it's run directly on the SCC DAG.
        So this function just needs to support DAGs.
        """
        visited = set()
        stack = []

        for root in self.vertices:
            if root in visited:
                continue
            visited.add(root)
            work = [(root, iter(self.graph[root]))]
            while work:
                node, neighbors = work[-1]
                for neighbor in neighbors:
                    if neighbor not in visited:
                        visited.add(neighbor)
                        work.append((neighbor, iter(self.graph[neighbor])))
                        break
                else:
                    work.pop()
                    stack.append(node)

        # Stack has reverse topological order (children first).
        # We want u before v (parents first).
        return stack[::-1]
