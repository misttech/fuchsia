#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -euo pipefail

# Deterministic check that shared build configs (Rust lint configs and GN
# declare_args() build arguments) are referenced and reused from a single source
# of truth, not wrapped or duplicated, by GN-to-Bazel migrations.
#
# 1. alias_obscures_config (error): an `alias()` added by the change (absent from
#    the file at $PLANTER_CHANGE_BASE) whose `actual` is a `rust_lint_config` (or
#    whose name/actual is lint-named). Point `lint_config` at the defining
#    rust_lint_config instead. Warning for new aliases of other config-like rules.
# 2. duplicated_default_lints (error): a dict literal in a changed BUILD.bazel/.bzl
#    that repeats most entries of a default lint dict owned by
#    //build/config/rust/lints (its exported .bzl constants, or the private dicts
#    of its BUILD.bazel while nothing is exported yet). Area configs must load the
#    exported constants and spell out only their own additions.
# 3. lint_config_gn_parity (error): a `lint_config` label in a changed dual-build
#    BUILD.bazel (sibling BUILD.gn has the BAZEL2GN sentinel) whose package has no
#    GN `config("<name>")`. bazel2gn copies lint_config verbatim, so the GN
#    counterpart must sit next to the Bazel rust_lint_config under the same name.
# 4. broken_lint_then_change (error): a `LINT.ThenChange(...)` in or next to a
#    changed file that points at a missing file, or at a file without the matching
#    `LINT.IfChange(<id>)` (e.g. after moving lint dicts into a .bzl).
# 5. duplicated_gn_build_arg / inline_gn_declare_args / unexported_gn_build_arg /
#    missing_gn_build_arg_lint_change (error): a GN `declare_args()` build argument
#    re-defined or recalculated in a `.bzl` or `BUILD.bazel` file instead of being
#    exported from its `.gni` file via `generated_file("gn_build_variables_for_bazel")`
#    in `//build/bazel/BUILD.gn` and loaded from `@fuchsia_build_info//:args.bzl`
#    with bidirectional `LINT.IfChange` / `LINT.ThenChange` comments.
#
# All findings are change-wide, so with several target directories only the run
# for the first one reports.

WORKDIR="${PLANTER_WORKDIR:-.}"
TARGET_DIR="${PLANTER_TARGET_DIR:-}"
TARGET_DIRS="${PLANTER_TARGET_DIRS:-$TARGET_DIR}"

first_dir="${TARGET_DIRS%% *}"
if [[ -n "$TARGET_DIR" && -n "$first_dir" && "$TARGET_DIR" != "$first_dir" ]]; then
  echo "[]"
  exit 0
fi

python3 - "$WORKDIR" "$TARGET_DIRS" <<'PYEOF'
import ast
import json
import math
import os
import re
import subprocess
import sys

workdir = os.path.abspath(sys.argv[1])
raw_dirs = sys.argv[2] if len(sys.argv) > 2 else ""
target_dirs = [d.strip().strip("/") for d in re.split(r"[\s,]+", raw_dirs) if d.strip().strip("/")]
change_base = os.environ.get("PLANTER_CHANGE_BASE", "").strip() or "HEAD"

LINTS_PKG = "build/config/rust/lints"
BAZEL_BUILD_FILES = ("BUILD.bazel", "BUILD")
CONFIG_RULES = {
    "config_setting", "label_flag", "bool_flag", "string_flag", "int_flag",
    "string_list_flag", "constraint_value", "platform",
}
LINT_NAME_RE = re.compile(r"(^|[_:/-])(lints?|clippy)([_:/-]|$)", re.I)
SENTINEL_RE = re.compile(r"^##\s*BAZEL2GN SENTINEL", re.M)
BASELINE_PAIRS = {("all", "allow")}
MIN_OVERLAP = 3
RATIO = 0.6


def git(args):
    try:
        return subprocess.check_output(
            ["git", "-C", workdir] + args, stderr=subprocess.DEVNULL, text=True
        )
    except Exception:
        return None


def git_lines(args):
    return [l.strip() for l in (git(args) or "").splitlines() if l.strip()]


def read(rel):
    try:
        with open(os.path.join(workdir, rel), encoding="utf-8") as f:
            return f.read()
    except Exception:
        return None


def parse(text, rel):
    if text is None:
        return None
    try:
        return ast.parse(text, filename=rel)
    except Exception:
        return None


