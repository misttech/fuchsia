#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -uo pipefail

# Deterministic check that the change preserves every test and test execution
# attribute of <build dir>/tests.json across GN-to-Bazel migrations.
#
# Key guarantees (implemented with verify_tests_json_parity.py):
# 1. Primary Product-Root Pass: Runs baseline and post-change `fx gen` against
#    the true CQ product roots (`//bundles/buildbot/core` and
#    `//bundles/buildbot/host`, preserving board/product from `args.gn`) WITHOUT
#    injecting synthetic per-directory `//<dir>:tests` roots, so severing an
#    ancestor `group("tests")` edge immediately drops the test from `tests.json`.
# 2. Off-CQ Leaf Discovery Pass: Any test target in a touched baseline
#    `BUILD.gn` absent from the primary product `tests.json` is traced upward
#    via `fx gn refs` (with static `BUILD.gn` group fallback) to its highest
#    enclosing `group(...)` root and checked in a secondary scoped `fx gen` pass,
#    preventing `0 == 0` blind spots on leaf packages without local `group("tests")`.
# 3. Dual-Build `-gn` vs. `gn2gone` State Machine: Distinguishes Case 1 full GN
#    test removal (`1 gn -> 1 bazel`), Case 2 dual-build (`1 gn -> 1 bazel +
#    1 gn_dual` unless attested via `Skipped-Dual-Tests:`), and Case 3 `gn2gone`
#    sentinel retirement (`1 bazel + 1 gn_dual -> 1 bazel` with environments
#    preserved).
# 4. Attribute Parity & Fail-Closed `fx gen`: Verifies `os`, `cpu`, normalized
#    `environments` (dimensions/tags), `log_settings.max_severity`, and
#    `build_only` (`false -> true` is an ERROR). Any `fx gen` failure reports
#    `tests_json_parity_fx_gen_failed` as ERROR (never skipped as INFO).
# 5. Bijective `Renamed-Tests: <old> -> <new>` and 1:1 same-package host wrapper
#    matching; remediation forbids waiving dropped tests via `disputed_findings`
#    or inventing `Deleted-Tests:`.
#
# Environment:
#   PLANTER_SKIP_BUILD=1           skip the check (reports an INFO finding).
#   PLANTER_BUILD_DRY_RUN=1        run nothing; report the plan as an INFO finding.
#   PLANTER_GEN_TIMEOUT=<secs>     per-`fx gen` timeout (default 1800).

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
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
exec python3 -B - "$WORKDIR" "$TARGET_DIRS" "$SCRIPT_DIR" <<'PYEOF'
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
target_dirs = [
    d.strip().strip("/")
    for d in re.split(r"[\s,]+", sys.argv[2])
    if d.strip().strip("/")
]
script_dir = os.path.abspath(sys.argv[3])
sys.path.insert(0, script_dir)

import verify_tests_json_parity as vtp  # noqa: E402

change_base = os.environ.get("PLANTER_CHANGE_BASE", "").strip() or "HEAD"
timeout = int(os.environ.get("PLANTER_GEN_TIMEOUT", "1800"))
dry_run = os.environ.get("PLANTER_BUILD_DRY_RUN", "").strip() not in ("", "0")
skip_build = os.environ.get("PLANTER_SKIP_BUILD", "").strip() not in ("", "0")
project_dir = os.path.realpath(
    os.environ.get("PLANTER_PROJECT_DIR", "") or os.devnull
)

SOURCE = vtp.SOURCE
# Bump when the cached baseline format or test normalization changes.
BASELINE_VERSION = "3"
# A change touching more files than this is not a migration on top of
# $PLANTER_CHANGE_BASE (e.g. a stale base); reverting it would be slow and risky.
MAX_REVERTED_PATHS = 2000
# Untracked files outside the target directories only matter to `fx gen` if
# they are build files.
BUILD_FILE = re.compile(
    r"(^|/)(BUILD|BUILD\.gn|BUILD\.bazel|MODULE\.bazel|[^/]+\.gni|[^/]+\.bzl)$"
)

FX_GEN_REMEDIATION = (
    "Fix the GN or Bazel generation failure so `fx gen` succeeds and "
    "`<build dir>/tests.json` can be verified (`tests_json_parity` fails "
    "closed on `fx gen` errors and cannot be waived via `disputed_findings`)."
)

findings = []


def add(category, severity, message, remediation=None, file=None):
    findings.append(
        vtp.make_finding(
            category=category,
            severity=severity,
            message=message,
            remediation=remediation,
            file=file,
        )
    )


