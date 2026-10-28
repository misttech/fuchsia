// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_LIB_PERCPU_WRITER_INCLUDE_LIB_PERCPU_WRITER_BUFFER_TYPES_H_
#define ZIRCON_KERNEL_LIB_PERCPU_WRITER_INCLUDE_LIB_PERCPU_WRITER_BUFFER_TYPES_H_

#include <stdint.h>
#include <zircon/time.h>

namespace percpu_writer {

/// This struct keeps track of the duration, number, and size of trace records dropped when the
/// buffer is full. These statistics are emitted to the trace buffer as a duration as soon as
/// space is available to do so, at which point the values are reset to 0, or false in the case
/// of has_dropped.
struct DroppedRecordStats {
  zx_instant_boot_ticks_t first_dropped{0};
  zx_instant_boot_ticks_t last_dropped{0};

  /// By storing num_dropped and bytes_dropped in 32-bit values, we ensure that they can each
  /// be stored in a single 64-bit word in the FXT record we emit when space is available.
  uint32_t num_dropped{0};
  uint32_t bytes_dropped{0};
  bool has_dropped{false};

  void Reset() {
    first_dropped = 0;
    last_dropped = 0;
    num_dropped = 0;
    bytes_dropped = 0;
    has_dropped = false;
  }

  void Track(zx_instant_boot_ticks_t now, uint32_t size) {
    if (!has_dropped) {
      first_dropped = now;
      has_dropped = true;
    }
    last_dropped = now;
    num_dropped++;
    bytes_dropped += size;
  }

  bool HasDropped() const { return has_dropped; }
};
static_assert(sizeof(DroppedRecordStats) == 32);

/// This is the structure of the FXT duration event that will store dropped record metadata in
/// the trace buffer. Normally, we would use the FXT serialization functions to build this record
/// dynamically, but we cannot do this in the PerCpuBuffer because those functions invoke
/// writer->Reserve, which would lead to recursion. To avoid this, we serialize the record
/// manually using this struct, which in turn is set up to match the structure outlined in the
/// FXT spec: https://fuchsia.dev/fuchsia-src/reference/tracing/trace-format#event-record
///
/// Eventually, it would be nice to have the FXT serialization library support in-place
/// serialization, as that would allow us to remove this bespoke functionality.
struct DroppedRecordDurationEvent {
  uint64_t header;
  zx_instant_boot_ticks_t start;
  uint64_t process_id;
  uint64_t thread_id;
  uint64_t num_dropped_arg;
  uint64_t bytes_dropped_arg;
  zx_instant_boot_ticks_t end;
};
static_assert(sizeof(DroppedRecordDurationEvent) == 56);

}  // namespace percpu_writer

#endif  // ZIRCON_KERNEL_LIB_PERCPU_WRITER_INCLUDE_LIB_PERCPU_WRITER_BUFFER_TYPES_H_
