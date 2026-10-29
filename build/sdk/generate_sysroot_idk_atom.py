#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Generate the Fuchsia IDK sysroot atom with multi-CPU and multi-API-level support."""

import argparse
import json
import os
import pathlib
import subprocess
import sys

sys.dont_write_bytecode = True


def make_symlink(src: pathlib.Path, dest: pathlib.Path) -> None:
    dest.parent.mkdir(parents=True, exist_ok=True)
    if dest.is_symlink() or dest.exists():
        dest.unlink()
    rel_src = os.path.relpath(src, dest.parent)
    dest.symlink_to(rel_src)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--manifest",
        type=pathlib.Path,
        required=True,
        help="Path to JSON file with input manifest describing headers, IFS, and per-CPU files",
    )
    parser.add_argument(
        "--output-dir",
        type=pathlib.Path,
        required=True,
        help="Path where the sysroot atom files will be exported",
    )
    parser.add_argument(
        "--target-cpus",
        nargs="+",
        required=True,
        help="List of target CPU architectures (e.g. arm64 riscv64 x64)",
    )
    parser.add_argument(
        "--api-levels",
        nargs="+",
        required=True,
        help="List of API levels (e.g. 24 25 26 NEXT)",
    )
    parser.add_argument(
        "--json-validator",
        type=pathlib.Path,
        required=True,
        help="Path to json_validator executable",
    )
    parser.add_argument(
        "--schema",
        type=pathlib.Path,
        required=True,
        help="Path to sysroot.json schema",
    )
    parser.add_argument(
        "--manifest-schema",
        type=pathlib.Path,
        required=False,
        help="Path to manifest.json schema (used if generating meta/manifest.json)",
    )
    parser.add_argument(
        "--meta-manifest-id",
        type=str,
        required=False,
        help="If specified, also generate top-level meta/manifest.json with this ID",
    )
    parser.add_argument(
        "--host-cpu",
        type=str,
        default="x86_64",
        help="Host architecture for manifest.json",
    )
    parser.add_argument(
        "--stamp-file",
        type=pathlib.Path,
        required=False,
        help="Path to stamp file to touch on success",
    )
    parser.add_argument(
        "--depfile",
        type=pathlib.Path,
        required=False,
        help="Path to write depfile",
    )

    args = parser.parse_args()

    with args.manifest.open() as f:
        manifest = json.load(f)

    headers = manifest["headers"]
    ifs_files = manifest["ifs_files"]
    musl_lib_ld = pathlib.Path(manifest["musl_lib_ld"]).resolve()

    dest_to_src: dict[pathlib.Path, pathlib.Path] = {}
    input_files: set[pathlib.Path] = {
        args.manifest.resolve(),
        args.schema.resolve(),
    }
    if args.manifest_schema:
        input_files.add(args.manifest_schema.resolve())

    # Map IFS files
    dest_to_src[args.output_dir / "pkg/sysroot/libc.ifs"] = pathlib.Path(
        ifs_files["libc"]
    ).resolve()
    dest_to_src[args.output_dir / "pkg/sysroot/zircon.ifs"] = pathlib.Path(
        ifs_files["zircon"]
    ).resolve()

    versions = {}
    variants = []

    manifest_cpus = manifest["cpus"]
    if isinstance(manifest_cpus, list):
        manifest_cpus = {item["cpu"]: item for item in manifest_cpus}

    for cpu in args.target_cpus:
        assert (
            cpu in manifest_cpus
        ), f"Missing configuration for cpu '{cpu}' in manifest"
        cpu_info = manifest_cpus[cpu]

        arch_base = f"arch/{cpu}/sysroot"
        version_headers = [f"{arch_base}/{h['dest']}" for h in headers]

        link_libs = [
            f"{arch_base}/lib/libc.so",
            f"{arch_base}/lib/Scrt1.o",
            f"{arch_base}/lib/libdl.so",
            f"{arch_base}/lib/libm.so",
            f"{arch_base}/lib/libpthread.so",
            f"{arch_base}/lib/librt.so",
            f"{arch_base}/lib/libzircon.so",
        ]

        dist_libs = [
            f"{arch_base}/dist/lib/ld.so.1",
            f"{arch_base}/dist/lib/asan/ld.so.1",
        ]

        debug_libs = [
            f"{arch_base}/debug/libc.so",
            f"{arch_base}/debug/asan/libc.so",
        ]

        if "hwasan_ld_so" in cpu_info["dist_libs"]:
            dist_libs.append(f"{arch_base}/dist/lib/hwasan/ld.so.1")
        if "hwasan_libc_so" in cpu_info["debug_libs"]:
            debug_libs.append(f"{arch_base}/debug/hwasan/libc.so")

        versions[cpu] = {
            "root": arch_base,
            "include_dir": f"{arch_base}/include",
            "dist_dir": arch_base,
            "headers": sorted(version_headers),
            "link_libs": link_libs,
            "dist_libs": dist_libs,
            "debug_libs": debug_libs,
        }

        # Per-CPU file mappings for arch/<cpu>/sysroot
        for h in headers:
            src = pathlib.Path(h["source"]).resolve()
            dest_to_src[args.output_dir / arch_base / h["dest"]] = src

        dest_to_src[args.output_dir / arch_base / "lib/libc.so"] = pathlib.Path(
            cpu_info["stubs"]["libc"]
        ).resolve()
        dest_to_src[
            args.output_dir / arch_base / "lib/libzircon.so"
        ] = pathlib.Path(cpu_info["stubs"]["libzircon"]).resolve()
        dest_to_src[args.output_dir / arch_base / "lib/Scrt1.o"] = pathlib.Path(
            cpu_info["startfiles"]["scrt1"]
        ).resolve()

        for lib_name in ["libdl.so", "libm.so", "libpthread.so", "librt.so"]:
            dest_to_src[
                args.output_dir / arch_base / f"lib/{lib_name}"
            ] = musl_lib_ld

        dest_to_src[
            args.output_dir / arch_base / "dist/lib/ld.so.1"
        ] = pathlib.Path(cpu_info["dist_libs"]["ld_so"]).resolve()
        dest_to_src[
            args.output_dir / arch_base / "dist/lib/asan/ld.so.1"
        ] = pathlib.Path(cpu_info["dist_libs"]["asan_ld_so"]).resolve()
        if "hwasan_ld_so" in cpu_info["dist_libs"]:
            dest_to_src[
                args.output_dir / arch_base / "dist/lib/hwasan/ld.so.1"
            ] = pathlib.Path(cpu_info["dist_libs"]["hwasan_ld_so"]).resolve()

        dest_to_src[
            args.output_dir / arch_base / "debug/libc.so"
        ] = pathlib.Path(cpu_info["debug_libs"]["libc_so"]).resolve()
        dest_to_src[
            args.output_dir / arch_base / "debug/asan/libc.so"
        ] = pathlib.Path(cpu_info["debug_libs"]["asan_libc_so"]).resolve()
        if "hwasan_libc_so" in cpu_info["debug_libs"]:
            dest_to_src[
                args.output_dir / arch_base / "debug/hwasan/libc.so"
            ] = pathlib.Path(cpu_info["debug_libs"]["hwasan_libc_so"]).resolve()

        # Per (CPU, API Level) variants
        for api_level in args.api_levels:
            api_str = str(api_level)
            obj_base = f"obj/{cpu}-api-{api_str}/sysroot"
            variant_headers = [f"{obj_base}/{h['dest']}" for h in headers]
            v_link_libs = [
                p.replace(f"arch/{cpu}/sysroot/", f"{obj_base}/")
                for p in link_libs
            ]
            v_dist_libs = [
                p.replace(f"arch/{cpu}/sysroot/", f"{obj_base}/")
                for p in dist_libs
            ]
            v_debug_libs = [
                p.replace(f"arch/{cpu}/sysroot/", f"{obj_base}/")
                for p in debug_libs
            ]

            variants.append(
                {
                    "constraints": {
                        "api_level": api_str,
                        "arch": cpu,
                    },
                    "values": {
                        "root": obj_base,
                        "include_dir": f"{obj_base}/include",
                        "sysroot_dir": obj_base,
                        "headers": sorted(variant_headers),
                        "link_libs": v_link_libs,
                        "dist_libs": v_dist_libs,
                        "debug_libs": v_debug_libs,
                    },
                }
            )

            # Files for obj/<cpu>-api-<level>/sysroot
            for h in headers:
                src = pathlib.Path(h["source"]).resolve()
                dest_to_src[args.output_dir / obj_base / h["dest"]] = src

            dest_to_src[
                args.output_dir / obj_base / "lib/libc.so"
            ] = pathlib.Path(cpu_info["stubs"]["libc"]).resolve()
            dest_to_src[
                args.output_dir / obj_base / "lib/libzircon.so"
            ] = pathlib.Path(cpu_info["stubs"]["libzircon"]).resolve()
            dest_to_src[
                args.output_dir / obj_base / "lib/Scrt1.o"
            ] = pathlib.Path(cpu_info["startfiles"]["scrt1"]).resolve()

            for lib_name in [
                "libdl.so",
                "libm.so",
                "libpthread.so",
                "librt.so",
            ]:
                dest_to_src[
                    args.output_dir / obj_base / f"lib/{lib_name}"
                ] = musl_lib_ld

            dest_to_src[
                args.output_dir / obj_base / "dist/lib/ld.so.1"
            ] = pathlib.Path(cpu_info["dist_libs"]["ld_so"]).resolve()
            dest_to_src[
                args.output_dir / obj_base / "dist/lib/asan/ld.so.1"
            ] = pathlib.Path(cpu_info["dist_libs"]["asan_ld_so"]).resolve()
            if "hwasan_ld_so" in cpu_info["dist_libs"]:
                dest_to_src[
                    args.output_dir / obj_base / "dist/lib/hwasan/ld.so.1"
                ] = pathlib.Path(
                    cpu_info["dist_libs"]["hwasan_ld_so"]
                ).resolve()

            dest_to_src[
                args.output_dir / obj_base / "debug/libc.so"
            ] = pathlib.Path(cpu_info["debug_libs"]["libc_so"]).resolve()
            dest_to_src[
                args.output_dir / obj_base / "debug/asan/libc.so"
            ] = pathlib.Path(cpu_info["debug_libs"]["asan_libc_so"]).resolve()
            if "hwasan_libc_so" in cpu_info["debug_libs"]:
                dest_to_src[
                    args.output_dir / obj_base / "debug/hwasan/libc.so"
                ] = pathlib.Path(
                    cpu_info["debug_libs"]["hwasan_libc_so"]
                ).resolve()

    meta = {
        "name": "sysroot",
        "type": "sysroot",
        "ifs_files": [
            "pkg/sysroot/libc.ifs",
            "pkg/sysroot/zircon.ifs",
        ],
        "versions": versions,
        "variants": variants,
    }

    meta_json_path = args.output_dir / "pkg/sysroot/meta.json"
    meta_json_path.parent.mkdir(parents=True, exist_ok=True)
    with meta_json_path.open("w") as f:
        json.dump(meta, f, indent=2, sort_keys=True)

    # Validate pkg/sysroot/meta.json
    res = subprocess.run(
        [
            str(args.json_validator.resolve()),
            str(args.schema.resolve()),
            str(meta_json_path),
        ],
        capture_output=True,
        text=True,
    )
    if res.returncode != 0:
        print(f"Error validating {meta_json_path}:", file=sys.stderr)
        print(res.stdout, file=sys.stderr)
        print(res.stderr, file=sys.stderr)
        return res.returncode

    # If requested, write top-level meta/manifest.json
    if args.meta_manifest_id:
        manifest_meta = {
            "arch": {
                "host": args.host_cpu,
                "target": sorted(args.target_cpus),
            },
            "id": args.meta_manifest_id,
            "parts": [
                {
                    "meta": "pkg/sysroot/meta.json",
                    "type": "sysroot",
                }
            ],
            "root": "..",
            "schema_version": "1",
        }
        manifest_path = args.output_dir / "meta/manifest.json"
        manifest_path.parent.mkdir(parents=True, exist_ok=True)
        with manifest_path.open("w") as f:
            json.dump(manifest_meta, f, indent=2, sort_keys=True)

        if args.manifest_schema:
            res = subprocess.run(
                [
                    str(args.json_validator.resolve()),
                    str(args.manifest_schema.resolve()),
                    str(manifest_path),
                ],
                capture_output=True,
                text=True,
            )
            if res.returncode != 0:
                print(f"Error validating {manifest_path}:", file=sys.stderr)
                print(res.stdout, file=sys.stderr)
                print(res.stderr, file=sys.stderr)
                return res.returncode

    # Create symlinks
    for dest, src in dest_to_src.items():
        input_files.add(src)
        make_symlink(src, dest)

    if args.stamp_file:
        args.stamp_file.parent.mkdir(parents=True, exist_ok=True)
        args.stamp_file.touch()

    if args.depfile:
        args.depfile.parent.mkdir(parents=True, exist_ok=True)
        target_output = str(
            args.stamp_file if args.stamp_file else meta_json_path
        )
        deps_str = " ".join(
            sorted(
                str(f).replace(" ", "\\ ")
                for f in input_files
                if f.exists() or f.is_symlink()
            )
        )
        with args.depfile.open("w") as f:
            f.write(f"{target_output}: {deps_str}\n")

    return 0


if __name__ == "__main__":
    sys.exit(main())