def finish():
    print(json.dumps(findings, indent=2))
    sys.exit(0)


# Turning SIGTERM into SystemExit runs the `finally` that restores the tree.
for sig in (signal.SIGTERM, signal.SIGHUP):
    signal.signal(sig, lambda signum, frame: sys.exit(128 + signum))


def git(args, text=True):
    return subprocess.check_output(
        ["git", "-C", workdir] + args, stderr=subprocess.DEVNULL, text=text
    )


def read(rel):
    try:
        with open(
            os.path.join(workdir, rel), encoding="utf-8", errors="replace"
        ) as f:
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
    """What is at `path`, for telling backed-up, written and edited contents apart."""
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
    add(
        "tests_json_parity_skipped",
        "INFO",
        f"{workdir} is not a git checkout; tests.json parity was not checked.",
    )
    finish()

state_dir = os.path.join(git_dir, "planter-tests-json-parity")
journal_path = os.path.join(state_dir, "restore.json")
backup_dir = os.path.join(state_dir, "backup")


def remove_file(path):
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
    """Backs up the files of `entries` and writes the journal."""
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
    """Restores backed-up files; raises RestoreConflict if edited externally."""
    with open(journal_path, encoding="utf-8") as f:
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
            shutil.copy2(
                os.path.join(backup_dir, rel), dst, follow_symlinks=False
            )
    if conflicts:
        write_journal(conflicts)
        raise RestoreConflict(", ".join(e["path"] for e in conflicts))
    os.unlink(journal_path)
    shutil.rmtree(backup_dir, ignore_errors=True)


def report_restore_failure(error):
    add(
        "tests_json_parity_restore_failed",
        "ERROR",
        "tests_json_parity temporarily wrote the $PLANTER_CHANGE_BASE version "
        f"of changed files (and test roots into args.gn) and could not restore "
        f"all of them: {error}. The originals are in {backup_dir}, listed in "
        f"{journal_path}. A file whose content changed after the check wrote "
        "it is never overwritten.",
        remediation=(
            f"Compare each file listed in {journal_path} with its copy in "
            f"{backup_dir}, keep the right content, then delete both and run "
            "the checks again."
        ),
    )


if os.path.exists(journal_path):
    try:
        restore_from_journal()
        add(
            "tests_json_parity_restored",
            "INFO",
            "Restored the files of an interrupted tests_json_parity run.",
        )
    except (RestoreConflict, OSError, ValueError, KeyError) as e:
        report_restore_failure(e)
        finish()

if skip_build:
    add(
        "tests_json_parity_skipped",
        "INFO",
        "PLANTER_SKIP_BUILD is set; tests.json parity was not checked.",
    )
    finish()

fx = next(
    (
        os.path.join(workdir, rel)
        for rel in (".jiri_root/bin/fx", "scripts/fx")
        if os.path.isfile(os.path.join(workdir, rel))
    ),
    None,
)
build_dir = (read(".fx-build-dir") or "").strip()
args_gn_rel = (
    os.path.relpath(os.path.join(workdir, build_dir, "args.gn"), workdir)
    if build_dir
    else ""
)
if (
    not fx
    or not build_dir
    or args_gn_rel.startswith("..")
    or read(args_gn_rel) is None
):
    add(
        "tests_json_parity_skipped",
        "INFO",
        "No fx or no configured build dir (`fx set`) in the workdir; "
        "tests.json parity was not checked.",
    )
    finish()
args_gn = os.path.join(workdir, args_gn_rel)
tests_json = os.path.join(os.path.dirname(args_gn), "tests.json")


def in_project_dir(rel):
    real = os.path.realpath(os.path.join(workdir, rel))
    return real == project_dir or real.startswith(project_dir + os.sep)


def in_target_dir(rel):
    return any(rel.startswith(d + "/") for d in target_dirs)


def base_entries(paths_list):
    """Maps each path that exists at $PLANTER_CHANGE_BASE to (mode, blob sha)."""
    entries = {}
    for i in range(0, len(paths_list), 500):
        out = git(
            ["ls-tree", "-z", change_base, "--"] + paths_list[i : i + 500]
        )
        for rec in out.split("\0"):
            if rec:
                meta, rel = rec.split("\t", 1)
                mode, _, blob = meta.split()
                entries[rel] = (mode, blob)
    return entries


