# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import json
import os
import re
import shutil
import subprocess
import sys


def get_fuchsia_dir():
    """Resolves the root Fuchsia directory path."""
    if "FUCHSIA_DIR" in os.environ:
        return os.environ["FUCHSIA_DIR"]
    # Fallback: Resolve relative to this script's path (four directories up from docs/skills/search-driver-docs/scripts/)
    script_dir = os.path.dirname(os.path.abspath(__file__))
    return os.path.abspath(os.path.join(script_dir, "../../../../"))


def extract_title(file_path):
    """Reads the first H1 (# Title) heading from a markdown file as the title.

    Fallback is the capitalized basename of the file.
    """
    try:
        with open(file_path, "r", encoding="utf-8") as f:
            for line in f:
                line = line.strip()
                if line.startswith("# "):
                    return line[2:].strip()
    except Exception as e:
        print(f"Warning: Could not read title from {file_path}: {e}")

    # Fallback: capitalized file name without extension
    basename = os.path.basename(file_path)
    name_without_ext = os.path.splitext(basename)[0]
    return name_without_ext.replace("-", " ").replace("_", " ").title()


def run_gemma(litert_lm_bin, fuchsia_dir, preset_path, target_file):
    """Executes litert-lm run to extract metadata for a single target file."""
    cmd = [
        litert_lm_bin,
        "run",
        "gemma4-e4b",
        f"--preset={preset_path}",
        f"--prompt=Process the file at {target_file}",
    ]

    print(f"Running Gemma for {os.path.relpath(target_file, fuchsia_dir)}...")
    try:
        result = subprocess.run(
            cmd, capture_output=True, text=True, check=True, timeout=120
        )
        return result.stdout
    except subprocess.TimeoutExpired:
        print(f"Error: Timeout expired while processing {target_file}")
        return ""
    except subprocess.CalledProcessError as e:
        print(f"Error running litert-lm: {e.stderr}")
        return ""
    except Exception as e:
        print(f"Error: {e}")
        return ""


def parse_model_output(output):
    """Parses description and keywords from the raw litert-lm stdout."""
    # Find description
    desc_matches = re.findall(
        r"(?:description|desc):\s*(.*)", output, re.IGNORECASE
    )
    description = desc_matches[-1].strip() if desc_matches else ""

    # Find keywords
    kw_matches = re.findall(
        r"(?:keywords|keyword|tags):\s*(.*)", output, re.IGNORECASE
    )
    keywords = []
    if kw_matches:
        # Split by comma, bullet points, or newlines
        kws = re.split(r"[,\n•]", kw_matches[-1])
        keywords = [k.strip().strip("\"*'").strip() for k in kws if k.strip()]
        # Clean up empty/invalid keywords
        keywords = [k for k in keywords if len(k) > 1]

    return description, keywords


def parse_existing_yaml(yaml_path):
    """Parses the existing driver-docs-index.yaml file into a dict mapping path to entry.

    NOTE: To keep this script portable and free of external library dependencies (like
    PyYAML) within the Fuchsia source tree, we use a custom parser tailored to a strict,
    deterministic subset of YAML.

    The expected format is a 'JSON-in-YAML' style, where values are JSON-serialized
    one-liners. Lines are structured as:
      - description: "JSON_STRING"
        path: "JSON_STRING"
        title: "JSON_STRING"
        keywords: ["JSON_STRING", ...]
    """
    if not os.path.exists(yaml_path):
        return {}

    try:
        with open(yaml_path, "r", encoding="utf-8") as f:
            content = f.read()
    except Exception as e:
        print(f"Error reading index file {yaml_path}: {e}")
        return {}

    entries = {}
    current_entry = None
    for line in content.splitlines():
        line_stripped = line.strip()
        if not line_stripped or line_stripped == "---":
            continue

        if line.startswith("- "):
            if current_entry and "path" in current_entry:
                entries[current_entry["path"]] = current_entry
            current_entry = {}
            line_stripped = line_stripped[2:]
        elif current_entry is None:
            continue

        if ":" in line_stripped:
            key, val = line_stripped.split(":", 1)
            key = key.strip()
            val = val.strip()
            try:
                current_entry[key] = json.loads(val)
            except json.JSONDecodeError:
                # Fallback for legacy or manually-edited lines that might not be valid JSON
                if val.startswith("[") and val.endswith("]"):
                    items = val[1:-1].split(",")
                    current_entry[key] = [
                        i.strip().strip("\"'") for i in items if i.strip()
                    ]
                else:
                    current_entry[key] = val.strip("\"'")

    if current_entry and "path" in current_entry:
        entries[current_entry["path"]] = current_entry

    return entries


def write_yaml_index(yaml_path, entries_dict, updated_file=None):
    """Writes the dictionary of entries back to the driver-docs-index.yaml file."""
    lines = []
    # Sort entries by path for determinism and clean git history
    for path in sorted(entries_dict.keys()):
        entry = entries_dict[path]
        # Output description, path, title, keywords
        lines.append(
            f"- description: {json.dumps(entry.get('description', ''))}"
        )
        lines.append(f"  path: {json.dumps(path)}")
        lines.append(f"  title: {json.dumps(entry.get('title', ''))}")

        keywords = entry.get("keywords", [])
        keywords_str = ", ".join(json.dumps(kw) for kw in keywords)
        lines.append(f"  keywords: [{keywords_str}]")

    lines.append("")

    try:
        with open(yaml_path, "w", encoding="utf-8") as f:
            f.write("\n".join(lines))
        if updated_file:
            print(
                f"Incrementally saved index with {len(entries_dict)} entries to {yaml_path} (after updating metadata for '{updated_file}')"
            )
        else:
            print(
                f"Successfully saved final index with {len(entries_dict)} entries to {yaml_path}"
            )
    except Exception as e:
        print(f"Error writing YAML index: {e}")


