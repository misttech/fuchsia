#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Verifies <build dir>/tests.json parity across GN-to-Bazel migrations.

Enforces:
1. True product-root parity (no synthetic per-directory `:tests` root injection
   into the primary pass, catching severed parent `group("tests")` edges).
2. Off-CQ developer leaf test discovery via `fx gn refs` upward walk to the
   highest enclosing `group(...)` root (never skipping leaf packages that lack
   a local `group("tests")`).
3. Dual-build `-gn` (`gn_dual`) vs. `gn2gone` (`case3_gn2gone`) state machine:
   - Case 1 (full GN test removal, no `## BAZEL2GN SENTINEL` after):
     `1 gn -> 1 bazel`.
   - Case 2 (dual-build, `## BAZEL2GN SENTINEL` present after):
     `1 gn -> 1 bazel + 1 gn_dual` (or both host entries) unless attested via
     `Skipped-Dual-Tests:` or kept unmigrated above the sentinel.
   - Case 3 (`gn2gone` sentinel removal, sentinel present before and removed
     after): `1 bazel + 1 gn_dual -> 1 bazel` with
     `env_set(bazel) >= env_set(gn_dual)`.
4. Attribute parity (`os`, `cpu`, `environments` dimensions/tags,
   `log_settings.max_severity`, and `build_only` `false -> true` regressions).
5. Strict 1:1 `Renamed-Tests: <old> -> <new>` bijection and 1:1 same-package
   host wrapper matching.
6. Remediation guidance that forbids coding agents from waiving dropped tests
   via `disputed_findings` or inventing `Deleted-Tests:`.
