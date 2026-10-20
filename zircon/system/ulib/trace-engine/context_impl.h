// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef ZIRCON_SYSTEM_ULIB_TRACE_ENGINE_CONTEXT_IMPL_H_
#define ZIRCON_SYSTEM_ULIB_TRACE_ENGINE_CONTEXT_IMPL_H_

#include <lib/trace-engine/buffer_internal.h>
#include <lib/trace-engine/context.h>
#include <lib/trace-engine/handler.h>
#include <lib/zx/event.h>
#include <zircon/assert.h>

#include <atomic>
#include <mutex>
#include <span>
#include <unordered_map>

#include "buffer.h"
#include "rolling_buffer.h"

// Two preprocessor symbols control what symbols we export in a .so:
// EXPORT and EXPORT_NO_DDK:
// - EXPORT is for symbols exported to both driver and non-driver versions of
//   the library ("non-driver" is the normal case).
// - EXPORT_NO_DDK is for symbols *not* exported in the DDK.
// A third variant is supported which is to export nothing. This is for cases
// like libvulkan which want tracing but do not have access to
// libtrace-engine.so.
// Two preprocessor symbols are provided by the build system to select which
// variant we are building: STATIC_LIBRARY. If it is not defined
// (normal case), or exactly one of them is defined.
#if defined(STATIC_LIBRARY)
#define EXPORT
#define EXPORT_NO_DDK
#else
#define EXPORT __EXPORT
#define EXPORT_NO_DDK __EXPORT
#endif

using trace::internal::trace_buffer_header;

// Return true if there are no buffer acquisitions of the trace context.
bool trace_engine_is_buffer_context_released();

// Called from trace_context to notify the engine a buffer needs saving.
void trace_engine_request_save_buffer(uint32_t wrapped_count, uint64_t durable_data_end);

// Maintains state for a single trace session.
// This structure is accessed concurrently from many threads which hold trace
// context references.
// Implements the opaque type declared in <trace-engine/context.h>.
struct trace_context {
  trace_context(void* buffer, size_t buffer_num_bytes, trace_buffering_mode_t buffering_mode,
                trace_handler_t* handler);

  ~trace_context();

  // Copy the current header state into dest
  void get_header(trace_buffer_header* dest) const {
    std::lock_guard<std::mutex> lock(header_mutex_);
    memcpy(dest, header_, sizeof(trace_buffer_header));
  }

  static size_t min_buffer_size() { return kMinPhysicalBufferSize; }

  static size_t max_buffer_size() { return kMaxPhysicalBufferSize; }

  uint32_t generation() const { return generation_; }

  trace_handler_t* handler() const { return handler_; }

  trace_buffering_mode_t buffering_mode() const { return buffering_mode_; }

  uint64_t num_records_dropped() const {
    return num_records_dropped_.load(std::memory_order_relaxed);
  }

  bool UsingDurableBuffer() const { return buffering_mode_ != TRACE_BUFFERING_MODE_ONESHOT; }

  // Return true if at least one record was dropped.
  bool WasRecordDropped() const { return num_records_dropped() != 0u; }

  void MarkRecordsDropped(size_t count) {
    num_records_dropped_.fetch_add(count, std::memory_order_relaxed);
  }
  void MarkRecordDropped() { MarkRecordsDropped(1); }

  void ResetRollingBufferPointers();
  void ResetBufferPointers();
  void InitBufferHeader();
  void ClearEntireBuffer();
  void ClearRollingBuffers();
  void UpdateBufferHeaderAfterStopped();

  void ServiceBuffers();

  uint64_t* AllocRecord(size_t num_bytes);
  uint64_t* AllocDurableRecord(size_t num_bytes);
  bool AllocThreadIndex(trace_thread_index_t* out_index);
  bool AllocStringIndex(trace_string_index_t* out_index);

  // Looks up |bytes_pointer| in the process-wide string literal cache using a lock-free
  // open-addressed read fast path. On a miss, acquires |string_table_mutex_|, allocates a new
  // string index, invokes |write_record(index)|, and publishes the entry with release semantics.
  template <typename WriteRecordFn>
  bool LookupOrAllocStringIndex(const unsigned char* bytes_pointer, trace_string_index_t* out_index,
                                WriteRecordFn&& write_record) {
    const uint64_t key = StringTableKey(bytes_pointer);
    const size_t start_slot = HashStringTableKey(key);
    if (FindStringTableEntry(key, start_slot, std::memory_order_acquire, out_index)) {
      return true;
    }

    std::scoped_lock lock(string_table_mutex_);
    // Re-probe under the lock: another thread may have inserted |key| since the lock-free probe.
    size_t empty_slot;
    if (FindStringTableEntry(key, start_slot, std::memory_order_relaxed, out_index, &empty_slot)) {
      return true;
    }
    trace_string_index_t index;
    if (likely(AllocStringIndex(&index) && write_record(index))) {
      if (likely(empty_slot < kStringTableCapacity)) {
        string_table_[empty_slot].store(MakeStringTableEntry(key, index),
                                        std::memory_order_release);
      }
      *out_index = index;
      return true;
    }
    return false;
  }

