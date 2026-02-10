// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_LIB_PERCPU_WRITER_INCLUDE_LIB_PERCPU_WRITER_BUFFER_H_
#define ZIRCON_KERNEL_LIB_PERCPU_WRITER_INCLUDE_LIB_PERCPU_WRITER_BUFFER_H_

#include <lib/fxt/interned_string.h>
#include <lib/fxt/record_types.h>
#include <lib/fxt/serializer.h>
#include <lib/fxt/string_ref.h>
#include <lib/fxt/thread_ref.h>
#include <lib/spsc_buffer/spsc_buffer.h>

#include <ktl/optional.h>

#include "kernel_aspace_allocator.h"
#include "platform/timer.h"

namespace percpu_writer {
using fxt::operator""_intern;

// percpu_writer::Buffer wraps an SpscBuffer and adds functionality to track dropped trace records.
class Buffer {
 public:
  using Reservation = SpscBuffer<KernelAspaceAllocator, const char*>::Reservation;

  // Initializes the underlying SpscBuffer and metadata.
  zx_status_t Init(uint32_t size, const char* buffer_name,
                   fxt::ThreadRef<fxt::RefType::kInline> assigned_cpu_ref) {
    // Allocate the KOIDs used to annotate CPU trace records.
    cpu_ref_ = assigned_cpu_ref;
    return buffer_.Init(size, buffer_name);
  }

  // Drains the underlying SpscBuffer.
  void Drain() { buffer_.Drain(); }

  // Reads from the underlying SpscBuffer.
  template <CopyOutFunction CopyFunc>
  zx::result<uint32_t> Read(CopyFunc copy_fn, uint32_t len) {
    return buffer_.Read(copy_fn, len);
  }

  // We interpose ourselves in the Reserve path to ensure that we can emit a record containing
  // the dropped records statistics if we need to.
  zx::result<Reservation> Reserve(uint32_t size) {
    // If first_dropped_ is set to a value, then we are currently tracking a run of dropped trace
    // records, so we need to emit a duration record containing that information. We could emit
    // this record independently (with its own call to SpscBuffer::Reserve), but this could lead
    // to situations in which we thrash and emit multiple DroppedRecordDurationEvents in a row.
    // To avoid this, we attempt to reserve the desired size plus the size needed to store the
    // DroppedRecordDurationEvent record, and then write the statistics into the first part of
    // the reservation before returning it.
    uint32_t total_size = size;
    if (first_dropped_.has_value()) {
      DEBUG_ASSERT(last_dropped_.has_value());
      total_size += sizeof(DroppedRecordDurationEvent);
    }

    // Pass the Reserve call on to the SpscBuffer.
    zx::result<Reservation> res = buffer_.Reserve(total_size);
    if (res.is_error()) {
      // If the reservation failed, then we did not have enough space in this buffer, and the
      // record we were attempting to write will be dropped. Add the "size" to the dropped record
      // statistics. Notably, we do not add the "total_size," because that may include the size
      // of the DroppedRecordDurationEvent.
      TrackDroppedRecord(size);
      return res.take_error();
    }

    // If we need to write a dropped record duration event, do that here.
    DEBUG_ASSERT(first_dropped_.has_value() || total_size == size);
    if (first_dropped_.has_value()) {
      DroppedRecordDurationEvent record = SerializeDropStats();
      res->Write(ktl::span<ktl::byte>(reinterpret_cast<ktl::byte*>(&record), sizeof(record)));
      // We've successfully written out the dropped record stats, so reset them for the next run.
      ResetDropStats();
    }
    return res;
  }

  // Emit the dropped record stats to the trace buffer.
  // If we're not tracking a run of dropped records, this is a no-op.
  zx_status_t EmitDropStats() {
    if (!first_dropped_.has_value()) {
      DEBUG_ASSERT(!last_dropped_.has_value());
      return ZX_OK;
    }

    // Try to reserve a slot for the duration record. This will fail if there still isn't enough
    // space in buffer to store the statistics.
    zx::result<Reservation> res = buffer_.Reserve(sizeof(DroppedRecordDurationEvent));
    if (res.is_error()) {
      return res.status_value();
    }
    DroppedRecordDurationEvent record = SerializeDropStats();

    ktl::span bytes =
        ktl::span<const ktl::byte>(reinterpret_cast<const ktl::byte*>(&record), sizeof(record));
    res->Write(bytes);
    res->Commit();

    // We've successfully emitted a record containing statistics on the last run of dropped
    // records. To prepare for the next run, we must reset the stats.
    ResetDropStats();
    return ZX_OK;
  }

