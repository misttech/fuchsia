#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Triage WLAN test failures across all builders in the wlan_e2e LUCI console."""

import argparse
import json
import os
import re
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timedelta, timezone
from typing import Any, Optional

PROJECT = "turquoise"
CONSOLE_GROUP = "wlan_e2e"
BUILDS_PER_BUILDER = 3
LAST_SCAN_CACHE_PATH = "/tmp/wlan_e2e_console_triage_last_scan.json"
DEFAULT_WLAN_COMPONENT_ID = "1007819"
ISSUES_BIN = "/google/bin/releases/issues-cli/issues"
NO_ERR_MSG = "No primary error message found"

# Generic wrapper catch-all bug in turquoise ("reason LIKE 'exit status %'", b/519159054).
IGNORED_WRAPPER_BUG_IDS = {"519159054"}


def _dedup(items: Any) -> list[Any]:
    return list(dict.fromkeys(items))


def _short_builder(builder: str) -> str:
    return builder.split("/")[-1].removeprefix("fuchsia_internal.")


def _is_specific_reason(reason: str) -> bool:
    stripped = re.sub(rf"\[[A-Za-z0-9_.]+\]|{NO_ERR_MSG}", "", reason)
    return len(re.findall(r"[A-Za-z0-9]", stripped)) >= 4


def extract_top_level_suite_id(test_id: str) -> str:
    """Extracts the top-level .sh suite test ID if test_id is a sub-test case."""
    return re.sub(r"(\.sh[^/]*)/.*$", r"\1", test_id)


def extract_base_sh_prefix(test_id: str) -> str:
    """Extracts the board-agnostic .sh prefix used by LUCI Analysis test rules."""
    return re.sub(r"\.sh\b.*", ".sh", test_id)


def extract_board(test_id: str, builder: str = "") -> str:
    """Extracts the target board (e.g., astro, iris, sorrel, nelson) from the test ID or builder."""
    if m := re.search(r"\.sh[^/]*\.([a-z0-9_]+)(?:/|$)", test_id):
        return m.group(1)
    if m := re.search(
        r"\.(?!arm64-|x64-|riscv64-)([a-z0-9_]+)-(?:release|debug)\b", builder
    ):
        return m.group(1)
    return "unknown"


def run_prpc(
    host: str, service_method: str, payload: dict[str, Any]
) -> dict[str, Any]:
    try:
        res = subprocess.run(
            ["prpc", "call", host, service_method],
            input=json.dumps(payload),
            capture_output=True,
            text=True,
            check=True,
        )
    except FileNotFoundError:
        sys.exit(
            "Error: Missing required command: prpc\n"
            "Install depot_tools: http://go/depottools#_setting_up"
        )
    except subprocess.CalledProcessError as e:
        err = (e.stderr or e.stdout or str(e)).strip()
        raise RuntimeError(f"prpc {service_method} failed: {err}") from e
    out = res.stdout.strip()
    return json.loads(out) if out else {}


def list_wlan_e2e_builders() -> list[tuple[str, str, str]]:
    """Fetches all builders in the wlan_e2e console group."""
    resp = run_prpc(
        "luci-milo.appspot.com",
        "luci.milo.v1.MiloInternal.ListBuilders",
        {"project": PROJECT, "group": CONSOLE_GROUP, "pageSize": 1000},
    )
    builders = [
        (bid["project"], bid["bucket"], bid["builder"])
        for b in resp.get("builders", [])
        if (bid := b.get("id", {})).get("project")
        and bid.get("bucket")
        and bid.get("builder")
    ]
    if not builders:
        raise RuntimeError(
            "No builders returned by MiloInternal.ListBuilders for wlan_e2e."
        )
    return builders


