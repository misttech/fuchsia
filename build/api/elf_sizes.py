# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Helpers for locating elf_sizes.json manifests and collecting symbol verification inputs."""

import json
import os
import sys
import typing as T
from pathlib import Path

from build.api import debug_symbols, gn_ninja_outputs
from build.bazel.scripts import build_utils


def extract_elf_sizes_paths(
    images_json: T.Sequence[dict[str, T.Any]],
    build_dir: Path,
    last_build_only: bool = False,
) -> list[Path]:
    """Return elf_sizes.json manifest paths from a parsed images.json list."""
    paths: list[Path] = []
    for entry in images_json:
        if entry.get("name") == "elf_sizes.json" and "path" in entry:
            path = build_dir / entry["path"]
            if last_build_only:
                # Verify the build produced the file as it should.
                assert (
                    path.exists()
                ), f"Missing elf_sizes.json file from build!: {path}"
            elif not path.exists():
                # There is no guarantee that the file exists.
                continue

            paths.append(path)
    return paths


def collect_elf_sizes_verification_inputs(
    build_dir: Path,
    fuchsia_dir: Path,
    host_tag: str,
    elf_sizes_paths: T.Sequence[Path],
    debug_symbol_entries: T.Sequence[dict[str, T.Any]],
    ninja_outputs: gn_ninja_outputs.NinjaOutputsBase | None = None,
) -> tuple[set[str], set[str]]:
    """Collect prebuilt blob merkles and exempted runtime build IDs.

    Args:
        build_dir: Path to the Ninja build directory.
        fuchsia_dir: Path to the Fuchsia source checkout directory.
        host_tag: Host platform/architecture tag (e.g. 'linux-x64').
        elf_sizes_paths: Sequence of elf_sizes.json manifest paths to scan.
        debug_symbol_entries: Sequence of resolved debug symbol manifest entries.
        ninja_outputs: Optional pre-loaded NinjaOutputsBase instance. If None,
            loads from build_dir when needed.

    Returns:
        A (prebuilt_merkles, exempt_build_ids) tuple where:
          - prebuilt_merkles is the set of blob merkle roots belonging to
            prebuilt packages in GN or Bazel package manifests.
          - exempt_build_ids is the set of GNU build-id hex strings for
            prebuilt Clang/Rust toolchain runtime shared libraries.
    """
    known_build_ids = {
        entry["elf_build_id"]
        for entry in debug_symbol_entries
        if entry.get("elf_build_id")
    }

    unmatched_blob_paths: list[str] = []
    unmatched_build_ids: set[str] = set()
    for elf_sizes_path in elf_sizes_paths:
        with elf_sizes_path.open("rt") as f:
            elf_sizes_json = json.load(f)
        for entry in elf_sizes_json.get("files", []):
            build_id = entry.get("build_id", "")
            if build_id and build_id not in known_build_ids:
                unmatched_build_ids.add(build_id)
                if "path" in entry:
                    unmatched_blob_paths.append(entry["path"])

    prebuilt_merkles: set[str] = set()
    exempt_build_ids: set[str] = set()
    if not unmatched_build_ids:
        return prebuilt_merkles, exempt_build_ids

    # 1. GN package manifests via gn_ninja_outputs database.
    gn_source_merkles: set[str] = set()
    outputs = ninja_outputs
    if outputs is None:
        try:
            outputs = gn_ninja_outputs.load_from_build_dir(build_dir)
        except RuntimeError as e:
            print(
                f"ERROR: Could not load outputs database: {e}", file=sys.stderr
            )
    if outputs is not None:
        for rel_path in outputs.get_paths(sort_result=False):
            if rel_path.endswith("/package_manifest.json"):
                pm_path = build_dir / rel_path
                if pm_path.exists():
                    with pm_path.open("rt") as f:
                        pm = json.load(f)
                    for blob in pm.get("blobs", []):
                        merkle = blob.get("merkle")
                        if not merkle:
                            continue
                        src = blob.get("source_path", "")
                        if (
                            src.startswith("blobs/")
                            or "/prebuilt/" in f"/{src}"
                        ):
                            prebuilt_merkles.add(merkle)
                        else:
                            gn_source_merkles.add(merkle)

    # 2. Bazel unpacked prebuilt packages via BazelPaths.execroot.
    if (
        build_utils.is_fuchsia_dir(fuchsia_dir)
        and (
            fuchsia_dir / build_utils._BAZEL_TOPDIR_CONFIG_FROM_FUCHSIA_DIR
        ).exists()
    ):
        bazel_execroot = build_utils.BazelPaths(fuchsia_dir, build_dir).execroot

        def _collect_rebased_pm(rebased_pm: Path) -> None:
            if not rebased_pm.is_file():
                return
            try:
                with rebased_pm.open("rt") as f:
                    pm = json.load(f)
            except (json.JSONDecodeError, UnicodeDecodeError):
                return
            if not isinstance(pm, dict):
                return
            for blob in pm.get("blobs", []):
                src = blob.get("source_path", "")
                if (
                    src.startswith(("content/blobs/", "blobs/", "../blobs/"))
                    or "/prebuilt/" in f"/{src}"
                ):
                    merkle = blob.get("merkle")
                    if merkle and merkle not in gn_source_merkles:
                        prebuilt_merkles.add(merkle)

        visited_containers: set[str] = set()

        def _collect_prebuilt_container(container_rel: str) -> None:
            if container_rel in visited_containers or container_rel.startswith(
                ("bazel-out/", "external/gn_targets")
            ):
                return
            visited_containers.add(container_rel)
            container_path = bazel_execroot / container_rel
            if not container_path.is_dir():
                return
            blobs_dir = container_path / "blobs"
            if blobs_dir.is_dir():
                for merkle in os.listdir(blobs_dir):
                    if (
                        merkle not in gn_source_merkles
                        and (blobs_dir / merkle).is_file()
                    ):
                        prebuilt_merkles.add(merkle)
            for sub_dir_name in ("packages", "subpackages"):
                sub_dir = container_path / sub_dir_name
                if sub_dir.is_dir():
                    for pkg_file in sub_dir.rglob("*"):
                        _collect_rebased_pm(pkg_file)

        visited_dirs: set[Path] = set()
        for blob_path in unmatched_blob_paths:
            if "/bazel_inputs/" in blob_path:
                continue
            cur = (bazel_execroot / blob_path).parent
            while cur not in visited_dirs and cur != cur.parent:
                visited_dirs.add(cur)
                rebased_pm = cur / "rebased_package_manifest.json"
                if rebased_pm.is_file():
                    _collect_rebased_pm(rebased_pm)
                    break
                if cur == bazel_execroot:
                    break
                cur = cur.parent

        # Also scan Bazel output directories for unpacked fuchsia_prebuilt_package
        # manifests (rebased_package_manifest.json) and prebuilt assembly containers
        # recorded in *.debug_symbols.json, since fuchsia_product_configuration and
        # fuchsia_hybrid_product_configuration copy package blobs into
        # <product_config>/blobs/<merkle> outside the original prebuilt directory.
        bazel_out_dir = bazel_execroot / "bazel-out"
        if bazel_out_dir.is_dir():
            for cfg_entry in os.scandir(bazel_out_dir):
                if not cfg_entry.is_dir():
                    continue
                bin_dir = Path(cfg_entry.path) / "bin"
                if not bin_dir.is_dir():
                    continue
                for dirpath, dirnames, filenames in os.walk(bin_dir):
                    if "rebased_package_manifest.json" in filenames:
                        _collect_rebased_pm(
                            Path(dirpath) / "rebased_package_manifest.json"
                        )
                    for filename in filenames:
                        if filename.endswith(".debug_symbols.json"):
                            with (Path(dirpath) / filename).open("rt") as f:
                                ds_manifest = json.load(f)
                            if isinstance(ds_manifest, list):
                                for entry in ds_manifest:
                                    if not isinstance(entry, dict):
                                        continue
                                    if "container_dir" in entry:
                                        _collect_prebuilt_container(
                                            entry["container_dir"]
                                        )
                                    elif "package_manifest" in entry:
                                        _collect_rebased_pm(
                                            bazel_execroot
                                            / entry["package_manifest"]
                                        )
                    dirnames[:] = [
                        d
                        for d in dirnames
                        if d
                        not in {
                            "bazel_inputs",
                            "blobs",
                            "buildfiles_genquery",
                            "content",
                            "packages",
                            "subpackages",
                            "_objs",
                            "external",
                        }
                        and not d.endswith((".runfiles", "_pkg"))
                    ]

    # 3. Prebuilt toolchain runtime shared libraries (e.g. non-ASAN or
    #    asan+noexcept multilib libc++/libunwind/libstd on ASAN builds).
    for tool in ("clang", "rust"):
        lib_dir = (
            fuchsia_dir / "prebuilt" / "third_party" / tool / host_tag / "lib"
        )
        if not lib_dir.is_dir():
            continue

        debug_build_id_dir = lib_dir / "debug" / ".build-id"
        for build_id in list(unmatched_build_ids - exempt_build_ids):
            rel_debug = Path(build_id[:2]) / f"{build_id[2:]}.debug"
            if (debug_build_id_dir / rel_debug).exists():
                exempt_build_ids.add(build_id)

        if unmatched_build_ids - exempt_build_ids:
            for dirpath, _, filenames in os.walk(lib_dir):
                for filename in filenames:
                    if ".so" not in filename:
                        continue
                    so_path = Path(dirpath) / filename
                    if not so_path.is_symlink() and so_path.is_file():
                        extracted = debug_symbols.extract_gnu_build_id(so_path)
                        if extracted and extracted in unmatched_build_ids:
                            exempt_build_ids.add(extracted)

    return prebuilt_merkles, exempt_build_ids
