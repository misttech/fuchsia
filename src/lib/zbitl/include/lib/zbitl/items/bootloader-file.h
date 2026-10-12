// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_LIB_ZBITL_INCLUDE_LIB_ZBITL_ITEMS_BOOTLOADER_FILE_H_
#define SRC_LIB_ZBITL_INCLUDE_LIB_ZBITL_ITEMS_BOOTLOADER_FILE_H_

#include <lib/zbitl/view.h>

#include <ranges>
#include <string_view>

namespace zbitl {

// The decoded representation of a ZBI_TYPE_BOOTLOADER_FILE item, or nothing.
// This object is contextually convertible to bool and the default-constructed
// state serves as the "no file" object, so there's no need to use something
// like std::optional to wrap it.
struct BootloaderFile {
  static constexpr bool IsFile(const BootloaderFile& file) { return !file.name.empty(); }

  constexpr explicit operator bool() const { return IsFile(*this); }

  // Consume the payload of a ZBI_TYPE_BOOTLOADER_FILE item.  Returns an empty
  // (false) object if the format is invalid.
  static constexpr BootloaderFile FromPayload(ByteView payload) {
    if (payload.empty()) [[unlikely]] {
      return {};
    }
    const uint8_t len = static_cast<uint8_t>(payload.front());
    payload = payload.subspan<1>();
    if (payload.size_bytes() < len) [[unlikely]] {
      return {};
    }
    return {
        .name = {reinterpret_cast<const char*>(payload.data()), len},
        .contents = payload.subspan(len),
    };
  }

  std::string_view name;
  ByteView contents;
};

// This takes the value_type of any zbitl::View whose payload is convertible to
// zbitl::ByteView.
inline constexpr auto ItemToBootLoaderFile = [](auto&& item) constexpr -> BootloaderFile {
  const auto& [header, payload] = item;
  if (header->type == ZBI_TYPE_BOOTLOADER_FILE) {
    return BootloaderFile::FromPayload(payload);
  }
  return {};
};

template <std::ranges::forward_range Zbi>
constexpr std::ranges::forward_range auto BootloaderFiles(Zbi&& zbi) {
  return std::views::filter(std::views::transform(std::forward<Zbi>(zbi),  //
                                                  ItemToBootLoaderFile),
                            BootloaderFile::IsFile);
}

}  // namespace zbitl

#endif  // SRC_LIB_ZBITL_INCLUDE_LIB_ZBITL_ITEMS_BOOTLOADER_FILE_H_
