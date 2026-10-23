// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#![no_std]
#![no_main]
// TODO(https://fxbug.dev/539292628): Allow dead code during the initial Rust
// conversion process.
#![allow(dead_code)]
#![allow(clippy::missing_safety_doc)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(clippy::new_without_default)]
#![feature(atomic_volatile)]
#![feature(cfg_sanitize)]
#![feature(sanitize)]

#[allow(unused_imports)]
#[macro_use]
extern crate ktrace_macro;

// Submodules in the kernel crate:

#[path = "arch/src/mod.rs"]
pub mod arch_rs;

#[path = "dev/hw_rng/hw_rng.rs"]
pub mod hw_rng;

#[path = "dev/interrupt/interrupt.rs"]
pub mod dev_interrupt;

#[path = "dev/pdev/hw_watchdog/watchdog.rs"]
pub mod pdev_watchdog;

#[path = "dev/pdev/power/power.rs"]
pub mod pdev_power;

#[path = "hypervisor/mod.rs"]
pub mod hypervisor;

#[path = "kernel/mod.rs"]
pub mod kernel;

#[path = "lib/backtrace/backtrace.rs"]
pub mod backtrace;

#[path = "lib/cbuf/src/mod.rs"]
pub mod cbuf;

#[cfg(console_enabled)]
#[path = "lib/console/mod.rs"]
pub mod console;

#[cfg(not(console_enabled))]
use console_env as _;

#[path = "lib/control-region/control_region.rs"]
pub mod control_region;

#[path = "lib/counters/src/lib.rs"]
pub mod counters;

#[path = "lib/crashlog/crashlog.rs"]
pub mod crashlog;

#[path = "lib/debuglog/debuglog.rs"]
pub mod debuglog_rs;

#[path = "lib/instrumentation/mod.rs"]
pub mod instrumentation;

#[path = "lib/jtrace/jtrace.rs"]
pub mod jtrace;

#[path = "lib/kconcurrent/src/mod.rs"]
pub mod kconcurrent;

#[path = "lib/ktrace/src/mod.rs"]
pub mod ktrace_rs;

#[cfg(with_lock_dep)]
#[path = "lib/lockdep/lock_dep.rs"]
pub mod lockdep_rs;

#[path = "lib/lockup_detector/lockup_detector.rs"]
pub mod lockup_detector;

#[path = "lib/mmu_rcu/src/mod.rs"]
pub mod mmu_rcu;

#[path = "lib/object_cache/src/lib.rs"]
pub mod object_cache;

#[path = "lib/page_cache/page_cache.rs"]
pub mod page_cache;

#[path = "lib/percpu_writer/allocator.rs"]
pub mod percpu_writer;

#[path = "lib/persistent-debuglog/persistent_debuglog.rs"]
pub mod persistent_debuglog_rs;

#[path = "lib/power-management/mod.rs"]
pub mod power_management;

#[path = "lib/root_resource_filter/src/mod.rs"]
pub mod root_resource_filter;

#[path = "lib/stall/src/mod.rs"]
pub mod stall;

#[path = "lib/suspend_wakeup_timer/suspend_wakeup_timer.rs"]
pub mod suspend_wakeup_timer;

#[path = "lib/syscalls/mod.rs"]
pub mod syscalls_rs;

#[path = "lib/topology/src/mod.rs"]
pub mod topology;

#[path = "lib/user_copy/src/mod.rs"]
pub mod user_copy;

#[path = "lib/userabi/userabi.rs"]
pub mod userabi;

#[path = "lib/virtual_alloc/virtual_alloc.rs"]
pub mod virtual_alloc;

#[path = "lib/wake-vector/wake_vector.rs"]
pub mod wake_vector;

#[path = "object/object.rs"]
pub mod object;

#[path = "platform/platform.rs"]
pub mod platform_rs;

#[path = "top/mod.rs"]
pub mod top;

#[path = "top/rust_panic.rs"]
pub mod rust_panic;

#[path = "vm/mod.rs"]
pub mod vm;

// Target/board-specific modules:

#[cfg(target_arch = "x86_64")]
#[path = "platform/pc/mod.rs"]
pub mod platform_pc;

#[cfg(target_arch = "riscv64")]
#[path = "platform/generic-riscv64/src/mod.rs"]
pub mod generic_riscv64;