def main():
    parser = argparse.ArgumentParser(
        description="Generate/update Fuchsia driver documentation metadata catalog index."
    )
    parser.add_argument(
        "targets",
        nargs="*",
        help="Optional custom target directory path(s) or file path(s) to scan. If omitted, default standard directories will be scanned.",
    )
    args = parser.parse_args()

    fuchsia_dir = get_fuchsia_dir()
    script_dir = os.path.dirname(os.path.abspath(__file__))
    preset_path = os.path.join(script_dir, "extract_metadata.py")
    yaml_path = os.path.abspath(
        os.path.join(script_dir, "../assets/driver-docs-index.yaml")
    )

    print(f"Fuchsia Directory: {fuchsia_dir}")
    print(f"Preset Script: {preset_path}")
    print(f"Index File: {yaml_path}")

    # 1. Pre-flight verification check for litert-lm
    litert_lm_bin = shutil.which("litert-lm")
    if not litert_lm_bin:
        # Fallback: check standard local bin location
        local_bin = os.path.expanduser("~/.local/bin/litert-lm")
        if os.path.exists(local_bin):
            litert_lm_bin = local_bin
        else:
            print(
                "Error: 'litert-lm' utility not found. Please make sure it is installed and accessible."
            )
            print("Run: uv tool install litert-lm")
            sys.exit(1)

    # 2. Verify model gemma4-e4b is imported and available
    try:
        res = subprocess.run(
            [litert_lm_bin, "list"], capture_output=True, text=True, timeout=10
        )
        if "gemma4-e4b" not in res.stdout:
            print(
                "Error: Local model 'gemma4-e4b' is not imported. Please import it before running this script."
            )
            print(
                "Run: litert-lm import --from-huggingface-repo=litert-community/gemma-4-E4B-it-litert-lm gemma-4-E4B-it.litertlm gemma4-e4b"
            )
            sys.exit(1)
    except Exception as e:
        print(f"Warning: Pre-flight model listing check failed: {e}")

    if not os.path.exists(preset_path):
        print(f"Error: Preset script not found at {preset_path}")
        sys.exit(1)

    if args.targets:
        # Resolve target paths
        targets = [os.path.abspath(t) for t in args.targets]
    else:
        # Locate standard target directories relative to fuchsia root
        targets = [
            os.path.join(fuchsia_dir, "docs/development/drivers"),
            os.path.join(fuchsia_dir, "docs/concepts/drivers"),
        ]

    # Find all markdown files
    md_files = []
    for target in targets:
        if not os.path.exists(target):
            print(f"Warning: Target {target} does not exist. Skipping.")
            continue
        if os.path.isdir(target):
            for root, _, files in os.walk(target):
                for file in files:
                    if file.endswith(".md"):
                        md_files.append(os.path.join(root, file))
        else:
            if target.endswith(".md"):
                md_files.append(target)

    print(f"Found {len(md_files)} driver documentation Markdown files.")

    # Load existing index
    entries_dict = parse_existing_yaml(yaml_path)
    print(f"Loaded {len(entries_dict)} existing entries from the index.")

    updated_count = 0
    failed_files = []
    try:
        for target_file in md_files:
            rel_path = os.path.relpath(target_file, fuchsia_dir)

            success = False
            description, keywords = "", []
            for attempt in range(1, 4):
                if attempt > 1:
                    print(
                        f"Retrying extraction for {rel_path} (Attempt {attempt}/3)..."
                    )

                # Run model to extract metadata
                stdout = run_gemma(
                    litert_lm_bin, fuchsia_dir, preset_path, target_file
                )
                if not stdout:
                    print(
                        f"Warning: Attempt {attempt} failed to get metadata for {rel_path}."
                    )
                    continue

                desc_candidate, kw_candidate = parse_model_output(stdout)
                if not desc_candidate:
                    print(
                        f"Warning: Attempt {attempt} failed to parse description for {rel_path}."
                    )
                    continue

                description, keywords = desc_candidate, kw_candidate
                success = True
                break

            if not success:
                print(
                    f"Error: All 3 attempts failed to retrieve valid metadata for {rel_path}. Skipping."
                )
                failed_files.append(rel_path)
                continue

            title = extract_title(target_file)

            entries_dict[rel_path] = {
                "description": description,
                "path": rel_path,
                "title": title,
                "keywords": keywords,
            }
            updated_count += 1

            # Incremental Save: Write index back to file immediately after successful extraction
            # This prevents losing progress if interrupted or crashed halfway through a large batch
            write_yaml_index(yaml_path, entries_dict, updated_file=rel_path)

        # Print final summary report
        print("\n" + "=" * 50)
        print("             INDEX GENERATION REPORT")
        print("=" * 50)
        print(f"Total files processed: {len(md_files)}")
        print(f"Successfully indexed:  {updated_count}")
        print(f"Failed to index:        {len(failed_files)}")
        if failed_files:
            print(
                "\nThe following files failed to yield valid metadata after 3 retries:"
            )
            for f in sorted(failed_files):
                print(f"  - {f}")
        else:
            print("\nAll driver documents were successfully indexed!")
        print("=" * 50 + "\n")

    except KeyboardInterrupt:
        print(
            "\nProcess interrupted by user. Saving current progress and exiting..."
        )
        if updated_count > 0:
            write_yaml_index(yaml_path, entries_dict)
        sys.exit(130)


if __name__ == "__main__":
    main()
