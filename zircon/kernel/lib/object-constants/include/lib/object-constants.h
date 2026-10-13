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

// Size and alignment for ChainLock.
//
// A ChainLock is a simple pair of atomics (the lock token and the contention
// state bitmask).  It is the sole member of ChainLockable, which in turn is the
// first base of every chain lockable type in the kernel, so these constants also
// describe the leading bytes of Thread, WaitQueueBase, WaitQueue, and
// OwnedWaitQueue.
constexpr size_t kChainLockSize = 16;
constexpr size_t kChainLockAlign = 8;

// Size and alignment for WaitQueueCollection.
constexpr size_t kWaitQueueCollectionSize = 32;
constexpr size_t kWaitQueueCollectionAlign = 8;

// Size and alignment for WaitQueue (and its WaitQueueBase base class, which has
// the same layout; WaitQueue declares no members of its own).
constexpr size_t kWaitQueueSize = 56;
constexpr size_t kWaitQueueAlign = 8;

// Size and alignment for OwnedWaitQueue.
constexpr size_t kOwnedWaitQueueSize = 88;
constexpr size_t kOwnedWaitQueueAlign = 8;

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

#endif  // ZIRCON_KERNEL_LIB_OBJECT_CONSTANTS_INCLUDE_LIB_OBJECT_CONSTANTS_H_
