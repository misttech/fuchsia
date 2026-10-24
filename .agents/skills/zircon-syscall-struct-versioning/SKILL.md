---
name: zircon-syscall-struct-versioning
description: >
  Rules for changing Zircon syscall ABI structs (struct versioning, also
  called struct evolution). Use when adding, removing, reordering, resizing,
  renaming, or changing the meaning of a field in any struct passed to or
  returned from a Zircon syscall, such as zx_info_*_t structs for
  zx_object_get_info() topics (ZX_INFO_*), zx_clock_*_args_vN_t, or
  zx_policy_basic_vN_t; when editing headers under
  zircon/system/public/zircon/ or sdk/rust/zx-types; or when reviewing such
  changes. Do NOT use for FIDL protocol versioning or @available annotations.
---

# Zircon syscall struct versioning

The source of truth is
[`docs/development/kernel/syscall_struct_versioning.md`][guide]. Read it in full
before you change or review a syscall struct. This skill is a summary and
checklist.

[guide]: /docs/development/kernel/syscall_struct_versioning.md

## Core rule

A struct that crosses the syscall boundary is ABI. Prebuilt binaries have its
size, layout, and field meanings baked in. Never modify it in place, even to
append a field. Instead:

1.  Copy the existing struct verbatim to the next `_vN_t` name and add a `_VN`
    topic or constant with the existing numeric value.
1.  Change the unversioned struct (for example, `zx_info_vmo_t`) to the new
    layout and bump the version in the unversioned topic (for example, with
    `__ZX_INFO_TOPIC(topic, version)`).
1.  Make the kernel serve every supported version.
1.  Remove the old version only after no binaries built against a supported
    API level use it. Never reuse a version number.

The kernel only needs to support supported API levels. The current headers
describe all of them, so don't dig through header history or retired API
levels.

Structs used only by `@next` syscalls (`zircon/syscalls-next.h`) are out of
scope.

Don't add a `version` field to syscall structs. The version is always carried by
the syscall's `topic` or `options` argument. Don't try to "just append" a field
either: it still changes the struct size and breaks arrays of structs.

Versioning propagates upward. If the struct you change is embedded in (or
pointed to by) other syscall structs, every root struct that can reach it needs
a new version, and every struct in between needs a `_vN` copy. Changing an
unversioned struct also changes every struct that embeds it, possibly on
syscalls you didn't mean to touch: version or pin each one. See "Nested and
shared structs" in the doc.

If the syscall has no version channel (topic, options version bits, or similar),
check the doc's "Syscalls without a version channel" section, and then stop and
tell the user: a new syscall or an RFC is probably required.

## Checklist for adding a version

- [ ] C header under `zircon/system/public/zircon/syscalls/`: old struct copied
  byte-for-byte to `_vN_t`, `_VN` constant uses the old numeric value,
  unversioned struct updated, unversioned topic version bumped.
- [ ] Every root that can reach the changed struct (topics, `options`-versioned
  arguments, structs reached through pointers) is identified. Each struct that
  contains the changed struct is either versioned along with its root or pinned
  to the `_vN` struct.
- [ ] Explicit padding fields, fixed-width types, and bit flags rather than
  `bool`.
- [ ] Kernel (for example, `zircon/kernel/lib/syscalls/object_info.rs`) handles
  all versions. Output: build the newest form, then convert. Input: validate
  size per version, translate old to new with defaults, and reject unknown
  versions.
- [ ] `sdk/rust/zx-types/src/lib.rs`: topics, structs, and `const_assert_eq!`
  size and alignment checks for every version.
- [ ] High-level wrappers in `sdk/rust/zx` and `zircon/system/ulib/zx`, if
  affected.
- [ ] Syscall docs in `zircon/vdso/*.fidl` doc comments.
- [ ] `zircon/kernel/tests/abi_type_validator.cc` entries for every version.
- [ ] Decoders that reference the topic: `tools/fidlcat`, `src/lib/fidl_codec`,
  and `src/lib/zxdump`.
- [ ] Core tests in `zircon/system/utest/core/` exercise every version.
- [ ] `zircon/public/sysroot_sdk/sysroot.api` updated if the build asks.
- [ ] In-tree users fixed in the same CL if fields were removed or renamed.
- [ ] CL description notes that `API-Review+1` from an API Council member is
  required.

## Verification

Run `fx build` and `fx core-tests`.
