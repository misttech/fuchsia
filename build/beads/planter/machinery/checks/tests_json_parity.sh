#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -uo pipefail

# Deterministic check that the change keeps every test of <build dir>/tests.json.
# cq_reachability, migration_sanity and build_verification's bazel_tests_exported
# step only check that the tests the change still defines are wired up; a test
# the migration loses entirely (a GN test package dropped without a Bazel
# replacement, a `group("tests")` entry removed, a Bazel `test_suite` that no
# longer lists a test) leaves nothing for them to check. This check compares the
# tests in <build dir>/tests.json at $PLANTER_CHANGE_BASE with the tests after
# the change.
#
# Identity: a device test (os "fuchsia") is kept when tests.json after the
# change has a test with the same package URL. A host test is kept when it has a
# test for the same os and cpu with the same name or the same label/source_label
# (ignoring the toolchain and Bazel's `@@` prefix): a GN host test
# `host_x64/foo_test` (`//pkg:foo_test`) moved to an exported Bazel host test
# keeps its label but not its name. Device tests don't match by label because
# all test components of a GN `fuchsia_test_package` share the package's label.
# Each test from before that is not kept is an ERROR (test_dropped_from_tests_json).
#
# Test roots: tests.json only lists tests reachable from the configured roots
# (product, `host_labels`, `developer_test_labels`), and planter slots usually
# don't include the migrated packages. For both generations the check appends
# the `//<dir>:tests` group of each changed package (and target directory) that
# defines one at that revision to args.gn: to `host_labels` when the BUILD.gn
# asserts `is_host`, otherwise to `developer_test_labels` (like
# `fx set --with-test`).
#
# Generations: tests.json is written by `fx gen` (regenerator.py, which also
# queries Bazel for the exported Bazel tests), so the baseline is a regeneration
# of the tree at $PLANTER_CHANGE_BASE:
#   1. Every file the change touched since $PLANTER_CHANGE_BASE (tracked files,
#      plus untracked build files and untracked files in the target directories)
#      and args.gn are copied to <git dir>/planter-tests-json-parity/backup, and
#      a restore journal records what was backed up and what the check writes.
#   2. Those files get their $PLANTER_CHANGE_BASE content (files the change added
#      are deleted), args.gn gets the base test roots, and `fx gen` writes the
#      baseline. HEAD and the index are never touched. Everything is restored,
#      keeping the original mtimes.
#   3. args.gn gets the test roots after the change and `fx gen` writes the tests
#      to compare. args.gn is restored.
#   4. A last `fx gen` makes the build dir match the original args.gn again.
# The baseline (or the failure of its `fx gen`) is cached per change base, build
# dir, base test roots and jiri update, so later rounds only do steps 3-4.
# Restoring never overwrites a file whose content is neither the backed-up nor
# the written content (someone edited it in between): such files are reported
# as an ERROR and kept in the backup. A run killed in the middle leaves the
# journal behind and the next run restores from it first. Files under
# $PLANTER_PROJECT_DIR (the machinery) are never touched.
#
# Like build_verification, this check runs once (for the first target directory)
# and only looks at the workdir's git repository.
#
# Environment:
#   PLANTER_SKIP_BUILD=1           skip the check (reports an INFO finding).
#   PLANTER_BUILD_DRY_RUN=1        run nothing; report the plan as an INFO finding.
#   PLANTER_GEN_TIMEOUT=<secs>     per-`fx gen` timeout (default 1800).

WORKDIR="${PLANTER_WORKDIR:-.}"
TARGET_DIR="${PLANTER_TARGET_DIR:-}"
TARGET_DIRS="${PLANTER_TARGET_DIRS:-$TARGET_DIR}"

first_dir="${TARGET_DIRS%% *}"
if [[ -n "$TARGET_DIR" && -n "$first_dir" && "$TARGET_DIR" != "$first_dir" ]]; then
  echo "[]"
  exit 0