  // Resets the dropped records statistics to their initial values.
  // This is used to clear the stats after they've been emitted to a trace buffer.
  void ResetDropStats() {
    first_dropped_ = ktl::nullopt;
    last_dropped_ = ktl::nullopt;
    num_dropped_ = 0;
    bytes_dropped_ = 0;
  }

  // This is the structure of the FXT duration event that will store dropped record metadata in
  // the trace buffer. Normally, we would use the FXT serialization functions to build this record
  // dynamically, but we cannot do this in the PerCpuBuffer because those functions invoke
  // writer->Reserve, which would lead to recursion. To avoid this, we serialize the record
  // manually using this struct, which in turn is set up to match the structure outlined in the
  // FXT spec: https://fuchsia.dev/fuchsia-src/reference/tracing/trace-format#event-record
  //
  // Eventually, it would be nice to have the FXT serialization library support in-place
  // serialization, as that would allow us to remove this bespoke functionality.
  struct DroppedRecordDurationEvent {
    uint64_t header;
    zx_instant_boot_ticks_t start;
    uint64_t process_id;
    uint64_t thread_id;
    uint64_t num_dropped_arg;
    uint64_t bytes_dropped_arg;
    zx_instant_boot_ticks_t end;
  };
  static_assert(std::is_standard_layout_v<DroppedRecordDurationEvent>);

  // Serializes the dropped record statistics into a DroppedRecordDurationEvent.
  DroppedRecordDurationEvent SerializeDropStats() {
    // This method should only be called if we are currently tracking a run of dropped records.
    DEBUG_ASSERT(first_dropped_.has_value());
    DEBUG_ASSERT(last_dropped_.has_value());

    constexpr fxt::WordSize record_size =
        fxt::WordSize::FromBytes(sizeof(DroppedRecordDurationEvent));
    const fxt::StringRef<fxt::RefType::kId> name_ref = fxt::StringRef{"drop_stats"_intern};
    const fxt::StringRef<fxt::RefType::kId> category_ref = fxt::StringRef{"kernel:meta"_intern};
    constexpr uint64_t num_args = 2;
    const fxt::Argument num_dropped_arg = fxt::Argument{"num_records"_intern, num_dropped_};
    const fxt::Argument bytes_dropped_arg = fxt::Argument{"num_bytes"_intern, bytes_dropped_};
    const uint64_t header =
        fxt::MakeHeader(fxt::RecordType::kEvent, record_size) |
        fxt::EventRecordFields::EventType::Make(
            ToUnderlyingType(fxt::EventType::kDurationComplete)) |
        fxt::EventRecordFields::ArgumentCount::Make(num_args) |
        fxt::EventRecordFields::ThreadRef::Make(cpu_ref_.HeaderEntry()) |
        fxt::EventRecordFields::CategoryStringRef::Make(category_ref.HeaderEntry()) |
        fxt::EventRecordFields::NameStringRef::Make(name_ref.HeaderEntry());

    return {
        .header = header,
        .start = first_dropped_.value(),
        .process_id = cpu_ref_.process().koid,
        .thread_id = cpu_ref_.thread().koid,
        .num_dropped_arg = num_dropped_arg.Header(),
        .bytes_dropped_arg = bytes_dropped_arg.Header(),
        .end = last_dropped_.value(),
    };
  }

  // Adds a dropped record of the given size to the tracked statistics.
  void TrackDroppedRecord(uint32_t size) {
    if (!first_dropped_.has_value()) {
      first_dropped_ = ktl::optional(current_boot_ticks());
    }
    last_dropped_ = ktl::optional(current_boot_ticks());
    num_dropped_++;
    bytes_dropped_ += size;
  }

  // The underlying SpscBuffer.
  SpscBuffer<KernelAspaceAllocator, const char*> buffer_;

  // This class keeps track of the duration, number, and size of trace records dropped when the
  // buffer is full. These statistics are emitted to the trace buffer as a duration as soon as
  // space is available to do so, at which point the values are reset to ktl::nullopt, in the
  // case of first_dropped_ and last_dropped_, or zero in the case of num_dropped_
  // and bytes_dropped_.
  ktl::optional<zx_instant_boot_ticks_t> first_dropped_;
  ktl::optional<zx_instant_boot_ticks_t> last_dropped_;
  // By storing num_dropped_ and bytes_dropped_ in 32-bit values, we ensure that they can each
  // be stored in a single 64-bit word in the FXT record we emit when space is available.
  uint32_t num_dropped_{0};
  uint32_t bytes_dropped_{0};
  fxt::ThreadRef<fxt::RefType::kInline> cpu_ref_{0, 0};
};

}  // namespace percpu_writer

#endif  // ZIRCON_KERNEL_LIB_PERCPU_WRITER_INCLUDE_LIB_PERCPU_WRITER_BUFFER_H_