#[cfg(any(target_arch = "aarch64", target_arch = "riscv64"))]
#[path = "dev/pdev/interrupt/interrupt.rs"]
pub mod pdev_interrupt;

#[cfg(target_arch = "aarch64")]
#[path = "dev/hw_rng/amlogic_rng/amlogic_rng.rs"]
pub mod amlogic_rng;

#[cfg(target_arch = "aarch64")]
#[path = "dev/hw_rng/qcom_rng/qcom_rng.rs"]
pub mod qcom_rng;

#[cfg(target_arch = "x86_64")]
#[path = "dev/hw_rng/intel_rng/intel_rng.rs"]
pub mod intel_rng;

#[cfg(target_arch = "aarch64")]
#[path = "dev/hw_watchdog/generic32/generic32.rs"]
pub mod generic32;

#[cfg(target_arch = "riscv64")]
#[path = "dev/interrupt/plic/plic.rs"]
pub mod plic;

#[cfg(target_arch = "aarch64")]
#[path = "dev/interrupt/gic/common/gic_common.rs"]
pub mod gic_common;

#[cfg(target_arch = "aarch64")]
#[path = "dev/interrupt/gic/v2/arm_gicv2.rs"]
pub mod arm_gicv2;

#[cfg(target_arch = "aarch64")]
#[path = "dev/interrupt/gic/v3/arm_gicv3.rs"]
pub mod arm_gicv3;

#[cfg(target_arch = "aarch64")]
#[path = "dev/power/iris/iris.rs"]
pub mod power_iris;

#[cfg(target_arch = "aarch64")]
#[path = "dev/power/moonflower/moonflower.rs"]
pub mod power_moonflower;

#[cfg(target_arch = "aarch64")]
#[path = "dev/power/motmot/motmot.rs"]
pub mod power_motmot;

#[cfg(target_arch = "aarch64")]
#[path = "dev/timer/armv7_mmio_timer/armv7_mmio_timer.rs"]
pub mod armv7_mmio_timer;

// Test modules:

#[cfg(ktest)]
#[path = "tests/arch_rs_tests.rs"]
pub mod arch_rs_tests;

#[cfg(all(console_enabled, ktest))]
#[path = "lib/boot-options/console.rs"]
pub mod boot_options_cli;

#[cfg(ktest)]
#[path = "lib/console/tests/mod.rs"]
pub mod console_tests_rust;

#[cfg(ktest)]
#[path = "lib/control-region/tests/kernel.rs"]
pub mod control_region_tests;

#[cfg(ktest)]
#[path = "lib/libc/tests/string_file_tests.rs"]
pub mod string_file_tests_rust;

#[cfg(ktest)]
#[path = "lib/pow2_range_allocator/tests/kernel.rs"]
pub mod pow2_range_allocator_tests;

#[cfg(all(console_enabled, ktest))]
#[path = "debugcommands/debugcommands.rs"]
pub mod debugcommands;

#[cfg(all(console_enabled, ktest))]
#[path = "lib/unittest/console.rs"]
pub mod unittest_console;

#[cfg(ktest)]
#[path = "lib/unittest/user_memory.rs"]
pub mod user_memory;

#[cfg(ktest)]
#[path = "lib/user_copy/tests/kernel.rs"]
pub mod user_copy_rs_kernel_tests;

#[cfg(ktest)]
#[path = "vm/unittests/mod.rs"]
pub mod vm_unittests;

#[cfg(ktest)]
#[path = "../../src/lib/ksync/tests/kernel.rs"]
pub mod ksync_kernel_tests;

#[cfg(ktest)]
#[path = "tests/rustlang-support/test_mod.rs"]
pub mod test_mod;

#[cfg(all(console_enabled, ktest))]
#[path = "tests/tests.rs"]
pub mod kernel_tests;

#[cfg(ktest)]
#[path = "tests/unused_rust.rs"]
pub mod unused_rust;

// Force linking of extern crates that provide C entry points:
// Only arch/arm64 depends on coresight today.
#[cfg(target_arch = "aarch64")]
use coresight as _;
// Only platform/pc depends on fixed_point today.
#[cfg(target_arch = "x86_64")]
use fixed_point as _;
use flow_id as _;
use init as _;
use relaxed_atomic as _;
#[cfg(ktest)]
use rust_libc_tests as _;
#[cfg(ktest)]
use trivial_tests as _;