"""

import argparse
import dataclasses
import json
import os
import re
import sys
from collections.abc import Callable, Mapping, Sequence
from typing import Any

SOURCE = "tests_json_parity"
MAX_DROPPED_FINDINGS = 25

SENTINEL_RE = re.compile(
    r"^##\s*BAZEL2GN SENTINEL|#LOCAL_BAZEL_BUILD_SENTINEL", re.M
)
PKG_URL_RE = re.compile(r"^fuchsia-pkg://([^/]+)/([^#]+)#(.+)$")

PRODUCT_TARGET_ROOT = "//bundles/buildbot/core"
PRODUCT_HOST_ROOT = "//bundles/buildbot/host"
ROOTS_MARKER = (
    "# Added by planter's tests_json_parity check; it restores this file."
)

GN_DIRECT_TEST_TEMPLATES = frozenset(
    {
        "test",
        "cc_test",
        "rustc_test",
        "go_test",
        "python_host_test",
        "host_test",
        "fuchsia_unittest_package",
        "fuchsia_test_package",
    }
)
GN_RUST_UNIT_TEST_TEMPLATES = frozenset(
    {
        "rustc_library",
        "rustc_binary",
        "rustc_proc_macro",
    }
)
GN_HOST_ONLY_TEST_TEMPLATES = frozenset(
    {
        "python_host_test",
        "host_test",
    }
)

REMEDIATION = (
    "Keep every test that ran before the migration. Export each Bazel device "
    "test (`fx_test`, `fx_unittest_package`) through `target_tests` and each "
    "Bazel host test through `host_tests` of a wired `bazel_test_suite` (via "
    "the ancestor area `test_suite()` when one exists), keep dual-build GN "
    'test copies wired into `group("tests")`, and preserve device package '
    "names, component names, `environments`, `log_settings.max_severity`, and "
    "`build_only`. Coding agents MUST NEVER waive dropped tests via "
    "`disputed_findings` or emit `Deleted-Tests:`: if a test was legitimately "
    "renamed, declare `Renamed-Tests: <old> -> <new>` (strict 1:1 bijection); "
    "if a test cannot be migrated to Bazel yet, keep it in `BUILD.gn` above "
    "`## BAZEL2GN SENTINEL` (Case 2) or halt and escalate to a human."
)


@dataclasses.dataclass(frozen=True)
class PackageState:
    """Before/after build file state of a touched package directory."""

    pkg_dir: str
    before_gn: str | None = None
    after_gn: str | None = None
    before_bazel: str | None = None
    after_bazel: str | None = None


@dataclasses.dataclass
class ParsedFooters:
    """Machine-parsed test-parity footers extracted from a commit message."""

    renamed_old_to_new: dict[str, str] = dataclasses.field(default_factory=dict)
    renamed_new_to_old: dict[str, str] = dataclasses.field(default_factory=dict)
    skipped_dual_tests: set[str] = dataclasses.field(default_factory=set)
    deleted_tests: set[str] = dataclasses.field(default_factory=set)
    is_gn2gone: bool = False
    errors: list[str] = dataclasses.field(default_factory=list)


def make_finding(
    category: str,
    severity: str,
    message: str,
    remediation: str | None = None,
    file: str | None = None,
) -> dict[str, Any]:
    """Builds a structured planter check finding dictionary."""
    finding: dict[str, Any] = {
        "source": SOURCE,
        "category": category,
        "severity": severity,
        "message": message,
    }
    if remediation:
        finding["remediation"] = remediation
    if file:
        finding["file"] = file
    return finding


def normalize_label(value: str | None) -> str:
    """Normalizes `@@//a:b(//toolchain)` or `//a/b:b` to `//a:b` / `//a/b:b`."""
    cleaned = re.sub(r"^@@?(?=//)", "", (value or "").strip()).split("(", 1)[0]
    if cleaned.startswith("//") and ":" not in cleaned:
        trimmed = cleaned.rstrip("/")
        leaf = trimmed.rsplit("/", 1)[-1]
        if leaf:
            return f"{trimmed}:{leaf}"
    return cleaned


def package_of(lbl: str) -> str:
    """Returns `//pkg/dir` from `//pkg/dir:name`, or empty string."""
    if not lbl.startswith("//"):
        return ""
    return lbl.split(":", 1)[0].rstrip("/")


def normalize_environment(env: Mapping[str, Any]) -> str:
    """Normalizes one `tests.json` environment scope to canonical JSON."""
    dims_raw = env.get("dimensions") or {}
    dims = {
        str(k): str(v)
        for k, v in sorted(dims_raw.items())
        if v is not None and str(v) != ""
    }
    tags_raw = env.get("tags") or []
    tags = sorted(str(t) for t in tags_raw if t is not None and str(t) != "")
    payload: dict[str, Any] = {"dimensions": dims}
    if tags:
        payload["tags"] = tags
    return json.dumps(payload, sort_keys=True, separators=(",", ":"))


def format_env_set(envs: Sequence[str]) -> str:
    """Formats normalized environment JSON strings for human error messages."""
    if not envs:
        return "[]"
    rendered = []
    for raw in sorted(set(envs)):
        try:
            parsed = json.loads(raw)
            dims = parsed.get("dimensions") or {}
            dim_str = ", ".join(f"{k}={v}" for k, v in sorted(dims.items()))
            tags = parsed.get("tags") or []
            if tags:
                dim_str += f" [tags: {','.join(tags)}]"
            rendered.append("{" + dim_str + "}")
        except ValueError:
            rendered.append(raw)
    return "[" + ", ".join(rendered) + "]"


def normalize_test_entry(entry: Mapping[str, Any]) -> dict[str, Any]:
    """Normalizes a single `<build dir>/tests.json` entry."""
    test_obj = entry.get("test") or {}
    raw_label = (test_obj.get("label") or "").strip()
    is_bazel = raw_label.startswith("@")
    name = (test_obj.get("name") or test_obj.get("package_url") or "").strip()
    os_name = (test_obj.get("os") or "").strip()
    cpu = (test_obj.get("cpu") or "").strip()
    norm_label = normalize_label(raw_label)
    norm_source = normalize_label(test_obj.get("source_label")) or norm_label

    canonical_id = name
    variant = "bazel" if is_bazel else "gn"
    url_match = PKG_URL_RE.match(name)
    if url_match:
        repo_host, pkg_name, fragment = url_match.groups()
        if pkg_name.endswith("-gn") and not is_bazel:
            base_pkg = pkg_name[:-3]
            canonical_id = f"fuchsia-pkg://{repo_host}/{base_pkg}#{fragment}"
            variant = "gn_dual"

    log_settings = (
        test_obj.get("log_settings") or entry.get("log_settings") or {}
    )
    raw_max_sev = (log_settings.get("max_severity") or "").strip().upper()
    if os_name == "fuchsia":
        max_severity = raw_max_sev or "WARN"
    else:
        max_severity = raw_max_sev

    build_only = bool(
        entry.get("build_only", False) or test_obj.get("build_only", False)
    )
    envs_raw = entry.get("environments")
    if envs_raw is None:
        envs_raw = test_obj.get("environments") or []
    environments = sorted(
        {
            normalize_environment(env)
            for env in envs_raw
            if isinstance(env, Mapping)
        }
    )

    return {
        "name": name,
        "canonical_id": canonical_id,
        "variant": variant,
        "bazel": is_bazel,
        "label": norm_label,
        "source_label": norm_source,
        "os": os_name,
        "cpu": cpu,
        "max_severity": max_severity,
        "build_only": build_only,
        "environments": environments,
    }


def load_tests_from_entries(
    entries: Sequence[Mapping[str, Any]],
) -> list[dict[str, Any]]:
    """Loads and deduplicates normalized test records from JSON entries."""
    deduped: dict[tuple[str, str, str, str], dict[str, Any]] = {}
    for entry in entries:
        if not isinstance(entry, Mapping):
            continue
        rec = normalize_test_entry(entry)
        if not rec["name"]:
            continue
        key = (rec["os"], rec["cpu"], rec["name"], rec["variant"])
        deduped[key] = rec
    return sorted(
        deduped.values(),
        key=lambda r: (r["os"], r["cpu"], r["canonical_id"], r["variant"]),
    )


def load_tests_file(path: str) -> list[dict[str, Any]]:
    """Reads `<build dir>/tests.json` and returns normalized test records."""
    with open(path, encoding="utf-8") as fh:
        entries = json.load(fh)
    if not isinstance(entries, list):
        raise ValueError("tests.json root must be a JSON list")
    return load_tests_from_entries(entries)


def _canonical_skip_key(raw: str) -> str:
    cleaned = raw.strip()
    if cleaned.startswith(("@//", "@@//", "//")):
        return normalize_label(cleaned)
    return cleaned


def parse_commit_footers(commit_msg: str) -> ParsedFooters:
    """Parses `Renamed-Tests:` and `Skipped-Dual-Tests:` footers."""
    parsed = ParsedFooters()
    if not commit_msg:
        return parsed

    first_line = commit_msg.splitlines()[0] if commit_msg.splitlines() else ""
    if "[gn2gone]" in first_line.lower():
        parsed.is_gn2gone = True

    for line_no, raw_line in enumerate(commit_msg.splitlines(), start=1):
        if raw_line != raw_line.lstrip():
            continue
        stripped = raw_line.rstrip()
        m_ren = re.match(r"^Renamed-Tests\s*:\s*(.*)$", stripped, re.I)
        if m_ren:
            rhs = m_ren.group(1).strip()
            if "->" not in rhs:
                parsed.errors.append(
                    f"Line {line_no}: malformed `Renamed-Tests:` footer "
                    f"`{stripped}` (expected `Renamed-Tests: <old> -> <new>`)."
                )
                continue
            parts = [p.strip() for p in rhs.split("->")]
            if len(parts) != 2 or not parts[0] or not parts[1]:
                parsed.errors.append(
                    f"Line {line_no}: malformed `Renamed-Tests:` footer "
                    f"`{stripped}` (expected exactly one `->`)."
                )
                continue
            old_id, new_id = parts
            if "*" in old_id or "*" in new_id:
                parsed.errors.append(
                    f"Line {line_no}: wildcards are forbidden in "
                    f"`Renamed-Tests:` (`{stripped}`)."
                )
                continue
            norm_old = _canonical_skip_key(old_id)
            norm_new = _canonical_skip_key(new_id)
            if (
                norm_old in parsed.renamed_old_to_new
                and parsed.renamed_old_to_new[norm_old] != norm_new
            ):
                parsed.errors.append(
                    f"Line {line_no}: non-bijective `Renamed-Tests:` footer "
                    f"maps `{old_id}` to multiple targets."
                )
                continue
            if (
                norm_new in parsed.renamed_new_to_old
                and parsed.renamed_new_to_old[norm_new] != norm_old
            ):
                parsed.errors.append(
                    f"Line {line_no}: non-bijective `Renamed-Tests:` footer "
                    f"maps multiple old tests (`{parsed.renamed_new_to_old[norm_new]}` "
                    f"and `{old_id}`) to the same target `{new_id}`."
                )
                continue
            parsed.renamed_old_to_new[norm_old] = norm_new
            parsed.renamed_new_to_old[norm_new] = norm_old
            continue

        m_skip = re.match(r"^Skipped-Dual-Tests\s*:\s*(.*)$", stripped, re.I)
        if m_skip:
            for item in re.split(r"[,\s]+", m_skip.group(1).strip()):
                if item:
                    parsed.skipped_dual_tests.add(_canonical_skip_key(item))
            continue

        m_del = re.match(r"^Deleted-Tests\s*:\s*(.*)$", stripped, re.I)
        if m_del:
            for item in re.split(r"[,\s]+", m_del.group(1).strip()):
                if item:
                    parsed.deleted_tests.add(_canonical_skip_key(item))

    return parsed


def _blank_gn_comments(text: str) -> str:
    """Blanks `#` comments outside GN strings while keeping byte offsets."""
    return re.sub(
        r'("(?:\\.|[^"\\])*")|#[^\n]*',
        lambda m: m.group(1) or " " * len(m.group(0)),
        text,
    )


def _closing_brace(code: str, open_idx: int) -> int:
    depth = 0
    in_str = False
    i = open_idx
    while i < len(code):
        ch = code[i]
        if in_str:
            if ch == "\\":
                i += 2
                continue
            in_str = ch != '"'
        elif ch == '"':
            in_str = True
        elif ch == "{":
            depth += 1
        elif ch == "}":
            depth -= 1
            if depth == 0:
                return i
        i += 1
    return len(code)


def extract_gn_test_targets(
    gn_text: str | None, pkg_dir: str
) -> list[dict[str, Any]]:
    """Extracts test target declarations from a `BUILD.gn` file."""
    if not gn_text:
        return []
    clean = _blank_gn_comments(gn_text)
    pkg_clean = pkg_dir.strip("/")
    asserts_host = bool(re.search(r"\bassert\(\s*is_host\b", clean))
    targets: list[dict[str, Any]] = []
    seen: set[str] = set()
    target_re = re.compile(r'\b([A-Za-z_]\w*)\(\s*"([^"$]+)"\s*\)\s*\{')
    for match in target_re.finditer(clean):
        tmpl, name = match.group(1), match.group(2)
        body_end = _closing_brace(clean, match.end() - 1)
        body = clean[match.end() : body_end]
        if tmpl in GN_DIRECT_TEST_TEMPLATES:
            label = f"//{pkg_clean}:{name}" if pkg_clean else f"//:{name}"
            if label not in seen:
                seen.add(label)
                is_host = (
                    asserts_host
                    or tmpl in GN_HOST_ONLY_TEST_TEMPLATES
                    or bool(
                        re.search(
                            r'":' + re.escape(name) + r'\(\$host_toolchain\)"',
                            clean,
                        )
                    )
                )
                targets.append(
                    {
                        "label": label,
                        "name": name,
                        "template": tmpl,
                        "is_host": is_host,
                    }
                )
        elif tmpl in GN_RUST_UNIT_TEST_TEMPLATES:
            has_ut = bool(re.search(r"\bwith_unit_tests\s*=\s*true\b", body))
            has_host_ut = bool(
                re.search(r"\bwith_host_unit_tests\s*=\s*true\b", body)
            )
            if has_ut or has_host_ut:
                ut_name = f"{name}_test"
                label = (
                    f"//{pkg_clean}:{ut_name}" if pkg_clean else f"//:{ut_name}"
                )
                if label not in seen:
                    seen.add(label)
                    is_host = (
                        asserts_host
                        or has_host_ut
                        or bool(
                            re.search(
                                r'":'
                                + re.escape(ut_name)
                                + r'\(\$host_toolchain\)"',
                                clean,
                            )
                        )
                    )
                    targets.append(
                        {
                            "label": label,
                            "name": ut_name,
                            "template": tmpl,
                            "is_host": is_host,
                        }
                    )
    return targets


def _kept_gn_test_names_above_sentinel(gn_text: str | None) -> set[str]:
    """Returns GN test target names defined above any `## BAZEL2GN SENTINEL`."""
    if not gn_text:
        return set()
    m_sent = SENTINEL_RE.search(gn_text)
    pre = gn_text[: m_sent.start()] if m_sent else gn_text
    kept: set[str] = set()
    for item in extract_gn_test_targets(pre, ""):
        kept.add(item["name"])
    # Exclude `<pkg>-gn` dual-build wrapper packages from unmigrated GN-only set.
    clean_pre = _blank_gn_comments(pre)
    target_re = re.compile(
        r'\b(?:fuchsia_unittest_package|fuchsia_test_package)\(\s*"([^"$]+)"\s*\)\s*\{'
    )
    for match in target_re.finditer(clean_pre):
        tname = match.group(1)
        body = clean_pre[
            match.end() : _closing_brace(clean_pre, match.end() - 1)
        ]
        if tname.endswith("-gn") or re.search(
            r'\bpackage_name\s*=\s*"[^"]+-gn"', body
        ):
            kept.discard(tname)
    return kept


def detect_package_mode(
    before_gn: str | None,
    after_gn: str | None,
    after_bazel: str | None,
    is_gn2gone_commit: bool = False,
) -> str:
    """Classifies a touched package's migration mode."""
    had_sentinel = bool(before_gn and SENTINEL_RE.search(before_gn))
    has_sentinel = bool(after_gn and SENTINEL_RE.search(after_gn))
    has_bazel = bool(after_bazel and after_bazel.strip())

    if has_sentinel:
        return "case2_dual_build"
    if had_sentinel and not has_sentinel and has_bazel:
        return "case3_gn2gone"
    if is_gn2gone_commit and has_bazel and not has_sentinel:
        return "case3_gn2gone"
    if has_bazel:
        return "case1_full_removal"
    return "gn_only"


