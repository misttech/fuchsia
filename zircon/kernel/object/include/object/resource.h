// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_RESOURCE_H_
#define ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_RESOURCE_H_

#include <zircon/compiler.h>
#include <zircon/syscalls/resource.h>
#include <zircon/types.h>

#include <fbl/ref_ptr.h>

// When validating MMIO resource ranges against a non-root parent MMIO resource,
// we need to verify that the requested range is completely contained by its
// parent's range.  There are two different ways that we can do this: a strict
// validation or a non-strict validation.
//
// A strict validation means any time we validate a range against an MMIO
// resource object, the MMIO resource's range *must* completely contain *all* of
// the requested range, without exception.
//
// A non-strict validation means that we check to make sure that the requested
// range is completely contained by the parent's _page aligned_ range.  Because
// of some legacy behavior, when creating a physical VMO using an MMIO resource
// as a token, user-mode drivers (PCIe in particular) depend on the kernel
// allowing the operation provided that the MMIO resource's range simply touches
// all of the pages covered by the MMIO resource.  IOW - if an MMIO resource
// covers the range `[X + 0x80, X + 0x100)`, user-mode expects this resource to
// allow it to create a physical VMO covering `[X, X + kPageSize)`.
//
// By default, we use non-strict validation.  However, when we attempt to create
// an MMIO resource as a child of another MMIO resource, we demand strict
// validation. Failure to do this means that someone could create an MMIO
// resource whose range is larger than that of its parents, which is more than a
// little bit confusing.
//
// TODO(b/506251014): Remove this when we get to the point where we can expect
// user-mode to always page-align all of its MMIO resource ranges.
//
enum class StrictMmioRangeValidation { No, Yes };

// Resource constants (ZX_RSRC_KIND_..., etc) are located
// in system/public/zircon/syscalls/resource.h

// Determines if this handle is to a resource of the specified
// kind *or* to the root resource, which can stand in for any kind.
// Used to provide access to privileged syscalls.
zx_status_t validate_resource(zx_handle_t handle, zx_rsrc_kind_t kind);

// Determines if this handle is to a resource of the specified base and kind
// or to the root resource.
zx_status_t validate_resource_kind_base(zx_handle_t handle, zx_rsrc_kind_t kind,
                                        zx_rsrc_system_base_t base);

// Validates a resource based on type and low/high range.
class ResourceDispatcher;
zx_status_t validate_ranged_resource(
    fbl::RefPtr<ResourceDispatcher> resource, zx_rsrc_kind_t kind, uint64_t base, size_t len,
    StrictMmioRangeValidation strict_validation = StrictMmioRangeValidation::No);
zx_status_t validate_ranged_resource(
    zx_handle_t handle, zx_rsrc_kind_t kind, uint64_t base, size_t len,
    StrictMmioRangeValidation strict_validation = StrictMmioRangeValidation::No);

// Validates enabling ioport access bits for a given process based on a resource handle
static inline zx_status_t validate_resource_ioport(zx_handle_t handle, uint64_t base, size_t len) {
  return validate_ranged_resource(handle, ZX_RSRC_KIND_IOPORT, base, len);
}

// Validates mapping an MMIO range based on a resource handle
static inline zx_status_t validate_resource_mmio(
    zx_handle_t handle, uint64_t base, size_t len,
    StrictMmioRangeValidation strict_validation = StrictMmioRangeValidation::No) {
  return validate_ranged_resource(handle, ZX_RSRC_KIND_MMIO, base, len, strict_validation);
}

// Validates creation of an interrupt object based on a resource handle
static inline zx_status_t validate_resource_irq(zx_handle_t handle, uint32_t irq) {
  return validate_ranged_resource(handle, ZX_RSRC_KIND_IRQ, irq, 1);
}

// Validates access to a SMC service call number based on a resource handle
static inline zx_status_t validate_resource_smc(zx_handle_t handle, uint64_t service_call_num) {
  return validate_ranged_resource(handle, ZX_RSRC_KIND_SMC, service_call_num, 1);
}

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_RESOURCE_H_