  // Looks up |vthread_koid| in the process-wide virtual thread cache. If not found, invokes
  // |register_vthread()| to write the thread records and caches the resulting thread reference.
  template <typename RegisterFn>
  trace_thread_ref_t LookupOrRegisterVThread(zx_koid_t vthread_koid,
                                             RegisterFn&& register_vthread) {
    std::scoped_lock lock(vthread_table_mutex_);
    if (auto it = vthread_table_.find(vthread_koid); it != vthread_table_.end()) {
      return it->second;
    }
    trace_thread_ref_t ref = register_vthread();
    vthread_table_.emplace(vthread_koid, ref);
    return ref;
  }

  // This is called by the handler when it has been notified that a buffer
  // has been saved.
  // |wrapped_count| is the wrapped count at the time the buffer save request
  // was made. Similarly for |durable_data_end|.
  void MarkRollingBufferSaved(uint32_t wrapped_count, uint64_t durable_data_end);

  // Requests to prepare the buffer to be read. Only has effect in streaming mode.
  //
  // Normally streaming mode only sends data once it has enough data to require a buffer swap and
  // data flush. This produces fewer, but larger and bursty data transfers. Some clients may want
  // more regular smaller data transfers. These clients may instead manually invoke FlushBuffer
  // whenever they are ready for data.
  //
  // Once received, trace-engine will prepare the buffer to be read, and send a callback
  // requesting a buffer save when it is ready.
  void FlushBuffer();

  // This is only called from the engine to initiate a buffer save.
  void HandleSaveRollingBufferRequest(uint32_t wrapped_count, uint64_t durable_data_end);

 private:
  // The physical buffer must be at least this big.
  // Mostly this is here to simplify buffer size calculations.
  // It's as small as it is to simplify some testcases.
  static constexpr size_t kMinPhysicalBufferSize = 4096;

  // The physical buffer can be at most this big.
  // To keep things simple we ignore the header.
  static constexpr size_t kMaxPhysicalBufferSize = RollingBuffer::kMaxRollingBufferSize;

  // Given a buffer of size |SIZE| in bytes, not including the header,
  // return how much to use for the durable buffer. This is further adjusted
  // to be at most |kMaxDurableBufferSize|, and to account for rolling
  // buffer size alignment constraints.
#define GET_DURABLE_BUFFER_SIZE(size) ((size) / 16)

  // Ensure the smallest buffer is still large enough to hold
  // |kMinDurableBufferSize|.
  static_assert(GET_DURABLE_BUFFER_SIZE(kMinPhysicalBufferSize - sizeof(trace_buffer_header)) >=
                DurableBuffer::kMinDurableBufferSize);

  void ComputeBufferSizes();

  void SetPendingBufferService(RollingBufferState prev_state);

  // The generation counter associated with this context to distinguish
  // it from previously created contexts.
  uint32_t generation_{0u};

  // The buffering mode.
  trace_buffering_mode_t const buffering_mode_;

  // The entire physical buffer.
  const std::span<uint8_t> buffer_;

  // Aliases with buffer_, but as a trace_buffer_header*
  //
  // We don't write the header as atomics and we can write to the header in both trace writing
  // threads in the case of a buffer switch as well as on the trace-engine async loop.
  // Coordinating writing them locklessly would be difficult, we grab a mutex.
  mutable std::mutex header_mutex_;
  trace_buffer_header* const header_ __TA_GUARDED(header_mutex_);

  // Durable-record buffer
  //
  // This only used in circular and streaming modes: There is no separate
  // buffer for durable records in oneshot mode.
  DurableBuffer durable_buffer_;

  // General record buffer.
  RollingBuffer rolling_buffer_;

  std::atomic<RollingBufferState> pending_service_{RollingBufferState()};

  // A count of the number of records that have been dropped.
  std::atomic<uint64_t> num_records_dropped_{0};

  // Handler associated with the trace session.
  trace_handler_t* const handler_;

