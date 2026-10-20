#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Checks the migration commit message:
#   subject: [bazel_migration][<subsystem>] //<migrated/dir>
#            or [bazel_migration] //<migrated/dir> when the directory's git log
#            shows no established subsystem tag (there is no parent subsystem).
#            A target dir build/secondary/<dir> (GN-only overlay) migrates to
#            //<dir> (gn_secondary_tree). A change that modifies no file versus
#            its parent migrates nothing: INFO instead of subject findings.
#   Test: footers: `fx bazel` commands need --config=..., Bazel labels start
#            with @//, no redundant //a/b:b labels, shac/bazel2gn verification is
#            the single line `Test: fx host-tool shac check`, listed last.
#            TODO(https://fxbug.dev/538640644): Replace with fx lint once it runs these shac checks.
# Only runs when HEAD is the task's own commit (PLANTER_CHANGE_BASE != HEAD).

WORKDIR="${PLANTER_WORKDIR:-.}"
TARGET_DIR="${PLANTER_TARGET_DIR:-}"
BASE="${PLANTER_CHANGE_BASE:-HEAD}"

python3 - "$WORKDIR" "$TARGET_DIR" "$BASE" <<'PYEOF'
import collections, json, os, re, subprocess, sys

workdir, target_dir, base = sys.argv[1], sys.argv[2], sys.argv[3]

def git(*args):
    try:
        r = subprocess.run(["git", "-C", workdir, *args], capture_output=True, text=True, timeout=30)
        return r.stdout.strip() if r.returncode == 0 else ""
    except Exception:
        return ""

# Resolve a git revision or jj revset (such as @ or @- in a jj colocated checkout) to a git commit SHA.
def resolve_rev(rev):
    if not rev.startswith("@"):
        sha = git("rev-parse", "--verify", f"{rev}^{{commit}}")
        if sha:
            return sha
    try:
        r = subprocess.run(
            ["jj", "--no-pager", "--ignore-working-copy", "-R", workdir, "log", "--no-graph", "-r", rev, "-T", "commit_id"],
            capture_output=True, text=True, timeout=30,
        )
        if r.returncode == 0 and r.stdout.strip():
            return git("rev-parse", "--verify", f"{r.stdout.strip()}^{{commit}}")
    except Exception:
        pass
    sha = git("rev-parse", "--verify", f"{rev}^{{commit}}")
    if sha:
        return sha
    if rev == "@-":
        return git("rev-parse", "--verify", "HEAD^{commit}")
    return ""

findings = []

def add(category, line, message, remediation, severity="warning"):
    findings.append({
        "source": "commit_message_format",
        "category": category,
        "severity": severity,
        "file": "/COMMIT_MSG",
        "line": line,
        "message": message,
        "remediation": remediation,
    })

head = resolve_rev("HEAD")
base_rev = resolve_rev(base)
# In a jj colocated checkout, git HEAD stays at @- while the working-copy
# commit @ holds the change and commit message once described.
if head and base_rev and head == base_rev and os.path.isdir(os.path.join(workdir, ".jj")):
    at_rev = resolve_rev("@")
    if at_rev and at_rev != base_rev and git("log", "-1", "--format=%B", at_rev).strip():
        head = at_rev
