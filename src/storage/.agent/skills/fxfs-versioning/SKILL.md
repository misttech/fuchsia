---
name: fxfs-versioning
description: Updating Fxfs versioned structures, cascading changes, type fingerprints, and golden images.
---

# Fxfs Versioning

Fxfs manages on-disk format compatibility using a project-wide `LATEST_VERSION`. Use this skill when implementing structural changes or storage format updates.

## Structural Changes (Major Version Bump)

When modifying a versioned structure, follow this standard procedure (the "versioning dance"):

1.  **Bump `LATEST_VERSION`**: Update the `LATEST_VERSION` constant in [types.rs](src/storage/fxfs/src/serialized_types/types.rs).
2.  **Version the Structure**:
    - Copy the existing `FooV(N)` to `FooV(LATEST_VERSION)` (e.g., `FooV53` to `FooV55`).
    - **Clean the Legacy Type**: Remove extraneous traits (`Debug`, `Eq`, `PartialEq`, `TypeFingerprint`, `Versioned`) and documentation comments from the older version (`FooV(N)`).
    - **Visibility**: Remove `pub` from the legacy type's fields and from the type itself in almost all cases. Nothing should use them except for the deserialization `into()` chains (which are defined in the same module) and other legacy version structs.
    - **Methods**: Methods should **always** remain on the unversioned type alias (e.g., `impl Foo { ... }`). Do not leave methods on the legacy struct (e.g., `impl FooV(N) { ... }`) unless a method is explicitly ONLY used for performing the data migration itself.
    - **Follow File Conventions**: If the project convention stores older versions in a separate file (e.g., `legacy.rs` or `old.rs`), move the older structure there to keep the primary source file concise.
    - **Item Aggregations (`ObjectItem` / `Item` / `ItemRef`)**: When versioning aggregations that compose separate structs like `Item<K, V>`, ensure you define the legacy item tuple correctly using the corresponding legacy component types (e.g., `pub(crate) type ObjectItemV50 = Item<ObjectKeyV43, ObjectValueV50>;`).

3.  **Implement Migration**:
    - **Prefer the `#[derive(Migrate)]` macro** over an explicit `impl From` wherever possible. For trivial type changes, it creates significantly less code bloat.
    - The macro requires identical field names and relies on `Into` implementations for child structures. When the macro is insufficient (e.g., fields are dropped or semantics materially altered), fall back to manually defining: `impl From<FooV(N)> for FooV(LATEST_VERSION)`.

4.  **Handle Cascading Updates**:
    - If a parent structure `Bar` contains `Foo`, then `Bar` must also be versioned (e.g., to `BarV55`) to reference the new `FooV55`.
    - Apply this same process recursively up the structure hierarchy.
5.  **Update Type Mappings**:
    - Add the new version mapping to the `versioned_types!` macro in [types.rs](src/storage/fxfs/src/serialized_types/types.rs).
6.  **Verify via Type Fingerprints**:
    - Update the `TypeFingerprint` implementation for the latest version.
    - Create a new type fingerprint test case in `tests.rs` to validate the new version.
    - When bumping the version, you must add a new golden file for type fingerprints (`src/serialized_types/golden/NN.json.golden`).
    - The easiest way to generate this is to run `fx build`. It will fail with a "Golden file mismatch" and output an `fx run-in-build-dir cp ...` command. Run that command to copy the generated `.json.golden` into your source tree, then `git add` it. Alternatively, build with `fx set ... --args update_goldens=true`.
7.  **Generate Golden Images (.zstd)**:
    - Regenerate the on-disk image test suite by running: `fx fxfs create_golden`.
    - This creates a new `fxfs_golden.NN.0.img.zstd` file in `testdata/`. Ensure you `git add` this new untracked file, along with adding it to `testdata/images.gni`.
    - This ensures backwards compatibility and is required for CQ verification.

## Pruning Legacy Versions

Removing old versioned code is a destructive operation that breaks compatibility with older devices.

> [!CAUTION]
> **Breaking Change**: Deleting older versions prevents Fxfs from mounting filesystems created with those versions. This is an infrequent operation that requires official project coordination.

1.  **Update Support Window**: Raise `EARLIEST_SUPPORTED_VERSION` in [types.rs](src/storage/fxfs/src/serialized_types/types.rs).
2.  **Clean Codebase**: Remove legacy struct definitions and their associated `From` conversion traits.
3.  **Refresh Test Assets**:
    - Delete the obsolete `.zstd` golden image in `testdata/`.
    - Delete the obsolete `.json.golden` in `src/serialized_types/golden/`.
    - Remove the corresponding entry from `images.gni`.
4.  **Finalize**: Run `fx fxfs create_golden` to update the remaining images and fingerprints.
