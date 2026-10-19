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

#if defined(WITH_LOCK_DEP) || \
    (defined(LOCK_DEP_ENABLED_FEATURE_LEVEL) && LOCK_DEP_ENABLED_FEATURE_LEVEL >= 1)
constexpr size_t kLockClassIdSize = 8;
#else
constexpr size_t kLockClassIdSize = 0;
#endif

// Size, alignment, and offset for BusTransactionInitiatorDispatcherState.
constexpr size_t kBusTransactionInitiatorDispatcherStateSize = 56 + kLockClassIdSize;
constexpr size_t kBusTransactionInitiatorDispatcherStateAlign = 8;
constexpr size_t kBusTransactionInitiatorDispatcherStateOffset = 48;

// Size, alignment, and offset for ClockDispatcherState.
constexpr size_t kClockDispatcherStateSize = 232;
constexpr size_t kClockDispatcherStateAlign = 8;
constexpr size_t kClockDispatcherStateOffset = 48;

// Size and alignment for ClockDispatcher.
constexpr size_t kClockTransformationStorageSize = 112;
constexpr size_t kClockTransformationStorageAlign = 8;

// Size, alignment, and offset for ChannelDispatcherState.
constexpr size_t kChannelDispatcherStateSize = 112;
constexpr size_t kChannelDispatcherStateAlign = 8;
constexpr size_t kChannelDispatcherStateOffset = 48;

// Size and alignment for MessageWaiter.
constexpr size_t kMessageWaiterSize = 136;
constexpr size_t kMessageWaiterAlign = 8;

// Size, alignment, and offset for CounterDispatcherState.
constexpr size_t kCounterDispatcherStateSize = 64;
constexpr size_t kCounterDispatcherStateAlign = 8;
constexpr size_t kCounterDispatcherStateOffset = 48;

// Size, alignment, and offset for EventDispatcherState.
constexpr size_t kEventDispatcherStateSize = 64;
constexpr size_t kEventDispatcherStateAlign = 8;
constexpr size_t kEventDispatcherStateOffset = 48;

// Size, alignment, and offset for EventPairDispatcherState.
constexpr size_t kEventPairDispatcherStateSize = 32;
constexpr size_t kEventPairDispatcherStateAlign = 8;
constexpr size_t kEventPairDispatcherStateOffset = 48;

// Size, alignment, and offset for ExceptionDispatcherState.
constexpr size_t kExceptionDispatcherStateSize = 160 + kLockClassIdSize;
constexpr size_t kExceptionDispatcherStateAlign = 8;
constexpr size_t kExceptionDispatcherStateOffset = 48;

// Size, alignment, and offset for FifoDispatcherState.
constexpr size_t kFifoDispatcherStateSize = 72;
constexpr size_t kFifoDispatcherStateAlign = 8;
constexpr size_t kFifoDispatcherStateOffset = 48;

// Size, alignment, and offset for GuestDispatcherState.
constexpr size_t kGuestDispatcherStateSize = 48;
constexpr size_t kGuestDispatcherStateAlign = 8;
constexpr size_t kGuestDispatcherStateOffset = 48;

// Size, alignment, and offset for VcpuDispatcherState.
constexpr size_t kVcpuDispatcherStateSize = 56 + kLockClassIdSize;
constexpr size_t kVcpuDispatcherStateAlign = 8;
constexpr size_t kVcpuDispatcherStateOffset = 48;

// Size, alignment, and offset for IoBufferDispatcherState.
constexpr size_t kIoBufferDispatcherStateSize = 64;
constexpr size_t kIoBufferDispatcherStateAlign = 8;
constexpr size_t kIoBufferDispatcherStateOffset = 56;

// Size, alignment, and offset for IoBufferSharedRegionDispatcherState.
constexpr size_t kIoBufferSharedRegionDispatcherStateSize = 88;
constexpr size_t kIoBufferSharedRegionDispatcherStateAlign = 8;
constexpr size_t kIoBufferSharedRegionDispatcherStateOffset = 48;

// Size, alignment, and offset for IommuDispatcherState.
constexpr size_t kIommuDispatcherStateSize = 48 + kLockClassIdSize;
constexpr size_t kIommuDispatcherStateAlign = 8;
constexpr size_t kIommuDispatcherStateOffset = 48;

