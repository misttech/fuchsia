#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import contextlib
import io
import json
import os
import pathlib
import shutil
import subprocess
import sys
import tempfile
import unittest
from typing import Any

_CHECKS_DIR = pathlib.Path(__file__).resolve().parent
sys.path.insert(0, str(_CHECKS_DIR))

import verify_tests_json_parity as vtp

_FAKE_GIT_PY = """\
import hashlib
import json
import os
import pathlib
import re
import sys


def _scan_worktree(repo: pathlib.Path) -> dict[str, str]:
    out: dict[str, str] = {}
    for dirpath, dirnames, filenames in os.walk(repo):
        dirnames[:] = [d for d in dirnames if d != ".git"]
        for fname in filenames:
            full = pathlib.Path(dirpath) / fname
            rel = full.relative_to(repo).as_posix()
            out[rel] = full.read_text(encoding="utf-8", errors="replace")
    return out


def _state_path(repo: pathlib.Path) -> pathlib.Path:
    return repo / ".git" / "fake_state.json"


def _load_state(repo: pathlib.Path) -> dict:
    sp = _state_path(repo)
    if not sp.is_file():
        sys.stderr.write("fatal: not a git repository\\n")
        sys.exit(128)
    return json.loads(sp.read_text(encoding="utf-8"))


def _save_state(repo: pathlib.Path, state: dict) -> None:
    sp = _state_path(repo)
    sp.parent.mkdir(parents=True, exist_ok=True)
    sp.write_text(json.dumps(state), encoding="utf-8")


def _resolve_rev(state: dict, rev_expr: str) -> dict | None:
    expr = rev_expr.strip()
    if not expr:
        return None
    if expr.endswith("^{commit}"):
        expr = expr[: -len("^{commit}")]
    steps = 0
    while True:
        if expr.endswith("^"):
            steps += 1
            expr = expr[:-1]
            continue
        m = re.match(r"^(.*)~(\\d*)$", expr)
        if m:
            steps += int(m.group(2) or "1")
            expr = m.group(1)
            continue
        break
    commits_by_sha = {c["sha"]: c for c in state["commits"]}
    if expr == "HEAD":
        cur_sha = state.get("head", "")
    elif expr in commits_by_sha:
        cur_sha = expr
    else:
        matches = [
            s for s in commits_by_sha if len(expr) >= 4 and s.startswith(expr)
        ]
        cur_sha = matches[0] if len(matches) == 1 else ""
    cur = commits_by_sha.get(cur_sha)
    for _ in range(steps):
        if cur is None or not cur["parent"]:
            return None
        cur = commits_by_sha.get(cur["parent"])
    return cur


def _matches_filters(path: str, filters: list[str]) -> bool:
    if not filters:
        return True
    for raw_f in filters:
        f = raw_f.strip().strip("/")
        if not f or path == f or path.startswith(f + "/"):
            return True
    return False


def main() -> int:
    argv = sys.argv[1:]
    repo = pathlib.Path.cwd()
    idx = 0
    while idx < len(argv):
        arg = argv[idx]
        if arg == "-C" and idx + 1 < len(argv):
            repo = pathlib.Path(argv[idx + 1]).resolve()
            idx += 2
        elif arg.startswith("-C") and len(arg) > 2:
            repo = pathlib.Path(arg[2:]).resolve()
            idx += 1
        elif arg == "--no-optional-locks":
            idx += 1
        else:
            break

    if idx >= len(argv):
        return 1
    cmd = argv[idx]
    sub = argv[idx + 1 :]

    if cmd == "init":
        (repo / ".git").mkdir(parents=True, exist_ok=True)
        (repo / ".git" / "HEAD").write_text(
            "ref: refs/heads/main\\n", encoding="utf-8"
        )
        _save_state(repo, {"commits": [], "head": "", "index": {}})
        return 0

    if cmd == "config":
        _load_state(repo)
        return 0

    if cmd == "rev-parse":
        if "--git-dir" in sub:
            if _state_path(repo).is_file():
                print(".git")
                return 0
            return 128
        revs = [a for a in sub if not a.startswith("-")]
        if not revs:
            return 128
        state = _load_state(repo)
        c = _resolve_rev(state, revs[-1])
        if c is None:
            return 128
        print(c["sha"])
        return 0

    state = _load_state(repo)

    if cmd == "add":
        wt = _scan_worktree(repo)
        raw_paths = [
            a
            for a in sub
            if a not in ("-A", "--all", "-u", "--") and not a.startswith("-")
        ]
        if not raw_paths or "." in raw_paths or "-A" in sub or "--all" in sub:
            state["index"] = wt
        else:
            index = dict(state["index"])
            for p in raw_paths:
                norm = p.strip().lstrip("./").strip("/")
                for k in list(index):
                    if not norm or k == norm or k.startswith(norm + "/"):
                        del index[k]
                for k, v in wt.items():
                    if not norm or k == norm or k.startswith(norm + "/"):
                        index[k] = v
            state["index"] = index
        _save_state(repo, state)
        return 0

    if cmd == "commit":
        msg = ""
        amend = "--amend" in sub
        i = 0
        while i < len(sub):
            if sub[i] == "-m" and i + 1 < len(sub):
                msg = sub[i + 1]
                i += 2
            elif sub[i].startswith("-m") and len(sub[i]) > 2:
                msg = sub[i][2:]
                i += 1
            else:
                i += 1
        head_c = _resolve_rev(state, "HEAD")
        parent_sha = (
            head_c["parent"]
            if (amend and head_c is not None)
            else state.get("head", "")
        )
        tree = dict(state["index"])
        digest_input = json.dumps(
            [len(state["commits"]), parent_sha, msg, tree], sort_keys=True
        ).encode("utf-8")
        sha = hashlib.sha1(digest_input).hexdigest()
        state["commits"].append(
            {
                "sha": sha,
                "parent": parent_sha,
                "message": msg,
                "tree": tree,
            }
        )
        state["head"] = sha
        _save_state(repo, state)
        return 0

    if cmd == "reset":
        revs = [a for a in sub if not a.startswith("-")]
        target_rev = revs[0] if revs else "HEAD"
        c = _resolve_rev(state, target_rev)
        if c is None:
            return 128
        if "--hard" in sub:
            wt = _scan_worktree(repo)
            for k in wt:
                if k not in c["tree"]:
                    (repo / k).unlink()
            for k, v in c["tree"].items():
                dest = repo / k
                dest.parent.mkdir(parents=True, exist_ok=True)
                dest.write_text(v, encoding="utf-8")
        state["head"] = c["sha"]
        state["index"] = dict(c["tree"])
        _save_state(repo, state)
        return 0

    if cmd == "status":
        head_c = _resolve_rev(state, "HEAD")
        head_tree = head_c["tree"] if head_c else {}
        index = state["index"]
        wt = _scan_worktree(repo)
        show_untracked = "--untracked-files=no" not in sub
        for k in sorted(set(head_tree) | set(index)):
            in_h = k in head_tree
            in_i = k in index
            in_w = k in wt
            x = (
                " "
                if (in_h == in_i and head_tree.get(k) == index.get(k))
                else ("A" if not in_h else ("D" if not in_i else "M"))
            )
            y = (
                " "
                if (not in_i or (in_w and wt.get(k) == index.get(k)))
                else ("D" if not in_w else "M")
            )
            if x != " " or y != " ":
                print(f"{x}{y} {k}")
        if show_untracked:
            for k in sorted(set(wt) - set(index) - set(head_tree)):
                print(f"?? {k}")
        return 0

    if cmd == "ls-files":
        if "--" in sub:
            dd = sub.index("--")
            flags, paths = sub[:dd], sub[dd + 1 :]
        else:
            flags = [a for a in sub if a.startswith("-")]
            paths = [a for a in sub if not a.startswith("-")]
        index = state["index"]
        wt = _scan_worktree(repo)
        if "--error-unmatch" in flags:
            norm_paths = [p.strip().lstrip("./").strip("/") for p in paths]
            for p in norm_paths:
                if p not in index:
                    return 1
            for p in norm_paths:
                print(p)
            return 0
        if "--others" in flags:
            for k in sorted(wt):
                if k not in index and _matches_filters(k, paths):
                    print(k)
            return 0
        for k in sorted(index):
            if _matches_filters(k, paths):
                print(k)
        return 0

    if cmd == "diff":
        if "--" in sub:
            dd = sub.index("--")
            before_dd, paths = sub[:dd], sub[dd + 1 :]
        else:
            before_dd, paths = sub, []
        name_status = "--name-status" in before_dd
        revs = [a for a in before_dd if not a.startswith("-")]
        wt = _scan_worktree(repo)
        index = state["index"]
        if len(revs) == 0:
            tree1 = index
            tree2 = {k: wt[k] for k in index if k in wt}
        elif len(revs) == 1:
            c1 = _resolve_rev(state, revs[0])
            if c1 is None:
                return 128
            tree1 = c1["tree"]
            tree2 = {k: wt[k] for k in index if k in wt}
        else:
            c1 = _resolve_rev(state, revs[0])
            c2 = _resolve_rev(state, revs[1])
            if c1 is None or c2 is None:
                return 128
            tree1 = c1["tree"]
            tree2 = c2["tree"]
        for k in sorted(set(tree1) | set(tree2)):
            if not _matches_filters(k, paths):
                continue
            in1 = k in tree1
            in2 = k in tree2
            if in1 and not in2:
                st = "D"
            elif in2 and not in1:
                st = "A"
            elif tree1[k] != tree2[k]:
                st = "M"
            else:
                continue
            if name_status:
                print(f"{st}\\t{k}")
            else:
                print(k)
        return 0

    if cmd == "show":
        specs = [a for a in sub if not a.startswith("-")]
        if not specs or ":" not in specs[0]:
            return 128
        rev_part, _, path_part = specs[0].partition(":")
        c = _resolve_rev(state, rev_part)
        norm_p = path_part.lstrip("./")
        if c is None or norm_p not in c["tree"]:
            return 128
        sys.stdout.write(c["tree"][norm_p])
        return 0

    if cmd == "log":
        if "--" in sub:
            dd = sub.index("--")
            before_dd, paths = sub[:dd], sub[dd + 1 :]
        else:
            before_dd, paths = sub, []
        max_count = 10**9
        fmt = "%B"
        revs = []
        i = 0
        while i < len(before_dd):
            a = before_dd[i]
            if a == "-1":
                max_count = 1
                i += 1
            elif a == "-n" and i + 1 < len(before_dd):
                max_count = int(before_dd[i + 1])
                i += 2
            elif a.startswith("-n") and a[2:].isdigit():
                max_count = int(a[2:])
                i += 1
            elif a.startswith("--format="):
                fmt = a[len("--format=") :]
                i += 1
            elif not a.startswith("-"):
                revs.append(a)
                i += 1
            else:
                i += 1
        start_rev = revs[0] if revs else "HEAD"
        cur = _resolve_rev(state, start_rev)
        if cur is None:
            return 128
        commits_by_sha = {c["sha"]: c for c in state["commits"]}
        out_lines: list[str] = []
        while cur is not None and len(out_lines) < max_count:
            parent_c = (
                commits_by_sha.get(cur["parent"]) if cur["parent"] else None
            )
            if paths:
                ptree = parent_c["tree"] if parent_c else {}
                ctree = cur["tree"]
                touched = [
                    k
                    for k in (set(ptree) | set(ctree))
                    if ptree.get(k) != ctree.get(k)
                    and _matches_filters(k, paths)
                ]
                if not touched:
                    cur = parent_c
                    continue
            if fmt == "%s":
                lines = cur["message"].splitlines()
                out_lines.append(lines[0] if lines else "")
            else:
                out_lines.append(cur["message"].rstrip("\\n"))
            cur = parent_c
        if out_lines:
            print("\\n".join(out_lines))
        return 0

    if cmd == "merge-base":
        is_anc = "--is-ancestor" in sub
        revs = [a for a in sub if not a.startswith("-")]
        if len(revs) < 2:
            return 128
        c1 = _resolve_rev(state, revs[0])
        c2 = _resolve_rev(state, revs[1])
        if c1 is None or c2 is None:
            return 128
        commits_by_sha = {c["sha"]: c for c in state["commits"]}
        if is_anc:
            cur = c2
            while cur is not None:
                if cur["sha"] == c1["sha"]:
                    return 0
                cur = (
                    commits_by_sha.get(cur["parent"]) if cur["parent"] else None
                )
            return 1
        anc1: set[str] = set()
        cur = c1
        while cur is not None:
            anc1.add(cur["sha"])
            cur = commits_by_sha.get(cur["parent"]) if cur["parent"] else None
        cur = c2
        while cur is not None:
            if cur["sha"] in anc1:
                print(cur["sha"])
                return 0
            cur = commits_by_sha.get(cur["parent"]) if cur["parent"] else None
        return 1

    return 1


if __name__ == "__main__":
    sys.exit(main())
"""


