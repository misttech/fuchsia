#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -euo pipefail

# Deterministic check that a migration keeps the GN dependency labels and edge
# kinds of every target it converts. For each directory of the change whose
# BUILD.gn existed at $PLANTER_CHANGE_BASE and still exists (a package whose
# BUILD.gn is deleted has no GN dependents left), every GN target that still
# exists under the same name is compared with its original over deps,
# public_deps, non_rust_deps, test_deps, proc_macro_deps, link_deps, data_deps
# and non_test_deps (moving a label between these lists is fine, except out of
# public_deps, see 3.; so is moving a test_deps label to another target of the
# package, e.g. an explicit test target replacing `with_unit_tests`). A label
# the target now reaches through a GN `group` it newly depends on (e.g. a
# package-local group above the BAZEL2GN SENTINEL that a BUILD.bazel entry is
# mapped to with `# @bazel2gn:path_overwrite:`) counts as kept.
# 1. dep_label_substituted (error): a dependency was swapped for another target
#    of the same package that still exists, e.g. a GN-only `<lib>_static`
#    wrapper above the dependency's BAZEL2GN SENTINEL replaced by the `<lib>`
#    it wraps. Such wrappers exist to add GN configs or link settings; the swap
#    builds in Bazel but breaks GN links. Pure forwarding groups and GN groups
#    replacing a migrated GN test target with `bazel_test_suite` and/or its
#    renamed `<package_name>-gn` dual-build test package are exempt.
# 2. dep_link_settings_dropped (error): a removed dependency carries
#    link-affecting settings (public_configs, all_dependent_configs, ldflags,
#    libs, rustflags, complete_static_lib). A removed test_deps entry of a
#    target that no longer sets with_unit_tests (its unit test moved to Bazel)
#    is exempt: only GN's generated <name>_test executable linked it.
# 3. public_dep_forwarding_lost (error): a dependency with public_configs was a
#    public_deps edge and is now reached only through private edges, and no
#    group the target newly depends on re-exports those configs as
#    all_dependent_configs. GN forwards public_configs to dependents only
#    through public_deps (bazel2gn emits Rust deps as private GN deps).
# 4. dep_label_removed (info): any other dependency that no target of the
#    package depends on anymore (e.g. an unused test_deps entry).

WORKDIR="${PLANTER_WORKDIR:-.}"
TARGET_DIR="${PLANTER_TARGET_DIR:-}"

python3 - "$WORKDIR" "$TARGET_DIR" <<'PYEOF'
import json
import os
import re
import subprocess
import sys

workdir = os.path.abspath(sys.argv[1])
target_dir = sys.argv[2].strip().strip("/")
change_base = os.environ.get("PLANTER_CHANGE_BASE", "").strip() or "HEAD"

GN_DEP_ATTRS = ("deps", "public_deps", "non_rust_deps", "test_deps", "proc_macro_deps", "link_deps", "data_deps", "non_test_deps")
LINK_ATTRS = ("public_configs", "all_dependent_configs", "ldflags", "libs", "rustflags", "complete_static_lib")
FORWARDING_ONLY_ATTRS = {"public_deps", "deps", "visibility", "testonly"}
GN_TEST_PACKAGE_TEMPLATES = {"fuchsia_unittest_package", "fuchsia_test_package"}
GN_TEST_TEMPLATES = GN_TEST_PACKAGE_TEMPLATES | {
    "fuchsia_unittest_component",
    "fuchsia_test_component",
    "fuchsia_test",
    "python_host_test",
    "host_test",
    "rustc_test",
    "go_test",
    "test",
    "cc_test_executable",
}
SENTINEL = re.compile(r"^##\s*BAZEL2GN SENTINEL|#LOCAL_BAZEL_BUILD_SENTINEL", re.M)
GN_TARGET = re.compile(r'\b([A-Za-z_]\w*)\(\s*"([^"$]+)"\s*\)\s*\{')
GN_STRING = re.compile(r'"((?:[^"\\]|\\.)*)"')
NOT_TARGETS = {"template", "declare_args", "foreach", "forward_variables_from"}


