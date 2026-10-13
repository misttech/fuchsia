#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -uo pipefail

# Deterministic check that actually builds the migration. Static checks and the
# review panel only read BUILD files; this is what proves the change compiles.
#
# Steps (each failing step becomes one ERROR finding with the relevant log lines):
#   1. gn_build:               `fx build` (incremental, the whole configured
#                              product), so the migrated GN targets and the GN
#                              dependents in the product graph are rebuilt.
#   2. gn_build_dependents:    `fx build <labels>` for the GN targets the product
#                              graph may leave out: the changed packages'
#                              `:tests` groups and test/binary targets, then the
#                              GN targets of other packages that depend on a
#                              changed package (tests and binaries first, then
#                              libraries), limited to labels of the GN graph
#                              (<build dir>/ninja_outputs.json). Catches GN-only
#                              link failures (e.g. undefined `operator new` in a
#                              GN test after a dependency label or edge changed)
#                              that Bazel builds and the default `fx build` miss.
#                              Skipped when gn_build fails.
#   3. verify_bazel2gn:        `fx host-tool shac check --only verify_bazel2gn
#                              <changed dirs' BUILD.bazel/BUILD.gn>`, which fails
#                              if a BUILD.gn has drifted from what `fx bazel2gn`
#                              generates from BUILD.bazel.
#   4. bazel_build_fuchsia:    `fx bazel build --config=fuchsia_platform
#                              --noexpand_test_suites //<dir>:all`
#                              for every changed directory with a BUILD.bazel
#                              (host-only targets are skipped as incompatible,
#                              and `--noexpand_test_suites` prevents `:all` from
#                              expanding `test_suite` targets before checking
#                              their `target_compatible_with`;
#                              `_validate_json_action` targets such as
#                              `<fidl>_validate_ir_json` are excluded from top-level
#                              `:all` so `assert_no_deps_aspect` does not crash on
#                              their scalar `data` attribute, while `:<fidl>` still
#                              runs IR JSON validation via `_validation`).
#   5. bazel_build_host:       `fx bazel build --config=host //<dir>:all` for the
#                              directories whose BUILD.bazel declares host targets.
#
# Directories: $PLANTER_TARGET_DIRS plus every directory whose BUILD.bazel or
# BUILD.gn changed since $PLANTER_CHANGE_BASE (e.g. migrated dependencies).
# With several target directories planter runs checks once per directory; this
# check only builds on the run for the first one.
#
# Result reuse: a run whose builds all pass records a fingerprint of exactly
# what it built in <git dir>/planter-build-verification.json. A later run with
# the same fingerprint reuses that result instead of rebuilding (reported as an
# INFO finding), so planter does not repeat the coder's final `run_checks.sh`.
# The fingerprint covers this script, the git tree of the whole working tree
# (committed or not, including untracked files), the build directories and
# steps, args.gn, the last jiri update and the environment below. Failures are
# never reused, and a result expires after PLANTER_BUILD_REUSE_TTL seconds.
#
# Environment:
#   PLANTER_SKIP_BUILD=1          skip the builds (still validates commit Test:
#                                 footers and reports an INFO finding).
#   PLANTER_BUILD_NO_REUSE=1      always build; do not reuse a recorded result.
#   PLANTER_BUILD_REUSE_TTL=<secs>  max age of a reused result (default 21600).
#   PLANTER_BUILD_DRY_RUN=1       run nothing; report the planned commands as
#                                 INFO findings.
#   PLANTER_BUILD_TIMEOUT=<secs>  per-step timeout (default 5400).
#   PLANTER_DEPENDENT_BUILD_LIMIT=<n>  max labels for gn_build_dependents
#                                 (default 40).

WORKDIR="${PLANTER_WORKDIR:-.}"
TARGET_DIR="${PLANTER_TARGET_DIR:-}"
TARGET_DIRS="${PLANTER_TARGET_DIRS:-$TARGET_DIR}"

first_dir="${TARGET_DIRS%% *}"
if [[ -n "$TARGET_DIR" && -n "$first_dir" && "$TARGET_DIR" != "$first_dir" ]]; then
  echo "[]"
  exit 0
fi

python3 - "$WORKDIR" "$TARGET_DIRS" "${BASH_SOURCE[0]}" <<'PYEOF'
import ast
import hashlib
import json
import os
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import time

workdir = os.path.abspath(sys.argv[1])
target_dirs = [d.strip().strip("/") for d in sys.argv[2].split() if d.strip().strip("/")]
change_base = os.environ.get("PLANTER_CHANGE_BASE", "").strip() or "HEAD"
timeout = int(os.environ.get("PLANTER_BUILD_TIMEOUT", "5400"))
dry_run = os.environ.get("PLANTER_BUILD_DRY_RUN", "").strip() not in ("", "0")
skip_build = os.environ.get("PLANTER_SKIP_BUILD", "").strip() not in ("", "0")
dependent_limit = int(os.environ.get("PLANTER_DEPENDENT_BUILD_LIMIT", "40"))


def git_lines(args):
    try:
        out = subprocess.check_output(["git", "-C", workdir] + args, stderr=subprocess.DEVNULL, text=True)
    except Exception:
        return []
    return [l.strip() for l in out.splitlines() if l.strip()]


EXCLUDED_GLOBAL_DIRS = (
    "bundles/assembly",
    "build/bazel",
    "build/bazel2gn",
    "build/images",
    "build/config/rust/lints",
)

dirs = list(target_dirs)
changed = git_lines(["diff", "--name-only", change_base]) + git_lines(["ls-files", "--others", "--exclude-standard"])
changed_set = set(changed)
for p in changed:
    if os.path.basename(p) in ("BUILD.bazel", "BUILD.gn"):
        d = os.path.dirname(p).strip("/")
        if (
            d
            and d not in ("tools", "src", "sdk")
            and not any(d == ex or d.startswith(ex + "/") for ex in EXCLUDED_GLOBAL_DIRS)
            and not (any(d == os.path.dirname(td) for td in target_dirs) and f"{d}/BUILD.bazel" not in changed_set)
            and d not in dirs
        ):
            dirs.append(d)

