#!/usr/bin/env fuchsia-vendored-python
#
# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Checks how out of date our third-party rust deps are

import argparse
import concurrent.futures
import json
import os
import re
import subprocess
import tomllib
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import List, Tuple
from urllib import error, request

from rust import HOST_PLATFORM, PREBUILT_THIRD_PARTY_DIR, ROOT_PATH

manifest = ROOT_PATH / "third_party/rust_crates/Cargo.toml"
cargo_binary = (
    PREBUILT_THIRD_PARTY_DIR / "rust" / HOST_PLATFORM / "bin" / "cargo"
)

cache_dir = (
    Path(os.environ.get("XDG_CACHE_HOME", Path.home() / ".cache"))
    / "fuchsia-rust-3p-outdated"
)
cache_dir.mkdir(exist_ok=True)

url = "https://crates.io/api/v1/crates/{}"
headers = {}
opener = request.build_opener()
opener.addheaders = [("user-agent", "fuchsia-rust-3p-outdated")]
request.install_opener(opener)


def parse_semver(
    version_str: str,
) -> Tuple[int, int, int, bool, Tuple[Tuple[Tuple[int, int | str], ...], ...]]:
    # Try to match major.minor.patch optional pre-release/build strings if available
    # This is the conventional semantic version format for almost all crates on crates.io.
    # If no match is found, treat it as a 0.0.0 version.
    match = re.match(
        r"^(\d+)\.(\d+)\.(\d+)(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?(?:\+([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?$",
        version_str,
    )
    if not match:
        return (0, 0, 0, False, ())

    major, minor, patch, prerelease, build = match.groups()
    major = int(major)
    minor = int(minor)
    patch = int(patch)

    # Prepare for semantic sorting in fetch_crate_versions()
    # Prerelease versions are older than release versions.
    # False < True, so release versions sort higher.
    is_release = prerelease is None

    pre_parts = []
    if prerelease:
        for part in prerelease.split("."):
            sub_parts = [
                (0, int(sub)) if sub.isdigit() else (1, sub)
                for sub in re.findall(r"\d+|\D+", part)
            ]
            pre_parts.append(tuple(sub_parts))

    return (major, minor, patch, is_release, tuple(pre_parts))


def is_semver_compatible(
    current_semver: Tuple[
        int, int, int, bool, Tuple[Tuple[Tuple[int, int | str], ...], ...]
    ],
    candidate_semver: Tuple[
        int, int, int, bool, Tuple[Tuple[Tuple[int, int | str], ...], ...]
    ],
) -> bool:
    c_major, c_minor, c_patch, _, _ = current_semver
    v_major, v_minor, v_patch, _, _ = candidate_semver
    if c_major > 0:
        return v_major == c_major
    if c_minor > 0:
        return v_major == 0 and v_minor == c_minor
    return v_major == 0 and v_minor == 0 and v_patch == c_patch


def fetch_crate_versions(
    crate: str, min_publish_age: timedelta, now: datetime
) -> List[Tuple[str, datetime, bool]]:
    cache_path = cache_dir / crate
    needs_fetch = not cache_path.exists()
    if not needs_fetch:
        cache_mtime = datetime.fromtimestamp(
            cache_path.stat().st_mtime, tz=timezone.utc
        )
        if now - cache_mtime >= min_publish_age:
            needs_fetch = True
    if needs_fetch:
        try:
            print("Info: fetching", crate, "release data from crates.io")
            tmp_path = cache_dir / f"{crate}.tmp.{os.getpid()}"
            try:
                request.urlretrieve(url.format(crate), tmp_path)
                os.replace(tmp_path, cache_path)
            finally:
                tmp_path.unlink(missing_ok=True)
        except error.HTTPError:
            print("Warning:", crate, "was not found on crates.io")
            return []
    with open(cache_path) as f:
        data = json.load(f)
    versions = [
        (
            v["num"],
            datetime.fromisoformat(v["created_at"]),
            v.get("yanked", False),
        )
        for v in data["versions"]
    ]
    # Sort semantically with newest crates first
    versions.sort(key=lambda x: parse_semver(x[0]), reverse=True)
    return versions


