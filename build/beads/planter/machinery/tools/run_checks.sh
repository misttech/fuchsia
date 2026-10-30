#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -uo pipefail

# Runs the deterministic checks of checks/manifest.json the way planter runs them
# after the coder reports: same scripts, arguments, environment, order, JSON
# parsing, severity normalization and de-duplication. ERROR and WARNING findings
# are blocking: planter sends the change back for another round if any check
# reports one.
#
# Usage: run_checks.sh [--skip-build] [--only NAME[,NAME...]] [--json] [--list] [DIR...]
#
#   --skip-build  Run the checks with PLANTER_SKIP_BUILD=1, so build_verification
#                 does not build (static checks only: seconds instead of minutes).
#   --only NAMES  Run only these checks (comma-separated, repeatable).
#   --json        Print all findings as one JSON array instead of a report.
#   --list        List the checks and exit.
#   DIR...        Target directories (default: $PLANTER_TARGET_DIRS).
#
# The checks run in $PLANTER_WORKDIR (default: the current directory) against the
# change since $PLANTER_CHANGE_BASE (default: HEAD). It never waits for a machinery
# evolution that planter is running alongside the coder: it runs the current checks
# and notes the evolution; planter re-runs the checks with the evolved machinery
# after the coder reports. Progress goes to stderr.
# Exit status: 0 if no finding blocks, 1 if some do, 2 on usage or manifest
# errors.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="${PLANTER_PROJECT_DIR:-$(dirname "$SCRIPT_DIR")}"

python3 - "$PROJECT_DIR" "$@" <<'PYEOF'
from concurrent.futures import ThreadPoolExecutor
import fcntl
import hashlib
import json
import os
import signal
import subprocess
import sys
import time

USAGE = """\
usage: run_checks.sh [--skip-build] [--only NAME[,NAME...]] [--json] [--list] [DIR...]

Runs the checks in checks/manifest.json the way planter runs them after the
coder reports. Exits 1 if any ERROR or WARNING finding would send the change
back, 0 otherwise. It does not wait for a machinery evolution in progress: planter
runs the checks again with the evolved machinery after you report.

  --skip-build  set PLANTER_SKIP_BUILD=1 so build_verification does not build
  --only NAMES  run only these checks (comma-separated, repeatable)
  --json        print all findings as one JSON array instead of a report
  --list        list the checks and exit
  DIR...        target directories (default: $PLANTER_TARGET_DIRS)"""

HEARTBEAT_SECS = 60

sys.stdout.reconfigure(errors="replace")
sys.stderr.reconfigure(errors="replace")


def log(msg):
    print(f"[run_checks] {msg}", file=sys.stderr, flush=True)


def die(msg):
    print(f"run_checks: {msg}", file=sys.stderr)
    sys.exit(2)


def usage_error(msg):
    print(f"run_checks: {msg}\n\n{USAGE}", file=sys.stderr)
    sys.exit(2)


project_dir = os.path.abspath(sys.argv[1])
skip_build = False
as_json = False
list_only = False
only = []
dir_args = []
args = sys.argv[2:]
i = 0
while i < len(args):
    a = args[i]
    if a in ("-h", "--help"):
        print(USAGE)
        sys.exit(0)
    elif a == "--skip-build":
        skip_build = True
    elif a == "--json":
        as_json = True
    elif a == "--list":
        list_only = True
    elif a == "--only" or a.startswith("--only="):
        if a == "--only":
            i += 1
            if i >= len(args):
                usage_error("--only needs a comma-separated list of check names")
            value = args[i]
        else:
            value = a[len("--only="):]
        names = [n.strip() for n in value.split(",") if n.strip()]
        if not names:
            usage_error("--only needs a comma-separated list of check names")
        only.extend(names)
    elif a == "--":
        dir_args.extend(args[i + 1:])
        break
    elif a.startswith("-"):
        usage_error(f"unknown option {a}")
    else:
        dir_args.append(a)
    i += 1

workdir = os.environ.get("PLANTER_WORKDIR", "") or os.getcwd()
raw_change_base = os.environ.get("PLANTER_CHANGE_BASE", "").strip() or "HEAD"


