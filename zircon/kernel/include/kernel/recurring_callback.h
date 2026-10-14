// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_INCLUDE_KERNEL_RECURRING_CALLBACK_H_
#define ZIRCON_KERNEL_INCLUDE_KERNEL_RECURRING_CALLBACK_H_

#include <zircon/compiler.h>

#include <fbl/macros.h>
#include <kernel/ffi.h>

class RecurringCallback;

__BEGIN_CDECLS

void rust_recurring_callback_init(RecurringCallback* cb, void (*callback)());
void rust_recurring_callback_destroy(RecurringCallback* cb);
void rust_recurring_callback_toggle(RecurringCallback* cb);

__END_CDECLS

class RecurringCallback {
 public:
  using CallbackFunc = void (*)();

  explicit RecurringCallback(CallbackFunc callback) {
    rust_recurring_callback_init(this, callback);
  }
  ~RecurringCallback() { rust_recurring_callback_destroy(this); }

  void Toggle() {
    static_assert(sizeof(RecurringCallback) == kStorageSize);
    static_assert(alignof(RecurringCallback) == kStorageAlign);
    rust_recurring_callback_toggle(this);
  }

 private:
  DISALLOW_COPY_ASSIGN_AND_MOVE(RecurringCallback);

#if SCHEDULER_LOCK_SPIN_TRACING_ENABLED
  static constexpr size_t kStorageSize = 112;
#elifdef WITH_LOCK_DEP
  static constexpr size_t kStorageSize = 104;
#else
  static constexpr size_t kStorageSize = 96;
#endif
  static constexpr size_t kStorageAlign = 8;

  [[maybe_unused]] OpaqueStorage<kStorageSize, kStorageAlign> storage_{};
};

#endif  // ZIRCON_KERNEL_INCLUDE_KERNEL_RECURRING_CALLBACK_H_
