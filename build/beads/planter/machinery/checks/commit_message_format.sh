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
TARGET_DIRS="${PLANTER_TARGET_DIRS:-$TARGET_DIR}"
BASE="${PLANTER_CHANGE_BASE:-HEAD}"

python3 - "$WORKDIR" "$TARGET_DIRS" "$BASE" <<'PYEOF'
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

raw_dirs = [
    d.strip().strip("/")
    for d in re.split(r"[\s,]+", target_dir)
    if d.strip().strip("/")
]
# build/secondary/<dir> is GN's overlay for //<dir> and must not get Bazel files
# (gn_secondary_tree): its code migrates to //<dir> (e.g. //third_party/<name>).
dirs = [d[len("build/secondary/"):] if d.startswith("build/secondary/") else d for d in raw_dirs]
check_dirs = list(dict.fromkeys(raw_dirs + dirs))

# Match a package path against the //dir or //.../<suffix> path in a commit subject.
def matches_subject_dir(pkg, sdir):
    if not pkg or not sdir:
        return False
    if sdir.startswith(".../"):
        suf = sdir[4:]
        return bool(suf) and (pkg == suf or pkg.endswith("/" + suf))
    return pkg == sdir


# Verify that a [bazel_migration] HEAD commit belongs to this task's target
# directories rather than a stacked parent migration commit.
def is_task_migration_head(head_sha, parent_sha, subj):
    if not head_sha or not parent_sha or not subj.startswith("[bazel_migration]"):
        return False
    if not check_dirs:
        return True
    head_files = [f for f in git("diff", "--name-only", parent_sha, head_sha).splitlines() if f]
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
        os.path.dirname(f)
        for f in git("ls-files", "--others", "--exclude-standard", "--", *check_dirs).splitlines()
        if os.path.basename(f) in ("BUILD.bazel", "BUILD")
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


head = resolve_rev("HEAD")
base_rev = resolve_rev(base)
# In a jj colocated checkout, git HEAD stays at @- while the working-copy
# commit @ holds the change and commit message once described.
if head and base_rev and head == base_rev and os.path.isdir(os.path.join(workdir, ".jj")):
    at_rev = resolve_rev("@")
    if at_rev and at_rev != base_rev and git("log", "-1", "--format=%B", at_rev).strip():
        head = at_rev

parent_rev = git("rev-parse", "--verify", f"{head}~1^{{commit}}") if head else ""
head_subj = git("log", "-1", "--format=%s", head) if head else ""
if is_task_migration_head(head, parent_rev, head_subj) and (not base_rev or base_rev == head):
    base_rev = parent_rev

# Advance a pre-rebase or pre-amend base revision to HEAD~1 (or the origin/main
# merge-base) so upstream commits pulled in by `git rebase origin/main` or prior
# patchsets before a rebase/amend are not diffed as part of this migration commit.
def advance_rebased_base(cur_head, cur_base):
    if not (cur_head and cur_base and cur_head != cur_base):
        return cur_base
    try:
        is_anc = (
            subprocess.run(
                ["git", "-C", workdir, "merge-base", "--is-ancestor", cur_base, cur_head],
                capture_output=True,
                timeout=15,
            ).returncode
            == 0
        )
    except Exception:
        is_anc = True
    if not is_anc:
        if parent_rev and is_task_migration_head(cur_head, parent_rev, head_subj):
            return parent_rev
    mb = git("merge-base", cur_head, "origin/main")
    if not (mb and mb != cur_base and mb != cur_head):
        return cur_base
    try:
        r = subprocess.run(
            ["git", "-C", workdir, "merge-base", "--is-ancestor", cur_base, mb],
            capture_output=True,
            timeout=15,
        )
        if r.returncode == 0:
            return mb
    except Exception:
        pass
    return cur_base