touched = set(git_lines(["diff", "--name-only", change_base]))
touched.update(git_lines(["ls-files", "--others", "--exclude-standard"]))
for td in target_dirs:
    td_abs = os.path.join(workdir, td)
    if not os.path.isdir(td_abs):
        continue
    for root, dirs, files in os.walk(td_abs):
        rel_root = os.path.relpath(root, workdir).strip(".")
        if rel_root != td and any(b in files for b in ("BUILD.bazel", "BUILD", "BUILD.gn")):
            dirs[:] = []
            continue
        for fname in files:
            if fname in ("BUILD.gn",) + BAZEL_BUILD_FILES or fname.endswith((".bzl", ".gni")):
                touched.add(os.path.join(rel_root, fname) if rel_root else fname)
changed = {p for p in touched if os.path.isfile(os.path.join(workdir, p))}


def is_starlark(rel):
    base = os.path.basename(rel)
    return base in BAZEL_BUILD_FILES or base.endswith(".bzl")


def str_kw(call, key):
    for kw in call.keywords:
        if kw.arg == key and isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str):
            return kw.value.value
    return None


def targets(tree):
    """name -> (rule, call) for the top-level rule calls of a BUILD file."""
    out = {}
    for node in (tree.body if tree else []):
        if isinstance(node, ast.Expr) and isinstance(node.value, ast.Call) and isinstance(node.value.func, ast.Name):
            name = str_kw(node.value, "name")
            if name:
                out[name] = (node.value.func.id, node.value)
    return out


def resolve(label, pkg):
    """(package, name) for an in-repo label, else None."""
    if not label or label.startswith("@"):
        return None
    if label.startswith(":"):
        return pkg, label[1:]
    if label.startswith("//"):
        body = label[2:]
        if ":" in body:
            p, n = body.split(":", 1)
        else:
            p, n = body, body.rsplit("/", 1)[-1]
        return p.strip("/"), n
    return None


_pkg_cache = {}


def pkg_targets(pkg):
    if pkg not in _pkg_cache:
        res = {}
        for base in BAZEL_BUILD_FILES:
            rel = os.path.join(pkg, base) if pkg else base
            text = read(rel)
            if text is not None:
                res = targets(parse(text, rel))
                break
        _pkg_cache[pkg] = res
    return _pkg_cache[pkg]


def rule_of(pkg, name, depth=0):
    t = pkg_targets(pkg).get(name)
    if not t:
        return None
    rule, call = t
    if rule == "alias" and depth < 5:
        r = resolve(str_kw(call, "actual"), pkg)
        if r:
            return rule_of(r[0], r[1], depth + 1) or "alias"
    return rule


def literal_pairs(node):
    pairs = set()
    for k, v in zip(node.keys, node.values):
        if (isinstance(k, ast.Constant) and isinstance(k.value, str)
                and isinstance(v, ast.Constant) and isinstance(v.value, str)):
            pairs.add((k.value, v.value))
    return pairs - BASELINE_PAIRS


def eval_dict(node, env):
    if isinstance(node, ast.Dict):
        return {k: v for k, v in literal_pairs(node)}
    if isinstance(node, ast.Name):
        return env.get(node.id)
    if isinstance(node, ast.BinOp) and isinstance(node.op, ast.BitOr):
        left, right = eval_dict(node.left, env), eval_dict(node.right, env)
        if left is None and right is None:
            return None
        return {**(left or {}), **(right or {})}
    return None


def dict_names(tree):
    """id(dict node) -> the variable or rule attribute it belongs to."""
    names = {}
    for node in tree.body:
        if isinstance(node, ast.Assign) and len(node.targets) == 1 and isinstance(node.targets[0], ast.Name):
            for sub in ast.walk(node.value):
                if isinstance(sub, ast.Dict):
                    names.setdefault(id(sub), node.targets[0].id)
        elif isinstance(node, ast.Expr) and isinstance(node.value, ast.Call) and isinstance(node.value.func, ast.Name):
            tname = str_kw(node.value, "name") or "?"
            for kw in node.value.keywords:
                for sub in ast.walk(kw.value):
                    if isinstance(sub, ast.Dict):
                        names.setdefault(id(sub), f"{node.value.func.id}({tname}).{kw.arg}")
    return names


def public_dicts(tree):
    env, public = {}, []
    for node in tree.body:
        if isinstance(node, ast.Assign) and len(node.targets) == 1 and isinstance(node.targets[0], ast.Name):
            val = eval_dict(node.value, env)
            if val is not None:
                env[node.targets[0].id] = val
                if val and not node.targets[0].id.startswith("_"):
                    public.append(node.targets[0].id)
    return public


def add_chunks(tree, rel, chunks):
    names = dict_names(tree)
    for node in ast.walk(tree):
        if isinstance(node, ast.Dict):
            pairs = literal_pairs(node)
            if len(pairs) >= MIN_OVERLAP:
                chunks.append((names.get(id(node), "dict"), rel, pairs))


findings = []


