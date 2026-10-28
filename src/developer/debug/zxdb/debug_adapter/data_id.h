// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_DEBUG_ZXDB_DEBUG_ADAPTER_DATA_ID_H_
#define SRC_DEVELOPER_DEBUG_ZXDB_DEBUG_ADAPTER_DATA_ID_H_

#include <cstdint>
#include <string>
#include <string_view>

#include "src/developer/debug/shared/arch.h"
#include "src/developer/debug/zxdb/client/breakpoint_settings.h"
#include "src/developer/debug/zxdb/common/err_or.h"

namespace zxdb {

class ExprValue;

// Represents the DAP `dataId` identifier exchanged between `dataBreakpointInfo` and
// `setDataBreakpoints` requests, as well as `zxdb.SetTracepoints` watch specifications.
//
// Format:
//   "<process_koid>:0x<address_hex>:<byte_size>"
//   Example: "17601:0x7ffee1234000:4"
//
// Fields & Invariants:
//   - `process_koid`: Base-10 Zircon KOID of the target process. Must be non-zero
//     (`ZX_KOID_INVALID` == 0 is rejected). Because virtual memory addresses are only meaningful
//     within the address space of the process in which the expression was evaluated, a valid
//     process KOID is always required so the watchpoint is scoped strictly to that process.
//   - `address`: Base-16 virtual address prefixed with "0x" or "0X". Must not overflow the 64-bit
//     address space when combined with `byte_size` (`address <= UINT64_MAX - byte_size`).
//   - `byte_size`: Base-10 size in bytes of the watched memory location. Must be supported by the
//     target architecture's hardware debug registers (`BreakpointSettings::ValidateSize`).
class DataId {
 public:
  // Evaluates whether `value` can be watched and returns a validated `DataId`, or an `Err`
  // describing why a data breakpoint cannot be set on `value`. Only properties of the value itself
  // are checked here; `process_koid` must already be a valid (non-zero) KOID.
  static ErrOr<DataId> FromExprValue(debug::Arch arch, uint64_t process_koid,
                                     const std::string& name, const ExprValue& value);

  // Parses a `<process_koid>:0x<address_hex>:<byte_size>` string and checks format, non-zero
  // `process_koid`, and 64-bit address range overflow invariants.
  static ErrOr<DataId> Parse(std::string_view data_id);

  // Validates that `byte_size()` is supported by `arch` for the given hardware watchpoint `type`.
  Err ValidateSize(debug::Arch arch, BreakpointSettings::Type type) const;

  // Formats this `DataId` into its canonical string representation.
  std::string ToString() const;

  uint64_t process_koid() const { return process_koid_; }
  uint64_t address() const { return address_; }
  uint32_t byte_size() const { return byte_size_; }

 private:
  DataId(uint64_t process_koid, uint64_t address, uint32_t byte_size)
      : process_koid_(process_koid), address_(address), byte_size_(byte_size) {}

  uint64_t process_koid_ = 0;
  uint64_t address_ = 0;
  uint32_t byte_size_ = 0;
};

}  // namespace zxdb

#endif  // SRC_DEVELOPER_DEBUG_ZXDB_DEBUG_ADAPTER_DATA_ID_H_