def _install_hermetic_tool_shims(
    bin_dir: pathlib.Path, *, include_git: bool = True
) -> None:
    """Installs `python3` (and optional `git`) shims for hermetic Swarming bots."""
    bin_dir.mkdir(parents=True, exist_ok=True)
    py_exe = os.path.abspath(sys.executable)

    py3_wrapper = bin_dir / "python3"
    py3_wrapper.write_text(
        f'#!/bin/sh\nexec "{py_exe}" "$@"\n',
        encoding="utf-8",
    )
    py3_wrapper.chmod(0o755)

    if include_git:
        fake_git_py = bin_dir / "_fake_git.py"
        fake_git_py.write_text(_FAKE_GIT_PY, encoding="utf-8")
        git_wrapper = bin_dir / "git"
        git_wrapper.write_text(
            f'#!/bin/sh\nexec "{py_exe}" -B "{fake_git_py}" "$@"\n',
            encoding="utf-8",
        )
        git_wrapper.chmod(0o755)


def _device_entry(
    pkg: str,
    comp: str = "foo_test",
    label: str = "//src/foo:foo-tests(//build/toolchain/fuchsia:x64)",
    source_label: str = "//src/foo:foo-tests",
    cpu: str = "x64",
    max_severity: str = "WARN",
    build_only: bool = False,
    envs: list[dict[str, Any]] | None = None,
) -> dict[str, Any]:
    url = f"fuchsia-pkg://fuchsia.com/{pkg}#meta/{comp}.cm"
    entry: dict[str, Any] = {
        "test": {
            "name": url,
            "package_url": url,
            "label": label,
            "source_label": source_label,
            "os": "fuchsia",
            "cpu": cpu,
            "log_settings": {"max_severity": max_severity},
        },
        "build_only": build_only,
    }
    if envs is None:
        envs = [{"dimensions": {"device_type": "AEMU"}}]
    entry["environments"] = envs
    return entry