def git_lines(args):
    try:
        out = subprocess.check_output(["git", "-C", workdir] + args, stderr=subprocess.DEVNULL, text=True)
    except Exception:
        return []
    return [l.strip() for l in out.splitlines() if l.strip()]


_shown = {}


def old_text(rel):
    """The file as of $PLANTER_CHANGE_BASE, or None."""
    if rel not in _shown:
        try:
            _shown[rel] = subprocess.check_output(
                ["git", "-C", workdir, "show", f"{change_base}:{rel}"], stderr=subprocess.DEVNULL, text=True
            )
        except Exception:
            _shown[rel] = None
    return _shown[rel]


def new_text(rel):
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
    """Index of the bracket closing the `{` or `[` at code[i]."""
    open_c = code[i]
    close_c = "}" if open_c == "{" else "]"
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
        elif c == open_c:
            depth += 1
        elif c == close_c:
            depth -= 1
            if depth == 0:
                return i
        i += 1
    return len(code)


def gn_targets(text):
    """{name: {"tmpl", "start", "line", "bodies", "vars"}} for every `tmpl("name") { ... }` block."""
    code = gn_code(text)
    gn_vars = {}
    for vm in re.finditer(r"(?m)^\s*([A-Za-z_]\w*)\s*=\s*\[", code):
        b_idx = vm.end() - 1
        gn_vars[vm.group(1)] = GN_STRING.findall(code[b_idx + 1 : closing(code, b_idx)])
    targets = {}
    for m in GN_TARGET.finditer(code):
        if m.group(1) in NOT_TARGETS:
            continue
        brace = m.end() - 1
        t = targets.setdefault(
            m.group(2),
            {
                "tmpl": m.group(1),
                "start": m.start(),
                "line": code.count("\n", 0, m.start()) + 1,
                "bodies": [],
                "vars": gn_vars,
            },
        )
        t["bodies"].append(code[brace + 1 : closing(code, brace)])
    return targets


_parsed = {}


def parsed(rel, version):
    """(text, gn_targets(text)) of a BUILD.gn as of the base ("old") or on disk ("new"); (None, {}) if absent."""
    key = (rel, version)
    if key not in _parsed:
        text = old_text(rel) if version == "old" else new_text(rel)
        _parsed[key] = (text, gn_targets(text) if text else {})
    return _parsed[key]


def gn_lists(body, attrs, gn_vars=None):
    """{attr: [string literals]} of `attr = ...` / `attr += ...` assignments in body."""
    gn_vars = gn_vars or {}
    res = {}
    n = len(body)
    for m in re.finditer(r"(?<![\w.])(%s)\s*\+?=\s*" % "|".join(attrs), body):
        attr = m.group(1)
        idx = m.end()
        items = res.setdefault(attr, [])
        while idx < n:
            while idx < n and body[idx].isspace():
                idx += 1
            if idx >= n:
                break
            if body[idx] == "[":
                end_b = closing(body, idx)
                items.extend(GN_STRING.findall(body[idx + 1 : end_b]))
                idx = end_b + 1
            else:
                id_m = re.match(r"[A-Za-z_]\w*", body[idx:])
                if not id_m:
                    break
                items.extend(gn_vars.get(id_m.group(0), ()))
                idx += id_m.end()
            while idx < n and body[idx].isspace():
                idx += 1
            if idx < n and body[idx] == "+":
                idx += 1
            else:
                break
    return res


def normalize(raw, pkg):
    """Absolute `//path:name` form of a GN label; None for labels built from variables."""
    raw = raw.strip()
    if not raw or "$" in raw or raw.startswith("@"):
        return None
    raw = re.sub(r"\(.*\)$", "", raw)
    if raw.startswith("//"):
        path, _, name = raw[2:].partition(":")
    elif raw.startswith(":"):
        path, name = pkg, raw[1:]
    else:
        rel, _, name = raw.partition(":")
        path = os.path.normpath(os.path.join(pkg, rel))
        if path.startswith(".."):
            return None
    path = path.strip("/")
    name = name or os.path.basename(path)
    return f"//{path}:{name}" if name else None