fi

# exec, so that whoever stops this check signals the process that edits the
# working tree, and its `finally` restores it.
exec python3 - "$WORKDIR" "$TARGET_DIRS" <<'PYEOF'
import hashlib
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile

workdir = os.path.abspath(sys.argv[1])
target_dirs = [d.strip().strip("/") for d in re.split(r"[\s,]+", sys.argv[2]) if d.strip().strip("/")]
change_base = os.environ.get("PLANTER_CHANGE_BASE", "").strip() or "HEAD"
timeout = int(os.environ.get("PLANTER_GEN_TIMEOUT", "1800"))
dry_run = os.environ.get("PLANTER_BUILD_DRY_RUN", "").strip() not in ("", "0")
skip_build = os.environ.get("PLANTER_SKIP_BUILD", "").strip() not in ("", "0")
project_dir = os.path.realpath(os.environ.get("PLANTER_PROJECT_DIR", "") or os.devnull)

SOURCE = "tests_json_parity"
# Bump when the cached baseline format or the test identity changes.
BASELINE_VERSION = "2"
# A change touching more files than this is not a migration on top of
# $PLANTER_CHANGE_BASE (e.g. a stale base); reverting it would be slow and risky.
MAX_REVERTED_PATHS = 2000
MAX_DROPPED_FINDINGS = 25
# Untracked files outside the target directories only matter to `fx gen` if
# they are build files.
BUILD_FILE = re.compile(r"(^|/)(BUILD|BUILD\.gn|BUILD\.bazel|MODULE\.bazel|[^/]+\.gni|[^/]+\.bzl)$")
ROOTS_MARKER = "# Added by planter's tests_json_parity check; it restores this file."

findings = []


def add(category, severity, message, remediation=None, file=None):
    f = {"source": SOURCE, "category": category, "severity": severity, "message": message}
    if remediation:
        f["remediation"] = remediation
    if file:
        f["file"] = file
    findings.append(f)


def finish():
    print(json.dumps(findings, indent=2))
    sys.exit(0)


# Turning SIGTERM into SystemExit runs the `finally` that restores the tree.
for sig in (signal.SIGTERM, signal.SIGHUP):
    signal.signal(sig, lambda signum, frame: sys.exit(128 + signum))


def git(args, text=True):
    return subprocess.check_output(["git", "-C", workdir] + args, stderr=subprocess.DEVNULL, text=text)


def read(rel):
    try:
        with open(os.path.join(workdir, rel), encoding="utf-8", errors="replace") as f:
            return f.read()
    except OSError:
        return None


def base_text(rel):
    try:
        return git(["show", f"{change_base}:{rel}"])
    except (OSError, subprocess.CalledProcessError):
        return None


def sha(data):
    return hashlib.sha256(data).hexdigest()


def file_digest(path):
    try:
        with open(path, "rb") as f:
            return sha(f.read())
    except OSError:
        return ""


def state(path):
    """What is at `path`, for telling the backed-up, written and edited contents apart."""
    if os.path.islink(path):
        return "link:" + os.readlink(path)
    if os.path.isfile(path):
        return "file:" + file_digest(path)
    if os.path.isdir(path):
        return "dir"
    return "absent"


try:
    git_dir = git(["rev-parse", "--absolute-git-dir"]).strip()
except (OSError, subprocess.CalledProcessError):
    add("tests_json_parity_skipped", "INFO", f"{workdir} is not a git checkout; tests.json parity was not checked.")
    finish()

state_dir = os.path.join(git_dir, "planter-tests-json-parity")
journal_path = os.path.join(state_dir, "restore.json")
backup_dir = os.path.join(state_dir, "backup")


def remove_file(path):
    # Directories are never removed: path conflicts are rejected before anything is written.
    if os.path.islink(path) or os.path.isfile(path):
        os.unlink(path)