try:
    base_commit = git(
        ["rev-parse", "--verify", change_base + "^{commit}"]
    ).strip()
    tracked = git(
        ["diff", "--name-only", "-z", "--no-renames", change_base, "--"]
    ).split("\0")
    untracked = git(
        ["ls-files", "--others", "--exclude-standard", "-z"]
    ).split("\0")
    paths = sorted(
        {p for p in tracked if p}
        | {
            p
            for p in untracked
            if p and (BUILD_FILE.search(p) or in_target_dir(p))
        }
    )
    paths = [p for p in paths if not in_project_dir(p)]
    base = base_entries(paths)
except (OSError, subprocess.CalledProcessError, ValueError) as e:
    add(
        "tests_json_parity_fx_gen_failed",
        "ERROR",
        f"Cannot list the change since {change_base} ({e}); tests.json parity "
        "could not be verified.",
        remediation=FX_GEN_REMEDIATION,
    )
    finish()

# Submodules and nested repositories are not files of this repository.
paths = [
    p
    for p in paths
    if base.get(p, ("",))[0] != "160000"
    and not os.path.isdir(os.path.join(workdir, p, ".git"))
]
if not paths:
    finish()

touched_dirs = set(target_dirs)
for p in paths:
    if os.path.basename(p) in ("BUILD.gn", "BUILD.bazel") and os.path.dirname(p):
        touched_dirs.add(os.path.dirname(p))

base_gn_by_dir = {d: base_text(f"{d}/BUILD.gn") for d in sorted(touched_dirs)}
base_gn_targets = []
for d, gn_txt in sorted(base_gn_by_dir.items()):
    base_gn_targets.extend(vtp.extract_gn_test_targets(gn_txt, d))


def path_conflict(rel):
    """True when reverting `rel` would replace a directory or traverse a symlink."""
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
            [fx, "gen"],
            cwd=workdir,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            timeout=timeout,
        )
    except subprocess.TimeoutExpired:
        return f"timed out after {timeout}s"
    if proc.returncode == 0:
        return None
    return f"exit {proc.returncode}:\n" + "\n".join(
        proc.stdout.splitlines()[-40:]
    )


def run_gn_refs(lbl):
    """Runs `fx gn refs <build_dir> <lbl>` to find enclosing GN groups."""
    try:
        proc = subprocess.run(
            [fx, "gn", "refs", os.path.join(workdir, build_dir), lbl],
            cwd=workdir,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            timeout=60,
        )
    except (OSError, subprocess.TimeoutExpired):
        return []
    if proc.returncode != 0:
        return []
    return [
        line.strip()
        for line in proc.stdout.splitlines()
        if line.strip().startswith("//")
    ]


def merge_tests(primary_list, secondary_list):
    """Merges normalized test records from primary product and scoped passes."""
    merged = {}
    for rec in list(primary_list) + list(secondary_list):
        key = (rec["os"], rec["cpu"], rec["name"], rec["variant"])
        merged[key] = rec
    return sorted(
        merged.values(),
        key=lambda r: (r["os"], r["cpu"], r["canonical_id"], r["variant"]),
    )


original_args = read(args_gn_rel)
product_args = vtp.build_product_args_gn(original_args)

key_material = json.dumps(
    [
        BASELINE_VERSION,
        base_commit,
        os.path.realpath(args_gn),
        sha(product_args.encode()),
        sha(json.dumps(sorted(touched_dirs)).encode()),
        file_digest(
            os.path.join(workdir, ".jiri_root", "update_history", "latest")
        ),
    ]
)
baseline_path = os.path.join(
    state_dir, "baseline-" + sha(key_material.encode())[:24] + ".json"
)
baseline = None
try:
    with open(baseline_path, encoding="utf-8") as f:
        loaded = json.load(f)
        if isinstance(loaded, dict) and "tests" in loaded and not loaded.get("error"):
            baseline = loaded
except (OSError, ValueError):
    pass

