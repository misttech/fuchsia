// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/percpu_writer/buffer.h>
#include <lib/unittest/unittest.h>

namespace {
bool TestDropStats() {
  BEGIN_TEST;
  percpu_writer::Buffer buffer;
  buffer.Init(128, "test-buffer", fxt::ThreadRef{fxt::Koid{0xAA}, fxt::Koid{0xBB}});
  // Fill up the buffer
  zx::result<percpu_writer::Buffer::Reservation> reservation = buffer.Reserve(128);
  ASSERT_TRUE(reservation.is_ok());
  ktl::byte data[8] = {ktl::byte{0x1}, ktl::byte{0x2}, ktl::byte{0x3}, ktl::byte{0x4},
                       ktl::byte{0x5}, ktl::byte{0x6}, ktl::byte{0x7}, ktl::byte{0x8}};
  for (unsigned i = 0; i < 128 / 8; i++) {
    reservation->Write({data, 8});
  }
  reservation->Commit();

  // Reserving anything should fail
  zx_instant_boot_ticks_t drop_start_time = current_boot_ticks();
  ASSERT_TRUE(buffer.Reserve(24).is_error());
  ASSERT_TRUE(buffer.Reserve(16).is_error());
  ASSERT_TRUE(buffer.Reserve(32).is_error());
  zx_instant_boot_ticks_t drop_end_time = current_boot_ticks();

  // Clear out the buffer so we can write the dropped records record
  buffer.Drain();
  zx::result<percpu_writer::Buffer::Reservation> trigger_stats = buffer.Reserve(8);
  ASSERT_TRUE(trigger_stats.is_ok());
  trigger_stats->Write({data, 8});
  trigger_stats->Commit();

  ktl::byte buf[128];
  auto copy_fn = [&buf](uint32_t offset, ktl::span<ktl::byte> src) mutable -> zx_status_t {
    ktl::ranges::copy(src, buf + offset);
    return ZX_OK;
  };
  zx::result<uint32_t> bytes_read = buffer.Read(copy_fn, 128);
  ASSERT_TRUE(bytes_read.is_ok());
  // We should only get the drop stats plus the record that triggered it.
  ASSERT_EQ(8 + sizeof(percpu_writer::Buffer::DroppedRecordDurationEvent), *bytes_read);

  percpu_writer::Buffer::DroppedRecordDurationEvent* record =
      reinterpret_cast<percpu_writer::Buffer::DroppedRecordDurationEvent*>(buf);

  ASSERT_GE(record->start, drop_start_time);
  ASSERT_LE(record->end, drop_end_time);

  uint64_t num_dropped_value = record->num_dropped_arg >> 32;
  uint64_t bytes_dropped_value = record->bytes_dropped_arg >> 32;
  ASSERT_EQ(num_dropped_value, uint64_t{3});
  ASSERT_EQ(bytes_dropped_value, uint64_t{24 + 16 + 32});

  ASSERT_EQ(record->process_id, uint64_t{0xAA});
  ASSERT_EQ(record->thread_id, uint64_t{0xBB});

  // After, we should get our record.
  ASSERT_BYTES_EQ(
      reinterpret_cast<uint8_t*>(buf + sizeof(percpu_writer::Buffer::DroppedRecordDurationEvent)),
      reinterpret_cast<uint8_t*>(data), size_t{8});

  END_TEST;
}
}  // namespace

UNITTEST_START_TESTCASE(percpu_writer_tests)
UNITTEST("DropStats", TestDropStats)
UNITTEST_END_TESTCASE(percpu_writer_tests, "percpu_writer", "PerCPU Fxt tests")