def prune_empty_dirs(rel):
    """Removes directories left empty by deleting `rel`, up to the workdir."""
    d = os.path.dirname(os.path.join(workdir, rel))
    while d.startswith(workdir + os.sep):
        try:
            os.rmdir(d)
        except OSError:
            return
        d = os.path.dirname(d)


class RestoreConflict(Exception):
    pass


def write_journal(entries):
    fd, tmp = tempfile.mkstemp(dir=state_dir, prefix=".restore.")
    with os.fdopen(fd, "w") as f:
        json.dump({"paths": entries}, f)
    os.replace(tmp, journal_path)


def back_up(entries):
    """Backs up the files of `entries` and writes the journal; call before changing any of them."""
    os.makedirs(state_dir, exist_ok=True)
    shutil.rmtree(backup_dir, ignore_errors=True)
    for e in entries:
        src = os.path.join(workdir, e["path"])
        e["backup_state"] = state(src)
        if e["backup_state"] != "absent":
            dst = os.path.join(backup_dir, e["path"])
            os.makedirs(os.path.dirname(dst), exist_ok=True)
            shutil.copy2(src, dst, follow_symlinks=False)
    write_journal(entries)


def restore_from_journal():
    """Puts the backed-up files back. Raises RestoreConflict for files edited since they were written."""
    with open(journal_path) as f:
        entries = json.load(f)["paths"]
    conflicts = []
    for e in entries:
        rel = e["path"]
        dst = os.path.join(workdir, rel)
        current = state(dst)
        if current == e["backup_state"]:
            continue
        if current not in e["written_states"]:
            conflicts.append(e)
            continue
        remove_file(dst)
        if e["backup_state"] == "absent":
            prune_empty_dirs(rel)
        else:
            os.makedirs(os.path.dirname(dst), exist_ok=True)
            shutil.copy2(os.path.join(backup_dir, rel), dst, follow_symlinks=False)
    if conflicts:
        write_journal(conflicts)
        raise RestoreConflict(", ".join(e["path"] for e in conflicts))
    os.unlink(journal_path)
    shutil.rmtree(backup_dir, ignore_errors=True)


def report_restore_failure(error):
    add(
        "tests_json_parity_restore_failed",
        "ERROR",
        "tests_json_parity temporarily wrote the $PLANTER_CHANGE_BASE version of changed files (and "
        f"test roots into args.gn) and could not restore all of them: {error}. The originals are in "
        f"{backup_dir}, listed in {journal_path}. A file whose content changed after the check wrote "
        "it is never overwritten.",
        remediation=f"Compare each file listed in {journal_path} with its copy in {backup_dir}, keep the "
        "right content, then delete both and run the checks again.",
    )


if os.path.exists(journal_path):
    try:
        restore_from_journal()
        add("tests_json_parity_restored", "INFO", "Restored the files of an interrupted tests_json_parity run.")
    except (RestoreConflict, OSError, ValueError, KeyError) as e:
        report_restore_failure(e)
        finish()

if skip_build:
    add("tests_json_parity_skipped", "INFO", "PLANTER_SKIP_BUILD is set; tests.json parity was not checked.")
    finish()

fx = next(
    (os.path.join(workdir, rel) for rel in (".jiri_root/bin/fx", "scripts/fx") if os.path.isfile(os.path.join(workdir, rel))),
    None,
)
build_dir = (read(".fx-build-dir") or "").strip()
args_gn_rel = os.path.relpath(os.path.join(workdir, build_dir, "args.gn"), workdir) if build_dir else ""
if not fx or not build_dir or args_gn_rel.startswith("..") or read(args_gn_rel) is None:
    add("tests_json_parity_skipped", "INFO", "No fx or no configured build dir (`fx set`) in the workdir; tests.json parity was not checked.")
    finish()
args_gn = os.path.join(workdir, args_gn_rel)
tests_json = os.path.join(os.path.dirname(args_gn), "tests.json")