def fetch_recent_builds_for_builder(
    project: str, bucket: str, builder: str, now: datetime
) -> tuple[str, list[dict[str, Any]], Optional[str]]:
    """Fetches the last 3 completed builds for a builder and filters by age (<24h or <7d for weekend)."""
    is_weekend = "weekend" in builder.lower()
    cutoff = now - (timedelta(days=7) if is_weekend else timedelta(hours=24))
    window_label = "7d" if is_weekend else "24h"
    b_short = f"{bucket}/{builder}"
    b_full = f"{project}/{b_short}"
    payload = {
        "predicate": {
            "builder": {
                "project": project,
                "bucket": bucket,
                "builder": builder,
            },
            "status": "ENDED_MASK",
        },
        "pageSize": BUILDS_PER_BUILDER,
        "fields": "builds.*.id,builds.*.createTime,builds.*.endTime",
    }
    try:
        resp = run_prpc(
            "cr-buildbucket.appspot.com",
            "buildbucket.v2.Builds.SearchBuilds",
            payload,
        )
    except Exception as e:
        return (
            b_short,
            [],
            f"WARNING: Failed to query builds for {b_full}: {e}",
        )

    builds = resp.get("builds", [])
    matching = [
        b
        for b in builds
        if (ts := b.get("endTime") or b.get("createTime"))
        and datetime.fromisoformat(ts) >= cutoff
    ]
    if matching:
        return b_short, matching, None

    latest_info = (
        f"most recent completed build b{builds[0].get('id')} finished at "
        f"{builds[0].get('endTime') or builds[0].get('createTime') or 'unknown time'}"
        if builds
        else "no completed builds found"
    )
    return (
        b_short,
        [],
        f"WARNING: Builder {b_full} has 0 completed builds in the last {window_label} ({latest_info}).",
    )


def fetch_wlan_failures_for_build(
    build_info: dict[str, Any], builder: str
) -> list[dict[str, Any]]:
    """Queries ResultDB for unexpected WLAN test failures in a single build."""
    build_id = build_info["id"]
    build_date = (
        build_info.get("createTime") or build_info.get("endTime") or ""
    )[:10]
    payload = {
        "invocations": [f"invocations/build-{build_id}"],
        "predicate": {
            "testIdRegexp": ".*[wW][lL][aA][nN].*",
            "expectancy": "VARIANTS_WITH_UNEXPECTED_RESULTS",
        },
        "pageSize": 1000,
        "readMask": "test_id,status,expected,failure_reason",
    }
    try:
        resp = run_prpc(
            "results.api.luci.app",
            "luci.resultdb.v1.ResultDB.QueryTestResults",
            payload,
        )
    except Exception as e:
        print(
            f"Warning: QueryTestResults failed for build-{build_id}: {e}",
            file=sys.stderr,
        )
        return []

    by_tid = {
        r.get("testId", ""): r
        for r in resp.get("testResults", [])
        if r.get("status", "") not in ("PASS", "SKIP")
        and r.get("expected", False) is not True
    }
    # Prefer sub-test case records over generic top-level .sh wrapper records per suite.
    suites_with_subtests = {
        s for tid in by_tid if (s := extract_top_level_suite_id(tid)) != tid
    }

    grouped_in_build: dict[tuple[str, str], dict[str, Any]] = {}
    for tid, r in by_tid.items():
        if tid in suites_with_subtests:
            continue
        board = extract_board(tid, builder)
        if board == "sorrel" and "adb" in tid.lower():
            continue
        suite_id = extract_top_level_suite_id(tid)
        primary_err = (
            r.get("failureReason", {}).get("primaryErrorMessage") or ""
        ).strip() or NO_ERR_MSG
        key = (suite_id, primary_err)
        if key not in grouped_in_build:
            grouped_in_build[key] = {
                "builder": builder,
                "board": board,
                "build_date": build_date,
                "build_url": f"https://ci.chromium.org/b/{build_id}",
                "test_id": tid,
                "sub_test_count": 1,
                "base_sh_prefix": extract_base_sh_prefix(tid),
                "failure_reason": primary_err,
            }
        else:
            grouped_in_build[key]["sub_test_count"] += 1

    return list(grouped_in_build.values())


