#!/usr/bin/env fuchsia-vendored-python

# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Post-processes the BUILD.bazel files that crate_universe generates.

`fx update-rustc-third-party` runs crate_universe's `crates_vendor` to generate
//third_party/rust_crates/vendor, then runs this script on every BUILD.bazel
file under it. crate_universe gets a few things wrong or doesn't support
them, so this script fixes them up in place:

- Patched crates. crate_universe assumes every crate lives at
  //third_party/rust_crates/vendor/<crate>-<version>, but crates in
  Cargo.toml's [patch.crates-io] section live elsewhere (e.g. under forks/,
  ask2patch/ or intree/). Labels pointing at such crates, both in aliases and
  in other targets' attributes, are rewritten to the patched location
  (https://github.com/bazelbuild/rules_rust/issues/3732).
- Dangling aliases. Aliases whose target package doesn't exist and can't be
  fixed this way are removed.
- Restricted crates. Aliases for crates whose Cargo.toml group_visibility
  points at //third_party/rust_crates/visibility.gni get a `visibility`
  pointing at the package_group with the same name in
  //third_party/rust_crates/BUILD.bazel, which lists the Bazel packages allowed
  to use the crate. The packages that define those crates are restricted to
  //third_party/rust_crates, so the aliases can't be bypassed by depending on
  e.g. //third_party/rust_crates/vendor/regex-1.13.1:regex directly. (GN does
  the same by restricting the versioned rustc_library targets to `:*`.)

The script is idempotent, so it's safe to rerun on already-processed files.
"""

import argparse
import pathlib
import re
import sys
import tomllib


def _load_cargo_toml(fuchsia_dir: pathlib.Path) -> dict[str, object]:
    """Parses //third_party/rust_crates/Cargo.toml."""
    with open(fuchsia_dir / "third_party/rust_crates/Cargo.toml", "rb") as f:
        return tomllib.load(f)


def get_rust_package_paths(
    fuchsia_dir: pathlib.Path,
    cargo_toml: dict[str, object] | None = None,
) -> dict[str, str]:
    """Maps each crate in Cargo.toml's [patch.crates-io] to its package path.

    Paths are relative to fuchsia_dir, e.g.
    {"zeroize": "third_party/rust_crates/forks/zeroize"}.
    """
    if cargo_toml is None:
        cargo_toml = _load_cargo_toml(fuchsia_dir)
    patch = cargo_toml.get("patch")
    if not isinstance(patch, dict):
        return {}
    crates_io = patch.get("crates-io")
    if not isinstance(crates_io, dict):
        return {}
    return {
        k: f'third_party/rust_crates/{v["path"]}'
        for k, v in crates_io.items()
        if isinstance(v, dict) and "path" in v
    }


def get_rust_package_visibilities(
    fuchsia_dir: pathlib.Path,
    cargo_toml: dict[str, object] | None = None,
) -> dict[tuple[str, str], str]:
    """Maps each restricted (crate, version) to its package_group label.

    Only crates whose Cargo.toml group_visibility points at
    //third_party/rust_crates/visibility.gni are included, e.g.
    {("regex", "1.13.1"): "//third_party/rust_crates:regex_visibility"}.
    """
    if cargo_toml is None:
        cargo_toml = _load_cargo_toml(fuchsia_dir)
    visibilities: dict[tuple[str, str], str] = {}
    gn = cargo_toml.get("gn")
    if not isinstance(gn, dict):
        return visibilities
    packages = gn.get("package")
    if not isinstance(packages, dict):
        return visibilities
    for crate_name, versions in packages.items():
        if not isinstance(versions, dict):
            continue
        for version, cfg in versions.items():
            if not isinstance(cfg, dict):
                continue
            # TODO(https://fxbug.dev/572815288): Also apply allowlists from
            # other .gni files (e.g. nix_visibility.gni) and inline
            # `visibility` lists, which are still only enforced in GN.
            group_vis = cfg.get("group_visibility")
            if (
                isinstance(group_vis, dict)
                and group_vis.get("import")
                == "//third_party/rust_crates/visibility.gni"
                and "variable" in group_vis
            ):
                visibilities[
                    (crate_name, version)
                ] = f"//third_party/rust_crates:{group_vis['variable']}"
    return visibilities


