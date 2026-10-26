# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Allowlist loading, schema validation, and matching for Bazel lint checks."""

import dataclasses
import json
import re
from pathlib import Path
from typing import Sequence

_BUG_URL_RE = re.compile(r"^https://fxbug\.dev/\d+$")
_ALLOWED_ENTRY_KEYS = frozenset(
    {"path", "path_prefix", "bug", "comment", "targets"}
)
_ALLOWED_VENDOR_PATHS = frozenset(
    {"vendor/BUILD.bazel", "vendor/google/BUILD.bazel"}
)
_ALLOWED_VENDOR_PATH_PREFIXES = frozenset({"vendor/", "vendor/google/"})


class AllowlistError(ValueError):
    """Raised when an allowlist JSON file fails schema validation."""


@dataclasses.dataclass(frozen=True)
class AllowlistEntry:
    """A single validated allowlist entry."""

    bug: str
    path: str | None = None
    path_prefix: str | None = None
    comment: str | None = None
    targets: frozenset[str] | None = None


class Allowlist:
    """Validated allowlist for a single Bazel lint check."""

    def __init__(self, entries: Sequence[AllowlistEntry] = ()) -> None:
        self._entries: tuple[AllowlistEntry, ...] = tuple(entries)

    @property
    def entries(self) -> tuple[AllowlistEntry, ...]:
        return self._entries

    def is_file_allowlisted(self, rel_path: str) -> bool:
        """Returns True if an entire file is exempted (not scoped to targets)."""
        for entry in self._entries:
            if entry.targets is not None:
                continue
            if entry.path is not None and rel_path == entry.path:
                return True
            if entry.path_prefix is not None and rel_path.startswith(
                entry.path_prefix
            ):
                return True
        return False

    def is_finding_allowlisted(
        self, rel_path: str, targets: Sequence[str] = ()
    ) -> bool:
        """Returns True if a finding (optionally scoped to targets) is exempted."""
        if self.is_file_allowlisted(rel_path):
            return True
        if not targets:
            return False

        allowed_targets: set[str] = set()
        for entry in self._entries:
            if entry.path == rel_path and entry.targets is not None:
                allowed_targets.update(entry.targets)
        return bool(allowed_targets) and all(
            t in allowed_targets for t in targets
        )


def load_allowlist(
    allowlist_path: Path, *, supports_targets_allowlist: bool = False
) -> Allowlist:
    """Loads and validates an allowlist JSON file.

    If `allowlist_path` does not exist, returns an empty `Allowlist`.
    """
    if not allowlist_path.is_file():
        return Allowlist()

    try:
        raw = json.loads(allowlist_path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as e:
        raise AllowlistError(f"{allowlist_path}: invalid JSON: {e}") from e

    if not isinstance(raw, dict):
        raise AllowlistError(
            f"{allowlist_path}: top-level JSON value must be an object"
        )

    unknown_top = set(raw.keys()) - {"entries"}
    if unknown_top:
        raise AllowlistError(
            f"{allowlist_path}: unknown top-level keys: {sorted(unknown_top)}"
        )

    raw_entries = raw.get("entries")
    if not isinstance(raw_entries, list):
        raise AllowlistError(f"{allowlist_path}: 'entries' must be a list")

    entries: list[AllowlistEntry] = []
    for i, item in enumerate(raw_entries):
        loc = f"{allowlist_path}: entries[{i}]"
        if not isinstance(item, dict):
            raise AllowlistError(f"{loc} must be an object")

        unknown_keys = set(item.keys()) - _ALLOWED_ENTRY_KEYS
        if unknown_keys:
            raise AllowlistError(
                f"{loc} has unknown keys: {sorted(unknown_keys)}"
            )

        has_path = "path" in item
        has_prefix = "path_prefix" in item
        if has_path == has_prefix:
            raise AllowlistError(
                f"{loc} must specify exactly one of 'path' or 'path_prefix'"
            )

        path: str | None = None
        path_prefix: str | None = None
        if has_path:
            if (
                not isinstance(item["path"], str)
                or not item["path"]
                or item["path"].endswith("/")
            ):
                raise AllowlistError(
                    f"{loc} 'path' must be a non-empty file path not ending with '/'"
                )
            path = item["path"]
            if path.startswith("vendor") and path not in _ALLOWED_VENDOR_PATHS:
                raise AllowlistError(
                    f"{loc} 'path' starting with 'vendor' must be one of "
                    f"{sorted(_ALLOWED_VENDOR_PATHS)} to avoid leaking private paths"
                )
        else:
            if (
                not isinstance(item["path_prefix"], str)
                or not item["path_prefix"]
                or not item["path_prefix"].endswith("/")
            ):
                raise AllowlistError(
                    f"{loc} 'path_prefix' must be a non-empty directory prefix ending with '/'"
                )
            path_prefix = item["path_prefix"]
            if (
                path_prefix.startswith("vendor")
                and path_prefix not in _ALLOWED_VENDOR_PATH_PREFIXES
            ):
                raise AllowlistError(
                    f"{loc} 'path_prefix' starting with 'vendor' must be one of "
                    f"{sorted(_ALLOWED_VENDOR_PATH_PREFIXES)} to avoid leaking private paths"
                )

        bug = item.get("bug")
        if not isinstance(bug, str) or not _BUG_URL_RE.match(bug):
            raise AllowlistError(
                f"{loc} 'bug' must be a string matching 'https://fxbug.dev/<id>'"
            )

        comment: str | None = None
        if "comment" in item:
            if not isinstance(item["comment"], str) or not item["comment"]:
                raise AllowlistError(
                    f"{loc} 'comment' must be a non-empty string"
                )
            comment = item["comment"]

        targets: frozenset[str] | None = None
        if "targets" in item:
            if not supports_targets_allowlist:
                raise AllowlistError(
                    f"{loc} 'targets' is not supported for this check"
                )
            if path is None:
                raise AllowlistError(
                    f"{loc} 'targets' can only be used with 'path', not 'path_prefix'"
                )
            raw_targets = item["targets"]
            if (
                not isinstance(raw_targets, list)
                or not raw_targets
                or not all(isinstance(t, str) and t for t in raw_targets)
            ):
                raise AllowlistError(
                    f"{loc} 'targets' must be a non-empty list of non-empty strings"
                )
            targets = frozenset(raw_targets)

        entries.append(
            AllowlistEntry(
                bug=bug,
                path=path,
                path_prefix=path_prefix,
                comment=comment,
                targets=targets,
            )
        )

    return Allowlist(entries)
