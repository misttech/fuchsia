// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef ZIRCON_KERNEL_LIB_CONTROL_REGION_INCLUDE_LIB_CONTROL_REGION_H_
#define ZIRCON_KERNEL_LIB_CONTROL_REGION_INCLUDE_LIB_CONTROL_REGION_H_

#include <stddef.h>

#include <kernel/ffi.h>
#include <kernel/spinlock.h>

extern "C" {
void rust_control_region_init(void* storage);
void rust_control_region_destroy(void* storage);
void rust_control_region_register(void* storage);
void rust_control_region_unregister(void* storage);
bool rust_control_region_try_enter(void* storage);
void rust_control_region_close_enter(void* storage);
bool rust_control_region_try_leave(void* storage);
void rust_control_region_open_leave(void* storage);
}

// ControlRegion is a CPU synchronization tool.
//
// Think of it as a region of a program that is protected by a gate.  When the gate is open, CPUs
// may freely enter or leave the controlled region.  The gate may only be closed when all CPUs are
// within the controlled region.  The last CPU to enter the region is responsible for closing the
// gate.  CPUs may not leave the region until the gate is opened by one of them.
//
// Usage:
//
//   // An instance shared by all CPUs.
//   ControlRegion region;
//
//   // A managed resource of some kind, shared by all CPUs.
//   Resource resource;
//
//   ...
//
//   // Called once for each CPU before any other use of the region.
//   // Be sure to |Unregister| when taking a CPU offline.
//   region.Register();
//
//   ...
//
//   resource.Use();
//
//   if (!region.TryEnter()) {
//     // We are the last one in, shutdown the resource before closing and entering.
//     resource.Shutdown();
//     region.CloseEnter();
//   }
//
//   // Start of the region.
//
//   // End of the region.
//
//   if (!region.TryLeave()) {
//     // We are the first leave, we must restore the resource before opening and leaving.
//     resource.Init();
//     region.OpenLeave();
//   }
//
//  resource.Use();
//
class ControlRegion {
 public:
  ControlRegion() {
    static_assert(alignof(ControlRegion) == kStorageAlign);
    rust_control_region_init(&storage_);
  }
  ~ControlRegion() { rust_control_region_destroy(&storage_); }

  ControlRegion(const ControlRegion&) = delete;
  ControlRegion& operator=(const ControlRegion&) = delete;
  ControlRegion(ControlRegion&&) = delete;
  ControlRegion& operator=(ControlRegion&&) = delete;

  // Registers a CPU for participation in a control region.
  //
  // This should be called once for each CPU that may participate.
  void Register() { rust_control_region_register(&storage_); }

  // Unregisters a CPU for participation in a control region.
  //
  // This method should be called before offlining a CPU that had been registered.
  void Unregister() { rust_control_region_unregister(&storage_); }

  // Attempt to enter the region.
  //
  // Returns true if this CPU has successfully entered and isn't the last one in.
  //
  // Returns false and sets the state to closing if this CPU is the only remaining CPU outside the
  // region.  This CPU is then obligated to first perform any resource management tasks associated
  // with this region, and then finish closing the gate and entering by calling |CloseEnter|.
  //
  // It is an error to call this method with interrupts enabled.
  [[nodiscard]] bool TryEnter() { return rust_control_region_try_enter(&storage_); }

  // Close the gate and enter the region.  This is intended to be called after a call to |TryEnter|
  // has returned false and the caller has performed any necessary resource management tasks.
  //
  // It is an error to call this method with interrupts enabled.
  void CloseEnter() { rust_control_region_close_enter(&storage_); }

  // Attempt to leave the region.
  //
  // Returns true if this CPU has successfully left the region and there is already at least one CPU
  // outside the region.
  //
  // Returns false and sets the state to opening if no other CPUs are outside the region.  This CPU
  // is then obligated to then perform any resource management tasks associated with the region, and
  // then finish opening the gate and leaving by calling |OpenLeave|.
  //
  // It is an error to call this method with interrupts enabled.
  //
  // Note: If another CPU is in the process of opening or closing the gate, this method may spin
  // (busy-wait) until that other CPU has finished.
  bool TryLeave() { return rust_control_region_try_leave(&storage_); }

  // Open the gate and leave the region.  This is intended to be called after a call to |TryLeave|
  // has returned false and the caller has performed any necessary resource management tasks.
  //
  // It is an error to call this method with interrupts enabled.
  void OpenLeave() { rust_control_region_open_leave(&storage_); }

 private:
  using LockType = DECLARE_SPINLOCK(ControlRegion);
  static constexpr size_t kLockSize = (sizeof(LockType) + 7) & ~size_t{7};
  static constexpr size_t kStorageSize = kLockSize + 8;
  static constexpr size_t kStorageAlign = 8;

  OpaqueStorage<kStorageSize, kStorageAlign> storage_{};
};

#endif  // ZIRCON_KERNEL_LIB_CONTROL_REGION_INCLUDE_LIB_CONTROL_REGION_H_