if dry_run:
    plan = []
    if baseline is None:
        plan.append(
            f"write the {change_base} version of {len(paths)} changed files "
            "and run `fx gen` with CQ product roots (plus scoped developer "
            "group roots if any touched GN test targets are off-CQ) for the "
            "baseline"
        )
    plan.append("run `fx gen` against the post-change tree")
    add(
        "build_dry_run",
        "INFO",
        "PLANTER_BUILD_DRY_RUN: tests_json_parity would "
        + ", then ".join(plan)
        + ".",
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
    os.makedirs(state_dir, exist_ok=True)
    fd, tmp = tempfile.mkstemp(dir=state_dir, prefix=".baseline.")
    with os.fdopen(fd, "w", encoding="utf-8") as f:
        json.dump(data, f)
    os.replace(tmp, baseline_path)
    old = sorted(
        (p for p in os.listdir(state_dir) if p.startswith("baseline-")),
        key=lambda p: os.path.getmtime(os.path.join(state_dir, p)),
        reverse=True,
    )
    for p in old[10:]:
        try:
            os.unlink(os.path.join(state_dir, p))
        except OSError:
            pass


def with_written_files(writes, action):
    """Writes `writes`, runs `action(update_file_fn)`, and restores originals."""
    entries = []
    entry_by_rel = {}
    for rel, (content, mode) in writes.items():
        if content is None:
            written = "absent"
        elif mode == "120000":
            written = "link:" + content.decode()
        else:
            written = "file:" + sha(content)
        rec = {"path": rel, "written_states": [written]}
        entries.append(rec)
        entry_by_rel[rel] = rec
    back_up(entries)

    def rewrite_tracked_file(rel, new_bytes, mode="100644"):
        rec = entry_by_rel[rel]
        new_state = "file:" + sha(new_bytes)
        if new_state not in rec["written_states"]:
            rec["written_states"].append(new_state)
            write_journal(entries)
        write_bytes(rel, new_bytes, mode)

    try:
        for rel, (content, mode) in writes.items():
            if content is None:
                remove_file(os.path.join(workdir, rel))
                prune_empty_dirs(rel)
            else:
                write_bytes(rel, content, mode)
        return action(rewrite_tracked_file)
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
        return {"tests": vtp.load_tests_file(tests_json)}
    except (OSError, ValueError) as e:
        return {"error": f"cannot read <build dir>/tests.json: {e}"}


ran_revert_or_custom_args = False

if baseline is None:
    if len(paths) > MAX_REVERTED_PATHS:
        add(
            "tests_json_parity_fx_gen_failed",
            "ERROR",
            f"The change touches {len(paths)} files since {change_base} (more "
            f"than {MAX_REVERTED_PATHS}); rebase or set PLANTER_CHANGE_BASE "
            "accurately so tests.json parity can be verified.",
            remediation=FX_GEN_REMEDIATION,
        )
        finish()
    conflicts = [p for p in paths if path_conflict(p)]
    if conflicts:
        add(
            "tests_json_parity_fx_gen_failed",
            "ERROR",
            "The change replaces files with directories or symlinks (or the "
            "reverse) at "
            + ", ".join(conflicts[:5])
            + "; tests.json parity could not revert cleanly.",
            remediation=FX_GEN_REMEDIATION,
        )
        finish()
    try:
        writes = {}
        for rel in paths:
            if rel in base:
                mode, blob = base[rel]
                writes[rel] = (
                    git(["cat-file", "blob", blob], text=False),
                    mode,
                )
            else:
                writes[rel] = (None, "")
    except (OSError, subprocess.CalledProcessError) as e:
        add(
            "tests_json_parity_fx_gen_failed",
            "ERROR",
            f"Cannot read the {change_base} version of the change ({e}); "
            "tests.json parity could not be verified.",
            remediation=FX_GEN_REMEDIATION,
        )
        finish()
    writes[args_gn_rel] = (product_args.encode(), "100644")

    def _compute_baseline(rewrite_file):
        primary_res = gen_and_load()
        if primary_res.get("error") is not None:
            return primary_res
        merged_tests = primary_res["tests"]
        uncovered = vtp.find_uncovered_gn_test_targets(
            base_gn_targets, merged_tests
        )
        scoped_host_roots = []
        scoped_dev_roots = []
        if uncovered:
            scoped_host_roots, scoped_dev_roots = (
                vtp.resolve_highest_enclosing_groups(
                    uncovered,
                    run_gn_refs,
                    {d: t for d, t in base_gn_by_dir.items() if t},
                    read_gn_fn=lambda rel_dir: read(f"{rel_dir}/BUILD.gn"),
                )
            )
            scoped_args = vtp.build_scoped_args_gn(
                original_args, scoped_host_roots, scoped_dev_roots
            )
            if scoped_args not in (original_args, product_args):
                rewrite_file(args_gn_rel, scoped_args.encode(), "100644")
                scoped_res = gen_and_load()
                if scoped_res.get("error") is not None:
                    return {
                        "error": (
                            "scoped developer-root `fx gen` failed: "
                            + scoped_res["error"]
                        )
                    }
                merged_tests = merge_tests(merged_tests, scoped_res["tests"])
        return {
            "tests": merged_tests,
            "scoped_host_roots": scoped_host_roots,
            "scoped_dev_roots": scoped_dev_roots,
        }

    ran_revert_or_custom_args = True
    baseline = with_written_files(writes, _compute_baseline)
    if baseline.get("error") is None:
        save_baseline(baseline)

after = None
if baseline.get("error") is None:
    scoped_host = baseline.get("scoped_host_roots") or []
    scoped_dev = baseline.get("scoped_dev_roots") or []
    scoped_after_args = (
        vtp.build_scoped_args_gn(original_args, scoped_host, scoped_dev)
        if (scoped_host or scoped_dev)
        else original_args
    )
    need_scoped_after = scoped_after_args not in (original_args, product_args)

    if product_args == original_args and not need_scoped_after:
        after = gen_and_load()
    else:
        ran_revert_or_custom_args = True

        def _compute_after(rewrite_file):
            primary_after = gen_and_load()
            if primary_after.get("error") is not None:
                return primary_after
            merged_after = primary_after["tests"]
            if need_scoped_after:
                rewrite_file(args_gn_rel, scoped_after_args.encode(), "100644")
                scoped_res = gen_and_load()
                if scoped_res.get("error") is not None:
                    return {
                        "error": (
                            "scoped developer-root `fx gen` failed: "
                            + scoped_res["error"]
                        )
                    }
                merged_after = merge_tests(merged_after, scoped_res["tests"])
            return {"tests": merged_after}

        after = with_written_files(
            {args_gn_rel: (product_args.encode(), "100644")}, _compute_after
        )

# Regenerate with original `args.gn` whenever files or `args.gn` were swapped so
# Ninja sees the exact post-change build graph.
if ran_revert_or_custom_args:
    restore_gen_err = run_gen()
    if restore_gen_err is not None:
        add(
            "tests_json_parity_fx_gen_failed",
            "ERROR",
            f"`fx gen` after restoring the change and args.gn failed ({restore_gen_err}).",
            remediation=FX_GEN_REMEDIATION,
        )
        finish()

if baseline.get("error") is not None:
    add(
        "tests_json_parity_fx_gen_failed",
        "ERROR",
        f"`fx gen` at {change_base} failed ({baseline['error']}); tests.json "
        "parity failed closed.",
        remediation=FX_GEN_REMEDIATION,
    )
    finish()

if after is None or after.get("error") is not None:
    err_detail = after["error"] if after else "unknown error"
    add(
        "tests_json_parity_fx_gen_failed",
        "ERROR",
        f"`fx gen` after the change failed ({err_detail}); tests.json parity "
        "failed closed.",
        remediation=FX_GEN_REMEDIATION,
    )
    finish()

package_states = {}
for d in sorted(touched_dirs):
    package_states[f"//{d}"] = vtp.PackageState(
        pkg_dir=d,
        before_gn=base_gn_by_dir.get(d),
        after_gn=read(f"{d}/BUILD.gn"),
        before_bazel=base_text(f"{d}/BUILD.bazel"),
        after_bazel=read(f"{d}/BUILD.bazel"),
    )

commit_msg = ""
msg_file = os.environ.get("PLANTER_COMMIT_MSG_FILE", "").strip()
if msg_file and os.path.isfile(msg_file):
    try:
        with open(msg_file, encoding="utf-8", errors="replace") as fh:
            commit_msg = fh.read()
    except OSError:
        commit_msg = ""
else:
    try:
        head_commit = git(["rev-parse", "--verify", "HEAD^{commit}"]).strip()
        if head_commit != base_commit:
            commit_msg = git(["log", "-1", "--format=%B", "HEAD"])
    except (OSError, subprocess.CalledProcessError):
        commit_msg = ""

before_tests, after_tests = baseline["tests"], after["tests"]
parity_findings = vtp.verify_parity(
    before_tests=before_tests,
    after_tests=after_tests,
    package_states=package_states,
    commit_msg=commit_msg,
    change_base=change_base,
    workdir=workdir,
)
findings.extend(parity_findings)

if not any(f["severity"] == "ERROR" for f in parity_findings):
    add(
        "tests_json_parity",
        "INFO",
        f"All {len(before_tests)} baseline test entries of "
        f"<build dir>/tests.json at {change_base} satisfy migration parity "
        "after the change.",
    )

finish()
PYEOF