def emit(category, severity, rel, line, message, remediation):
    findings.append({
        "source": "shared_config_reuse",
        "category": category,
        "severity": severity,
        "file": rel,
        "line": line,
        "message": message,
        "remediation": remediation,
    })


# 1. New alias() wrappers around lint configs.
for rel in sorted(changed):
    if os.path.basename(rel) not in BAZEL_BUILD_FILES:
        continue
    pkg = os.path.dirname(rel)
    tree = parse(read(rel), rel)
    if tree is None:
        continue
    base_tree = parse(git(["show", f"{change_base}:{rel}"]), rel)
    base_aliases = {n for n, (r, _) in targets(base_tree).items() if r == "alias"}
    for name, (rule, call) in sorted(targets(tree).items()):
        if rule != "alias" or name in base_aliases:
            continue
        actual = str_kw(call, "actual") or ""
        r = resolve(actual, pkg)
        actual_rule = rule_of(*r) if r else None
        label = f"//{pkg}:{name}"
        if actual_rule == "rust_lint_config" or LINT_NAME_RE.search(name) or LINT_NAME_RE.search(actual):
            emit(
                "alias_obscures_config", "error", rel, call.lineno,
                f"New alias `{label}` only re-exports the lint config `{actual}`"
                + (f" ({actual_rule})" if actual_rule else "")
                + ". Wrapper labels hide which lints a target gets.",
                f"Delete `{label}` (and any GN `config(\"{name}\")` that only forwards to the same "
                f"lints) and set `lint_config = \"{actual}\"` on the targets that use it. bazel2gn copies "
                "lint_config verbatim, so the same-named GN `config()` must live in the BUILD.gn of the "
                "package that defines the Bazel rust_lint_config. Keep pre-existing wrapper labels "
                "unchanged; never add new ones.",
            )
        elif actual_rule in CONFIG_RULES:
            emit(
                "alias_obscures_config", "warning", rel, call.lineno,
                f"New alias `{label}` re-exports the {actual_rule} `{actual}` under another name.",
                f"Reference `{actual}` directly unless the alias is required for label compatibility; "
                "explain it in a comment if it is.",
            )

# 2. Copied default lint dicts.
canonical_files, exported, chunks = [], [], []
lints_dir = os.path.join(workdir, LINTS_PKG)
if os.path.isdir(lints_dir):
    for f in sorted(os.listdir(lints_dir)):
        if not f.endswith(".bzl"):
            continue
        rel = f"{LINTS_PKG}/{f}"
        tree = parse(read(rel), rel)
        names = public_dicts(tree) if tree else []
        if names:
            canonical_files.append(rel)
            exported += [(rel, n) for n in names]
            add_chunks(tree, rel, chunks)
    if not canonical_files:
        for base in BAZEL_BUILD_FILES:
            rel = f"{LINTS_PKG}/{base}"
            tree = parse(read(rel), rel)
            if tree is not None:
                canonical_files.append(rel)
                add_chunks(tree, rel, chunks)
                break

if exported:
    loads = "; ".join(
        f"load(\"//{os.path.dirname(r)}:{os.path.basename(r)}\", "
        + ", ".join(f"\"{n}\"" for rr, n in exported if rr == r) + ")"
        for r in sorted({r for r, _ in exported})
    )
    dup_fix = (
        f"{loads} and compose the config from those constants, e.g. "
        "`clippy = <production defaults> | _AREA_CLIPPY` (test variant: `<test defaults> | _AREA_CLIPPY`) "
        "and `rustc = <rustc defaults>`; keep only the area-specific entries as literals."
    )
else:
    dup_fix = (
        f"The defaults are private dicts in //{LINTS_PKG}/BUILD.bazel. Export them in this change: move "
        f"them verbatim (with their LINT.IfChange/ThenChange markers) into a .bzl in //{LINTS_PKG} under "
        "public names, load() them in that BUILD.bazel keeping every rust_lint_config target with the same "
        f"name and contents, point the LINT.ThenChange lines of //{LINTS_PKG}/BUILD.gn at the .bzl, then "
        "load the constants here and keep only the area-specific entries as literals."
    )

for rel in sorted(changed):
    if not is_starlark(rel) or rel in canonical_files:
        continue
    tree = parse(read(rel), rel)
    if tree is None:
        continue
    names = dict_names(tree)
    for node in sorted((n for n in ast.walk(tree) if isinstance(n, ast.Dict)), key=lambda n: n.lineno):
        pairs = literal_pairs(node)
        if len(pairs) < MIN_OVERLAP:
            continue
        best = None
        for cname, crel, cpairs in chunks:
            overlap = len(pairs & cpairs)
            if overlap >= MIN_OVERLAP and overlap >= math.ceil(RATIO * len(cpairs)):
                if best is None or overlap > best[0]:
                    best = (overlap, cname, crel, len(cpairs))
        if best:
            overlap, cname, crel, total = best
            where = names.get(id(node), "dict literal")
            emit(
                "duplicated_default_lints", "error", rel, node.lineno,
                f"`{where}` restates {overlap}/{total} entries of the default lint dict `{cname}` "
                f"owned by '{crel}'. Copies drift from the defaults and are not covered by its "
                "LINT.IfChange blocks.",
                dup_fix,
            )