def in_project_dir(rel):
    real = os.path.realpath(os.path.join(workdir, rel))
    return real == project_dir or real.startswith(project_dir + os.sep)


def in_target_dir(rel):
    return any(rel.startswith(d + "/") for d in target_dirs)


def base_entries(paths):
    """Maps each path that exists at $PLANTER_CHANGE_BASE to (mode, blob sha)."""
    entries = {}
    for i in range(0, len(paths), 500):
        out = git(["ls-tree", "-z", change_base, "--"] + paths[i : i + 500])
        for rec in out.split("\0"):
            if rec:
                meta, rel = rec.split("\t", 1)
                mode, _, blob = meta.split()
                entries[rel] = (mode, blob)
    return entries


try:
    base_commit = git(["rev-parse", "--verify", change_base + "^{commit}"]).strip()
    tracked = git(["diff", "--name-only", "-z", "--no-renames", change_base, "--"]).split("\0")
    untracked = git(["ls-files", "--others", "--exclude-standard", "-z"]).split("\0")
    paths = sorted(
        {p for p in tracked if p} | {p for p in untracked if p and (BUILD_FILE.search(p) or in_target_dir(p))}
    )
    paths = [p for p in paths if not in_project_dir(p)]
    base = base_entries(paths)
except (OSError, subprocess.CalledProcessError, ValueError) as e:
    add("tests_json_parity_skipped", "INFO", f"Cannot list the change since {change_base} ({e}); tests.json parity was not checked.")
    finish()

# Submodules and nested repositories are not files of this repository.
paths = [p for p in paths if base.get(p, ("",))[0] != "160000" and not os.path.isdir(os.path.join(workdir, p, ".git"))]
if not paths:
    finish()


def test_roots(read_build_gn):
    """args.gn assignments adding the `:tests` groups of the changed packages, or ""."""
    dirs = set(target_dirs)
    for p in paths:
        if os.path.basename(p) in ("BUILD.gn", "BUILD.bazel") and os.path.dirname(p):
            dirs.add(os.path.dirname(p))
    original = read(args_gn_rel)
    host, device = [], []
    for d in sorted(dirs):
        text = read_build_gn(f"{d}/BUILD.gn")
        lbl = f"//{d}:tests"
        if not text or not re.search(r'^\s*group\("tests"\)', text, re.M) or f'"{lbl}"' in original:
            continue
        (host if re.search(r"^assert\(\s*is_host\b", text, re.M) else device).append(lbl)
    lines = []
    for var, labels in (("host_labels", host), ("developer_test_labels", device)):
        if labels:
            op = "+=" if re.search(rf"^\s*{var}\s*\+?=", original, re.M) else "="
            lines.append(f"{var} {op} {json.dumps(labels)}")
    return "\n".join([f"\n{ROOTS_MARKER}"] + lines) + "\n" if lines else ""


def path_conflict(rel):
    """True when reverting `rel` would have to replace a directory or write through a symlink."""
    full = os.path.join(workdir, rel)
    if os.path.isdir(full) and not os.path.islink(full):
        return True
    d = os.path.dirname(rel)
    while d:
        a = os.path.join(workdir, d)
        if os.path.islink(a) or (os.path.lexists(a) and not os.path.isdir(a)):
            return True
        d = os.path.dirname(d)
    return False


def run_gen():
    """Runs `fx gen`; returns None on success or the tail of its output."""
    try:
        proc = subprocess.run(
            [fx, "gen"], cwd=workdir, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT, text=True, timeout=timeout,
        )
    except subprocess.TimeoutExpired:
        return f"timed out after {timeout}s"
    if proc.returncode == 0:
        return None
    return f"exit {proc.returncode}:\n" + "\n".join(proc.stdout.splitlines()[-40:])


def label(value):
    """`@@//a:b(//toolchain)` -> `//a:b`."""
    return re.sub(r"^@@?(?=//)", "", (value or "").strip()).split("(", 1)[0]