def build_product_args_gn(original_args: str) -> str:
    """Appends true CQ product roots without per-directory `:tests` injection."""
    clean = _blank_gn_comments(original_args)
    lines: list[str] = []
    for var, root_lbl in (
        ("target_labels", PRODUCT_TARGET_ROOT),
        ("host_labels", PRODUCT_HOST_ROOT),
    ):
        if f'"{root_lbl}"' in clean:
            continue
        op = "+=" if re.search(rf"^\s*{var}\s*\+?=", clean, re.M) else "="
        lines.append(f"{var} {op} {json.dumps([root_lbl])}")
    if not lines:
        return original_args
    return original_args + "\n".join([f"\n{ROOTS_MARKER}"] + lines) + "\n"


def build_scoped_args_gn(
    original_args: str,
    host_roots: Sequence[str],
    device_roots: Sequence[str],
) -> str:
    """Appends enclosing group roots for off-CQ developer tests."""
    clean = _blank_gn_comments(original_args)
    host_filtered = sorted(
        {lbl for lbl in host_roots if lbl and f'"{lbl}"' not in clean}
    )
    dev_filtered = sorted(
        {lbl for lbl in device_roots if lbl and f'"{lbl}"' not in clean}
    )
    lines: list[str] = []
    for var, labels in (
        ("host_labels", host_filtered),
        ("developer_test_labels", dev_filtered),
    ):
        if labels:
            op = "+=" if re.search(rf"^\s*{var}\s*\+?=", clean, re.M) else "="
            lines.append(f"{var} {op} {json.dumps(labels)}")
    if not lines:
        return original_args
    return original_args + "\n".join([f"\n{ROOTS_MARKER}"] + lines) + "\n"


def find_uncovered_gn_test_targets(
    gn_targets: Sequence[Mapping[str, Any]],
    baseline_tests: Sequence[Mapping[str, Any]],
) -> list[dict[str, Any]]:
    """Returns baseline touched GN test targets absent from product tests.json."""
    covered_labels: set[str] = set()
    covered_pkgs_with_device_tests: set[str] = set()
    for rec in baseline_tests:
        for field in ("label", "source_label"):
            val = rec.get(field) or ""
            if val:
                covered_labels.add(val)
                if rec.get("os") == "fuchsia":
                    covered_pkgs_with_device_tests.add(package_of(val))

    uncovered: list[dict[str, Any]] = []
    for tgt in gn_targets:
        lbl = tgt["label"]
        pkg = package_of(lbl)
        if lbl in covered_labels:
            continue
        # Inner binary (`rustc_test` / `test`) packaged into a covered device
        # test package in the same directory is already covered.
        if (
            not tgt.get("is_host")
            and tgt["template"] in ("test", "cc_test", "rustc_test", "go_test")
            and pkg in covered_pkgs_with_device_tests
        ):
            continue
        uncovered.append(dict(tgt))
    return uncovered