# 3. lint_config labels that bazel2gn will copy into BUILD.gn.
def label_values(node):
    if isinstance(node, ast.Constant) and isinstance(node.value, str):
        yield node.value
    elif isinstance(node, ast.Dict):
        for v in node.values:
            yield from label_values(v)
    elif isinstance(node, ast.Call):
        for a in node.args:
            yield from label_values(a)
    elif isinstance(node, (ast.List, ast.Tuple)):
        for e in node.elts:
            yield from label_values(e)
    elif isinstance(node, ast.BinOp):
        yield from label_values(node.left)
        yield from label_values(node.right)


for rel in sorted(changed):
    if os.path.basename(rel) not in BAZEL_BUILD_FILES:
        continue
    pkg = os.path.dirname(rel)
    gn_text = read(os.path.join(pkg, "BUILD.gn"))
    if not gn_text or not SENTINEL_RE.search(gn_text):
        continue
    tree = parse(read(rel), rel)
    if tree is None:
        continue
    for node in ast.walk(tree):
        if not isinstance(node, ast.keyword) or node.arg != "lint_config":
            continue
        for label in label_values(node.value):
            if label == "//conditions:default":
                continue
            r = resolve(label, pkg)
            if not r:
                continue
            lpkg, lname = r
            lgn = read(os.path.join(lpkg, "BUILD.gn"))
            if lgn and re.search(r'^\s*config\(\s*"' + re.escape(lname) + r'"\s*\)', lgn, re.M):
                continue
            emit(
                "lint_config_gn_parity", "error", rel, getattr(node.value, "lineno", 0),
                f"`lint_config = \"{label}\"` is copied verbatim into the generated BUILD.gn by bazel2gn, "
                f"but '{os.path.join(lpkg, 'BUILD.gn')}' defines no `config(\"{lname}\")`.",
                f"Add `config(\"{lname}\")` to '{os.path.join(lpkg, 'BUILD.gn')}', next to the Bazel "
                "rust_lint_config of the same name, holding only the area-specific rustflags (GN appends "
                "the defaults and drops production lints on testonly targets). Do not add an alias or a "
                "forwarding config in another package instead.",
            )

# 4. LINT.ThenChange references broken by moving lint dicts.
scan = set(changed)
for rel in touched:
    d = os.path.dirname(rel)
    try:
        entries = os.listdir(os.path.join(workdir, d))
    except OSError:
        continue
    for f in entries:
        if f in ("BUILD.gn",) + BAZEL_BUILD_FILES or f.endswith((".bzl", ".gni")):
            scan.add(os.path.join(d, f) if d else f)

THEN_RE = re.compile(r"LINT\.ThenChange\(")


def then_change_items(text):
    """Yields (line, item) for every LINT.ThenChange(...) target in text."""
    for m in THEN_RE.finditer(text or ""):
        end = text.find(")", m.end())
        if end < 0:
            continue
        # Continuation lines start with a comment marker; the first line does not.
        arg = re.sub(r"\n\s*(#|//)[ \t]*", "\n", text[m.end():end])
        line = text.count("\n", 0, m.start()) + 1
        for item in re.split(r"[,\s]+", arg):
            item = item.strip().strip("\"'")
            if item:
                yield line, item


def split_item(item, src):
    path, label = item, None
    if ":" in item.rsplit("/", 1)[-1]:
        path, label = item.rsplit(":", 1)
    if path.startswith("/"):
        return path.lstrip("/"), label
    return os.path.normpath(os.path.join(os.path.dirname(src), path)), label


def problem_with(trel, label, text):
    if text is None:
        return f"points at '{trel}', which does not exist"
    if label and not re.search(r"LINT\.IfChange\(\s*" + re.escape(label) + r"\s*\)", text):
        return f"points at '{trel}', which has no `LINT.IfChange({label})`"
    return None


def current_text(trel):
    if os.path.isdir(os.path.join(workdir, trel)):
        return ""
    return read(trel)