def load_tests():
    with open(tests_json) as f:
        entries = json.load(f)
    tests = {}
    for e in entries:
        t = e.get("test") or {}
        rec = {
            "name": t.get("name", ""),
            "label": label(t.get("label")),
            "source_label": label(t.get("source_label")) or label(t.get("label")),
            "os": t.get("os", ""),
            "cpu": t.get("cpu", ""),
            "bazel": (t.get("label") or "").startswith("@"),
        }
        tests[(rec["os"], rec["cpu"], rec["name"])] = rec
    return sorted(tests.values(), key=lambda r: (r["os"], r["cpu"], r["name"]))


original_args = read(args_gn_rel)
args_base = original_args + test_roots(base_text)
args_after = original_args + test_roots(read)

key_material = json.dumps([
    BASELINE_VERSION,
    base_commit,
    os.path.realpath(args_gn),
    sha(args_base.encode()),
    file_digest(os.path.join(workdir, ".jiri_root", "update_history", "latest")),
])
baseline_path = os.path.join(state_dir, "baseline-" + sha(key_material.encode())[:24] + ".json")
baseline = None
try:
    with open(baseline_path) as f:
        baseline = json.load(f)
except (OSError, ValueError):
    pass

if dry_run:
    plan = []
    if baseline is None:
        plan.append(f"write the {change_base} version of {len(paths)} changed files and run `fx gen` for the baseline")
    plan.append("run `fx gen` with the test roots of the change")
    add("build_dry_run", "INFO", "PLANTER_BUILD_DRY_RUN: tests_json_parity would " + ", then ".join(plan) + ".")
    finish()

if baseline is not None and baseline.get("error"):
    add(
        "tests_json_parity_skipped",
        "INFO",
        f"`fx gen` at {change_base} failed in an earlier run ({baseline['error']}); tests.json parity was not checked.",
    )
    finish()


def write_bytes(rel, content, mode="100644"):
    dst = os.path.join(workdir, rel)
    remove_file(dst)
    os.makedirs(os.path.dirname(dst), exist_ok=True)
    if mode == "120000":
        os.symlink(content.decode(), dst)
    else:
        with open(dst, "wb") as f:
            f.write(content)
        os.chmod(dst, 0o755 if mode == "100755" else 0o644)


def save_baseline(data):
    fd, tmp = tempfile.mkstemp(dir=state_dir, prefix=".baseline.")
    with os.fdopen(fd, "w") as f:
        json.dump(data, f)
    os.replace(tmp, baseline_path)
    # Each change base and build config adds a baseline; keep the newest few.
    old = sorted(
        (p for p in os.listdir(state_dir) if p.startswith("baseline-")),
        key=lambda p: os.path.getmtime(os.path.join(state_dir, p)),
        reverse=True,
    )
    for p in old[10:]:
        os.unlink(os.path.join(state_dir, p))


def with_written_files(writes, action):
    """Writes `writes` ({path: (content bytes or None to delete, mode)}), runs `action`, restores the files."""
    entries = []
    for rel, (content, mode) in writes.items():
        if content is None:
            written = "absent"
        elif mode == "120000":
            written = "link:" + content.decode()
        else:
            written = "file:" + sha(content)
        entries.append({"path": rel, "written_states": [written]})
    back_up(entries)
    try:
        for rel, (content, mode) in writes.items():
            if content is None:
                remove_file(os.path.join(workdir, rel))
                prune_empty_dirs(rel)
            else:
                write_bytes(rel, content, mode)
        return action()
    finally:
        try:
            restore_from_journal()
        except (RestoreConflict, OSError, ValueError, KeyError) as e:
            report_restore_failure(e)
            finish()


def gen_and_load():
    error = run_gen()
    if error is not None:
        return {"error": error}
    try:
        return {"tests": load_tests()}
    except (OSError, ValueError) as e:
        return {"error": f"cannot read <build dir>/tests.json: {e}"}