def _extract_gn_groups(
    gn_text: str, pkg_dir: str
) -> tuple[dict[str, set[str]], set[tuple[str, str]], set[str]]:
    """Maps `//pkg:group_name` -> deps, `(dep, group)` host edges, and `if (is_host)` groups."""
    clean = _blank_gn_comments(gn_text)
    pkg_clean = pkg_dir.strip("/")
    out: dict[str, set[str]] = {}
    host_edges: set[tuple[str, str]] = set()
    host_guarded_groups: set[str] = set()

    if_host_spans: list[tuple[int, int]] = []
    for m_if in re.finditer(r"\bif\s*\(\s*is_host\b[^{]*\{", clean):
        span_end = _closing_brace(clean, m_if.end() - 1)
        if_host_spans.append((m_if.start(), span_end))

    grp_re = re.compile(
        r'\b(?:group|standardized_test_groups)\(\s*"([^"$]+)"\s*\)\s*\{'
    )
    for match in grp_re.finditer(clean):
        gname = match.group(1)
        body = clean[match.end() : _closing_brace(clean, match.end() - 1)]
        glabel = f"//{pkg_clean}:{gname}" if pkg_clean else f"//:{gname}"
        if any(s <= match.start() < e for s, e in if_host_spans):
            host_guarded_groups.add(glabel)
        refs = out.setdefault(glabel, set())
        for raw_dep in re.findall(
            r'"([^"$\n]+(?:\(\$host_toolchain\))?)"', body
        ):
            is_host_dep = "($host_toolchain)" in raw_dep or (
                "(" in raw_dep and "host" in raw_dep.split("(", 1)[1]
            )
            dep_no_tc = raw_dep.split("(", 1)[0].strip()
            norm_dep = ""
            if dep_no_tc.startswith(":"):
                norm_dep = (
                    f"//{pkg_clean}{dep_no_tc}"
                    if pkg_clean
                    else f"//{dep_no_tc}"
                )
            elif dep_no_tc.startswith("//"):
                norm_dep = normalize_label(dep_no_tc)
            elif "/" in dep_no_tc or ":" in dep_no_tc:
                joined = f"{pkg_clean}/{dep_no_tc}".strip("/")
                norm_dep = normalize_label(f"//{joined}")
            if norm_dep:
                refs.add(norm_dep)
                if is_host_dep:
                    host_edges.add((norm_dep, glabel))
    return out, host_edges, host_guarded_groups


def resolve_highest_enclosing_groups(
    uncovered_targets: Sequence[Mapping[str, Any]],
    gn_refs_fn: Callable[[str], Sequence[str]] | None,
    gn_files_by_pkg: Mapping[str, str],
    read_gn_fn: Callable[[str], str | None] | None = None,
) -> tuple[list[str], list[str]]:
    """Walks `fx gn refs` (with static BUILD.gn fallback) to top `group(...)`."""
    reverse_groups: dict[str, set[str]] = {}
    host_edges: set[tuple[str, str]] = set()
    known_groups: set[str] = set()
    host_only_pkgs: set[str] = set()
    host_guarded_groups: set[str] = set()
    indexed_pkgs: set[str] = set()

    def _index_pkg(pkg_dir: str, gn_text: str | None) -> None:
        clean_dir = pkg_dir.strip("/")
        if clean_dir in indexed_pkgs:
            return
        indexed_pkgs.add(clean_dir)
        if not gn_text:
            return
        if re.search(r"\bassert\(\s*is_host\b", _blank_gn_comments(gn_text)):
            host_only_pkgs.add(f"//{clean_dir}" if clean_dir else "//")
        groups_map, pkg_host_edges, pkg_host_groups = _extract_gn_groups(
            gn_text, clean_dir
        )
        host_edges.update(pkg_host_edges)
        host_guarded_groups.update(pkg_host_groups)
        for glabel, deps in groups_map.items():
            known_groups.add(glabel)
            for dep in deps:
                reverse_groups.setdefault(dep, set()).add(glabel)

    for pkg_dir, gn_text in gn_files_by_pkg.items():
        _index_pkg(pkg_dir, gn_text)

    def _ensure_pkg_indexed(lbl: str) -> None:
        if read_gn_fn is None:
            return
        pkg = package_of(lbl)
        if not pkg:
            return
        rel_dir = pkg[2:]
        if rel_dir not in indexed_pkgs:
            _index_pkg(rel_dir, read_gn_fn(rel_dir))

    host_roots: set[str] = set()
    device_roots: set[str] = set()

    for tgt in uncovered_targets:
        start_lbl = normalize_label(tgt["label"])
        _ensure_pkg_indexed(start_lbl)
        start_is_host = bool(tgt.get("is_host"))
        visited: set[tuple[str, bool]] = set()
        frontier: list[tuple[str, bool]] = [(start_lbl, start_is_host)]
        top_groups: set[tuple[str, bool]] = set()

        while frontier and len(visited) < 500:
            cur, cur_is_host = frontier.pop(0)
            if (cur, cur_is_host) in visited:
                continue
            visited.add((cur, cur_is_host))

            parents_with_host: dict[str, bool] = {}
            if gn_refs_fn is not None:
                for raw_ref in gn_refs_fn(cur):
                    ref_has_host_tc = (
                        "(" in raw_ref and "host" in raw_ref.split("(", 1)[1]
                    )
                    norm_ref = normalize_label(raw_ref)
                    if norm_ref and norm_ref != cur:
                        _ensure_pkg_indexed(norm_ref)
                        edge_host = (
                            cur_is_host
                            or ref_has_host_tc
                            or (cur, norm_ref) in host_edges
                        )
                        parents_with_host[norm_ref] = (
                            parents_with_host.get(norm_ref, False) or edge_host
                        )
            for grp_parent in reverse_groups.get(cur, set()):
                edge_host = cur_is_host or (cur, grp_parent) in host_edges
                parents_with_host[grp_parent] = (
                    parents_with_host.get(grp_parent, False) or edge_host
                )

            next_steps = [
                (p, p_host)
                for p, p_host in sorted(parents_with_host.items())
                if (
                    p in known_groups
                    or (gn_refs_fn is not None and (p, p_host) not in visited)
                )
            ]
            if not next_steps:
                if cur in known_groups and cur != start_lbl:
                    top_groups.add((cur, cur_is_host))
                continue
            for parent, p_host in next_steps:
                if (parent, p_host) not in visited:
                    frontier.append((parent, p_host))
                elif parent in known_groups:
                    top_groups.add((parent, p_host))

        if not top_groups:
            # If the leaf has no enclosing group anywhere, root at the leaf's
            # own `:tests` group if defined, otherwise at the target itself.
            pkg = package_of(start_lbl)
            local_tests = f"{pkg}:tests" if pkg else "//:tests"
            chosen = local_tests if local_tests in known_groups else start_lbl
            top_groups.add((chosen, start_is_host))

        for root_lbl, root_is_host in top_groups:
            root_pkg = package_of(root_lbl)
            # In Fuchsia's //BUILD.gn, `host_labels` attaches `($host_toolchain)`
            # while `developer_test_labels` evaluates in `$default_toolchain`
            # (which is required for `bazel_test_suite` and also follows
            # `($host_toolchain)` edges inside `$default_toolchain` groups).
            # Put roots in `host_labels` when the root package asserts `is_host`,
            # the group is inside `if (is_host)`, or the root is a bare host
            # test target with no enclosing group.
            is_bare_host_target = root_lbl not in known_groups and root_is_host
            if (
                root_pkg in host_only_pkgs
                or root_lbl in host_guarded_groups
                or is_bare_host_target
            ):
                host_roots.add(root_lbl)
            else:
                device_roots.add(root_lbl)

    return sorted(host_roots), sorted(device_roots)


