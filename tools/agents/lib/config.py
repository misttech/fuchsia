# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Configuration management, atomic writing, and grant merging for AI coding agents."""

from __future__ import annotations

import json
import os
import pathlib
import shutil
import sys
from collections.abc import Sequence
from typing import Any


def get_default_config_path() -> pathlib.Path:
    """Resolve default Gemini config path, respecting GEMINI_CONFIG_DIR or GEMINI_HOME."""
    if "GEMINI_CONFIG_DIR" in os.environ:
        return pathlib.Path(os.environ["GEMINI_CONFIG_DIR"]) / "config.json"
    if "GEMINI_HOME" in os.environ:
        return (
            pathlib.Path(os.environ["GEMINI_HOME"]) / "config" / "config.json"
        )
    return pathlib.Path.home() / ".gemini" / "config" / "config.json"


DEFAULT_CONFIG_PATH = get_default_config_path()


def load_config(config_path: pathlib.Path) -> dict[str, Any]:
    """Load and parse JSON configuration from config_path.

    Returns an empty dict if the file does not exist or does not contain a JSON object.
    """
    if not config_path.is_file():
        return {}
    data = json.loads(config_path.read_text(encoding="utf-8"))
    return data if isinstance(data, dict) else {}


def save_config_atomic(
    config_path: pathlib.Path, config_data: dict[str, Any]
) -> None:
    """Atomically write JSON configuration to disk via a temporary file."""
    config_path.parent.mkdir(parents=True, exist_ok=True)
    temp_config = config_path.with_name(f".{config_path.name}.tmp")
    temp_config.write_text(
        json.dumps(config_data, indent=2) + "\n", encoding="utf-8"
    )
    temp_config.replace(config_path)


def merge_grants(
    existing: Sequence[str], to_add: Sequence[str]
) -> tuple[list[str], list[str]]:
    """Merge new grants into existing grant list, preserving order and deduplicating.

    Returns:
        tuple of (updated_list, added_items)
    """
    seen = set(existing)
    updated = list(existing)
    added: list[str] = []
    for item in to_add:
        if item not in seen:
            seen.add(item)
            updated.append(item)
            added.append(item)
    return updated, added


def apply_grants(
    config_path: pathlib.Path | None = None,
    allow: Sequence[str] = (),
    deny: Sequence[str] = (),
    ask: Sequence[str] = (),
    dry_run: bool = False,
) -> bool:
    """Load config.json, merge allow/deny/ask grants, create backup, and write atomically."""
    config_path = config_path or get_default_config_path()
    try:
        config_data = load_config(config_path)
    except json.JSONDecodeError as error:
        print(
            f"Error reading JSON from {config_path}: {error}",
            file=sys.stderr,
        )
        return False

    user_settings = config_data.setdefault("userSettings", {})
    grants = user_settings.setdefault("globalPermissionGrants", {})

    categories = (
        ("allow", allow),
        ("deny", deny),
        ("ask", ask),
    )

    print("\n=== Permission Updates Summary ===")
    total_added = 0
    for category_name, new_rules in categories:
        existing_rules = grants.get(category_name, [])
        updated_rules, added_rules = merge_grants(existing_rules, new_rules)
        grants[category_name] = updated_rules
        total_added += len(added_rules)
        if added_rules:
            print(f"  + Added to [{category_name}] ({len(added_rules)}):")
            for grant in added_rules:
                print(f"      - {grant}")

    if total_added == 0:
        print(
            "  (No new rules to add; all entries already present in config.json)"
        )

    if dry_run:
        print(f"\n[DRY RUN] Would write updated config to: {config_path}")
        if config_path.exists():
            print(f"[DRY RUN] Would create backup at: {config_path}.bak")
        return True

    if config_path.exists():
        backup_path = config_path.with_name(f"{config_path.name}.bak")
        shutil.copy2(config_path, backup_path)
        print(f"\nCreated backup of existing config at: {backup_path}")

    save_config_atomic(config_path, config_data)
    print(f"\nSuccessfully wrote updated config to: {config_path}")
    return True