fx = None
for rel in (".jiri_root/bin/fx", "scripts/fx"):
    cand = os.path.join(workdir, rel)
    if os.path.isfile(cand):
        fx = cand
        break
if fx is None:
    print(json.dumps([{
        "source": "build_verification",
        "category": "build_setup",
        "severity": "ERROR",
        "message": f"Cannot find fx in {workdir} (.jiri_root/bin/fx or scripts/fx); the migration could not be built.",
    }], indent=2))
    sys.exit(0)

bazel_dirs = [d for d in dirs if os.path.isfile(os.path.join(workdir, d, "BUILD.bazel"))]
host_markers = re.compile(
    r"HOST_CONSTRAINTS|HOST_OS_CONSTRAINTS|_host_tool\b|with_host_unit_tests|"
    r"\bwith_unit_tests\s*=\s*\"(?:host|both)\"|"
    r"\bhost_(?:go|rustc|py)_test\b|\bhost_test\(|\bwrap_host_rust_test\("
)
host_dirs = []
for d in bazel_dirs:
    try:
        with open(os.path.join(workdir, d, "BUILD.bazel")) as f:
            if host_markers.search(f.read()):
                host_dirs.append(d)
    except OSError:
        pass

# Bazel tests exported to tests.json by GN `bazel_test_suite()` targets in the changed directories:
# {"host_tests": [labels], "target_tests": [labels]}. Host tests are run with `fx bazel test`;
# device tests (`fx_test`) cannot run under `bazel test` and are only built (bazel_build_fuchsia).
exported_tests = {"host_tests": [], "target_tests": []}
for d in dirs:
    try:
        with open(os.path.join(workdir, d, "BUILD.gn"), encoding="utf-8", errors="replace") as f:
            gn_text = re.sub(r"#[^\n]*", "", f.read())
    except OSError:
        continue
    for sm in re.finditer(r'\bbazel_test_suite\(\s*"[^"]+"\s*\)\s*\{', gn_text):
        depth, j = 0, sm.end() - 1
        while j < len(gn_text):
            depth += {"{": 1, "}": -1}.get(gn_text[j], 0)
            if depth == 0:
                break
            j += 1
        for lm in re.finditer(r"\b(host_tests|target_tests)\s*\+?=\s*\[([^\]]*)\]", gn_text[sm.end() : j]):
            for label in re.findall(r'"(//[^"]+)"', lm.group(2)):
                if label not in exported_tests[lm.group(1)]:
                    exported_tests[lm.group(1)].append(label)

LINKED_TEMPLATES = {"executable", "test", "rustc_binary", "rustc_test", "loadable_module", "shared_library", "rustc_cdylib", "fuchsia_driver"}
LIBRARY_TEMPLATES = {"rustc_library", "rustc_macro", "rustc_staticlib", "static_library", "source_set"}
GN_TARGET = re.compile(r'\b([A-Za-z_]\w*)\(\s*"([^"$]+)"\s*\)\s*\{')
GN_DEP_LIST = re.compile(r"(?<![\w.])(?:public_deps|deps|non_rust_deps|link_deps|test_deps|data_deps)\s*\+?=\s*\[([^\]]*)\]")
GN_DEP_VAR = re.compile(r"(?<![\w.])(?:public_deps|deps|non_rust_deps|link_deps|test_deps|data_deps)\s*\+?=\s*([A-Za-z_][^\n]*)")
LIST_VAR = re.compile(r"(?<![\w.])([A-Za-z_]\w*)\s*\+?=\s*\[([^\]]*)\]")
UNIT_TESTS = re.compile(r"(?<![\w.])with_unit_tests\s*=\s*true")


def read(rel):
    try:
        with open(os.path.join(workdir, rel), encoding="utf-8", errors="replace") as f:
            return f.read()
    except OSError:
        return None


def gn_code(text):
    """Blanks out GN comments without moving offsets, so strings and brackets can be scanned."""
    out, i, n, in_str = [], 0, len(text), False
    while i < n:
        c = text[i]
        if in_str:
            if c == "\\":
                out.append(text[i : i + 2])
                i += 2
                continue
            in_str = c != '"'
        elif c == '"':
            in_str = True
        elif c == "#":
            j = text.find("\n", i)
            j = n if j < 0 else j
            out.append(" " * (j - i))
            i = j
            continue
        out.append(c)
        i += 1
    return "".join(out)


def closing(code, i):
    """Index of the `}` closing the `{` at code[i]."""
    depth, in_str = 0, False
    while i < len(code):
        c = code[i]
        if in_str:
            if c == "\\":
                i += 2
                continue
            in_str = c != '"'
        elif c == '"':
            in_str = True
        elif c == "{":
            depth += 1
        elif c == "}":
            depth -= 1
            if depth == 0:
                return i
        i += 1
    return len(code)


def gn_blocks(text):
    """{name: [template, body]} for every `template("name") { ... }` block of a BUILD.gn."""
    code = gn_code(text)
    blocks = {}
    for m in GN_TARGET.finditer(code):
        if m.group(1) in ("template", "declare_args", "foreach", "forward_variables_from"):
            continue
        brace = m.end() - 1
        blocks.setdefault(m.group(2), [m.group(1), ""])[1] += code[brace + 1 : closing(code, brace)]
    return blocks


def label_dir(raw, pkg):
    """Package directory of a GN label written in package pkg; None for labels built from variables."""
    raw = raw.strip()
    if not raw or "$" in raw or raw.startswith("@"):
        return None
    path = re.sub(r"\(.*\)$", "", raw).split(":", 1)[0]
    if path.startswith("//"):
        return path[2:].strip("/")
    return os.path.normpath(os.path.join(pkg, path)).strip("/") if path else pkg


