// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_DEBUG_ZXDB_SYMBOLS_DWARF_DIE_REF_H_
#define SRC_DEVELOPER_DEBUG_ZXDB_SYMBOLS_DWARF_DIE_REF_H_

#include <lib/syslog/cpp/macros.h>
#include <stdint.h>

#include <optional>
#include <ostream>

namespace zxdb {

class DwarfDieRef {
 public:
  constexpr DwarfDieRef() = default;

  static constexpr DwarfDieRef Main(uint64_t off) { return DwarfDieRef(off); }

  constexpr bool is_valid() const { return offset_.has_value(); }
  constexpr explicit operator bool() const { return is_valid(); }

  constexpr uint64_t offset() const {
    FX_DCHECK(is_valid());
    return *offset_;
  }

  constexpr auto operator<=>(const DwarfDieRef& other) const = default;

  constexpr DwarfDieRef operator+(uint64_t addend) const {
    if (!is_valid()) {
      return DwarfDieRef();
    }
    return DwarfDieRef(*offset_ + addend);
  }

 private:
  constexpr explicit DwarfDieRef(uint64_t off) : offset_(off) {}

  std::optional<uint64_t> offset_;
};

inline std::ostream& operator<<(std::ostream& out, const DwarfDieRef& ref) {
  if (!ref.is_valid()) {
    return out << "<invalid>";
  }
  return out << "offset 0x" << std::hex << ref.offset();
}

}  // namespace zxdb

#endif  // SRC_DEVELOPER_DEBUG_ZXDB_SYMBOLS_DWARF_DIE_REF_H_