def _apply_alias_visibility(full_block: str, vis_label: str) -> str:
    """Sets the visibility of an alias() block to [vis_label].

    Replaces any existing visibility, so it's safe to rerun on an already
    processed alias.
    """
    if re.search(r"\bvisibility\s*=", full_block):
        new_block, count = re.subn(
            r"\bvisibility\s*=\s*\[[^\]]*\]",
            f'visibility = ["{vis_label}"]',
            full_block,
        )
    else:
        new_block, count = re.subn(
            r"\n(\)\s*)$",
            f'\n    visibility = ["{vis_label}"],\n\\1',
            full_block,
        )
    # Failing loudly matters here: if crate_universe changes how it formats
    # aliases, a silent no-op would leave restricted crates public.
    if count != 1:
        raise ValueError(f"failed to set visibility on alias:\n{full_block}")
    return new_block


def get_restricted_crate_packages(
    fuchsia_dir: pathlib.Path,
    path_overrides: dict[str, str],
    visibilities: dict[tuple[str, str], str],
) -> dict[str, str]:
    """Maps the package path of each restricted crate to its visibility label.

    Raises ValueError if a restricted crate's package can't be found, since
    silently skipping it would leave the crate public.
    """
    pkg_path_to_vis: dict[str, str] = {}
    for (crate, version), vis in visibilities.items():
        # crates_vendor replaces the "+" of SemVer build metadata with "-" in
        # directory names, e.g. vendor/ash-0.38.0-1.3.281.
        vendor_path = f"third_party/rust_crates/vendor/{crate}-{version.replace('+', '-')}"
        if (fuchsia_dir / vendor_path).is_dir():
            pkg_path_to_vis[vendor_path] = vis
            continue
        fixed_path = path_overrides.get(crate) or path_overrides.get(
            crate.replace("_", "-")
        )
        if fixed_path is not None and (fuchsia_dir / fixed_path).is_dir():
            pkg_path_to_vis[fixed_path] = vis
            continue
        raise ValueError(
            f"failed to find the package for restricted crate {crate} "
            f"{version}; is its [gn.package] version in Cargo.toml stale?"
        )
    return pkg_path_to_vis


_PUBLIC_DEFAULT_VISIBILITY = 'default_visibility = ["//visibility:public"]'
_RESTRICTED_DEFAULT_VISIBILITY = (
    'default_visibility = ["//third_party/rust_crates:__subpackages__"]'
)


def restrict_crate_package(build_file: pathlib.Path) -> None:
    """Restricts a crate's package to //third_party/rust_crates.

    Only the crate aliases and other third-party crates may then depend on the
    crate's targets.
    """
    content = build_file.read_text()
    if _RESTRICTED_DEFAULT_VISIBILITY in content:
        return
    new_content, count = re.subn(
        re.escape(_PUBLIC_DEFAULT_VISIBILITY),
        _RESTRICTED_DEFAULT_VISIBILITY,
        content,
    )
    # Failing loudly matters here for the same reason as in
    # _apply_alias_visibility.
    if count != 1:
        raise ValueError(f"failed to restrict the visibility of {build_file}")
    build_file.write_text(new_content)


def check_package_groups_exist(
    fuchsia_dir: pathlib.Path, visibilities: dict[tuple[str, str], str]
) -> None:
    """Checks that each visibility label names a package_group that exists.

    Aliases are tagged manual, so a missing package_group would otherwise only
    surface when some target that depends on the crate is built.
    """
    build_file = fuchsia_dir / "third_party/rust_crates/BUILD.bazel"
    content = build_file.read_text()
    for label in sorted(set(visibilities.values())):
        name = label.rpartition(":")[2]
        if not re.search(rf'name = "{re.escape(name)}"', content):
            raise ValueError(
                f"{build_file} has no package_group named {name!r}, which "
                "Cargo.toml's group_visibility settings refer to"
            )


