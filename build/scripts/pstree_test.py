#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from __future__ import annotations

import io
import json
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path
from unittest import mock

import pstree


class PstreeTest(unittest.TestCase):
    def test_cmdline_str_escapes_newlines(self) -> None:
        node = pstree.ProcessNode.create(
            pid=1,
            ppid=0,
            comm="python3",
            state="R",
            cmdline=["python3", "-c", "import sys\nprint('hello\rworld')"],
        )
        self.assertEqual(
            node.cmdline_str(),
            r"python3 -c import sys\nprint('hello\rworld')",
        )

    def test_read_process_node(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            proc_root = Path(tmpdir)
            pid_dir = proc_root / "42"
            pid_dir.mkdir()
            (pid_dir / "stat").write_text(
                "42 (ninja) S 1 42 1 0 0 0 0 0 0 0 5 15"
            )
            (pid_dir / "cmdline").write_bytes(b"ninja\0-C\0out/default\0")
            (pid_dir / "wchan").write_text("pipe_wait\n")

            node = pstree.read_process_node(42, proc_root=proc_root)
            self.assertIsNotNone(node)
            assert node is not None
            self.assertEqual(node.pid, 42)
            self.assertEqual(node.ppid, 1)
            self.assertEqual(node.comm, "ninja")
            self.assertEqual(node.state, "S")
            self.assertEqual(node.cmdline, ["ninja", "-C", "out/default"])
            self.assertEqual(node.pgrp, 42)
            self.assertEqual(node.wchan, "pipe_wait")
            self.assertEqual(node.utime, 5)
            self.assertEqual(node.stime, 15)

    def test_read_process_node_wchan_zero(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            proc_root = Path(tmpdir)
            pid_dir = proc_root / "42"
            pid_dir.mkdir()
            (pid_dir / "stat").write_text("42 (ninja) R 1 42 1 0 0")
            (pid_dir / "wchan").write_text("0\n")

            node = pstree.read_process_node(42, proc_root=proc_root)
            self.assertIsNotNone(node)
            assert node is not None
            self.assertIsNone(node.wchan)

    def test_read_process_node_missing(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            proc_root = Path(tmpdir)
            self.assertIsNone(
                pstree.read_process_node(999, proc_root=proc_root)
            )

    def _create_mock_proc(
        self,
        proc_root: Path,
        pid: int,
        ppid: int,
        comm: str,
        cmdline: list[str],
        pgrp: int | None = None,
        state: str = "S",
        wchan: str = "",
    ) -> None:
        pid_dir = proc_root / str(pid)
        pid_dir.mkdir(parents=True, exist_ok=True)
        actual_pgrp = pgrp if pgrp is not None else pid
        (pid_dir / "stat").write_text(
            f"{pid} ({comm}) {state} {ppid} {actual_pgrp} 0 0"
        )
        (pid_dir / "cmdline").write_bytes(
            "\0".join(cmdline).encode("utf-8") + b"\0"
        )
        if wchan:
            (pid_dir / "wchan").write_text(wchan)

    def test_build_process_tree(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            proc_root = Path(tmpdir)
            # Create process hierarchy:
            # 100 (root)
            #   200
            #     300
            #     301
            #   201
            self._create_mock_proc(proc_root, 100, 1, "root_app", ["root_app"])
            self._create_mock_proc(proc_root, 200, 100, "worker1", ["worker1"])
            self._create_mock_proc(proc_root, 201, 100, "worker2", ["worker2"])
            self._create_mock_proc(proc_root, 301, 200, "leaf2", ["leaf2"])
            self._create_mock_proc(proc_root, 300, 200, "leaf1", ["leaf1"])
            # Unrelated process
            self._create_mock_proc(proc_root, 999, 1, "other", ["other"])

            tree = pstree.build_process_tree(100, proc_root=proc_root)
            self.assertIsNotNone(tree)
            assert tree is not None
            self.assertEqual(tree.pid, 100)
            self.assertEqual(len(tree.children), 2)
            self.assertEqual(tree.children[0].pid, 200)
            self.assertEqual(tree.children[1].pid, 201)

            # Check sorting of 200's children (300 before 301)
            self.assertEqual(
                [c.pid for c in tree.children[0].children], [300, 301]
            )

            # Check leaves
            leaves = list(pstree.find_leaves(tree))
            leaf_pids = [leaf.pid for leaf in leaves]
            self.assertEqual(leaf_pids, [300, 301, 201])

    def test_build_process_tree_orphaned_descendant_in_pgrp(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            proc_root = Path(tmpdir)
            # Root: 100 (group leader)
            # Child 200 died, leaving grandchild 300 with ppid=1, but pgrp=100
            self._create_mock_proc(
                proc_root, 100, 1, "root_app", ["root_app"], pgrp=100
            )
            self._create_mock_proc(
                proc_root, 300, 1, "orphan_child", ["orphan_child"], pgrp=100
            )
            # 300's child 400 has ppid=300, pgrp=100
            self._create_mock_proc(
                proc_root,
                400,
                300,
                "orphan_grandchild",
                ["orphan_grandchild"],
                pgrp=100,
            )

            tree = pstree.build_process_tree(
                100, proc_root=proc_root, include_pgrp=True
            )
            self.assertIsNotNone(tree)
            assert tree is not None
            self.assertEqual(tree.pid, 100)
            # 300 should be attached under root 100
            self.assertEqual([c.pid for c in tree.children], [300])
            # 400 should be attached under 300
            self.assertEqual([c.pid for c in tree.children[0].children], [400])

    def test_build_process_tree_missing_root(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            proc_root = Path(tmpdir)
            self.assertIsNone(
                pstree.build_process_tree(100, proc_root=proc_root)
            )

    def test_serialization_and_formatters(self) -> None:
        root = pstree.ProcessNode.create(
            pid=10,
            ppid=1,
            comm="ninja",
            state="S",
            cmdline=["ninja", "-C", "out"],
            wchan="futex_wait",
            utime=100,
            stime=50,
            children=[
                pstree.ProcessNode.create(
                    pid=20,
                    ppid=10,
                    comm="clang++",
                    state="R",
                    cmdline=["clang++", "-c", "foo.cc"],
                    utime=200,
                    stime=30,
                )
            ],
        )

        # JSON serialization
        json_obj = root.to_dict()
        self.assertEqual(json_obj["pid"], 10)
        self.assertEqual(json_obj["utime"], 100)
        self.assertEqual(json_obj["stime"], 50)
        children = json_obj["children"]
        self.assertIsInstance(children, list)
        assert isinstance(children, list)
        self.assertEqual(len(children), 1)
        first_child = children[0]
        self.assertIsInstance(first_child, dict)
        assert isinstance(first_child, dict)
        self.assertEqual(first_child["pid"], 20)
        self.assertEqual(first_child["utime"], 200)

        # Round-trip deserialization via from_dict
        restored = pstree.ProcessNode.from_dict(json_obj)
        self.assertEqual(restored.pid, 10)
        self.assertEqual(restored.comm, "ninja")
        self.assertEqual(restored.wchan, "futex_wait")
        self.assertEqual(len(restored.children), 1)
        self.assertEqual(restored.children[0].pid, 20)
        self.assertEqual(restored.to_dict(), json_obj)

        # Formatter: tree
        tree_lines = list(pstree.FORMATTERS["tree"](root))
        self.assertEqual(len(tree_lines), 2)
        self.assertIn(
            "10 ninja (state: S [futex_wait]): ninja -C out", tree_lines[0]
        )
        self.assertIn(
            "+-- 20 clang++ (state: R): clang++ -c foo.cc", tree_lines[1]
        )

        # Formatter: leaves
        leaf_lines = list(pstree.FORMATTERS["leaves"](root))
        self.assertEqual(len(leaf_lines), 1)
        self.assertIn("20 clang++ (state: R): clang++ -c foo.cc", leaf_lines[0])

        # Formatter: json
        json_lines = list(pstree.FORMATTERS["json"](root))
        self.assertEqual(len(json_lines), 1)
        loaded = json.loads(json_lines[0])
        self.assertEqual(loaded["pid"], 10)

    def test_format_tree_multilevel_branches(self) -> None:
        root = pstree.ProcessNode.create(
            pid=1,
            ppid=0,
            comm="root",
            state="S",
            children=[
                pstree.ProcessNode.create(
                    pid=2,
                    ppid=1,
                    comm="child1",
                    state="S",
                    children=[
                        pstree.ProcessNode.create(
                            pid=4, ppid=2, comm="grandchild1", state="R"
                        ),
                        pstree.ProcessNode.create(
                            pid=5, ppid=2, comm="grandchild2", state="R"
                        ),
                    ],
                ),
                pstree.ProcessNode.create(
                    pid=3,
                    ppid=1,
                    comm="child2",
                    state="S",
                    children=[
                        pstree.ProcessNode.create(
                            pid=6, ppid=3, comm="grandchild3", state="R"
                        ),
                    ],
                ),
            ],
        )
        lines = list(pstree.format_tree(root))
        self.assertEqual(
            lines,
            [
                "1 root (state: S): [root]",
                "+-- 2 child1 (state: S): [child1]",
                "|   +-- 4 grandchild1 (state: R): [grandchild1]",
                "|   +-- 5 grandchild2 (state: R): [grandchild2]",
                "+-- 3 child2 (state: S): [child2]",
                "    +-- 6 grandchild3 (state: R): [grandchild3]",
            ],
        )

    def test_main_cli_default_root_pid_1(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            proc_root = Path(tmpdir)
            self._create_mock_proc(proc_root, 1, 0, "init", ["init"])
            self._create_mock_proc(proc_root, 10, 1, "child", ["child"])

            stdout_buf = io.StringIO()
            with redirect_stdout(stdout_buf):
                ret = pstree.main(["--proc-root", str(proc_root)])
            self.assertEqual(ret, 0)
            output = stdout_buf.getvalue()
            self.assertIn("1 init", output)
            self.assertIn("10 child", output)

    def test_main_cli_default_format(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            proc_root = Path(tmpdir)
            self._create_mock_proc(
                proc_root, 10, 1, "test_proc", ["test_proc", "arg"]
            )

            stdout_buf = io.StringIO()
            with redirect_stdout(stdout_buf):
                ret = pstree.main(["10", "--proc-root", str(proc_root)])
            self.assertEqual(ret, 0)
            self.assertIn("10 test_proc", stdout_buf.getvalue())

    def test_main_cli_format_json(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            proc_root = Path(tmpdir)
            self._create_mock_proc(
                proc_root, 10, 1, "test_proc", ["test_proc", "arg"]
            )

            stdout_buf = io.StringIO()
            with redirect_stdout(stdout_buf):
                ret = pstree.main(
                    [
                        "--root",
                        "10",
                        "--proc-root",
                        str(proc_root),
                        "--format",
                        "json",
                    ]
                )
            self.assertEqual(ret, 0)
            data = json.loads(stdout_buf.getvalue())
            self.assertEqual(data["pid"], 10)

    def test_main_cli_format_leaves(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            proc_root = Path(tmpdir)
            self._create_mock_proc(proc_root, 10, 1, "parent", ["parent"])
            self._create_mock_proc(proc_root, 20, 10, "child", ["child"])

            stdout_buf = io.StringIO()
            with redirect_stdout(stdout_buf):
                ret = pstree.main(
                    [
                        "10",
                        "--proc-root",
                        str(proc_root),
                        "--format",
                        "leaves",
                    ]
                )
            self.assertEqual(ret, 0)
            output = stdout_buf.getvalue()
            self.assertNotIn("10 parent", output)
            self.assertIn("20 child", output)

    def test_main_cli_broken_pipe(self) -> None:
        """Verifies that pipe closure by downstream tools (e.g. head) exits cleanly with 0."""

        class BrokenPipeStream(io.StringIO):
            def write(self, s: str) -> int:
                raise BrokenPipeError()

        with tempfile.TemporaryDirectory() as tmpdir:
            proc_root = Path(tmpdir)
            self._create_mock_proc(proc_root, 10, 1, "test_proc", ["test_proc"])

            with redirect_stdout(BrokenPipeStream()):
                ret = pstree.main(["10", "--proc-root", str(proc_root)])
            self.assertEqual(ret, 0)

    def test_main_cli_output_file(self) -> None:
        """Verifies that --output writes formatted content directly to a file."""
        with tempfile.TemporaryDirectory() as tmpdir:
            proc_root = Path(tmpdir) / "proc"
            self._create_mock_proc(proc_root, 10, 1, "parent", ["parent"])
            self._create_mock_proc(proc_root, 20, 10, "child", ["child"])

            out_file = Path(tmpdir) / "nested" / "tree.json"
            ret = pstree.main(
                [
                    "10",
                    "--proc-root",
                    str(proc_root),
                    "--format",
                    "json",
                    "--output",
                    str(out_file),
                ]
            )
            self.assertEqual(ret, 0)
            self.assertTrue(out_file.exists())
            data = json.loads(out_file.read_text(encoding="utf-8"))
            self.assertEqual(data["pid"], 10)
            self.assertEqual(data["children"][0]["pid"], 20)

    def test_main_cli_not_found(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            proc_root = Path(tmpdir)
            stderr_buf = io.StringIO()
            with redirect_stderr(stderr_buf):
                ret = pstree.main(["999", "--proc-root", str(proc_root)])
            self.assertEqual(ret, 1)
            self.assertIn("Process 999 not found", stderr_buf.getvalue())

    def test_main_cli_broken_pipe_on_flush(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            proc_root = Path(tmpdir)
            self._create_mock_proc(proc_root, 10, 1, "parent", ["parent"])

            mock_stdout = mock.Mock()
            mock_stdout.flush.side_effect = BrokenPipeError("Broken pipe")

            with mock.patch("sys.stdout", mock_stdout):
                ret = pstree.main(["10", "--proc-root", str(proc_root)])
            self.assertEqual(ret, 0)
            mock_stdout.close.assert_called_once()

    def test_build_process_tree_handles_ppid_cycle(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            proc_root = Path(tmpdir)
            self._create_mock_proc(proc_root, 10, 1, "root", ["root"])
            # Create a mutual ppid cycle between 20 and 30
            self._create_mock_proc(proc_root, 20, 30, "node20", ["node20"])
            self._create_mock_proc(proc_root, 30, 20, "node30", ["node30"])

            tree = pstree.build_process_tree(10, proc_root=proc_root)
            self.assertIsNotNone(tree)
            assert tree is not None
            self.assertEqual(tree.pid, 10)
            self.assertEqual(tree.children, [])

    def test_build_process_tree_pgrp_orphan_no_duplicate_attachment(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            proc_root = Path(tmpdir)
            # Root: pid 10, pgrp 10
            self._create_mock_proc(proc_root, 10, 1, "root", ["root"], pgrp=10)
            # Child: pid 20, ppid 10, pgrp 10
            self._create_mock_proc(
                proc_root, 20, 10, "child", ["child"], pgrp=10
            )
            # Intermediate parent in another pgrp 99, but child of 20
            self._create_mock_proc(
                proc_root, 30, 20, "intermediate", ["intermediate"], pgrp=99
            )
            # Descendant: pid 40, ppid 30, but in pgrp 10
            self._create_mock_proc(proc_root, 40, 30, "leaf", ["leaf"], pgrp=10)

            tree = pstree.build_process_tree(10, proc_root=proc_root)
            self.assertIsNotNone(tree)
            assert tree is not None
            # Node 40 should not be attached directly to root since its ancestor 20 is in pgrp 10
            root_child_pids = [c.pid for c in tree.children]
            self.assertNotIn(40, root_child_pids)


if __name__ == "__main__":
    unittest.main()
