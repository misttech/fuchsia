#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import json
import os
import pathlib
import shutil
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent.parent))

import check_bazel_test_parity as parity

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


def _install_hermetic_git_shim(bin_dir: pathlib.Path) -> pathlib.Path:
    """Installs a hermetic Python-backed `git` shim for git-less Swarming hosts."""
    bin_dir.mkdir(parents=True, exist_ok=True)
    py_exe = os.path.abspath(sys.executable)
    fake_git_py = bin_dir / "_fake_git.py"
    fake_git_py.write_text(_FAKE_GIT_PY, encoding="utf-8")
    git_wrapper = bin_dir / "git"
    git_wrapper.write_text(
        f'#!/bin/sh\nexec "{py_exe}" -B "{fake_git_py}" "$@"\n',
        encoding="utf-8",
    )
    git_wrapper.chmod(0o755)
    return git_wrapper


class CheckBazelTestParityTest(unittest.TestCase):
    _shim_temp_dir: tempfile.TemporaryDirectory[str] | None = None
    _orig_path: str | None = None

    @classmethod
    def setUpClass(cls) -> None:
        super().setUpClass()
        if shutil.which("git") is None:
            cls._shim_temp_dir = tempfile.TemporaryDirectory()
            shim_bin = pathlib.Path(cls._shim_temp_dir.name)
            _install_hermetic_git_shim(shim_bin)
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

    def setUp(self) -> None:
        self.temp_dir = self.enterContext(tempfile.TemporaryDirectory())
        self.root = pathlib.Path(self.temp_dir)
        self._git("init", "-q")
        self._git("config", "user.email", "test@fuchsia.dev")
        self._git("config", "user.name", "Fuchsia Test")

    def _git(self, *args: str) -> None:
        env = dict(os.environ)
        env["GIT_AUTHOR_DATE"] = "2026-01-01T00:00:00Z"
        env["GIT_COMMITTER_DATE"] = "2026-01-01T00:00:00Z"
        subprocess.run(
            ["git", "-C", str(self.root), *args],
            check=True,
            capture_output=True,
            text=True,
            env=env,
        )

    def _write_files(self, files: dict[str, str | None]) -> None:
        for rel, content in files.items():
            target = self.root / rel
            if content is None:
                if target.exists():
                    target.unlink()
            else:
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_text(content, encoding="utf-8")

    def _commit(self, files: dict[str, str | None], message: str) -> None:
        self._write_files(files)
        self._git("add", "-A")
        self._git("commit", "-q", "-m", message, "--allow-empty")

    def test_01_clean_wired_migration_with_inner_binary_consolidation(
        self,
    ) -> None:
        self._commit(
            {
                "src/foo/src/lib.rs": "#[test]\nfn it_works() {}\n",
                "src/foo/BUILD.gn": (
                    'rustc_test("foo_bin_test") {\n'
                    '  sources = [ "src/lib.rs" ]\n'
                    "}\n"
                    'fuchsia_unittest_package("foo_tests") {\n'
                    '  deps = [ ":foo_bin_test" ]\n'
                    "}\n"
                    'group("tests") {\n'
                    "  testonly = true\n"
                    '  deps = [ ":foo_tests" ]\n'
                    "}\n"
                ),
            },
            "Initial GN package",
        )
        self._commit(
            {
                "src/foo/BUILD.bazel": (
                    "rustc_test(\n"
                    '    name = "foo_bin_test",\n'
                    '    srcs = ["src/lib.rs"],\n'
                    ")\n"
                    "fx_unittest_package(\n"
                    '    name = "foo_tests",\n'
                    '    unit_tests = [":foo_bin_test"],\n'
                    ")\n"
                ),
                "src/foo/BUILD.gn": (
                    'bazel_test_suite("bazel_tests") {\n'
                    '  target_tests = [ "//src/foo:foo_tests" ]\n'
                    "}\n"
                    'group("tests") {\n'
                    "  testonly = true\n"
                    '  deps = [ ":bazel_tests" ]\n'
                    "}\n"
                ),
            },
            "[bazel_migration] //src/foo\n\nBazel-Migration-Target: //src/foo:foo_tests",
        )
        findings = parity.analyze_repository(self.root)
        self.assertEqual(findings, [])

    def test_02_ancestor_test_suite_wiring_passes(self) -> None:
        self._commit(
            {
                "src/area/BUILD.gn": (
                    'bazel_test_suite("bazel_tests") {\n'
                    '  host_tests = [ "//src/area:host_tests" ]\n'
                    "}\n"
                    'group("tests") {\n'
                    "  testonly = true\n"
                    '  deps = [ ":bazel_tests" ]\n'
                    "}\n"
                ),
                "src/area/BUILD.bazel": (
                    "test_suite(\n"
                    '    name = "host_tests",\n'
                    "    target_compatible_with = HOST_OS_CONSTRAINTS,\n"
                    '    tests = ["//src/area/leaf:leaf_test"],\n'
                    ")\n"
                ),
                "src/area/leaf/test.py": "assert True\n",
                "src/area/leaf/BUILD.gn": (
                    'python_host_test("leaf_test") {\n'
                    '  main_source = "test.py"\n'
                    "}\n"
                ),
            },
            "Base",
        )
        self._commit(
            {
                "src/area/leaf/BUILD.gn": None,
                "src/area/leaf/BUILD.bazel": (
                    "host_py_test(\n"
                    '    name = "leaf_test",\n'
                    '    srcs = ["test.py"],\n'
                    ")\n"
                ),
            },
            "[bazel_migration] //src/area/leaf",
        )
        findings = parity.analyze_repository(self.root)
        self.assertEqual(findings, [])

    def test_03_class1_unwired_bazel_test_target_errors(self) -> None:
        self._commit({"src/foo/test.cc": "int main() { return 0; }\n"}, "Base")
        self._commit(
            {
                "src/foo/BUILD.bazel": (
                    "fx_unittest_package(\n"
                    '    name = "orphan_tests",\n'
                    '    srcs = ["test.cc"],\n'
                    ")\n"
                ),
            },
            "[bazel_migration] //src/foo",
        )
        findings = parity.analyze_repository(self.root)
        self.assertEqual(len(findings), 1)
        self.assertEqual(findings[0].level, "error")
        self.assertEqual(findings[0].properties, {})
        self.assertIn("🚨 [TEST PARITY ALERT", findings[0].message)
        self.assertIn("UNWIRED BAZEL TEST TARGET", findings[0].message)
        self.assertIn("//src/foo:orphan_tests", findings[0].message)

    def test_04_class1_manual_tag_errors_unless_allow_comment(self) -> None:
        self._commit({"src/foo/test.cc": "int main() { return 0; }\n"}, "Base")
        self._commit(
            {
                "src/foo/BUILD.bazel": (
                    "host_cc_test(\n"
                    '    name = "manual_test",\n'
                    '    srcs = ["test.cc"],\n'
                    '    tags = ["manual"],\n'
                    ")\n"
                ),
                "src/foo/BUILD.gn": (
                    'bazel_test_suite("bazel_tests") {\n'
                    '  host_tests = [ "//src/foo:manual_test" ]\n'
                    "}\n"
                    'group("tests") {\n'
                    '  deps = [ ":bazel_tests" ]\n'
                    "}\n"
                ),
            },
            "Add manual test",
        )
        findings = parity.analyze_repository(self.root)
        self.assertEqual(len(findings), 1)
        self.assertEqual(findings[0].level, "error")
        self.assertEqual(findings[0].properties, {})
        self.assertIn('tags = ["manual"]', findings[0].message)

        # Adding `# test-parity-allow-manual: <reason>` clears the manual error.
        self._commit(
            {
                "src/foo/BUILD.bazel": (
                    "# test-parity-allow-manual: invoked via custom host runner\n"
                    "host_cc_test(\n"
                    '    name = "manual_test",\n'
                    '    srcs = ["test.cc"],\n'
                    '    tags = ["manual"],\n'
                    ")\n"
                ),
            },
            "Document manual opt-out",
        )
        self.assertEqual(parity.analyze_repository(self.root), [])

    def test_05_fm2_bazel_test_suite_inside_if_is_host_errors(self) -> None:
        self._commit({"src/foo/test.py": "pass\n"}, "Base")
        self._commit(
            {
                "src/foo/BUILD.bazel": (
                    'host_py_test(name = "foo_test", srcs = ["test.py"])\n'
                ),
                "src/foo/BUILD.gn": (
                    "if (is_host) {\n"
                    '  bazel_test_suite("bazel_tests") {\n'
                    '    host_tests = [ "//src/foo:foo_test" ]\n'
                    "  }\n"
                    "}\n"
                    'group("tests") {\n'
                    '  deps = [ ":bazel_tests($host_toolchain)" ]\n'
                    "}\n"
                ),
            },
            "[bazel_migration] //src/foo",
        )
        findings = parity.analyze_repository(self.root)
        self.assertTrue(
            any(
                "FM2: bazel_test_suite INSIDE if (is_host)" in f.message
                for f in findings
            ),
            [f.message for f in findings],
        )
        for f in findings:
            self.assertEqual(f.properties, {})

    def test_06_fm4_host_test_in_target_tests_errors(self) -> None:
        self._commit({"src/foo/test.py": "pass\n"}, "Base")
        self._commit(
            {
                "src/foo/BUILD.bazel": (
                    'host_py_test(name = "foo_test", srcs = ["test.py"])\n'
                ),
                "src/foo/BUILD.gn": (
                    'bazel_test_suite("bazel_tests") {\n'
                    '  target_tests = [ "//src/foo:foo_test" ]\n'
                    "}\n"
                    'group("tests") {\n'
                    '  deps = [ ":bazel_tests" ]\n'
                    "}\n"
                ),
            },
            "[bazel_migration] //src/foo",
        )
        findings = parity.analyze_repository(self.root)
        self.assertTrue(
            any(
                "FM4: HOST TEST IN target_tests" in f.message for f in findings
            ),
            [f.message for f in findings],
        )

    def test_07_fm4_device_package_in_host_tests_errors(self) -> None:
        self._commit({"src/foo/test.cc": "int main() {}\n"}, "Base")
        self._commit(
            {
                "src/foo/BUILD.bazel": (
                    'fx_unittest_package(name = "foo_tests", srcs = ["test.cc"])\n'
                ),
                "src/foo/BUILD.gn": (
                    'bazel_test_suite("bazel_tests") {\n'
                    '  host_tests = [ "//src/foo:foo_tests" ]\n'
                    "}\n"
                    'group("tests") {\n'
                    '  deps = [ ":bazel_tests" ]\n'
                    "}\n"
                ),
            },
            "[bazel_migration] //src/foo",
        )
        findings = parity.analyze_repository(self.root)
        self.assertTrue(
            any(
                "FM4: DEVICE TEST PACKAGE IN host_tests" in f.message
                for f in findings
            ),
            [f.message for f in findings],
        )

    def test_08_fm3_nested_host_test_suite_missing_host_os_constraints(
        self,
    ) -> None:
        self._commit({"src/foo/test.py": "pass\n"}, "Base")
        self._commit(
            {
                "src/foo/BUILD.bazel": (
                    'host_py_test(name = "foo_test", srcs = ["test.py"])\n'
                    "test_suite(\n"
                    '    name = "host_tests",\n'
                    '    tests = [":foo_test"],\n'
                    ")\n"
                ),
                "src/foo/BUILD.gn": (
                    'bazel_test_suite("bazel_tests") {\n'
                    '  host_tests = [ "//src/foo:host_tests" ]\n'
                    "}\n"
                    'group("tests") {\n'
                    '  deps = [ ":bazel_tests" ]\n'
                    "}\n"
                ),
            },
            "[bazel_migration] //src/foo",
        )
        findings = parity.analyze_repository(self.root)
        self.assertEqual(len(findings), 1)
        self.assertIn(
            "FM3: HOST test_suite MISSING HOST_OS_CONSTRAINTS",
            findings[0].message,
        )
        self.assertEqual(findings[0].properties, {})

    def test_09_class1_removed_gn_test_while_sources_exist_errors(self) -> None:
        self._commit(
            {
                "src/foo/lib.cc": "int lib() { return 1; }\n",
                "src/foo/foo_test.cc": "int main() { return 0; }\n",
                "src/foo/BUILD.gn": (
                    'source_set("foo") { sources = [ "lib.cc" ] }\n'
                    'test("foo_test") { sources = [ "foo_test.cc" ] }\n'
                    'fuchsia_unittest_package("foo_tests") {\n'
                    '  deps = [ ":foo_test" ]\n'
                    "}\n"
                    'group("tests") { deps = [ ":foo_tests" ] }\n'
                ),
            },
            "Base",
        )
        # Migrate only the library, silently dropping foo_tests while foo_test.cc
        # remains on disk! Even if author tries Deleted-Tests:, Class 1 CANNOT be waived!
        self._commit(
            {
                "src/foo/BUILD.bazel": (
                    'cc_library(name = "foo", srcs = ["lib.cc"])\n'
                ),
                "src/foo/BUILD.gn": 'group("tests") { deps = [] }\n',
            },
            "[bazel_migration] //src/foo\n\nBug: 42000001\nDeleted-Tests: //src/foo:foo_tests",
        )
        findings = parity.analyze_repository(self.root)
        self.assertTrue(
            any(
                "CLASS 1 SILENT ORPHANING (REMOVED TEST TARGET WITH EXISTING SOURCES)"
                in f.message
                for f in findings
            ),
            [f.message for f in findings],
        )
        for f in findings:
            self.assertEqual(f.properties, {})

    def test_10_class1_dual_host_and_device_gn_test_missing_host_leg_errors(
        self,
    ) -> None:
        self._commit(
            {
                "src/foo/src/lib.rs": "#[cfg(test)]\nmod tests { #[test] fn t() {} }\n",
                "src/foo/BUILD.gn": (
                    'rustc_library("foo") {\n'
                    "  with_unit_tests = true\n"
                    '  sources = [ "src/lib.rs" ]\n'
                    "}\n"
                    'fuchsia_unittest_package("foo_tests") {\n'
                    '  deps = [ ":foo_test" ]\n'
                    "}\n"
                    'group("tests") {\n'
                    '  deps = [ ":foo_tests", ":foo_test($host_toolchain)" ]\n'
                    "}\n"
                ),
            },
            "Base dual host+device Rust test",
        )
        # Only migrate the device package, dropping the host leg!
        self._commit(
            {
                "src/foo/BUILD.bazel": (
                    'rustc_library(name = "foo", srcs = ["src/lib.rs"])\n'
                    'rustc_test(name = "foo_bin_test", crate = ":foo")\n'
                    "fx_unittest_package(\n"
                    '    name = "foo_tests",\n'
                    '    unit_tests = [":foo_bin_test"],\n'
                    ")\n"
                ),
                "src/foo/BUILD.gn": (
                    'bazel_test_suite("bazel_tests") {\n'
                    '  target_tests = [ "//src/foo:foo_tests" ]\n'
                    "}\n"
                    'group("tests") { deps = [ ":bazel_tests" ] }\n'
                ),
            },
            "[bazel_migration] //src/foo",
        )
        findings = parity.analyze_repository(self.root)
        self.assertEqual(len(findings), 1)
        self.assertIn("//src/foo:foo_test", findings[0].message)
        self.assertIn("host leg", findings[0].message)
        self.assertEqual(findings[0].properties, {})

    def test_11_class1_severed_parent_tests_group_edge_errors(self) -> None:
        self._commit(
            {
                "src/parent/BUILD.gn": (
                    'group("tests") {\n'
                    "  testonly = true\n"
                    '  deps = [ "child:tests" ]\n'
                    "}\n"
                ),
                "src/parent/child/test.py": "pass\n",
                "src/parent/child/BUILD.gn": (
                    'python_host_test("child_test") { main_source = "test.py" }\n'
                    'group("tests") { deps = [ ":child_test($host_toolchain)" ] }\n'
                ),
            },
            "Base",
        )
        self._commit(
            {
                "src/parent/BUILD.gn": (
                    'group("tests") {\n  testonly = true\n  deps = []\n}\n'
                ),
            },
            "[bazel_migration] //src/parent",
        )
        findings = parity.analyze_repository(self.root)
        self.assertEqual(len(findings), 1)
        self.assertIn("SEVERED :tests GROUP EDGE", findings[0].message)
        self.assertIn("child:tests", findings[0].message)

    def test_12_class2_gn2gone_sentinel_retirement_passes(self) -> None:
        self._commit(
            {
                "src/foo/src/lib.rs": "#[test]\nfn t() {}\n",
                "src/foo/BUILD.bazel": (
                    'rustc_library(name = "foo", srcs = ["src/lib.rs"])\n'
                    'host_rustc_test(name = "foo_test", crate = ":foo")\n'
                ),
                "src/foo/BUILD.gn": (
                    'bazel_test_suite("bazel_tests") {\n'
                    '  host_tests = [ "//src/foo:foo_test" ]\n'
                    "}\n"
                    'group("tests") {\n'
                    '  deps = [ ":bazel_tests", ":foo_test($host_toolchain)" ]\n'
                    "}\n"
                    "## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##\n"
                    'rustc_test("foo_test") { sources = [ "src/lib.rs" ] }\n'
                ),
            },
            "Dual-build state",
        )
        self._commit(
            {
                "src/foo/BUILD.gn": (
                    'bazel_test_suite("bazel_tests") {\n'
                    '  host_tests = [ "//src/foo:foo_test" ]\n'
                    "}\n"
                    'group("tests") {\n'
                    '  deps = [ ":bazel_tests" ]\n'
                    "}\n"
                ),
            },
            "[gn2gone] //src/foo",
        )
        self.assertEqual(parity.analyze_repository(self.root), [])

    def test_13_class3_renamed_tests_bijection_passes_and_collapse_fails(
        self,
    ) -> None:
        self._commit(
            {
                "src/foo/a_test.py": "pass\n",
                "src/foo/b_test.py": "pass\n",
                "src/foo/BUILD.gn": (
                    'python_host_test("old_a_test") { main_source = "a_test.py" }\n'
                    'python_host_test("old_b_test") { main_source = "b_test.py" }\n'
                    'group("tests") {\n'
                    '  deps = [ ":old_a_test($host_toolchain)", ":old_b_test($host_toolchain)" ]\n'
                    "}\n"
                ),
            },
            "Base",
        )
        # Valid 1:1 bijection passes!
        self._commit(
            {
                "src/foo/BUILD.bazel": (
                    'host_py_test(name = "new_a_test", srcs = ["a_test.py"])\n'
                    'host_py_test(name = "new_b_test", srcs = ["b_test.py"])\n'
                ),
                "src/foo/BUILD.gn": (
                    'bazel_test_suite("bazel_tests") {\n'
                    '  host_tests = [ "//src/foo:new_a_test", "//src/foo:new_b_test" ]\n'
                    "}\n"
                    'group("tests") { deps = [ ":bazel_tests" ] }\n'
                ),
            },
            (
                "[bazel_migration] //src/foo\n\n"
                "Renamed-Tests: //src/foo:old_a_test -> //src/foo:new_a_test\n"
                "Renamed-Tests: //src/foo:old_b_test -> //src/foo:new_b_test"
            ),
        )
        self.assertEqual(parity.analyze_repository(self.root), [])

        # Now test 2-to-1 collapse rejection!
        self._git("reset", "--hard", "HEAD^")
        self._commit(
            {
                "src/foo/BUILD.bazel": (
                    'host_py_test(name = "merged_test", srcs = ["a_test.py"])\n'
                ),
                "src/foo/BUILD.gn": (
                    'bazel_test_suite("bazel_tests") {\n'
                    '  host_tests = [ "//src/foo:merged_test" ]\n'
                    "}\n"
                    'group("tests") { deps = [ ":bazel_tests" ] }\n'
                ),
            },
            (
                "[bazel_migration] //src/foo\n\n"
                "Renamed-Tests: //src/foo:old_a_test -> //src/foo:merged_test\n"
                "Renamed-Tests: //src/foo:old_b_test -> //src/foo:merged_test"
            ),
        )
        findings = parity.analyze_repository(self.root)
        self.assertTrue(
            any(
                "NON-BIJECTIVE Renamed-Tests: COLLAPSE" in f.message
                for f in findings
            ),
            [f.message for f in findings],
        )

    def test_14_fm8_rust_test_crate_missing_crate_features_errors(self) -> None:
        self._commit({"src/foo/src/lib.rs": "pub fn f() {}\n"}, "Base")
        self._commit(
            {
                "src/foo/BUILD.bazel": (
                    "rustc_library(\n"
                    '    name = "foo",\n'
                    '    srcs = ["src/lib.rs"],\n'
                    '    crate_features = ["experimental", "serde"],\n'
                    ")\n"
                    "host_rustc_test(\n"
                    '    name = "foo_test",\n'
                    '    crate = ":foo",\n'
                    '    crate_features = ["serde"],\n'
                    ")\n"
                ),
                "src/foo/BUILD.gn": (
                    'bazel_test_suite("bazel_tests") {\n'
                    '  host_tests = [ "//src/foo:foo_test" ]\n'
                    "}\n"
                    'group("tests") { deps = [ ":bazel_tests" ] }\n'
                ),
            },
            "[bazel_migration] //src/foo",
        )
        findings = parity.analyze_repository(self.root)
        self.assertEqual(len(findings), 1)
        self.assertIn(
            "FM8 LOST #[cfg(feature)] UNIT TESTS", findings[0].message
        )
        self.assertIn("experimental", findings[0].message)
        self.assertEqual(findings[0].properties, {})

    def test_15_fm5_dual_build_bazel2gn_skip_unattested_vs_attested(
        self,
    ) -> None:
        self._commit({"src/foo/src/lib.rs": "pub fn f() {}\n"}, "Base")
        # 1. Missing comment + footer -> unwaivable error (no fail_if_missing_label)
        self._commit(
            {
                "src/foo/BUILD.bazel": (
                    'rustc_library(name = "foo", srcs = ["src/lib.rs"])\n'
                    "# @bazel2gn:skip\n"
                    'host_rustc_test(name = "foo_test", crate = ":foo")\n'
                ),
                "src/foo/BUILD.gn": (
                    'bazel_test_suite("bazel_tests") {\n'
                    '  host_tests = [ "//src/foo:foo_test" ]\n'
                    "}\n"
                    'group("tests") { deps = [ ":bazel_tests" ] }\n'
                    "## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##\n"
                    'rustc_library("foo") { sources = [ "src/lib.rs" ] }\n'
                ),
            },
            "[bazel_migration] //src/foo",
        )
        findings = parity.analyze_repository(self.root)
        self.assertEqual(len(findings), 1)
        self.assertIn(
            "UNATTESTED DUAL-BUILD TEST SKIP (FM5)", findings[0].message
        )
        self.assertEqual(findings[0].properties, {})

        # 2. Both reviewer comment AND Skipped-Dual-Tests + Bug -> requires
        # Test-Drop-Verified +1!
        self._git("reset", "--hard", "HEAD^")
        self._commit(
            {
                "src/foo/BUILD.bazel": (
                    'rustc_library(name = "foo", srcs = ["src/lib.rs"])\n'
                    "# Needs reviewer attention (bazel2gn skip): expensive 10min soak test\n"
                    "# @bazel2gn:skip\n"
                    'host_rustc_test(name = "foo_test", crate = ":foo")\n'
                ),
                "src/foo/BUILD.gn": (
                    'bazel_test_suite("bazel_tests") {\n'
                    '  host_tests = [ "//src/foo:foo_test" ]\n'
                    "}\n"
                    'group("tests") { deps = [ ":bazel_tests" ] }\n'
                    "## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##\n"
                    'rustc_library("foo") { sources = [ "src/lib.rs" ] }\n'
                ),
            },
            (
                "[bazel_migration] //src/foo\n\n"
                "Bug: 42099999\n"
                "Skipped-Dual-Tests: //src/foo:foo_test"
            ),
        )
        findings = parity.analyze_repository(self.root)
        self.assertEqual(len(findings), 1)
        self.assertEqual(findings[0].level, "error")
        self.assertEqual(
            findings[0].properties,
            {"fail_if_missing_label": "Test-Drop-Verified"},
        )
        self.assertIn(
            "DUAL-BUILD TEST SKIP REQUIRES Test-Drop-Verified +1",
            findings[0].message,
        )

    def test_16_fm5_bazel2gn_skip_with_handwritten_gn_copy_above_sentinel_passes(
        self,
    ) -> None:
        self._commit({"src/foo/src/lib.rs": "pub fn f() {}\n"}, "Base")
        self._commit(
            {
                "src/foo/BUILD.bazel": (
                    'rustc_library(name = "foo", srcs = ["src/lib.rs"])\n'
                    'rustc_test(name = "foo_test", crate = ":foo")\n'
                    "# @bazel2gn:skip\n"
                    "fx_unittest_package(\n"
                    '    name = "foo-tests",\n'
                    '    resources = [":res"],\n'
                    '    unit_tests = [":foo_test"],\n'
                    ")\n"
                ),
                "src/foo/BUILD.gn": (
                    'fuchsia_unittest_package("foo-tests") {\n'
                    '  package_name = "foo-tests-gn"\n'
                    '  deps = [ ":foo_test" ]\n'
                    "}\n"
                    'bazel_test_suite("bazel_tests") {\n'
                    '  target_tests = [ "//src/foo:foo-tests" ]\n'
                    "}\n"
                    'group("tests") {\n'
                    '  deps = [ ":bazel_tests", ":foo-tests" ]\n'
                    "}\n"
                    "## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##\n"
                    'rustc_library("foo") { sources = [ "src/lib.rs" ] }\n'
                    'rustc_test("foo_test") { sources = [ "src/lib.rs" ] }\n'
                ),
            },
            "[bazel_migration] //src/foo",
        )
        self.assertEqual(parity.analyze_repository(self.root), [])

    def test_17_class4_migration_unattested_test_deletion_screams_unwaivable(
        self,
    ) -> None:
        self._commit(
            {
                "src/foo/lib.cc": "int f() { return 0; }\n",
                "src/foo/dead_test.cc": "int main() { return 0; }\n",
                "src/foo/BUILD.gn": (
                    'source_set("foo") { sources = [ "lib.cc" ] }\n'
                    'test("dead_test") { sources = [ "dead_test.cc" ] }\n'
                    'group("tests") { deps = [ ":dead_test($host_toolchain)" ] }\n'
                ),
            },
            "Base",
        )
        self._commit(
            {
                "src/foo/dead_test.cc": None,
                "src/foo/BUILD.bazel": (
                    'cc_library(name = "foo", srcs = ["lib.cc"])\n'
                ),
                "src/foo/BUILD.gn": 'group("tests") { deps = [] }\n',
            },
            "[bazel_migration] //src/foo",
        )
        findings = parity.analyze_repository(self.root)
        self.assertEqual(len(findings), 1)
        self.assertEqual(findings[0].level, "error")
        self.assertEqual(findings[0].properties, {})
        self.assertIn(
            "UNATTESTED MIGRATION TEST DELETION (CLASS 4)", findings[0].message
        )
        self.assertIn("//src/foo:dead_test", findings[0].message)

    def test_18_class4_migration_attested_test_deletion_requires_label(
        self,
    ) -> None:
        self._commit(
            {
                "src/foo/lib.cc": "int f() { return 0; }\n",
                "src/foo/dead_test.cc": "int main() { return 0; }\n",
                "src/foo/BUILD.gn": (
                    'source_set("foo") { sources = [ "lib.cc" ] }\n'
                    'test("dead_test") { sources = [ "dead_test.cc" ] }\n'
                    'group("tests") { deps = [ ":dead_test($host_toolchain)" ] }\n'
                ),
            },
            "Base",
        )
        self._commit(
            {
                "src/foo/dead_test.cc": None,
                "src/foo/BUILD.bazel": (
                    'cc_library(name = "foo", srcs = ["lib.cc"])\n'
                ),
                "src/foo/BUILD.gn": 'group("tests") { deps = [] }\n',
            },
            (
                "[bazel_migration] //src/foo\n\n"
                "Bug: 42012345\n"
                "Deleted-Tests: //src/foo:dead_test"
            ),
        )
        findings = parity.analyze_repository(self.root)
        self.assertEqual(len(findings), 1)
        self.assertEqual(findings[0].level, "error")
        self.assertEqual(
            findings[0].properties,
            {"fail_if_missing_label": "Test-Drop-Verified"},
        )
        self.assertIn(
            "ATTESTED ESCAPE-HATCH TEST DELETION (REQUIRES Test-Drop-Verified +1)",
            findings[0].message,
        )

    def test_19_class4_planter_agent_forbidden_from_deleted_tests_waiver(
        self,
    ) -> None:
        self._commit(
            {
                "src/foo/dead_test.cc": "int main() { return 0; }\n",
                "src/foo/BUILD.gn": (
                    'test("dead_test") { sources = [ "dead_test.cc" ] }\n'
                    'group("tests") { deps = [ ":dead_test($host_toolchain)" ] }\n'
                ),
            },
            "Base",
        )
        self._commit(
            {
                "src/foo/dead_test.cc": None,
                "src/foo/BUILD.gn": 'group("tests") { deps = [] }\n',
            },
            (
                "[bazel_migration] //src/foo\n\n"
                "Bug: 42012345\n"
                "Deleted-Tests: //src/foo:dead_test\n"
                "TAG: planter"
            ),
        )
        findings = parity.analyze_repository(self.root)
        self.assertEqual(len(findings), 1)
        self.assertEqual(findings[0].level, "error")
        self.assertEqual(findings[0].properties, {})
        self.assertIn("TAG: planter", findings[0].message)

    def test_20_class4_deleted_tests_wildcard_rejected(self) -> None:
        self._commit(
            {
                "src/foo/dead_test.cc": "int main() { return 0; }\n",
                "src/foo/BUILD.gn": (
                    'test("dead_test") { sources = [ "dead_test.cc" ] }\n'
                ),
            },
            "Base",
        )
        self._commit(
            {
                "src/foo/dead_test.cc": None,
                "src/foo/BUILD.gn": "",
            },
            "[bazel_migration] //src/foo\n\nBug: 42012345\nDeleted-Tests: //src/foo:*",
        )
        findings = parity.analyze_repository(self.root)
        self.assertEqual(len(findings), 1)
        self.assertEqual(findings[0].properties, {})
        self.assertIn("wildcards", findings[0].message)

    def test_21_class4_non_migration_feature_cleanup_emits_warning(
        self,
    ) -> None:
        self._commit(
            {
                "src/feature/old.cc": "void old() {}\n",
                "src/feature/old_test.cc": "int main() {}\n",
                "src/feature/BUILD.gn": (
                    'source_set("old") { sources = [ "old.cc" ] }\n'
                    'test("old_test") { sources = [ "old_test.cc" ] }\n'
                ),
            },
            "Base",
        )
        self._commit(
            {
                "src/feature/old.cc": None,
                "src/feature/old_test.cc": None,
                "src/feature/BUILD.gn": "",
            },
            "[feature] Retire legacy old component",
        )
        findings = parity.analyze_repository(self.root)
        self.assertEqual(len(findings), 1)
        self.assertEqual(findings[0].level, "warning")
        self.assertEqual(findings[0].properties, {})
        self.assertIn("⚠️ [TEST DELETED", findings[0].message)
        self.assertIn("//src/feature:old_test", findings[0].message)

    def test_22_fm1_build_only_true_allowed_on_pure_library_migration(
        self,
    ) -> None:
        self._commit(
            {
                "src/lib/pure/lib.cc": "int x() { return 42; }\n",
                "src/lib/pure/BUILD.gn": (
                    'source_set("pure") {\n  sources = [ "lib.cc" ]\n}\n'
                ),
            },
            "Base",
        )
        self._commit(
            {
                "src/lib/pure/BUILD.bazel": (
                    'cc_library(name = "pure", srcs = ["lib.cc"])\n'
                ),
                "src/lib/pure/BUILD.gn": (
                    "## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##\n"
                    'source_set("pure") {\n  sources = [ "lib.cc" ]\n}\n'
                ),
            },
            "[bazel_migration] //src/lib/pure\n\nBuild-Only: true",
        )
        self.assertEqual(parity.analyze_repository(self.root), [])

    def test_23_fm1_build_only_true_rejected_when_tests_touched(self) -> None:
        self._commit(
            {
                "src/lib/foo/test.py": "pass\n",
                "src/lib/foo/BUILD.gn": (
                    'python_host_test("foo_test") { main_source = "test.py" }\n'
                    'group("tests") { deps = [ ":foo_test($host_toolchain)" ] }\n'
                ),
            },
            "Base",
        )
        self._commit(
            {
                "src/lib/foo/BUILD.bazel": (
                    'host_py_test(name = "foo_test", srcs = ["test.py"])\n'
                ),
                "src/lib/foo/BUILD.gn": (
                    'bazel_test_suite("bazel_tests") {\n'
                    '  host_tests = [ "//src/lib/foo:foo_test" ]\n'
                    "}\n"
                    'group("tests") { deps = [ ":bazel_tests" ] }\n'
                ),
            },
            "[bazel_migration] //src/lib/foo\n\nBuild-Only: true",
        )
        findings = parity.analyze_repository(self.root)
        self.assertEqual(len(findings), 1)
        self.assertEqual(findings[0].level, "error")
        self.assertEqual(findings[0].properties, {})
        self.assertIn(
            "ILLEGAL Build-Only: true ON TEST-TOUCHING CL (FM1)",
            findings[0].message,
        )

    def test_24_dirty_working_tree_and_cli_json_output(self) -> None:
        self._commit(
            {
                "src/dirty/foo_test.cc": "int main() { return 0; }\n",
                "src/dirty/BUILD.gn": (
                    'test("foo_test") { sources = [ "foo_test.cc" ] }\n'
                    'group("tests") { deps = [ ":foo_test($host_toolchain)" ] }\n'
                ),
            },
            "Base",
        )
        # Modify working tree WITHOUT committing (`base_ref` auto-detects `HEAD`).
        self._write_files(
            {
                "src/dirty/BUILD.gn": 'group("tests") { deps = [] }\n',
            }
        )
        out_json = self.root / "out.json"
        rc = parity.main(
            [
                "--root",
                str(self.root),
                "--output",
                str(out_json),
                "src/dirty/BUILD.gn",
            ]
        )
        self.assertEqual(rc, 1)
        payload = json.loads(out_json.read_text(encoding="utf-8"))
        self.assertEqual(len(payload), 1)
        self.assertIn("//src/dirty:foo_test", payload[0]["message"])

    def test_25_untouched_preexisting_host_suite_in_gn_edit_not_flagged(
        self,
    ) -> None:
        self._commit(
            {
                "scripts/foo/BUILD.bazel": (
                    'test_suite(name = "host_tests", tests = [])\n'
                ),
                "scripts/foo/BUILD.gn": (
                    'python_host_test("existing_test") { main_source = "a.py" }\n'
                    'bazel_test_suite("bazel_defined_tests") {\n'
                    '  host_tests = [ "//scripts/foo:host_tests" ]\n'
                    "}\n"
                    'group("tests") {\n'
                    '  deps = [ ":bazel_defined_tests", ":existing_test" ]\n'
                    "}\n"
                ),
            },
            "Base with pre-existing host_tests suite",
        )
        # Unrelated GN addition that touches neither BUILD.bazel nor bazel_test_suite.
        self._commit(
            {
                "scripts/foo/BUILD.gn": (
                    'python_host_test("existing_test") { main_source = "a.py" }\n'
                    'python_host_test("new_test") { main_source = "b.py" }\n'
                    'bazel_test_suite("bazel_defined_tests") {\n'
                    '  host_tests = [ "//scripts/foo:host_tests" ]\n'
                    "}\n"
                    'group("tests") {\n'
                    '  deps = [ ":bazel_defined_tests", ":existing_test", ":new_test" ]\n'
                    "}\n"
                ),
            },
            "Add new_test to scripts/foo/BUILD.gn",
        )
        findings = parity.analyze_repository(self.root)
        self.assertEqual(findings, [])

    def test_26_transitive_tests_no_e2e_and_preexisting_bazel2gn_skip(
        self,
    ) -> None:
        self._commit(
            {
                "sdk/BUILD.gn": (
                    'bazel_test_suite("bazel_tests") {\n'
                    '  host_tests = [ "//sdk:host_tests" ]\n'
                    "}\n"
                    'group("tests_no_e2e") {\n'
                    "  testonly = true\n"
                    '  deps = [ ":bazel_tests" ]\n'
                    "}\n"
                    'group("tests") {\n'
                    "  testonly = true\n"
                    '  deps = [ ":tests_no_e2e" ]\n'
                    "}\n"
                ),
                "sdk/BUILD.bazel": (
                    "test_suite(\n"
                    '    name = "host_tests",\n'
                    "    target_compatible_with = HOST_OS_CONSTRAINTS,\n"
                    '    tests = ["//sdk/sub:legacy_skipped_test", "//sdk/sub:existing_test"],\n'
                    ")\n"
                ),
                "sdk/sub/a_test.py": "pass\n",
                "sdk/sub/b_test.py": "pass\n",
                "sdk/sub/BUILD.bazel": (
                    "# @bazel2gn:skip\n"
                    'host_py_test(name = "legacy_skipped_test", srcs = ["a_test.py"])\n'
                    'host_py_test(name = "existing_test", srcs = ["b_test.py"])\n'
                ),
                "sdk/sub/BUILD.gn": (
                    "## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##\n"
                    'python_host_test("existing_test") { main_source = "b_test.py" }\n'
                ),
            },
            "Base with transitive tests_no_e2e and pre-existing @bazel2gn:skip",
        )
        self._commit(
            {
                "sdk/sub/c_test.py": "pass\n",
                "sdk/BUILD.bazel": (
                    "test_suite(\n"
                    '    name = "host_tests",\n'
                    "    target_compatible_with = HOST_OS_CONSTRAINTS,\n"
                    '    tests = ["//sdk/sub:legacy_skipped_test", "//sdk/sub:existing_test", "//sdk/sub:new_test"],\n'
                    ")\n"
                ),
                "sdk/sub/BUILD.bazel": (
                    "# @bazel2gn:skip\n"
                    'host_py_test(name = "legacy_skipped_test", srcs = ["a_test.py"])\n'
                    'host_py_test(name = "existing_test", srcs = ["b_test.py"])\n'
                    'host_py_test(name = "new_test", srcs = ["c_test.py"])\n'
                ),
                "sdk/sub/BUILD.gn": (
                    "## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##\n"
                    'python_host_test("existing_test") { main_source = "b_test.py" }\n'
                    'python_host_test("new_test") { main_source = "c_test.py" }\n'
                ),
            },
            "Wire //sdk/sub:new_test without touching legacy_skipped_test",
        )
        findings = parity.analyze_repository(self.root)
        self.assertEqual(findings, [])

    def test_27_hermetic_git_shim_and_bazel_fuchsia_test_package(self) -> None:
        with tempfile.TemporaryDirectory() as shim_tmp, tempfile.TemporaryDirectory() as repo_tmp:
            git_shim = _install_hermetic_git_shim(pathlib.Path(shim_tmp))
            repo = pathlib.Path(repo_tmp)
            for cmd in (
                [str(git_shim), "-C", str(repo), "init", "-q"],
                [
                    str(git_shim),
                    "-C",
                    str(repo),
                    "config",
                    "user.email",
                    "test@fuchsia.dev",
                ],
                [
                    str(git_shim),
                    "-C",
                    str(repo),
                    "config",
                    "user.name",
                    "Fuchsia Test",
                ],
            ):
                subprocess.run(cmd, check=True, capture_output=True, text=True)

            (repo / "src" / "drv").mkdir(parents=True)
            (repo / "src" / "drv" / "drv_test.cc").write_text(
                "int main() { return 0; }\n", encoding="utf-8"
            )
            (repo / "src" / "drv" / "BUILD.gn").write_text(
                'test("drv-test-bin") { sources = [ "drv_test.cc" ] }\n'
                'fuchsia_unittest_package("drv-test-pkg") {\n'
                '  deps = [ ":drv-test-bin" ]\n'
                "}\n"
                'group("tests") { testonly = true deps = [ ":drv-test-pkg" ] }\n',
                encoding="utf-8",
            )
            subprocess.run(
                [str(git_shim), "-C", str(repo), "add", "-A"],
                check=True,
                capture_output=True,
                text=True,
            )
            subprocess.run(
                [str(git_shim), "-C", str(repo), "commit", "-q", "-m", "Base"],
                check=True,
                capture_output=True,
                text=True,
            )

            # Migrate driver test package using OOT-style `fuchsia_cc_test` +
            # `fuchsia_unittest_package` bridged via `bazel_fuchsia_test_package`.
            (repo / "src" / "drv" / "BUILD.bazel").write_text(
                'fuchsia_cc_test(name = "drv_test_bin", srcs = ["drv_test.cc"])\n'
                "fuchsia_unittest_package(\n"
                '    name = "drv-test-pkg",\n'
                '    unit_tests = [":drv_test_bin"],\n'
                ")\n",
                encoding="utf-8",
            )
            (repo / "src" / "drv" / "BUILD.gn").write_text(
                'bazel_fuchsia_test_package("drv-test-pkg-bazel") {\n'
                '  test_package_name = "drv-test-pkg"\n'
                '  test_component_names = [ "drv_test_bin" ]\n'
                '  bazel_target = "//src/drv:drv-test-pkg"\n'
                "}\n"
                'group("tests") {\n'
                "  testonly = true\n"
                '  deps = [ ":drv-test-pkg-bazel" ]\n'
                "}\n",
                encoding="utf-8",
            )
            subprocess.run(
                [str(git_shim), "-C", str(repo), "add", "-A"],
                check=True,
                capture_output=True,
                text=True,
            )
            subprocess.run(
                [
                    str(git_shim),
                    "-C",
                    str(repo),
                    "commit",
                    "-q",
                    "-m",
                    "[bazel_migration] //src/drv",
                ],
                check=True,
                capture_output=True,
                text=True,
            )
            findings = parity.analyze_repository(repo, git_bin=str(git_shim))
            self.assertEqual(findings, [])


if __name__ == "__main__":
    unittest.main()
