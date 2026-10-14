// Copyright 2024 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT
#ifndef ZIRCON_KERNEL_LIB_WAKE_VECTOR_INCLUDE_LIB_WAKE_VECTOR_H_
#define ZIRCON_KERNEL_LIB_WAKE_VECTOR_INCLUDE_LIB_WAKE_VECTOR_H_

#include <lib/user_copy/user_ptr.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <zircon/compiler.h>
#include <zircon/syscalls-next.h>
#include <zircon/syscalls/system.h>
#include <zircon/types.h>

#include <kernel/ffi.h>
#include <ktl/array.h>
#include <ktl/type_traits.h>
#include <platform/timer.h>

namespace wake_vector {

// Forward declaration.
class WakeEvent;

// WakeVector is an interface implemented by objects that will generate system wake events using the
// WakeEvent type. This interface provides diagnostic information about the wake vector to the
// suspend subsystem.
class WakeVector {
 public:
  // This constructor verifies that the derived class has a WakeEvent member at compile time to help
  // avoid misuse. A derived class must pass a pointer-to-member to its WakeEvent member. This
  // constructor does not touch the contents of the WakeEvent instance, which most likely is
  // uninitialized at this point.
  //
  // Example:
  //
  // MyWakeVector::MyWakeVector() : WakeVector{&MyWakeVector::wake_event_}, wake_event_{*this} {}
  //
  template <typename Class>
  explicit WakeVector(WakeEvent Class::* wake_event_member) {
    static_assert(ktl::is_base_of_v<WakeVector, Class>);
  }
  virtual ~WakeVector() = default;

  // Diagnostic information about the wake vector managed by the implementor of this interface.
  struct Diagnostics {
    // Indicates that the given wake vector is enabled and can generate wake events. Disabled wake
    // vectors are not listed in diagnostic logs.
    bool enabled = false;

    // The koid of the object implementing this interface, if any.
    zx_koid_t koid = ZX_KOID_INVALID;

    // Extra information specific to the wake vector that can aid in determining the source of the
    // wake event and potentially its state.
    ktl::array<char, ZX_MAX_NAME_LEN> extra{};

    // Utility to write into the extra field printf style.
    int PrintExtra(const char* format, ...) __PRINTFLIKE(2, 3) {
      va_list ap;
      va_start(ap, format);
      const int err = vsnprintf(extra.data(), extra.size(), format, ap);
      va_end(ap);
      return err;
    }
  };

  // Provides diagnostic information about the wake vector object implementing this interface.
  virtual void GetDiagnostics(Diagnostics& diagnostics_out) const = 0;
};

// The result of a request to wake up the system.
enum class WakeResult : uint8_t {
  // The system was not suspended at the time of the event.
  Active = 0,

  // The system was suspended at the time of the wake event.
  Resumed = 1,

  // The system was in the process of suspending at the time of the wake event.
  SuspendAborted = 2,

  // An already pending wake event was triggered again.
  BadState = 3,
};

enum class AckBehavior : uint8_t { ClearSignaled = 0, RemainSignaled = 1 };

}  // namespace wake_vector

extern "C" {
void cpp_wake_vector_get_diagnostics(const wake_vector::WakeVector* wake_vector,
                                     wake_vector::WakeVector::Diagnostics* diagnostics_out);

bool rust_wake_event_has_pending_wake_events();
void rust_wake_event_new(wake_vector::WakeEvent* event, const wake_vector::WakeVector* wake_vector);
void rust_wake_event_drop(wake_vector::WakeEvent* event);
void rust_wake_event_initialize(wake_vector::WakeEvent* event);
void rust_wake_event_destroy(wake_vector::WakeEvent* event);
wake_vector::WakeResult rust_wake_event_trigger(wake_vector::WakeEvent* event,
                                                zx_instant_boot_t trigger_time);
void rust_wake_event_acknowledge(wake_vector::WakeEvent* event,
                                 wake_vector::AckBehavior ack_behavior);
wake_vector::WakeResult rust_wake_event_strobe(wake_vector::WakeEvent* event,
                                               zx_instant_boot_t trigger_time);
void rust_wake_event_dump(FILE* f, zx_instant_boot_t log_triggered_after_boot_time);
zx_status_t rust_wake_event_generate_wake_event_report(zx_instant_boot_t suspend_start_time,
                                                       zx_wake_source_report_header_t* out_header,
                                                       zx_wake_source_report_entry_t* out_entries,
                                                       uint32_t num_entries,
                                                       uint32_t* actual_entries);
void rust_wake_event_discard_wake_event_report();
}

namespace wake_vector {

// WakeEvent manages the lifecycle of wake events triggered by wake vectors.
class WakeEvent {
 public:
  using AckBehavior = wake_vector::AckBehavior;

  static bool has_pending_wake_events() { return rust_wake_event_has_pending_wake_events(); }

  // Construct a WakeEvent referencing the given wake_vector.
  explicit WakeEvent(const WakeVector& wake_vector) { rust_wake_event_new(this, &wake_vector); }

  ~WakeEvent() { rust_wake_event_drop(this); }

  WakeEvent(const WakeEvent&) = delete;
  WakeEvent& operator=(const WakeEvent&) = delete;
  WakeEvent(WakeEvent&&) = delete;
  WakeEvent& operator=(WakeEvent&&) = delete;

  void Initialize() { rust_wake_event_initialize(this); }
  void Destroy() { rust_wake_event_destroy(this); }

  WakeResult Trigger(zx_instant_boot_t trigger_time) {
    return rust_wake_event_trigger(this, trigger_time);
  }

  void Acknowledge(AckBehavior ack_behavior) { rust_wake_event_acknowledge(this, ack_behavior); }

  WakeResult Strobe(zx_instant_boot_t trigger_time = current_boot_time()) {
    return rust_wake_event_strobe(this, trigger_time);
  }

  static void Dump(FILE* f, zx_instant_boot_t log_triggered_after_boot_time = ZX_TIME_INFINITE) {
    rust_wake_event_dump(f, log_triggered_after_boot_time);
  }

  static zx_status_t GenerateWakeEventReport(
      zx_instant_boot_t suspend_start_time, user_out_ptr<zx_wake_source_report_header_t> out_header,
      user_out_ptr<zx_wake_source_report_entry_t> out_entries, uint32_t num_entries,
      user_out_ptr<uint32_t> actual_entries) {
    return rust_wake_event_generate_wake_event_report(
        suspend_start_time, out_header.get(), out_entries.get(), num_entries, actual_entries.get());
  }

  static void DiscardWakeEventReport() { rust_wake_event_discard_wake_event_report(); }

 private:
  [[maybe_unused]] OpaqueStorage<112, 8> storage_{};
};

static_assert(sizeof(WakeVector::Diagnostics) == 48);
static_assert(alignof(WakeVector::Diagnostics) == 8);
static_assert(offsetof(WakeVector::Diagnostics, enabled) == 0);
static_assert(offsetof(WakeVector::Diagnostics, koid) == 8);
static_assert(offsetof(WakeVector::Diagnostics, extra) == 16);
static_assert(sizeof(WakeEvent) == 112);
static_assert(alignof(WakeEvent) == 8);

}  // namespace wake_vector

#endif  // ZIRCON_KERNEL_LIB_WAKE_VECTOR_INCLUDE_LIB_WAKE_VECTOR_H_
