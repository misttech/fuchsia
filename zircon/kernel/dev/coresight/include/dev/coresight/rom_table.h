// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_DEV_CORESIGHT_INCLUDE_DEV_CORESIGHT_ROM_TABLE_H_
#define ZIRCON_KERNEL_DEV_CORESIGHT_INCLUDE_DEV_CORESIGHT_ROM_TABLE_H_

#include <lib/fit/result.h>
#include <zircon/assert.h>

#include <cstddef>
#include <cstdint>
#include <string_view>
#include <type_traits>

#include <dev/coresight/component.h>
#include <hwreg/bitfields.h>
#include <hwreg/internal.h>

extern "C" {
bool rust_coresight_rom_table_walk(void* io_ctx, uint32_t (*read32)(void*, uint32_t),
                                   uint64_t (*read64)(void*, uint32_t), uint32_t max_offset,
                                   void* cb_ctx, void (*callback)(void*, uint32_t),
                                   const char** out_error_reason_ptr, size_t* out_error_reason_len,
                                   uint32_t* out_error_offset);
}

namespace coresight {

// [CS] D6.4.4
struct Class0x1RomEntry : public hwreg::RegisterBase<Class0x1RomEntry, uint32_t> {
  DEF_FIELD(31, 12, offset);
  DEF_RSVDZ_FIELD(11, 9);
  DEF_FIELD(8, 4, powerid);
  DEF_RSVDZ_BIT(3);
  DEF_BIT(2, powerid_valid);
  DEF_BIT(1, format);
  DEF_BIT(0, present);

  static auto Get(uint32_t N) {
    return hwreg::RegisterAddr<Class0x1RomEntry>(N * static_cast<uint32_t>(sizeof(uint32_t)));
  }
};

// [CS] D7.5.17
struct Class0x9Rom32BitEntry : public hwreg::RegisterBase<Class0x9Rom32BitEntry, uint32_t> {
  DEF_FIELD(31, 12, offset);
  DEF_RSVDZ_FIELD(11, 9);
  DEF_FIELD(8, 4, powerid);
  DEF_RSVDZ_BIT(3);
  DEF_BIT(2, powerid_valid);
  DEF_FIELD(1, 0, present);

  static auto Get(uint32_t N) {
    return hwreg::RegisterAddr<Class0x9Rom32BitEntry>(N * static_cast<uint32_t>(sizeof(uint32_t)));
  }
};

// [CS] D7.5.17
struct Class0x9Rom64BitEntry : public hwreg::RegisterBase<Class0x9Rom64BitEntry, uint64_t> {
  DEF_FIELD(63, 12, offset);
  DEF_RSVDZ_FIELD(11, 9);
  DEF_FIELD(8, 4, powerid);
  DEF_RSVDZ_BIT(3);
  DEF_BIT(2, powerid_valid);
  DEF_FIELD(1, 0, present);

  static auto Get(uint32_t N) {
    return hwreg::RegisterAddr<Class0x9Rom64BitEntry>(N * static_cast<uint32_t>(sizeof(uint64_t)));
  }
};

// [CS] D7.5.10
struct Class0x9RomDeviceIdRegister
    : public hwreg::RegisterBase<Class0x9RomDeviceIdRegister, uint32_t> {
  enum class Format : uint8_t {
    k32Bit = 0,
    k64Bit = 1,
  };

  DEF_RSVDZ_FIELD(31, 6);
  DEF_BIT(5, prr);
  DEF_BIT(4, sysmem);
  DEF_ENUM_FIELD(Format, 3, 0, format);

  static auto Get() { return hwreg::RegisterAddr<Class0x9RomDeviceIdRegister>(0xfc8); }
};

// [CS] D5
// A ROM table is a basic CoreSight component that provides pointers to other
// components (including other ROM tables) in its lower registers via offsets
// from its base address. It is an organizational structure that can be used to
// find all CoreSight components - possibly as well as legacy or
// vendor-specific ones - on an SoC. Thought of as a tree, the leaves are the
// system's CoreSight components and the root is typically referred to as the
// "base ROM table" (or, more plainly, "the ROM table").
class RomTable {
 public:
  // Represents an error occurred while walking the table.
  struct WalkError {
    std::string_view reason;
    uint32_t offset{};
  };

  // Walks the underlying tree of components with no dynamic allocation,
  // calling `callback` on the offset from the table's base address (implicitly
  // encoded in `io`) of each component found. The (`io`, `max_offset`)
  // together implicitly give the aperture to walk.
  //
  // The walk will visit and access the first page of memory of each found
  // component. Unfortunately, however, there is no canonical means to
  // determine how large a region of memory this entails. The determination of
  // the maximum visited offset - or at least something deemed large enough -
  // is left to the caller. The offset must at least be
  // `kMininumComponentSize`, which is the size of the base table proper.
  template <typename IoProvider, typename ComponentCallback>
  static fit::result<WalkError> Walk(IoProvider io, uint32_t max_offset,
                                     ComponentCallback&& callback) {
    static_assert(std::is_invocable_v<ComponentCallback, uint32_t>);
    ZX_ASSERT(max_offset >= kMinimumComponentSize);
    auto read32 = [](void* ctx, uint32_t offset) -> uint32_t {
      auto* io_ptr = static_cast<IoProvider*>(ctx);
      uint32_t ret = 0;
      hwreg::internal::Visit(
          [offset, &ret](auto&& inner_io) { ret = inner_io.template Read<uint32_t>(offset); },
          *io_ptr);
      return ret;
    };
    auto read64 = [](void* ctx, uint32_t offset) -> uint64_t {
      auto* io_ptr = static_cast<IoProvider*>(ctx);
      uint64_t ret = 0;
      hwreg::internal::Visit(
          [offset, &ret](auto&& inner_io) { ret = inner_io.template Read<uint64_t>(offset); },
          *io_ptr);
      return ret;
    };
    auto cb = [](void* ctx, uint32_t offset) {
      auto* cb_ptr = static_cast<std::remove_reference_t<ComponentCallback>*>(ctx);
      (*cb_ptr)(offset);
    };
    const char* err_ptr = nullptr;
    size_t err_len = 0;
    uint32_t err_offset = 0;
    if (!rust_coresight_rom_table_walk(&io, read32, read64, max_offset, &callback, cb, &err_ptr,
                                       &err_len, &err_offset)) {
      return fit::error(WalkError{std::string_view(err_ptr, err_len), err_offset});
    }
    return fit::ok();
  }
};

}  // namespace coresight

#endif  // ZIRCON_KERNEL_DEV_CORESIGHT_INCLUDE_DEV_CORESIGHT_ROM_TABLE_H_