def rdep_build_files():
    """BUILD.gn files outside the changed directories that mention one of them."""
    patterns = [a for d in dirs for a in ("-e", f"//{d}")]
    rg = shutil.which("rg", path=os.environ.get("PATH", "") + os.pathsep + os.path.expanduser("~/.cargo/bin"))
    cmd = (
        [rg, "-l", "--fixed-strings", "-g", "BUILD.gn"] + patterns + ["."]
        if rg
        else ["git", "grep", "-l", "--fixed-strings"] + patterns + ["--", "BUILD.gn", "*/BUILD.gn"]
    )
    try:
        out = subprocess.run(cmd, cwd=workdir, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, timeout=600).stdout
    except (OSError, subprocess.SubprocessError):
        return []
    files = {os.path.normpath(l.strip()) for l in out.splitlines() if l.strip()}
    return sorted(f for f in files if os.path.dirname(f) not in dirs and not f.startswith("out/"))


def load_toolchains():
    try:
        with open(os.path.join(workdir, ".fx-build-dir")) as f:
            build_dir = f.read().strip()
        with open(os.path.join(workdir, build_dir, "ninja_outputs.json")) as f:
            known = json.load(f)
    except (OSError, ValueError) as e:
        return None, str(e)
    toolchains = {}
    for key, outs in sorted(known.items()):
        if isinstance(outs, list) and any(isinstance(o, str) and o.strip() for o in outs):
            toolchains.setdefault(key.split("(", 1)[0], []).append(key)
    return toolchains, None


def dependent_labels():
    """(labels, notes) for gn_build_dependents: the changed packages' `:tests` groups and test/binary/library targets,
    then GN targets of other packages that depend on a changed package (tests and binaries, `<name>_test` of
    `with_unit_tests` Rust targets, then libraries), as keys of <build dir>/ninja_outputs.json."""
    toolchains, err = load_toolchains()
    if toolchains is None:
        return [], [f"Cannot read the GN target list (<build dir>/ninja_outputs.json): {err}. GN dependents were not built."]
    own, linked, unit_tests, libraries = [], [], [], []
    unconfigured_own_tests = []
    for d in dirs:
        blocks = gn_blocks(read(f"{d}/BUILD.gn") or "")
        if "tests" in blocks:
            t_lbl = f"//{d}:tests"
            own.append(t_lbl)
            if t_lbl not in toolchains:
                unconfigured_own_tests.append(t_lbl)
        for name, (tmpl, body) in sorted(blocks.items()):
            if tmpl in LINKED_TEMPLATES or tmpl in LIBRARY_TEMPLATES:
                own.append(f"//{d}:{name}")
            if tmpl.startswith("rustc_") and UNIT_TESTS.search(body):
                own.append(f"//{d}:{name}_test")
    for rel in rdep_build_files():
        pkg = os.path.dirname(rel)
        text = read(rel) or ""

        def names_changed(items):
            return bool({label_dir(s, pkg) for s in re.findall(r'"([^"]*)"', items)} & set(dirs))

        # List variables (e.g. `_core_deps = [ ... ]`) naming a changed package, used as `deps = _core_deps`.
        dep_vars = {
            m.group(1)
            for m in LIST_VAR.finditer(gn_code(text))
            if m.group(1) not in ("public_deps", "deps", "non_rust_deps", "link_deps", "test_deps", "data_deps", "visibility")
            and names_changed(m.group(2))
        }
        for name, (tmpl, body) in sorted(gn_blocks(text).items()):
            if not any(names_changed(m.group(1)) for m in GN_DEP_LIST.finditer(body)) and not any(
                re.search(r"(?<![\w.])(%s)\b" % "|".join(map(re.escape, dep_vars)), m.group(1))
                for m in GN_DEP_VAR.finditer(body)
                if dep_vars
            ):
                continue
            if tmpl in LINKED_TEMPLATES:
                linked.append(f"//{pkg}:{name}")
            if tmpl.startswith("rustc_") and UNIT_TESTS.search(body):
                unit_tests.append(f"//{pkg}:{name}_test")
            if tmpl in LIBRARY_TEMPLATES:
                libraries.append(f"//{pkg}:{name}")
    labels = []
    for label in dict.fromkeys(own + linked + unit_tests + libraries):
        keys = toolchains.get(label, [])
        key = label if label in keys else next((k for k in keys if "host" in k), keys[0] if keys else None)
        if key and key not in labels:
            labels.append(key)
    notes = []
    if unconfigured_own_tests:
        notes.append(
            f"Not in the configured GN graph (<build dir>/ninja_outputs.json): {' '.join(unconfigured_own_tests)}. "
            f"Do NOT run `fx build {' '.join(unconfigured_own_tests)}` or include unconfigured GN labels in "
            "`CoderReport.tests_run` or `Test:` commit footers (Planter's test_verifier will fail with `Unknown GN label`)."
        )
    if len(labels) > dependent_limit:
        notes.append(
            f"gn_build_dependents builds only the first {dependent_limit} of {len(labels)} labels "
            f"(PLANTER_DEPENDENT_BUILD_LIMIT); not built: {' '.join(labels[dependent_limit:])}"
        )
        labels = labels[:dependent_limit]
    return labels, notes


def bazel_tests_export_notes():
    """INFO notes on Bazel tests listed by the changed directories' bazel_test_suite targets but
    missing from <build dir>/tests.json. Whether a suite reaches tests.json also depends on the
    configured test roots (product, --with-test), so a missing test is a hint, not a failure."""
    build_dir = (read(".fx-build-dir") or "").strip()
    try:
        with open(os.path.join(workdir, build_dir, "tests.json")) as f:
            entries = json.load(f)
    except (OSError, ValueError) as e:
        return [f"Cannot read <build dir>/tests.json ({e}); the export of Bazel tests was not verified."]

    def norm(label):
        return re.sub(r"^@@?//", "//", label or "")

    bazel_entries = set()
    for e in entries if isinstance(entries, list) else []:
        t = e.get("test", {}) if isinstance(e, dict) else {}
        label = norm(t.get("source_label") or t.get("label"))
        if label and "(" not in label:
            bazel_entries.add(label)
    missing = []
    for label in exported_tests["target_tests"] + exported_tests["host_tests"]:
        pkg = label.split(":", 1)[0]
        if label not in bazel_entries and not any(b.startswith(pkg + ":") for b in bazel_entries):
            missing.append(label)
    if not missing:
        return []
    return [
        "Not in <build dir>/tests.json after `fx build`: " + " ".join(missing) + ". Either their "
        "`bazel_test_suite` is not reachable from the configured test roots (check with "
        "`fx set ... --with-test //<dir>:tests`), or it is not wired into `group(\"tests\")`. "
        "Do not record `fx test` commands for these tests in `tests_run` unless they ran."
    ]


