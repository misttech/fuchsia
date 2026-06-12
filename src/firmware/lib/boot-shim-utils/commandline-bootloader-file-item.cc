// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/boot-shim-utils/commandline-bootloader-file-item.h>
#include <stdio.h>
#include <string.h>

#include <limits>
#include <ranges>

#include "third_party/modp_b64/modp_b64.h"

namespace {

// Executes `callback` on each `cmdline` arg starting with `prefix`.
template <typename Callback>
void ForEachChunk(std::string_view cmdline, std::string_view prefix, Callback&& callback) {
  // Don't bother with quoting or complex cases, we don't expect the bootloader to be passing
  // anything that won't work with simple space delimiters.
  for (const auto range : std::views::split(cmdline, std::string_view(" "))) {
    std::string_view arg(range.begin(), range.end());
    if (arg.starts_with(prefix)) {
      callback(arg.substr(prefix.length()));
    }
  }
}

// Returns the `ZBI_TYPE_BOOTLOADER_FILE` payload size, or error if it won't fit.
fit::result<CommandlineBootloaderFileItem::DataZbi::Error, uint32_t> PayloadSize(
    std::string_view filename, size_t content_size) {
  // Filename length must fit in a single byte.
  if (filename.size() > std::numeric_limits<uint8_t>::max()) {
    return fit::error(CommandlineBootloaderFileItem::DataZbi::Error{
        .zbi_error = "Bootloader file name overflow",
        .item_offset = 0,
    });
  }

  // ZBI item payload length must fit in a U32.
  size_t payload_size = 1 + filename.size() + content_size;
  if (payload_size <= content_size || payload_size > std::numeric_limits<uint32_t>::max()) {
    return fit::error(CommandlineBootloaderFileItem::DataZbi::Error{
        .zbi_error = "Bootloader file size overflow",
        .item_offset = 0,
    });
  }

  return fit::ok(static_cast<uint32_t>(payload_size));
}

}  // namespace

void CommandlineBootloaderFileItem::Init(std::string_view cmdline, std::string_view prefix,
                                         std::string_view filename) {
  cmdline_ = cmdline;
  prefix_ = prefix;
  filename_ = filename;

  // Pre-calculate Base64 length since we'll need this a few times.
  base64_size_ = 0;
  ForEachChunk(cmdline_, prefix_, [&](std::string_view chunk) { base64_size_ += chunk.size(); });
}

size_t CommandlineBootloaderFileItem::size_bytes() const {
  if (base64_size_ == 0) {
    return 0;
  }
  // This function can't fail so just return 0 (no ZBI space allocation) on error; we'll report the
  // actual error later in `AppendItems()`.
  auto res = PayloadSize(filename_, base64_size_);
  return res.is_ok() ? ItemSize(res.value()) : 0;
}

fit::result<CommandlineBootloaderFileItem::DataZbi::Error>
CommandlineBootloaderFileItem::AppendItems(DataZbi& zbi) const {
  if (base64_size_ == 0) {
    return fit::ok();
  }

  auto base64_payload_size = PayloadSize(filename_, base64_size_);
  if (base64_payload_size.is_error()) {
    return base64_payload_size.take_error();
  }

  auto zbi_item = zbi.Append({
      .type = ZBI_TYPE_BOOTLOADER_FILE,
      // Request enough buffer for the filename header plus full Base64 capacity; we will shrink
      // the final item length after decoding the data.
      .length = *base64_payload_size,
      .extra = 0,
      .flags = 0,
      .magic = ZBI_ITEM_MAGIC,
  });
  if (zbi_item.is_error()) {
    return zbi_item.take_error();
  }

  // Write the header: filename length (1 byte) and filename.
  zbi_item->payload[0] = static_cast<std::byte>(filename_.size());
  memcpy(zbi_item->payload.data() + 1, filename_.data(), filename_.size());

  // Copy the Base64 data after the header.
  char* base64_buffer = reinterpret_cast<char*>(zbi_item->payload.data()) + 1 + filename_.size();
  size_t offset = 0;
  ForEachChunk(cmdline_, prefix_, [&](std::string_view chunk) {
    memcpy(base64_buffer + offset, chunk.data(), chunk.size());
    offset += chunk.size();
  });

  // Decode Base64 in-place. Since decoded data is always smaller, this only overwrites data which
  // has already been read so is safe to do.
  size_t decoded_size = modp_b64_decode(base64_buffer, base64_buffer, base64_size_);
  if (decoded_size == MODP_B64_ERROR) {
    return fit::error(DataZbi::Error{
        .zbi_error = "Invalid Base64 in commandline bootloader file chunks",
        .item_offset = 0,
    });
  }

  printf("commandline-bootloader-file-item: registering bootloader file '%.*s' (%zu bytes)\n",
         static_cast<int>(filename_.size()), filename_.data(), decoded_size);

  fit::result final_payload_size = PayloadSize(filename_, decoded_size);
  if (final_payload_size.is_error()) {
    return final_payload_size.take_error();
  }

  // Resize the item to only contain the decoded data size.
  if (auto trim_res = zbi.TrimLastItem(*zbi_item, *final_payload_size); trim_res.is_error()) {
    return trim_res.take_error();
  }

  return fit::ok();
}
