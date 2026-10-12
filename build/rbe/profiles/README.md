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
- `worker_pools` (object, required): A mapping of abstract worker pool names
  (e.g., `"default"`, `"large"`) to the concrete platform property strings
  (e.g., `"Pool=default"`, `"Pool=large_worker"`) used by the specified
  RBE instance.
- `auth_type` (string, required): The authentication strategy or provider
  to use for this service (e.g., `"google"`, `"none"`, or `"api_key"`).