def _fetch_bug_metadata(bug_ids: list[str]) -> dict[str, dict[str, str]]:
    """Queries issues-cli batch-get for priority and assignee of associated bugs."""
    if not bug_ids:
        return {}
    try:
        res = subprocess.run(
            [
                ISSUES_BIN,
                "readonly",
                "batch-get",
                "--issue_ids",
                ",".join(bug_ids),
            ],
            capture_output=True,
            text=True,
            check=True,
        )
    except Exception as e:
        print(
            f"Warning: Failed to fetch bug metadata via issues-cli ({e}).",
            file=sys.stderr,
        )
        return {}

    meta: dict[str, dict[str, str]] = {}
    for line in res.stdout.splitlines():
        if m := re.match(
            r"^- Issue (\d+): .*\(Priority:\s*([^,)]*),\s*Assignee:\s*([^)]*)\)\s*$",
            line,
        ):
            bug_id, priority, assignee = m.groups()
            meta[bug_id] = {
                "priority": priority.strip() or "unknown",
                "assignee": assignee.strip() or "unassigned",
            }
    return meta


def populate_cluster_bugs(failures: list[dict[str, Any]]) -> None:
    """Queries LUCI Analysis Clusters.Cluster, Rules.Get, and issues-cli to find associated bugs for each failure."""
    if not failures:
        return

    test_results_payload = [
        {
            "requestTag": str(i),
            "testId": tid,
            "failureReason": (
                {"primaryErrorMessage": f["failure_reason"][:1000]}
                if f["failure_reason"] != NO_ERR_MSG
                else {}
            ),
        }
        for i, f in enumerate(failures)
        for tid in _dedup(
            [f["test_id"], extract_top_level_suite_id(f["test_id"])]
        )
    ]

    clustered_by_idx: dict[int, list[dict[str, Any]]] = {
        i: [] for i in range(len(failures))
    }
    for start in range(0, len(test_results_payload), 300):
        chunk = test_results_payload[start : start + 300]
        try:
            resp = run_prpc(
                "analysis.api.luci.app",
                "luci.analysis.v1.Clusters.Cluster",
                {"project": PROJECT, "testResults": chunk},
            )
            for item in resp.get("clusteredTestResults", []):
                clustered_by_idx[int(item["requestTag"])].extend(
                    c
                    for c in item.get("clusters", [])
                    if c.get("clusterId", {}).get("algorithm") == "rules"
                    and c.get("clusterId", {}).get("id")
                    and (bid := c.get("bug", {}).get("id", ""))
                    and bid not in IGNORED_WRAPPER_BUG_IDS
                )
        except Exception as e:
            print(f"Warning: Clusters.Cluster failed: {e}", file=sys.stderr)

    matched_rule_ids = _dedup(
        c["clusterId"]["id"]
        for clist in clustered_by_idx.values()
        for c in clist
    )
    matched_bug_ids = _dedup(
        c["bug"]["id"] for clist in clustered_by_idx.values() for c in clist
    )

    def _fetch_rule(rid: str) -> tuple[str, str]:
        try:
            r = run_prpc(
                "analysis.api.luci.app",
                "luci.analysis.v1.Rules.Get",
                {"name": f"projects/{PROJECT}/rules/{rid}"},
            )
            return rid, r.get("ruleDefinition", "")
        except Exception as e:
            print(
                f"Warning: Failed to fetch LUCI rule {rid} ({e}).",
                file=sys.stderr,
            )
            return rid, ""

    with ThreadPoolExecutor(max_workers=8) as executor:
        bug_meta_future = executor.submit(_fetch_bug_metadata, matched_bug_ids)
        rule_defs = dict(executor.map(_fetch_rule, matched_rule_ids))
        bug_meta = bug_meta_future.result()

    for i, f in enumerate(failures):
        f["associated_bugs"] = list(
            {
                c["bug"]["id"]: {
                    "bug_id": c["bug"]["id"],
                    "link_text": c["bug"].get(
                        "linkText", f"b/{c['bug']['id']}"
                    ),
                    "priority": bug_meta.get(c["bug"]["id"], {}).get(
                        "priority", "unknown"
                    ),
                    "assignee": bug_meta.get(c["bug"]["id"], {}).get(
                        "assignee", "unknown"
                    ),
                    "rule_definition": rule_defs.get(c["clusterId"]["id"], ""),
                }
                for c in clustered_by_idx[i]
            }.values()
        )


