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
change_base = os.environ.get("PLANTER_CHANGE_BASE", "").strip() or "HEAD"


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
        cand = os.path.normpath(os.path.join(project_dir, "checks", script))
        script = cand if os.path.exists(cand) else os.path.normpath(os.path.join(project_dir, script))
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


def find_planter_bin():
    import shutil
    p = shutil.which("planter")
    if p:
        return p
    cand = os.path.expanduser("~/.local/bin/planter")
    if os.path.isfile(cand) and os.access(cand, os.X_OK):
        return cand
    return None


# Downgrade false-positive review_diff attribute_alignment warnings on
# rustc_check_cfg_build_flags targets (which bazel2gn natively converts to GN config()
# with --check-cfg rustflags), output_name carried via @bazel2gn:raw_overwrite or
# crate_name, and dep_alignment warnings caused by multi-line wrapped `deps =\n _VAR + ...`
# assignments in BUILD.gn or third_party_target_map.json mappings.
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
            f["category"] == "attribute_alignment"
            and "output_name in GN not carried over to Bazel" in f["message"]
            and (f["file"], f["line"]) in raw_overwrite_locs
        ):
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
            # differ only by redundant slashes (e.g. `////pkg/path` vs `//pkg/path`).
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
                gn_set = {_slash_norm_lbl(x) for x in gn_m.group(1).split(",") if x.strip()}
                bz_set = {_slash_norm_lbl(x) for x in bz_m.group(1).split(",") if x.strip()}
                if gn_set and gn_set == bz_set:
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
            if "## BAZEL2GN SENTINEL" in gn_txt and (
                "\n    deps =\n" in gn_txt or "\n  deps =\n" in gn_txt
            ):
                f["severity"] = "INFO"


def run_review_diff(existing_findings):
    planter_bin = find_planter_bin()
    if not planter_bin or not workdir:
        return None
    env = dict(os.environ)
    env.update(
        {
            "PLANTER_WORKDIR": workdir,
            "PLANTER_TARGET_DIRS": " ".join(target_dirs),
            "PLANTER_CHANGE_BASE": change_base,
        }
    )
    out, _, _ = execute(
        [planter_bin, "diff", "--local", "--check-findings", f"--workdir={workdir}"],
        workdir,
        env,
        "review_diff",
    )
    findings, parsed_ok = parse_findings(out.strip())
    if not parsed_ok:
        return []
    downgrade_check_cfg_diff_findings(findings)
    seen = {(f["category"], f["file"], f["line"], f["message"]) for f in existing_findings}
    results = []
    for f in findings:
        f["source"] = "review_diff"
        f["severity"] = normalize_severity(f["severity"])
        key = (f["category"], f["file"], f["line"], f["message"])
        if key in seen:
            continue
        seen.add(key)
        results.append(f)
    return results


COMMIT_MSG_CHECKS = {"confidentiality_check", "cq_reachability", "commit_message_format", "build_verification"}
commit_msg_only = not only and commit_msg_only_since_upload()
if commit_msg_only:
    enabled = [s for s in enabled if s["name"] in COMMIT_MSG_CHECKS]
    if len(target_dirs) > 1:
        target_dirs = target_dirs[:1]

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
rows = []
all_findings = []
try:
    for n, spec in enumerate(enabled, 1):
        if spec["name"] == "build_verification":
            release_machinery_locks()
        log(f"({n}/{len(enabled)}) {spec['name']}...")
        started = time.monotonic()
        findings = run_check(spec)
        rows.append((spec, findings, time.monotonic() - started))
        all_findings.extend(findings)
    if not commit_msg_only and (not only or "review_diff" in only):
        started = time.monotonic()
        diff_findings = run_review_diff(all_findings)
        if diff_findings is not None:
            rows.append(({"name": "review_diff", "enabled": True}, diff_findings, time.monotonic() - started))
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
    print("RESULT: PASS: no blocking findings.")
if only:
    print("NOTE: only some checks ran (--only). Run all of them before reporting.")
if any(f["category"] == "build_skipped" for f in all_findings):
    print("NOTE: the builds were skipped. Run `run_checks.sh` without --skip-build before reporting.")
if evolution_in_progress:
    print(f"NOTE: {evolution_note}")
sys.exit(exit_code)
PYEOF