def gn_deps(target, pkg):
    """{attr: set(labels)} over all the blocks defining a GN target."""
    res = {}
    gn_vars = target.get("vars") or {}
    for body in target["bodies"]:
        for attr, items in gn_lists(body, GN_DEP_ATTRS, gn_vars).items():
            res.setdefault(attr, set()).update(l for l in (normalize(s, pkg) for s in items) if l)
    return res


def split(label):
    path, _, name = label[2:].partition(":")
    return path, name


def compact(text):
    return re.sub(r"\s+", " ", text).strip()


_resolved = {}


def resolve(label, now=False):
    """(info or None, exists_now) for the GN target behind `label`, as of $PLANTER_CHANGE_BASE (falling
    back to the file on disk), or as on disk (falling back to the base) when `now` is set."""
    key = (label, now)
    if key in _resolved:
        return _resolved[key]
    path, name = split(label)
    gn_rel = f"{path}/BUILD.gn"
    first, second = ("new", "old") if now else ("old", "new")
    text, targets = parsed(gn_rel, first)
    if text is None:
        text, targets = parsed(gn_rel, second)
    info = None
    t = targets.get(name)
    if t:
        body = "\n".join(t["bodies"])
        sentinel = SENTINEL.search(text)
        link = []
        for attr in LINK_ATTRS:
            m = re.search(r"(?<![\w.])%s\s*\+?=\s*(\[[^\]]*\]|true)" % attr, body)
            if m:
                link.append(f"{attr} = {compact(m.group(1))}")
        lists = gn_lists(body, ("public_deps", "deps", "public_configs", "all_dependent_configs"))

        def labels(attr):
            return {l for l in (normalize(s, path) for s in lists.get(attr, [])) if l}

        pkg_name_m = re.search(r'\bpackage_name\s*=\s*"([^"$]+)"', body)
        attrs = set(re.findall(r"(?m)^\s*([A-Za-z_]\w*)\s*[+-]?=", body))
        info = {
            "tmpl": t["tmpl"],
            "above_sentinel": bool(sentinel) and t["start"] < sentinel.start(),
            "link": link,
            "public_configs": labels("public_configs"),
            "all_dependent_configs": labels("all_dependent_configs"),
            "public_deps": labels("public_deps"),
            "deps": labels("deps"),
            "forwarding_group": t["tmpl"] == "group" and attrs <= FORWARDING_ONLY_ATTRS and not link,
            "package_name": pkg_name_m.group(1) if pkg_name_m else name,
        }
    now_bazel = new_text(f"{path}/BUILD.bazel") or ""
    in_bazel = bool(re.search(r'\bname\s*=\s*"%s"' % re.escape(name), now_bazel))
    in_gn_now = name in parsed(gn_rel, "new")[1]
    exists_now = in_bazel or in_gn_now
    if info is not None:
        info["gn_only"] = not in_bazel
        info["in_gn_now"] = in_gn_now
        info["in_bazel_now"] = in_bazel
    _resolved[key] = (info, exists_now)
    return _resolved[key]


