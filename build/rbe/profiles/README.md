<!--
Copyright 2026 The Fuchsia Authors. All rights reserved.
Use of this source code is governed by a BSD-style license that can be
found in the LICENSE file.
-->

# RBE Profiles

This directory contains RBE (Remote Build Execution) profile
configuration files in JSON format. Developers can list and
select these profiles via the `fx rbe profile` CLI subcommand.

## JSON Schema

Each profile JSON file supports the following fields:

- `rbe_instance` (string, required): The target RBE instance URI
  (e.g., `projects/<project-id>/instances/<instance-name>`).
- `remote_exec_root` (string, required): The root path of the execution
  workspace on remote workers (typically `/b/f/w`).
- `container_image` (string, required): The fully-qualified Docker container image
  URI for remote execution environments (e.g., `docker://gcr.io/...`).
- `os_family` (string, required): The operating system family
  of the remote workers (e.g., `"Linux"`).
- `worker_pools` (object, required): A mapping of abstract worker pool names
  (e.g., `"default"`, `"large"`) to concrete remote execution platform
  property strings formatted as `"key=value"` (e.g., `"Pool=default"`,
  `"Pool=large_worker"`). These platform properties are forwarded to
  reclient/rewrapper as `--platform=key=value` and parsed by Bazel into
  `exec_properties = {"key": "value"}`. Both the property key (e.g., `"Pool"`)
  and property value are derived directly from this string.
- `auth_type` (string, required): The authentication strategy or provider
  to use for this service (e.g., `"google"`, `"none"`, or `"api_key"`).
