// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_ROOT_JOB_OBSERVER_H_
#define ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_ROOT_JOB_OBSERVER_H_

#include <lib/object-constants.h>

#include <fbl/ref_ptr.h>
#include <kernel/ffi.h>
#include <ktl/array.h>
#include <object/process_dispatcher.h>

class RootJobObserver final {
 public:
  ~RootJobObserver();

  RootJobObserver(const RootJobObserver&) = delete;
  RootJobObserver& operator=(const RootJobObserver&) = delete;
  RootJobObserver(RootJobObserver&&) = delete;
  RootJobObserver& operator=(RootJobObserver&&) = delete;

  // Record that any critical process is in some stage of being torn down.
  static void SetCriticalProcessDying();
  static bool GetCriticalProcessDying();
  // Record the dead process responsible for getting the root job killed.
  static void CriticalProcessKill(fbl::RefPtr<ProcessDispatcher> dead_process);

 private:
  OpaqueStorage<kRootJobObserverStorageSize, kRootJobObserverStorageAlign> opaque_storage_;
};

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_ROOT_JOB_OBSERVER_H_
