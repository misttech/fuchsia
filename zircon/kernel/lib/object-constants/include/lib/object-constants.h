// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_LIB_OBJECT_CONSTANTS_INCLUDE_LIB_OBJECT_CONSTANTS_H_
#define ZIRCON_KERNEL_LIB_OBJECT_CONSTANTS_INCLUDE_LIB_OBJECT_CONSTANTS_H_

#include <stddef.h>
#include <stdint.h>

// Size and alignment constants for Rust dispatcher states stored in C++ OpaqueStorage.
// These values must match the exact size and alignment of their corresponding Rust structs,
// which is enforced by static_asserts in both Rust and C++.

// Size and alignment for CounterDispatcherState.
constexpr size_t kCounterDispatcherStateSize = 64;
constexpr size_t kCounterDispatcherStateAlign = 8;
constexpr size_t kCounterDispatcherStateOffset = 48;

// Size and alignment for EventDispatcherState.
constexpr size_t kEventDispatcherStateSize = 64;
constexpr size_t kEventDispatcherStateAlign = 8;
constexpr size_t kEventDispatcherStateOffset = 48;

// Size and alignment for EventPairDispatcherState.
constexpr size_t kEventPairDispatcherStateSize = 32;
constexpr size_t kEventPairDispatcherStateAlign = 8;
constexpr size_t kEventPairDispatcherStateOffset = 48;

// Size and alignment for IommuDispatcherState.
constexpr size_t kIommuDispatcherStateSize = 48;
constexpr size_t kIommuDispatcherStateAlign = 8;
constexpr size_t kIommuDispatcherStateOffset = 48;

// Size and alignment for MsiDispatcherState.
constexpr size_t kMsiDispatcherStateSize = 64;
constexpr size_t kMsiDispatcherStateAlign = 8;
constexpr size_t kMsiDispatcherStateOffset = 48;

// Size and alignment for LogDispatcherState.
constexpr size_t kLogDispatcherStateSize = 112;
constexpr size_t kLogDispatcherStateAlign = 8;
constexpr size_t kLogDispatcherStateOffset = 48;

// Size and alignment for DlogReaderStorage (DlogReader).
constexpr size_t kDlogReaderStorageSize = 48;
constexpr size_t kDlogReaderStorageAlign = 8;

// Size and alignment for ProfileDispatcherState.
constexpr size_t kProfileDispatcherStateSize = 96;
constexpr size_t kProfileDispatcherStateAlign = 8;
constexpr size_t kProfileDispatcherStateOffset = 48;

// Size and alignment for SamplerDispatcherState.
constexpr size_t kSamplerDispatcherStateSize = 64;
constexpr size_t kSamplerDispatcherStateAlign = 8;
constexpr size_t kSamplerDispatcherStateOffset = 48;

// Size and alignment for SuspendTokenDispatcherState.
constexpr size_t kSuspendTokenDispatcherStateSize = 64;
constexpr size_t kSuspendTokenDispatcherStateAlign = 8;
constexpr size_t kSuspendTokenDispatcherStateOffset = 48;

// Size and alignment for SchedulerState::BaseProfile.
constexpr size_t kSchedulerStateBaseProfileSize = 32;
constexpr size_t kSchedulerStateBaseProfileAlign = 8;

// Size and alignment for TimerDispatcherState.
constexpr size_t kTimerDispatcherStateSize = 200;
constexpr size_t kTimerDispatcherStateAlign = 8;
constexpr size_t kTimerDispatcherStateOffset = 48;

// Size and alignment for DpcStorage (Dpc).
constexpr size_t kDpcStorageSize = 32;
constexpr size_t kDpcStorageAlign = 8;

#endif  // ZIRCON_KERNEL_LIB_OBJECT_CONSTANTS_INCLUDE_LIB_OBJECT_CONSTANTS_H_
