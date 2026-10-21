#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Logs /autoda-fix skill invocations with skill version and session model telemetry.

Executes at the start of Phase 0 (Step 0.1) in `/autoda-fix` to record:
- timestamp (UTC ISO-8601)
- user (LDAP)
- bug_id (Buganizer issue ID)
- conversation_id (Root orchestrator conversation UUID)
- trajectory_id (Resolved via reflection_cli or transcript.jsonl)
- skill ("autoda-fix")
- skill_version (Parsed from SKILL.md YAML frontmatter)
- model (Concise model + checkpoint summary)
- model_id (Logical model identifier)
- model_version (Full response model string from reflection_cli)
- verification_mode ("Mode A", "Mode B", or "Mode C" when selected)

When Fuchsia metrics collection (`fx metrics` / `ffx config analytics`) is
enabled, appends/upserts the record to the shared dashboard telemetry JSONL file
(and a local fallback in ~/.gemini/jetski/autoda_skill_invocations.jsonl).
Always prints the resolved JSON object to stdout for inclusion in `bug_spec.md`,
`bug_devlog.md`, and Gerrit commit trailers.
"""

import argparse
import datetime
import getpass
import json
import os
import re
import subprocess
import sys
from pathlib import Path
from typing import Any, Dict, Optional, Tuple

_REFLECTION_CLI = "/google/bin/releases/gemini-agents-reflection/reflection_cli"
_DEFAULT_SHARED_TELEMETRY_PATH = (
    "/google/data/rw/users/ch/chadnorvell/autoda/dashboard/data/"
    "skill_invocations.jsonl"
)
_DEFAULT_SKILL_VERSION = "1.2.0"


def read_skill_version(skill_md_path: Path) -> str:
    """Extracts `version:` from SKILL.md YAML frontmatter."""
    if not skill_md_path.is_file():
        return _DEFAULT_SKILL_VERSION
    try:
        content = skill_md_path.read_text(encoding="utf-8", errors="replace")[
            :2048
        ]
        m = re.search(r"(?m)^version\s*:\s*[\"']?([^\"'\n]+)[\"']?", content)
        if m:
            return m.group(1).strip()
    except OSError:
        pass
    return _DEFAULT_SKILL_VERSION


def format_concise_model(model_id: str, model_version: str) -> str:
    """Formats a concise human-readable model + version label."""
    short_id = model_id.replace("MODEL_PLACEHOLDER_", "").strip()
    checkpoint = ""
    if model_version:
        if "/ml-gemini-experimental/" in model_version:
            checkpoint = model_version.split("/ml-gemini-experimental/", 1)[
                1
            ].strip()
        elif "/tfhub/" in model_version:
            parts = [p for p in model_version.split("/") if p]
            checkpoint = (
                "/".join(parts[-2:]) if len(parts) >= 2 else model_version
            )
        else:
            checkpoint = model_version.strip()

    if checkpoint and short_id and short_id not in checkpoint:
        return f"{checkpoint} ({short_id})"
    if checkpoint:
        return checkpoint
    if short_id:
        return short_id
    return "unknown"


def query_session_telemetry(
    conversation_id: str,
) -> Tuple[str, str, str, str]:
    """Resolves (trajectory_id, model, model_id, model_version) for a session."""
    trajectory_id = ""
    model_id = ""
    model_version = ""

    if conversation_id and os.path.exists(_REFLECTION_CLI):
        try:
            proc_id = subprocess.run(
                [
                    _REFLECTION_CLI,
                    "info",
                    "-c",
                    conversation_id,
                    "-s",
                    "identity",
                ],
                capture_output=True,
                text=True,
                timeout=8,
                check=False,
            )
            if proc_id.returncode == 0:
                m_traj = re.search(
                    r"Trajectory ID:\s*([0-9a-fA-F-]{16,})", proc_id.stdout
                )
                if m_traj:
                    trajectory_id = m_traj.group(1).strip()
        except Exception:  # pylint: disable=broad-except
            pass

        try:
            proc_mod = subprocess.run(
                [
                    _REFLECTION_CLI,
                    "info",
                    "-c",
                    conversation_id,
                    "-s",
                    "model",
                ],
                capture_output=True,
                text=True,
                timeout=8,
                check=False,
            )
            if proc_mod.returncode == 0:
                m_curr = re.search(r"Current Model:\s*(\S+)", proc_mod.stdout)
                if m_curr:
                    model_id = m_curr.group(1).strip()
                m_resp = re.search(r"Response Model:\s*(\S+)", proc_mod.stdout)
                if m_resp:
                    model_version = m_resp.group(1).strip()
        except Exception:  # pylint: disable=broad-except
            pass

    # Fallback to local transcript files if reflection_cli was unavailable
    if conversation_id and (not model_id and not model_version):
        brain_root = (
            Path(
                os.environ.get(
                    "ANTIGRAVITY_APP_DATA_DIR",
                    str(Path.home() / ".gemini" / "jetski"),
                )
            )
            / "brain"
        )
        for fname in ("transcript_full.jsonl", "transcript.jsonl"):
            t_path = (
                brain_root
                / conversation_id
                / ".system_generated"
                / "logs"
                / fname
            )
            if not t_path.is_file():
                continue
            try:
                with t_path.open("r", encoding="utf-8", errors="replace") as f:
                    for line in f:
                        if "generatorModel" in line or "modelUsage" in line:
                            obj = json.loads(line)
                            meta = obj.get("metadata", {})
                            gen_mod = obj.get("generatorModel") or meta.get(
                                "generatorModel"
                            )
                            usage_mod = meta.get("modelUsage", {}).get(
                                "model"
                            ) or obj.get("model")
                            if gen_mod and not model_id:
                                model_id = str(gen_mod)
                            if usage_mod and not model_version:
                                model_version = str(usage_mod)
                            if model_id or model_version:
                                break
            except Exception:  # pylint: disable=broad-except
                pass
            if model_id or model_version:
                break

    concise_model = format_concise_model(model_id, model_version)
    return trajectory_id, concise_model, model_id, model_version


def upsert_jsonl_record(jsonl_path: Path, record: Dict[str, Any]) -> bool:
    """Appends or updates a telemetry record in a JSONL file."""
    try:
        jsonl_path.parent.mkdir(parents=True, exist_ok=True)
        existing_lines = []
        updated = False
        if jsonl_path.is_file():
            with jsonl_path.open("r", encoding="utf-8", errors="replace") as f:
                for raw_line in f:
                    line = raw_line.strip()
                    if not line:
                        continue
                    try:
                        obj = json.loads(line)
                        same_conv = bool(
                            record.get("conversation_id")
                            and obj.get("conversation_id")
                            == record.get("conversation_id")
                        )
                        same_bug = bool(
                            record.get("bug_id")
                            and obj.get("bug_id") == record.get("bug_id")
                        )
                        if same_conv and (same_bug or not obj.get("bug_id")):
                            merged = dict(obj)
                            for k, v in record.items():
                                if v != "" and v is not None:
                                    merged[k] = v
                            existing_lines.append(
                                json.dumps(merged, sort_keys=False)
                            )
                            updated = True
                        else:
                            existing_lines.append(line)
                    except ValueError:
                        existing_lines.append(line)

        if not updated:
            existing_lines.append(json.dumps(record, sort_keys=False))

        with jsonl_path.open("w", encoding="utf-8") as f:
            for l in existing_lines:
                f.write(l + "\n")
        return True
    except OSError:
        return False


def normalize_verification_mode(raw_mode: str) -> str:
    """Normalizes verification mode strings to 'Mode A', 'Mode B', or 'Mode C'."""
    val = (raw_mode or "").strip().strip('"').strip("'")
    if not val or "pending" in val.lower():
        return ""
    m = re.search(r"\b(?:Mode\s*)?([ABC])\b", val, re.IGNORECASE)
    if m:
        return f"Mode {m.group(1).upper()}"
    return val


def read_spec_frontmatter_fields(
    conversation_id: str,
) -> Tuple[str, str, Optional[int]]:
    """Reads (verification_mode, adversarial_review, adversarial_review_rounds) from bug_spec.md."""
    if not conversation_id:
        return "", "", None
    brain_root = (
        Path(
            os.environ.get(
                "ANTIGRAVITY_APP_DATA_DIR",
                str(Path.home() / ".gemini" / "jetski"),
            )
        )
        / "brain"
    )
    spec_path = brain_root / conversation_id / "bug_spec.md"
    if not spec_path.is_file():
        return "", "", None
    mode = ""
    adv_review = ""
    adv_rounds: Optional[int] = None
    try:
        content = spec_path.read_text(encoding="utf-8", errors="replace")[:4096]
        m_mode = re.search(
            r"(?m)^verification_mode\s*:\s*[\"']?([^\"'\n]+)[\"']?", content
        )
        if m_mode:
            mode = normalize_verification_mode(m_mode.group(1))
        m_adv = re.search(
            r"(?m)^adversarial_review\s*:\s*[\"']?([^\"'\n]+)[\"']?", content
        )
        if m_adv:
            val = m_adv.group(1).strip().lower()
            if "pending" not in val:
                adv_review = val
        m_rounds = re.search(
            r"(?m)^adversarial_review_rounds\s*:\s*(\d+)", content
        )
        if m_rounds:
            adv_rounds = int(m_rounds.group(1))
    except OSError:
        pass
    return mode, adv_review, adv_rounds


def _get_fuchsia_metrics_dir() -> Path:
    """Returns the Fuchsia metrics configuration directory (~/.local/share/Fuchsia/metrics)."""
    xdg_data = os.environ.get("XDG_DATA_HOME", "").strip()
    base = Path(xdg_data) if xdg_data else (Path.home() / ".local" / "share")
    return base / "Fuchsia" / "metrics"


def is_fx_metrics_enabled(fuchsia_dir: Path) -> bool:
    """Checks whether Fuchsia metrics collection is enabled via `fx metrics` / `ffx config analytics`.

    Follows the configuration hierarchy in `//tools/devshell/lib/metrics.sh`:
    1. `${XDG_DATA_HOME:-~/.local/share}/Fuchsia/metrics/analytics-status-internal` (`1` or `2` = enabled)
    2. `${XDG_DATA_HOME:-~/.local/share}/Fuchsia/metrics/analytics-status` (`1` = enabled, `0` = disabled)
    3. `${FUCHSIA_DIR}/.fx/config/metrics` (or `.fx-metrics-config`) (`METRICS_ENABLED=1` with `METRICS_UUID`)
    """
    metrics_dir = _get_fuchsia_metrics_dir()
    internal_status_file = metrics_dir / "analytics-status-internal"
    if internal_status_file.is_file():
        try:
            val = internal_status_file.read_text(
                encoding="utf-8", errors="replace"
            ).strip()
            return val in ("1", "2")
        except OSError:
            return False

    status_file = metrics_dir / "analytics-status"
    if status_file.is_file():
        try:
            val = (
                status_file.read_text(encoding="utf-8", errors="replace")
                .strip()
                .lower()
            )
            if val in ("0", "false", "disabled"):
                return False
            if val in ("1", "2", "true", "enabled"):
                return True
        except OSError:
            pass

    for rel in (Path(".fx/config/metrics"), Path(".fx-metrics-config")):
        cfg_path = fuchsia_dir / rel
        if not cfg_path.is_file():
            continue
        try:
            content = cfg_path.read_text(encoding="utf-8", errors="replace")
            m_en = re.search(
                r"(?m)^METRICS_ENABLED\s*=\s*[\"']?(\d+)[\"']?", content
            )
            m_uuid = re.search(
                r"(?m)^METRICS_UUID\s*=\s*[\"']?([^\"'\s]+)[\"']?", content
            )
            if m_en:
                return m_en.group(1) == "1" and bool(
                    m_uuid and m_uuid.group(1).strip()
                )
        except OSError:
            pass

    return False


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Log /autoda-fix invocation telemetry (Signal B)."
    )
    parser.add_argument(
        "--conversation-id",
        default="",
        help="Root Bug Orchestrator conversation UUID.",
    )
    parser.add_argument(
        "--bug-id",
        default="",
        help="Buganizer issue ID.",
    )
    parser.add_argument(
        "--verification-mode",
        "--mode",
        dest="verification_mode",
        default="",
        help="Selected verification mode ('Mode A', 'Mode B', or 'Mode C').",
    )
    parser.add_argument(
        "--adversarial-review",
        default="",
        help="Adversarial review policy ('enabled' or 'skipped').",
    )
    parser.add_argument(
        "--adversarial-review-rounds",
        type=int,
        default=None,
        help="Number of adversarial review rounds executed (0..3).",
    )
    parser.add_argument(
        "--skill-version",
        default="",
        help="Optional override for skill version.",
    )
    parser.add_argument(
        "--model",
        default="",
        help="Optional override for session model name/version.",
    )
    parser.add_argument(
        "--telemetry-path",
        default=os.environ.get(
            "AUTODA_TELEMETRY_PATH", _DEFAULT_SHARED_TELEMETRY_PATH
        ),
        help="Path to shared skill_invocations.jsonl.",
    )
    args = parser.parse_args()

    here = Path(__file__).resolve().parent
    skill_md = here.parent / "SKILL.md"
    fuchsia_dir = Path(
        os.environ.get("FUCHSIA_DIR", str(here.parents[3]))
    ).resolve()
    skill_version = args.skill_version.strip() or read_skill_version(skill_md)

    clean_bug = re.sub(r"^(?:b/|bug:)", "", args.bug_id.strip(), flags=re.I)
    m_digits = re.search(r"(\d{5,})", clean_bug)
    bug_id = m_digits.group(1) if m_digits else clean_bug

    conv_id = args.conversation_id.strip()
    traj_id, resolved_model, model_id, model_version = query_session_telemetry(
        conv_id
    )
    final_model = args.model.strip() or resolved_model
    spec_mode, spec_adv, spec_rounds = read_spec_frontmatter_fields(conv_id)
    verification_mode = (
        normalize_verification_mode(args.verification_mode) or spec_mode
    )
    adv_review = args.adversarial_review.strip().lower() or spec_adv
    adv_rounds = (
        args.adversarial_review_rounds
        if args.adversarial_review_rounds is not None
        else spec_rounds
    )

    try:
        user = os.environ.get("USER") or getpass.getuser()
    except Exception:  # pylint: disable=broad-except
        user = "unknown"

    record: Dict[str, Any] = {
        "timestamp": (
            datetime.datetime.now(datetime.timezone.utc).strftime(
                "%Y-%m-%dT%H:%M:%SZ"
            )
        ),
        "user": user,
        "bug_id": bug_id,
        "conversation_id": conv_id,
        "trajectory_id": traj_id,
        "skill": "autoda-fix",
        "skill_version": skill_version,
        "model": final_model,
        "model_id": model_id,
        "model_version": model_version,
        "verification_mode": verification_mode,
        "adversarial_review": adv_review,
    }
    if adv_rounds is not None:
        record["adversarial_review_rounds"] = adv_rounds

    metrics_enabled = is_fx_metrics_enabled(fuchsia_dir)
    shared_ok = False
    local_ok = False
    if metrics_enabled:
        if args.telemetry_path:
            shared_ok = upsert_jsonl_record(Path(args.telemetry_path), record)

        local_fallback = (
            Path.home()
            / ".gemini"
            / "jetski"
            / "autoda_skill_invocations.jsonl"
        )
        local_ok = upsert_jsonl_record(local_fallback, record)

    output = dict(record)
    output["metrics_enabled"] = metrics_enabled
    output["logged_to_shared"] = shared_ok
    output["logged_to_local"] = local_ok
    print(json.dumps(output, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main())