// Size, alignment, and offset for MemoryStallEventDispatcherState. Unlike the others this state
// sits inside a subclass, after the EventDispatcher state its base class holds.
constexpr size_t kMemoryStallEventDispatcherStateSize = 8;
constexpr size_t kMemoryStallEventDispatcherStateAlign = 4;
constexpr size_t kMemoryStallEventDispatcherStateOffset =
    kEventDispatcherStateOffset + kEventDispatcherStateSize;

// Size, alignment, and offset for MsiDispatcherState.
constexpr size_t kMsiDispatcherStateSize = 64;
constexpr size_t kMsiDispatcherStateAlign = 8;
constexpr size_t kMsiDispatcherStateOffset = 48;

// Size, alignment, and offset for LogDispatcherState.
constexpr size_t kLogDispatcherStateSize = 112;
constexpr size_t kLogDispatcherStateAlign = 8;
constexpr size_t kLogDispatcherStateOffset = 48;

// Size and alignment for DlogReaderStorage (DlogReader).
constexpr size_t kDlogReaderStorageSize = 48;
constexpr size_t kDlogReaderStorageAlign = 8;

// Size, alignment, and offset for PagerDispatcherState.
constexpr size_t kPagerDispatcherStateSize = 120 + (2 * kLockClassIdSize);
constexpr size_t kPagerDispatcherStateAlign = 8;
constexpr size_t kPagerDispatcherStateOffset = 48;
constexpr uint32_t kPagerProxyTrapDirty = (1u << 0u);

// Size, alignment, and offset for PinnedMemoryTokenDispatcherState.
constexpr size_t kPinnedMemoryTokenDispatcherStateSize = 56 + kLockClassIdSize;
constexpr size_t kPinnedMemoryTokenDispatcherStateAlign = 8;
constexpr size_t kPinnedMemoryTokenDispatcherStateOffset = 48;

// Size, alignment, and offset for ProfileDispatcherState.
constexpr size_t kProfileDispatcherStateSize = 96 + kLockClassIdSize;
constexpr size_t kProfileDispatcherStateAlign = 8;
constexpr size_t kProfileDispatcherStateOffset = 48;

// Size, alignment, and offset for SamplerDispatcherState.
constexpr size_t kSamplerDispatcherStateSize = 64;
constexpr size_t kSamplerDispatcherStateAlign = 8;
constexpr size_t kSamplerDispatcherStateOffset = 48;

// Size, alignment, and offset for SuspendTokenDispatcherState.
constexpr size_t kSuspendTokenDispatcherStateSize = 64;
constexpr size_t kSuspendTokenDispatcherStateAlign = 8;
constexpr size_t kSuspendTokenDispatcherStateOffset = 48;

// Size, alignment, and offset for SocketDispatcherState.
constexpr size_t kSocketDispatcherStateSize = 88;
constexpr size_t kSocketDispatcherStateAlign = 8;
constexpr size_t kSocketDispatcherStateOffset = 48;

// Size and alignment for StreamDispatcherState.
constexpr size_t kStreamDispatcherStateSize = 112;
constexpr size_t kStreamDispatcherStateAlign = 8;
constexpr size_t kStreamDispatcherStateOffset = 48;

// Size and alignment for SchedulerState::BaseProfile.
constexpr size_t kSchedulerStateBaseProfileSize = 32;
constexpr size_t kSchedulerStateBaseProfileAlign = 8;

// Size, alignment, and offset for TimerDispatcherState.
constexpr size_t kTimerDispatcherStateSize = 200;
constexpr size_t kTimerDispatcherStateAlign = 8;
constexpr size_t kTimerDispatcherStateOffset = 48;

// Size and alignment for DpcStorage (Dpc).
constexpr size_t kDpcStorageSize = 32;
constexpr size_t kDpcStorageAlign = 8;

// Size and alignment for WaitSignalObserver.
constexpr size_t kWaitSignalObserverSize = 72;
constexpr size_t kWaitSignalObserverAlign = 8;