def process_one(
    build_file: pathlib.Path,
    fuchsia_dir: pathlib.Path,
    path_overrides: dict[str, str],
    visibilities: dict[tuple[str, str], str] | None = None,
) -> None:
    """Fixes up one BUILD.bazel file generated by crates_vendor in place.

    Rewrites labels of patched crates to their patched locations, removes
    aliases whose targets don't exist, and restricts the visibility of aliases
    for restricted crates. See the module docstring for details.
    """
    with open(build_file, "r") as f:
        orig_content = f.read()

    # This logic is similar to the one in replacement_func, but it operates on
    # individual labels found within other rules (e.g., in `deps` attributes),
    # rather than on the `actual` attribute of an `alias` rule. If a fix is
    # found, it returns the corrected label.
    def fix_missing_vendor_label(match: re.Match[str]) -> str:
        label = match.group(1)
        parts = label[2:].split(":")
        if len(parts) == 2:
            pkg_path, pkg_name = parts
            full_pkg_path = fuchsia_dir / pkg_path
            if not full_pkg_path.is_dir():
                fixed_path = path_overrides.get(pkg_name) or path_overrides.get(
                    pkg_name.replace("_", "-")
                )
                if (
                    fixed_path is not None
                    and (fuchsia_dir / fixed_path).is_dir()
                ):
                    return f'"//{fixed_path}:{pkg_name}"'
        return match.group(0)

    content = re.sub(
        r'"(//third_party/rust_crates/vendor/[^"]+)"',
        fix_missing_vendor_label,
        orig_content,
    )

    pkg_path_to_vis = get_restricted_crate_packages(
        fuchsia_dir, path_overrides, visibilities or {}
    )

    # Regex to match alias targets.
    # This matches the aliases generated by crate_universe.
    # It captures the entire block and the `actual` attribute.
    alias_block_pattern = re.compile(
        r'(alias\(\s*name\s*=\s*"[^"]+",\s*actual\s*=\s*"([^"]+)"\s*,?.*?\)\s*)',
        re.DOTALL,  # To match newlines.
    )

    def replacement_func(match: re.Match[str]) -> str:
        """Checks if the actual target of an alias exists in the filesystem.
        If the target is a Bazel label pointing to a directory that does not
        exist, the entire alias block is removed. Otherwise, the block is
        updated with any path and visibility overrides.
        """
        full_block = match.group(1)
        actual_target = match.group(2)

        # We are looking for labels like //third_party/rust_crates/vendor/aes-0.8.4:aes
        if actual_target.startswith("//"):
            parts = actual_target[2:].split(":")
            if len(parts) == 2:
                pkg_path, pkg_name = parts
                full_pkg_path = fuchsia_dir / pkg_path
                resolved_pkg_path: str | None = None
                if full_pkg_path.is_dir():
                    resolved_pkg_path = pkg_path
                else:
                    # Look to see if we can fix up the path based on overrides in Cargo.toml
                    fixed_path = path_overrides.get(
                        pkg_name
                    ) or path_overrides.get(pkg_name.replace("_", "-"))
                    if (
                        fixed_path is not None
                        and (fuchsia_dir / fixed_path).is_dir()
                    ):
                        full_block = full_block.replace(pkg_path, fixed_path)
                        resolved_pkg_path = fixed_path
                    else:
                        # Nothing found. Remove the block.
                        return ""

                vis_label = pkg_path_to_vis.get(resolved_pkg_path)
                if vis_label is not None:
                    full_block = _apply_alias_visibility(full_block, vis_label)
                return full_block

        return full_block

    new_content = alias_block_pattern.sub(replacement_func, content)
    if new_content != orig_content:
        with open(build_file, "w") as f:
            f.write(new_content)


def main() -> int:
    """Post-processes the BUILD.bazel files passed via --search-dir."""
    parser = argparse.ArgumentParser(
        description="Fix Bazel aliases in crates_vendor generated BUILD.bazel files."
    )
    parser.add_argument(
        "--search-dir",
        required=True,
        type=pathlib.Path,
        nargs="+",
        help="Path to the BUILD.bazel file(s) or directory",
    )
    parser.add_argument(
        "--fuchsia-dir",
        required=True,
        type=pathlib.Path,
        help="Path to the Fuchsia directory",
    )
    args = parser.parse_args()

    build_files: list[pathlib.Path] = []
    for bf in args.search_dir:
        if bf.is_dir():
            build_files.extend(bf.rglob("BUILD.bazel"))
        else:
            build_files.append(bf)

    cargo_toml = _load_cargo_toml(args.fuchsia_dir)
    path_overrides = get_rust_package_paths(args.fuchsia_dir, cargo_toml)
    visibilities = get_rust_package_visibilities(args.fuchsia_dir, cargo_toml)
    check_package_groups_exist(args.fuchsia_dir, visibilities)

    for bf in build_files:
        process_one(bf, args.fuchsia_dir, path_overrides, visibilities)

    # Restricted crates in [patch.crates-io] live outside of --search-dir, so
    # this can't happen in process_one.
    for pkg_path in get_restricted_crate_packages(
        args.fuchsia_dir, path_overrides, visibilities
    ):
        restrict_crate_package(args.fuchsia_dir / pkg_path / "BUILD.bazel")

    return 0


if __name__ == "__main__":
    sys.exit(main())