def resolve_effective_change_base(raw_base):
    """Advance change_base to merge-base(origin/main, HEAD) when HEAD was rebased onto a newer
    origin/main after PLANTER_CHANGE_BASE was pinned, so checks diff against the rebased base."""
    def _git_out(cmd_args):
        try:
            return subprocess.check_output(
                ["git", "-C", workdir] + cmd_args, stderr=subprocess.DEVNULL, text=True
            ).strip()
        except Exception:
            return ""

    def _is_anc(anc, desc):
        if not anc or not desc:
            return False
        return (
            subprocess.run(
                ["git", "-C", workdir, "merge-base", "--is-ancestor", anc, desc],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                check=False,
            ).returncode
            == 0
        )

    base = (raw_base or "").strip() or "HEAD"
    head_sha = _git_out(["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])
    base_sha = _git_out(["rev-parse", "--verify", "--quiet", f"{base}^{{commit}}"])
    origin_main = _git_out(["rev-parse", "--verify", "--quiet", "origin/main^{commit}"])
    merge_base = (
        _git_out(["merge-base", origin_main, head_sha]) if (origin_main and head_sha) else ""
    )
    if (
        base_sha
        and merge_base
        and base_sha != merge_base
        and _is_anc(base_sha, merge_base)
    ):
        return merge_base
    if head_sha and base_sha and not _is_anc(base_sha, head_sha):
        return merge_base or "HEAD~1"
    return base


change_base = resolve_effective_change_base(raw_change_base)


def normalize_dir(d):
    d = d.strip()
    if os.path.isabs(d):
        rel = os.path.relpath(d, workdir)
        if rel != ".." and not rel.startswith("../"):
            d = rel
    if d.startswith("//"):
        d = d[2:]
    while d.startswith("./"):
        d = d[2:]
    d = d.rstrip("/")
    return "" if d == "." else d


if dir_args:
    target_dirs = [d for d in (normalize_dir(a) for a in dir_args) if d]
else:
    target_dirs = os.environ.get("PLANTER_TARGET_DIRS", "").split()
    if not target_dirs and os.environ.get("PLANTER_TARGET_DIR", "").strip():
        target_dirs = [os.environ["PLANTER_TARGET_DIR"].strip()]


# Match a package path against the //dir or //.../<suffix> path in a commit subject.
def matches_subject_dir(pkg, sdir):
    if not pkg or not sdir:
        return False
    if sdir.startswith(".../"):
        suf = sdir[4:]
        return bool(suf) and (pkg == suf or pkg.endswith("/" + suf))
    return pkg == sdir


# Check whether a [bazel_migration] HEAD commit belongs to the current task's
# target_dirs rather than a stacked parent migration commit.
def is_task_migration_head(wdir, head_sha, parent_sha, subj, tdirs):
    if not head_sha or not parent_sha or not subj.startswith("[bazel_migration]"):
        return False
    env_dirs = os.environ.get("PLANTER_TARGET_DIRS", "").split()
    if os.environ.get("PLANTER_TARGET_DIR", "").strip():
        env_dirs.append(os.environ["PLANTER_TARGET_DIR"].strip())
    check_dirs = []
    for d in list(tdirs) + env_dirs:
        nd = normalize_dir(d).strip("/")
        if nd and nd not in check_dirs:
            check_dirs.append(nd)
            if nd.startswith("build/secondary/"):
                ov = nd[len("build/secondary/"):]
                if ov and ov not in check_dirs:
                    check_dirs.append(ov)
    if not check_dirs:
        return True
    head_files = [
        f.strip()
        for f in subprocess.check_output(
            ["git", "-C", wdir, "diff", "--name-only", parent_sha, head_sha],
            stderr=subprocess.DEVNULL, text=True,
        ).splitlines()
        if f.strip()
    ]
    head_pkgs = {
        os.path.dirname(f)
        for f in head_files
        if os.path.basename(f) in ("BUILD.bazel", "BUILD.gn", "BUILD")
    }
    head_bazel_pkgs = {
        os.path.dirname(f)
        for f in head_files
        if os.path.basename(f) in ("BUILD.bazel", "BUILD")
    }
    untracked_bazel_pkgs = {
        os.path.dirname(f.strip())
        for f in subprocess.check_output(
            ["git", "-C", wdir, "ls-files", "--others", "--exclude-standard", "--"]
            + check_dirs,
            stderr=subprocess.DEVNULL, text=True,
        ).splitlines()
        if os.path.basename(f.strip()) in ("BUILD.bazel", "BUILD")
    }
    if untracked_bazel_pkgs and not (head_bazel_pkgs & set(check_dirs)):
        return False
    sdir = (
        subj.split(" //", 1)[1].split(":", 1)[0].strip().rstrip(".").strip("/")
        if " //" in subj
        else ""
    )
    subj_matches = bool(sdir) and any(matches_subject_dir(td, sdir) for td in check_dirs)
    sdir_in_head = bool(sdir) and any(matches_subject_dir(hp, sdir) for hp in head_pkgs)
    if sdir_in_head and not subj_matches and not (head_bazel_pkgs & set(check_dirs)):
        return False
    if head_pkgs:
        return bool(head_pkgs & set(check_dirs)) or subj_matches
    return (
        any(f == td or f.startswith(td + "/") for td in check_dirs for f in head_files)
        or subj_matches
    )


# Resolve the effective change base when HEAD is already the migration commit or was rebased.
def resolve_effective_change_base(wdir, raw_base, tdirs):
    try:
        head = subprocess.check_output(
            ["git", "-C", wdir, "rev-parse", "--verify", "--quiet", "HEAD^{commit}"],
            stderr=subprocess.DEVNULL, text=True,
        ).strip()
        parent = subprocess.check_output(
            ["git", "-C", wdir, "rev-parse", "--verify", "--quiet", "HEAD~1^{commit}"],
            stderr=subprocess.DEVNULL, text=True,
        ).strip()
        if not head or not parent:
            return raw_base
        subj = subprocess.check_output(
            ["git", "-C", wdir, "log", "-1", "--format=%s", "HEAD"],
            stderr=subprocess.DEVNULL, text=True,
        ).strip()
        is_mig_head = is_task_migration_head(wdir, head, parent, subj, tdirs)
        base_sha = subprocess.check_output(
            ["git", "-C", wdir, "rev-parse", "--verify", "--quiet", f"{raw_base}^{{commit}}"],
            stderr=subprocess.DEVNULL, text=True,
        ).strip()
        if raw_base == "HEAD" or base_sha == head:
            return "HEAD~1" if is_mig_head else raw_base
        if base_sha and base_sha != parent and is_mig_head:
            is_anc_parent = (
                subprocess.run(
                    ["git", "-C", wdir, "merge-base", "--is-ancestor", base_sha, parent],
                    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                ).returncode
                == 0
            )
            is_anc_head = (
                subprocess.run(
                    ["git", "-C", wdir, "merge-base", "--is-ancestor", base_sha, head],
                    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                ).returncode
                == 0
            )
            if is_anc_parent or not is_anc_head:
                return "HEAD~1"
    except Exception:
        pass
    return raw_base


change_base = resolve_effective_change_base(workdir, change_base, target_dirs)


def fmt_secs(secs):
    if secs < 60:
        return f"{secs:.1f}s"
    return f"{int(secs // 60)}m{int(secs % 60):02d}s"


def on_sigterm(signum, frame):
    sys.exit(128 + signum)


signal.signal(signal.SIGTERM, on_sigterm)


# --- Machinery evolutions that planter runs alongside the coder ---


def machinery_lock_paths():
    # planter's lockMachinery lock: <lockDir>/planter-machinery-<sha256(abs dir)[:16]>.lock.
    # Check PLANTER_LOCK_DIR, $(dirname PLANTER_DB)/locks, ~/.planter/locks, TMPDIR, and /tmp.
    name = "planter-machinery-" + hashlib.sha256(project_dir.encode()).hexdigest()[:16] + ".lock"
    dirs = []
    if os.environ.get("PLANTER_LOCK_DIR"):
        dirs.append(os.environ["PLANTER_LOCK_DIR"])
    if os.environ.get("PLANTER_DB"):
        dirs.append(os.path.join(os.path.dirname(os.path.abspath(os.environ["PLANTER_DB"])), "locks"))
    dirs.extend([
        os.path.expanduser("~/.planter/locks"),
        os.environ.get("TMPDIR") or "/tmp",
        "/tmp",
    ])
    return list(dict.fromkeys(os.path.join(d, name) for d in dirs if d))


_held_machinery_locks = []


def release_machinery_locks():
    while _held_machinery_locks:
        fd = _held_machinery_locks.pop()
        try:
            os.close(fd)
        except OSError:
            pass


import atexit
atexit.register(release_machinery_locks)


def evolution_running():
    for path in machinery_lock_paths():
        try:
            fd = os.open(path, os.O_RDONLY)
        except OSError:
            continue  # No evolution has run since the lock file was cleaned up.
        try:
            fcntl.flock(fd, fcntl.LOCK_SH | fcntl.LOCK_NB)
            # Keep the shared lock open while reading manifest/scripts so mergeEvolution
            # cannot atomically overwrite check files mid-read.
            _held_machinery_locks.append(fd)
        except BlockingIOError:
            try:
                os.close(fd)
            except OSError:
                pass
            release_machinery_locks()
            return True
        except OSError:
            try:
                os.close(fd)
            except OSError:
                pass
    return False


# Never wait for an evolution: it can take many minutes, and planter itself applies it
# before running the checks after the coder reports.
evolution_in_progress = not list_only and evolution_running()


# --- checks/manifest.json, read the way planter reads it (Go encoding/json) ---


def reject_constant(name):
    raise ValueError(f"invalid JSON constant {name}")


def go_field(obj, name):
    """Returns obj's value for the struct field `name`, matching keys the way Go's
    encoding/json does (exact or case-insensitive, the last match wins, null is a no-op)."""
    value = None
    for k, v in obj.items():
        if (k == name or k.lower() == name) and v is not None:
            value = v
    return value


def machinery_root():
    # Mirrors project.Workspace.baseRoots: a physical machinery/ directory wins if only it
    # has prompts/coder.md.
    alt = os.path.join(project_dir, "machinery")
    if (
        os.path.isdir(alt)
        and not os.path.islink(alt)
        and os.path.exists(os.path.join(alt, "prompts", "coder.md"))
        and not os.path.exists(os.path.join(project_dir, "prompts", "coder.md"))
    ):
        return alt
    return project_dir


def load_specs(path):
    try:
        with open(path, encoding="utf-8") as f:
            text = f.read()
    except OSError:
        return None  # Planter runs no checks without a readable manifest.
    try:
        doc = json.loads(text, parse_constant=reject_constant)
    except ValueError as e:
        raise ValueError(f"invalid JSON: {e}")
    if doc is None:
        return []
    if not isinstance(doc, dict):
        raise ValueError("the manifest is not a JSON object")
    version = go_field(doc, "version")
    if version is not None and (isinstance(version, bool) or not isinstance(version, int)):
        raise ValueError('"version" is not an integer')
    checks = go_field(doc, "checks")
    if checks is None:
        return []
    if not isinstance(checks, list):
        raise ValueError('"checks" is not a list')
    specs = []
    for n, c in enumerate(checks):
        c = {} if c is None else c
        if not isinstance(c, dict):
            raise ValueError(f"checks[{n}] is not an object")
        spec = {"name": "", "description": "", "script_file": "", "args": [], "enabled": False}
        for field in ("name", "description", "script_file"):
            v = go_field(c, field)
            if v is not None:
                if not isinstance(v, str):
                    raise ValueError(f"checks[{n}].{field} is not a string")
                spec[field] = v
        v = go_field(c, "args")
        if v is not None:
            if not isinstance(v, list) or not all(a is None or isinstance(a, str) for a in v):
                raise ValueError(f"checks[{n}].args is not a list of strings")
            spec["args"] = ["" if a is None else a for a in v]
        v = go_field(c, "enabled")
        if v is not None:
            if not isinstance(v, bool):
                raise ValueError(f"checks[{n}].enabled is not a boolean")
            spec["enabled"] = v
        specs.append(spec)
    return specs


manifest_path = os.path.join(machinery_root(), "checks", "manifest.json")
try:
    specs = load_specs(manifest_path)
except ValueError as e:
    die(f"cannot parse {manifest_path} (planter would fail the same way): {e}")
if specs is None:
    log(f"no readable {manifest_path}; planter runs no checks")
    specs = []

if list_only:
    for s in specs:
        note = "" if s["enabled"] else " [disabled]"
        print(f"{s['name']}{note}: {s['description']}")
    sys.exit(0)

enabled = [s for s in specs if s["enabled"]]
disabled = [s for s in specs if not s["enabled"]]
if only:
    known = {s["name"] for s in specs} | {"review_diff"}
    unknown = [n for n in only if n not in known]
    if unknown:
        usage_error(f"unknown check(s): {', '.join(unknown)}; checks: {', '.join(s['name'] for s in specs)}, review_diff")
    wanted = set(only)
    enabled = [s for s in enabled if s["name"] in wanted]
    disabled = [s for s in disabled if s["name"] in wanted]


# --- Findings, parsed and normalized the way planter does it ---

FINDING_FIELDS = ("source", "category", "severity", "file", "line", "message", "remediation")
WARNING_SEVERITIES = {"WARNING", "WARN", "MINOR", "MEDIUM", "MODERATE"}
INFO_SEVERITIES = {"INFO", "INFORMATIONAL", "NOTE", "NIT", "NITPICK", "SUGGESTION", "OPTIONAL", "LOW", "FYI"}


def new_finding(**kw):
    f = {k: "" for k in FINDING_FIELDS}
    f["line"] = 0
    f.update(kw)
    return f


def normalize_severity(s):
    s = s.strip().upper()
    if s in WARNING_SEVERITIES:
        return "WARNING"
    if s in INFO_SEVERITIES:
        return "INFO"
    return "ERROR"  # ERROR, CRITICAL, HIGH, "", anything unknown.


def blocking(f):
    return normalize_severity(f["severity"]) in ("ERROR", "WARNING")


def decode_findings(value):
    """Decodes a JSON array like Go's json.Unmarshal into []substrate.Finding. Returns
    (findings, ok); like Go, values of the wrong type are skipped and make the decode fail."""
    ok = True
    out = []
    for el in value:
        f = new_finding()
        if isinstance(el, dict):
            for k, v in el.items():
                name = k if k in f else k.lower() if k.lower() in f else None
                if name is None or v is None:
                    continue
                if name == "line":
                    if isinstance(v, int) and not isinstance(v, bool) and -(1 << 63) <= v < (1 << 63):
                        f["line"] = v
                    else:
                        ok = False
                elif isinstance(v, str):
                    f[name] = v
                else:
                    ok = False
        elif el is not None:
            ok = False
        out.append(f)
    return out, ok


def parse_findings(raw):
    """Mirrors planter's extractJSONArray: the whole output if it is a findings array (or
    null), else the array ending last (and starting first) in the output that decodes as
    findings. Returns (findings, parsed_ok); like planter, a failed parse can still leave the
    partially decoded findings of the last candidate it tried."""
    if not raw:
        return [], False
    partial = []
    try:
        whole = json.loads(raw, parse_constant=reject_constant)
    except ValueError:
        whole = ValueError
    if whole is None:
        return [], True
    if isinstance(whole, list):
        findings, ok = decode_findings(whole)
        if ok:
            return findings, True
        partial = findings
    decoder = json.JSONDecoder(parse_constant=reject_constant)
    candidates = []
    start = raw.find("[")
    while start != -1:
        try:
            value, end = decoder.raw_decode(raw, start)
            candidates.append((end, start, value))
        except ValueError:
            pass
        start = raw.find("[", start + 1)
    # Planter tries the candidates by descending end, then ascending start.
    candidates.sort(key=lambda c: (-c[0], c[1]))
    for _, _, value in candidates:
        findings, ok = decode_findings(value)
        if ok:
            return findings, True
        partial = findings
    return partial, False


# --- Running the checks ---

SIGNAL_NAMES = {
    signal.SIGHUP: "hangup",
    signal.SIGINT: "interrupt",
    signal.SIGQUIT: "quit",
    signal.SIGABRT: "aborted",
    signal.SIGBUS: "bus error",
    signal.SIGFPE: "floating point exception",
    signal.SIGKILL: "killed",
    signal.SIGSEGV: "segmentation fault",
    signal.SIGPIPE: "broken pipe",
    signal.SIGTERM: "terminated",
}


def exit_text(code):
    # Formats like Go's *exec.ExitError, which planter puts in check_script_failure findings.
    if code >= 0:
        return f"exit status {code}"
    return f"signal: {SIGNAL_NAMES.get(-code, str(-code))}"


def descendants(pid):
    """Returns the pids of pid's descendant processes (Linux /proc; empty elsewhere)."""
    children = {}
    try:
        entries = os.listdir("/proc")
    except OSError:
        return []
    for entry in entries:
        if not entry.isdigit():
            continue
        try:
            with open(f"/proc/{entry}/stat") as f:
                stat = f.read()
            # "pid (comm) state ppid ...", where comm may contain spaces and parentheses.
            ppid = int(stat[stat.rindex(")") + 2 :].split()[1])
        except (OSError, ValueError, IndexError):
            continue
        children.setdefault(ppid, []).append(int(entry))
    found, todo = [], [pid]
    while todo:
        for child in children.get(todo.pop(), []):
            found.append(child)
            todo.append(child)
    return found


def signal_all(pids, sig):
    for pid in pids:
        try:
            os.kill(pid, sig)
        except OSError:
            pass


def alive(pid):
    try:
        os.kill(pid, 0)
        return True
    except OSError:
        return False


def stop(proc):
    # Stops the check and everything it started (e.g. the fx build of build_verification),
    # which would otherwise keep running after bash exits.
    if proc.poll() is not None:
        return
    pids = [proc.pid] + descendants(proc.pid)
    signal_all(pids, signal.SIGTERM)
    deadline = time.monotonic() + 10
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        pass
    while time.monotonic() < deadline and any(alive(p) for p in pids[1:]):
        time.sleep(0.2)
    signal_all(pids, signal.SIGKILL)
    proc.wait()


def execute(cmd, cwd, env, label):
    """Runs cmd and returns (stdout, stderr, error text or None)."""
    try:
        proc = subprocess.Popen(
            cmd, cwd=cwd, env=env, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE
        )
    except OSError as e:
        return "", "", str(e)
    started = time.monotonic()
    try:
        while True:
            try:
                out, err = proc.communicate(timeout=HEARTBEAT_SECS)
                break
            except subprocess.TimeoutExpired:
                log(f"{label} still running ({fmt_secs(time.monotonic() - started)})")
    except BaseException:
        stop(proc)
        raise
    out = out.decode("utf-8", "replace")
    err = err.decode("utf-8", "replace")
    return out, err, None if proc.returncode == 0 else exit_text(proc.returncode)


def run_once(spec, target_dir):
    # Mirrors adapters.SubprocessToolExecutor.runCheckOnce.
    script = spec["script_file"]
    if not os.path.isabs(script):
        mroot = machinery_root()
        for cand in (
            os.path.normpath(os.path.join(mroot, "checks", script)),
            os.path.normpath(os.path.join(project_dir, "checks", script)),
            os.path.normpath(os.path.join(mroot, script)),
            os.path.normpath(os.path.join(project_dir, script)),
        ):
            if os.path.exists(cand):
                script = cand
                break
        else:
            return [
                new_finding(
                    source=spec["name"],
                    category="check_script_missing_in_pin",
                    severity="INFO",
                    message=(
                        f"check script {json.dumps(spec['script_file'])} is not present "
                        "in this pinned machinery snapshot; skipping."
                    ),
                )
            ]
    path = os.environ.get("PATH", "")
    env = dict(os.environ)
    env.update(
        {
            "SMELT_PROJECT_DIR": project_dir,
            "PLANTER_PROJECT_DIR": project_dir,
            "PLANTER_WORKDIR": workdir,
            "PLANTER_CHECK_NAME": spec["name"],
            "PLANTER_TARGET_DIR": target_dir,
            "PLANTER_TARGET_DIRS": " ".join(target_dirs),
            "PLANTER_CHANGE_BASE": change_base,
            "PATH": os.path.join(project_dir, "tools") + (os.pathsep + path if path else ""),
        }
    )
    if skip_build:
        env["PLANTER_SKIP_BUILD"] = "1"
    label = spec["name"] + (f" ({target_dir})" if len(target_dirs) > 1 else "")
    out, err, err_text = execute(["bash", script] + spec["args"], workdir or project_dir, env, label)

    raw = out.strip()
    findings, parsed_ok = parse_findings(raw)
    for f in findings:
        if not f["source"]:
            f["source"] = spec["name"]
        f["severity"] = normalize_severity(f["severity"])
    if err_text is not None and (not parsed_ok or not any(blocking(f) for f in findings)):
        msg = err.strip() or raw or err_text
        quoted = json.dumps(spec["name"], ensure_ascii=False)
        findings.append(
            new_finding(
                source=spec["name"],
                category="check_script_failure",
                severity="ERROR",
                message=f"check tool {quoted} exited with error ({err_text}): {msg}",
            )
        )
    return findings


def run_check(spec):
    # Mirrors adapters.SubprocessToolExecutor.RunCheckTool: once per target directory, with
    # identical findings reported once.
    seen = set()
    results = []
    for d in target_dirs or [""]:
        for f in run_once(spec, d):
            key = (f["source"], f["category"], f["file"], f["line"], f["message"])
            if key in seen:
                continue
            seen.add(key)
            if len(target_dirs) > 1 and not f["file"] and d:
                f["message"] = f"[{d}] {f['message']}"
            results.append(f)
    return results


def commit_msg_only_since_upload():
    uploaded_commit = os.environ.get("PLANTER_UPLOADED_COMMIT", "").strip()
    if not uploaded_commit or not workdir:
        return False
    try:
        head_sha = subprocess.check_output(
            ["git", "-C", workdir, "rev-parse", "HEAD"],
            stderr=subprocess.DEVNULL, text=True,
        ).strip()
        if not head_sha or head_sha == uploaded_commit:
            return False
        up_tree = subprocess.check_output(
            ["git", "-C", workdir, "rev-parse", f"{uploaded_commit}^{{tree}}"],
            stderr=subprocess.DEVNULL, text=True,
        ).strip()
        head_tree = subprocess.check_output(
            ["git", "-C", workdir, "rev-parse", "HEAD^{tree}"],
            stderr=subprocess.DEVNULL, text=True,
        ).strip()
        if not up_tree or up_tree != head_tree:
            return False
        # Folding an unmerged ancestor commit into HEAD (`git reset --soft <base> && git commit --amend`)
        # preserves HEAD^{tree} while changing HEAD~1; treat parent changes as full file changes.
        up_parent = subprocess.check_output(
            ["git", "-C", workdir, "rev-parse", "--verify", "--quiet", f"{uploaded_commit}~1^{{commit}}"],
            stderr=subprocess.DEVNULL, text=True,
        ).strip()
        head_parent = subprocess.check_output(
            ["git", "-C", workdir, "rev-parse", "--verify", "--quiet", "HEAD~1^{commit}"],
            stderr=subprocess.DEVNULL, text=True,
        ).strip()
        if up_parent != head_parent:
            return False
        diff_wt = subprocess.check_output(
            ["git", "-C", workdir, "diff", "--name-only", "HEAD", "--"],
            stderr=subprocess.DEVNULL, text=True,
        ).strip()
        if diff_wt:
            return False
        untracked = [
            u.strip() for u in subprocess.check_output(
                ["git", "-C", workdir, "ls-files", "--others", "--exclude-standard"],
                stderr=subprocess.DEVNULL, text=True,
            ).splitlines() if u.strip()
        ]
        if untracked:
            return False
        vg = os.path.join(workdir, "vendor", "google")
        if os.path.exists(os.path.join(vg, ".git")):
            diff_vg = subprocess.check_output(
                ["git", "-C", vg, "diff", "--name-only", "HEAD", "--"],
                stderr=subprocess.DEVNULL, text=True,
            ).strip()
            if diff_vg:
                return False
            untracked_vg = [
                u.strip() for u in subprocess.check_output(
                    ["git", "-C", vg, "ls-files", "--others", "--exclude-standard"],
                    stderr=subprocess.DEVNULL, text=True,
                ).splitlines() if u.strip()
            ]
            if untracked_vg:
                return False
        return True
    except Exception:
        return False


# Discover candidate planter executables, prioritizing ancestor and non-`diff`
# planter daemon processes in /proc/<pid>/exe (resolving to their stable on-disk
# readlink path unless unlinked after a rebuild) before PATH fallbacks so
# standalone review_diff runs match the in-memory daemon binary without racing
# short-lived sibling `planter diff` processes.
def find_planter_bins():
    import shutil

    bins = []
    seen = set()

    def is_transient_diff_proc(pid_val):
        try:
            with open(f"/proc/{pid_val}/cmdline", "rb") as cf:
                parts = cf.read().split(b"\x00")
                return len(parts) > 1 and parts[1] == b"diff"
        except OSError:
            return True

    def add_candidate(exe):
        if not exe:
            return
        resolved = exe
        if exe.startswith("/proc/"):
            try:
                target = os.readlink(exe)
            except OSError:
                return
            if (
                not target.endswith(" (deleted)")
                and os.path.isfile(target)
                and os.access(target, os.X_OK)
            ):
                resolved = target
        if not os.access(resolved, os.X_OK):
            return
        try:
            st = os.stat(resolved)
        except OSError:
            return
        key = (resolved.startswith("/proc/"), st.st_dev, st.st_ino)
        if key not in seen:
            seen.add(key)
            bins.append(resolved)

    pid = os.getppid()
    visited = set()
    while pid > 1 and pid not in visited:
        visited.add(pid)
        next_pid = 0
        try:
            with open(f"/proc/{pid}/comm", encoding="utf-8") as cf:
                if cf.read().strip() == "planter" and not is_transient_diff_proc(pid):
                    add_candidate(f"/proc/{pid}/exe")
            with open(f"/proc/{pid}/status", encoding="utf-8") as sf:
                for line in sf:
                    if line.startswith("PPid:"):
                        next_pid = int(line.split()[1])
                        break
        except (OSError, ValueError):
            break
        pid = next_pid

    try:
        for entry in os.listdir("/proc"):
            if not entry.isdigit() or int(entry) in visited:
                continue
            try:
                with open(f"/proc/{entry}/comm", encoding="utf-8") as cf:
                    if cf.read().strip() != "planter":
                        continue
            except OSError:
                continue
            if is_transient_diff_proc(entry):
                continue
            add_candidate(f"/proc/{entry}/exe")
    except OSError:
        pass

    add_candidate(shutil.which("planter"))
    cand = os.path.expanduser("~/.local/bin/planter")
    if os.path.isfile(cand):
        add_candidate(cand)
    return bins


def find_planter_bin():
    bins = find_planter_bins()
    return bins[0] if bins else None


# Fallback filter for standalone `--only review_diff` runs or mixed missing
# sources when *_test.go files from GN go_library.sources moved to a
# same-package go_test/host_go_test. In full check runs (and planter's own
# post-report check run), migration_sanity runs first and emits a matching INFO
# finding with the same (category, file, line, message) key so `seen` below (and
# planter's finding deduplicator) suppresses diffengine's raw missing_sources.
def filter_go_test_missing_sources(f):
    if f.get("category") != "missing_sources" or not f.get("file"):
        return f
    import re
    m = re.match(
        r'^(Target\s+"([^"]+)"\s+is missing source file\(s\) from BUILD\.gn:\s*)(.+)$',
        f.get("message", "").strip(),
    )
    if not m:
        return f
    prefix, target_name, files_str = m.group(1), m.group(2), m.group(3)
    missing = [s.strip() for s in files_str.split(",") if s.strip()]
    if not missing or not any(s.endswith("_test.go") for s in missing):
        return f
    bazel_path = os.path.join(workdir, f["file"])
    try:
        with open(bazel_path, encoding="utf-8") as bf:
            content = bf.read()
    except OSError:
        return f
    lib_pat = re.compile(
        r'\bgo_library\s*\([^)]*?\bname\s*=\s*"' + re.escape(target_name) + r'"',
        re.DOTALL,
    )
    if not lib_pat.search(content):
        return f
    test_srcs = set()
    for block in re.findall(r'\b(?:host_)?go_test\s*\((.*?)\n\)', content, re.DOTALL):
        if f'":{target_name}"' not in block and f'"{target_name}"' not in block:
            continue
        for s in re.findall(r'"([^"]+_test\.go)"', block):
            test_srcs.add(s)
    if not test_srcs:
        return f
    rem = [s for s in missing if not (s.endswith("_test.go") and s in test_srcs)]
    if not rem:
        return None
    if len(rem) != len(missing):
        f = dict(f)
        f["message"] = prefix + ", ".join(rem)
    return f


# Downgrade false-positive review_diff attribute_alignment warnings on
# rustc_check_cfg_build_flags targets (which bazel2gn natively converts to GN config()
# with --check-cfg rustflags), output_name carried via @bazel2gn:raw_overwrite or
# crate_name, GN test_specs.log_settings.max_severity carried via max_log_severity on
# fx_test / fx_unittest_package, and dep_alignment warnings caused by multi-line wrapped
# `deps =\n _VAR + ...` assignments in BUILD.gn or third_party_target_map.json mappings.
def downgrade_check_cfg_diff_findings(findings):
    check_cfg_locs = {
        (f["file"], f["line"])
        for f in findings
        if f["category"] == "attribute_alignment"
        and "Unusual rule mapping from config to rustc_check_cfg_build_flags" in f["message"]
    }
    raw_overwrite_locs = {
        (f["file"], f["line"])
        for f in findings
        if f["category"] == "bazel2gn_directive"
    }
    for f in findings:
        if (f["file"], f["line"]) in check_cfg_locs and f["category"] == "attribute_alignment":
            if (
                "Unusual rule mapping from config to rustc_check_cfg_build_flags" in f["message"]
                or "rustflags in GN not carried over to Bazel" in f["message"]
            ):
                f["severity"] = "INFO"
        elif (
            f["category"] == "template_parity"
            and "rustc_library_with_features(" in f["message"]
            and "bazel2gn generated rustc_library(" in f["message"]
        ):
            f["severity"] = "INFO"
        elif (
            f["category"] == "attribute_alignment"
            and "Unusual rule mapping from fuchsia_" in f["message"]
            and " to fx_" in f["message"]
        ):
            f["severity"] = "INFO"
        elif (
            f["category"] == "attribute_alignment"
            and (
                "output_name in GN not carried over to Bazel" in f["message"]
                or "rustenv (" in f["message"]
                or "added lint_config=" in f["message"]
            )
            and (f["file"], f["line"]) in raw_overwrite_locs
        ):
            f["severity"] = "INFO"
        elif (
            f["category"] == "missing_sources"
            and f["file"].endswith("BUILD.bazel")
            and os.path.isfile(os.path.join(workdir, f["file"]))
        ):
            bz_txt = open(
                os.path.join(workdir, f["file"]), encoding="utf-8", errors="ignore"
            ).read()
            if "compile_data" in bz_txt:
                f["severity"] = "INFO"
        elif (
            f["category"] == "attribute_alignment"
            and (
                "Unusual rule mapping from fuchsia_component_manifest to fx_component_manifest" in f["message"]
                or "Unusual rule mapping from resource_tree to fuchsia_package_resource_group" in f["message"]
                or ("sources_root in GN not carried over to Bazel" in f["message"] and "dest_dir in GN not carried over to Bazel" in f["message"])
            )
        ):
            f["severity"] = "INFO"
        # Downgrade gn_unmodified when BUILD.gn already has the sentinel or the sibling
        # BUILD.bazel only exports files / non-bazel2gn targets.
        elif (
            f["category"] == "gn_unmodified"
            and f["file"].endswith("BUILD.gn")
            and os.path.isfile(os.path.join(workdir, f["file"]))
        ):
            gn_txt = open(
                os.path.join(workdir, f["file"]), encoding="utf-8", errors="ignore"
            ).read()
            bz_path = os.path.join(workdir, os.path.dirname(f["file"]), "BUILD.bazel")
            bz_txt = (
                open(bz_path, encoding="utf-8", errors="ignore").read()
                if os.path.isfile(bz_path)
                else ""
            )
            if (
                "## BAZEL2GN SENTINEL" in gn_txt
                or "exports_files(" in bz_txt
                or "# @bazel2gn:skip" in bz_txt
            ):
                f["severity"] = "INFO"
        elif (
            f["category"] in ("dep_alignment", "unit_test_parity")
            and f["file"].endswith("BUILD.bazel")
            and os.path.isfile(os.path.join(workdir, f["file"]))
        ):
            bz_txt = open(
                os.path.join(workdir, f["file"]), encoding="utf-8", errors="ignore"
            ).read()
            if (
                (f["file"], f["line"]) in raw_overwrite_locs
                or "crate =" in bz_txt
            ) and (
                "@bazel2gn:raw_attribute:" in bz_txt
                or "@bazel2gn:path_overwrite:" in bz_txt
            ):
                f["severity"] = "INFO"
        elif (
            f["category"] == "attribute_alignment"
            and "max_severity in GN not carried over to Bazel" in f["message"]
            and f["file"].endswith("BUILD.bazel")
            and os.path.isfile(os.path.join(workdir, f["file"]))
        ):
            bz_txt = open(
                os.path.join(workdir, f["file"]), encoding="utf-8", errors="ignore"
            ).read()
            if "max_log_severity" in bz_txt:
                f["severity"] = "INFO"
        elif (
            f["category"] == "attribute_alignment"
            and "args in GN not carried over to Bazel" in f["message"]
            and f["file"].endswith("BUILD.bazel")
        ):
            bz_path = os.path.join(workdir, f["file"])
            try:
                bz_txt = open(bz_path, encoding="utf-8").read()
            except OSError:
                bz_txt = ""
            if "args =" in bz_txt or "test_args =" in bz_txt:
                f["severity"] = "INFO"
        elif (
            f["category"] == "dep_alignment"
            and "unmatched in GN: [//third_party/googletest:" in f["message"]
            and "unmatched in Bazel: [@com_google_googletest//:gtest" in f["message"]
        ):
            f["severity"] = "INFO"
        elif (
            f["category"] == "dep_alignment"
            and "unmatched in GN: [" in f["message"]
            and "unmatched in Bazel: [" in f["message"]
        ):
            # Downgrade dep_alignment warnings when unmatched GN and Bazel labels
            # differ only by redundant slashes (e.g. `////pkg/path` vs `//pkg/path`)
            # or expand a GN forwarding group in the same dependency package.
            import re as _re
            gn_m = _re.search(r"unmatched in GN: \[([^\]]+)\]", f["message"])
            bz_m = _re.search(r"unmatched in Bazel: \[([^\]]+)\]", f["message"])
            if gn_m and bz_m:
                def _slash_norm_lbl(raw):
                    s = raw.strip().strip('"')
                    if s.startswith("/"):
                        p, sep, t = s.partition(":")
                        p = "//" + _re.sub(r"/+", "/", p.strip("/"))
                        s = f"{p}:{t}" if sep else p
                    if s.startswith("//") and ":" in s:
                        p, t = s.split(":", 1)
                        if t == p.rsplit("/", 1)[-1]:
                            s = p
                    return s.replace("-", "_")

                def _is_fwd_grp_pair(g_lbl, b_lbl):
                    if g_lbl == b_lbl:
                        return True
                    if not (g_lbl.startswith("//") and b_lbl.startswith("//")):
                        return False
                    gp, _, gt = g_lbl[2:].partition(":")
                    bp, _, bt = b_lbl[2:].partition(":")
                    if not gp or gp != bp:
                        return False
                    gt_raw = gt or os.path.basename(gp)
                    bt_norm = (bt or os.path.basename(bp)).replace("-", "_")
                    dep_gn = os.path.join(workdir, gp, "BUILD.gn")
                    if not os.path.isfile(dep_gn):
                        return False
                    try:
                        d_txt = open(dep_gn, encoding="utf-8", errors="ignore").read()
                    except OSError:
                        return False
                    for gm in _re.finditer(r'\bgroup\(\s*"([^"$]+)"\s*\)\s*\{([^}]*)\}', d_txt):
                        if gm.group(1).replace("-", "_") == gt_raw.replace("-", "_"):
                            for dm in _re.findall(r'"([^"]+)"', gm.group(2)):
                                dm_clean = dm.lstrip(":").split(":")[-1].replace("-", "_")
                                if dm_clean == bt_norm:
                                    return True
                    return False

                gn_list = [_slash_norm_lbl(x) for x in gn_m.group(1).split(",") if x.strip()]
                bz_list = [_slash_norm_lbl(x) for x in bz_m.group(1).split(",") if x.strip()]
                if (
                    gn_list
                    and bz_list
                    and all(any(_is_fwd_grp_pair(g, b) for b in bz_list) for g in gn_list)
                    and all(any(_is_fwd_grp_pair(g, b) for g in gn_list) for b in bz_list)
                ):
                    f["severity"] = "INFO"
        elif (
            f["category"] == "dep_alignment"
            and "unmatched in GN: [" in f["message"]
            and "unmatched in Bazel:" not in f["message"]
            and f["file"].endswith("BUILD.bazel")
        ):
            # Downgrade dep_alignment warnings when unmatched GN test-package deps
            # (e.g. expect_includes shard targets) are placed in aspect_hints of
            # the test binaries in BUILD.bazel.
            import re as _re
            gn_m = _re.search(r"unmatched in GN: \[([^\]]+)\]", f["message"])
            bz_path = os.path.join(workdir, f["file"])
            try:
                bz_txt = open(bz_path, encoding="utf-8").read()
            except OSError:
                bz_txt = ""
            if gn_m and "aspect_hints" in bz_txt:
                unmatched_lbls = [x.strip().strip('"') for x in gn_m.group(1).split(",") if x.strip()]
                if unmatched_lbls and all(f'"{lbl}"' in bz_txt for lbl in unmatched_lbls):
                    f["severity"] = "INFO"
        elif (
            f["category"] == "dep_alignment"
            and "unmatched in Bazel:" in f["message"]
            and "unmatched in GN:" not in f["message"]
            and f["file"].endswith("BUILD.bazel")
        ):
            gn_path = os.path.join(workdir, os.path.dirname(f["file"]), "BUILD.gn")
            try:
                gn_txt = open(gn_path, encoding="utf-8").read()
            except OSError:
                gn_txt = ""
            if "## BAZEL2GN SENTINEL" in gn_txt:
                post_gn = gn_txt.split("## BAZEL2GN SENTINEL", 1)[1]
                bz_m = f["message"].split("unmatched in Bazel: [", 1)
                unmatched_bz = (
                    [x.strip() for x in bz_m[1].split("]", 1)[0].split(",") if x.strip()]
                    if len(bz_m) > 1
                    else []
                )
                if (
                    "\n    deps =\n" in gn_txt
                    or "\n  deps =\n" in gn_txt
                    or (unmatched_bz and all(f'"{u}"' in post_gn for u in unmatched_bz))
                ):
                    f["severity"] = "INFO"
        # Downgrade copyright_header and package_hygiene/default_visibility on
        # auto-generated crate_universe third_party/rust_crates BUILD.bazel files
        # and shared bundles/assembly files even when `--only review_diff` runs.
        elif (
            f["category"] in ("copyright_header", "package_hygiene", "default_visibility")
            and (
                (
                    f["file"].startswith("third_party/rust_crates/")
                    and not f["file"].startswith("third_party/rust_crates/compat/")
                )
                or f["file"].startswith("bundles/assembly/")
            )
            and f["file"].endswith(("BUILD.bazel", "BUILD"))
        ):
            f["severity"] = "INFO"
        elif f["file"].startswith("bundles/assembly/") and f["category"] in (
            "sentinel_drift",
            "dropped_comment",
            "dep_alignment",
            "attribute_alignment",
            "gn_unmodified",
        ):
            f["severity"] = "INFO"
        elif (
            f["category"] == "attribute_alignment"
            and (
                "output_name in GN not carried over to Bazel" in f["message"]
                or "GN Rust configs handled by toolchain defaults / lint_config" in f["message"]
                or "configs mapped to Bazel equivalents" in f["message"]
                or "added crate_name=" in f["message"]
            )
            and (
                "GN Rust configs handled by toolchain defaults / lint_config" in f["message"]
                or "configs mapped to Bazel equivalents" in f["message"]
                or "GN with_unit_tests decomposed into explicit test target" in f["message"]
                or "!= lint_config (" in f["message"]
            )
        ):
            f["severity"] = "INFO"
        elif (
            f["category"] == "dep_alignment"
            and "unmatched in GN: [" in f["message"]
            and "unmatched in Bazel:" not in f["message"]
            and f["file"].endswith("BUILD.bazel")
            and os.path.isfile(os.path.join(workdir, f["file"]))
        ):
            bz_txt = open(
                os.path.join(workdir, f["file"]), encoding="utf-8", errors="ignore"
            ).read()
            if "fx_package(" in bz_txt and (
                "components =" in bz_txt or "resources =" in bz_txt
            ):
                f["severity"] = "INFO"
        elif (
            f["category"] == "attribute_alignment"
            and "added package_name=" in f["message"]
            and (
                "log_settings in GN not carried over to Bazel" in f["message"]
                or "max_severity in GN not carried over to Bazel" in f["message"]
                or "test_specs in GN not carried over to Bazel" in f["message"]
            )
            and f["file"].endswith("BUILD.bazel")
            and os.path.isfile(os.path.join(workdir, f["file"]))
        ):
            bz_txt = open(
                os.path.join(workdir, f["file"]), encoding="utf-8", errors="ignore"
            ).read()
            if "fx_package(" in bz_txt:
                f["severity"] = "INFO"
        elif (
            f["category"] == "attribute_alignment"
            and (
                "component_name ([" in f["message"]
                or "cm_label in GN not carried over to Bazel" in f["message"]
                or ("added component_name=" in f["message"] and " in Bazel" in f["message"])
            )
            and f["file"].endswith("BUILD.bazel")
            and os.path.isfile(os.path.join(workdir, f["file"]))
        ):
            bz_txt = open(
                os.path.join(workdir, f["file"]), encoding="utf-8", errors="ignore"
            ).read()
            if "fx_component(" in bz_txt or "fx_test_component(" in bz_txt:
                f["severity"] = "INFO"
        # Downgrade attribute_alignment on fx_unittest_package / fx_unittest_component
        # when GN had default or bazel2gn-synthesized <name>-gn package_name / component_name.
        elif (
            f["category"] == "attribute_alignment"
            and (
                "package_name in GN not carried over to Bazel" in f["message"]
                or "component_name in GN not carried over to Bazel" in f["message"]
            )
            and f["file"].endswith("BUILD.bazel")
            and os.path.isfile(os.path.join(workdir, f["file"]))
        ):
            bz_txt = open(
                os.path.join(workdir, f["file"]), encoding="utf-8", errors="ignore"
            ).read()
            if "fx_unittest_package(" in bz_txt or "fx_unittest_component(" in bz_txt:
                f["severity"] = "INFO"
        # Downgrade unmatched_target, dep_alignment, dropped_comment, and
        # attribute_alignment when diffengine greedily pairs an unmigrated
        # pre-sentinel GN target with a Bazel target via crate_name/output_name/binary_name,
        # leaving the bazel2gn-synced GN target of the same name unmatched.
        elif (
            f["category"] == "unmatched_target"
            and f["file"].endswith("BUILD.gn")
            and f["message"].startswith('GN target "')
            and os.path.isfile(os.path.join(workdir, f["file"]))
        ):
            tname = f["message"].split('"', 2)[1]
            gn_txt = open(
                os.path.join(workdir, f["file"]), encoding="utf-8", errors="ignore"
            ).read()
            bz_path = os.path.join(workdir, os.path.dirname(f["file"]), "BUILD.bazel")
            bz_txt = (
                open(bz_path, encoding="utf-8", errors="ignore").read()
                if os.path.isfile(bz_path)
                else ""
            )
            if (
                "## BAZEL2GN SENTINEL" in gn_txt
                and (
                    (
                        f'name = "{tname}"' in bz_txt
                        and f'("{tname}")' in gn_txt.split("## BAZEL2GN SENTINEL", 1)[1]
                    )
                    or (
                        "target_name" in tname
                        and "template(" not in gn_txt.split("## BAZEL2GN SENTINEL", 1)[0]
                    )
                )
            ):
                f["severity"] = "INFO"
        elif (
            f["category"] in ("dep_alignment", "dropped_comment", "attribute_alignment")
            and f["file"].endswith("BUILD.bazel")
            and f["message"].startswith('Target "')
            and os.path.isfile(os.path.join(workdir, f["file"]))
        ):
            tname = f["message"].split('"', 2)[1]
            gn_path = os.path.join(workdir, os.path.dirname(f["file"]), "BUILD.gn")
            gn_txt = (
                open(gn_path, encoding="utf-8", errors="ignore").read()
                if os.path.isfile(gn_path)
                else ""
            )
            bz_txt = open(
                os.path.join(workdir, f["file"]), encoding="utf-8", errors="ignore"
            ).read()
            if "## BAZEL2GN SENTINEL" in gn_txt:
                pre_gn, post_gn = gn_txt.split("## BAZEL2GN SENTINEL", 1)
                if (
                    f'name = "{tname}"' in bz_txt
                    and f'("{tname}")' in post_gn
                    and f'("{tname}")' not in pre_gn
                    and any(
                        f'{attr} = "' in bz_txt
                        for attr in ("crate_name", "output_name", "binary_name")
                    )
                ):
                    f["severity"] = "INFO"
        elif (
            f["category"] == "dropped_comment"
            and "dropped comment from BUILD.gn:" in f["message"]
        ):
            import re as _re
            _, _, dropped_txt = f["message"].partition("dropped comment from BUILD.gn:")
            if _re.match(
                r'^\s*#\s*["\']((?://|:|@)[^"\'\s]+|[A-Za-z0-9_./-]+\.'
                r'(?:rs|cc|cpp|c|h|hpp|py|go|fidl|cml|json\d*))["\']\s*,?\s*(?:#.*)?$',
                dropped_txt.strip(),
            ):
                f["severity"] = "INFO"


# Check whether a BUILD.bazel outside the migrated target directories (pre-existing or
# newly created) only registers tests in test_suite(), exports files/shards via
# exports_files()/cml_shard(), or updates target visibility allowlists, without dual-build
# migrating that package.
_REG_ONLY_PKG_CACHE = {}


def is_registration_only_bazel_pkg(pkg_dir):
    pkg_dir = pkg_dir.strip("/")
    if pkg_dir in _REG_ONLY_PKG_CACHE:
        return _REG_ONLY_PKG_CACHE[pkg_dir]
    if not pkg_dir or not workdir or pkg_dir in set(target_dirs):
        _REG_ONLY_PKG_CACHE[pkg_dir] = False
        return False
    gn_path = os.path.join(workdir, pkg_dir, "BUILD.gn")
    if os.path.isfile(gn_path):
        try:
            if "BAZEL2GN SENTINEL" in open(gn_path, encoding="utf-8", errors="ignore").read():
                _REG_ONLY_PKG_CACHE[pkg_dir] = False
                return False
        except OSError:
            pass
    bazel_rel = f"{pkg_dir}/BUILD.bazel"
    bazel_path = os.path.join(workdir, bazel_rel)
    if not os.path.isfile(bazel_path):
        _REG_ONLY_PKG_CACHE[pkg_dir] = False
        return False
    try:
        cur_src = open(bazel_path, encoding="utf-8").read()
    except OSError:
        _REG_ONLY_PKG_CACHE[pkg_dir] = False
        return False
    prev_src = ""
    for ref in dict.fromkeys([change_base, "HEAD", "HEAD~1"]):
        if not ref:
            continue
        try:
            cand = subprocess.check_output(
                ["git", "-C", workdir, "show", f"{ref}:{bazel_rel}"],
                stderr=subprocess.DEVNULL,
                text=True,
            )
            if cand == cur_src and ref != "HEAD~1":
                continue
            prev_src = cand
            break
        except Exception:
            continue
    try:
        import ast

        prev_tree = ast.parse(prev_src, filename=bazel_rel)
        cur_tree = ast.parse(cur_src, filename=bazel_rel)
    except Exception:
        _REG_ONLY_PKG_CACHE[pkg_dir] = False
        return False

    def strip_vis_dump(stmt):
        if not (isinstance(stmt, ast.Expr) and isinstance(stmt.value, ast.Call)):
            return None
        call = stmt.value
        kept = [(kw.arg, ast.dump(kw.value)) for kw in call.keywords if kw.arg != "visibility"]
        return (ast.dump(call.func), tuple(ast.dump(a) for a in call.args), tuple(kept))

    prev_dumps = {ast.dump(s) for s in prev_tree.body}
    prev_stripped = {d for d in (strip_vis_dump(s) for s in prev_tree.body) if d is not None}
    reg_only_fns = (
        "load",
        "package",
        "visibility",
        "test_suite",
        "exports_files",
        "stamp_group",
        "filegroup",
        "package_group",
        "cml_shard",
    )
    for stmt in cur_tree.body:
        if ast.dump(stmt) in prev_dumps:
            continue
        if not (isinstance(stmt, ast.Expr) and isinstance(stmt.value, ast.Call)):
            _REG_ONLY_PKG_CACHE[pkg_dir] = False
            return False
        call = stmt.value
        fn = (
            call.func.id
            if isinstance(call.func, ast.Name)
            else (call.func.attr if isinstance(call.func, ast.Attribute) else "")
        )
        if fn in reg_only_fns or strip_vis_dump(stmt) in prev_stripped:
            continue
        _REG_ONLY_PKG_CACHE[pkg_dir] = False
        return False
    _REG_ONLY_PKG_CACHE[pkg_dir] = True
    return True


GN_TO_BAZEL_ATTR_EQUIVALENTS = {
    "args_sources": ("args_srcs", "args_sources"),
    "args_source_root": ("args_crate_root", "args_source_root"),
    "sources": ("srcs", "sources"),
    "public": ("hdrs", "public"),
    "configs": (
        "copts",
        "build_flags",
        "disable_build_flags",
        "lint_config",
        "configs",
        "compiler_subflags",
    ),
    "inputs": ("compile_data", "data", "srcs", "inputs"),
    "non_rust_deps": ("data", "test_data", "non_rust_deps"),
    "data_deps": ("data", "test_data", "data_deps"),
    "features": ("crate_features", "features"),
    "rustflags": ("rustc_flags", "build_flags", "rustflags"),
    "source_root": ("crate_root", "source_root"),
    "rustenv": ("rustc_env", "rustenv"),
    "output_name": ("binary_name", "crate_name", "out", "output_name"),
    "sdk_category": ("sdk_category", "category"),
    "sdk_publishable": ("category", "sdk_publishable"),
    "sdk_area": ("sdk_area", "api_area"),
    "api": ("api_file_path", "api"),
    "sdk_name": ("idk_name", "sdk_name"),
    "sdk_target_name": ("sdk_target_name", "idk_name", "category"),
    "sdk_headers_for_internal_use": ("hdrs_for_internal_use", "sdk_headers_for_internal_use"),
    "outputs": ("outs", "tool_output_names", "dest_dir", "files_map", "outputs"),
    "main_source": ("main", "main_source"),
    "global": ("global_registry", "global"),
    "check_cfgs": ("build_flags", "check_cfgs"),
    "feature_sets": ("build_flags", "feature_sets"),
}
BAZEL_TEST_RULES = {
    "rustc_test",
    "rust_test",
    "host_rustc_test",
    "wrap_host_rust_test",
    "fx_test",
    "fx_unittest_package",
    "fx_unittest_component",
    "fx_test_component",
    "cc_test",
    "fx_cc_test",
    "host_cc_test",
    "host_py_test",
    "host_go_test",
    "host_test",
    "go_test",
}
DECOMPOSED_GN_UNIT_TEST_ATTRS = {
    "with_unit_tests",
    "args_with_unit_tests",
    "test_deps",
    "args_test_deps",
    "deps_for_tests",
}


# Verify whether a Target Attributes attribute_alignment finding only flags GN attributes
# that are mapped in BUILD.bazel (e.g. args_sources -> args_srcs, sdk_category), preserved
# below the BAZEL2GN SENTINEL in BUILD.gn, or decomposed into a Bazel test target.
def is_verified_target_attributes_alignment(f):
    import re

    msg = f.get("message", "")
    m = re.match(r'^Target "([^"]+)" \(Target Attributes\): (.+)$', msg)
    if not m or not workdir:
        return False
    target_name = m.group(1)
    raw_clauses = m.group(2).replace("; the IDK atom is renamed", "")
    notes = [n.strip() for n in raw_clauses.split(";") if n.strip()]
    if not notes:
        return False
    pkg_dir = os.path.dirname(str(f.get("file", ""))).strip("/")
    bazel_path = os.path.join(workdir, pkg_dir, "BUILD.bazel")
    if not os.path.isfile(bazel_path):
        return False
    try:
        import ast

        tree = ast.parse(open(bazel_path, encoding="utf-8").read(), filename=bazel_path)
    except Exception:
        return False
    bt_rule = None
    bt_kws = set()
    has_bazel_test_rule = False
    has_bazel_test_args = False
    for stmt in tree.body:
        if isinstance(stmt, ast.Expr) and isinstance(stmt.value, ast.Call):
            call = stmt.value
            rule = call.func.id if isinstance(call.func, ast.Name) else None
            if not rule:
                continue
            kws = {kw.arg: kw.value for kw in call.keywords if kw.arg}
            if rule in BAZEL_TEST_RULES:
                has_bazel_test_rule = True
                if "test_args" in kws or "args" in kws:
                    has_bazel_test_args = True
            tname = kws.get("name")
            if (
                isinstance(tname, ast.Constant)
                and isinstance(tname.value, str)
                and tname.value == target_name
            ):
                bt_rule = rule
                bt_kws = set(kws.keys())
    if not bt_rule:
        return False
    gen_gn_body = ""
    gn_path = os.path.join(workdir, pkg_dir, "BUILD.gn")
    if os.path.isfile(gn_path):
        try:
            gn_cur = open(gn_path, encoding="utf-8", errors="ignore").read()
            if "BAZEL2GN SENTINEL" in gn_cur:
                gen_gn_body = gn_cur.split("BAZEL2GN SENTINEL", 1)[1]
        except OSError:
            pass
    for note in notes:
        if note == "GN with_unit_tests decomposed into explicit test target":
            if not (has_bazel_test_rule or "with_unit_tests" in bt_kws or "with_host_unit_tests" in bt_kws):
                return False
            continue
        if note == "GN Rust configs handled by toolchain defaults / lint_config":
            continue
        if (
            re.match(
                r'^idk_name=\S+ differs from GN\'s default sdk_name "[^"]+" \(the target name\)$',
                note,
            )
            and "idk_name" in bt_kws
        ):
            continue
        am = re.match(r"^([A-Za-z_]\w*) in GN not carried over to Bazel$", note)
        if not am:
            return False
        gn_attr = am.group(1)
        bz_equivs = GN_TO_BAZEL_ATTR_EQUIVALENTS.get(gn_attr, (gn_attr,))
        in_bazel = gn_attr in bt_kws or any(b in bt_kws for b in bz_equivs)
        in_gen_gn = bool(
            gn_attr != "args"
            and gen_gn_body
            and re.search(r"(?<![\w.])" + re.escape(gn_attr) + r"\s*\+?=", gen_gn_body)
        )
        is_decomposed_test = (
            (gn_attr in DECOMPOSED_GN_UNIT_TEST_ATTRS and has_bazel_test_rule)
            or (gn_attr == "args" and has_bazel_test_args)
        )
        if not (in_bazel or in_gen_gn or is_decomposed_test):
            return False
    return True


# Verify whether a review_diff unit_test_parity finding is already satisfied via
# aspect_hints, data/test_data/compile_data, link_deps, @bazel2gn directives, or
# the bazel2gn-generated GN unit test target below the BAZEL2GN SENTINEL.
def is_verified_unit_test_parity(f):
    import re

    msg = f.get("message", "")
    m = re.match(
        r'^Unit tests of "([^"]+)": GN test_deps \[([^\]]+)\] '
        r'are not dependencies of "([^"]+)"$',
        msg,
    )
    if not m:
        return False
    missing_deps = [d.strip() for d in m.group(2).split(",") if d.strip()]
    if not missing_deps:
        return False
    rel_file = str(f.get("file", ""))
    pkg_dir = os.path.dirname(rel_file).strip("/")
    bazel_path = os.path.join(workdir, rel_file)
    bazel_text = ""
    if os.path.isfile(bazel_path):
        try:
            bazel_text = open(bazel_path, encoding="utf-8", errors="ignore").read()
        except OSError:
            pass
    gen_gn_body = ""
    gn_path = os.path.join(workdir, pkg_dir, "BUILD.gn")
    if os.path.isfile(gn_path):
        try:
            gn_cur = open(gn_path, encoding="utf-8", errors="ignore").read()
            if "BAZEL2GN SENTINEL" in gn_cur:
                gen_gn_body = gn_cur.split("BAZEL2GN SENTINEL", 1)[1]
        except OSError:
            pass
    for dep in missing_deps:
        base_dep = dep.split("(", 1)[0].strip()
        if f'"{base_dep}"' not in bazel_text and f'"{base_dep}"' not in gen_gn_body:
            return False
    return True


# Verify whether a review_diff missing_sources finding only flags GN relative parent
# paths (`../...`) or cross-package paths that resolve to the same workspace-relative
# file path as canonical `//<owner_pkg>:<rel_path>` labels on the Bazel target.
def is_verified_cross_package_sources(f):
    import re

    msg = f.get("message", "")
    m = re.match(
        r'^Target "([^"]+)" is missing source file\(s\) from BUILD\.gn:\s*(.+)$',
        msg.strip(),
    )
    if not m:
        return False
    missing_srcs = [s.strip() for s in m.group(2).split(",") if s.strip()]
    if not missing_srcs:
        return False
    rel_file = str(f.get("file", ""))
    pkg_dir = os.path.dirname(rel_file).strip("/")
    bazel_path = os.path.join(workdir, rel_file)
    if not os.path.isfile(bazel_path):
        return False
    try:
        bazel_text = open(bazel_path, encoding="utf-8", errors="ignore").read()
    except OSError:
        return False
    bz_resolved = set()
    for raw_lbl in re.findall(r'"([^"]+)"', bazel_text):
        s = raw_lbl.strip()
        if not s or "$" in s or s.startswith("@"):
            continue
        if s.startswith("//"):
            norm_b = os.path.normpath(s[2:].replace(":", "/")).strip("/")
        else:
            norm_b = os.path.normpath(os.path.join(pkg_dir, s.lstrip(":"))).strip("/")
        if norm_b:
            bz_resolved.add(norm_b)
    for g in missing_srcs:
        if g.startswith("//"):
            norm_g = os.path.normpath(g[2:].replace(":", "/")).strip("/")
        else:
            norm_g = os.path.normpath(os.path.join(pkg_dir, g.lstrip(":"))).strip("/")
        if not norm_g or norm_g not in bz_resolved:
            return False
    return True


# Verify whether a review_diff sentinel_drift finding only flags Bazel targets
# whose rules map to `noGNEquivalent` in `//build/tools/bazel2gn/constants.go`.
def is_verified_no_gn_equiv_sentinel_drift(f):
    import ast
    import re

    msg = f.get("message", "")
    if (
        msg.strip()
        == "BUILD.gn does not have a bazel2gn sentinel; verify whether not using bazel2gn is intentional."
        and workdir
    ):
        gn_path = os.path.join(workdir, str(f.get("file", "")))
        if os.path.isfile(gn_path):
            try:
                gn_txt = open(gn_path, encoding="utf-8", errors="ignore").read()
                if "BAZEL2GN SENTINEL" not in gn_txt and any(
                    tok in gn_txt
                    for tok in (
                        "bazel_ffx_tool(",
                        "bazel_host_tool(",
                        "bazel_fuchsia_package(",
                        "bazel_test_suite(",
                    )
                ):
                    return True
            except OSError:
                pass
    m = re.match(
        r"^Bazel target\(s\) \[([^\]]+)\] without `# @bazel2gn:skip` are missing below ",
        msg.strip(),
    )
    if not m or not workdir:
        return False
    missing_names = [x.strip() for x in m.group(1).split(",") if x.strip()]
    if not missing_names:
        return False
    const_path = os.path.join(workdir, "build", "tools", "bazel2gn", "constants.go")
    no_gn_rules = {"ffx_tool", "idk_ffx_tool"}
    if os.path.isfile(const_path):
        try:
            c_txt = open(const_path, encoding="utf-8", errors="ignore").read()
            no_gn_rules.update(
                re.findall(r'"([A-Za-z0-9_]+)"\s*:\s*noGNEquivalent\b', c_txt)
            )
        except OSError:
            pass
    pkg_dir = os.path.dirname(str(f.get("file", ""))).strip("/")
    bazel_path = os.path.join(workdir, pkg_dir, "BUILD.bazel")
    if not os.path.isfile(bazel_path):
        return False
    try:
        tree = ast.parse(open(bazel_path, encoding="utf-8").read(), filename=bazel_path)
    except Exception:
        return False
    rules_by_name = {}
    for stmt in tree.body:
        if isinstance(stmt, ast.Expr) and isinstance(stmt.value, ast.Call):
            call = stmt.value
            rule = call.func.id if isinstance(call.func, ast.Name) else None
            if not rule:
                continue
            for kw in call.keywords:
                if (
                    kw.arg == "name"
                    and isinstance(kw.value, ast.Constant)
                    and isinstance(kw.value.value, str)
                ):
                    rules_by_name[kw.value.value] = rule
    return all(rules_by_name.get(n) in no_gn_rules for n in missing_names)


# Check whether a review_diff finding is a verified false positive for mapped/decomposed
# target attributes, cross-package source labels, unit_test_parity aspect_hints,
# bazel_fuchsia_package wrappers, noGNEquivalent sentinel omissions, or unmigrated
# ancestor registration packages.
def is_benign_review_diff_finding(f):
    f_pkg = os.path.dirname(str(f.get("file", ""))).strip("/")
    if f_pkg and is_registration_only_bazel_pkg(f_pkg):
        return True
    cat = f.get("category", "")
    msg = f.get("message", "")
    if cat == "sentinel_drift" and is_verified_no_gn_equiv_sentinel_drift(f):
        return True
    if cat == "missing_sources" and is_verified_cross_package_sources(f):
        return True
    if cat == "unit_test_parity" and is_verified_unit_test_parity(f):
        return True
    if cat == "attribute_alignment":
        if is_verified_target_attributes_alignment(f):
            return True
        if (
            "Unusual rule mapping from fuchsia_package" in msg
            or "Unusual rule mapping from bazel_fuchsia_package" in msg
            or "Unusual rule mapping from ffx_tool to idk_ffx_tool" in msg
        ):
            return True
    return False


def run_review_diff(existing_findings):
    planter_bins = find_planter_bins()
    if not planter_bins or not workdir:
        return None
    env = dict(os.environ)
    env.update(
        {
            "PLANTER_WORKDIR": workdir,
            "PLANTER_TARGET_DIRS": " ".join(target_dirs),
            "PLANTER_CHANGE_BASE": change_base,
        }
    )
    findings, parsed_ok = [], False
    for planter_bin in planter_bins:
        out, _, err_text = execute(
            [planter_bin, "diff", "--local", "--check-findings", f"--workdir={workdir}"],
            workdir,
            env,
            "review_diff",
        )
        findings, parsed_ok = parse_findings(out.strip())
        if parsed_ok or err_text is None:
            break
    if not parsed_ok:
        return []
    downgrade_check_cfg_diff_findings(findings)
    seen = {(f["category"], f["file"], f["line"], f["message"]) for f in existing_findings}
    results = []
    for f in findings:
        key = (f["category"], f["file"], f["line"], f["message"])
        if key in seen:
            continue
        f = filter_go_test_missing_sources(f)
        if f is None:
            continue
        f["source"] = "review_diff"
        f["severity"] = normalize_severity(f["severity"])
        if is_benign_review_diff_finding(f):
            f["severity"] = "INFO"
        key = (f["category"], f["file"], f["line"], f["message"])
        if key in seen:
            continue
        seen.add(key)
        results.append(f)
    return results


COMMIT_MSG_CHECKS = {
    "confidentiality_check",
    "cq_reachability",
    "commit_message_format",
    "build_verification",
}
commit_msg_only = not only and commit_msg_only_since_upload()
if commit_msg_only:
    enabled = [s for s in enabled if s["name"] in COMMIT_MSG_CHECKS]
    if len(target_dirs) > 1:
        target_dirs = target_dirs[:1]


# Snapshot check manifest and script metadata to detect mid-run machinery evolutions.
def machinery_fingerprint():
    checks_dir = os.path.join(machinery_root(), "checks")
    entries = []
    try:
        names = sorted(os.listdir(checks_dir))
    except OSError:
        return ()
    for fn in names:
        if fn == "manifest.json" or fn.endswith(".sh"):
            p = os.path.join(checks_dir, fn)
            try:
                st = os.stat(p)
                entries.append((fn, st.st_mtime_ns, st.st_size))
            except OSError:
                pass
    return tuple(entries)


# Wait for an in-progress machinery evolution after build_verification completes.
def wait_for_evolution_after_build(max_wait_secs=180):
    release_machinery_locks()
    if not evolution_running():
        return True
    log("waiting for in-progress machinery evolution to finish merging...")
    wait_start = time.monotonic()
    last_hb = wait_start
    while time.monotonic() - wait_start < max_wait_secs:
        release_machinery_locks()
        time.sleep(2.0)
        if not evolution_running():
            return True
        now = time.monotonic()
        if now - last_hb >= HEARTBEAT_SECS:
            log(f"still waiting for machinery evolution ({fmt_secs(now - wait_start)})")
            last_hb = now
    return False


builds_skipped = skip_build or os.environ.get("PLANTER_SKIP_BUILD", "").strip() not in ("", "0")
evolution_note = (
    "planter is evolving the machinery alongside you, so the checks may still change. Do not wait "
    "for it: report when this run passes; planter runs the checks again with the evolved "
    "machinery and sends any new finding back as another round."
)
if evolution_in_progress:
    log(evolution_note)
log(
    f"{len(enabled)} check(s) in {workdir}; target dirs: {' '.join(target_dirs) or '(none)'}; "
    f"change base: {change_base}"
    + ("; commit-msg only since upload" if commit_msg_only else "")
    + ("; builds skipped" if builds_skipped and not commit_msg_only else "")
)
initial_fp = machinery_fingerprint()
rows = []
all_findings = []
release_machinery_locks()

max_workers = min(8, os.cpu_count() or 4)
if os.environ.get("PLANTER_SERIAL_CHECKS", "").strip() in ("1", "true"):
    max_workers = 1

# Checks that invoke `fx build` / `fx gen` (regenerator.py) or temporarily revert
# working-tree build files and args.gn must never run concurrently with static
# checks or with each other when builds are enabled.
SERIAL_BUILD_CHECKS = {"build_verification", "tests_json_parity"}


def _exec_check(spec):
    started = time.monotonic()
    findings = run_check(spec)
    duration = time.monotonic() - started
    return spec, findings, duration


# Run read-only static checks in parallel first, then execute mutating/build
# checks serially while preserving checks/manifest.json output order.
def run_check_batch(specs_to_run):
    total = len(specs_to_run)
    if builds_skipped or max_workers <= 1:
        parallel_specs = list(specs_to_run)
        serial_specs = []
    else:
        parallel_specs = [s for s in specs_to_run if s["name"] not in SERIAL_BUILD_CHECKS]
        serial_specs = [s for s in specs_to_run if s["name"] in SERIAL_BUILD_CHECKS]

    results_by_idx = {}
    idx_map = {id(s): idx for idx, s in enumerate(specs_to_run)}
    done_count = 0

    if parallel_specs:
        with ThreadPoolExecutor(max_workers=max_workers) as executor:
            futures = [executor.submit(_exec_check, spec) for spec in parallel_specs]
            for fut in futures:
                spec, findings, duration = fut.result()
                done_count += 1
                log(f"({done_count}/{total}) {spec['name']} done ({fmt_secs(duration)})")
                results_by_idx[idx_map[id(spec)]] = (spec, findings, duration)

    for spec in serial_specs:
        spec, findings, duration = _exec_check(spec)
        done_count += 1
        log(f"({done_count}/{total}) {spec['name']} done ({fmt_secs(duration)})")
        results_by_idx[idx_map[id(spec)]] = (spec, findings, duration)

    batch_rows = [results_by_idx[i] for i in range(total)]
    batch_findings = [f for _, flist, _ in batch_rows for f in flist]
    return batch_rows, batch_findings


try:
    rows, all_findings = run_check_batch(enabled)
    if not commit_msg_only and (not only or "review_diff" in only):
        started = time.monotonic()
        diff_findings = run_review_diff(all_findings)
        if diff_findings is not None:
            rows.append(
                (
                    {"name": "review_diff", "enabled": True},
                    diff_findings,
                    time.monotonic() - started,
                )
            )
            all_findings.extend(diff_findings)
    # Re-run fast static checks when a parallel machinery evolution merged during this run.
    if not builds_skipped and (evolution_in_progress or evolution_running()):
        if wait_for_evolution_after_build():
            evolution_in_progress = False
    else:
        release_machinery_locks()
        if evolution_in_progress and not evolution_running():
            evolution_in_progress = False
    if not evolution_in_progress and machinery_fingerprint() != initial_fp:
        log("machinery evolution merged updated checks; re-running static checks...")
        try:
            reloaded = load_specs(manifest_path) or []
        except ValueError:
            reloaded = specs
        re_enabled = [s for s in reloaded if s["enabled"]]
        disabled = [s for s in reloaded if not s["enabled"]]
        if only:
            wanted = set(only)
            re_enabled = [s for s in re_enabled if s["name"] in wanted]
            disabled = [s for s in disabled if s["name"] in wanted]
        if commit_msg_only:
            re_enabled = [s for s in re_enabled if s["name"] in COMMIT_MSG_CHECKS]
        fp_before = {fn: (mt, sz) for fn, mt, sz in initial_fp}
        fp_after = {fn: (mt, sz) for fn, mt, sz in machinery_fingerprint()}
        bv_rows = {
            s["name"]: (s, f, t)
            for s, f, t in rows
            if s["name"] == "build_verification"
            or (
                s["name"] in SERIAL_BUILD_CHECKS
                and fp_before.get(s["script_file"]) == fp_after.get(s["script_file"])
            )
        }
        rows = []
        all_findings = []
        for s in re_enabled:
            if s["name"] in bv_rows:
                row = bv_rows[s["name"]]
            else:
                st = time.monotonic()
                row = (s, run_check(s), time.monotonic() - st)
            rows.append(row)
            all_findings.extend(row[1])
        if not commit_msg_only and (not only or "review_diff" in only):
            st = time.monotonic()
            diff_findings = run_review_diff(all_findings)
            if diff_findings is not None:
                rows.append(
                    (
                        {"name": "review_diff", "enabled": True},
                        diff_findings,
                        time.monotonic() - st,
                    )
                )
                all_findings.extend(diff_findings)
except KeyboardInterrupt:
    log("interrupted")
    sys.exit(130)

blocking_findings = [f for f in all_findings if blocking(f)]
exit_code = 1 if blocking_findings else 0

if as_json:
    out = []
    for f in all_findings:
        # Same fields as substrate.Finding, including its omitempty rules.
        o = {"source": f["source"], "category": f["category"], "severity": f["severity"]}
        if f["file"]:
            o["file"] = f["file"]
        if f["line"]:
            o["line"] = f["line"]
        o["message"] = f["message"]
        if f["remediation"]:
            o["remediation"] = f["remediation"]
        out.append(o)
    print(json.dumps(out, indent=2))
    sys.exit(exit_code)


def counts(findings):
    parts = []
    for sev in ("ERROR", "WARNING", "INFO"):
        n = sum(1 for f in findings if f["severity"] == sev)
        if n:
            parts.append(f"{n} {sev}")
    return ", ".join(parts)


def print_finding(prefix, f):
    where = f["file"] + (f":{f['line']}" if f["file"] and f["line"] else "")
    head = f"{prefix}[{f['severity']}] {f['source']}/{f['category'] or '-'}"
    print(head + (f"  {where}" if where else ""))
    indent = " " * 4
    for line in f["message"].splitlines() or [""]:
        print(indent + line)
    if f["remediation"]:
        rem = f["remediation"].splitlines()
        print(indent + "Remediation: " + rem[0])
        for line in rem[1:]:
            print(indent + line)


width = max([len(s["name"]) for s, _, _ in rows] + [len(s["name"]) for s in disabled] + [4])
print("Checks (planter runs these, in this order, after you report):")
for spec, findings, secs in rows:
    status = "FAIL" if any(blocking(f) for f in findings) else "PASS"
    detail = counts(findings)
    print(f"  {status}  {spec['name']:<{width}}  {fmt_secs(secs):>7}" + (f"  {detail}" if detail else ""))
for spec in disabled:
    print(f"  ----  {spec['name']:<{width}}  disabled in checks/manifest.json (planter skips it)")
if not rows:
    print("  (no enabled checks)")

if blocking_findings:
    print(f"\nBlocking findings ({len(blocking_findings)}); planter sends the change back for any of these:\n")
    for n, f in enumerate(blocking_findings, 1):
        print_finding(f"{n}. ", f)
        print()
others = [f for f in all_findings if not blocking(f)]
if others:
    print(f"\nNon-blocking findings ({len(others)}):\n")
    for f in others:
        print_finding("- ", f)
        print()

print()
if blocking_findings:
    sources = sorted({f["source"] for f in blocking_findings})
    print(
        f"RESULT: FAIL: {len(blocking_findings)} blocking finding(s) ({counts(blocking_findings)}) "
        f"from {', '.join(sources)}."
    )
else:
    print(
        "RESULT: PASS: no blocking findings "
        "(set disputed_findings to [] in CoderReport; any entry triggers machinery_conflict)."
    )
if only:
    print("NOTE: only some checks ran (--only). Run all of them before reporting.")
if any(f["category"] == "build_skipped" for f in all_findings):
    print("NOTE: the builds were skipped. Run `run_checks.sh` without --skip-build before reporting.")
if evolution_in_progress:
    print(f"NOTE: {evolution_note}")
sys.exit(exit_code)
PYEOF