def host_pkg_stems(rec: Mapping[str, Any]) -> set[tuple[str, str, str, str]]:
    """Computes normalized `(os, cpu, pkg, stem)` keys for a host test."""
    if rec["os"] == "fuchsia":
        return set()
    pkg = package_of(rec["source_label"] or rec["label"])
    if not pkg:
        return set()
    raw_tokens: list[str] = []
    name = rec["name"]
    if name.startswith("//"):
        if ":" in name:
            raw_tokens.append(name.split(":", 1)[1])
    elif name:
        raw_tokens.append(os.path.basename(name))
    for lbl in (rec["label"], rec["source_label"]):
        if lbl and ":" in lbl:
            raw_tokens.append(lbl.split(":", 1)[1])

    stems: set[tuple[str, str, str, str]] = set()
    suffixes = (
        "_lib_test",
        "_bin_test",
        "_instrumented_test",
        "_host_unittests",
        "_host_unittest",
        "_host_tests",
        "_host_test",
        "_unittest",
        "_unittests",
        "_tests",
        "_test",
    )
    generic = {
        "lib",
        "bin",
        "host",
        "test",
        "tests",
        "unit",
        "unittest",
        "unittests",
    }
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
                stems.add((rec["os"], rec["cpu"], pkg, pkg_stem))
        elif stem:
            stems.add((rec["os"], rec["cpu"], pkg, stem))
            for mid in ("_lib", "_bin", "_host"):
                if stem.endswith(mid) and len(stem) > len(mid):
                    stems.add((rec["os"], rec["cpu"], pkg, stem[: -len(mid)]))
    return stems


def _record_matches_rename_token(rec: Mapping[str, Any], token: str) -> bool:
    """Checks if `rec` matches a `Renamed-Tests:` key/target token."""
    norm_tok = _canonical_skip_key(token)
    if norm_tok in (
        rec["name"],
        rec["canonical_id"],
        rec["label"],
        rec["source_label"],
    ):
        return True
    if not norm_tok.startswith("//") and "/" not in norm_tok:
        for lbl in (rec["label"], rec["source_label"]):
            if lbl.endswith(f":{norm_tok}"):
                return True
    return False


def _is_skipped_dual_test(
    rec: Mapping[str, Any], skipped_dual_tests: set[str]
) -> bool:
    """Checks whether `rec` is covered by `Skipped-Dual-Tests:`."""
    if not skipped_dual_tests:
        return False
    candidates = {
        rec["name"],
        rec["canonical_id"],
        rec["label"],
        rec["source_label"],
    }
    url_match = PKG_URL_RE.match(rec["canonical_id"])
    if url_match:
        pkg_name = url_match.group(2)
        pkg_dir = package_of(rec["source_label"] or rec["label"])
        if pkg_dir:
            candidates.add(f"{pkg_dir}:{pkg_name}")
    return bool(candidates & skipped_dual_tests)


def _best_Build_file(pkg: str, prefer_bazel: bool, workdir: str) -> str | None:
    if not pkg or not pkg.startswith("//") or not workdir:
        return None
    rel_dir = pkg[2:]
    order = (
        ("BUILD.bazel", "BUILD.gn")
        if prefer_bazel
        else ("BUILD.gn", "BUILD.bazel")
    )
    for bname in order:
        cand = os.path.join(rel_dir, bname)
        if os.path.isfile(os.path.join(workdir, cand)):
            return cand
    return None


def _check_attribute_parity(
    before_rec: Mapping[str, Any],
    after_rec: Mapping[str, Any],
    change_base: str,
    workdir: str,
) -> list[dict[str, Any]]:
    """Checks `cpu`, `build_only`, `environments`, and `max_severity` parity."""
    errs: list[dict[str, Any]] = []
    pkg = package_of(
        after_rec["source_label"]
        or after_rec["label"]
        or before_rec["source_label"]
        or before_rec["label"]
    )
    bfile = _best_Build_file(pkg, bool(after_rec.get("bazel")), workdir)
    test_disp = after_rec["name"] or before_rec["name"]

    if before_rec["os"] != after_rec["os"]:
        errs.append(
            make_finding(
                "test_os_mismatch",
                "ERROR",
                f"Test `{test_disp}` changed OS from `{before_rec['os']}` at "
                f"{change_base} to `{after_rec['os']}` after the change.",
                remediation=REMEDIATION,
                file=bfile,
            )
        )

    if before_rec["cpu"] != after_rec["cpu"]:
        errs.append(
            make_finding(
                "test_cpu_mismatch",
                "ERROR",
                f"Test `{test_disp}` changed CPU architecture from "
                f"`{before_rec['cpu']}` at {change_base} to "
                f"`{after_rec['cpu']}` after the change.",
                remediation=REMEDIATION,
                file=bfile,
            )
        )

    if not before_rec["build_only"] and after_rec["build_only"]:
        errs.append(
            make_finding(
                "test_became_build_only",
                "ERROR",
                f"Test `{test_disp}` (`{after_rec['source_label']}`) flipped "
                f"`build_only` from `false` at {change_base} to `true` after "
                "the change, disabling CQ test execution.",
                remediation=REMEDIATION,
                file=bfile,
            )
        )

    if not after_rec["build_only"]:
        before_envs = set(before_rec["environments"])
        after_envs = set(after_rec["environments"])
        lost_envs = sorted(before_envs - after_envs)
        if lost_envs:
            errs.append(
                make_finding(
                    "test_environments_lost",
                    "ERROR",
                    f"Test `{test_disp}` (`{after_rec['source_label']}`) lost "
                    f"execution environment(s) {format_env_set(lost_envs)} "
                    f"present at {change_base} (now has "
                    f"{format_env_set(sorted(after_envs))}).",
                    remediation=REMEDIATION,
                    file=bfile,
                )
            )

    if before_rec["os"] == "fuchsia" and after_rec["os"] == "fuchsia":
        if before_rec["max_severity"] != after_rec["max_severity"]:
            errs.append(
                make_finding(
                    "test_max_severity_changed",
                    "ERROR",
                    f"Test `{test_disp}` (`{after_rec['source_label']}`) "
                    f"changed `log_settings.max_severity` from "
                    f"`{before_rec['max_severity']}` at {change_base} to "
                    f"`{after_rec['max_severity']}` after the change.",
                    remediation=REMEDIATION,
                    file=bfile,
                )
            )

    return errs