# Check whether a GN group dependency on a migrated test target was replaced by
# bazel_test_suite and/or a renamed dual-build GN test package (<package_name>-gn).
def is_migrated_test_group_dep(group_tmpl, label, info, old_all, subs, new_all):
    if group_tmpl != "group" or not info or not subs:
        return False
    if info["tmpl"] not in GN_TEST_TEMPLATES or info["link"] or info.get("in_gn_now"):
        return False
    dep_pkg = split(label)[0]
    if not new_text(f"{dep_pkg}/BUILD.bazel"):
        return False
    old_pkg_names = set()
    for old_lbl in old_all:
        if split(old_lbl)[0] != dep_pkg:
            continue
        oi, _ = resolve(old_lbl)
        if oi and oi["tmpl"] in GN_TEST_PACKAGE_TEMPLATES:
            old_pkg_names.add(oi["package_name"])
    allowed_gn_pkg_names = old_pkg_names | {f"{p}-gn" for p in old_pkg_names}
    has_suite = False
    for s in subs:
        si, _ = resolve(s, now=True)
        if not si:
            return False
        if si["tmpl"] == "bazel_test_suite":
            has_suite = True
        elif (
            si["tmpl"] in GN_TEST_PACKAGE_TEMPLATES
            and si["package_name"] in allowed_gn_pkg_names
        ):
            continue
        else:
            return False
    if info["tmpl"] in GN_TEST_PACKAGE_TEMPLATES:
        expected_names = {info["package_name"], f"{info['package_name']}-gn"}
        for n_lbl in new_all:
            if split(n_lbl)[0] != dep_pkg:
                continue
            ni, _ = resolve(n_lbl, now=True)
            if (
                ni
                and ni["tmpl"] in GN_TEST_PACKAGE_TEMPLATES
                and ni["package_name"] in expected_names
            ):
                return True
    return bool(info.get("in_bazel_now") or has_suite)


def reach(labels):
    """Labels reached from `labels` through GN groups as currently defined: ({label: [(first hop,
    whether public configs are forwarded along the way)]}, all_dependent_configs of those groups)."""
    reached, adc, seen = {}, set(), set()
    todo = [(l, l, True) for l in labels]
    while todo:
        label, hop, forwards = todo.pop()
        if (label, forwards) in seen:
            continue
        seen.add((label, forwards))
        info, _ = resolve(label, now=True)
        if not info or info["tmpl"] != "group":
            continue
        adc |= info["all_dependent_configs"]
        for kind in ("public_deps", "deps"):
            for dep in sorted(info[kind]):
                fwd = forwards and kind == "public_deps"
                reached.setdefault(dep, []).append((hop, fwd))
                todo.append((dep, hop, fwd))
    return reached, adc


def spelled(label, pkg):
    """The ways a label can be written in a BUILD file of package pkg."""
    path, name = split(label)
    forms = [f'"//{path}:{name}"', f"path_overwrite://{path}:{name}"]
    if name == os.path.basename(path):
        forms += [f'"//{path}"', f"path_overwrite://{path}\n"]
    if path == pkg:
        forms += [f'":{name}"', f"path_overwrite::{name}"]
    m = re.match(r"^//third_party/rust_crates:(.+)$", label)
    if m:
        forms.append(f'"//third_party/rust_crates/vendor:{m.group(1)}"')
    return forms


def locate(pkg, labels, fallback_file, fallback_line):
    """File and line in BUILD.bazel (preferred, it is what the coder edits) or BUILD.gn naming a label."""
    for fname in ("BUILD.bazel", "BUILD.gn"):
        text = new_text(f"{pkg}/{fname}")
        if not text:
            continue
        for n, line in enumerate(text.splitlines(), 1):
            if any(f in line + "\n" for l in labels for f in spelled(l, pkg)):
                return f"{pkg}/{fname}", n
    return fallback_file, fallback_line


def describe(label, info):
    if info is None:
        return f"`{label}`"
    where = []
    if info["gn_only"]:
        where.append(
            "defined only in GN"
            + (f" (above the BAZEL2GN SENTINEL of {split(label)[0]}/BUILD.gn)" if info["above_sentinel"] else "")
        )
    if info["link"]:
        where.append("carrying " + "; ".join(f"`{l}`" for l in info["link"]))
    return f"`{label}` (`{info['tmpl']}`" + (", " + ", ".join(where) if where else "") + ")"


