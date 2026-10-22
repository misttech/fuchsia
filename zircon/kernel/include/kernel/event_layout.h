// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_INCLUDE_KERNEL_EVENT_LAYOUT_H_
#define ZIRCON_KERNEL_INCLUDE_KERNEL_EVENT_LAYOUT_H_

#include <stddef.h>
#include <stdint.h>

#include <kernel/event.h>

// Layout contract between the C++ Event and its Rust mirror in //zircon/kernel/kernel/event.rs.
// bindgen turns this into the event_bindings crate, where Rust asserts its own layout and
// constants against it at compile time.
//
// Event embeds a WaitQueue, which is not a standard-layout class, so offsetof on it is only
// conditionally supported; in our compiler and ABI the layout is deterministic, hence the
// -Winvalid-offsetof suppression.
#pragma GCC diagnostic push
#pragma GCC diagnostic ignored "-Winvalid-offsetof"

struct EventLayout {
  static_assert(sizeof(ktl::atomic<zx_status_t>) == sizeof(int32_t));
  static_assert(alignof(ktl::atomic<zx_status_t>) == alignof(int32_t));
  static_assert(sizeof(Event::Flags) == sizeof(uint32_t));

  static constexpr size_t kSize = sizeof(Event);
  static constexpr size_t kAlign = alignof(Event);
  static constexpr size_t kAutounsignalSize = sizeof(AutounsignalEvent);
  static constexpr size_t kAutounsignalAlign = alignof(AutounsignalEvent);

  static constexpr size_t kMagicOffset = offsetof(Event, magic_);
  static constexpr size_t kResultOffset = offsetof(Event, result_);
  static constexpr size_t kFlagsOffset = offsetof(Event, flags_);
  static constexpr size_t kWaitOffset = offsetof(Event, wait_);

  static constexpr uint32_t kMagic = Event::kMagic;
  static constexpr int32_t kNotSignaled = Event::kNotSignaled;
  static constexpr uint32_t kFlagsNone = Event::Flags::NONE;
  static constexpr uint32_t kFlagsAutounsignal = Event::Flags::AUTOUNSIGNAL;
};

#pragma GCC diagnostic pop

#endif  // ZIRCON_KERNEL_INCLUDE_KERNEL_EVENT_LAYOUT_H_