def _format_occ_stats(occs: list[dict[str, Any]]) -> str:
    boards = sorted({o["board"] for o in occs})
    builds = len({o["build_url"] for o in occs})
    sub_tests = sum(o["sub_test_count"] for o in occs)
    builders = sorted({_short_builder(o["builder"]) for o in occs})
    dates = sorted({o["build_date"] for o in occs})
    return (
        f"[board: {', '.join(boards)}] "
        f"({builds}b/{sub_tests}t on {', '.join(builders)}; {', '.join(dates)})"
    )


def handle_scan(_args: argparse.Namespace) -> None:
    now = datetime.now(timezone.utc)
    builders = sorted(list_wlan_e2e_builders(), key=lambda b: (b[1], b[2]))

    with ThreadPoolExecutor(max_workers=16) as executor:
        builder_reports = list(
            executor.map(
                lambda b: fetch_recent_builds_for_builder(
                    b[0], b[1], b[2], now
                ),
                builders,
            )
        )

    warnings = [w for _, _, w in builder_reports if w]
    builds_to_query = [
        (b, builder)
        for builder, matching, _ in builder_reports
        for b in matching
    ]

    all_failures = []
    with ThreadPoolExecutor(max_workers=16) as executor:
        for res in executor.map(
            lambda item: fetch_wlan_failures_for_build(*item), builds_to_query
        ):
            all_failures.extend(res)

    populate_cluster_bugs(all_failures)

    associated = [f for f in all_failures if f.get("associated_bugs")]
    unassociated = [f for f in all_failures if not f.get("associated_bugs")]

    assoc_grouped_map: dict[str, list[dict[str, Any]]] = {}
    for f in associated:
        bug_key = ",".join(sorted(b["bug_id"] for b in f["associated_bugs"]))
        assoc_grouped_map.setdefault(bug_key, []).append(f)

    unassoc_grouped_map: dict[tuple[str, str], list[dict[str, Any]]] = {}
    for f in unassociated:
        key = (
            ("", f["failure_reason"])
            if _is_specific_reason(f["failure_reason"])
            else (f["base_sh_prefix"], f["failure_reason"])
        )
        unassoc_grouped_map.setdefault(key, []).append(f)

    unassociated_groups = []
    for (_, reason), occs in unassoc_grouped_map.items():
        unassociated_groups.append(
            {
                "suite_prefixes": sorted({o["base_sh_prefix"] for o in occs}),
                "test_ids": sorted({o["test_id"] for o in occs}),
                "boards": sorted({o["board"] for o in occs}),
                "failure_reason": reason,
                "builders": sorted({o["builder"] for o in occs}),
                "dates": sorted({o["build_date"] for o in occs}),
                "sample_build_url": occs[0]["build_url"],
                "_stats": _format_occ_stats(occs),
            }
        )

    unassociated_groups.sort(
        key=lambda g: (
            g["suite_prefixes"][0].rsplit("/", 1)[-1],
            g["failure_reason"],
        )
    )

    print(
        f"=== WLAN E2E Console Triage ({now.strftime('%Y-%m-%d %H:%M UTC')}) | "
        f"Builders: {len(builders)} | Recent builds: {len(builds_to_query)} ==="
    )
    active_builder_summaries = [
        f"{_short_builder(builder)} ({len(matching)})"
        for builder, matching, _ in builder_reports
        if matching
    ]
    if active_builder_summaries:
        print(f"  Active builders: {', '.join(active_builder_summaries)}")
    for w in warnings:
        print(f"  ! {w}")

    if not builds_to_query:
        return
    if not all_failures:
        print("  No WLAN test failures found in the examined builds!")
        return

    print(
        f"=== Failures WITH Associated Bugs ({len(assoc_grouped_map)} bug group(s)) ==="
    )
    if not assoc_grouped_map:
        print("  None.")
    else:
        for occs in assoc_grouped_map.values():
            bugs = occs[0]["associated_bugs"]
            bug_parts = [
                f"{b['link_text']} (Priority: {b['priority']}, Assignee: {b['assignee']})"
                for b in bugs
            ]
            suites = sorted(
                {o["base_sh_prefix"].rsplit("/", 1)[-1] for o in occs}
            )
            reasons = _dedup(o["failure_reason"] for o in occs)
            print(
                f"  - {', '.join(bug_parts)}: {', '.join(suites)} "
                f"{_format_occ_stats(occs)}"
            )
            for b in bugs:
                if b.get("rule_definition"):
                    print(
                        f"      Rule ({b['link_text']}): {b['rule_definition']}"
                    )
            for r_text in reasons:
                print(f"      Reason: {r_text}")

    print(
        f"=== Failures WITHOUT an Associated Bug ({len(unassociated_groups)} group(s)) ==="
    )
    if not unassociated_groups:
        with open(LAST_SCAN_CACHE_PATH, "w", encoding="utf-8") as f:
            json.dump({"unassociated_groups": []}, f, indent=2)
        print(
            "  All WLAN test failures already have an associated bug in LUCI Analysis!"
        )
        return

    for idx, g in enumerate(unassociated_groups, 1):
        stats = g.pop("_stats")
        prefixes = g["suite_prefixes"]
        suites = [p.rsplit("/", 1)[-1] for p in prefixes]
        sample_tid = g["test_ids"][0]
        header_label = (
            sample_tid
            if len(suites) == 1
            else f"{len(suites)} suites ({', '.join(suites)}) [sample: {sample_tid}]"
        )
        print(f"  ({idx}) {header_label} {stats} | {g['sample_build_url']}")
        print(f"      Suite prefix: {', '.join(prefixes)}")
        print(f"      Reason: {g['failure_reason']}")

    with open(LAST_SCAN_CACHE_PATH, "w", encoding="utf-8") as f:
        json.dump({"unassociated_groups": unassociated_groups}, f, indent=2)