def rust_forwarding_remedy(label, configs, pkg):
    name = split(label)[1]
    listed = ", ".join(f'"{c}"' for c in sorted(configs)) or "<its public_configs>"
    return (
        "bazel2gn emits Rust deps as private GN `deps` and cannot generate a `public_deps` edge. Keep the configs "
        f"reaching GN dependents with a package-local GN group above the BAZEL2GN SENTINEL of {pkg}/BUILD.gn, e.g. "
        f'`group("{name}") {{ visibility = [ ":*" ] public_deps = [ "{label}" ] all_dependent_configs = [ {listed} ] }}` '
        "with a comment saying why, point the BUILD.bazel entry at it with "
        f"`# @bazel2gn:path_overwrite::{name}`, run `fx bazel2gn -d {pkg}` and mention the group in the CoderReport "
        "summary. Do not approximate it any other way (no extra rustc_flags or -l flags, no edits to other "
        "packages' BUILD files, no source edits)."
    )


EXCLUDED_GLOBAL_DIRS = (
    "bundles/assembly",
    "build/bazel",
    "build/bazel2gn",
    "build/images",
    "build/config/rust/lints",
)

candidate_dirs = {target_dir} if target_dir else set()
changed_all = git_lines(["diff", "--name-only", change_base]) + git_lines(["ls-files", "--others", "--exclude-standard"])
changed_set = set(changed_all)
for p in changed_all:
    if os.path.basename(p) in ("BUILD.bazel", "BUILD.gn"):
        d = os.path.dirname(p).strip("/")
        if (
            d
            and d not in ("tools", "src", "sdk", "build")
            and not any(d == ex or d.startswith(ex + "/") for ex in EXCLUDED_GLOBAL_DIRS)
            and not (target_dir and d == os.path.dirname(target_dir) and f"{d}/BUILD.bazel" not in changed_set)
        ):
            candidate_dirs.add(d)

