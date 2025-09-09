#!/usr/bin/env fuchsia-vendored-python
#!/usr/bin/env python3
# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import json
import os
import sys

# Hardcoded dictionary of public MCP servers in fuchsia.git (not vendor/google)
SERVERS_TO_ADD: dict[str, dict[str, str]] = {}
INTERNAL_SERVERS_PATH = (
    "vendor/google/tools/gemini-extensions/mcp-servers.json",
)
GOOGLE3_SERVERS_PATH = (
    "vendor/google/tools/gemini-extensions/google3-mcp-servers.json",
)


def main() -> None:
    parser = argparse.ArgumentParser(description="Set up MCP servers.")
    parser.add_argument("settings_path", help="Path to the settings.json file.")
    parser.add_argument("fuchsia_dir", help="Path to the FUCHSIA_DIR.")
    parser.add_argument(
        "--uninstall",
        action="store_true",
        help="Remove the servers instead of adding them.",
    )
    parser.add_argument(
        "--extension",
        action="store_true",
        help="Install as a self-contained extension.",
    )
    args = parser.parse_args()

    if os.getcwd() != args.fuchsia_dir:
        print(
            f"Error: This script must be run from the FUCHSIA_DIR ({args.fuchsia_dir}).",
            file=sys.stderr,
        )
        sys.exit(1)

    internal_servers_path = os.path.join(
        args.fuchsia_dir, *INTERNAL_SERVERS_PATH
    )
    google3_servers_path = os.path.join(args.fuchsia_dir, *GOOGLE3_SERVERS_PATH)

    # Combine public and internal server lists.
    servers_to_process = SERVERS_TO_ADD.copy()
    if os.path.exists("/google"):
        with open(google3_servers_path, "r") as f:
            try:
                google3_servers = json.load(f)
                servers_to_process.update(google3_servers)
            except json.JSONDecodeError:
                print(
                    f"Warning: Could not decode JSON from {google3_servers_path}"
                )

    if os.path.exists(internal_servers_path):
        with open(internal_servers_path, "r") as f:
            try:
                internal_servers = json.load(f)
                servers_to_process.update(internal_servers)
            except json.JSONDecodeError:
                print(
                    f"Warning: Could not decode JSON from {internal_servers_path}"
                )

    if args.extension:
        # Extension mode
        if args.uninstall:
            if os.path.exists(args.settings_path):
                os.remove(args.settings_path)
                print(f"Removed extension file: {args.settings_path}")
            else:
                print("Extension file does not exist. Nothing to remove.")
            return

        extension_data = {
            "name": "fuchsia",
            "version": "1.0.0",
            "mcpServers": servers_to_process,
        }
        with open(args.settings_path, "w") as f:
            json.dump(extension_data, f, indent=4)
        print(f"Wrote MCP extension settings to {args.settings_path}")

    else:
        # Default (settings.json) mode
        settings = {}
        if not os.path.exists(args.settings_path):
            if args.uninstall:
                print("Settings file does not exist. Nothing to remove.")
                return
        else:
            with open(args.settings_path, "r") as f:
                try:
                    settings = json.load(f)
                except json.JSONDecodeError:
                    print(
                        f"Warning: Could not decode JSON from {args.settings_path}, starting with a new configuration."
                    )

        if "mcpServers" not in settings:
            settings["mcpServers"] = {}
        elif isinstance(settings["mcpServers"], list):
            # Convert list to dict for backward compatibility
            server_list = settings["mcpServers"]
            settings["mcpServers"] = {}
            for server in server_list:
                if isinstance(server, dict) and "name" in server:
                    settings["mcpServers"][server["name"]] = server

        if args.uninstall:
            for server_name in servers_to_process:
                if server_name in settings["mcpServers"]:
                    del settings["mcpServers"][server_name]
        else:
            # Add or update servers.
            settings["mcpServers"].update(servers_to_process)

        # Write the updated settings back to the file.
        with open(args.settings_path, "w") as f:
            json.dump(settings, f, indent=4)
        print(f"Updated MCP server settings in {args.settings_path}")


if __name__ == "__main__":
    main()