  // The next thread index to be assigned.
  std::atomic<trace_thread_index_t> next_thread_index_{TRACE_ENCODED_THREAD_REF_MIN_INDEX};

  // The next string table index to be assigned.
  std::atomic<trace_string_index_t> next_string_index_{TRACE_ENCODED_STRING_REF_MIN_INDEX};

  // Fixed-capacity open-addressed process-wide cache of registered string literals keyed by
  // pointer address. Each slot packs (pointer, string index) into one 64-bit word so that a probe
  // costs a single 8-byte atomic load: Zircon user addresses fit in 48 bits (bits above 47 only
  // ever carry TBI/HWASan tags, which never distinguish objects) and string indices fit in 16 bits.
  // Entries are insert-only within a trace session and cleared only in ResetBufferPointers() when
  // no thread holds an acquired context, so readers probe lock-free while insertions serialize on
  // |string_table_mutex_|. Probing is bounded so that a full or clustered table degrades to an
  // uncached index allocation instead of a scan of the whole table.
  static constexpr size_t kStringTableCapacityLog2 = 10;
  static constexpr size_t kStringTableCapacity = size_t{1} << kStringTableCapacityLog2;
  static constexpr size_t kStringTableMaxProbes = 32;
  static constexpr unsigned kStringTableKeyBits = 48;
  static constexpr uint64_t kStringTableKeyMask = (uint64_t{1} << kStringTableKeyBits) - 1;
  static constexpr uint64_t kEmptyStringTableEntry = 0;
  static_assert(sizeof(uintptr_t) == sizeof(uint64_t));
  static_assert(kStringTableMaxProbes <= kStringTableCapacity);
  static_assert(TRACE_ENCODED_STRING_REF_MAX_INDEX < (uint64_t{1} << (64 - kStringTableKeyBits)));
  static_assert(TRACE_ENCODED_STRING_REF_MIN_INDEX > 0, "a populated entry must never be 0");
  static uint64_t StringTableKey(const unsigned char* ptr) {
    return reinterpret_cast<uintptr_t>(ptr) & kStringTableKeyMask;
  }
  static size_t HashStringTableKey(uint64_t key) {
    key ^= key >> 33u;
    return static_cast<size_t>((key * uint64_t{0x9e3779b97f4a7c15}) >>
                               (uint64_t{64} - kStringTableCapacityLog2));
  }
  static uint64_t MakeStringTableEntry(uint64_t key, trace_string_index_t index) {
    return (static_cast<uint64_t>(index) << kStringTableKeyBits) | key;
  }
  static uint64_t StringTableEntryKey(uint64_t entry) { return entry & kStringTableKeyMask; }
  static trace_string_index_t StringTableEntryIndex(uint64_t entry) {
    return static_cast<trace_string_index_t>(entry >> kStringTableKeyBits);
  }
  // Probes up to kStringTableMaxProbes slots starting at |start_slot|. Returns true and sets
  // |*out_index| if |key| is present. Otherwise, if |empty_slot| is non-null, sets it to the first
  // empty slot in the probe window, or kStringTableCapacity if the window is full.
  bool FindStringTableEntry(uint64_t key, size_t start_slot, std::memory_order order,
                            trace_string_index_t* out_index, size_t* empty_slot = nullptr) const {
    if (empty_slot) {
      *empty_slot = kStringTableCapacity;
    }
    for (size_t i = 0; i < kStringTableMaxProbes; ++i) {
      const size_t slot = (start_slot + i) & (kStringTableCapacity - 1);
      const uint64_t entry = string_table_[slot].load(order);
      if (entry == kEmptyStringTableEntry) {
        if (empty_slot) {
          *empty_slot = slot;
        }
        return false;
      }
      if (StringTableEntryKey(entry) == key) {
        *out_index = StringTableEntryIndex(entry);
        return true;
      }
    }
    return false;
  }
  std::mutex string_table_mutex_;
  std::atomic<uint64_t> string_table_[kStringTableCapacity]{};

  // Process-wide cache of registered virtual threads keyed by artificial koid.
  // Deduplicates virtual thread registrations across threads and when a single
  // thread exceeds the 4-entry thread-local cache, avoiding repeated durable
  // kKernelObject and kThread writes.
  std::mutex vthread_table_mutex_;
  std::unordered_map<zx_koid_t, trace_thread_ref_t> vthread_table_
      __TA_GUARDED(vthread_table_mutex_);
};

#endif  // ZIRCON_SYSTEM_ULIB_TRACE_ENGINE_CONTEXT_IMPL_H_