for rel in sorted(scan):
    text = read(rel)
    if not text or "LINT.ThenChange(" not in text:
        continue
    base_items = None
    for line, item in then_change_items(text):
        trel, label = split_item(item, rel)
        if rel not in touched and trel not in touched:
            continue
        problem = problem_with(trel, label, current_text(trel))
        if not problem:
            continue
        # Only report references this change broke, not pre-existing stale ones.
        if base_items is None:
            base_items = {i for _, i in then_change_items(git(["show", f"{change_base}:{rel}"]))}
        if item in base_items and problem_with(trel, label, git(["show", f"{change_base}:{trel}"])):
            continue
        emit(
            "broken_lint_then_change", "error", rel, line,
            f"`LINT.ThenChange({item})` {problem}.",
            "Update the LINT.ThenChange path/label to the file that now holds the matching "
            "`LINT.IfChange(<id>)` block (e.g. the .bzl the lint dicts were moved to), keeping "
            "both sides of every IfChange/ThenChange pair.",
        )

# 5. GN declare_args() single source of truth via //build/bazel:gn_build_variables_for_bazel.
def strip_gn_comments(text):
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


def find_closing_brace(code, open_idx):
    depth, in_str, i, n = 0, False, open_idx, len(code)
    while i < n:
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
    return n


def infer_gn_arg_type(rhs):
    s = rhs.strip()
    if s in ("true", "false") or any(op in s for op in ("==", "!=", "&&", "||")) or s.startswith("!"):
        return "bool"
    if s.startswith("["):
        return "array_of_strings"
    if s.startswith('"//'):
        return "path"
    return "string"


def parse_gn_declare_args(text):
    """Returns {arg_name: (line_no, rhs_str, has_if_then_build_bazel)}."""
    if not text:
        return {}
    code = strip_gn_comments(text)
    out = {}
    for m in re.finditer(r"\bdeclare_args\s*\(\s*\)\s*\{", code):
        brace_open = m.end() - 1
        brace_close = find_closing_brace(code, brace_open)
        raw_block = text[m.start() : brace_close + 1]
        has_lint = (
            "LINT.IfChange" in raw_block
            and bool(re.search(r"LINT\.ThenChange\(\s*//build/bazel/BUILD\.gn", raw_block))
        )
        body = code[brace_open + 1 : brace_close]
        base_line = code.count("\n", 0, brace_open + 1) + 1
        depth = 0
        for rel_idx, line in enumerate(body.splitlines()):
            if depth == 0:
                am = re.match(r"^\s*([A-Za-z_]\w*)\s*=\s*(.+?)\s*$", line)
                if am:
                    out[am.group(1)] = (base_line + rel_idx, am.group(2), has_lint)
            depth += line.count("{") - line.count("}")
    return out


def parse_exported_gn_args():
    """Returns {arg_name: (decl_rel, has_lint_pair)} from //build/bazel/BUILD.gn."""
    bb_text = read("build/bazel/BUILD.gn")
    if not bb_text:
        return {}
    code = strip_gn_comments(bb_text)
    m = re.search(r'\bgenerated_file\(\s*"gn_build_variables_for_bazel"\s*\)\s*\{', code)
    if not m:
        return {}
    brace_open = m.end() - 1
    brace_close = find_closing_brace(code, brace_open)
    block_text = bb_text[brace_open + 1 : brace_close]
    exported = {}
    decl_iter = list(re.finditer(r'\bdeclaration\s*=\s*"([^"]+)"', block_text))
    for idx, dm in enumerate(decl_iter):
        decl_val = dm.group(1).lstrip("/")
        seg_start = max(0, dm.start() - 120)
        seg_end = decl_iter[idx + 1].start() if idx + 1 < len(decl_iter) else len(block_text)
        seg = block_text[seg_start:seg_end]
        has_if = "LINT.IfChange" in block_text[max(0, dm.start() - 120) : dm.end()]
        has_then = bool(
            re.search(r"LINT\.ThenChange\(\s*//" + re.escape(decl_val) + r"(?:[:)\s])", seg)
        )
        for nm in re.finditer(r'\bname\s*=\s*"([^"]+)"', block_text[dm.end() : seg_end]):
            exported[nm.group(1)] = (decl_val, has_if and has_then)
    return exported


pkg_dirs = set(target_dirs)
for p in changed:
    if os.path.basename(p) in ("BUILD.gn",) + BAZEL_BUILD_FILES:
        d = os.path.dirname(p).strip("/")
        if d and not d.startswith(("build/bazel", LINTS_PKG, "third_party/")):
            pkg_dirs.add(d)

gn_arg_files = set()
for p in changed:
    if p.endswith(".gni") or os.path.basename(p) == "BUILD.gn":
        if not p.startswith(("build/bazel", "third_party/")):
            gn_arg_files.add(p)

