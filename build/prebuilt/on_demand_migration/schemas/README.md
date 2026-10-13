# JSON Schemas for On-Demand Prebuilt Migration

Human-readable JSON Schema (Draft-07) definitions documenting the structure of
all machine-generated artifacts under `//build/prebuilt/on_demand_migration/`.

## Schemas

* **`direct_prebuilt_allowlist.schema.json`** -- Schema for
  `shac/direct_prebuilt_allowlist.json` (in both `fuchsia.git` and
  `vendor/google`), enforced by the `check_no_new_direct_prebuilts` SHAC check.

## Build-Time Validation

All checked-in JSON artifacts are automatically validated against their
corresponding `.schema.json` definitions via Fuchsia's native `validate_json()`
GN template (`//build/json/validate_json.gni`) when building tests:

```bash
fx build //build/prebuilt:tests
```