findings = []
for pkg in sorted(candidate_dirs):
    gn_rel = f"{pkg}/BUILD.gn"
    before = old_text(gn_rel)
    if not before:
        continue
    old_targets = gn_targets(before)
    after = new_text(gn_rel)
    if after is None:
        # BUILD.gn deleted (full removal): no GN target can depend on this package anymore.
        continue
    new_file = gn_rel
    new_targets = {
        n: {"line": t["line"], "tmpl": t["tmpl"], "deps": gn_deps(t, pkg), "bodies": t["bodies"]}
        for n, t in gn_targets(after).items()
    }
    package_now = set().union(*[set().union(*t["deps"].values()) for t in new_targets.values() if t["deps"]])
    has_bazel_test_suite = any(t["tmpl"] == "bazel_test_suite" for t in new_targets.values())

    # Identify pure single-target forwarding `group("<outer>")` targets in `old_targets`
    # whose same-package target `:<inner>` was folded directly into `<outer>` in `new_targets`.
    folded_from_group = {}
    folded_into_group = {}
    for outer, old_g in sorted(old_targets.items()):
        if old_g["tmpl"] != "group" or outer not in new_targets:
            continue
        if new_targets[outer]["tmpl"] == "group":
            continue
        g_body = "\n".join(old_g["bodies"])
        g_attrs = set(re.findall(r"(?m)^\s*([A-Za-z_]\w*)\s*[+-]?=", g_body))
        if not g_attrs or not (g_attrs <= FORWARDING_ONLY_ATTRS):
            continue
        if any(re.search(r"(?<![\w.])%s\s*\+?=" % a, g_body) for a in LINK_ATTRS):
            continue
        raw_lists = gn_lists(g_body, ("public_deps", "deps"), old_g.get("vars"))
        raw_deps = [s for items in raw_lists.values() for s in items]
        if len(raw_deps) != 1 or "(" in raw_deps[0]:
            continue
        norm_dep = normalize(raw_deps[0], pkg)
        if not norm_dep:
            continue
        dep_pkg, inner = split(norm_dep)
        if dep_pkg != pkg or inner == outer or inner not in old_targets or inner in new_targets:
            continue
        if old_targets[inner]["tmpl"] != new_targets[outer]["tmpl"]:
            continue
        folded_from_group[outer] = inner
        folded_into_group[inner] = outer

    for name, old_t in sorted(old_targets.items()):
        if name in folded_into_group:
            continue
        if name in folded_from_group:
            old_t = old_targets[folded_from_group[name]]
        # Skip targets replaced by a Bazel host binary, test suite, or package bridge in GN.
        if (
            name not in new_targets
            or new_targets[name]["tmpl"]
            in ("bazel_host_tool", "bazel_test_suite", "bazel_fuchsia_package")
            or (
                has_bazel_test_suite
                and new_targets[name]["tmpl"] == "group"
                and old_t["tmpl"] in ("fuchsia_unittest_package", "fuchsia_test_package")
            )
        ):
            continue
        tmpl = old_t["tmpl"]
        old_deps = gn_deps(old_t, pkg)
        new_deps = new_targets[name]["deps"]
        old_all = set().union(*old_deps.values()) if old_deps else set()
        new_direct = set().union(*new_deps.values()) if new_deps else set()
        new_public = new_deps.get("public_deps", set())
        reached, adc = reach(sorted(new_direct - old_all))
        new_all = new_direct | set(reached)
        removed, added = old_all - new_all, new_all - old_all
        target_desc = f'`{tmpl}("{name}")` in //{pkg}'
        rust = tmpl.startswith("rust")

        def with_hops(labels):
            """labels plus the groups through which the target reaches them (to locate the BUILD.bazel entry)."""
            return labels + sorted({h for l in labels for h, _ in reached.get(l, ()) if h not in labels})

        for label in sorted(removed):
            dep_pkg = split(label)[0]
            kinds = [a for a in GN_DEP_ATTRS if label in old_deps.get(a, ())]
            if kinds == ["test_deps"] and label in package_now:
                continue  # Moved to another target, e.g. `with_unit_tests` replaced by an explicit test target.
            # test_deps only reach GN's generated `<name>_test` executable; if GN no longer builds it (the
            # unit test moved to Bazel), no GN link can lose the dependency's link settings.
            gn_unit_test_gone = kinds == ["test_deps"] and not any(
                re.search(r"\bwith_unit_tests\s*=\s*true\b", b) for b in new_targets[name]["bodies"]
            )
            info, exists_now = resolve(label)
            subs = sorted(l for l in added if split(l)[0] == dep_pkg)
            public = "public_deps" in kinds
            file, line = locate(pkg, with_hops(subs) or [label], new_file, new_targets[name]["line"])
            forwarded = (info["public_deps"] | info["deps"]) if info else set()
            # Pure forwarding groups expanded to their forwarded target(s) are preserved.
            if info and info["forwarding_group"] and forwarded and forwarded <= new_all:
                continue
            # When a GN fuchsia_unittest_package/fuchsia_test_package("<name>") is migrated to
            # fx_package(name = "<name>-package", package_name = "<name>", test_components = ...) +
            # fx_test(name = "<name>"), bazel2gn generates fuchsia_test_package("<name>-package")
            # (and no GN target "<name>"), so group("tests") updates its dep to the generated package.
            new_dep_pkg_targets = parsed(f"{dep_pkg}/BUILD.gn", "new")[1]
            if (
                info
                and info["tmpl"] in ("fuchsia_unittest_package", "fuchsia_test_package")
                and split(label)[1] not in new_dep_pkg_targets
                and any(
                    new_dep_pkg_targets.get(split(s)[1], {}).get("tmpl")
                    in ("fuchsia_test_package", "fuchsia_unittest_package")
                    for s in subs
                )
            ):
                continue
            if is_migrated_test_group_dep(tmpl, label, info, old_all, subs, new_all):
                continue
            if subs and exists_now:
                findings.append({
                    "source": "gn_dep_parity",
                    "category": "dep_label_substituted",
                    "severity": "ERROR",
                    "file": file,
                    "line": line,
                    "message": (
                        f"{target_desc} depended on {describe(label, info)} via `{'`/`'.join(kinds)}`, but the "
                        f"migrated target depends on {', '.join(f'`{s}`' for s in subs)} instead. That is a "
                        "different GN target of the same package: swapping a dependency for a sibling target changes "
                        "what GN compiles and links even though Bazel builds pass (e.g. dropping a wrapper whose "
                        "`public_configs` link the static C++ standard library leaves GN test executables and GN "
                        "dependents with undefined `operator new`/`operator delete`)."
                        + (
                            f" It was a `public_deps` edge, so GN also forwarded its public configs to every "
                            f"dependent of `{name}`."
                            if public
                            else ""
                        )
                    ),
                    "remediation": (
                        "Keep the exact original GN label and its public forwarding. "
                        + rust_forwarding_remedy(label, info["public_configs"], pkg)
                        if public and rust and info and info["public_configs"]
                        else f"Keep the exact original GN label: in {pkg}/BUILD.bazel depend on the Bazel target that "
                        f"holds the code and put `# @bazel2gn:path_overwrite:{label}` on that list entry, then run "
                        f"`fx bazel2gn -d {pkg}`."
                    ),
                })
            elif info and info["link"] and exists_now and not subs and not gn_unit_test_gone:
                findings.append({
                    "source": "gn_dep_parity",
                    "category": "dep_link_settings_dropped",
                    "severity": "ERROR",
                    "file": file,
                    "line": line,
                    "message": (
                        f"{target_desc} no longer depends on {describe(label, info)} (was `{'`/`'.join(kinds)}`). "
                        "Its link-affecting settings no longer reach this target"
                        + (" or its GN dependents" if public else "")
                        + ", so GN links of test executables and binaries can fail even though Bazel builds pass."
                    ),
                    "remediation": (
                        f"Restore the dependency with its exact GN label (put `# @bazel2gn:path_overwrite:{label}` on "
                        f"the matching BUILD.bazel entry if Bazel needs a different label) and run `fx bazel2gn -d {pkg}`."
                    ),
                })
            elif label not in package_now:
                findings.append({
                    "source": "gn_dep_parity",
                    "category": "dep_label_removed",
                    "severity": "INFO",
                    "file": new_file,
                    "line": new_targets[name]["line"],
                    "message": (
                        f"{target_desc} no longer depends on `{label}` (was `{'`/`'.join(kinds)}`) and no other target "
                        f"of //{pkg} does. Make sure it was really unused (e.g. an unused `test_deps` entry of a test "
                        "GN never built) and say so in the CoderReport summary."
                    ),
                })

        for label in sorted(old_deps.get("public_deps", set()) & new_all):
            if label in new_public or any(h in new_public and fwd for h, fwd in reached.get(label, ())):
                continue
            info, _ = resolve(label)
            if not info or not info["public_configs"] or info["public_configs"] <= adc:
                continue
            via = sorted({h for h, _ in reached.get(label, ())})
            file, line = locate(pkg, with_hops([label]), new_file, new_targets[name]["line"])
            findings.append({
                "source": "gn_dep_parity",
                "category": "public_dep_forwarding_lost",
                "severity": "ERROR",
                "file": file,
                "line": line,
                "message": (
                    f"`{label}` was a `public_deps` edge of {target_desc} and is now reached only through private "
                    "edges"
                    + (f" (via {', '.join(f'`{h}`' for h in via)})" if via else "")
                    + f", but it has public_configs {', '.join(f'`{c}`' for c in sorted(info['public_configs']))}. "
                    f"GN forwards those to the dependents of `{name}` only through `public_deps` (or when a dependency "
                    f"re-exports them as `all_dependent_configs`), so GN tests and binaries in other packages that use "
                    f"`{name}` lose them (e.g. link flags, giving undefined symbols; include dirs)."
                ),
                "remediation": (
                    "For C/C++ targets, list it in `deps` rather than `implementation_deps` in BUILD.bazel (bazel2gn "
                    "emits those as GN `public_deps`)."
                    if not rust
                    else rust_forwarding_remedy(label, info["public_configs"], pkg)
                ),
            })

print(json.dumps(findings, indent=2))
PYEOF