FIDL_MACROS = {"fidl_library", "_fidl_library", "fidl_ir"}
VALIDATE_JSON_MACROS = {"validate_json", "validate_json5", "_validate_json_action"}
FDOMAIN_UNSUPPORTED_HANDLE_RE = re.compile(
    r"\bzx\.Handle\s*:\s*(?:LOG|DEBUGLOG|INTERRUPT|MSI|IOMMU|BTI|PMT|PAGER|PCI_DEVICE|PROFILE|SUSPEND_TOKEN|VCPU)\b"
)


def _dir_has_fdomain_unsupported_handles(d):
    abs_d = os.path.join(workdir, d)
    try:
        names = os.listdir(abs_d)
    except OSError:
        return False
    for fn in sorted(names):
        if fn.endswith(".fidl"):
            content = read(f"{d}/{fn}") or ""
            if FDOMAIN_UNSUPPORTED_HANDLE_RE.search(content):
                return True
    return False


def validate_json_exclusions(d):
    """Returns `-//<d>:<target>` exclusions for `_validate_json_action` targets in `d/BUILD.bazel`
    (and `<name>_rust_fdomain{,_flex}` when `.fidl` sources use `zx.Handle` subtypes missing from
    `fdomain_client`). `//build/bazel/aspects:assert_no_deps.bzl` iterates
    `getattr(ctx.rule.attr, "data", [])`, which crashes with `Error: type 'Target' is not iterable`
    when `:all` applies `assert_no_deps_aspect` at the top level to `_validate_json_action`
    (`data = attr.label(...)`). Excluding `<name>_validate_ir_json` from top-level `:all` still runs
    IR JSON validation via `:<name>`'s `_validation` output group."""
    text = read(f"{d}/BUILD.bazel")
    if not text:
        return []
    out = []
    seen = set()
    skip_fdomain = _dir_has_fdomain_unsupported_handles(d)

    def _add(target):
        lbl = f"-//{d}:{target}"
        if lbl not in seen:
            seen.add(lbl)
            out.append(lbl)

    def _add_fidl(tname, fn):
        _add(f"{tname}_validate_ir_json" if fn in FIDL_MACROS else tname)
        if fn in {"fidl_library", "_fidl_library"} and skip_fdomain:
            _add(f"{tname}_rust_fdomain")
            _add(f"{tname}_rust_fdomain_flex")

    try:
        tree = ast.parse(text)
        for node in ast.walk(tree):
            if not isinstance(node, ast.Call):
                continue
            fn = node.func.id if isinstance(node.func, ast.Name) else (
                node.func.attr if isinstance(node.func, ast.Attribute) else None
            )
            if fn not in FIDL_MACROS and fn not in VALIDATE_JSON_MACROS:
                continue
            for kw in node.keywords:
                if kw.arg == "name" and isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str):
                    _add_fidl(kw.value.value, fn)
    except SyntaxError:
        for m in re.finditer(
            r"\b(fidl_library|_fidl_library|fidl_ir|validate_json5?|_validate_json_action)\s*\(([^)]*)\)",
            gn_code(text),
            re.S,
        ):
            fn, args_body = m.group(1), m.group(2)
            nm = re.search(r'\bname\s*=\s*"([^"]+)"', args_body)
            if nm:
                _add_fidl(nm.group(1), fn)
    return out


def has_non_suite_bazel_targets(d):
    """True when `d/BUILD.bazel` defines at least one rule/macro target other than `test_suite`,
    `package`, `exports_files`, or `licenses` (so `bazel_build_fuchsia` does not build
    `test_suite`-only ancestor aggregator packages under `--config=fuchsia_platform`)."""
    text = read(f"{d}/BUILD.bazel")
    if not text:
        return False
    non_build_calls = {"package", "exports_files", "licenses", "load", "test_suite"}
    try:
        tree = ast.parse(text)
        for node in tree.body:
            if isinstance(node, ast.Expr) and isinstance(node.value, ast.Call):
                fn = node.value.func.id if isinstance(node.value.func, ast.Name) else (
                    node.value.func.attr if isinstance(node.value.func, ast.Attribute) else None
                )
                if fn and fn not in non_build_calls:
                    return True
        return False
    except SyntaxError:
        calls = set(re.findall(r"^\s*([A-Za-z_]\w*)\s*\(", gn_code(text), re.M))
        return bool(calls - non_build_calls)


def bazel_target_args(pkg_dirs):
    pos = [f"//{d}:all" for d in pkg_dirs]
    neg = [lbl for d in pkg_dirs for lbl in validate_json_exclusions(d)]
    return (["--"] + pos + neg) if neg else pos


def fmt_cmd(cmd):
    return shlex.join(["fx"] + cmd[1:])


steps = [
    ("gn_build", [fx, "build"]),
    ("gn_build_dependents", None),  # Labels are computed once gn_build has regenerated the GN graph.
]
sync_files = [
    os.path.join(d, f) for d in dirs for f in ("BUILD.bazel", "BUILD.gn") if os.path.isfile(os.path.join(workdir, d, f))
]
if sync_files:
    steps.append(("verify_bazel2gn", [fx, "host-tool", "shac", "check", "--only", "verify_bazel2gn"] + sync_files))
fuchsia_bazel_dirs = [d for d in bazel_dirs if has_non_suite_bazel_targets(d)]
if fuchsia_bazel_dirs:
    steps.append((
        "bazel_build_fuchsia",
        [fx, "bazel", "build", "--config=fuchsia_platform", "--noexpand_test_suites"]
        + bazel_target_args(fuchsia_bazel_dirs),
    ))
if host_dirs:
    steps.append(("bazel_build_host", [fx, "bazel", "build", "--config=host"] + bazel_target_args(host_dirs)))