if baseline is None:
    if len(paths) > MAX_REVERTED_PATHS:
        add(
            "tests_json_parity_skipped",
            "INFO",
            f"The change touches {len(paths)} files since {change_base} (more than {MAX_REVERTED_PATHS}); "
            "tests.json parity was not checked.",
        )
        finish()
    conflicts = [p for p in paths if path_conflict(p)]
    if conflicts:
        add(
            "tests_json_parity_skipped",
            "INFO",
            "The change replaces files with directories or symlinks (or the reverse) at "
            + ", ".join(conflicts[:5])
            + "; tests.json parity was not checked.",
        )
        finish()
    try:
        writes = {}
        for rel in paths:
            if rel in base:
                mode, blob = base[rel]
                writes[rel] = (git(["cat-file", "blob", blob], text=False), mode)
            else:
                writes[rel] = (None, "")
    except (OSError, subprocess.CalledProcessError) as e:
        add("tests_json_parity_skipped", "INFO", f"Cannot read the {change_base} version of the change ({e}); tests.json parity was not checked.")
        finish()
    writes[args_gn_rel] = (args_base.encode(), "100644")
    baseline = with_written_files(writes, gen_and_load)
    save_baseline(baseline)

after = None
if baseline.get("error") is None:
    if args_after == original_args:
        after = gen_and_load()
    else:
        after = with_written_files({args_gn_rel: (args_after.encode(), "100644")}, gen_and_load)

# The restored files kept their mtimes, so ninja would not notice that the
# build dir was last generated for other files or args: regenerate it.
if baseline.get("error") is not None or args_after != original_args:
    error = run_gen()
    if error is not None:
        add(
            "build_failure_tests_json_parity_gen",
            "ERROR",
            f"`fx gen` after restoring the change and args.gn failed ({error}).",
            remediation="Fix the GN or Bazel error (build_verification usually reports it too), then run the checks again.",
        )
        finish()

if baseline.get("error") is not None:
    add("tests_json_parity_skipped", "INFO", f"`fx gen` at {change_base} failed ({baseline['error']}); tests.json parity was not checked.")
    finish()
if after.get("error") is not None:
    add(
        "tests_json_parity_skipped",
        "INFO",
        f"`fx gen` with the change's test roots failed ({after['error']}); tests.json parity was not checked.",
    )
    finish()

before, after = baseline["tests"], after["tests"]


def package_of(lbl):
    return lbl.split(":", 1)[0] if lbl.startswith("//") else ""


# Normalize host test stems within a package so a GN rustc_library/rustc_binary
# unit test (<crate>_lib_test / <bin>_bin_test, :lib_test / :bin_test) or
# rustc_library_with_features test matches its decomposed Bazel host test
# (<crate>_test, <pkg>_test, <base>-host-test) in the same package.
def host_pkg_stems(t):
    if t["os"] == "fuchsia":
        return set()
    pkg = package_of(t["source_label"] or t["label"])
    if not pkg:
        return set()
    raw_tokens = []
    name = t["name"]
    if name.startswith("//"):
        if ":" in name:
            raw_tokens.append(name.split(":", 1)[1])
    elif name:
        raw_tokens.append(os.path.basename(name))
    for l in (t["label"], t["source_label"]):
        if l and ":" in l:
            raw_tokens.append(l.split(":", 1)[1])
    stems = set()
    suffixes = (
        "_lib_test",
        "_bin_test",
        "_instrumented_test",
        "_host_test",
        "_unittest",
        "_unittests",
        "_test",
    )
    generic = {"lib", "bin", "test", "tests", "unit", "unittest", "unittests"}
    pkg_stem = os.path.basename(pkg).replace("-", "_")
    for tok in raw_tokens:
        norm = tok.replace("-", "_")
        stem = norm
        for sfx in suffixes:
            if norm.endswith(sfx) and len(norm) > len(sfx):
                stem = norm[: -len(sfx)]
                break
        if stem in generic:
            if pkg_stem:
                stems.add((t["os"], t["cpu"], pkg, pkg_stem))
        elif stem:
            stems.add((t["os"], t["cpu"], pkg, stem))
            if stem.endswith(("_lib", "_bin")) and len(stem) > 4:
                stems.add((t["os"], t["cpu"], pkg, stem[:-4]))
    return stems


