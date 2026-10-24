# Idk and zx library attribute alignment parity

- **Learned from:** inner loop friction on a local task (task `zircon_system_ulib_mmio-ptr`)
- **Date:** 2026-10-23
- **Changed:** `checks/gn_attr_parity.sh`, `checks/manifest.json`

## Root cause

When migrating GN zx_library and sdk_* targets to Bazel idk_cc_* and
cc_*_library_zx rules, review_diff (diffengine) flags mandatory and renamed
Bazel IDK attributes (category mapped from sdk_publishable, mandatory idk_name,
api_area, api_file_path, and includes) as added in Bazel and flags GN template
attributes encoded in the Bazel rule name or hdrs list (sdk, sdk_headers,
sdk_headers_for_internal_use) as not carried over to Bazel. Because
gn_attr_parity only emitted non-blocking INFO attribute_alignment entries for
build_flags and rustc_library_with_features expansions, review_diff emitted
false-positive blocking WARNING findings on valid IDK and zx_library migrations.

## Why not a one-off fix

Hardcoding an exception for a single library target would leave every future
zx_library, sdk_source_set, sdk_static_library, sdk_shared_library, and IDK
host-tool migration across the repository vulnerable to identical false-positive
review_diff attribute_alignment warnings across randomized Go map iteration
orders.