def _like_matches(pat: str, text: str) -> bool:
    tok_map = {"%": ".*", "_": ".", r"\n": "\n", r"\r": "\r", r"\t": "\t"}
    regex = "".join(
        tok_map.get(tok, re.escape(tok[-1]))
        for tok in re.findall(r"\\.|.", pat, flags=re.DOTALL)
    )
    return bool(re.fullmatch(regex, text, flags=re.DOTALL))


def validate_rule_definition(
    rule_definition: str, groups: list[dict[str, Any]]
) -> str:
    """Validates LUCI rule syntax, non-trivial `reason` constraint, and self-match against `groups`."""
    for g in groups:
        if not _is_specific_reason(g["failure_reason"]):
            raise ValueError(
                f"Selected group has bare or missing failure reason ({g['failure_reason']!r}); "
                "investigate logs instead of creating a rule."
            )
    cleaned = (rule_definition or "").strip()
    if re.search(r"\b(?:reason|test)\s*(?:LIKE|=|=~|!~)\s*'", cleaned, re.I):
        raise ValueError(
            f'LUCI rules must use double quotes ("..."), not single quotes: {cleaned!r}'
        )
    s_no_quotes = re.sub(r'"(?:\\.|[^"\\])*"', '""', cleaned)
    s_top = re.sub(r"\([^()]*\)", "()", s_no_quotes)
    if re.search(r"\btest\b", s_no_quotes, re.I) and (
        not re.search(r"\bAND\b", s_no_quotes, re.I)
        or (
            re.search(r"\btest\b", s_top, re.I)
            and re.search(r"\bOR\b", s_top, re.I)
        )
    ):
        raise ValueError(
            f"Rule must combine `test` and `reason` with `AND` (and parenthesize `OR` clauses): {cleaned!r}"
        )
    match_fn = any if re.search(r"\bOR\b", s_no_quotes, re.I) else all
    for field, key in (("reason", "failure_reason"), ("test", "test_ids")):
        pats = re.findall(
            rf'\b(?<!NOT\s){field}\s+LIKE\s+"((?:\\.|[^"\\])*)"', cleaned, re.I
        )
        if not pats:
            if field == "reason" or re.search(r"\btest\b", s_no_quotes, re.I):
                raise ValueError(
                    f'Rule must constrain `{field}` using `{field} LIKE "..."`: {cleaned!r}'
                )
            continue
        if field == "reason" and not all(_is_specific_reason(p) for p in pats):
            raise ValueError(
                f"Reason LIKE pattern in {cleaned!r} is too broad."
            )
        for p in pats:
            if field == "reason" and any(
                (m_tag := re.match(r"^\[[A-Za-z0-9_.]+\]", g["failure_reason"]))
                and _like_matches(p, m_tag.group(0))
                for g in groups
            ):
                raise ValueError(
                    f'Reason LIKE pattern "{p}" matches bare exception tag.'
                )
            if not any(
                _like_matches(p, c)
                for g in groups
                for c in ([g[key]] if field == "reason" else g[key])
            ):
                raise ValueError(
                    f'{field} LIKE "{p}" does not match selected group '
                    "(check for typos or missing leading/trailing '%')."
                )
        for g in groups:
            cands = [g[key]] if field == "reason" else g[key]
            if not match_fn(
                any(_like_matches(p, c) for c in cands) for p in pats
            ):
                raise ValueError(
                    f"Selected group {g['test_ids'][0]!r} is not matched by `{field}` clauses in {cleaned!r}."
                )
    return cleaned