def verify_parity(
    before_tests: Sequence[Mapping[str, Any]],
    after_tests: Sequence[Mapping[str, Any]],
    package_states: Mapping[str, PackageState] | None = None,
    commit_msg: str = "",
    change_base: str = "HEAD",
    workdir: str = "",
) -> list[dict[str, Any]]:
    """Compares normalized baseline and post-change `tests.json` records."""
    before = [
        normalize_test_entry(t) if "canonical_id" not in t else dict(t)
        for t in before_tests
    ]
    after = [
        normalize_test_entry(t) if "canonical_id" not in t else dict(t)
        for t in after_tests
    ]
    pkg_states = dict(package_states or {})
    footers = parse_commit_footers(commit_msg)

    findings: list[dict[str, Any]] = []
    for err in footers.errors:
        findings.append(
            make_finding(
                "invalid_renamed_tests_footer",
                "ERROR",
                err,
                remediation=REMEDIATION,
                file="/COMMIT_MSG",
            )
        )

    pkg_modes: dict[str, str] = {}
    kept_gn_by_pkg: dict[str, set[str]] = {}
    for pkg_key, pstate in pkg_states.items():
        norm_pkg = (
            pkg_key if pkg_key.startswith("//") else f"//{pkg_key.strip('/')}"
        )
        pkg_modes[norm_pkg] = detect_package_mode(
            pstate.before_gn,
            pstate.after_gn,
            pstate.after_bazel,
            is_gn2gone_commit=footers.is_gn2gone,
        )
        kept_gn_by_pkg[norm_pkg] = _kept_gn_test_names_above_sentinel(
            pstate.after_gn
        )

    # Validate explicit `Renamed-Tests: <old> -> <new>` mappings (strict 1:1).
    explicit_rename_map: dict[int, list[dict[str, Any]]] = {}
    claimed_after_ids: set[int] = set()
    for old_tok, new_tok in footers.renamed_old_to_new.items():
        old_matches = [
            b for b in before if _record_matches_rename_token(b, old_tok)
        ]
        new_matches = [
            a for a in after if _record_matches_rename_token(a, new_tok)
        ]
        if not old_matches:
            findings.append(
                make_finding(
                    "unmatched_renamed_tests_footer",
                    "ERROR",
                    f"`Renamed-Tests: {old_tok} -> {new_tok}` names `{old_tok}`, "
                    f"which does not exist in `<build dir>/tests.json` at "
                    f"{change_base}.",
                    remediation=REMEDIATION,
                    file="/COMMIT_MSG",
                )
            )
            continue
        if not new_matches:
            findings.append(
                make_finding(
                    "unmatched_renamed_tests_footer",
                    "ERROR",
                    f"`Renamed-Tests: {old_tok} -> {new_tok}` names `{new_tok}`, "
                    "which does not exist in `<build dir>/tests.json` after "
                    "the change.",
                    remediation=REMEDIATION,
                    file="/COMMIT_MSG",
                )
            )
            continue
        old_oses = {m["os"] == "fuchsia" for m in old_matches}
        new_oses = {m["os"] == "fuchsia" for m in new_matches}
        if old_oses != new_oses:
            findings.append(
                make_finding(
                    "renamed_tests_polarity_mismatch",
                    "ERROR",
                    f"`Renamed-Tests: {old_tok} -> {new_tok}` mismatches "
                    "device (`fuchsia`) vs. host test polarity.",
                    remediation=REMEDIATION,
                    file="/COMMIT_MSG",
                )
            )
            continue
        # Group multiple variants of the same logical new test (e.g. Case 2
        # `bazel` + `gn_dual` of the renamed package) for the old logical test.
        new_canon_ids = {m["canonical_id"] for m in new_matches}
        old_canon_ids = {m["canonical_id"] for m in old_matches}
        if len(old_canon_ids) > 1 or len(new_canon_ids) > 1:
            findings.append(
                make_finding(
                    "non_bijective_renamed_tests_footer",
                    "ERROR",
                    f"`Renamed-Tests: {old_tok} -> {new_tok}` matches "
                    f"{len(old_canon_ids)} baseline test(s) and "
                    f"{len(new_canon_ids)} post-change test(s); "
                    "`Renamed-Tests:` must be a strict 1:1 bijection.",
                    remediation=REMEDIATION,
                    file="/COMMIT_MSG",
                )
            )
            continue
        for b_rec in old_matches:
            explicit_rename_map[id(b_rec)] = new_matches
        for a_rec in new_matches:
            claimed_after_ids.add(id(a_rec))

    # Group device tests by `(os, cpu, canonical_id)` and host tests via 1:1
    # matching per variant.
    after_device_by_canon: dict[tuple[str, str, str], list[dict[str, Any]]] = {}
    after_host_pool: list[dict[str, Any]] = []
    for a_rec in after:
        if a_rec["os"] == "fuchsia":
            after_device_by_canon.setdefault(
                (a_rec["os"], a_rec["cpu"], a_rec["canonical_id"]), []
            ).append(a_rec)
        else:
            after_host_pool.append(a_rec)

    before_device_groups: dict[tuple[str, str, str], list[dict[str, Any]]] = {}
    before_host_list: list[dict[str, Any]] = []
    for b_rec in before:
        if b_rec["os"] == "fuchsia":
            before_device_groups.setdefault(
                (b_rec["os"], b_rec["cpu"], b_rec["canonical_id"]), []
            ).append(b_rec)
        else:
            before_host_list.append(b_rec)

    dropped_records: list[dict[str, Any]] = []
    matched_after_ids: set[int] = set(claimed_after_ids)

    # 1. Verify all Fuchsia device test groups.
    for (os_name, cpu, canon_id), b_group in sorted(
        before_device_groups.items()
    ):
        if id(b_group[0]) in explicit_rename_map:
            a_group = explicit_rename_map[id(b_group[0])]
        else:
            a_group = after_device_by_canon.get((os_name, cpu, canon_id), [])
            if not a_group:
                # Check if `cpu` changed for the same `canonical_id` so we emit
                # an explicit `test_cpu_mismatch` error.
                other_cpu = [
                    r
                    for (o, c, cid), grp in after_device_by_canon.items()
                    if o == os_name and cid == canon_id and c != cpu
                    for r in grp
                ]
                if other_cpu:
                    for oc in other_cpu:
                        matched_after_ids.add(id(oc))
                    findings.extend(
                        _check_attribute_parity(
                            b_group[0], other_cpu[0], change_base, workdir
                        )
                    )
                    continue

        if not a_group:
            dropped_records.append(b_group[0])
            continue

        for a_rec in a_group:
            matched_after_ids.add(id(a_rec))

        b_by_var = {r["variant"]: r for r in b_group}
        a_by_var = {r["variant"]: r for r in a_group}

        pkg = package_of(
            b_group[0]["source_label"]
            or b_group[0]["label"]
            or a_group[0]["source_label"]
            or a_group[0]["label"]
        )
        mode = pkg_modes.get(pkg, "untouched")
        bfile = _best_Build_file(pkg, "bazel" in a_by_var, workdir)
        kept_gn_names = kept_gn_by_pkg.get(pkg, set())
        src_target_name = (
            (b_group[0]["source_label"] or "").split(":", 1)[1]
            if ":" in (b_group[0]["source_label"] or "")
            else ""
        )
        is_kept_gn_only = (
            src_target_name in kept_gn_names
            and "gn" in a_by_var
            and "bazel" not in a_by_var
        )

        if mode == "case1_full_removal":
            if "bazel" not in a_by_var and not is_kept_gn_only:
                findings.append(
                    make_finding(
                        "test_dropped_from_tests_json",
                        "ERROR",
                        f"Device test `{canon_id}` in Case 1 package `{pkg}` "
                        "has no exported Bazel variant (`@//...`) in "
                        "`<build dir>/tests.json` after the change.",
                        remediation=REMEDIATION,
                        file=bfile,
                    )
                )
        elif mode == "case2_dual_build":
            if is_kept_gn_only:
                pass
            else:
                if "bazel" not in a_by_var:
                    findings.append(
                        make_finding(
                            "dual_build_bazel_variant_missing",
                            "ERROR",
                            f"Dual-build package `{pkg}` keeps only the GN "
                            f"variant of `{canon_id}` without exporting the "
                            "Bazel variant through `bazel_test_suite`.",
                            remediation=REMEDIATION,
                            file=bfile,
                        )
                    )
                if "gn_dual" not in a_by_var and not any(
                    _is_skipped_dual_test(r, footers.skipped_dual_tests)
                    for r in (b_group + a_group)
                ):
                    findings.append(
                        make_finding(
                            "dual_build_gn_variant_dropped",
                            "ERROR",
                            f"Dual-build package `{pkg}` (`## BAZEL2GN "
                            f"SENTINEL` present) dropped the `-gn` GN test "
                            f"copy of `{canon_id}` from `<build dir>/tests.json` "
                            "without `Skipped-Dual-Tests:` attestation.",
                            remediation=REMEDIATION,
                            file=bfile,
                        )
                    )
        elif mode == "case3_gn2gone":
            if "bazel" not in a_by_var:
                findings.append(
                    make_finding(
                        "gn2gone_bazel_variant_missing",
                        "ERROR",
                        f"`gn2gone` sentinel removal in `{pkg}` removed the "
                        f"`-gn` variant of `{canon_id}` without keeping an "
                        "exported Bazel variant (`@//...`) in "
                        "`<build dir>/tests.json`.",
                        remediation=REMEDIATION,
                        file=bfile,
                    )
                )
            elif "gn_dual" in b_by_var:
                findings.extend(
                    _check_attribute_parity(
                        b_by_var["gn_dual"],
                        a_by_var["bazel"],
                        change_base,
                        workdir,
                    )
                )

        # Attribute parity check between baseline primary record and every
        # surviving post-change variant.
        primary_before = (
            b_by_var.get("bazel")
            or b_by_var.get("gn")
            or b_by_var.get("gn_dual")
            or b_group[0]
        )
        for a_rec in a_group:
            findings.extend(
                _check_attribute_parity(
                    primary_before, a_rec, change_base, workdir
                )
            )

    # 2. Verify all host tests with strict 1:1 matching per variant!
    # Group baseline host tests by logical identity `(os, cpu, pkg, logical_key)`
    # so dual-built baseline (`1 bazel + 1 gn`) is recognized cleanly.
    used_after_bazel_host: set[int] = set()
    used_after_gn_host: set[int] = set()

    def _claim_host_match(
        b_rec: Mapping[str, Any], want_variant: str
    ) -> dict[str, Any] | None:
        used_set = (
            used_after_bazel_host
            if want_variant == "bazel"
            else used_after_gn_host
        )
        pool = [
            a
            for a in after_host_pool
            if a["variant"] == want_variant and id(a) not in used_set
        ]
        # Tier 1: exact `(os, cpu, name)`
        for cand in pool:
            if (cand["os"], cand["cpu"], cand["name"]) == (
                b_rec["os"],
                b_rec["cpu"],
                b_rec["name"],
            ):
                used_set.add(id(cand))
                matched_after_ids.add(id(cand))
                return cand
        # Tier 2: exact `(os, cpu, source_label or label)`
        b_lbls = {lbl for lbl in (b_rec["label"], b_rec["source_label"]) if lbl}
        for cand in pool:
            c_lbls = {
                lbl for lbl in (cand["label"], cand["source_label"]) if lbl
            }
            if (
                cand["os"] == b_rec["os"]
                and cand["cpu"] == b_rec["cpu"]
                and (b_lbls & c_lbls)
            ):
                used_set.add(id(cand))
                matched_after_ids.add(id(cand))
                return cand
        # Tier 3: same-package 1:1 host wrapper stem (e.g. `//dir:foo_test` ->
        # `//dir:foo_host_test`), preferring attribute-compatible candidates.
        b_stems = host_pkg_stems(b_rec)
        if b_stems:
            stem_cands = [
                cand for cand in pool if b_stems & host_pkg_stems(cand)
            ]
            # Prefer exact env + build_only match first.
            stem_cands.sort(
                key=lambda c: (
                    c["build_only"] != b_rec["build_only"],
                    set(b_rec["environments"]) - set(c["environments"])
                    != set(),
                )
            )
            if stem_cands:
                chosen = stem_cands[0]
                used_set.add(id(chosen))
                matched_after_ids.add(id(chosen))
                return chosen
        return None

    # Pair baseline `bazel` + `gn` host entries of the same logical test so
    # Case 2 / Case 3 transitions check both variants together.
    paired_before_host_ids: set[int] = set()
    logical_host_groups: list[list[dict[str, Any]]] = []
    for i, b_rec in enumerate(before_host_list):
        if id(b_rec) in paired_before_host_ids:
            continue
        paired_before_host_ids.add(id(b_rec))
        grp = [b_rec]
        b_stems = host_pkg_stems(b_rec)
        b_lbls = {l for l in (b_rec["label"], b_rec["source_label"]) if l}
        for other in before_host_list[i + 1 :]:
            if (
                id(other) in paired_before_host_ids
                or other["variant"] == b_rec["variant"]
                or other["os"] != b_rec["os"]
                or other["cpu"] != b_rec["cpu"]
            ):
                continue
            o_lbls = {l for l in (other["label"], other["source_label"]) if l}
            if (b_lbls & o_lbls) or (
                b_stems and (b_stems & host_pkg_stems(other))
            ):
                paired_before_host_ids.add(id(other))
                grp.append(other)
                break
        logical_host_groups.append(grp)

    for b_grp in logical_host_groups:
        primary_b = b_grp[0]
        if id(primary_b) in explicit_rename_map:
            a_matches = explicit_rename_map[id(primary_b)]
            for a_rec in a_matches:
                if a_rec["variant"] == "bazel":
                    used_after_bazel_host.add(id(a_rec))
                else:
                    used_after_gn_host.add(id(a_rec))
        else:
            a_matches = []
            m_bz = _claim_host_match(primary_b, "bazel")
            if m_bz is not None:
                a_matches.append(m_bz)
            m_gn = _claim_host_match(primary_b, "gn")
            if m_gn is not None:
                a_matches.append(m_gn)

        if not a_matches:
            dropped_records.append(primary_b)
            continue

        a_by_var = {r["variant"]: r for r in a_matches}
        b_by_var = {r["variant"]: r for r in b_grp}
        pkg = package_of(primary_b["source_label"] or primary_b["label"])
        mode = pkg_modes.get(pkg, "untouched")
        bfile = _best_Build_file(pkg, "bazel" in a_by_var, workdir)
        kept_gn_names = kept_gn_by_pkg.get(pkg, set())
        src_name = (
            (primary_b["source_label"] or "").split(":", 1)[1]
            if ":" in (primary_b["source_label"] or "")
            else ""
        )
        is_kept_gn_only = (
            src_name in kept_gn_names
            and "gn" in a_by_var
            and "bazel" not in a_by_var
        )

        if mode == "case1_full_removal":
            if "bazel" not in a_by_var and not is_kept_gn_only:
                findings.append(
                    make_finding(
                        "test_dropped_from_tests_json",
                        "ERROR",
                        f"Host test `{primary_b['name']}` (`{primary_b['source_label']}`) "
                        f"in Case 1 package `{pkg}` has no exported Bazel "
                        "host test in `<build dir>/tests.json` after the change.",
                        remediation=REMEDIATION,
                        file=bfile,
                    )
                )
        elif mode == "case2_dual_build":
            if not is_kept_gn_only:
                if "bazel" not in a_by_var:
                    findings.append(
                        make_finding(
                            "dual_build_bazel_variant_missing",
                            "ERROR",
                            f"Dual-build package `{pkg}` keeps only the GN "
                            f"host test `{primary_b['name']}` without "
                            "exporting the Bazel host test in `bazel_test_suite`.",
                            remediation=REMEDIATION,
                            file=bfile,
                        )
                    )
                if "gn" not in a_by_var and not any(
                    _is_skipped_dual_test(r, footers.skipped_dual_tests)
                    for r in (b_grp + a_matches)
                ):
                    findings.append(
                        make_finding(
                            "dual_build_gn_variant_dropped",
                            "ERROR",
                            f"Dual-build package `{pkg}` (`## BAZEL2GN "
                            "SENTINEL` present) dropped the GN host test copy "
                            f"of `{primary_b['name']}` (`{primary_b['source_label']}`) "
                            "without `Skipped-Dual-Tests:` attestation.",
                            remediation=REMEDIATION,
                            file=bfile,
                        )
                    )
        elif mode == "case3_gn2gone":
            if "bazel" not in a_by_var:
                findings.append(
                    make_finding(
                        "gn2gone_bazel_variant_missing",
                        "ERROR",
                        f"`gn2gone` sentinel removal in `{pkg}` dropped the "
                        f"GN host test `{primary_b['name']}` without keeping "
                        "an exported Bazel host test in `<build dir>/tests.json`.",
                        remediation=REMEDIATION,
                        file=bfile,
                    )
                )
            elif "gn" in b_by_var:
                findings.extend(
                    _check_attribute_parity(
                        b_by_var["gn"],
                        a_by_var["bazel"],
                        change_base,
                        workdir,
                    )
                )

        for a_rec in a_matches:
            findings.extend(
                _check_attribute_parity(primary_b, a_rec, change_base, workdir)
            )

    added_records = [a for a in after if id(a) not in matched_after_ids]

    for rec in dropped_records[:MAX_DROPPED_FINDINGS]:
        pkg = package_of(rec["source_label"] or rec["label"])
        bfile = _best_Build_file(pkg, bool(rec.get("bazel")), workdir)
        msg = (
            f"Test `{rec['name']}` ({rec['os']}/{rec['cpu']}, "
            f"`{rec['source_label'] or rec['label']}`) is in "
            f"`<build dir>/tests.json` at {change_base} but not after the "
            "change, so neither `fx test` nor infra runs it anymore."
        )
        same_pkg = [
            a
            for a in added_records
            if a["os"] == rec["os"]
            and pkg
            and package_of(a["source_label"] or a["label"]) == pkg
        ]
        if same_pkg:
            shown = ", ".join(f"`{a['name']}`" for a in same_pkg[:5])
            extra = (
                "" if len(same_pkg) <= 5 else f" and {len(same_pkg) - 5} more"
            )
            msg += (
                f" Tests the change added in the same package: {shown}{extra}."
            )
        findings.append(
            make_finding(
                "test_dropped_from_tests_json",
                "ERROR",
                msg,
                remediation=REMEDIATION,
                file=bfile,
            )
        )

    if len(dropped_records) > MAX_DROPPED_FINDINGS:
        rest = dropped_records[MAX_DROPPED_FINDINGS:]
        rest_names = ", ".join(f"`{t['name']}`" for t in rest[:50])
        suffix = "" if len(rest) <= 50 else ", ..."
        findings.append(
            make_finding(
                "test_dropped_from_tests_json",
                "ERROR",
                f"{len(rest)} more tests from `<build dir>/tests.json` at "
                f"{change_base} are missing after the change: "
                f"{rest_names}{suffix}",
                remediation=REMEDIATION,
            )
        )

    return findings


