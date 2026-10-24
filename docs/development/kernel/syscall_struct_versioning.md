# Versioning Zircon syscall structs

This guide explains how to change a struct that crosses the Zircon syscall
boundary without breaking existing binaries. This process has historically been
called *struct versioning* or *struct evolution*. The intended audience is
kernel developers and anyone (human or agent) modifying the Zircon system
interface.

If you are about to add, remove, reorder, resize, rename, or change the meaning
of a field in any struct declared under
[`//zircon/system/public/zircon/`][public-headers] (or mirrored in
[`//sdk/rust/zx-types`][zx-types]), read this guide first.

Structs that are used only by syscalls annotated with `@next` (declared in
[`zircon/syscalls-next.h`][syscalls-next-h]) are out of scope for this guide.
Those syscalls are not part of the stable ABI, so their structs follow different
rules.

## Why structs can't be changed in place

The syscall ABI is a contract between the kernel and every binary built against
a [supported API level][rfc-0239], including prebuilt binaries that are not
rebuilt alongside the kernel (for example, SDK consumers, prebuilt packages,
and CTF tests). Those binaries have the size, layout, and meaning of each
struct baked in.

The kernel only needs to support binaries built against a supported API level.
Like the rest of the platform, the kernel can drop support for an API level
once that level is retired. This has two consequences:

* When an API level is retired, the kernel can stop supporting struct versions
  that only binaries targeting that level use. See
  [Removing an old version](#removing-an-old-version).
* The current headers describe every supported API level, using the API level
  macros in [`availability.h`][availability-h] (such as `ZX_AVAILABLE_SINCE`
  and `FUCHSIA_API_LEVEL_AT_LEAST`) where needed. You don't need to dig through
  the history of the headers or consider retired API levels to find out which
  struct versions the kernel must support.

For more about API levels and the compatibility guarantees that apply to them,
see [RFC-0239: Platform versioning in practice][rfc-0239] and the
[Fuchsia platform API evolution guidelines][api-evolution].

Any of these changes silently break an existing binary:

* **Adding a field, even at the end.** For example, for `zx_object_get_info()`,
  the kernel copies `sizeof(struct)` bytes and returns
  `ZX_ERR_BUFFER_TOO_SMALL` when the caller's buffer is smaller. An old binary
  that passes a buffer sized for the old struct starts failing.
* **Removing, reordering, or resizing a field.** Old binaries read or write
  the wrong bytes.
* **Changing the meaning or units of an existing field.** The layout is
  unchanged, but old binaries misinterpret the value.

Renaming a field without changing its layout or meaning is ABI-compatible but
breaks source compatibility. See [Source compatibility](#source-compatibility).

## Design rationale

The versioning scheme is guided by these principles:

* **Keep callers simple.** The kernel does the work of supporting multiple
  versions. Callers don't need a library to encode, decode, or translate
  structs (as they would with protobuf-style encodings). Every supported
  language can use the plain C struct layouts through its native FFI.
* **Preserve performance.** If a single syscall gets or sets a group of
  related values today, a new version must still allow that in a single
  syscall.
* **Allow soft transitions.** An interface evolves in steps: add the new
  version alongside the old, migrate callers, and then remove the old version.

### Alternatives considered

* **Only append new fields to the end of a struct.** This doesn't work when
  the syscall reads or writes an array of structs (for example,
  `ZX_INFO_PROCESS_MAPS` or `ZX_INFO_PROCESS_VMOS`), because the element
  stride changes. It also forces new fields into a position that may waste
  space on alignment padding. With versioned structs, each new version can
  place fields where they logically belong.
* **Add a `version` field to each struct.** This is awkward for arrays of
  structs and requires more kernel work for output structs. Because nearly
  all syscalls that take or return structs already have a `topic` or `options`
  argument, the version is encoded there instead. Don't add version fields to
  syscall structs.

## The versioning scheme

Instead of modifying a struct in place, you introduce a new version of the
struct and keep serving the old version until no binaries depend on it:

1. Snapshot the existing struct under a versioned name (for example,
   `zx_info_vmo_v3_t`) and keep supporting it in the kernel.
1. Make the unversioned name (`zx_info_vmo_t`) refer to the new layout.
1. Once all callers have been recompiled against the new version and no binary
   built against a supported API level uses the old one, delete the old
   version.

This scheme only works when the caller can tell the kernel which version it is
using. Each syscall that supports versioning has a *version channel* in its
arguments:

| Syscall(s) | Version channel | Encoding | Example |
| --- | --- | --- | --- |
| `zx_object_get_info()` | `topic` argument | Upper 4 bits of the topic, using `__ZX_INFO_TOPIC(topic, version)` in [`object.h`][object-h] | `ZX_INFO_VMO_V3` = `__ZX_INFO_TOPIC(23u, 2)`, `ZX_INFO_VMO` = `__ZX_INFO_TOPIC(23u, 3)` |
| `zx_clock_create()`, `zx_clock_get_details()`, `zx_clock_update()` | `options` argument | `ZX_CLOCK_ARGS_VERSION(n)` in [`clock.h`][clock-h] (6 bits at bit 58) | `zx_clock_update_args_v1_t` and `zx_clock_update_args_v2_t` |
| `zx_job_set_policy()` | `topic` argument | Distinct topic constants in [`policy.h`][policy-h] | `ZX_JOB_POL_BASIC_V1` and `ZX_JOB_POL_BASIC_V2` |

Note: Version numbers in the encoding are zero-based for `__ZX_INFO_TOPIC`,
while struct and constant names are one-based. For example,
`ZX_INFO_VMO_V1` is `__ZX_INFO_TOPIC(23u, 0)`.

The `__ZX_INFO_TOPIC` encoding has 4 bits of version, so a single
`zx_object_get_info()` topic can have at most 16 versions over its lifetime.

### Syscalls without a version channel

A few syscalls take or return structs but have no version channel:

* **Syscalls whose structs are considered final.** For example,
  `zx_object_wait_many()` takes an array of `zx_wait_item_t`. The struct
  already carries everything needed to wait on signals, so a change would
  imply a different operation (such as waiting on something other than
  signals). In that case, add a new syscall.
* **Syscalls whose struct format is determined by another syscall.** For
  example, `zx_port_queue()` and `zx_port_wait()` have no version channel, but
  they exchange `zx_port_packet_t`, a tagged union. New packet types of the
  same size can be added to the union, and the `options` argument of
  `zx_port_create()` could select a different packet format for a port.

Otherwise, if the syscall that consumes or produces the struct has no topic,
options bits, or other way for the caller to select a version, you cannot use
this scheme. Your options are to add a new syscall or to add a version channel
in a new syscall and migrate callers to it. Either option is a change to the
Zircon system interface; see [Process requirements](#process-requirements).

When you design a new syscall that takes or returns a struct, include a version
channel (such as reserved version bits in `options`) from the start so that the
struct can evolve later. The [system API rubric][system-rubric] requires this.

## Naming conventions

Follow the existing convention in [`object.h`][object-h]:

* The **unversioned** names (`zx_info_vmo_t`, `ZX_INFO_VMO`) always refer to the
  **newest** version. Most code should use these.
* **Older** versions get a `_vN` suffix on the struct (`zx_info_vmo_v1_t`,
  `zx_info_vmo_v2_t`, ...) and a `_VN` suffix on the topic or constant
  (`ZX_INFO_VMO_V1`, ...).
* Nested structs that change are versioned together with their parent (for
  example, `zx_info_maps_v2_t` contains `zx_info_maps_mapping_v2_t`). See
  [Nested and shared structs](#nested-and-shared-structs).
* Version numbers are never reused. After an old version is removed, the
  unversioned topic keeps its version bits. For example, `ZX_INFO_PROCESS`
  is still `__ZX_INFO_TOPIC(3u, 1u)` even though `ZX_INFO_PROCESS_V1` was
  removed.

## Nested and shared structs {#nested-and-shared-structs}

A version number identifies everything the kernel reads or writes for that
call, not just the outermost struct. Think of the struct selected by the
version channel as the *root*. The ABI covered by the root's version includes:

* Every struct, array, and union the root embeds by value, recursively.
* Any data the kernel reads or writes through pointers in the root.

So when you change a struct, the version bump propagates **upward** to every
root that can reach it:

* **Embedded by value:** If a nested struct changes size, the layout of every
  struct that contains it changes, as does the stride of any array of those
  structs. Even if the size doesn't change, old binaries misread a field whose
  meaning changed.
* **Reached through a pointer:** The outer struct's layout doesn't change, but
  old binaries still pass or expect the old format of the pointed-to data. The
  kernel can only tell the formats apart by the root's version, so the root
  needs a new version.

Because the kernel keeps serving the old root, every struct between the
changed struct and the root needs an old copy too. For example, when
`zx_arm64_exc_data_t` grew, the old layouts were preserved as a chain of
versioned structs served under `ZX_INFO_THREAD_EXCEPTION_REPORT_V1`:

```c
typedef struct zx_exception_report_v1 {
    zx_exception_header_t header;
    zx_exception_context_v1_t context;   // contains zx_arm64_exc_data_v1_t
} zx_exception_report_v1_t;
```

Versioning propagates only upward, toward roots. Changing an outer struct
doesn't require you to version the structs it contains.

### Shared structs

Since the unversioned name always refers to the newest version, changing an
unversioned struct changes **every** struct that embeds it, including structs
used by syscalls you didn't intend to change. If any of those syscalls don't
get a version bump, the result is a silent ABI break.

For example, `zx_exception_context_t` reaches the kernel through several paths:

* Inside `zx_exception_report_t`, returned by `zx_object_get_info()` with
  `ZX_INFO_THREAD_EXCEPTION_REPORT`.
* As the `context` argument of `zx_thread_raise_exception()`.
* Inside `zx_restricted_exception_t` (through `zx_exception_report_t`), which
  restricted mode writes without any version channel. This struct is
  currently exempt only because it is declared in the unstable
  `zircon/syscalls-next.h`.

Before you change a struct, find every root it can be reached from: each
`zx_object_get_info()` topic, each argument versioned through `options`, and
each pointer the kernel follows. For each struct that contains the changed
struct, do one of the following:

* **Version it as well**, along with its root. This requires the root to have
  a version channel.
* **Pin it** to the old version by changing it to embed the `_vN` struct
  explicitly, so its layout is unchanged.

If any affected root has no version channel, see
[Syscalls without a version channel](#syscalls-without-a-version-channel).

## Struct layout rules

New struct versions must follow the general rules for syscall structs:

* Use fixed-width types (`uint32_t`, `uint64_t`, `zx_koid_t`, etc.).
* Make all padding explicit with named fields such as `uint8_t padding1[4];`.
  Don't rely on implicit compiler padding.
* Prefer bit flags in a `uint32_t` or `uint64_t` over `bool` fields.
* Document every field, including whether it is valid only in some cases.

## Step-by-step: adding a new version

The following steps describe the usual single-CL approach used by recent
changes, such as the change that introduced the current `ZX_INFO_KMEM_STATS`
(RFC-0262) and the `zx_info_vmo_t`, `zx_info_maps_t`, and
`zx_info_task_stats_t` versions from RFC-0254.

### 1. Update the C headers

In the appropriate header under
[`//zircon/system/public/zircon/syscalls/`][syscalls-headers]:

1. Copy the current struct definition verbatim to a new struct with the next
   `_vN` name (for example, `zx_info_kmem_stats_v1_t`). The layout must be
   byte-for-byte identical to the struct that exists today.
1. Add a `_VN` topic or constant for that old struct with the **existing
   numeric value** of the unversioned topic. Old binaries keep sending this
   value.
1. Modify the unversioned struct (`zx_info_kmem_stats_t`) to the new layout.
1. Bump the version bits of the unversioned topic (for example, from
   `__ZX_INFO_TOPIC(17u, 0)` to `__ZX_INFO_TOPIC(17u, 1)`).
1. If the struct you changed is embedded in, or pointed to by, other syscall
   structs, version or pin each of them and their roots as described in
   [Nested and shared structs](#nested-and-shared-structs).

For example, this is how `ZX_INFO_KMEM_STATS` was versioned:

```c
-#define ZX_INFO_KMEM_STATS     ((zx_object_info_topic_t) 17u) // zx_info_kmem_stats_t[1]
+#define ZX_INFO_KMEM_STATS     __ZX_INFO_TOPIC(17u, 1)        // zx_info_kmem_stats_t[1]
+#define ZX_INFO_KMEM_STATS_V1  __ZX_INFO_TOPIC(17u, 0)        // zx_info_kmem_stats_v1_t[1]
```

### 2. Update the kernel

The kernel must accept every supported version.

* **Output structs** (for example, `zx_object_get_info()` topics): Gather the
  information once in the newest internal form, then convert it to the
  requested version before you copy it out. See the handling of
  `ZX_INFO_VMO_V1`, `ZX_INFO_VMO_V2`, and `ZX_INFO_VMO_V3` in
  [`object_info.rs`][object-info-rs] and the `vmo_info_to_version` and
  `MapsInfoVersion` helpers in [`memory_stats.rs`][memory-stats-rs].
* **Input structs** (for example, `zx_clock_update()` arguments or
  `zx_job_set_policy()` policies): Decode the version from the version channel,
  validate the buffer size for *that* version, and translate older versions to
  the internal representation with well-defined defaults for new fields.
  Reject unknown versions with `ZX_ERR_INVALID_ARGS`.

### 3. Update the bindings, documentation, and validators

Search the tree for the existing topic and struct names and update every place
that mirrors them. This usually includes:

* [`//sdk/rust/zx-types/src/lib.rs`][zx-types]: Add the `_VN` topic and the
  `_vN_t` struct, update the unversioned struct, and keep the
  `const_assert_eq!` size and alignment checks accurate for every version.
* [`//sdk/rust/zx`][zx-rust] and [`//zircon/system/ulib/zx`][zx-cpp]:
  Update high-level wrappers that expose the struct.
* [`//zircon/vdso/*.fidl`][vdso]: Update the syscall reference documentation
  in the doc comments, including documentation for the older versions.
* [`//zircon/kernel/tests/abi_type_validator.cc`][abi-validator]: Add size,
  alignment, and field offset checks for the new struct and keep the checks
  for the old versions.
* Tools that decode syscall structs, if they reference the topic, such as
  [`//tools/fidlcat`][fidlcat], [`//src/lib/fidl_codec`][fidl-codec], and
  [`//src/lib/zxdump`][zxdump].
* [`//zircon/public/sysroot_sdk/sysroot.api`][sysroot-api]: The build fails
  with instructions to update this file when public sysroot headers change.

### 4. Add tests

Add or update Zircon core tests (for example, under
[`//zircon/system/utest/core/`][core-tests]) that exercise **every** supported
version, not just the newest one. For example, see how
[`vmo.cc`][vmo-test] queries `ZX_INFO_VMO_V1`, `ZX_INFO_VMO_V2`,
`ZX_INFO_VMO_V3`, and `ZX_INFO_VMO`, and checks that the shared fields agree.

Run the core tests with `fx core-tests` and build with `fx build`.

## Source compatibility

The scheme above preserves ABI compatibility for all existing binaries. Source
compatibility depends on the kind of change:

* **Only adding fields:** Code that uses the unversioned names picks up the new
  version when recompiled, and usually continues to compile. Code that relies
  on the exact size or on aggregate initialization of every field might need
  updates.
* **Removing or renaming fields:** This breaks source compatibility for code
  that uses those fields. Update all in-tree users in the same CL. Out-of-tree
  users can pin to the old `_vN` names until they migrate.

If a source-breaking change affects a lot of out-of-tree code, use the
alias-based soft transition described in RFC-0074 instead:

1. Introduce both `_V1` and `_V2`, with the unversioned names temporarily
   aliased to `_V1`.
1. Migrate all callers to `_V2`.
1. Repoint the unversioned aliases to `_V2`.
1. Migrate callers back to the unversioned names.
1. Remove `_V1`.

## Removing an old version

You can remove an old version only after no binary built against a supported
API level can still use it. Binaries that target a retired API level don't need
to be considered. Before you remove it, verify the following:

* No in-tree code references the `_vN_t` struct or `_VN` constant.
* No prebuilt binaries, SDK consumers at a supported API level, or CTF tests
  depend on it. If in doubt, ask the Zircon kernel OWNERS.

The removal CL should delete the old struct and constant from the C headers,
the kernel handling, the Rust bindings, the ABI validator entries, the tools'
decoders, the vDSO documentation, and the tests for that version. For an
example, see the CL that removed `ZX_INFO_PROCESS_V1` (commit `934bcf504d3`).

Don't renumber the remaining versions, and don't reuse the removed version
number.

## Process requirements

* Changes to syscall structs are changes to the Zircon system interface and
  require `API-Review+1` from an [API Council][api-council] member.
* Adding or removing Zircon system interfaces, or significantly changing their
  semantics, might require an RFC. See
  [RFC-0006: Addendum to the RFC process for Zircon][rfc-0006].
* Changing the meaning of an existing field in all versions is an exception
  that requires explicit justification and API review, as done in RFC-0254.

## Prior art

These RFCs used this scheme:

* [RFC-0074: Expose process start time][rfc-0074] (`zx_info_process_v2_t`)
* [RFC-0084: More per-task runtime information][rfc-0084]
  (`zx_info_task_runtime_v1_t`)
* [RFC-0254: Changing attribution for copy-on-write pages][rfc-0254]
  (`zx_info_vmo_t`, `zx_info_maps_t`, `zx_info_task_stats_t`)
* [RFC-0262: Expand kmem stats][rfc-0262] (`zx_info_kmem_stats_v1_t`)

For the mechanics of adding a brand new syscall, see
[Adding a new syscall][adding-syscall].

[abi-validator]: /zircon/kernel/tests/abi_type_validator.cc
[adding-syscall]: /docs/development/kernel/adding_a_new_syscall.md
[api-council]: /docs/contribute/governance/api_council.md
[api-evolution]: /docs/development/api/evolution.md
[availability-h]: /zircon/system/public/zircon/availability.h
[clock-h]: /zircon/system/public/zircon/syscalls/clock.h
[core-tests]: /zircon/system/utest/core/
[fidl-codec]: /src/lib/fidl_codec/
[fidlcat]: /tools/fidlcat/
[memory-stats-rs]: /zircon/kernel/vm/memory_stats.rs
[object-h]: /zircon/system/public/zircon/syscalls/object.h
[object-info-rs]: /zircon/kernel/lib/syscalls/object_info.rs
[policy-h]: /zircon/system/public/zircon/syscalls/policy.h
[public-headers]: /zircon/system/public/zircon/
[rfc-0006]: /docs/contribute/governance/rfcs/0006_addendum_to_rfc_process_for_zircon.md
[rfc-0074]: /docs/contribute/governance/rfcs/0074_expose_process_start_time.md
[rfc-0084]: /docs/contribute/governance/rfcs/0084_zircon_more_per_task_runtime_information.md
[rfc-0239]: /docs/contribute/governance/rfcs/0239_platform_versioning_in_practice.md
[rfc-0254]: /docs/contribute/governance/rfcs/0254_changing_attribution_for_copy_on_write_pages.md
[rfc-0262]: /docs/contribute/governance/rfcs/0262_expand_kmem_stats.md
[syscalls-headers]: /zircon/system/public/zircon/syscalls/
[syscalls-next-h]: /zircon/system/public/zircon/syscalls-next.h
[system-rubric]: /docs/development/api/system.md#versioning-structs
[sysroot-api]: /zircon/public/sysroot_sdk/sysroot.api
[vdso]: /zircon/vdso/
[vmo-test]: /zircon/system/utest/core/vmo/vmo.cc
[zx-cpp]: /zircon/system/ulib/zx/
[zx-rust]: /sdk/rust/zx/
[zx-types]: /sdk/rust/zx-types/src/lib.rs
[zxdump]: /src/lib/zxdump/