if exported_tests["host_tests"]:
    steps.append(("bazel_test_host", [fx, "bazel", "test", "--config=host"] + exported_tests["host_tests"]))
if exported_tests["host_tests"] or exported_tests["target_tests"]:
    steps.append(("bazel_tests_exported", None))  # Checks <build dir>/tests.json after gn_build.

ERROR_LINE = re.compile(
    r"(^ERROR:|^FAILED:|\berror(\[E\d+\])?:|^error\b|: error\b|\bundefined reference\b|"
    r"no such (target|package)|is not visible|Unable to load|not declared|drift|differs|--- a/|\+\+\+ b/)",
    re.I,
)
LOCATION = re.compile(r"(?:-->\s*)?((?:\.\./\.\./)?[\w./+-]+\.(?:rs|cc|h|c|gn|gni|bazel|bzl)):(\d+)")
BUILD_FILE_LOCATION = re.compile(r"((?:\.\./\.\./)?[\w./+-]*(?:BUILD\.gn|BUILD\.bazel|\.gni|\.bzl)):(\d+)")


def summarize(output, location=LOCATION):
    lines = output.splitlines()
    picked = []
    for i, line in enumerate(lines):
        if ERROR_LINE.search(line):
            for j in range(max(0, i - 1), min(len(lines), i + 4)):
                if not picked or picked[-1] < j:
                    picked.append(j)
    text = "\n".join(lines[j] for j in picked) if picked else "\n".join(lines[-40:])
    if len(text) > 6000:
        text = text[:6000] + "\n... (truncated)"
    file, line = "", 0
    m = location.search(text)
    if m:
        file = m.group(1).replace("../../", "", 1)
        if file.startswith(workdir + "/"):
            file = file[len(workdir) + 1:]
        line = int(m.group(2))
    return text, file, line


REMEDIATION = {
    "gn_build": "Fix the BUILD.bazel/BUILD.gn so the GN build passes (renamed/removed labels that dependents still use, "
                "missing deps, attribute drift), then re-run `fx bazel2gn -d <dir>` and `fx build`. Never edit sources to make it pass.",
    "gn_build_dependents": "A GN target that the default `fx build` does not build failed: a test or binary of a changed "
                           "package, or a GN dependent in another package. Undefined symbols at link time (e.g. `operator "
                           "new`/`operator delete` from C++ code linked into a Rust test) or missing configs mean the "
                           "migration changed a GN dependency label or edge kind: a GN-only wrapper (e.g. a `*_static` "
                           "target carrying link settings) replaced by the target it wraps, or a `public_deps` edge turned "
                           "into `deps`. Restore the original labels and edge kinds in the migrated packages (see the "
                           "`gn_dep_parity` findings and \"Keep Every GN Dependency Label and Edge Kind\" in the coder "
                           "instructions), re-run `fx bazel2gn -d <dir>` and `fx build <failing label>`. Never edit sources "
                           "or the dependents' BUILD files to make it pass.",
    "verify_bazel2gn": "BUILD.gn no longer matches BUILD.bazel. Run `fx bazel2gn -d <dir>` for every dual-build directory "
                       "you touched (never hand-edit below the BAZEL2GN SENTINEL) and re-run this check.",
    "bazel_build_fuchsia": "Fix BUILD.bazel so the `bazel_build_fuchsia` command passes. If a dependency has no "
                           "Bazel target yet, migrate that dependency's directory in this change (see the coder instructions).",
    "bazel_build_host": "Fix BUILD.bazel so the `bazel_build_host` command passes. If a dependency has no Bazel target "
                        "yet, migrate that dependency's directory in this change (see the coder instructions).",
    "bazel_test_host": "A Bazel host test exported by a GN `bazel_test_suite(host_tests = ...)` fails, so it would fail "
                       "on infra too. Fix its BUILD.bazel attributes (deps, data/test_data, test_args, "
                       "lint/rustc flags) to match what GN ran on host. If GN never ran it on host, do not export "
                       "it as a host test (a device-only test, C++ or Rust, becomes an `fx_test` package; for Rust "
                       "unit tests use `with_unit_tests = \"fuchsia\"`).",
}