base_rev = advance_rebased_base(head, base_rev)
if head and base_rev and head != base_rev:
    msg = git("log", "-1", "--format=%B", head)
    lines = msg.splitlines()
    subject = lines[0] if lines else ""
    changed = [f for f in git("diff", "--name-only", base_rev, head).splitlines() if f]
    changed_dirs = [
        d for d in dirs if any(f == d or f.startswith(d + "/") for f in changed)
    ]
    first = (changed_dirs[0] if changed_dirs else dirs[0]) if dirs else "<dir>"

    # Subsystem tags used by earlier commits touching the directory.
    # Ignore cross-cutting language/sweep tags unless the tag is itself a
    # path segment of the migrated directory.
    sweep_tags = {
        "bazel_migration", "bazel", "bazel2gn", "build", "gn", "roll", "release",
        "rust", "rs", "cpp", "c++", "cxx", "go", "py", "python", "fidl",
        "clippy", "rustfmt", "clang", "lint", "format", "cleanup", "refactor",
        "flake", "fix", "trivial",
    }
    dir_segments = {seg.lower() for d in dirs for seg in d.split("/") if seg}
    generic_segments = {
        "src", "sdk", "lib", "libs", "bin", "bins", "tools", "testing", "tests",
        "test", "fidl", "service", "services", "client", "server", "common",
        "util", "utils", "internal", "sys", "developer", "bringup", "zircon",
        "products", "bundles", "boards", "examples", "third_party",
    }
    leaves = [d.rsplit("/", 1)[-1] for d in dirs]
    ancestor_subsystems = {
        seg.lower()
        for d in dirs
        for seg in d.split("/")[:-1]
        if seg and seg.lower() not in generic_segments
    }
    tags = collections.Counter()
    if dirs:
        log = git("log", "-n", "200", "--format=%s", base_rev, "--", *dict.fromkeys(raw_dirs + dirs))
        for s in log.splitlines():
            for t in re.findall(r"\[([A-Za-z0-9_.\-]+)\]", s):
                tl = t.lower()
                if tl in ("bazel_migration", "bazel", "bazel2gn", "build", "gn", "roll", "release"):
                    continue
                if tl in sweep_tags and tl not in dir_segments:
                    continue
                tags[t] += 1
    common = sorted(
        [t for t, n in tags.most_common() if n >= 2],
        key=lambda t: (t.lower() not in ancestor_subsystems, -tags[t]),
    )
    shown = " ".join(dict.fromkeys(raw_dirs[:1] + dirs[:1])) or "<dir>"
    hint = (f"Tags used for this directory in `git log --format=%s -- {shown}`: "
            + ", ".join(f"[{t}]" for t in common[:5]) + ".") if common else (
            f"`git log --format=%s -- {shown}` shows no recurring subsystem tag.")
    moved = [f"//{r} -> //{d}" for r, d in zip(raw_dirs, dirs) if r != d]
    if moved:
        hint += (" GN overlay directories migrate to the directory they overlay (gn_secondary_tree): "
                 + ", ".join(moved) + ".")

    non_leaf_common = [t for t in common if t not in leaves]

    # Build a single-line commit subject <= 65 chars by abbreviating leading path segments when needed.
    def abbreviate_subject_path(prefix, rel_dir, limit=65):
        full = f"{prefix} //{rel_dir}"
        if len(full) <= limit:
            return full
        segs = [s for s in rel_dir.split("/") if s]
        for i in range(1, len(segs)):
            cand = f"{prefix} //.../{'/'.join(segs[i:])}"
            if len(cand) <= limit:
                return cand
        return f"{prefix} //.../{segs[-1]}" if segs else full

    if non_leaf_common:
        tag0 = non_leaf_common[0]
        tagged_full = f"[bazel_migration][{tag0}] //{first}"
        untagged_full = f"[bazel_migration] //{first}"
        if len(tagged_full) <= 65:
            pref_subj = tagged_full
            alt_subj = ""
        elif len(untagged_full) <= 65:
            pref_subj = untagged_full
            alt_subj = abbreviate_subject_path(f"[bazel_migration][{tag0}]", first)
        else:
            pref_subj = abbreviate_subject_path(f"[bazel_migration][{tag0}]", first)
            alt_subj = abbreviate_subject_path("[bazel_migration]", first)
    else:
        pref_subj = abbreviate_subject_path("[bazel_migration]", first)
        alt_subj = ""
    alt_note = f" (or '{alt_subj}')" if alt_subj and alt_subj != pref_subj else ""
    subj_path = (
        subject.split(" //", 1)[1].split(":", 1)[0].strip().rstrip(".").strip("/")
        if " //" in subject
        else ""
    )
    m_tag = re.match(r"^\[bazel_migration\]\[([A-Za-z0-9_.\-]+)\] //\S+(?<![.,;:])$", subject)
    m_none = re.match(r"^\[bazel_migration\] //\S+(?<![.,;:])$", subject)
    if not changed and not git("status", "--porcelain", "--ignore-submodules=all"):
        add("commit_subject_empty_change", 1,
            "The change modifies no file versus its parent, so nothing is migrated and the "
            "'[bazel_migration] //<migrated dir>' subject is not required.",
            "Do not announce a migration that did not happen: keep a subject that describes the commit and "
            "report the blocker and the steps it needs first in the summary. Once the change migrates "
            f"targets, use '{pref_subj}'{alt_note}. {hint}", severity="info")
    elif not (m_tag or m_none):
        add("commit_subject_format", 1,
            f"Commit subject {subject!r} does not follow '[bazel_migration][<subsystem>] //<migrated dir>' "
            "(or '[bazel_migration] //<migrated dir>' when there is no established subsystem, "
            "without trailing punctuation).",
            f"Use '{pref_subj}'{alt_note} on a single unwrapped line <= 65 characters (without a trailing period). "
            "When adding `[<subsystem>]` would push the full `//<dir>` path over 65 characters, omit `[<subsystem>]` "
            "or abbreviate leading path segments as `//.../<suffix>` rather than wrapping `//<dir>` onto the next line. "
            f"{hint}", severity="error")
    elif len(subject) > 65:
        subj_tag = m_tag.group(1) if m_tag else (non_leaf_common[0] if non_leaf_common else "")
        over_untagged = f"[bazel_migration] //{first}"
        if len(over_untagged) <= 65:
            over_pref = over_untagged
            over_alt = (
                abbreviate_subject_path(f"[bazel_migration][{subj_tag}]", first)
                if subj_tag
                else ""
            )
        elif subj_tag:
            over_pref = abbreviate_subject_path(f"[bazel_migration][{subj_tag}]", first)
            over_alt = abbreviate_subject_path("[bazel_migration]", first)
        else:
            over_pref = abbreviate_subject_path("[bazel_migration]", first)
            over_alt = ""
        over_alt_note = f" or `{over_alt}`" if over_alt and over_alt != over_pref else ""
        add(
            "commit_subject_format", 1,
            f"Commit subject `{subject}` is {len(subject)} characters, exceeding the "
            "65-character limit (which causes subject line wrapping/truncation).",
            f"Use `{over_pref}`{over_alt_note} on a single unwrapped line <= 65 characters "
            "(omit `[<subsystem>]` or abbreviate leading path segments as `//.../<suffix>` "
            "instead of wrapping `//<dir>` onto the next line).",
            severity="error",
        )
    elif (
        len(dirs) == 1
        and subj_path
        and "..." not in subj_path
        and first.startswith(subj_path + "/")
        and f"{subj_path}/BUILD.bazel" not in changed
    ):
        add(
            "commit_subject_format",
            1,
            f"Commit subject `{subject}` names the parent directory `//{subj_path}` instead of "
            f"the migrated directory `//{first}`.",
            f"Use `{pref_subj}`{alt_note} (omit `[<subsystem>]` or abbreviate with `//.../` "
            f"when adding `[<subsystem>]` would push the full `//{first}` subject over "
            "65 characters).",
            severity="error",
        )
    elif (
        m_tag
        and m_tag.group(1).lower() in sweep_tags
        and m_tag.group(1).lower() not in dir_segments
    ):
        rec_subj = (
            abbreviate_subject_path(f"[bazel_migration][{non_leaf_common[0]}]", first)
            if non_leaf_common
            else abbreviate_subject_path("[bazel_migration]", first)
        )
        add(
            "commit_subject_sweep_tag",
            1,
            f"Commit subject tag [{m_tag.group(1)}] is a cross-cutting "
            "language/chore tag from tree-wide sweeps rather than the "
            f"subsystem tag for `//{first}`.",
            f"Use '{rec_subj}' instead. {hint}",
        )
    elif m_tag and m_tag.group(1) in leaves and not non_leaf_common:
        add("commit_subject_leaf_tag", 1,
            f"Commit subject tag [{m_tag.group(1)}] is just the leaf directory name, which may be too narrow to be "
            "a subsystem tag.",
            f"Keep it only if the history shows it is an established subsystem; otherwise, with no parent "
            f"subsystem, use '{abbreviate_subject_path('[bazel_migration]', first)}'. {hint}")
    elif (
        m_none
        and non_leaf_common
        and len(f"[bazel_migration][{non_leaf_common[0]}] //{first}") <= 65
    ):
        add("commit_subject_missing_tag", 1,
            "Commit subject has no subsystem tag although the directory's git log uses one.",
            f"Use '{pref_subj}'{alt_note}. {hint}")

    test_lines = [(i + 1, l) for i, l in enumerate(lines) if l.startswith("Test:")]
    for ln, l in test_lines:
        cmd = l[len("Test:"):].strip()
        if re.search(r"\bfx bazel (build|test|run|query|cquery)\b", cmd) and "--config=" not in cmd:
            add("test_footer_bazel_no_config", ln,
                f"`Test:` footer `{cmd}` runs `fx bazel` without `--config=...`, which is invalid.",
                "Pass `--config=fuchsia_platform` (device build) or `--config=host` (host test), "
                "e.g. `fx bazel test --config=host @//<dir>:<test>` or "
                "`fx bazel build --config=fuchsia_platform @//<dir>:<test>`; record only commands "
                "that exited 0.")
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
        # Check that `fx test` footers do not pass a GN `group("tests")` label (`//<dir>:tests`),
        # which is not a leaf test entry in `tests.json` and fails `fx test --dry`.
        if re.search(r"\bfx\s+(?:-t\s+\S+\s+)?test\b", cmd):
            for pkg_d, t_name in re.findall(r"(?<![@\w])//([\w./\-]+):([\w.\-]+)", cmd):
                pkg_d = pkg_d.strip("/")
                try:
                    with open(f"{workdir}/{pkg_d}/BUILD.gn") as fh:
                        gn_txt = fh.read()
                except OSError:
                    continue
                if t_name == "tests" and re.search(r'\bgroup\(\s*"tests"\s*\)', gn_txt):
                    add(
                        "test_footer_gn_tests_group_label",
                        ln,
                        f"`Test:` footer `{cmd}` passes GN `group(\"tests\")` label "
                        f"`//{pkg_d}:tests` to `fx test`, which fails `fx test --dry` "
                        "because `<build dir>/tests.json` only indexes leaf test targets, "
                        "not GN groups.",
                        f"Use `Test: fx test //{pkg_d}:<test_target>` or "
                        f"`Test: fx test //{pkg_d}` (without `:tests`), which selects "
                        "both the Bazel `bazel_test_suite` test and the GN test.",
                        severity="error",
                    )
                    continue
                if pkg_d not in dirs:
                    continue
                try:
                    with open(f"{workdir}/{pkg_d}/BUILD.bazel") as fh:
                        bz_txt = fh.read()
                except OSError:
                    bz_txt = ""
                in_bazel = bool(
                    re.search(r'\bname\s*=\s*"' + re.escape(t_name) + r'"', bz_txt)
                    or re.search(r'"//' + re.escape(f"{pkg_d}:{t_name}") + r'"', gn_txt)
                    or (
                        t_name.endswith("_test")
                        and re.search(r'\bname\s*=\s*"' + re.escape(t_name[:-5]) + r'"', bz_txt)
                    )
                )
                if (
                    not in_bazel
                    and re.search(r'\bgroup\(\s*"tests"\s*\)', gn_txt)
                    and re.search(r"\bbazel_test_suite\(", gn_txt)
                    and re.search(r'\(\s*"' + re.escape(t_name) + r'"\s*\)', gn_txt)
                ):
                    add(
                        "test_footer_gn_only_test_label",
                        ln,
                        f"`Test:` footer `{cmd}` passes the GN-only test target "
                        f"`//{pkg_d}:{t_name}` rather than `//{pkg_d}`, "
                        "which only selects the GN test in `BUILD.gn`.",
                        f"Use `Test: fx test //{pkg_d}` (without `:tests`) so `fx test` "
                        "selects all tests under the package in `tests.json` (both "
                        "`bazel_test_suite` and GN tests).",
                    )
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
            f"Prefer the higher-level suite, e.g. `Test: fx bazel test --config=host @//{a}:{n}`, or `fx test` on the "
            "package directory (e.g. `Test: fx test --host //<pkg>`), which also runs "
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
        r"(?:\s+from\s+GN)?(?:\s+to\s+Bazel)?"
        r"(?:,?\s+and\s+(?:generates?|regenerates?|updates?|removes?|deletes?)\s+.*"
        r"(?:BUILD\.gn|GN\s+targets?|bazel2gn))?"
        r"(?:\s+(?:with|using)\s+(?:dual[- ]build(?:\s+support)?|bazel2gn)"
        r"(?:\s+(?:via|using|with)\s+bazel2gn)?)?\.?$",
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

    # Inspect whether a package directory still has unmigrated GN targets in BUILD.gn
    # or only adds exports_files() without migrating the entire directory.
    gn_meta_constructs = {
        "group",
        "config",
        "bazel_test_suite",
        "bazel_host_tool",
        "declare_args",
        "import",
        "set_defaults",
        "template",
        "foreach",
        "if",
        "assert",
    }
    gn_test_only_rules = {
        "fuchsia_test_package",
        "fuchsia_unittest_package",
        "fuchsia_test_component",
        "fuchsia_unittest_component",
        "fuchsia_test",
        "rustc_test",
        "test",
        "go_test",
        "host_test",
        "python_host_test",
        "bootfs_test",
    }

    # Check whether a target block in BUILD.bazel is preceded by `# @bazel2gn:skip`.
    def has_bazel2gn_skip(btxt_str, start_pos):
        prefix_lines = btxt_str[:start_pos].splitlines()
        idx = len(prefix_lines) - 1
        while idx >= 0:
            s = prefix_lines[idx].strip()
            if not s:
                idx -= 1
                continue
            if s.startswith("#"):
                if "@bazel2gn:skip" in s:
                    return True
                idx -= 1
                continue
            break
        return False

    def inspect_dir_migration(rel_dir):
        gn_path = os.path.join(workdir, rel_dir, "BUILD.gn")
        bz_path = os.path.join(workdir, rel_dir, "BUILD.bazel")
        try:
            with open(gn_path) as fh:
                gtxt = fh.read()
        except OSError:
            gtxt = ""
        try:
            with open(bz_path) as fh:
                btxt = fh.read()
        except OSError:
            btxt = ""
        pre_gtxt = gtxt.split("## BAZEL2GN SENTINEL", 1)[0] if gtxt else ""
        suite_names = set(
            re.findall(r'^\s*(?:bazel_test_suite|bazel_host_tool)\(\s*"([^"]+)"\s*\)', gtxt, re.M)
        )
        kept_pre_gn = {}
        for grule, gname in re.findall(r'^\s*([A-Za-z_]\w*)\(\s*"([^"]+)"\s*\)', pre_gtxt, re.M):
            if grule not in gn_meta_constructs and gname not in suite_names:
                kept_pre_gn[gname] = grule
        base_btxt = git("show", f"{base_rev}:{rel_dir}/BUILD.bazel")
        bz_names = set()
        new_bz_labels = []
        base_bz_names = set()
        for m in re.finditer(r"^([A-Za-z_]\w*)\((?:[^\n#]*\)|.*?^\))", base_btxt, re.M | re.S):
            n = re.search(r'^\s*name\s*=\s*"([^"]+)"', m.group(0), re.M)
            if n:
                base_bz_names.add(n.group(1))
        for m in re.finditer(r"^([A-Za-z_]\w*)\((?:[^\n#]*\)|.*?^\))", btxt, re.M | re.S):
            rule, blk = m.group(1), m.group(0)
            n = re.search(r'^\s*name\s*=\s*"([^"]+)"', blk, re.M)
            if not n:
                continue
            tname = n.group(1)
            if (
                tname in kept_pre_gn
                and has_bazel2gn_skip(btxt, m.start())
                and rule not in ("fx_test_component", "rustc_test", "go_test")
            ):
                continue
            bz_names.add(tname)
            if rule not in ("package", "exports_files", "fx_packaged_binary", "fx_unittest_component"):
                if tname not in base_bz_names:
                    new_bz_labels.append(f"//{rel_dir}:{tname}")
        for m in re.finditer(r"^exports_files\(\s*\[(.*?)\]", btxt, re.M | re.S):
            for exp in re.findall(r'"([^"]+)"', m.group(1)):
                if f'"{exp}"' not in base_btxt:
                    new_bz_labels.append(f"//{rel_dir}:{exp}")
        if not gtxt:
            return [], [], new_bz_labels
        unmigrated_all = []
        unmigrated_non_test = []
        has_sentinel = "## BAZEL2GN SENTINEL" in gtxt
        for rule, gname in re.findall(r'^\s*([A-Za-z_]\w*)\(\s*"([^"]+)"\s*\)', gtxt, re.M):
            if rule in gn_meta_constructs or gname in bz_names or gname in suite_names:
                continue
            item = f'{rule}("{gname}")'
            if item not in unmigrated_all:
                unmigrated_all.append(item)
            if (
                rule not in gn_test_only_rules
                and not (
                    has_sentinel
                    and rule in ("fuchsia_package", "fuchsia_package_with_single_component")
                )
                and item not in unmigrated_non_test
            ):
                unmigrated_non_test.append(item)
        return unmigrated_all, unmigrated_non_test, new_bz_labels

    # Resolve a subject directory that may use a leading `.../` abbreviation, and determine
    # which package directories actually migrate targets in this commit against base_rev.
    all_cand_dirs = list(
        dict.fromkeys(
            dirs
            + [
                os.path.dirname(f)
                for f in changed
                if os.path.basename(f) in ("BUILD.bazel", "BUILD.gn") and os.path.dirname(f)
            ]
        )
    )

    def resolve_subject_dir(spec):
        s = spec.split(":", 1)[0].strip("/")
        if s.startswith(".../"):
            suf = "/" + s[4:].strip("/")
            for cand in all_cand_dirs:
                if ("/" + cand).endswith(suf):
                    return cand
        return s

    def is_dir_migrated_in_commit(rel_dir):
        _, _, new_bz_lbls = inspect_dir_migration(rel_dir)
        if new_bz_lbls:
            return True
        gn_rel = f"{rel_dir}/BUILD.gn"
        if gn_rel not in changed:
            return False
        base_gtxt = git("show", f"{base_rev}:{gn_rel}")
        if not base_gtxt:
            return False
        gn_path = os.path.join(workdir, gn_rel)
        if not os.path.isfile(gn_path):
            return True
        try:
            with open(gn_path) as fh:
                cur_gtxt = fh.read()
        except OSError:
            return False
        if "## BAZEL2GN SENTINEL" in cur_gtxt and "## BAZEL2GN SENTINEL" not in base_gtxt:
            return True
        base_pre = base_gtxt.split("## BAZEL2GN SENTINEL", 1)[0]
        cur_pre = cur_gtxt.split("## BAZEL2GN SENTINEL", 1)[0]
        base_t = {
            n
            for r, n in re.findall(r'^\s*([A-Za-z_]\w*)\(\s*"([^"]+)"\s*\)', base_pre, re.M)
            if r not in gn_meta_constructs
        }
        cur_t = {
            n
            for r, n in re.findall(r'^\s*([A-Za-z_]\w*)\(\s*"([^"]+)"\s*\)', cur_pre, re.M)
            if r not in gn_meta_constructs
        }
        return bool(base_t - cur_t)

    active_mig_dirs = [d for d in all_cand_dirs if is_dir_migrated_in_commit(d)]
    if active_mig_dirs:
        first = active_mig_dirs[0]
    resolved_subj_dir = resolve_subject_dir(subj_path) if subj_path else ""

    # Check that commit message paths are relative to the Fuchsia checkout root
    # (never prefixed with .jiri_root/worktrees/<name>/ or host filesystem paths)
    # and that partially migrated or already-migrated directories are caught.
    non_checkout_path_re = re.compile(
        r"(?:@?//)?\.jiri_root/(?:worktrees/[^/\s]+/)?([\w./\-]*)"
        r"|(?<![\w/:.])/(?:usr|home|tmp|var|opt)/[\w./\-]+"
    )

    migrated_list_headers = {
        "Also migrated in this change:",
        "Migrated directories:",
        "Migrated packages:",
        "Migrated targets:",
    }

    def check_checkout_relative_paths(msg_lines):
        in_also_migrated = False
        for idx, raw in enumerate(msg_lines, start=1):
            stripped = raw.strip()
            if stripped in migrated_list_headers:
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
                b_spec = bullet[2:].strip("/") if bullet.startswith("//") else ""
                b_dir = b_spec.split(":", 1)[0].strip("/")
                if not b_spec or b_spec.startswith("."):
                    msg_text = (
                        f"`Also migrated in this change:` bullet `{bullet}` is not a valid "
                        "Fuchsia checkout-relative `//<dir>` or `//<dir>:<target>` label."
                    )
                    rem_text = (
                        "Write each bullet as `- //<dir>` (when the entire directory is "
                        "migrated) or `- //<dir>:<target>` (when only specific targets in "
                        "`//<dir>` are migrated), or remove the block if no extra targets "
                        "were migrated."
                    )
                    add("commit_msg_non_checkout_path", idx, msg_text, rem_text, severity="error")
                elif b_dir in (first, resolved_subj_dir) and (
                    b_dir == resolved_subj_dir or len(active_mig_dirs) <= 1
                ):
                    msg_text = (
                        f"`Also migrated in this change:` bullet `{bullet}` duplicates the "
                        f"primary migrated directory `//{b_dir}`."
                    )
                    rem_text = (
                        "Remove the redundant bullet (or the entire `Also migrated in this "
                        f"change:` block when only `//{first}` is migrated in this change), "
                        "and pass checkout-relative paths in `CoderReport.modified_files`."
                    )
                    add("commit_body_redundant_also_migrated", idx, msg_text, rem_text)
                elif not is_dir_migrated_in_commit(b_dir):
                    msg_text = (
                        f"`Also migrated in this change:` bullet `{bullet}` names "
                        f"`//{b_dir}`, which was already migrated before this commit (no "
                        f"targets in `//{b_dir}` are newly migrated against base)."
                    )
                    rem_text = (
                        f"Remove `- {bullet}` from `Also migrated in this change:` (and "
                        "remove the entire `Also migrated in this change:` block if only "
                        f"`//{first}` is migrated in this change), and revert any leftover "
                        f"rebase edits in `{b_dir}/`."
                    )
                    add(
                        "also_migrated_already_migrated",
                        idx,
                        msg_text,
                        rem_text,
                        severity="error",
                    )
                elif ":" not in b_spec:
                    unmig_all, _, new_bz_lbls = inspect_dir_migration(b_dir)
                    if unmig_all:
                        sample = ", ".join(f"`{t}`" for t in unmig_all[:3])
                        examples = (
                            ", ".join(f"`- {lbl}`" for lbl in new_bz_lbls[:4])
                            if new_bz_lbls
                            else f"`- //{b_dir}:<target>`"
                        )
                        msg_text = (
                            f"`Also migrated in this change:` bullet `{bullet}` names the "
                            f"entire directory `//{b_dir}`, but `{b_dir}/BUILD.gn` still "
                            f"defines unmigrated targets ({sample})."
                        )
                        rem_text = (
                            f"Do not imply the entire `//{b_dir}` directory is migrated: "
                            "be specific about which targets (or exported files) are "
                            f"migrated by writing `- //{b_dir}:<target>` (e.g. {examples}), "
                            "or remove the bullet if no targets in that directory were "
                            "migrated."
                        )
                        add(
                            "also_migrated_partial_directory",
                            idx,
                            msg_text,
                            rem_text,
                            severity="error",
                        )

    # Check that a bare directory subject does not name a directory that was already migrated
    # before this commit or imply an entire directory is migrated when BUILD.gn still retains
    # unmigrated non-test targets.
    def check_subject_partial_directory(subj_str):
        m_subj = re.match(r"^\[bazel_migration\](?:\[[A-Za-z0-9_.\-]+\])? //(\S+)$", subj_str)
        if not m_subj:
            return
        raw_spec = m_subj.group(1).strip("/")
        subj_dir = resolve_subject_dir(raw_spec)
        if active_mig_dirs and subj_dir and not is_dir_migrated_in_commit(subj_dir):
            msg_text = (
                f"Commit subject `{subj_str}` names `//{subj_dir}`, which was already "
                f"migrated before this commit (no targets in `//{subj_dir}` are newly "
                f"migrated against base), whereas this change migrates `//{first}`."
            )
            rem_text = (
                f"Update the commit subject to name `//{first}` (the directory actually "
                f"migrated in this change), remove already-migrated directories from "
                "`Also migrated in this change:`, and revert any leftover rebase edits in "
                f"`{subj_dir}/`."
            )
            add(
                "commit_subject_already_migrated_directory",
                1,
                msg_text,
                rem_text,
                severity="error",
            )
            return
        if ":" in raw_spec:
            return
        unmig_all, unmig_non_test, new_bz_lbls = inspect_dir_migration(subj_dir)
        if unmig_non_test and new_bz_lbls and len(f"[bazel_migration] {new_bz_lbls[0]}") <= 65:
            sample = ", ".join(f"`{t}`" for t in unmig_non_test[:3])
            ex_lbl = new_bz_lbls[0]
            msg_text = (
                f"Commit subject `{subj_str}` names the bare directory `//{subj_dir}`, "
                f"but `{subj_dir}/BUILD.gn` still defines unmigrated targets ({sample})."
            )
            rem_text = (
                f"Be specific about which target is migrated (e.g. `{ex_lbl}`) instead "
                f"of implying that the entire `//{subj_dir}` directory is migrated."
            )
            add("commit_subject_partial_directory", 1, msg_text, rem_text, severity="error")

    # Check that any pre-existing source bitrot compile fix is explained in the commit body.
    source_suffixes = (".rs", ".cc", ".cpp", ".c", ".h", ".hpp")
    bitrot_reason_re = re.compile(
        r"\b(bitrot|unbuilt|not\s+built|disconnected|pre-existing|compile|build\s+error"
        r"|type\s+error|syntax|E0\d{3})\b",
        re.I,
    )

    def check_source_bitrot_explanation():
        for f in changed:
            if not f.endswith(source_suffixes):
                continue
            base_name = f.rsplit("/", 1)[-1]
            rel_names = [f, base_name]
            for d in dirs:
                if f.startswith(d + "/"):
                    rel_names.append(f[len(d) + 1:])
            mentioned = any(n in body for n in rel_names)
            has_reason = bool(bitrot_reason_re.search(body))
            if not (mentioned and has_reason):
                msg_text = (
                    f"The change modifies source file `{f}` (allowed by `build_only_scope` "
                    "only to repair pre-existing compile bitrot in an unbuilt GN package), "
                    "but the commit body does not mention the file and explain the "
                    "pre-existing compile errors, making the source diff appear unrelated "
                    "to the Bazel migration during review."
                )
                rem_text = (
                    f"Explain the `{f}` compile fixes in the commit body (naming `{base_name}` "
                    "and stating that the package was disconnected from the GN build graph and "
                    "had pre-existing compile errors that prevented the migrated Bazel target "
                    "from compiling). Keep the source diff strictly minimal (e.g. changing a "
                    "misplaced `///` doc comment inside a closure to `//` or moving it without "
                    "unrelated reformatting, and fixing only broken types/APIs)."
                )
                add("commit_body_unmentioned_source_fix", 1, msg_text, rem_text)

    check_redundant_body_title(lines)
    check_checkout_relative_paths(lines)
    check_subject_partial_directory(subject)
    check_source_bitrot_explanation()

    # Check subject and body line lengths (mirroring scripts/shac/commit_msg_checker.py).
    exempt_subject_re = re.compile(r"^(?:Revert(?:\^\d+)?|Reland|fixup!|squash!|amend!) ")
    exempt_footer_re = re.compile(
        r"^(Bug|Fixed|Test|Bazel-Migration-Target|Build-Only|Skip-Readme-Check|Change-Id|"
        r"Cq-Include-Trybots|Fuchsia-Auto-Submit|Multiply|Depends-on|Run-All-Tests|"
        r"Exempt|No-Tree-Checks|No-Presubmit|No-Try|TAG|CONV|Reviewed-on|Reviewed-by|"
        r"Commit-Queue|Presubmit-Verified|API-Review)[:=]"
    )

    def check_line_lengths(msg_lines):
        if msg_lines:
            subj = msg_lines[0]
            if len(subj) > 65 and not exempt_subject_re.match(subj):
                msg_text = (
                    f"Commit subject line exceeds 65 characters ({len(subj)} chars): "
                    f"{subj!r} (triggers shac `commit_msg` warning)."
                )
                rem_text = (
                    "Shorten the commit subject line to at most 65 characters "
                    "(recommended <= 50; abbreviate long middle path segments with "
                    "`//.../<leaf>` when needed while keeping the full target label "
                    "in `Bazel-Migration-Target:`)."
                )
                add("commit_subject_too_long", 1, msg_text, rem_text, severity="error")
        for idx, raw in enumerate(msg_lines[2:], start=3):
            if len(raw) <= 72:
                continue
            if re.search(r"https?://[^\s]+", raw):
                continue
            if exempt_footer_re.match(raw.strip()) or footer_line_re.match(raw.strip()):
                continue
            if raw.startswith(("    ", "\t", ">", "#", "```")):
                continue
            msg_text = (
                f"Commit message line {idx} exceeds 72 characters ({len(raw)} chars): "
                f"`{raw}` (triggers shac `commit_msg` warning: "
                f"`Line {idx} exceeds 72 characters ({len(raw)} chars). "
                "Wrap body text at 72 characters.`)."
            )
            rem_text = (
                "Wrap commit message body lines at 72 characters or fewer "
                "(only URL lines, indented code blocks, and single-line footers "
                "such as `Test:` and `Bazel-Migration-Target:` may exceed 72 characters)."
            )
            add("commit_body_line_too_long", idx, msg_text, rem_text, severity="error")

    check_line_lengths(lines)
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
    # Scan both primary target_dir packages and any other BUILD.bazel packages modified
    # in the commit so co-migrated targets are recognized when target_dir only updates
    # exports_files().
    is_empty = not changed and not git("status", "--porcelain", "--ignore-submodules=all")
    bmt_lines = [(i + 1, l) for i, l in enumerate(lines) if re.match(r"^Bazel-Migration-Target\s*:", l)]
    migrated_targets = []
    required_bmt_targets = []
    fallback_dir_targets = []
    excluded_bmt = {}
    helper_rules = {
        "package",
        "exports_files",
        "alias",
        "config_setting",
        "fx_packaged_binary",
        "fx_unittest_component",
    }
    synth_component_rules = {"fx_component_manifest", "fx_test_component"}
    ut_re = r'^\s*(with_unit_tests\s*=\s*"(host|both|fuchsia)"|with_unit_tests\s*=\s*True|with_host_unit_tests\s*=\s*True)'
    bmt_dirs = list(
        dict.fromkeys(
            dirs
            + [
                os.path.dirname(f)
                for f in changed
                if f.endswith("/BUILD.bazel") and os.path.dirname(f)
            ]
        )
    )
    kept_gn_by_dir = {}
    for d in bmt_dirs:
        try:
            with open(f"{workdir}/{d}/BUILD.bazel") as fh:
                btxt = fh.read()
        except OSError:
            continue
        try:
            with open(f"{workdir}/{d}/BUILD.gn") as fh:
                cur_gtxt = fh.read()
        except OSError:
            cur_gtxt = ""
        pre_gtxt = cur_gtxt.split("## BAZEL2GN SENTINEL", 1)[0] if cur_gtxt else ""
        cur_suite_names = set(
            re.findall(
                r'^\s*(?:bazel_test_suite|bazel_host_tool)\(\s*"([^"]+)"\s*\)',
                cur_gtxt,
                re.M,
            )
        )
        kept_gn_targets = {}
        for grule, gname in re.findall(
            r'^\s*([A-Za-z_]\w*)\(\s*"([^"]+)"\s*\)', pre_gtxt, re.M
        ):
            if grule not in gn_meta_constructs and gname not in cur_suite_names:
                kept_gn_targets[gname] = grule
        kept_gn_by_dir[d] = kept_gn_targets
        base_btxt = git("show", f"{base_rev}:{d}/BUILD.bazel")
        base_gtxt = git("show", f"{base_rev}:{d}/BUILD.gn")
        base_bazel_names = set()
        for m in re.finditer(r"^([A-Za-z_]\w*)\((?:[^\n#]*\)|.*?^\))", base_btxt, re.M | re.S):
            n = re.search(r'^\s*name\s*=\s*"([^"]+)"', m.group(0), re.M)
            if n:
                base_bazel_names.add(n.group(1))
                if re.search(ut_re, m.group(0), re.M):
                    base_bazel_names.add(f"{n.group(1)}_test")
        post_gtxt = (
            cur_gtxt.split("## BAZEL2GN SENTINEL", 1)[1]
            if "## BAZEL2GN SENTINEL" in cur_gtxt
            else ""
        )
        base_gn_names = {
            gname
            for grule, gname in re.findall(
                r'^\s*([A-Za-z_]\w*)\(\s*"([^"]+)"\s*\)',
                base_gtxt + "\n" + post_gtxt,
                re.M,
            )
            if grule not in gn_meta_constructs
        }
        blocks = list(re.finditer(r"^([A-Za-z_]\w*)\((?:[^\n#]*\)|.*?^\))", btxt, re.M | re.S))
        explicit_names = set()
        test_wrapped_pkgs = set()
        wrapped_bins = set()
        wrapped_test_bins = set()
        for m in blocks:
            rule, blk = m.group(1), m.group(0)
            n = re.search(r'^\s*name\s*=\s*"([^"]+)"', blk, re.M)
            if n:
                explicit_names.add(n.group(1))
            if rule == "fx_test":
                p = re.search(r'^\s*package\s*=\s*":([^"]+)"', blk, re.M)
                if p:
                    test_wrapped_pkgs.add(p.group(1))
            if rule in ("host_test", "fx_packaged_binary", "fx_unittest_component"):
                b = re.search(r'^\s*binary\s*=\s*":([^"]+)"', blk, re.M)
                if b:
                    if b.group(1) not in base_gn_names:
                        wrapped_bins.add(b.group(1))
                    if rule in ("host_test", "fx_unittest_component") or re.search(
                        r"^\s*testonly\s*=\s*True\b", blk, re.M
                    ):
                        wrapped_test_bins.add(b.group(1))
            if rule == "wrap_host_rust_test":
                bn = re.search(r'^\s*binary_name\s*=\s*"([^"]+)"', blk, re.M)
                if bn:
                    wrapped_test_bins.add(bn.group(1))
            if rule == "fx_unittest_package":
                u = re.search(r'unit_tests\s*=\s*\[(.*?)\]', blk, re.S)
                if u:
                    for b in re.findall(r'":([^"]+)"', u.group(1)):
                        wrapped_test_bins.add(b)
                        if b not in base_gn_names:
                            wrapped_bins.add(b)
        all_dir_targets, new_dir_targets, dir_required_bmt = [], [], []
        for m in blocks:
            rule, blk = m.group(1), m.group(0)
            n = re.search(r'^\s*name\s*=\s*"([^"]+)"', blk, re.M)
            if not n:
                continue
            tname = n.group(1)
            lbl = f"//{d}:{tname}"
            if (
                tname in kept_gn_targets
                and has_bazel2gn_skip(btxt, m.start())
                and rule not in ("fx_test_component", "rustc_test", "go_test")
            ):
                grule = kept_gn_targets[tname]
                excluded_bmt[lbl] = (
                    f'remains hand-written as `{grule}("{tname}")` above the BAZEL2GN SENTINEL '
                    f'in `{d}/BUILD.gn` (only copied/skipped in `BUILD.bazel`, not migrated from GN)'
                )
                continue
            is_synth_comp = rule in synth_component_rules and tname not in base_gn_names
            if (
                rule in helper_rules
                or is_synth_comp
                or (rule == "fx_package" and tname in test_wrapped_pkgs)
                or tname in wrapped_bins
            ):
                excluded_bmt[lbl] = (
                    "is an internal Bazel packaging/test-wrapper helper target, "
                    "not a migrated GN target"
                )
                continue
            all_dir_targets.append(lbl)
            if tname not in base_bazel_names:
                new_dir_targets.append(lbl)
                if (
                    tname in base_gn_names
                    and tname not in wrapped_test_bins
                    and rule != "test_suite"
                ):
                    dir_required_bmt.append(lbl)
            if re.search(ut_re, blk, re.M) and f"{tname}_test" not in explicit_names:
                tlbl = f"//{d}:{tname}_test"
                all_dir_targets.append(tlbl)
                if f"{tname}_test" not in base_bazel_names:
                    new_dir_targets.append(tlbl)
        for tname, grule in kept_gn_targets.items():
            lbl = f"//{d}:{tname}"
            if lbl not in excluded_bmt and tname not in explicit_names:
                excluded_bmt[lbl] = (
                    f'remains unmigrated as `{grule}("{tname}")` in `{d}/BUILD.gn`'
                )
        if new_dir_targets:
            for lbl in all_dir_targets:
                if lbl not in new_dir_targets:
                    excluded_bmt[lbl] = "already existed in `BUILD.bazel` before this commit"
            migrated_targets.extend(new_dir_targets)
            if dir_required_bmt:
                required_bmt_targets.extend(dir_required_bmt)
            elif not base_gn_names and len(new_dir_targets) == 1:
                required_bmt_targets.extend(new_dir_targets)
        elif d in dirs and is_dir_migrated_in_commit(d):
            fallback_dir_targets.extend(all_dir_targets)
        else:
            for lbl in all_dir_targets:
                excluded_bmt[lbl] = "already existed in `BUILD.bazel` before this commit"
    if not migrated_targets:
        migrated_targets.extend(fallback_dir_targets)
    if not is_empty:
        migrated_targets = list(dict.fromkeys(migrated_targets))
        required_bmt_targets = list(dict.fromkeys(required_bmt_targets))
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
        build_file_bmt_re = re.compile(
            r"(?:^|/|:)(?:BUILD(?:\.gn|\.bazel)?|[\w.\-]+\.(?:gn|gni|bazel|bzl))$"
        )
        present_bmt = set()
        for ln, l in bmt_lines:
            val = l.split(":", 1)[1].strip()
            canon_val = val if ":" in val else f"{val}:{val.rsplit('/', 1)[-1]}"
            present_bmt.add(val)
            present_bmt.add(canon_val)
            wrapped = (
                ln < len(lines)
                and bool(lines[ln].strip())
                and not re.match(r"^[A-Za-z0-9\-]+\s*:", lines[ln])
            )
            leaf_tok = val.split(":", 1)[-1].rsplit("/", 1)[-1]
            is_build_filename = leaf_tok in ("BUILD.gn", "BUILD.bazel", "BUILD")
            if build_file_bmt_re.search(val) or is_build_filename:
                add(
                    "bazel_migration_target_footer_format",
                    ln,
                    f"`Bazel-Migration-Target:` footer `{l}` names a build "
                    f"file (`{val}`) rather than a migrated target label.",
                    "Replace the file path with one "
                    "`Bazel-Migration-Target: //<dir>:<target>` line per "
                    f"migrated target, e.g. {example_bmt}.",
                )
            elif not re.match(r"^//[\w./\-]+(:[\w.\-]+)?$", val) or wrapped:
                add(
                    "bazel_migration_target_footer_format",
                    ln,
                    f"`Bazel-Migration-Target:` footer `{l}` must specify a "
                    "single canonical `//<dir>:<target>` (or `//<dir>`) target label "
                    "on one unwrapped line (without a leading `@` or `/BUILD.gn` suffix).",
                    "Write one unwrapped line per migrated target (may exceed "
                    f"72 characters), e.g. {example_bmt}.",
                )
            elif canon_val in excluded_bmt:
                add(
                    "bazel_migration_target_not_migrated",
                    ln,
                    f"`Bazel-Migration-Target:` footer `{l}` names `{val}`, "
                    f"which {excluded_bmt[canon_val]}.",
                    "List only targets migrated from GN in this commit, "
                    f"e.g. {example_bmt}.",
                    severity="error",
                )
        # Check that no migrated GN target is omitted when Bazel-Migration-Target footers exist.
        if bmt_lines and required_bmt_targets:
            missing_bmt = [
                lbl for lbl in required_bmt_targets if lbl not in present_bmt
            ]
            if missing_bmt:
                miss_list = ", ".join(f"`{lbl}`" for lbl in missing_bmt)
                miss_footers = ", ".join(
                    f"`Bazel-Migration-Target: {lbl}`" for lbl in missing_bmt
                )
                add(
                    "missing_bazel_migration_target_footer",
                    bmt_lines[-1][0],
                    "Commit message is missing `Bazel-Migration-Target:` "
                    f"footer(s) for migrated target(s): {miss_list}.",
                    f"Add {miss_footers} to the `Bazel-Migration-Target:` "
                    "footer block so every target migrated from GN in this "
                    "commit is recorded.",
                    severity="error",
                )

        all_changed = set(changed) | {f for f in git("diff", "--name-only", base_rev).splitlines() if f}

        # Check that a `Migrate ...` sentence in the commit body does not claim to migrate
        # targets that still remain hand-written in BUILD.gn or claim to migrate/sync BUILD.gn
        # when the package's BUILD.gn file was not touched by the commit.
        def check_body_unmigrated_targets(msg_lines, kept_by_dir, mig_lbls, changed_paths):
            for start_ln, end_ln, para in body_paragraphs(msg_lines):
                for sent in re.split(r"(?<=\.)\s+", para):
                    is_mig_sent = bool(
                        re.match(r"^(?:this\s+(?:cl|change|commit|patch)\s+)?migrates?\b", sent, re.I)
                    )
                    claims_gn_sync = bool(
                        re.search(r"\b(?:sync|regenerate)\s+BUILD\.gn\b|\bvia\s+fx\s+bazel2gn\b", sent, re.I)
                    )
                    if not (is_mig_sent or claims_gn_sync):
                        continue
                    clause = re.split(
                        r"\b(?:while\s+keeping|keeping|and\s+keep)\b", sent, maxsplit=1, flags=re.I
                    )[0]
                    for d, kmap in kept_by_dir.items():
                        gn_rel = f"{d}/BUILD.gn"
                        gn_untouched = (
                            os.path.isfile(os.path.join(workdir, gn_rel))
                            and gn_rel not in changed_paths
                        )
                        if is_mig_sent:
                            for kname, krule in kmap.items():
                                pat = (
                                    r'(?:[A-Za-z_]\w*\(\s*"'
                                    + re.escape(kname)
                                    + r'"\s*\)|`?'
                                    + re.escape(f":{kname}")
                                    + r'`?)'
                                )
                                if re.search(pat, clause):
                                    gn_note = (
                                        f" and `{gn_rel}` was not modified in this commit"
                                        if gn_untouched
                                        else ""
                                    )
                                    msg_text = (
                                        f'Commit body claims to migrate `{kname}`, which remains '
                                        f'hand-written as `{krule}("{kname}")` in `{gn_rel}`'
                                        f"{gn_note} (copied with `# @bazel2gn:skip` in "
                                        "`BUILD.bazel`, not migrated from GN)."
                                    )
                                    rem_text = (
                                        "Describe `# @bazel2gn:skip` targets whose GN definitions "
                                        f"remain in `{gn_rel}` as copied to `BUILD.bazel` while "
                                        f"keeping `{gn_rel}` unchanged (do not claim `Migrate ...` "
                                        f"or `sync BUILD.gn via fx bazel2gn` when `{gn_rel}` is "
                                        "untouched), and remove any unreferenced `# @bazel2gn:skip` "
                                        "duplicate targets from `BUILD.bazel`."
                                    )
                                    add(
                                        "commit_body_claims_unmigrated_target",
                                        start_ln,
                                        msg_text,
                                        rem_text,
                                        severity="error",
                                    )
                                    return
                        if claims_gn_sync and gn_untouched and (d in sent or len(kept_by_dir) == 1):
                            msg_text = (
                                f"Commit body claims to sync `{gn_rel}` via `fx bazel2gn`, "
                                f"but `{gn_rel}` was not modified in this commit."
                            )
                            rem_text = (
                                f"Describe the `# @bazel2gn:skip` targets in `{d}/BUILD.bazel` "
                                f"as copied to `BUILD.bazel` while keeping `{gn_rel}` unchanged, "
                                "and remove the false `sync BUILD.gn via fx bazel2gn` claim."
                            )
                            add(
                                "commit_body_claims_unmigrated_target",
                                start_ln,
                                msg_text,
                                rem_text,
                                severity="error",
                            )
                            return

        check_body_unmigrated_targets(lines, kept_gn_by_dir, migrated_targets, all_changed)
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
        # Build-Only footer check: fxr/1913427 (Bug 576757166) allows `Build-Only: true` on
        # [bazel_migration] CLs that touch BUILD.gn and BUILD.bazel files exclusively,
        # skipping device/emulator tests in CQ to accelerate presubmit turnaround.
        build_only_lines = [(i + 1, l) for i, l in enumerate(lines) if re.match(r"^Build-Only\s*:", l, re.I)]
        aib_paths = {f for f in all_changed if f.startswith(("bundles/assembly/", "build/images/")) or (f.startswith("vendor/") and "/assembly/" in f)}
        non_build_files = {f for f in all_changed if not (f.endswith("/BUILD.gn") or f.endswith("/BUILD.bazel") or f in ("BUILD.gn", "BUILD.bazel"))}
        if build_only_lines:
            for ln, l in build_only_lines:
                val = l.split(":", 1)[1].strip()
                if val.lower() not in ("true", "1"):
                    add("build_only_invalid_value", ln,
                        f"`Build-Only:` footer `{l}` has invalid value `{val}` (expected `true`).",
                        "Write `Build-Only: true` in the contiguous footer block.", severity="error")
                if not subject.startswith("[bazel_migration]"):
                    add("build_only_invalid_subject", ln,
                        "`Build-Only: true` is only permitted on changes whose subject begins with `[bazel_migration]` "
                        "(per Gerrit submit requirement No-Build-Only-Footer in fxr/1913427).",
                        "Ensure the commit subject begins with `[bazel_migration][<subsystem>] //<dir>` or remove `Build-Only: true`.",
                        severity="error")
                if non_build_files:
                    sample_bad = ", ".join(f"`{f}`" for f in sorted(non_build_files)[:3])
                    add("build_only_non_build_file_modified", ln,
                        "`Build-Only: true` is only permitted when the change modifies `BUILD.gn` and `BUILD.bazel` files "
                        f"exclusively (per Gerrit submit requirement No-Build-Only-Footer in fxr/1913427). This change modifies "
                        f"non-build file(s): {sample_bad}.",
                        "Remove `Build-Only: true` from the commit footers because changes touching non-BUILD files cannot skip presubmit tests.",
                        severity="error")
                if aib_paths:
                    sample_aib = ", ".join(f"`{f}`" for f in sorted(aib_paths)[:3])
                    add("build_only_assembly_modified", ln,
                        f"`Build-Only: true` must not be used on changes that modify Assembly Input Bundles (AIBs) or product contents ({sample_aib}).",
                        "Remove `Build-Only: true` from the commit footers.", severity="error")
        elif subject.startswith("[bazel_migration]") and not non_build_files and not aib_paths and dirs and changed:
            add("build_only_eligible", footer_ln,
                "This change modifies `BUILD.gn` and `BUILD.bazel` exclusively and does not touch Assembly Input Bundles (AIBs). "
                "It is eligible for the `Build-Only: true` footer (allowed under fxr/1913427).",
                "Consider adding `Build-Only: true` to the contiguous footer block to skip device/emulator presubmit testing in CQ and accelerate landing turnaround.",
                severity="info")

        # Check for unwarranted Cq-Include-Trybots: footers when migrated build files
        # have no architecture-specific conditionals matching the requested trybot
        # and no source files were modified for bitrot repair.
        def check_cq_include_trybots(msg_lines):
            if any(f.endswith(source_suffixes) for f in changed):
                return
            cq_lines = [
                (i + 1, l)
                for i, l in enumerate(msg_lines)
                if re.match(r"^Cq-Include-Trybots\s*:", l, re.I)
            ]
            if not cq_lines:
                return
            build_blobs = []
            for d in dirs:
                for fn in ("BUILD.bazel", "BUILD.gn"):
                    p = os.path.join(workdir, d, fn)
                    if os.path.isfile(p):
                        try:
                            build_blobs.append(open(p, encoding="utf-8").read())
                        except OSError:
                            pass
            combined_builds = "\n".join(build_blobs)
            for ln, l in cq_lines:
                arch_tokens = re.findall(r"\b(riscv64|arm64|x64)\b", l)
                if not arch_tokens or not all(
                    re.search(r"\b" + re.escape(tok) + r"\b", combined_builds)
                    for tok in arch_tokens
                ):
                    msg_text = (
                        f"Commit message includes `{l}`, which specifies extra trybot(s) not "
                        "motivated by architecture-specific conditions in the migrated build files."
                    )
                    rem_text = (
                        "Remove the `Cq-Include-Trybots:` footer line from the commit message; "
                        "default CQ presubmit builders already cover standard GN-to-Bazel "
                        "migrations."
                    )
                    add(
                        "unnecessary_cq_include_trybots_footer",
                        ln,
                        msg_text,
                        rem_text,
                        severity="error",
                    )

        check_cq_include_trybots(lines)
        nonempty_idxs = [i for i, l in enumerate(lines) if l.strip()]
        if nonempty_idxs:
            final_start = nonempty_idxs[-1]
            while final_start > 0 and lines[final_start - 1].strip():
                final_start -= 1
            for ln, l in bmt_lines + test_lines + skip_readme_lines + build_only_lines:
                if ln - 1 < final_start:
                    add("footer_block_not_contiguous", ln,
                        f"Footer `{l}` is separated from the final footer block by a blank line, so Gerrit will not "
                        "index it as a commit footer (breaking `hasfooter:Bazel-Migration-Target` queries).",
                        "Group all footers (`Bug:`/`Fixed:`, `Skip-Readme-Check:`, `Build-Only:`, `Test:`, `Bazel-Migration-Target:`, `TAG:`, "
                        "`Change-Id:`) into a single contiguous block at the very bottom of the commit message with "
                        "no blank lines between them.")
                    break
print(json.dumps(findings, indent=2))
PYEOF
