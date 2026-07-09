# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import os
import re
import sys


def find_oneof_data_closing_brace(proto_content: str) -> int:
    """Finds the character index of the closing brace for the 'oneof data' block.

    Args:
        proto_content: The full content of the proto file to search.

    Returns:
        The 0-based character index of the closing brace ('}') of the block.

    Raises:
        ValueError: If 'oneof data' or its braces are not found or matched.
    """
    oneof_start = proto_content.find("oneof data {")
    if oneof_start == -1:
        raise ValueError("'oneof data {' not found in trace proto")

    brace_index = proto_content.find("{", oneof_start)
    if brace_index == -1:
        raise ValueError("Open brace not found after 'oneof data'")

    brace_count = 1
    for i in range(brace_index + 1, len(proto_content)):
        if proto_content[i] == "{":
            brace_count += 1
        elif proto_content[i] == "}":
            brace_count -= 1
            if brace_count == 0:
                return i

    raise ValueError("Could not find closing brace of 'oneof data' block")


def remove_field_from_extensions(proto_content: str, field_id: str) -> str:
    """Removes a field ID from the extension ranges lists in the proto content.

    Args:
        proto_content: The full content of the proto file.
        field_id: The string field ID to remove.

    Returns:
        The updated proto content with the field ID removed from extension lists.

    Raises:
        ValueError: If no extension range containing the field ID is found.
    """
    ext_range_match = re.search(
        rf"extensions\s+([^;]*?\b{field_id}\b[^;]*?);", proto_content
    )
    if not ext_range_match:
        raise ValueError(
            f"Could not find extension range containing {field_id}"
        )

    ext_range_target = ext_range_match.group(0)
    ext_range_replacement = ext_range_target
    if re.search(rf"extensions\s+\b{field_id}\b\s*;", ext_range_replacement):
        ext_range_replacement = ""
    else:
        ext_range_replacement = re.sub(
            rf"\b{field_id}\b\s*,\s*", "", ext_range_replacement
        )
        ext_range_replacement = re.sub(
            rf"\s*,\s*\b{field_id}\b", "", ext_range_replacement
        )

    return proto_content.replace(ext_range_target, ext_range_replacement)


def merge_extension_field(
    proto_content: str,
    message_type: str,
    field_name: str,
    field_id: str,
) -> str:
    """Inserts the merged field definition into the 'oneof data' block.

    Args:
        proto_content: The content of the main trace proto file.
        message_type: The name of the message type (e.g. FrameTimelineEvent).
        field_name: The name of the field to add in TracePacket (e.g. frame_timeline_event).
        field_id: The field ID to assign to the new field (e.g. 76).

    Returns:
        The updated trace content.

    Raises:
        ValueError: If 'oneof data' block closing brace cannot be found.
    """
    closing_index = find_oneof_data_closing_brace(proto_content)
    insertion = f"  {message_type} {field_name} = {field_id};\n  "
    return (
        proto_content[:closing_index]
        + insertion
        + proto_content[closing_index:]
    )


def main() -> None:
    """Main entrypoint for resolving proto extensions at build time."""
    parser = argparse.ArgumentParser(
        description="Resolve proto extensions at build time for Perfetto trace proto file"
    )
    parser.add_argument(
        "--input-proto",
        required=True,
        help="Path to input perfetto_trace.proto",
    )
    parser.add_argument(
        "--input-extensions",
        required=True,
        help="Path to input extensions proto",
    )
    parser.add_argument(
        "--output-proto", required=True, help="Path to output proto"
    )
    parser.add_argument(
        "--merge-field-id",
        required=True,
        help="Field ID of the extension to merge back into the main proto",
    )
    parser.add_argument(
        "--merge-message-type",
        required=True,
        help="Name of the message type to merge (e.g. FrameTimelineEvent)",
    )
    parser.add_argument(
        "--merge-field-name",
        required=True,
        help="Name of the field to add in TracePacket (e.g. frame_timeline_event)",
    )

    args = parser.parse_args()

    input_proto_path = args.input_proto
    extensions_proto_path = args.input_extensions
    output_path = args.output_proto
    field_id = args.merge_field_id
    message_type = args.merge_message_type
    field_name = args.merge_field_name

    with open(input_proto_path, "r") as f:
        proto_content = f.read()

    if not os.path.exists(extensions_proto_path):
        print(
            f"Error: Extensions proto path '{extensions_proto_path}' does not exist"
        )
        sys.exit(1)

    # Check if the field is already defined.
    # Exact match (correct type and field ID): safe no-op.
    # Write out the original proto file as the output and exit.
    if re.search(
        rf"\b{message_type}\s+\w+\s*=\s*{field_id}\s*;", proto_content
    ):
        with open(output_path, "w") as f:
            f.write(proto_content)
        sys.exit(0)

    # Check for conflict (field ID is defined but with a different type).
    if re.search(rf"=\s*{field_id}\s*;", proto_content):
        print(
            f"Error: Field ID {field_id} is already defined in the trace proto, "
            f"but not with message type {message_type}"
        )
        sys.exit(1)

    # Check that the field id is being handled as an extension.
    if not re.search(rf"extensions\s+.*?{field_id}.*?;", proto_content):
        print(
            f"Error: Field ID {field_id} is not being handled as an extension."
        )
        sys.exit(1)

    # Merge the extension field definition into main proto content
    try:
        proto_content = merge_extension_field(
            proto_content, message_type, field_name, field_id
        )
    except ValueError as e:
        print(f"Error: {e}")
        sys.exit(1)

    # Then remove it from the extension list.
    try:
        proto_content = remove_field_from_extensions(proto_content, field_id)
    except ValueError as e:
        print(f"Error: {e}")
        sys.exit(1)

    # Read the extensions proto file
    with open(extensions_proto_path, "r") as f:
        ext_content = f.read()

    # Extract the definitions from extensions file
    # We want everything starting from 'message <message_type>' to the end of the file
    match = re.search(
        rf"message {message_type}\s+\\{{.*", ext_content, re.DOTALL
    )
    if not match:
        print(f"Error: 'message {message_type}' not found in extensions proto")
        sys.exit(1)

    message_def = match.group(0)

    # Append to the end of proto_content
    final_content = (
        proto_content
        + "\n\n// Re-added for Fuchsia/Starnix compatibility:\n"
        + message_def
    )

    with open(output_path, "w") as f:
        f.write(final_content)


if __name__ == "__main__":
    main()