for d in sorted(pkg_dirs):
    d_abs = os.path.join(workdir, d)
    if os.path.isdir(d_abs):
        for root, dirs, files in os.walk(d_abs):
            rel_root = os.path.relpath(root, workdir).strip(".")
            if rel_root != d and any(b in files for b in ("BUILD.bazel", "BUILD", "BUILD.gn")):
                dirs[:] = []
                continue
            for fname in files:
                if fname == "BUILD.gn" or fname.endswith(".gni"):
                    gn_arg_files.add(os.path.join(rel_root, fname) if rel_root else fname)
    for gn_src in (read(os.path.join(d, "BUILD.gn")), git(["show", f"{change_base}:{d}/BUILD.gn"])):
        if not gn_src:
            continue
        for im in re.finditer(r'\bimport\(\s*"//([^"]+)"\s*\)', strip_gn_comments(gn_src)):
            imp_rel = im.group(1).strip("/")
            if not imp_rel.startswith(("build/components", "build/rust/", "build/tools/bazel2gn/", "build/fidl/", "build/cpp/", "build/go/", "build/python/", "build/zircon/")):
                gn_arg_files.add(imp_rel)

declared_args = {}
for grel in sorted(gn_arg_files, key=lambda p: (0 if p.endswith(".gni") else 1, p)):
    cur_txt = read(grel)
    parsed = parse_gn_declare_args(cur_txt) if cur_txt is not None else {}
    if not parsed and grel in touched:
        parsed = parse_gn_declare_args(git(["show", f"{change_base}:{grel}"]))
    for aname, (aline, arhs, alint) in parsed.items():
        if aname not in declared_args or (declared_args[aname][0].endswith("BUILD.gn") and grel.endswith(".gni")):
            declared_args[aname] = (grel, aline, arhs, alint)

exported_gn_args = parse_exported_gn_args()


def gn_arg_remediation(aname, decl_rel, rhs, pkg):
    arg_type = infer_gn_arg_type(rhs)
    gni_rel = decl_rel if decl_rel.endswith(".gni") else f"{pkg}/build/args.gni"
    move_note = (
        f"Move `declare_args()` for `{aname}` out of '{decl_rel}' into `//{gni_rel}` and add "
        f"`import(\"//{gni_rel}\")` above `## BAZEL2GN SENTINEL` in '{decl_rel}' "
        "(`//build/bazel:gn_build_variables_for_bazel` can only `import()` a `.gni` file, not `BUILD.gn`). "
        if decl_rel.endswith("BUILD.gn")
        else f"Keep `declare_args()` in '{gni_rel}' (imported above `## BAZEL2GN SENTINEL` in `{pkg}/BUILD.gn`). "
    )
    return (
        move_note
        + f"Wrap `{aname} = ...` in '{gni_rel}' with `# LINT.IfChange` and `# LINT.ThenChange(//build/bazel/BUILD.gn:{aname})` "
        "(a scoped, named label: 'build/bazel/BUILD.gn' holds much unrelated content), "
        f"export `{aname}` in `generated_file(\"gn_build_variables_for_bazel\")` in 'build/bazel/BUILD.gn' "
        f"(`# LINT.IfChange({aname})`, `declaration = \"//{gni_rel}\"`, `import(declaration)`, "
        f"`contents += [ {{ name = \"{aname}\" value = {aname} type = \"{arg_type}\" location = declaration }} ]`, "
        f"`# LINT.ThenChange(//{gni_rel})`), delete any duplicate `.bzl` file or Starlark assignment, "
        f"and load `{aname}` in `{pkg}/BUILD.bazel` via `load(\"@fuchsia_build_info//:args.bzl\", \"{aname}\")` "
        f"before running `fx bazel2gn -d {pkg}`."
    )


