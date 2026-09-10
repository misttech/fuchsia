#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Android misc_info.txt utility.

Parses an Android `misc_info.txt` file and extracts vbmeta property descriptors
into JSON format.
"""

import argparse
import json
import pathlib


def _misc_info_to_dict(content: str) -> dict[str, str]:
    """Parses misc_info.txt contents into key-value pairs.

    Processes all lines that have KEY=VALUE format.
    """
    misc_info: dict[str, str] = {}
    for line in content.splitlines():
        if "=" in line:
            key, val = line.split("=", maxsplit=1)
            misc_info[key.strip()] = val.strip()
    return misc_info


def _extract_vbmeta_properties(key: str, value: str) -> dict[str, str]:
    """Extracts vbmeta property name/value pairs from a misc_info.txt key/value.

    Args:
        key: a misc_info.txt key
        value: a misc_info.txt value

    Returns:
        A {prop_name: prop_value} dict, or the empty dict if `key` doesn't look like a
        vbmeta commandline.
    """
    # Example:
    #   key: "avb_boot_add_hash_footer_args"
    #   value: "--prop foo:10 --prop bar:ABC --rollback_index 1000"
    if not (key.startswith("avb_") and key.endswith("_args")):
        return {}

    # misc_info.txt doesn't quote or escape, so we don't handle it for simplicity
    # but want to be alerted if we ever do need to start handling it.
    if any(c in value for c in ['"', "'", "\\"]):
        raise NotImplementedError(f"Unsupported meta-char in '{value}'")

    # Use argparse to extract just the `--prop` args we care about.
    parser = argparse.ArgumentParser(allow_abbrev=False, exit_on_error=False)
    parser.add_argument("--prop", action="append", default=[])
    try:
        parsed_args, _ = parser.parse_known_args(value.split())
    except argparse.ArgumentError as e:
        raise ValueError(f"Failed to parse arguments: {e}") from e

    props: dict[str, str] = {}
    for prop in parsed_args.prop:
        if ":" not in prop:
            raise ValueError(
                f"Invalid property format '{prop}', expected 'KEY:VALUE'"
            )
        prop_key, prop_val = prop.split(":", 1)
        props[prop_key] = prop_val

    return props


def process_misc_info(content: str) -> dict[str, str]:
    """Parses misc_info.txt and extracts vbmeta properties as a {name: value} dict."""
    misc_info_entries = _misc_info_to_dict(content)

    all_props: dict[str, str] = {}
    for key, value in misc_info_entries.items():
        new_props = _extract_vbmeta_properties(key, value)
        # We combine all properties into a single flat dict, discarding the specific
        # vbmeta blob the property was written into. Make sure each property is unique
        # so we don't silently clobber a property from one vbmeta image with a property
        # from another.
        overlap = all_props.keys() & new_props.keys()
        if overlap:
            raise ValueError(f"Duplicate properties found: {overlap}")
        all_props |= new_props

    return all_props


def _parse_args() -> argparse.Namespace:
    """Parses command-line arguments."""
    parser = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument(
        "misc_info",
        type=pathlib.Path,
        help="Path to misc_info.txt file",
    )
    parser.add_argument(
        "-o",
        "--output",
        type=pathlib.Path,
        help="Path to output JSON file (defaults to stdout)",
    )
    return parser.parse_args()


def main() -> None:
    args = _parse_args()
    props = process_misc_info(args.misc_info.read_text(encoding="utf-8"))
    output = json.dumps(props, indent=2, sort_keys=True)

    if args.output:
        args.output.write_text(output + "\n", encoding="utf-8")
    else:
        print(output)


if __name__ == "__main__":
    main()
