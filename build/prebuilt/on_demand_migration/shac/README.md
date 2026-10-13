# Direct Prebuilt SHAC Allowlist (`fuchsia.git`)

This directory stores baseline allowlists enforced by SHAC presubmit checks
(`scripts/shac/prebuilts.star`) during the on-demand CIPD prebuilt migration
(`b/566304584`, `b/567197573`).

## Files

* **`direct_prebuilt_allowlist.json`** -- One-way presubmit ratchet mapping
  repository-relative file paths in `fuchsia.git` to the maximum number of
  direct `//prebuilt` references currently allowed in each file.

## File Format

Validated against [`../schemas/direct_prebuilt_allowlist.schema.json`][schema].

`direct_prebuilt_allowlist.json` is a sorted JSON object formatted with 4-space
indentation (`fx format-code` / `jsonfmt` style):

```json
{
    "build/bazel/bazel_sdk/BUILD.gn": 1,
    "src/developer/ffx/build/ffx_action.gni": 1
}
```

* **Keys:** Repository-relative file paths inside `fuchsia.git`.
* **Values:** Integer count of legacy direct prebuilt accesses
  (`//prebuilt/...`, relative `prebuilt/...`, or `${PREBUILT_...}`) present
  outside `cipd_prebuilt*()` rules or devshell metadata headers
  (`#### EXECUTABLE=` / `#### PREBUILTS=`).
* **Ratchet behavior:**
  * Adding new unmigrated `//prebuilt` references beyond a file's recorded count
    fails `check_no_new_direct_prebuilts`.
  * New files not present in `direct_prebuilt_allowlist.json` have an implicit
    allowance of `0`.

## Updating the Allowlist

After migrating targets or scripts away from raw `//prebuilt` paths (or when
ratcheting down stale baseline counts), regenerate the allowlists across both
`fuchsia.git` and `vendor/google`:

```bash
./build/prebuilt/on_demand_migration/scripts/ratchet/ratchet.py \
  --update-allowlists
```

Verify the SHAC check locally:

```bash
fx host-tool shac check --only check_no_new_direct_prebuilts --all
```

[schema]: ../schemas/direct_prebuilt_allowlist.schema.json