def process_crate(
    crate: str,
    version: str,
    min_publish_age: timedelta,
    now: datetime,
    raw_versions: List[Tuple[str, datetime, bool]] | None = None,
    semver_only: bool = False,
) -> Tuple[timedelta, int, str, str, Tuple[str, datetime]] | None:
    if raw_versions is None:
        raw_versions = fetch_crate_versions(crate, min_publish_age, now)
    if not raw_versions:
        return None

    # Ignore yanked versions on crates.io, keeping the current in-tree version
    # so we can still compute outdatedness if it was yanked upstream.
    versions = [
        (v_num, created_at)
        for v_num, created_at, yanked in raw_versions
        if v_num == version or not yanked
    ]

    current_semver = parse_semver(version)
    current_is_release = current_semver[3]

    if current_is_release:
        # If the current version is a stable release, ignore the previous pre-releases
        versions = [v for v in versions if parse_semver(v[0])[3]]

    if semver_only:
        versions = [
            v
            for v in versions
            if is_semver_compatible(current_semver, parse_semver(v[0]))
        ]

    if min_publish_age > timedelta(0):
        versions = [
            v
            for v in versions
            if v[0] == version or now - v[1] >= min_publish_age
        ]

    try:
        versions = versions[
            : next((i for i, (v, _) in enumerate(versions) if v == version)) + 1
        ]
    except StopIteration:
        return None
    if not versions:
        return None
    current = versions[-1][1]
    newest = versions[0][1]

    age = newest - current
    if age < timedelta(0):
        age = timedelta(0)

    return (age, len(versions) - 1, crate, version, versions[0])


def process_crate_versions(
    crate: str,
    crate_versions: List[Tuple[str, bool]],
    min_publish_age: timedelta,
    now: datetime,
) -> List[Tuple[timedelta, int, str, str, Tuple[str, datetime]]]:
    raw_versions = fetch_crate_versions(crate, min_publish_age, now)
    if not raw_versions:
        return []
    highest_direct_version = max(
        (v for v, is_direct in crate_versions if is_direct),
        key=parse_semver,
        default=None,
    )
    results = []
    for version, is_direct in crate_versions:
        semver_only = not (is_direct and version == highest_direct_version)
        res = process_crate(
            crate,
            version,
            min_publish_age,
            now,
            raw_versions,
            semver_only=semver_only,
        )
        if res is not None:
            results.append(res)
    return results


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Checks how out of date our third-party rust deps are."
    )
    age_group = parser.add_mutually_exclusive_group()
    age_group.add_argument(
        "--min-publish-age",
        type=int,
        default=None,
        help="Override the minimum publish age (in days).",
    )
    age_group.add_argument(
        "--no-min-publish-age",
        "--skip-min-publish-age",
        action="store_const",
        const=0,
        dest="min_publish_age",
        help="Skip minimum publish age filtering and cache expiration.",
    )
    parser.add_argument(
        "--out-dir",
        help="Path to the Fuchsia build directory.",
    )
    return parser.parse_args()


if __name__ == "__main__":
    args = parse_args()

    with open(manifest, "rb") as f:
        manifest_data = tomllib.load(f)
    default_min_publish_age = (
        manifest_data.get("gn", {}).get("config", {}).get("min_publish_age", 0)
    )
    min_age_days = (
        args.min_publish_age
        if args.min_publish_age is not None
        else default_min_publish_age
    )
    min_publish_age = timedelta(days=min_age_days)
    now = datetime.now(timezone.utc)

    cargo_process = subprocess.run(
        [cargo_binary, "metadata", "--frozen", "--manifest-path", manifest],
        text=True,
        capture_output=True,
    )
    metadata = json.loads(cargo_process.stdout)
    resolve = metadata.get("resolve") or {}
    root_id = resolve.get("root")
    direct_pkg_ids = {
        dep["pkg"]
        for node in resolve.get("nodes", [])
        if node.get("id") == root_id
        for dep in node.get("deps", [])
    }
    crates: dict[str, List[Tuple[str, bool]]] = {}
    for p in metadata["packages"]:
        crates.setdefault(p["name"], []).append(
            (p["version"], p["id"] in direct_pkg_ids)
        )
    updates = []

    # Limit workers to avoid overwhelming crates.io and hitting rate limits.
    with concurrent.futures.ThreadPoolExecutor(max_workers=16) as executor:
        future_to_crate = {
            executor.submit(
                process_crate_versions,
                crate,
                crate_versions,
                min_publish_age,
                now,
            ): crate
            for crate, crate_versions in crates.items()
        }
        for future in concurrent.futures.as_completed(future_to_crate):
            crate = future_to_crate[future]
            try:
                results = future.result()
                updates.extend(results)
            except Exception as exc:
                print(
                    f"Warning: processing {crate} generated an exception: {exc}"
                )

    updates.sort()
    for age, releases, crate, version, newest in updates:
        print(crate, version, "is", end=" ")
        if version != newest[0]:
            print(age.days, "days out of date")
            print(f"\t{releases} newer versions")
            print(
                f"\tmost recent release was {newest[0]} on {newest[1].date()}"
            )
        else:
            print("up to date")

    print()
    print("median outdatedness:", updates[len(updates) // 2][0].days, "days")
    print(
        f"average outdatedness: {sum(u[0].days for u in updates) // len(updates)} days"
    )
    print("most out of date:", updates[-1][0].days, "days")
    releases_behind = sorted(u[1] for u in updates)
    print()
    print("median releases behind:", releases_behind[len(releases_behind) // 2])
    print(
        f"average releases behind {sum(releases_behind) / len(releases_behind):.1f}"
    )
    print("most releases behind:", releases_behind[-1])