def resolve_failure_metadata(args: argparse.Namespace) -> dict[str, str]:
    """Resolves failure metadata from --group in the scan cache and validates --rule."""
    if not os.path.exists(LAST_SCAN_CACHE_PATH):
        raise RuntimeError(
            f"Scan cache {LAST_SCAN_CACHE_PATH} not found. Run `scan` first."
        )
    with open(LAST_SCAN_CACHE_PATH, "r", encoding="utf-8") as f:
        unassoc = json.load(f).get("unassociated_groups", [])

    idxs = _dedup(int(m) for m in re.findall(r"\d+", args.group))
    if not idxs or any(i < 1 or i > len(unassoc) for i in idxs):
        raise ValueError(
            f"--group indices must be in 1..{len(unassoc)}: {args.group!r}"
        )
    groups = [unassoc[i - 1] for i in idxs]

    all_suites = _dedup(
        p.rsplit("/", 1)[-1] for g in groups for p in g["suite_prefixes"]
    )
    all_test_ids = _dedup(tid for g in groups for tid in g["test_ids"])
    test_id = (
        all_test_ids[0] if len(all_test_ids) == 1 else ", ".join(all_suites)
    )
    return {
        "test_id": test_id,
        "board": ", ".join(_dedup(b for g in groups for b in g["boards"])),
        "failure_reason": "\n\n".join(
            _dedup(g["failure_reason"] for g in groups)
        ),
        "builder": ", ".join(_dedup(b for g in groups for b in g["builders"])),
        "build_date": ", ".join(_dedup(d for g in groups for d in g["dates"])),
        "url": ", ".join(_dedup(g["sample_build_url"] for g in groups)),
        "rule_definition": validate_rule_definition(args.rule, groups),
    }


def _create_luci_rule(
    bug_id: str, rule_definition: str
) -> tuple[str, str, str]:
    resp = run_prpc(
        "analysis.api.luci.app",
        "luci.analysis.v1.Rules.Create",
        {
            "parent": f"projects/{PROJECT}",
            "rule": {
                "project": PROJECT,
                "ruleDefinition": rule_definition,
                "bug": {"system": "buganizer", "id": str(bug_id)},
                "isActive": True,
                "isManagingBug": False,
                "isManagingBugPriority": False,
            },
        },
    )
    rule_id = resp.get("ruleId", "")
    rule_url = (
        f"https://luci-milo.appspot.com/ui/tests/p/{PROJECT}/rules/{rule_id}"
    )
    return rule_id, rule_url, resp.get("ruleDefinition", rule_definition)