// Size and alignment for RootJobSignalObserver.
constexpr size_t kRootJobSignalObserverSize = 48;
constexpr size_t kRootJobSignalObserverAlign = 8;

// Size and alignment for RootJobObserver (Rust state).
constexpr size_t kRootJobObserverStorageSize = 80;
constexpr size_t kRootJobObserverStorageAlign = 8;

// Size, alignment, and offset for WaitSignalObserverState.
constexpr size_t kWaitSignalObserverStorageSize = 32;
constexpr size_t kWaitSignalObserverStorageAlign = 8;
constexpr size_t kWaitSignalObserverStorageOffset = 40;

// Size, alignment, and offset for ResourceDispatcherState.
constexpr size_t kResourceDispatcherStateSize = 128 + kLockClassIdSize;
constexpr size_t kResourceDispatcherStateAlign = 8;
constexpr size_t kResourceDispatcherStateOffset = 48;

// Size, alignment, and offset for VmAddressRegionDispatcherState.
constexpr size_t kVmAddressRegionDispatcherStateSize = 56 + kLockClassIdSize;
constexpr size_t kVmAddressRegionDispatcherStateAlign = 8;
constexpr size_t kVmAddressRegionDispatcherStateOffset = 48;

// Size and alignment for MemoryWatchdogState.
constexpr size_t kMemoryWatchdogStateSize = 304;
constexpr size_t kMemoryWatchdogStateAlign = 8;

// Size, alignment, and offset for VmObjectDispatcherState.
constexpr size_t kVmObjectDispatcherStateSize = 64 + kLockClassIdSize;
constexpr size_t kVmObjectDispatcherStateAlign = 8;
constexpr size_t kVmObjectDispatcherStateOffset = 56;

// Size, alignment, and offset for InterruptDispatcherState.
constexpr size_t kInterruptDispatcherStateSize = 296 + (2 * kLockClassIdSize);
constexpr size_t kInterruptDispatcherStateAlign = 8;
constexpr size_t kInterruptDispatcherStateOffset = 56;

// Size and alignment for PortInterruptPacket.
constexpr size_t kPortInterruptPacketSize = 32;
constexpr size_t kPortInterruptPacketAlign = 8;

// Size and alignment for FutexContext (Rust state).
constexpr size_t kFutexContextStorageSize = 56;
constexpr size_t kFutexContextStorageAlign = 8;

// Size, alignment, and offset for ThreadDispatcherState.
//
// The offset is large because most of ThreadDispatcher's fields are still C++ and precede
// opaque_storage_; it shrinks as those fields migrate into the Rust state.
//
// It is also configuration dependent, unlike every other dispatcher here, whose state sits
// immediately after the Dispatcher base. Three lockdep-instrumented locks live in the C++
// fields that precede opaque_storage_ - runtime_stats_'s SeqLock, exceptionate_'s mutex, and
// core_thread_lock_ - and each grows by one lock class id when lockdep is enabled, moving
// opaque_storage_ later by 3 * kLockClassIdSize bytes. The base offset is taken from the
// compiler, not computed by hand.
constexpr size_t kThreadDispatcherStateSize = 64;
constexpr size_t kThreadDispatcherStateAlign = 8;
constexpr size_t kThreadDispatcherStateOffset = 416 + (3 * kLockClassIdSize);

// Size and alignment for Backtrace.
constexpr size_t kBacktraceSize = 272;
constexpr size_t kBacktraceAlign = 8;

// Size and alignment for HandleTableStorage (HandleTable).
#if defined(__riscv)
#if defined(WITH_LOCK_DEP)
constexpr size_t kHandleTableStorageSize = 120;
#else
constexpr size_t kHandleTableStorageSize = 112;
#endif
constexpr size_t kHandleTableStorageAlign = 8;
#else  // __x86_64__ or __aarch64__
#if defined(WITH_LOCK_DEP)
constexpr size_t kHandleTableStorageSize = 192;
#else
constexpr size_t kHandleTableStorageSize = 176;
#endif
constexpr size_t kHandleTableStorageAlign = 16;
#endif

#endif  // ZIRCON_KERNEL_LIB_OBJECT_CONSTANTS_INCLUDE_LIB_OBJECT_CONSTANTS_H_