for rel in sorted(changed):
    if not is_starlark(rel) or rel.startswith(("build/bazel/", LINTS_PKG, "third_party/")):
        continue
    tree = parse(read(rel), rel)
    if tree is None:
        continue
    pkg = os.path.dirname(rel)
    for p_cand in sorted(pkg_dirs, key=len, reverse=True):
        if rel == p_cand or rel.startswith(p_cand + "/"):
            pkg = p_cand
            break

    for node in tree.body:
        # 5a. Top-level assignment in BUILD.bazel or .bzl duplicating a GN declare_args() variable.
        if isinstance(node, ast.Assign):
            for t in node.targets:
                if isinstance(t, ast.Name) and t.id in declared_args:
                    decl_rel, decl_line, rhs, _ = declared_args[t.id]
                    emit(
                        "duplicated_gn_build_arg", "error", rel, node.lineno,
                        f"GN build argument `{t.id}` (declared in `declare_args()` in '{decl_rel}:{decl_line}') "
                        f"is duplicated by a Starlark assignment in '{rel}'. Defining or recalculating a GN "
                        "`declare_args()` variable in `.bzl` or `BUILD.bazel` creates two sources of truth "
                        "that can drift out of sync and ignores `args.gn` overrides.",
                        gn_arg_remediation(t.id, decl_rel, rhs, pkg),
                    )
        # 5b. load() of a GN declare_args() variable from a custom .bzl instead of @fuchsia_build_info//:args.bzl,
        #     or load() from @fuchsia_build_info//:args.bzl without proper export / LINT markers.
        if (
            isinstance(node, ast.Expr)
            and isinstance(node.value, ast.Call)
            and isinstance(node.value.func, ast.Name)
            and node.value.func.id == "load"
            and node.value.args
            and isinstance(node.value.args[0], ast.Constant)
            and isinstance(node.value.args[0].value, str)
        ):
            mod = node.value.args[0].value
            loaded_syms = []
            for a in node.value.args[1:]:
                if isinstance(a, ast.Constant) and isinstance(a.value, str):
                    loaded_syms.append(a.value)
            for kw in node.value.keywords:
                if isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str):
                    loaded_syms.append(kw.value.value)
            for sym in loaded_syms:
                if sym in declared_args and mod != "@fuchsia_build_info//:args.bzl":
                    decl_rel, decl_line, rhs, _ = declared_args[sym]
                    emit(
                        "duplicated_gn_build_arg", "error", rel, node.lineno,
                        f"`{rel}` loads GN build argument `{sym}` (declared in `declare_args()` in "
                        f"'{decl_rel}:{decl_line}') from `{mod}` instead of `@fuchsia_build_info//:args.bzl`.",
                        gn_arg_remediation(sym, decl_rel, rhs, pkg),
                    )
                elif mod == "@fuchsia_build_info//:args.bzl" and sym != "target_cpu":
                    decl_info = declared_args.get(sym)
                    decl_rel = decl_info[0] if decl_info else f"{pkg}/build/args.gni"
                    rhs = decl_info[2] if decl_info else "false"
                    if decl_info and decl_rel.endswith("BUILD.gn"):
                        emit(
                            "inline_gn_declare_args", "error", decl_rel, decl_info[1],
                            f"GN build argument `{sym}` is declared inline in '{decl_rel}', which "
                            "`//build/bazel:gn_build_variables_for_bazel` cannot `import()`.",
                            gn_arg_remediation(sym, decl_rel, rhs, pkg),
                        )
                    if sym not in exported_gn_args:
                        emit(
                            "unexported_gn_build_arg", "error", rel, node.lineno,
                            f"`{rel}` loads `{sym}` from `@fuchsia_build_info//:args.bzl`, but `{sym}` is not "
                            "exported in `generated_file(\"gn_build_variables_for_bazel\")` in 'build/bazel/BUILD.gn'.",
                            gn_arg_remediation(sym, decl_rel, rhs, pkg),
                        )
                    elif decl_info:
                        exp_decl, bb_has_lint = exported_gn_args[sym]
                        gni_has_lint = decl_info[3]
                        if not gni_has_lint:
                            emit(
                                "missing_gn_build_arg_lint_change", "error", decl_rel, decl_info[1],
                                f"GN build argument `{sym}` in '{decl_rel}' is exported to Bazel in "
                                "'build/bazel/BUILD.gn' but is missing `# LINT.IfChange` / "
                                f"`# LINT.ThenChange(//build/bazel/BUILD.gn:{sym})` inside `declare_args()`.",
                                f"Wrap `{sym} = ...` in '{decl_rel}' with `# LINT.IfChange` and "
                                f"`# LINT.ThenChange(//build/bazel/BUILD.gn:{sym})`, and name the export "
                                f"block's marker in 'build/bazel/BUILD.gn' `# LINT.IfChange({sym})`.",
                            )
                        elif decl_rel in touched:
                            # 5c. A LINT pair added/edited by the change must scope its ThenChange into
                            #     the large, shared //build/bazel/BUILD.gn with a named label.
                            bb_all = read("build/bazel/BUILD.gn") or ""
                            for tm in re.finditer(
                                r"LINT\.ThenChange\(\s*//build/bazel/BUILD\.gn(?::([\w.-]+))?\s*\)",
                                read(decl_rel) or "",
                            ):
                                lbl = tm.group(1)
                                t_line = (read(decl_rel) or "").count("\n", 0, tm.start()) + 1
                                if lbl and re.search(r"LINT\.IfChange\(\s*" + re.escape(lbl) + r"\s*\)", bb_all):
                                    continue
                                emit(
                                    "unscoped_gn_build_arg_lint_change", "error", decl_rel, t_line,
                                    f"`LINT.ThenChange(//build/bazel/BUILD.gn{':' + lbl if lbl else ''})` in "
                                    f"'{decl_rel}' (exporting `{sym}`) "
                                    + ("names a label with no matching `LINT.IfChange(" + lbl + ")` in "
                                       "'build/bazel/BUILD.gn'." if lbl else
                                       "is unscoped: 'build/bazel/BUILD.gn' holds much unrelated content, so "
                                       "the pair must point at the specific export block via a named label."),
                                    f"Change it to `# LINT.ThenChange(//build/bazel/BUILD.gn:{sym})` and change "
                                    f"the export block's `# LINT.IfChange` (above `declaration = \"//{exp_decl}\"`) "
                                    f"in 'build/bazel/BUILD.gn' to `# LINT.IfChange({sym})` (one label per export "
                                    "block; its `# LINT.ThenChange(//" + exp_decl + ")` back to the small .gni "
                                    "may stay unscoped).",
                                )
                        if not bb_has_lint:
                            emit(
                                "missing_gn_build_arg_lint_change", "error", "build/bazel/BUILD.gn", 1,
                                f"Export block for `{sym}` (`declaration = \"//{exp_decl}\"`) in "
                                "'build/bazel/BUILD.gn' is missing `# LINT.IfChange` / "
                                f"`# LINT.ThenChange(//{exp_decl})`.",
                                f"Wrap the `declaration = \"//{exp_decl}\"` export block in 'build/bazel/BUILD.gn' "
                                f"with `# LINT.IfChange` and `# LINT.ThenChange(//{exp_decl})`.",
                            )