def handle_create_bug(args: argparse.Namespace) -> None:
    """Creates a new Buganizer issue and an active LUCI Analysis rule linking the failure."""
    meta = resolve_failure_metadata(args)
    rule_definition = meta["rule_definition"]
    title = args.title[:247] + "..." if len(args.title) > 250 else args.title

    description = f"""The WLAN E2E test `{meta["test_id"]}` failed on board `{meta["board"]}`.

* **Board**: `{meta["board"]}`
* **Builder**: `{meta["builder"]}` ({meta["build_date"]})
* **Failure Link**: {meta["url"]}

### Failure Reason
```
{meta["failure_reason"]}
```

### LUCI Analysis Rule Definition
```sql
{rule_definition}
```

---
*Filed via the wlan-e2e-console-triage workflow.*
"""
    try:
        res = subprocess.run(
            [
                ISSUES_BIN,
                "mutate",
                "create",
                "--title",
                title,
                "--description",
                description,
                "--component_id",
                DEFAULT_WLAN_COMPONENT_ID,
                "--priority",
                "P2",
                "--type",
                "BUG",
                "--assignee",
                "fuchsia-wlan-fireteam@google.com",
            ],
            capture_output=True,
            text=True,
            check=True,
        )
    except subprocess.CalledProcessError as e:
        raise RuntimeError(
            f"issues mutate create failed: {e.stderr.strip()}"
        ) from e

    if not (m := re.search(r"\b(\d{7,10})\b", res.stdout)):
        raise RuntimeError(
            f"Could not parse created Bug ID from issues output:\n{res.stdout}"
        )
    bug_id = m.group(1)
    bug_url = f"https://issuetracker.google.com/issues/{bug_id}"

    try:
        rule_id, rule_url, _ = _create_luci_rule(bug_id, rule_definition)
    except Exception as e:
        raise RuntimeError(
            f"Bug {bug_url} was created, but LUCI rule creation failed ({e}). "
            f"Do NOT re-run create-bug; recover with: "
            f"associate-bug --group {args.group} --bug-id {bug_id} --rule '{rule_definition}'"
        ) from e

    print(
        json.dumps(
            {
                "status": "SUCCESS",
                "action": "created_new_bug_and_rule",
                "bug_id": bug_id,
                "bug_url": bug_url,
                "rule_id": rule_id,
                "rule_url": rule_url,
                "rule_definition": rule_definition,
            },
            indent=2,
        )
    )