findings = []
head_sha = (git_lines(["rev-parse", "HEAD"]) or [""])[0]
uploaded_commit_env = os.environ.get("PLANTER_UPLOADED_COMMIT", "").strip()
worktree_dirty_vs_head = bool(
    git_lines(["diff", "--name-only", "HEAD", "--"])
    or [u for u in git_lines(["ls-files", "--others", "--exclude-standard"]) if any(u == d or u.startswith(d + "/") for d in dirs)]
)
if change_base != "HEAD" and not worktree_dirty_vs_head and (not uploaded_commit_env or head_sha != uploaded_commit_env):
    toolchains_map, _ = load_toolchains()
    tests_json_targets = None
    build_dir_rel = (read(".fx-build-dir") or "").strip()
    if build_dir_rel:
        try:
            with open(os.path.join(workdir, build_dir_rel, "tests.json")) as f:
                tj_data = json.load(f)
            if isinstance(tj_data, list):
                tests_json_targets = set()
                for entry in tj_data:
                    t_obj = entry.get("test", {}) if isinstance(entry, dict) else {}
                    for k in ("label", "source_label", "name", "package_url"):
                        val = t_obj.get(k)
                        if isinstance(val, str) and val.strip():
                            tests_json_targets.add(re.sub(r"^@@?//", "//", val.strip()).split("(", 1)[0])
        except (OSError, ValueError):
            tests_json_targets = None
    commit_msg = "\n".join(git_lines(["log", "-1", "--format=%B", "HEAD"]))
    test_footer_lines = []
    for m_line in commit_msg.splitlines():
        tm = re.match(r"^\s*Test\s*:\s*(.+)$", m_line, re.I)
        if not tm:
            continue
        test_footer_lines.append(m_line.strip())
        test_cmd = tm.group(1).strip()
        if re.match(r"^(?:fx|ffx|bazel)\b", test_cmd):
            syntax_proc = subprocess.run(
                ["bash", "-n", "-c", test_cmd],
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
            )
            if syntax_proc.returncode != 0:
                err_detail = " ".join(syntax_proc.stdout.strip().splitlines()) or f"exit status {syntax_proc.returncode}"
                findings.append({
                    "source": "build_verification",
                    "category": "malformed_test_footer_command",
                    "severity": "ERROR",
                    "file": "",
                    "line": 0,
                    "message": (
                        f"Commit `Test:` footer `{m_line.strip()}` has a bash syntax error ({err_detail}). "
                        "Planter's test_verifier runs `Test:` commands via `bash -c`, so unquoted parentheses "
                        "in GN toolchain labels like `//pkg:target(//build/toolchain:...)` fail."
                    ),
                    "remediation": (
                        "Quote any GN label containing toolchain parentheses in the commit `Test:` footer and "
                        "`CoderReport.tests_run` (e.g. `fx build '//pkg:target(//build/toolchain:...)'`), or copy "
                        "the exact quoted command from `PLANTER_BUILD_DRY_RUN=1 run_checks.sh --only build_verification`."
                    ),
                })
        if "fx bazel build" in m_line:
            missing_excls = []
            for d_all in re.findall(r"//([^\s:;\"'`]+):all\b", m_line):
                for excl in validate_json_exclusions(d_all.strip("/")):
                    if excl not in m_line and excl not in missing_excls:
                        missing_excls.append(excl)
            if missing_excls:
                findings.append({
                    "source": "build_verification",
                    "category": "unexcluded_validate_json_in_test_footer",
                    "severity": "ERROR",
                    "file": "",
                    "line": 0,
                    "message": (
                        f"Commit `Test:` footer `{m_line.strip()}` runs `fx bazel build` with `:all` on a package "
                        f"defining `fidl_library`/`validate_json` without excluding `{' '.join(missing_excls)}`. "
                        "`assert_no_deps_aspect` crashes with `Error: type 'Target' is not iterable` when `:all` "
                        "applies it at the top level to `_validate_json_action`."
                    ),
                    "remediation": (
                        f"Pass `-- ... {' '.join(missing_excls)}` in the commit `Test:` footer and "
                        "`CoderReport.tests_run` (copy the exact command from "
                        "`PLANTER_BUILD_DRY_RUN=1 run_checks.sh --only build_verification`)."
                    ),
                })
        if toolchains_map is not None and "fx build" in m_line:
            bad_lbls = []
            for lbl in re.findall(r"//[^\s,;\"'`]+", m_line):
                base_lbl = lbl.split("(", 1)[0]
                if base_lbl not in toolchains_map and lbl not in toolchains_map.get(base_lbl, []):
                    bad_lbls.append(lbl)
            if bad_lbls:
                findings.append({
                    "source": "build_verification",
                    "category": "unconfigured_test_footer_label",
                    "severity": "ERROR",
                    "file": "",
                    "line": 0,
                    "message": (
                        f"Commit `Test:` footer `{m_line.strip()}` passes GN label(s) not in the configured GN graph "
                        f"(<build dir>/ninja_outputs.json): {' '.join(bad_lbls)}. `fx build` fails with `Unknown GN label` "
                        "on labels outside the configured product graph."
                    ),
                    "remediation": (
                        "Amend the commit message `Test:` footer (and only list commands in `CoderReport.tests_run`) to use "
                        "commands that actually succeed in the configured build (copy the exact shell-quoted commands from "
                        "`PLANTER_BUILD_DRY_RUN=1 run_checks.sh --only build_verification`)."
                    ),
                })
        if tests_json_targets is not None and re.search(r"\bfx\s+(?:-t\s+\S+\s+)?test\b", test_cmd):
            fx_test_lbls = [
                re.sub(r"^@@?//", "//", lbl).split("(", 1)[0]
                for lbl in re.findall(r"@?//[^\s,;\"'`]+", test_cmd)
            ]
            missing_fx_tests = []
            if not tests_json_targets:
                missing_fx_tests = fx_test_lbls or [test_cmd]
            else:
                for lbl in fx_test_lbls:
                    pkg_prefix = lbl.split(":", 1)[0]
                    if lbl not in tests_json_targets and not any(
                        t == lbl or t.startswith(pkg_prefix + ":") for t in tests_json_targets
                    ):
                        missing_fx_tests.append(lbl)
            if missing_fx_tests:
                findings.append({
                    "source": "build_verification",
                    "category": "unconfigured_fx_test_footer_label",
                    "severity": "ERROR",
                    "file": "",
                    "line": 0,
                    "message": (
                        f"Commit `Test:` footer `{m_line.strip()}` runs `fx test` on target(s) not present in "
                        f"`<build dir>/tests.json`: {' '.join(missing_fx_tests)} "
                        f"({len(tests_json_targets)} total tests in `<build dir>/tests.json`). "
                        "Planter's `test_verifier` runs `fx test --dry` and fails when a test target is not in `tests.json`."
                    ),
                    "remediation": (
                        "Replace `Test: fx test ...` in the commit message (and `CoderReport.tests_run`) with a Bazel "
                        "command that runs or builds the migrated test directly, such as "
                        "`Test: fx bazel test --config=host @//<dir>:<test>` (or the ancestor `test_suite()`) for host "
                        "tests or `Test: fx bazel build --config=fuchsia_platform @//<dir>:<test>` for device tests."
                    ),
                })
    if len(test_footer_lines) > 3:
        findings.append({
            "source": "build_verification",
            "category": "too_many_test_footer_lines",
            "severity": "ERROR",
            "file": "COMMIT_MSG",
            "line": 0,
            "message": (
                f"Commit message has {len(test_footer_lines)} `Test:` footer lines (at most 3 allowed). "
                "Per-package incremental build or query lines clutter the commit message when more than 3 `Test:` lines are present."
            ),
            "remediation": (
                "Consolidate the commit message `Test:` footers (and `CoderReport.tests_run`) to at most 3 lines: "
                "one `fx bazel test|build --config=fuchsia_platform @//<dir>:<test>` line for the migrated test targets, "
                "one `fx bazel test --config=host @//<dir>:<host test>` line when host tests exist, and "
                "`fx host-tool shac check --only verify_bazel2gn` last; "
                "remove redundant per-package `fx build` / `fx bazel build` / `fx bazel query` lines."
            ),
        })