def _host_entry(
    name: str,
    label: str,
    source_label: str = "",
    cpu: str = "x64",
    build_only: bool = False,
    envs: list[dict[str, Any]] | None = None,
) -> dict[str, Any]:
    entry: dict[str, Any] = {
        "test": {
            "name": name,
            "label": label,
            "source_label": source_label or label,
            "os": "linux",
            "cpu": cpu,
        },
        "build_only": build_only,
        "environments": (
            [{"dimensions": {"cpu": cpu, "os": "Linux"}}]
            if envs is None
            else envs
        ),
    }
    return entry


class VerifyTestsJsonParityTest(unittest.TestCase):
    _shim_temp_dir: tempfile.TemporaryDirectory[str] | None = None
    _orig_path: str | None = None

    @classmethod
    def setUpClass(cls) -> None:
        super().setUpClass()
        cls._shim_temp_dir = tempfile.TemporaryDirectory()
        shim_bin = pathlib.Path(cls._shim_temp_dir.name)
        _install_hermetic_tool_shims(
            shim_bin, include_git=(shutil.which("git") is None)
        )
        cls._orig_path = os.environ.get("PATH")
        existing = cls._orig_path or ""
        os.environ["PATH"] = (
            f"{shim_bin}:{existing}" if existing else str(shim_bin)
        )

    @classmethod
    def tearDownClass(cls) -> None:
        if cls._shim_temp_dir is not None:
            if cls._orig_path is None:
                os.environ.pop("PATH", None)
            else:
                os.environ["PATH"] = cls._orig_path
            cls._shim_temp_dir.cleanup()
            cls._shim_temp_dir = None
        super().tearDownClass()

    def test_01_blind_spot_1_product_args_gn_does_not_inject_leaf_tests(
        self,
    ) -> None:
        original = (
            'import("//boards/x64.gni")\n'
            'import("//products/minimal.gni")\n'
            "target_labels = []\n"
            "host_labels = []\n"
        )
        rendered = vtp.build_product_args_gn(original)
        self.assertIn('target_labels += ["//bundles/buildbot/core"]', rendered)
        self.assertIn('host_labels += ["//bundles/buildbot/host"]', rendered)
        self.assertNotIn(":tests", rendered)

        # Idempotent if product roots are already configured.
        rendered_twice = vtp.build_product_args_gn(rendered)
        self.assertEqual(rendered, rendered_twice)

    def test_02_blind_spot_1_severed_parent_tests_edge_caught_immediately(
        self,
    ) -> None:
        # Parent `src/foo/BUILD.gn` severed `bar:tests`, so `bar-tests` vanished
        # from product `tests.json` even though `src/foo/bar/BUILD.gn` was not
        # edited directly.
        before = [
            _device_entry("bar-tests", source_label="//src/foo/bar:bar-tests")
        ]
        after: list[dict[str, Any]] = []
        states = {
            "//src/foo": vtp.PackageState(
                pkg_dir="src/foo",
                before_gn='group("tests") { deps = [ "bar:tests" ] }',
                after_gn='group("tests") { deps = [] }',
            )
        }
        findings = vtp.verify_parity(before, after, states)
        self.assertEqual(len(findings), 1)
        self.assertEqual(
            findings[0]["category"], "test_dropped_from_tests_json"
        )
        self.assertEqual(findings[0]["severity"], "ERROR")
        self.assertIn("bar-tests", findings[0]["message"])

    def test_03_blind_spot_2_off_cq_leaf_without_local_group_tests_discovered(
        self,
    ) -> None:
        leaf_gn = """
        rustc_library("leaf") {
          with_unit_tests = true
          sources = [ "src/lib.rs" ]
        }
        fuchsia_unittest_package("leaf-tests") {
          deps = [ ":leaf_test" ]
        }
        """
        gn_targets = vtp.extract_gn_test_targets(leaf_gn, "src/dev/leaf")
        uncovered = vtp.find_uncovered_gn_test_targets(gn_targets, [])
        self.assertTrue(
            any(t["label"] == "//src/dev/leaf:leaf-tests" for t in uncovered)
        )

        # Simulate `fx gn refs` walking from leaf -> mid -> highest enclosing
        # off-CQ developer root `//src/dev:developer_only_tests`.
        refs_graph = {
            "//src/dev/leaf:leaf-tests": ["//src/dev/mid:all_leaf_tests"],
            "//src/dev/leaf:leaf_test": [
                "//src/dev/leaf:leaf-tests",
                "//src/dev:host_dev_tests",
            ],
            "//src/dev/mid:all_leaf_tests": ["//src/dev:developer_only_tests"],
            "//src/dev:developer_only_tests": [],
            "//src/dev:host_dev_tests": [],
        }
        gn_files = {
            "src/dev/leaf": leaf_gn,
            "src/dev/mid": (
                'group("all_leaf_tests") { deps = [ "leaf:leaf-tests" ] }'
            ),
            "src/dev": (
                'group("developer_only_tests") { deps = [ "mid:all_leaf_tests" ] }\n'
                "if (is_host) {\n"
                '  group("host_dev_tests") { deps = [ "leaf:leaf_test" ] }\n'
                "}"
            ),
        }
        host_roots, dev_roots = vtp.resolve_highest_enclosing_groups(
            uncovered,
            lambda lbl: refs_graph.get(lbl, []),
            gn_files,
        )
        self.assertEqual(dev_roots, ["//src/dev:developer_only_tests"])
        self.assertIn("//src/dev:host_dev_tests", host_roots)

    def test_04_blind_spot_3_case1_full_removal_1_gn_to_1_bazel_passes(
        self,
    ) -> None:
        before = [_device_entry("foo-tests")]
        after = [
            _device_entry(
                "foo-tests",
                label="@@//src/foo:foo-tests",
                source_label="//src/foo:foo-tests",
            )
        ]
        states = {
            "//src/foo": vtp.PackageState(
                pkg_dir="src/foo",
                before_gn='fuchsia_unittest_package("foo-tests") {}',
                after_gn=None,
                after_bazel='fx_unittest_package(name = "foo-tests")',
            )
        }
        findings = vtp.verify_parity(before, after, states)
        self.assertEqual(findings, [])

    def test_05_blind_spot_3_case2_dual_build_requires_both_bazel_and_gn_dual(
        self,
    ) -> None:
        before = [_device_entry("foo-tests")]
        bazel_after = _device_entry(
            "foo-tests",
            label="@@//src/foo:foo-tests",
            source_label="//src/foo:foo-tests",
        )
        gn_dual_after = _device_entry(
            "foo-tests-gn",
            label="//src/foo:foo-tests(//build/toolchain/fuchsia:x64)",
            source_label="//src/foo:foo-tests",
        )
        states = {
            "//src/foo": vtp.PackageState(
                pkg_dir="src/foo",
                before_gn='fuchsia_unittest_package("foo-tests") {}',
                after_gn=(
                    'group("tests") { deps = [ ":foo-tests" ] }\n'
                    "## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##\n"
                    'fuchsia_unittest_package("foo-tests") {\n'
                    '  package_name = "foo-tests-gn"\n'
                    "}\n"
                ),
                after_bazel='fx_unittest_package(name = "foo-tests")',
            )
        }
        # Both present -> PASS
        self.assertEqual(
            vtp.verify_parity(before, [bazel_after, gn_dual_after], states), []
        )

        # Dropping `-gn` silently -> ERROR
        drop_gn_findings = vtp.verify_parity(before, [bazel_after], states)
        self.assertEqual(len(drop_gn_findings), 1)
        self.assertEqual(
            drop_gn_findings[0]["category"], "dual_build_gn_variant_dropped"
        )

        # Attested via `Skipped-Dual-Tests:` -> PASS
        attested_findings = vtp.verify_parity(
            before,
            [bazel_after],
            states,
            commit_msg="Skipped-Dual-Tests: //src/foo:foo-tests\nBug: 12345\n",
        )
        self.assertEqual(attested_findings, [])

        # Missing Bazel export (only `-gn` present) -> ERROR
        drop_bz_findings = vtp.verify_parity(before, [gn_dual_after], states)
        self.assertEqual(len(drop_bz_findings), 1)
        self.assertEqual(
            drop_bz_findings[0]["category"], "dual_build_bazel_variant_missing"
        )

    def test_06_blind_spot_3_case3_gn2gone_retires_gn_dual_cleanly_if_envs_kept(
        self,
    ) -> None:
        nuc_env = {"dimensions": {"device_type": "NUC11"}}
        emu_env = {"dimensions": {"device_type": "AEMU"}}
        before = [
            _device_entry(
                "foo-tests",
                label="@@//src/foo:foo-tests",
                source_label="//src/foo:foo-tests",
                envs=[emu_env, nuc_env],
            ),
            _device_entry(
                "foo-tests-gn",
                label="//src/foo:foo-tests(//build/toolchain/fuchsia:x64)",
                source_label="//src/foo:foo-tests",
                envs=[emu_env, nuc_env],
            ),
        ]
        states = {
            "//src/foo": vtp.PackageState(
                pkg_dir="src/foo",
                before_gn=(
                    "## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##\n"
                    'fuchsia_unittest_package("foo-tests") {}\n'
                ),
                after_gn=None,
                after_bazel='fx_unittest_package(name = "foo-tests")',
            )
        }
        # Bazel keeps all environments of `gn_dual` -> PASS
        after_ok = [
            _device_entry(
                "foo-tests",
                label="@@//src/foo:foo-tests",
                source_label="//src/foo:foo-tests",
                envs=[emu_env, nuc_env],
            )
        ]
        self.assertEqual(vtp.verify_parity(before, after_ok, states), [])

        # Bazel lost `NUC11` environment that `gn_dual` had -> ERROR
        after_lost_nuc = [
            _device_entry(
                "foo-tests",
                label="@@//src/foo:foo-tests",
                source_label="//src/foo:foo-tests",
                envs=[emu_env],
            )
        ]
        lost_findings = vtp.verify_parity(before, after_lost_nuc, states)
        self.assertTrue(
            any(
                f["category"] == "test_environments_lost" for f in lost_findings
            )
        )

    def test_07_blind_spot_4_build_only_max_severity_and_cpu_regressions(
        self,
    ) -> None:
        before = [_device_entry("foo-tests", max_severity="ERROR")]
        states = {
            "//src/foo": vtp.PackageState(
                pkg_dir="src/foo",
                before_gn='fuchsia_unittest_package("foo-tests") {}',
                after_gn=None,
                after_bazel='fx_unittest_package(name = "foo-tests")',
            )
        }

        # Flipping `build_only: False -> True` -> ERROR
        after_bo = [
            _device_entry(
                "foo-tests",
                label="@@//src/foo:foo-tests",
                max_severity="ERROR",
                build_only=True,
                envs=[],
            )
        ]
        bo_findings = vtp.verify_parity(before, after_bo, states)
        self.assertEqual(len(bo_findings), 1)
        self.assertEqual(bo_findings[0]["category"], "test_became_build_only")

        # Changing `max_severity` (`ERROR` -> `WARN`) -> ERROR
        after_sev = [
            _device_entry(
                "foo-tests",
                label="@@//src/foo:foo-tests",
                max_severity="WARN",
            )
        ]
        sev_findings = vtp.verify_parity(before, after_sev, states)
        self.assertEqual(len(sev_findings), 1)
        self.assertEqual(
            sev_findings[0]["category"], "test_max_severity_changed"
        )

        # Changing `cpu` (`x64` -> `arm64`) -> ERROR
        after_cpu = [
            _device_entry(
                "foo-tests",
                label="@@//src/foo:foo-tests",
                max_severity="ERROR",
                cpu="arm64",
            )
        ]
        cpu_findings = vtp.verify_parity(before, after_cpu, states)
        self.assertEqual(len(cpu_findings), 1)
        self.assertEqual(cpu_findings[0]["category"], "test_cpu_mismatch")

    def test_08_blind_spot_5_renamed_tests_bijection_and_collapse_rejection(
        self,
    ) -> None:
        before = [
            _device_entry("old-a-tests", source_label="//src/foo:old-a-tests"),
            _device_entry("old-b-tests", source_label="//src/foo:old-b-tests"),
        ]
        after = [
            _device_entry(
                "new-a-tests",
                label="@@//src/foo:new-a-tests",
                source_label="//src/foo:new-a-tests",
            ),
            _device_entry(
                "new-b-tests",
                label="@@//src/foo:new-b-tests",
                source_label="//src/foo:new-b-tests",
            ),
        ]
        states = {
            "//src/foo": vtp.PackageState(
                pkg_dir="src/foo",
                before_gn="...",
                after_gn=None,
                after_bazel="...",
            )
        }
        valid_msg = (
            "Migrate foo\n\n"
            "Renamed-Tests: //src/foo:old-a-tests -> //src/foo:new-a-tests\n"
            "Renamed-Tests: //src/foo:old-b-tests -> //src/foo:new-b-tests\n"
        )
        self.assertEqual(
            vtp.verify_parity(before, after, states, commit_msg=valid_msg), []
        )

        # Collapsing 2 old tests onto the same new target -> ERROR
        collapse_msg = (
            "Migrate foo\n\n"
            "Renamed-Tests: //src/foo:old-a-tests -> //src/foo:new-a-tests\n"
            "Renamed-Tests: //src/foo:old-b-tests -> //src/foo:new-a-tests\n"
        )
        collapse_findings = vtp.verify_parity(
            before, after[:1], states, commit_msg=collapse_msg
        )
        self.assertTrue(
            any(
                f["category"] == "invalid_renamed_tests_footer"
                for f in collapse_findings
            )
        )

    def test_09_blind_spot_5_host_wrapper_1_to_1_match_and_collapse_detection(
        self,
    ) -> None:
        # Single `//src/foo:foo_lib_test` -> `//src/foo:foo_host_test` passes.
        before_single = [
            _host_entry(
                "host_x64/foo_lib_test",
                "//src/foo:foo_lib_test(//build/toolchain:host_x64)",
                "//src/foo:foo_lib_test",
            )
        ]
        after_single = [
            _host_entry(
                "@@//src/foo:foo_host_test",
                "@@//src/foo:foo_host_test",
                "//src/foo:foo_host_test",
            )
        ]
        states = {
            "//src/foo": vtp.PackageState(
                pkg_dir="src/foo",
                before_gn='rustc_library("foo") { with_unit_tests = true }',
                after_gn=None,
                after_bazel='wrap_host_rust_test(name = "foo_host_test")',
            )
        }
        self.assertEqual(
            vtp.verify_parity(before_single, after_single, states), []
        )

        # Two baseline host tests sharing a stem (`foo_lib_test` + `foo_bin_test`)
        # collapsed into only ONE Bazel host test MUST fail!
        before_two = before_single + [
            _host_entry(
                "host_x64/foo_bin_test",
                "//src/foo:foo_bin_test(//build/toolchain:host_x64)",
                "//src/foo:foo_bin_test",
            )
        ]
        two_findings = vtp.verify_parity(before_two, after_single, states)
        self.assertEqual(len(two_findings), 1)
        self.assertEqual(
            two_findings[0]["category"], "test_dropped_from_tests_json"
        )

    def test_10_blind_spot_6_remediation_forbids_disputed_findings_waiver(
        self,
    ) -> None:
        self.assertIn("MUST NEVER waive dropped tests", vtp.REMEDIATION)
        self.assertIn("disputed_findings", vtp.REMEDIATION)
        self.assertNotIn("say why in `disputed_findings`", vtp.REMEDIATION)
        self.assertNotIn("say why in disputed_findings", vtp.REMEDIATION)

    def test_11_cli_end_to_end_exit_codes(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            before_p = pathlib.Path(tmp) / "before.json"
            after_p = pathlib.Path(tmp) / "after.json"
            states_p = pathlib.Path(tmp) / "states.json"
            before_p.write_text(
                json.dumps([_device_entry("foo-tests")]), encoding="utf-8"
            )
            after_p.write_text(json.dumps([]), encoding="utf-8")
            states_p.write_text(
                json.dumps(
                    {
                        "//src/foo": {
                            "before_gn": 'fuchsia_unittest_package("foo-tests") {}',
                            "after_gn": None,
                            "after_bazel": "",
                        }
                    }
                ),
                encoding="utf-8",
            )
            buf = io.StringIO()
            with contextlib.redirect_stdout(buf):
                rc = vtp.main(
                    [
                        "--before",
                        str(before_p),
                        "--after",
                        str(after_p),
                        "--package-states-json",
                        str(states_p),
                    ]
                )
            self.assertEqual(rc, 1)

    def _init_git_repo(self, root: pathlib.Path) -> None:
        for cmd in (
            ["git", "init", "-q"],
            ["git", "config", "user.email", "test@example.com"],
            ["git", "config", "user.name", "Test User"],
        ):
            subprocess.run(cmd, cwd=root, check=True)

    def test_12_test_suite_layout_allows_area_boundary_and_flags_leaf_shim(
        self,
    ) -> None:
        script = _CHECKS_DIR / "test_suite_layout.sh"
        py_bin_dir = str(pathlib.Path(sys.executable).parent)
        env = dict(os.environ)
        env["PATH"] = f"{py_bin_dir}:{env.get('PATH', '')}"
        env["PYTHONDONTWRITEBYTECODE"] = "1"

        with tempfile.TemporaryDirectory() as tmp:
            repo = pathlib.Path(tmp)
            self._init_git_repo(repo)
            (repo / "src").mkdir(parents=True)
            (repo / "src" / "BUILD.gn").write_text(
                'group("tests") { testonly = true deps = [ "hwinfo:tests" ] }\n',
                encoding="utf-8",
            )
            (repo / "src" / "hwinfo" / "mock").mkdir(parents=True)
            (repo / "src" / "hwinfo" / "BUILD.gn").write_text(
                'group("tests") { testonly = true deps = [ "mock:tests" ] }\n',
                encoding="utf-8",
            )
            (repo / "src" / "hwinfo" / "mock" / "BUILD.gn").write_text(
                'fuchsia_unittest_package("mock-tests") {}\n'
                'group("tests") { testonly = true deps = [ ":mock-tests" ] }\n',
                encoding="utf-8",
            )
            subprocess.run(["git", "add", "."], cwd=repo, check=True)
            subprocess.run(
                ["git", "commit", "-q", "-m", "base"], cwd=repo, check=True
            )
            base_sha = subprocess.check_output(
                ["git", "rev-parse", "HEAD"], cwd=repo, text=True
            ).strip()

            # Case A: Leaf `src/hwinfo/mock/BUILD.gn` kept as shim while parent
            # `src/hwinfo/BUILD.gn` wraps `mock:tests` -> flagged!
            (repo / "src" / "hwinfo" / "mock" / "BUILD.gn").write_text(
                'import("//build/bazel/bazel_test_suite.gni")\n'
                'bazel_test_suite("bazel_tests") {\n'
                '  target_tests = [ "//src/hwinfo/mock:mock-tests" ]\n'
                "}\n"
                'group("tests") { testonly = true deps = [ ":bazel_tests" ] }\n',
                encoding="utf-8",
            )
            (repo / "src" / "hwinfo" / "mock" / "BUILD.bazel").write_text(
                'fx_unittest_package(name = "mock-tests", unit_tests = [":bin"])\n',
                encoding="utf-8",
            )
            run_env = dict(
                env, PLANTER_WORKDIR=str(repo), PLANTER_CHANGE_BASE=base_sha
            )
            out_a = json.loads(
                subprocess.check_output(
                    ["bash", str(script)], env=run_env, text=True
                )
            )
            cats_a = {f["category"] for f in out_a}
            self.assertIn("bazel_test_suite_only_build_gn", cats_a)

            # Case B: Move `bazel_test_suite` to `src/hwinfo/BUILD.gn` (the
            # `group("tests")` area boundary under umbrella `src/BUILD.gn`) and
            # delete `src/hwinfo/mock/BUILD.gn` -> 0 findings!
            (repo / "src" / "hwinfo" / "mock" / "BUILD.gn").unlink()
            (repo / "src" / "hwinfo" / "BUILD.gn").write_text(
                'import("//build/bazel/bazel_test_suite.gni")\n'
                'bazel_test_suite("bazel_tests") {\n'
                '  target_tests = [ "//src/hwinfo/mock:mock-tests" ]\n'
                "}\n"
                'group("tests") { testonly = true deps = [ ":bazel_tests" ] }\n',
                encoding="utf-8",
            )
            out_b = json.loads(
                subprocess.check_output(
                    ["bash", str(script)], env=run_env, text=True
                )
            )
            self.assertEqual(out_b, [])

    def test_13_commit_message_format_rejects_agent_deleted_tests_and_illegal_build_only(
        self,
    ) -> None:
        script = _CHECKS_DIR / "commit_message_format.sh"
        py_bin_dir = str(pathlib.Path(sys.executable).parent)
        env = dict(os.environ)
        env["PATH"] = f"{py_bin_dir}:{env.get('PATH', '')}"
        env["PYTHONDONTWRITEBYTECODE"] = "1"

        with tempfile.TemporaryDirectory() as tmp:
            repo = pathlib.Path(tmp)
            self._init_git_repo(repo)
            (repo / "src" / "foo").mkdir(parents=True)
            (repo / "src" / "foo" / "BUILD.gn").write_text(
                'source_set("foo") { sources = [ "foo.cc" ] }\n'
                'fuchsia_unittest_package("foo-tests") {}\n',
                encoding="utf-8",
            )
            subprocess.run(["git", "add", "."], cwd=repo, check=True)
            subprocess.run(
                ["git", "commit", "-q", "-m", "base"], cwd=repo, check=True
            )
            base_sha = subprocess.check_output(
                ["git", "rev-parse", "HEAD"], cwd=repo, text=True
            ).strip()

            # Commit touching a test rule + claiming Build-Only: true +
            # agent TAG: planter + Deleted-Tests: -> both rejected!
            (repo / "src" / "foo" / "BUILD.gn").write_text(
                'source_set("foo") { sources = [ "foo.cc" ] }\n',
                encoding="utf-8",
            )
            (repo / "src" / "foo" / "BUILD.bazel").write_text(
                'fx_cc_library(name = "foo", srcs = ["foo.cc"], alwayslink = True)\n',
                encoding="utf-8",
            )
            subprocess.run(["git", "add", "."], cwd=repo, check=True)
            commit_msg = (
                "[bazel_migration] //src/foo\n\n"
                "Bug: 123456\n"
                "Deleted-Tests: //src/foo:foo-tests\n"
                "Build-Only: true\n"
                "Bazel-Migration-Target: //src/foo:foo\n"
                "Test: fx bazel build --config=fuchsia_platform @//src/foo\n"
                "Test: fx host-tool shac check\n"
                "TAG: planter\n"
                "Change-Id: I1234567890abcdef1234567890abcdef12345678\n"
            )
            subprocess.run(
                ["git", "commit", "-q", "-m", commit_msg], cwd=repo, check=True
            )
            run_env = dict(
                env,
                PLANTER_WORKDIR=str(repo),
                PLANTER_TARGET_DIR="src/foo",
                PLANTER_CHANGE_BASE=base_sha,
            )
            out_bad = json.loads(
                subprocess.check_output(
                    ["bash", str(script)], env=run_env, text=True
                )
            )
            bad_cats = {f["category"] for f in out_bad}
            self.assertIn("forbidden_deleted_tests_footer", bad_cats)
            self.assertIn("illegal_build_only_footer", bad_cats)

            # Now test a pure library-only migration where test rules in
            # BUILD.gn are untouched and BUILD.bazel adds zero test rules ->
            # Build-Only: true IS permitted!
            (repo / "src" / "foo" / "BUILD.gn").write_text(
                'fuchsia_unittest_package("foo-tests") {}\n'
                "## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##\n"
                'source_set("foo") { sources = [ "foo.cc" ] }\n',
                encoding="utf-8",
            )
            subprocess.run(["git", "add", "."], cwd=repo, check=True)
            clean_msg = (
                "[bazel_migration] //src/foo\n\n"
                "Build-Only: true\n"
                "Bazel-Migration-Target: //src/foo:foo\n"
                "Test: fx bazel build --config=fuchsia_platform @//src/foo\n"
                "Test: fx host-tool shac check\n"
                "TAG: planter\n"
                "Change-Id: I1234567890abcdef1234567890abcdef12345678\n"
            )
            subprocess.run(
                ["git", "commit", "--amend", "-q", "-m", clean_msg],
                cwd=repo,
                check=True,
            )
            out_ok = json.loads(
                subprocess.check_output(
                    ["bash", str(script)], env=run_env, text=True
                )
            )
            ok_cats = {f["category"] for f in out_ok}
            self.assertNotIn("illegal_build_only_footer", ok_cats)
            self.assertNotIn("forbidden_deleted_tests_footer", ok_cats)

    def test_14_hermetic_gitless_shim_executes_shell_checks(self) -> None:
        layout_script = _CHECKS_DIR / "test_suite_layout.sh"
        msg_script = _CHECKS_DIR / "commit_message_format.sh"

        with tempfile.TemporaryDirectory() as shim_tmp, tempfile.TemporaryDirectory() as repo_tmp:
            shim_dir = pathlib.Path(shim_tmp) / "shims"
            sys_bin = pathlib.Path(shim_tmp) / "sys_bin"
            sys_bin.mkdir(parents=True, exist_ok=True)
            for tool in ("sh", "bash"):
                found_tool = shutil.which(tool)
                if found_tool:
                    (sys_bin / tool).symlink_to(found_tool)
            _install_hermetic_tool_shims(shim_dir, include_git=True)
            bash_bin = (
                str(sys_bin / "bash")
                if (sys_bin / "bash").exists()
                else "/bin/bash"
            )
            hermetic_env = {
                "PATH": f"{shim_dir}:{sys_bin}",
                "PYTHONDONTWRITEBYTECODE": "1",
                "HOME": shim_tmp,
            }
            repo = pathlib.Path(repo_tmp)
            git_exe = str(shim_dir / "git")
            subprocess.run(
                [git_exe, "init", "-q"],
                cwd=repo,
                env=hermetic_env,
                check=True,
            )
            (repo / "src" / "foo").mkdir(parents=True)
            (repo / "src" / "foo" / "BUILD.gn").write_text(
                'source_set("foo") { sources = [ "foo.cc" ] }\n'
                'fuchsia_unittest_package("foo-tests") {}\n',
                encoding="utf-8",
            )
            subprocess.run(
                [git_exe, "add", "."],
                cwd=repo,
                env=hermetic_env,
                check=True,
            )
            subprocess.run(
                [git_exe, "commit", "-q", "-m", "base"],
                cwd=repo,
                env=hermetic_env,
                check=True,
            )
            base_sha = subprocess.check_output(
                [git_exe, "rev-parse", "HEAD"],
                cwd=repo,
                env=hermetic_env,
                text=True,
            ).strip()

            (repo / "src" / "foo" / "BUILD.gn").write_text(
                'source_set("foo") { sources = [ "foo.cc" ] }\n',
                encoding="utf-8",
            )
            (repo / "src" / "foo" / "BUILD.bazel").write_text(
                'fx_cc_library(name = "foo", srcs = ["foo.cc"], alwayslink = True)\n',
                encoding="utf-8",
            )
            subprocess.run(
                [git_exe, "add", "."],
                cwd=repo,
                env=hermetic_env,
                check=True,
            )
            subprocess.run(
                [
                    git_exe,
                    "commit",
                    "-q",
                    "-m",
                    (
                        "[bazel_migration] //src/foo\n\n"
                        "Build-Only: true\n"
                        "Bazel-Migration-Target: //src/foo:foo\n"
                        "Test: fx bazel build --config=fuchsia_platform @//src/foo\n"
                        "Test: fx host-tool shac check\n"
                        "TAG: planter\n"
                        "Change-Id: I1234567890abcdef1234567890abcdef12345678\n"
                    ),
                ],
                cwd=repo,
                env=hermetic_env,
                check=True,
            )
            run_env = dict(
                hermetic_env,
                PLANTER_WORKDIR=str(repo),
                PLANTER_TARGET_DIR="src/foo",
                PLANTER_CHANGE_BASE=base_sha,
            )
            layout_out = json.loads(
                subprocess.check_output(
                    [bash_bin, str(layout_script)], env=run_env, text=True
                )
            )
            self.assertEqual(layout_out, [])
            msg_out = json.loads(
                subprocess.check_output(
                    [bash_bin, str(msg_script)], env=run_env, text=True
                )
            )
            msg_cats = {f["category"] for f in msg_out}
            self.assertIn("illegal_build_only_footer", msg_cats)


def main() -> None:
    global _CHECKS_DIR
    parser = argparse.ArgumentParser(add_help=False)
    parser.add_argument("--checks-dir", default="")
    args, remaining = parser.parse_known_args()
    if args.checks_dir:
        _CHECKS_DIR = pathlib.Path(args.checks_dir).resolve()
    unittest.main(argv=[sys.argv[0]] + remaining)


if __name__ == "__main__":
    main()