def handle_associate_bug(args: argparse.Namespace) -> None:
    """Associates a failure with an existing Buganizer bug by creating or updating its LUCI Analysis rule."""
    if not (m_bug := re.search(r"\d+", str(args.bug_id))):
        raise ValueError(f"Invalid --bug-id: {args.bug_id!r}")
    bug_id = m_bug.group(0)
    if bug_id in IGNORED_WRAPPER_BUG_IDS:
        raise ValueError(
            f"Refusing to associate with catch-all wrapper bug b/{bug_id}."
        )
    meta = resolve_failure_metadata(args)
    new_clause = meta["rule_definition"]

    lookup_resp = run_prpc(
        "analysis.api.luci.app",
        "luci.analysis.v1.Rules.LookupBug",
        {"system": "buganizer", "id": bug_id},
    )
    existing_rule_names = [
        r
        for r in lookup_resp.get("rules", [])
        if r.startswith(f"projects/{PROJECT}/rules/")
    ]

    if existing_rule_names:
        rule_name = existing_rule_names[0]
        existing_rule = run_prpc(
            "analysis.api.luci.app",
            "luci.analysis.v1.Rules.Get",
            {"name": rule_name},
        )
        current_def = existing_rule.get("ruleDefinition", "").strip()
        if not current_def or new_clause in current_def:
            updated_def = current_def or new_clause
        else:
            updated_def = f"({current_def}) OR ({new_clause})"

        update_resp = run_prpc(
            "analysis.api.luci.app",
            "luci.analysis.v1.Rules.Update",
            {
                "rule": {
                    "name": rule_name,
                    "ruleDefinition": updated_def,
                    "isActive": True,
                },
                "updateMask": "ruleDefinition,isActive",
                "etag": existing_rule.get("etag", ""),
            },
        )
        rule_id = update_resp.get("ruleId", rule_name.split("/")[-1])
        rule_url = f"https://luci-milo.appspot.com/ui/tests/p/{PROJECT}/rules/{rule_id}"
        action = "updated_existing_rule"
        final_rule_def = update_resp.get("ruleDefinition", updated_def)
    else:
        rule_id, rule_url, final_rule_def = _create_luci_rule(
            bug_id, new_clause
        )
        action = "created_rule_for_existing_bug"

    comment_text = f"""Associated WLAN E2E test failure with this issue via LUCI Analysis:

* **Test**: `{meta["test_id"]}`
* **Board**: `{meta["board"]}`
* **Builder**: `{meta["builder"]}` ({meta["build_date"]})
* **Failure Link**: {meta["url"]}
* **LUCI Analysis Rule**: {rule_url}
* **Rule Definition**: `{final_rule_def}`

### Failure Reason
```
{meta["failure_reason"]}
```
"""
    c_res = subprocess.run(
        [
            ISSUES_BIN,
            "mutate",
            "comment",
            "--issue_id",
            bug_id,
            "--comment",
            comment_text,
        ],
        capture_output=True,
        text=True,
        check=False,
    )
    comment_posted = c_res.returncode == 0
    if not comment_posted:
        print(
            f"Warning: Failed to post comment on b/{bug_id}: {c_res.stderr.strip()}",
            file=sys.stderr,
        )

    print(
        json.dumps(
            {
                "status": "SUCCESS",
                "action": action,
                "bug_id": bug_id,
                "bug_url": f"https://issuetracker.google.com/issues/{bug_id}",
                "rule_id": rule_id,
                "rule_url": rule_url,
                "rule_definition": final_rule_def,
                "comment_posted": comment_posted,
            },
            indent=2,
        )
    )


def _add_common_mutation_args(parser: argparse.ArgumentParser) -> None:
    parser.add_argument(
        "--group",
        required=True,
        help="1-based unassociated group index (or comma-separated indices, e.g. '1,2') from the last `scan`",
    )
    parser.add_argument(
        "--rule",
        required=True,
        help="LUCI Analysis rule definition constraining `reason` (e.g., 'reason LIKE \"%%...\"')",
    )


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Triage WLAN test failures across the wlan_e2e builder console."
    )
    subparsers = parser.add_subparsers(dest="command", required=True)

    scan_parser = subparsers.add_parser(
        "scan",
        help="Scan the last 3 completed builds (<24h, or <7d for weekend builders) across all wlan_e2e builders.",
    )
    scan_parser.set_defaults(func=handle_scan)

    create_parser = subparsers.add_parser(
        "create-bug",
        help="Create a new Buganizer issue and LUCI Analysis rule.",
    )
    _add_common_mutation_args(create_parser)
    create_parser.add_argument(
        "--title", required=True, help="Title for the new Buganizer issue"
    )
    create_parser.set_defaults(func=handle_create_bug)

    assoc_parser = subparsers.add_parser(
        "associate-bug",
        help="Associate a test failure with an existing Buganizer bug.",
    )
    _add_common_mutation_args(assoc_parser)
    assoc_parser.add_argument(
        "--bug-id", required=True, help="Existing Buganizer issue ID"
    )
    assoc_parser.set_defaults(func=handle_associate_bug)

    args = parser.parse_args()
    args.func(args)


if __name__ == "__main__":
    main()