if head and base_rev and head != base_rev:
    msg = git("log", "-1", "--format=%B", head)
    lines = msg.splitlines()
    subject = lines[0] if lines else ""
    raw_dirs = [d.strip().strip("/") for d in target_dir.split(",") if d.strip().strip("/")]
    # build/secondary/<dir> is GN's overlay for //<dir> and must not get Bazel files
    # (gn_secondary_tree): its code migrates to //<dir> (e.g. //third_party/<name>).
    dirs = [d[len("build/secondary/"):] if d.startswith("build/secondary/") else d for d in raw_dirs]
    first = dirs[0] if dirs else "<dir>"
    changed = [f for f in git("diff", "--name-only", base_rev, head).splitlines() if f]

    # Subsystem tags used by earlier commits touching the directory.
    tags = collections.Counter()
    if dirs:
        log = git("log", "-n", "200", "--format=%s", base_rev, "--", *dict.fromkeys(raw_dirs + dirs))
        for s in log.splitlines():
            for t in re.findall(r"\[([A-Za-z0-9_.\-]+)\]", s):
                if t.lower() not in ("bazel_migration", "bazel", "build", "gn", "roll", "release"):
                    tags[t] += 1
    common = [t for t, n in tags.most_common() if n >= 2]
    shown = " ".join(dict.fromkeys(raw_dirs[:1] + dirs[:1])) or "<dir>"
    hint = (f"Tags used for this directory in `git log --format=%s -- {shown}`: "
            + ", ".join(f"[{t}]" for t in common[:5]) + ".") if common else (
            f"`git log --format=%s -- {shown}` shows no recurring subsystem tag.")
    moved = [f"//{r} -> //{d}" for r, d in zip(raw_dirs, dirs) if r != d]
    if moved:
        hint += (" GN overlay directories migrate to the directory they overlay (gn_secondary_tree): "
                 + ", ".join(moved) + ".")

    leaves = [d.rsplit("/", 1)[-1] for d in dirs]
    m_tag = re.match(r"^\[bazel_migration\]\[([A-Za-z0-9_.\-]+)\] //\S+$", subject)
    m_none = re.match(r"^\[bazel_migration\] //\S+$", subject)
    if not changed and not git("status", "--porcelain", "--ignore-submodules=all"):
        add("commit_subject_empty_change", 1,
            "The change modifies no file versus its parent, so nothing is migrated and the "
            "'[bazel_migration] //<migrated dir>' subject is not required.",
            "Do not announce a migration that did not happen: keep a subject that describes the commit and "
            "report the blocker and the steps it needs first in the summary. Once the change migrates "
            f"targets, use '[bazel_migration][<subsystem>] //{first}'. {hint}", severity="info")
    elif not (m_tag or m_none):
        add("commit_subject_format", 1,
            f"Commit subject {subject!r} does not follow '[bazel_migration][<subsystem>] //<migrated dir>' "
            "(or '[bazel_migration] //<migrated dir>' when there is no established subsystem).",
            f"Use '[bazel_migration][<subsystem>] //{first}' with <subsystem> taken from the directory's git log "
            "history (prefer the broader subsystem over a leaf directory or crate name); if the history has no "
            f"recurring tag and no parent directory is a subsystem, use '[bazel_migration] //{first}'. {hint}")
    elif m_tag and m_tag.group(1) in leaves and not [t for t in common if t not in leaves]:
        add("commit_subject_leaf_tag", 1,
            f"Commit subject tag [{m_tag.group(1)}] is just the leaf directory name, which may be too narrow to be "
            "a subsystem tag.",
            f"Keep it only if the history shows it is an established subsystem; otherwise, with no parent "
            f"subsystem, use '[bazel_migration] //{first}'. {hint}")
    elif m_none and [t for t in common if t not in leaves]:
        add("commit_subject_missing_tag", 1,
            "Commit subject has no subsystem tag although the directory's git log uses one.",
            f"Use '[bazel_migration][<subsystem>] //{first}'. {hint}")

    test_lines = [(i + 1, l) for i, l in enumerate(lines) if l.startswith("Test:")]
    for ln, l in test_lines:
        cmd = l[len("Test:"):].strip()
        if re.search(r"\bfx bazel (build|test|run|query|cquery)\b", cmd) and "--config=" not in cmd:
            add("test_footer_bazel_no_config", ln,
                f"`Test:` footer `{cmd}` runs `fx bazel` without `--config=...`, which is invalid.",
                "Pass `--config=fuchsia_platform` (device) or `--config=host`, e.g. "
                "`fx bazel test --config=fuchsia_platform @//<dir>:<test>`; record only commands that exited 0.")
        if re.search(r"\bfx bazel\b", cmd) and re.search(r"(?<![@\w])//", cmd):
            add("test_footer_label_without_at", ln,
                f"`Test:` footer `{cmd}` passes Bazel labels without a leading `@`.",
                "Write Bazel labels as `@//<dir>:<target>` in `fx bazel ...` commands.")
        for pkg, name in re.findall(r"@?//([\w./\-]+):([\w.\-]+)", cmd):
            if pkg.rsplit("/", 1)[-1] == name:
                add("test_footer_redundant_label", ln,
                    f"`Test:` footer label `//{pkg}:{name}` repeats the package name.",
                    f"Write `//{pkg}` instead of `//{pkg}:{name}`.")
        # TODO(https://fxbug.dev/538640644): Replace with fx lint once it runs these shac checks.
        if (
            re.search(r"\bfx bazel2gn\b", cmd)
            or re.search(r"verify_bazel2gn", cmd)
            or (re.search(r"\bshac\s+check\b", cmd) and cmd != "fx host-tool shac check")
        ):
            add("test_footer_bazel2gn_form", ln,
                f"`Test:` footer `{cmd}` verifies bazel2gn/shac in a narrower or indirect form.",
                "Use the single line `Test: fx host-tool shac check` (runs `verify_bazel2gn` and all other shac checks).")
        if re.search(r"\bfx bazel build\b.*:all\b", cmd):
            add("test_footer_build_all_not_test", ln,
                f"`Test:` footer `{cmd}` builds `:all` instead of building or running the migrated test target.",
                "Name the test target: `fx bazel test --config=fuchsia_platform @//<dir>:<test>` if it runs, else "
                "`fx bazel build ...`; add `fx bazel test --config=host @//<dir>:<test>` when the target can build "
                "for host (no target_compatible_with restriction).")
    # Test footers: prefer an ancestor test_suite() that includes the migrated tests.
    suites, has_tests = [], False
    for d in dirs:
        try:
            with open(f"{workdir}/{d}/BUILD.bazel") as fh:
                own = fh.read()
            has_tests |= bool(re.search(r'with_host_unit_tests\s*=\s*True|with_unit_tests\s*=\s*"(host|both|fuchsia)"'
                                        r'|^\s*(host_\w*test|rustc_test|fx_test|fx_unittest_package|test_suite)\(', own, re.M))
        except OSError:
            pass
        parts = d.split("/")
        for i in range(len(parts) - 1, 0, -1):
            anc = "/".join(parts[:i])
            try:
                with open(f"{workdir}/{anc}/BUILD.bazel") as fh:
                    text = fh.read()
            except OSError:
                continue
            for blk in re.findall(r"^test_suite\((.*?)^\)", text, re.M | re.S):
                n = re.search(r'^\s*name\s*=\s*"([^"]+)"', blk, re.M)
                if n and re.search(r'"@?//' + re.escape(d) + r'[:"]', blk):
                    suites.append((anc, n.group(1)))
    def names_suite(l, a, n):
        return f"//{a}:{n}" in l or (n == a.rsplit("/", 1)[-1] and re.search(r"//" + re.escape(a) + r"(?![\w/:.-])", l))
    if suites and test_lines and not any(names_suite(l, a, n) for _, l in test_lines for a, n in suites):
        a, n = suites[-1]
        add("test_footer_not_ancestor_suite", test_lines[0][0],
            f"`Test:` footers run the migrated tests directly, but the ancestor `test_suite()` //{a}:{n} includes them.",
            f"Prefer the higher-level suite, e.g. `Test: fx bazel test --config=host @//{a}:{n}`, or the GN group that "
            "depends on it through a `bazel_test_suite` (e.g. `Test: fx test --host //<pkg>:tests`), which also runs "
            "remaining GN tests. Use it only after verifying the new tests run as a result, and state the dependency "
            "chain in the body before the footers.")
    if dirs and has_tests and not suites:
        add("migrated_tests_not_in_ancestor_suite", 1,
            "No ancestor BUILD.bazel `test_suite()` includes the migrated tests.",
            "Add the migrated tests to the appropriate `test_suite()` in an ancestor directory's BUILD.bazel (the one "
            "already wired into a GN `bazel_test_suite`). If none exists (e.g. first migration in the area), do not "
            "invent one: say in the summary that the developer must choose or define the `test_suite()` (possibly "
            "consulting the Build team).", severity="info")
    # Body: changes not specific to the migrated targets must be described.
    body = "\n".join(l for l in lines[1:] if not re.match(r"^(Test|Bug|Fixed|Bazel-Migration-Target|Skip-Readme-Check|TAG|Change-Id|Multiply|Cq-[\w-]+):", l))
    # Check that the commit body does not restate the [bazel_migration] //<dir> subject line,
    # including when wrapped across multiple lines or when the subject abbreviates with //.../
    footer_line_re = re.compile(
        r"^(Test|Bug|Fixed|Bazel-Migration-Target|Skip-Readme-Check|TAG|Change-Id|"
        r"Multiply|Cq-[\w-]+|Reviewed-on|Reviewed-by|Commit-Queue|Fuchsia-Auto-Submit|"
        r"Presubmit-Verified|API-Review):"
    )
    redundant_body_para_re = re.compile(
        r"^(?:\[[\w.\-]+\]\s*)+(?:migrates?\b.*|@?//[\w./\-]+\.?)$"
        r"|^@?//[\w./\-]+\.?$"
        r"|^(?:this\s+(?:cl|change|commit|patch)\s+)?migrates?\s+(?:the\s+)?(?:@?//)?[\w./\-]+"
        r"(?:\s+(?:and\s+(?:its\s+)?(?:unit\s+)?tests?|targets?|package|library|binary))?"
        r"(?:\s+from\s+GN)?(?:\s+to\s+Bazel)?\.?$",
        re.I,
    )

    def body_paragraphs(msg_lines):
        paragraphs = []
        cur_start, cur_end, cur_parts = None, None, []
        for idx, raw in enumerate(msg_lines[1:], start=2):
            if footer_line_re.match(raw):
                break
            stripped = raw.strip()
            if not stripped:
                if cur_parts:
                    paragraphs.append((cur_start, cur_end, " ".join(cur_parts)))
                    cur_start, cur_end, cur_parts = None, None, []
                continue
            if cur_start is None:
                cur_start = idx
            cur_end = idx
            cur_parts.append(stripped)
        if cur_parts:
            paragraphs.append((cur_start, cur_end, " ".join(cur_parts)))
        return paragraphs

    def check_redundant_body_title(msg_lines):
        for start_ln, end_ln, para in body_paragraphs(msg_lines):
            if redundant_body_para_re.match(para):
                span = f"lines {start_ln}-{end_ln}" if end_ln > start_ln else f"line {start_ln}"
                msg_text = (
                    f"Commit body ({span}: `{para}`) merely restates the "
                    "`[bazel_migration] //<dir>` subject line without adding information "
                    "(even when the subject abbreviates the path with `//.../`)."
                )
                rem_text = (
                    "Remove the entire redundant `Migrate //<dir>...` paragraph from the "
                    "commit body (leave the body empty when no non-obvious changes need "
                    "explanation; the full path is already recorded in "
                    "`Bazel-Migration-Target:`)."
                )
                add("commit_body_redundant_title", end_ln, msg_text, rem_text)
                break

    # Check that commit message paths are relative to the Fuchsia checkout root
    # (never prefixed with .jiri_root/worktrees/<name>/ or host filesystem paths).
    non_checkout_path_re = re.compile(
        r"(?:@?//)?\.jiri_root/(?:worktrees/[^/\s]+/)?([\w./\-]*)"
        r"|(?<![\w/:.])/(?:usr|home|tmp|var|opt)/[\w./\-]+"
    )

    def check_checkout_relative_paths(msg_lines):
        in_also_migrated = False
        for idx, raw in enumerate(msg_lines, start=1):
            stripped = raw.strip()
            if stripped == "Also migrated in this change:":
                in_also_migrated = True
                continue
            if in_also_migrated and not stripped.startswith("- "):
                in_also_migrated = False
            m_bad = non_checkout_path_re.search(raw)
            if m_bad:
                bad_tok = m_bad.group(0)
                rel_sub = (m_bad.group(1) or "<dir>").strip("/") or "<dir>"
                msg_text = (
                    f"Commit message line {idx} contains non-checkout-relative path "
                    f"`{bad_tok}` (includes `.jiri_root`/worktree or host filesystem prefix)."
                )
                rem_text = (
                    f"Use a path relative to the Fuchsia checkout root (`//{rel_sub}`), "
                    "or remove the `Also migrated in this change:` block if no directories "
                    f"other than `//{first}` were migrated. Also ensure `modified_files` in "
                    "CoderReport uses checkout-relative paths (`<dir>/BUILD.bazel`, never "
                    "`.jiri_root/worktrees/<name>/...`)."
                )
                add("commit_msg_non_checkout_path", idx, msg_text, rem_text, severity="error")
                continue
            if in_also_migrated and stripped.startswith("- "):
                bullet = stripped[2:].strip()
                b_dir = bullet[2:].strip("/") if bullet.startswith("//") else ""
                if not b_dir or b_dir.startswith("."):
                    msg_text = (
                        f"`Also migrated in this change:` bullet `{bullet}` is not a valid "
                        "Fuchsia checkout-relative `//<dir>` path."
                    )
                    rem_text = (
                        "Write each bullet as `- //<dir>` relative to the Fuchsia checkout "
                        "root, or remove the block if no extra directories were migrated."
                    )
                    add("commit_msg_non_checkout_path", idx, msg_text, rem_text, severity="error")
                elif b_dir == first and len(dirs) <= 1:
                    msg_text = (
                        f"`Also migrated in this change:` bullet `{bullet}` duplicates the "
                        f"primary migrated directory `//{first}`."
                    )
                    rem_text = (
                        "Remove the redundant `Also migrated in this change:` block when "
                        f"only `//{first}` is migrated, and pass checkout-relative paths "
                        "in `CoderReport.modified_files`."
                    )
                    add("commit_body_redundant_also_migrated", idx, msg_text, rem_text)

    check_redundant_body_title(lines)
    check_checkout_relative_paths(lines)
    in_dirs = lambda f: any(f == d or f.startswith(d + "/") for d in dirs)
    for f in changed:
        if in_dirs(f) or not (f.endswith(".bzl") or f.rsplit("/", 1)[-1] == "MODULE.bazel"):
            continue
        if f not in body and f.rsplit("/", 1)[-1] not in body:
            add("commit_body_unmentioned_shared_change", 1,
                f"The change edits `{f}`, which is not specific to the migrated targets, but the commit body "
                "does not mention it.",
                f"Describe the `{f}` change and why it is needed in the body (one bullet per topic). Wiring edits "
                "(parent BUILD files, `*.gni` lists) need no mention. Keep the body in "
                "sync as changes are added or removed during review. First confirm the shared edit is needed at "
                "all (e.g. test-only files belong in `test_data`, not in a rule edit forwarding `data`).")
    # Body: name only the rules/macros actually called in the migrated BUILD.bazel files.
    called = set()
    for d in dirs:
        try:
            with open(f"{workdir}/{d}/BUILD.bazel") as fh:
                called |= set(re.findall(r"^\s*([A-Za-z_]\w*)\(", fh.read(), re.M))
        except OSError:
            pass
    if dirs and called:
        for rule in sorted(set(re.findall(r"(?<![\w.])(rust_(?:library|binary|test|proc_macro)|cc_(?:library|binary|test))(?![\w.])", body))):
            if rule not in called:
                add("commit_body_uncalled_rule", 1,
                    f"The commit body names `{rule}`, which no migrated BUILD.bazel calls directly.",
                    "Name only the rules/macros that define targets in BUILD.bazel, written as `rustc_library()`; "
                    "say which attribute generates implicit targets (e.g. `with_host_unit_tests = True` generates "
                    "\"lib_test\"). Refer to targets by name and give the crate name separately when different "
                    "(target \"lib\" (crate name \"foo\")); put identifiers in backticks or quotes.")
    # Body: a crate_name that differs from its target name must not be presented as a target name.
    crates = {}
    for d in dirs:
        try:
            with open(f"{workdir}/{d}/BUILD.bazel") as fh:
                text = fh.read()
        except OSError:
            continue
        for blk in re.findall(r"^[A-Za-z_]\w*\((.*?)^\)", text, re.M | re.S):
            n = re.search(r'^\s*name\s*=\s*"([^"]+)"', blk, re.M)
            c = re.search(r'^\s*crate_name\s*=\s*"([^"]+)"', blk, re.M)
            if n and c and n.group(1) != c.group(1):
                crates[c.group(1)] = n.group(1)
    for crate, tgt in sorted(crates.items()):
        for m in re.finditer(r"(?<![\w/.:-])" + re.escape(crate) + r"(_test)?(?![\w-])", body):
            if not re.search(r"\bcrate\b", body[max(0, m.start() - 40):m.end() + 20], re.I):
                add("commit_body_crate_name_as_target", 1,
                    f"The commit body names `{m.group(0)}` like a target, but `{crate}` is the crate_name of target "
                    f"\"{tgt}\" (its unit test target is \"{tgt}_test\").",
                    f"Refer to the target by name and give the crate name separately, e.g. target \"{tgt}\" "
                    f"(crate name \"{crate}\"), and say which attribute generates \"{tgt}_test\".")
                break
    b2g = [ln for ln, l in test_lines if re.search(r"\bfx bazel2gn\b|verify_bazel2gn|\bshac\s+check\b", l)]
    if b2g and test_lines and b2g[-1] != test_lines[-1][0]:
        add("test_footer_bazel2gn_not_last", b2g[-1],
            "The shac/bazel2gn verification `Test:` line is not last.",
            "Group build/test commands together and put `Test: fx host-tool shac check` last.")
    # Bazel-Migration-Target: footers (required for every non-empty migration commit).
    is_empty = not changed and not git("status", "--porcelain", "--ignore-submodules=all")
    bmt_lines = [(i + 1, l) for i, l in enumerate(lines) if re.match(r"^Bazel-Migration-Target\s*:", l)]
    migrated_targets = []
    excluded_bmt = {}
    helper_rules = {"package", "exports_files", "alias", "config_setting",
                    "fx_packaged_binary", "fx_component_manifest", "fx_test_component",
                    "fx_unittest_component"}
    ut_re = r'^\s*(with_unit_tests\s*=\s*"(host|both|fuchsia)"|with_unit_tests\s*=\s*True|with_host_unit_tests\s*=\s*True)'
    for d in dirs:
        try:
            with open(f"{workdir}/{d}/BUILD.bazel") as fh:
                btxt = fh.read()
        except OSError:
            continue
        base_btxt = git("show", f"{base_rev}:{d}/BUILD.bazel")
        base_gtxt = git("show", f"{base_rev}:{d}/BUILD.gn")
        base_bazel_names = set()
        for m in re.finditer(r"^([A-Za-z_]\w*)\((.*?)^\)", base_btxt, re.M | re.S):
            n = re.search(r'^\s*name\s*=\s*"([^"]+)"', m.group(2), re.M)
            if n:
                base_bazel_names.add(n.group(1))
                if re.search(ut_re, m.group(2), re.M):
                    base_bazel_names.add(f"{n.group(1)}_test")
        base_gn_names = set(re.findall(r'^\s*[A-Za-z_]\w*\(\s*"([^"]+)"\s*\)', base_gtxt, re.M))
        blocks = list(re.finditer(r"^([A-Za-z_]\w*)\((.*?)^\)", btxt, re.M | re.S))
        explicit_names = set()
        test_wrapped_pkgs = set()
        wrapped_bins = set()
        for m in blocks:
            rule, blk = m.group(1), m.group(2)
            n = re.search(r'^\s*name\s*=\s*"([^"]+)"', blk, re.M)
            if n:
                explicit_names.add(n.group(1))
            if rule == "fx_test":
                p = re.search(r'^\s*package\s*=\s*":([^"]+)"', blk, re.M)
                if p:
                    test_wrapped_pkgs.add(p.group(1))
            if rule in ("host_test", "fx_packaged_binary", "fx_unittest_component"):
                b = re.search(r'^\s*binary\s*=\s*":([^"]+)"', blk, re.M)
                if b and b.group(1) not in base_gn_names:
                    wrapped_bins.add(b.group(1))
            if rule == "fx_unittest_package":
                u = re.search(r'unit_tests\s*=\s*\[(.*?)\]', blk, re.S)
                if u:
                    for b in re.findall(r'":([^"]+)"', u.group(1)):
                        if b not in base_gn_names:
                            wrapped_bins.add(b)
        all_dir_targets, new_dir_targets = [], []
        for m in blocks:
            rule, blk = m.group(1), m.group(2)
            n = re.search(r'^\s*name\s*=\s*"([^"]+)"', blk, re.M)
            if not n:
                continue
            tname = n.group(1)
            lbl = f"//{d}:{tname}"
            if rule in helper_rules or (rule == "fx_package" and tname in test_wrapped_pkgs) or tname in wrapped_bins:
                excluded_bmt[lbl] = "is an internal Bazel packaging/test-wrapper helper target, not a migrated GN target"
                continue
            all_dir_targets.append(lbl)
            if tname not in base_bazel_names:
                new_dir_targets.append(lbl)
            if re.search(ut_re, blk, re.M) and f"{tname}_test" not in explicit_names:
                tlbl = f"//{d}:{tname}_test"
                all_dir_targets.append(tlbl)
                if f"{tname}_test" not in base_bazel_names:
                    new_dir_targets.append(tlbl)
        if new_dir_targets:
            for lbl in all_dir_targets:
                if lbl not in new_dir_targets:
                    excluded_bmt[lbl] = "already existed in `BUILD.bazel` before this commit"
            migrated_targets.extend(new_dir_targets)
        else:
            migrated_targets.extend(all_dir_targets)
    if not is_empty:
        migrated_targets = list(dict.fromkeys(migrated_targets))
        footer_ln = test_lines[-1][0] if test_lines else max(1, len(lines))
        example_bmt = ", ".join(
            f"`Bazel-Migration-Target: {t}`"
            for t in (migrated_targets or [f"//{first}:<target>"])
        )
        if not bmt_lines:
            add(
                "missing_bazel_migration_target_footer",
                footer_ln,
                "Commit message is missing `Bazel-Migration-Target:` footer(s) "
                "recording the migrated target label(s).",
                "Add one `Bazel-Migration-Target: //<dir>:<target>` (or "
                "`//<dir>`) footer line per migrated target in the single "
                "contiguous footer block at the bottom of the commit message "
                "(after `Test:` and before `TAG:`/`Change-Id:`, with no blank "
                "lines between footers and no line wrapping; "
                "`Bazel-Migration-Target:` lines may exceed 72 characters), "
                f"e.g. {example_bmt}.",
            )
        for ln, l in bmt_lines:
            val = l.split(":", 1)[1].strip()
            canon_val = val if ":" in val else f"{val}:{val.rsplit('/', 1)[-1]}"
            wrapped = (
                ln < len(lines)
                and bool(lines[ln].strip())
                and not re.match(r"^[A-Za-z0-9\-]+\s*:", lines[ln])
            )
            if not re.match(r"^//[\w./\-]+(:[\w.\-]+)?$", val) or wrapped:
                add(
                    "bazel_migration_target_footer_format",
                    ln,
                    f"`Bazel-Migration-Target:` footer `{l}` must specify a "
                    "single canonical `//<dir>:<target>` (or `//<dir>`) label "
                    "on one unwrapped line (without a leading `@`).",
                    "Write one unwrapped line per migrated target (may exceed "
                    f"72 characters), e.g. {example_bmt}.",
                )
            elif ":" in val and canon_val in excluded_bmt:
                add(
                    "bazel_migration_target_not_migrated",
                    ln,
                    f"`Bazel-Migration-Target:` footer `{l}` names `{val}`, "
                    f"which {excluded_bmt[canon_val]}.",
                    "List only targets migrated from GN in this commit, "
                    f"e.g. {example_bmt}.",
                )
        skip_readme_lines = [(i + 1, l) for i, l in enumerate(lines) if re.match(r"^Skip-Readme-Check\s*:", l)]
        has_valid_skip_readme = False
        for ln, l in skip_readme_lines:
            if len(l) > 72:
                add("skip_readme_check_line_too_long", ln,
                    f"`Skip-Readme-Check:` footer `{l}` is {len(l)} characters, exceeding the 72-character upload limit.",
                    "Shorten it to `Skip-Readme-Check: No version change, Bazel build files only.` (<= 72 characters).",
                    severity="error")
            elif l.split(":", 1)[1].strip():
                has_valid_skip_readme = True
            else:
                add("skip_readme_check_empty_reason", ln,
                    "The `Skip-Readme-Check:` footer must be followed by a non-empty reason.",
                    "Write `Skip-Readme-Check: No version change, Bazel build files only.` in the contiguous footer block at the bottom of the commit message.",
                    severity="error")
        all_changed = set(changed) | {f for f in git("diff", "--name-only", base_rev).splitlines() if f}
        missing_readmes = set()
        for f in sorted(all_changed):
            if not f.startswith("third_party/") or f.startswith("third_party/rust_crates/"):
                continue
            parts = f.split("/")
            for depth in range(len(parts) - 1, 1, -1):
                cand = "/".join(parts[:depth])
                readme = f"{cand}/README.fuchsia"
                if os.path.isfile(f"{workdir}/{readme}"):
                    if f != readme and readme not in all_changed:
                        missing_readmes.add((cand, readme))
                    break
        if missing_readmes and not has_valid_skip_readme:
            cand, readme = sorted(missing_readmes)[0]
            add("missing_skip_readme_check_footer", footer_ln,
                f"Files under `{cand}` were modified without modifying `{readme}`, which fails the CQ `third_party_readme_update` shac check unless waived by a `Skip-Readme-Check:` footer.",
                "Because GN-to-Bazel migrations must not modify `README.fuchsia` (`build_only_scope`), add `Skip-Readme-Check: No version change, Bazel build files only.` to the contiguous footer block at the bottom of the commit message (before `TAG:`/`Change-Id:`).",
                severity="error")
        nonempty_idxs = [i for i, l in enumerate(lines) if l.strip()]
        if nonempty_idxs:
            final_start = nonempty_idxs[-1]
            while final_start > 0 and lines[final_start - 1].strip():
                final_start -= 1
            for ln, l in bmt_lines + test_lines + skip_readme_lines:
                if ln - 1 < final_start:
                    add("footer_block_not_contiguous", ln,
                        f"Footer `{l}` is separated from the final footer block by a blank line, so Gerrit will not "
                        "index it as a commit footer (breaking `hasfooter:Bazel-Migration-Target` queries).",
                        "Group all footers (`Bug:`/`Fixed:`, `Skip-Readme-Check:`, `Test:`, `Bazel-Migration-Target:`, `TAG:`, "
                        "`Change-Id:`) into a single contiguous block at the very bottom of the commit message with "
                        "no blank lines between them.")
                    break
print(json.dumps(findings, indent=2))
PYEOF