STEP_BLOCKING = ("ERROR", "WARNING")
uploaded_commit = os.environ.get("PLANTER_UPLOADED_COMMIT", "").strip()
if uploaded_commit and not dry_run and os.environ.get("PLANTER_BUILD_NO_REUSE", "").strip() in ("", "0"):
    try:
        up_tree = subprocess.check_output(
            ["git", "-C", workdir, "rev-parse", f"{uploaded_commit}^{{tree}}"],
            stderr=subprocess.DEVNULL, text=True,
        ).strip()
        head_tree = subprocess.check_output(
            ["git", "-C", workdir, "rev-parse", "HEAD^{tree}"],
            stderr=subprocess.DEVNULL, text=True,
        ).strip()
    except Exception:
        up_tree, head_tree = "", ""
    diff_worktree = git_lines(["diff", "--name-only", "HEAD", "--"])
    untracked_in_dirs = [
        u for u in git_lines(["ls-files", "--others", "--exclude-standard"])
        if any(u == d or u.startswith(d + "/") for d in dirs)
    ]
    if up_tree and up_tree == head_tree and not diff_worktree and not untracked_in_dirs:
        findings.append({
            "source": "build_verification",
            "category": "build_reused",
            "severity": "INFO",
            "message": f"No files changed since uploaded patchset ({uploaded_commit[:12]}); only commit message Test: footers were verified.",
        })
        print(json.dumps(findings, indent=2))
        sys.exit(0)

reuse_path = None
reuse_key = None
reused = None
reused_time = 0
if not dry_run and os.environ.get("PLANTER_BUILD_NO_REUSE", "").strip() in ("", "0"):
    def _git_bytes(args):
        return subprocess.run(["git", "-C", workdir] + args, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, check=True).stdout

    def _file_digest(path):
        h = hashlib.sha256()
        try:
            if os.path.islink(path):
                h.update(b"link:" + os.readlink(path).encode())
            else:
                with open(path, "rb") as f:
                    for chunk in iter(lambda: f.read(1 << 20), b""):
                        h.update(chunk)
        except OSError:
            h.update(b"missing")
        return h.hexdigest()

    def _worktree_tree():
        # The git tree of the whole working tree (tracked edits and untracked, non-ignored files),
        # so committing the change after a run does not change the fingerprint. Uses a copy of
        # the index so the real one is untouched and its stat cache keeps this fast.
        git_dir = _git_bytes(["rev-parse", "--absolute-git-dir"]).decode().strip()
        fd, index = tempfile.mkstemp(dir=git_dir, prefix=".planter-build-verification-index.")
        os.close(fd)
        try:
            try:
                shutil.copyfile(os.path.join(git_dir, "index"), index)
            except OSError:
                os.unlink(index)
            env = dict(os.environ, GIT_INDEX_FILE=index)
            subprocess.run(["git", "-C", workdir, "add", "-A", "."], env=env, stdout=subprocess.DEVNULL,
                           stderr=subprocess.DEVNULL, check=True)
            return subprocess.run(["git", "-C", workdir, "write-tree"], env=env, stdout=subprocess.PIPE,
                                  stderr=subprocess.DEVNULL, check=True).stdout
        finally:
            try:
                os.unlink(index)
            except OSError:
                pass

    def _fingerprint():
        h = hashlib.sha256()

        def _add(label, data):
            if isinstance(data, str):
                data = data.encode()
            h.update(label.encode() + b"\0" + str(len(data)).encode() + b"\0" + data)

        _add("tree", _worktree_tree())
        _add("dirs", json.dumps([sorted(dirs), sorted(bazel_dirs), sorted(host_dirs)]))
        _add("steps", json.dumps([[n, c] for n, c in steps]))
        _add("env", json.dumps([timeout, dependent_limit]))
        build_dir = (read(".fx-build-dir") or "").strip()
        _add("build_dir", build_dir)
        if build_dir:
            _add("args.gn", _file_digest(os.path.join(workdir, build_dir, "args.gn")))
        _add("jiri", _file_digest(os.path.join(workdir, ".jiri_root", "update_history", "latest")))
        return h.hexdigest()

    try:
        git_dir = _git_bytes(["rev-parse", "--absolute-git-dir"]).decode().strip()
        reuse_path = os.path.join(git_dir, "planter-build-verification.json")
        reuse_key = _fingerprint()
        ttl = int(os.environ.get("PLANTER_BUILD_REUSE_TTL", "21600"))
        with open(reuse_path) as f:
            rec = json.load(f)
        entry = None
        if isinstance(rec.get("entries"), dict):
            entry = rec["entries"].get(reuse_key)
        if entry is None and rec.get("key") == reuse_key:
            entry = rec
        if isinstance(entry, dict) and 0 <= time.time() - entry.get("time", 0) <= ttl:
            cached = entry.get("findings")
            if isinstance(cached, list) and not any(c.get("severity") in STEP_BLOCKING for c in cached):
                reused = cached
                reused_time = entry.get("time", 0)
    except (OSError, ValueError, subprocess.CalledProcessError):
        pass

if reused is not None:
    findings.extend(reused)
    findings.append({
        "source": "build_verification",
        "category": "build_reused",
        "severity": "INFO",
        "message": "Reused the passing build result recorded for this exact change "
                   f"({time.strftime('%Y-%m-%d %H:%M:%S', time.localtime(reused_time))}); "
                   "set PLANTER_BUILD_NO_REUSE=1 to rebuild.",
    })
    print(json.dumps(findings, indent=2))
    sys.exit(0)

if skip_build:
    findings.append({
        "source": "build_verification",
        "category": "build_skipped",
        "severity": "INFO",
        "message": "PLANTER_SKIP_BUILD is set; the migration was NOT built.",
    })
    print(json.dumps(findings, indent=2))
    sys.exit(0)