# 6. Check IFTTT LINT markers on dual-maintained build_flags() and GN config() definitions.
def check_build_flags_ifttt_lint():
    """Verify build_flags() mirroring a hand-maintained GN config() has LINT.IfChange/ThenChange."""
    for rel in sorted(changed):
        if os.path.basename(rel) not in BAZEL_BUILD_FILES:
            continue
        pkg = os.path.dirname(rel)
        gn_rel = os.path.join(pkg, "BUILD.gn") if pkg else "BUILD.gn"
        gn_text = read(gn_rel)
        bz_text = read(rel)
        if not gn_text or not bz_text:
            continue
        above_sentinel = gn_text.split("BAZEL2GN SENTINEL")[0] if SENTINEL_RE.search(gn_text) else gn_text
        gn_configs = {
            m.group(1)
            for m in re.finditer(r'^\s*config\(\s*"([^"]+)"\s*\)', above_sentinel, re.M)
        }
        if not gn_configs:
            continue
        tree = parse(bz_text, rel)
        if tree is None:
            continue
        for name, (rule, call) in sorted(targets(tree).items()):
            if rule != "build_flags" or name not in gn_configs:
                continue
            has_bz_if = bool(re.search(r"LINT\.IfChange\(\s*" + re.escape(name) + r"\s*\)", bz_text))
            has_bz_then = bool(
                re.search(
                    r"LINT\.ThenChange\(\s*(?://" + re.escape(gn_rel) + r"|BUILD\.gn):"
                    + re.escape(name) + r"\s*\)",
                    bz_text,
                )
            )
            has_gn_if = bool(re.search(r"LINT\.IfChange\(\s*" + re.escape(name) + r"\s*\)", above_sentinel))
            has_gn_then = bool(
                re.search(
                    r"LINT\.ThenChange\(\s*(?://" + re.escape(rel) + r"|BUILD\.bazel):"
                    + re.escape(name) + r"\s*\)",
                    above_sentinel,
                )
            )
            if not (has_bz_if and has_bz_then and has_gn_if and has_gn_then):
                emit(
                    "missing_build_flags_lint_ifttt",
                    "error",
                    rel,
                    call.lineno,
                    f"`build_flags(name = \"{name}\")` in '{rel}' mirrors hand-maintained "
                    f"`config(\"{name}\")` in '{gn_rel}', but lacks bidirectional "
                    f"`# LINT.IfChange({name})` / `# LINT.ThenChange(...)` comments.",
                    f"Wrap `build_flags(name = \"{name}\")` in '{rel}' with "
                    f"`# LINT.IfChange({name})` and `# LINT.ThenChange(//{gn_rel}:{name})`, "
                    f"and wrap `config(\"{name}\")` in '{gn_rel}' with "
                    f"`# LINT.IfChange({name})` and `# LINT.ThenChange(//{rel}:{name})`.",
                )


check_build_flags_ifttt_lint()

unique, seen = [], set()
for f in findings:
    key = (f["category"], f["file"], f["line"], f["message"])
    if key not in seen:
        seen.add(key)
        unique.append(f)
print(json.dumps(unique, indent=2))
PYEOF
