<!--
Copyright 2026 The Fuchsia Authors. All rights reserved.
Use of this source code is governed by a BSD-style license that can be
found in the LICENSE file.
-->

# ResultStore Profiles

This directory contains ResultStore/BES (Build Event Service) profile
configuration files in JSON format. Developers can list and select
these profiles via the `fx resultstore profile` CLI subcommand.

## JSON Schema

Each profile JSON file supports the following fields:

- `rs_instance` (string, required): The target ResultStore instance URI
  (e.g., `projects/<project-id>/instances/<instance-name>`).
- `cas_instance` (string, required): The target CAS (Content Addressable
  Storage) instance URI. Typically configured to match the companion
  RBE profile's instance name.
- `results_url` (string, required): The base web URL used to view build
  results on Sponge/ResultStore (e.g., `http://go/fxbtx`).
- `auth_type` (string, required): The authentication strategy or provider
  to use for this service (e.g., `"google"`, `"none"`, or `"api_key"`).