FROM_TARGET = re.compile(r"\(from target (?:@@?[\w.~+-]*)?(//[^\s)]+)\)")


def unrelated_bazel_failure(output):
    """Returns a note when `fx build` failed only in Bazel actions of targets that live in another git
    repository than the change and have no Bazel dependency path to any changed package (typically a
    checkout whose repositories are out of sync), else None. Any native Ninja failure, unparsable
    output, change to shared .bzl/MODULE.bazel files, or query error keeps the failure an ERROR."""
    targets = sorted(set(FROM_TARGET.findall(output)))
    if not targets or not bazel_dirs:
        return None
    if any(p.endswith((".bzl", "MODULE.bazel")) for p in changed):
        return None
    lines = output.splitlines()
    for i, l in enumerate(lines):
        if l.startswith("FAILED:") and not (i + 1 < len(lines) and "bazel_ninja_delayed_actions.py" in lines[i + 1]):
            return None
    main_top = "".join(git_lines(["rev-parse", "--show-toplevel"]))
    for t in targets:
        path = os.path.join(workdir, t[2:].split(":")[0])
        if not os.path.isdir(path):
            return None
        try:
            top = subprocess.check_output(["git", "-C", path, "rev-parse", "--show-toplevel"],
                                          stderr=subprocess.DEVNULL, text=True).strip()
        except Exception:
            return None
        if not top or top == main_top:
            return None
    expr = "somepath(set({}), set({}))".format(" ".join(targets), " ".join(f"//{d}:*" for d in bazel_dirs))
    try:
        proc = subprocess.run([fx, "bazel", "query", expr], cwd=workdir, stdout=subprocess.PIPE,
                              stderr=subprocess.DEVNULL, text=True, timeout=900)
    except subprocess.TimeoutExpired:
        return None
    if proc.returncode != 0 or proc.stdout.strip():
        return None
    return (f"the failing Bazel targets ({' '.join(targets)}) are in another git repository and have no "
            "dependency path to a changed package, so the failure predates the change (out-of-sync checkout). "
            "Do not edit them; the changed packages are still verified by the remaining steps.")


failed = set()
step_findings_start = len(findings)
for name, cmd in steps:
    if name == "gn_build_dependents":
        if "gn_build" in failed:
            continue
        labels, notes = dependent_labels()
        findings.extend(
            {"source": "build_verification", "category": "dependents_note", "severity": "INFO", "message": n} for n in notes
        )
        if not labels:
            continue
        cmd = [fx, "build"] + labels
    if name == "bazel_tests_exported":
        if dry_run:
            findings.append({
                "source": "build_verification",
                "category": "build_dry_run",
                "severity": "INFO",
                "message": "PLANTER_BUILD_DRY_RUN: bazel_tests_exported would check <build dir>/tests.json for "
                           + " ".join(exported_tests["target_tests"] + exported_tests["host_tests"]),
            })
            continue
        if "gn_build" in failed:
            continue
        findings.extend(
            {"source": "build_verification", "category": "bazel_tests_note", "severity": "INFO", "message": n}
            for n in bazel_tests_export_notes()
        )
        continue
    if dry_run:
        findings.append({
            "source": "build_verification",
            "category": "build_dry_run",
            "severity": "INFO",
            "message": f"PLANTER_BUILD_DRY_RUN: {name} would run `{fmt_cmd(cmd)}`",
        })
        continue
    try:
        proc = subprocess.run(cmd, cwd=workdir, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, timeout=timeout)
        code, output = proc.returncode, proc.stdout
    except subprocess.TimeoutExpired as e:
        code = "timeout"
        output = (e.stdout or b"").decode(errors="replace") if isinstance(e.stdout, bytes) else (e.stdout or "")
        output += f"\n(timed out after {timeout}s)"
    if code == 0:
        continue
    if name == "gn_build":
        note = unrelated_bazel_failure(output)
        if note:
            text, file, line = summarize(output, LOCATION)
            findings.append({
                "source": "build_verification",
                "category": "preexisting_failure_gn_build",
                "severity": "INFO",
                "file": file,
                "line": line,
                "message": f"`{fmt_cmd(cmd)}` failed ({code}) outside the change: {note}\n{text}",
            })
            continue
    failed.add(name)
    text, file, line = summarize(output, BUILD_FILE_LOCATION if name == "gn_build_dependents" else LOCATION)
    findings.append({
        "source": "build_verification",
        "category": f"build_failure_{name}",
        "severity": "ERROR",
        "file": file,
        "line": line,
        "message": f"`{fmt_cmd(cmd)}` failed ({code}):\n{text}",
        "remediation": REMEDIATION[name],
    })
    if name == "gn_build" and re.search(r"fx set|no build directory|Unable to find build directory", output, re.I):
        break  # The tree is not configured; the remaining steps would fail the same way.

step_findings = findings[step_findings_start:]
if reuse_key and not any(f.get("severity") in STEP_BLOCKING for f in step_findings):
    try:
        if _fingerprint() != reuse_key:
            raise OSError("the change was edited during the build")
        entries = {}
        try:
            with open(reuse_path) as f:
                old = json.load(f)
            if isinstance(old.get("entries"), dict):
                entries = old["entries"]
            elif old.get("key"):
                entries[old["key"]] = {"time": old.get("time", 0), "findings": old.get("findings", [])}
        except (OSError, ValueError):
            pass
        now = time.time()
        entries[reuse_key] = {"time": now, "findings": step_findings}
        if len(entries) > 20:
            newest = sorted(entries.items(), key=lambda kv: kv[1].get("time", 0), reverse=True)[:20]
            entries = dict(newest)
        fd, tmp = tempfile.mkstemp(dir=os.path.dirname(reuse_path), prefix=".planter-build-verification.")
        with os.fdopen(fd, "w") as f:
            json.dump({"key": reuse_key, "time": now, "findings": step_findings, "entries": entries}, f)
        os.replace(tmp, reuse_path)
    except (OSError, ValueError, subprocess.CalledProcessError):
        pass

print(json.dumps(findings, indent=2))
PYEOF
