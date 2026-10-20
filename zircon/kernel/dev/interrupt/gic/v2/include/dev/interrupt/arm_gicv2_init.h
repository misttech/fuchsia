// Copyright 2021 The Fuchsia Authors
// Copyright (c) 2016, Google, Inc. All rights reserved
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_DEV_INTERRUPT_GIC_V2_INCLUDE_DEV_INTERRUPT_ARM_GICV2_INIT_H_
#define ZIRCON_KERNEL_DEV_INTERRUPT_GIC_V2_INCLUDE_DEV_INTERRUPT_ARM_GICV2_INIT_H_

#include <lib/zbi-format/driver-config.h>

#include <kernel/ffi.h>

// The driver itself is implemented in Rust (arm_gicv2.rs), which cannot export the overloaded
// `ArmGicInit*` names that <platform/generic-arm/dev-init.cc> resolves via ktl::visit(). The
// overloads below are thin forwarders to the uniquely named entry points the Rust code exports.
extern "C" {
void rust_arm_gicv2_init_early(const zbi_dcfg_arm_gic_v2_driver_t& config);
void rust_arm_gicv2_init_post_vm(const zbi_dcfg_arm_gic_v2_driver_t& config);
void rust_arm_gicv2_init_late(const zbi_dcfg_arm_gic_v2_driver_t& config);
}

// Keep the layout of the configuration struct shared with Rust in sync; the Rust side asserts
// the same values via zr::static_assert!.
static_assert(sizeof(zbi_dcfg_arm_gic_v2_driver_t) == 56);
static_assert(alignof(zbi_dcfg_arm_gic_v2_driver_t) == 8);

// Early initialization routines for the driver.
FFI_ALWAYS_INLINE inline void ArmGicInitEarly(const zbi_dcfg_arm_gic_v2_driver_t& config) {
  rust_arm_gicv2_init_early(config);
}

// Post VM initialization step, able to use the VM to map registers.
FFI_ALWAYS_INLINE inline void ArmGicInitPostVm(const zbi_dcfg_arm_gic_v2_driver_t& config) {
  rust_arm_gicv2_init_post_vm(config);
}

// Any post threading initialization.
FFI_ALWAYS_INLINE inline void ArmGicInitLate(const zbi_dcfg_arm_gic_v2_driver_t& config) {
  rust_arm_gicv2_init_late(config);
}

#endif  // ZIRCON_KERNEL_DEV_INTERRUPT_GIC_V2_INCLUDE_DEV_INTERRUPT_ARM_GICV2_INIT_H_