def main(argv: Sequence[str] | None = None) -> int:
    """CLI entry point for standalone `tests.json` parity verification."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--before", required=True, help="Path to baseline tests.json."
    )
    parser.add_argument(
        "--after", required=True, help="Path to post-change tests.json."
    )
    parser.add_argument(
        "--package-states-json",
        default="",
        help="Optional JSON file mapping pkg_dir -> {before_gn, after_gn, after_bazel}.",
    )
    parser.add_argument(
        "--commit-msg-file",
        default="",
        help="Optional file containing commit message for footer parsing.",
    )
    parser.add_argument(
        "--change-base",
        default="HEAD",
        help="Change base label for diagnostics.",
    )
    parser.add_argument(
        "--workdir",
        default="",
        help="Optional Fuchsia checkout root for resolving BUILD file paths.",
    )
    args = parser.parse_args(argv)

    before_tests = load_tests_file(args.before)
    after_tests = load_tests_file(args.after)

    pkg_states: dict[str, PackageState] = {}
    if args.package_states_json:
        with open(args.package_states_json, encoding="utf-8") as fh:
            raw_states = json.load(fh)
        for pkg_dir, item in raw_states.items():
            pkg_states[pkg_dir] = PackageState(
                pkg_dir=pkg_dir,
                before_gn=item.get("before_gn"),
                after_gn=item.get("after_gn"),
                before_bazel=item.get("before_bazel"),
                after_bazel=item.get("after_bazel"),
            )

    commit_msg = ""
    if args.commit_msg_file and os.path.isfile(args.commit_msg_file):
        with open(
            args.commit_msg_file, encoding="utf-8", errors="replace"
        ) as fh:
            commit_msg = fh.read()

    findings = verify_parity(
        before_tests=before_tests,
        after_tests=after_tests,
        package_states=pkg_states,
        commit_msg=commit_msg,
        change_base=args.change_base,
        workdir=args.workdir,
    )
    if not findings:
        findings.append(
            make_finding(
                "tests_json_parity",
                "INFO",
                f"All {len(before_tests)} baseline test entries in "
                f"`<build dir>/tests.json` at {args.change_base} satisfy "
                "migration parity.",
            )
        )
    print(json.dumps(findings, indent=2))
    return 1 if any(f["severity"] == "ERROR" for f in findings) else 0


if __name__ == "__main__":
    sys.exit(main())