def match_sets(tests):
    names = {(t["os"], t["cpu"], t["name"]) for t in tests}
    labels = {
        (t["os"], t["cpu"], l) for t in tests if t["os"] != "fuchsia" for l in (t["label"], t["source_label"]) if l
    }
    pkg_stems = {s for t in tests for s in host_pkg_stems(t)}
    return names, labels, pkg_stems


def kept(t, sets):
    names, labels, pkg_stems = sets
    if (t["os"], t["cpu"], t["name"]) in names:
        return True
    if t["os"] != "fuchsia" and any((t["os"], t["cpu"], l) in labels for l in (t["label"], t["source_label"]) if l):
        return True
    return bool(host_pkg_stems(t) & pkg_stems)


after_sets, before_sets = match_sets(after), match_sets(before)
dropped = [t for t in before if not kept(t, after_sets)]
added = [t for t in after if not kept(t, before_sets)]

REMEDIATION = (
    "Keep every test that ran before the migration. Export each Bazel device test (`fx_test`, "
    "`fx_unittest_package`) through `target_tests` and each Bazel host test through `host_tests` of a "
    "GN `bazel_test_suite` listed in the package's `group(\"tests\")`, keep GN test targets in "
    "`group(\"tests\")`, and keep the package and component names of device tests (they are the test "
    "names infra and test history use). If the test was renamed or removed on purpose, say why in "
    "`disputed_findings`."
)

for t in dropped[:MAX_DROPPED_FINDINGS]:
    pkg = package_of(t["source_label"] or t["label"])
    order = ("BUILD.bazel", "BUILD.gn") if t.get("bazel") else ("BUILD.gn", "BUILD.bazel")
    file = next((f"{pkg[2:]}/{b}" for b in order if pkg and os.path.isfile(os.path.join(workdir, pkg[2:], b))), None)
    msg = (
        f"Test `{t['name']}` ({t['os']}/{t['cpu']}, `{t['source_label'] or t['label']}`) is in "
        f"<build dir>/tests.json at {change_base} but not after the change, so neither `fx test` nor infra "
        "runs it anymore."
    )
    same_pkg = [a for a in added if a["os"] == t["os"] and pkg and package_of(a["source_label"] or a["label"]) == pkg]
    if same_pkg:
        msg += " Tests the change added in the same package: " + ", ".join(
            f"`{a['name']}`" for a in same_pkg[:5]
        ) + ("" if len(same_pkg) <= 5 else f" and {len(same_pkg) - 5} more") + "."
    add("test_dropped_from_tests_json", "ERROR", msg, remediation=REMEDIATION, file=file)

if len(dropped) > MAX_DROPPED_FINDINGS:
    rest = dropped[MAX_DROPPED_FINDINGS:]
    add(
        "test_dropped_from_tests_json",
        "ERROR",
        f"{len(rest)} more tests from <build dir>/tests.json at {change_base} are missing after the change: "
        + ", ".join(f"`{t['name']}`" for t in rest[:50])
        + ("" if len(rest) <= 50 else ", ..."),
        remediation=REMEDIATION,
    )

if not dropped:
    add(
        "tests_json_parity",
        "INFO",
        f"All {len(before)} tests of <build dir>/tests.json at {change_base} (with the changed packages' "
        f":tests groups as roots) are still in it after the change ({len(added)} added).",
    )

finish()
PYEOF
