// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_STORAGE_LIB_DISK_INSPECTOR_DISK_PRIMITIVE_H_
#define SRC_STORAGE_LIB_DISK_INSPECTOR_DISK_PRIMITIVE_H_

#include <lib/syslog/cpp/macros.h>
#include <zircon/assert.h>
#include <zircon/errors.h>
#include <zircon/types.h>

#include <cstdint>
#include <cstdlib>
#include <ios>
#include <limits>
#include <sstream>
#include <string>
#include <type_traits>
#include <utility>
#include <vector>

#include "src/storage/lib/disk_inspector/disk_obj.h"
#include "src/storage/lib/disk_inspector/supported_types.h"

namespace disk_inspector {

namespace internal {

template <typename T>
zx_status_t StringToUint(const std::string& string, T* out) {
  static_assert(std::is_integral<T>::value && std::is_unsigned<T>::value);
  char* endptr;
  uint64_t value = std::strtoull(string.c_str(), &endptr, 0);
  if (*endptr != '\0' || value > std::numeric_limits<T>::max()) {
    FX_LOGS(INFO) << "String " << string << " cannot be converted to unsigned int.";
    return ZX_ERR_INVALID_ARGS;
  }
  *out = static_cast<T>(value);
  return ZX_OK;
}

template <typename T>
std::string UintToHexString(T* position) {
  static_assert(std::is_integral<T>::value && std::is_unsigned<T>::value);
  std::ostringstream stream;
  stream << "0x" << std::hex << *position;
  return stream.str();
}

template <typename T>
std::string UintToString(T* position) {
  static_assert(std::is_integral<T>::value && std::is_unsigned<T>::value);
  return std::to_string(*position);
}

}  // namespace internal

template <typename T>
class Primitive : public DiskObj {
  static_assert(std::is_integral<T>::value && std::is_unsigned<T>::value);

 public:
  explicit Primitive(std::string name) : name_(std::move(name)) {}
  ~Primitive() override = default;

  // DiskStruct interface:
  std::string GetTypeName() override { return name_; }

  uint64_t GetSize() override { return sizeof(T); }

  zx_status_t WriteField(void* position, std::vector<std::string> keys,
                         std::vector<uint64_t> indices, const std::string& value) override {
    ZX_DEBUG_ASSERT(keys.empty() && indices.empty());
    auto element = reinterpret_cast<T*>(position);
    return internal::StringToUint<T>(value, element);
  }

  std::string ToString(void* position, const PrintOptions& options) override {
    auto element = reinterpret_cast<T*>(position);
    if (options.display_hex) {
      return internal::UintToHexString<T>(element);
    }
    return internal::UintToString<T>(element);
  }

 private:
  std::string name_;
};

}  // namespace disk_inspector

#endif  // SRC_STORAGE_LIB_DISK_INSPECTOR_DISK_PRIMITIVE_H_
