// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_GRAPHICS_DISPLAY_LIB_API_TYPES_CPP_CLIENT_PRIORITY_H_
#define SRC_GRAPHICS_DISPLAY_LIB_API_TYPES_CPP_CLIENT_PRIORITY_H_

#include <fidl/fuchsia.hardware.display/cpp/wire.h>

#include <cstdint>

#if __cplusplus >= 202002L
#include <format>
#endif

namespace display {

// Equivalent to the FIDL type [`fuchsia.hardware.display/ClientPriority`].
//
// This is a value type with a strong ordering relationship. Instances can be
// stored in containers and sorted. Copying, moving and destruction are trivial.
class ClientPriority {
 public:
  explicit constexpr ClientPriority(fuchsia_hardware_display::wire::ClientPriorityValue fidl_value)
      : value_(fidl_value) {}

  constexpr ClientPriority(const ClientPriority&) noexcept = default;
  constexpr ClientPriority(ClientPriority&&) noexcept = default;
  constexpr ClientPriority& operator=(const ClientPriority&) = default;
  constexpr ClientPriority& operator=(ClientPriority&&) noexcept = default;
  ~ClientPriority() = default;

  constexpr fuchsia_hardware_display::wire::ClientPriority ToFidl() const {
    return {.value = value_};
  }
  constexpr fuchsia_hardware_display::wire::ClientPriorityValue ToFidlValue() const {
    return value_;
  }

  // Raw numerical value of the equivalent FIDL value.
  //
  // This is intended to be used for developer-facing output, such as logging
  // and Inspect. The values have the same stability guarantees as the
  // equivalent FIDL type.
  constexpr uint32_t ValueForLogging() const { return value_; }

  // See [`fuchsia.hardware.display/INVALID_CLIENT_PRIORITY_VALUE`].
  static const ClientPriority kInvalid;

  // See [`fuchsia.hardware.display/VIRTCON_CLIENT_PRIORITY_VALUE`].
  static const ClientPriority kVirtcon;

  // See [`fuchsia.hardware.display/PRIMARY_CLIENT_PRIORITY_VALUE`].
  static const ClientPriority kPrimary;

 private:
  friend constexpr bool operator==(const ClientPriority& lhs, const ClientPriority& rhs);
  friend constexpr bool operator!=(const ClientPriority& lhs, const ClientPriority& rhs);
  friend constexpr bool operator>(const ClientPriority& lhs, const ClientPriority& rhs);
  friend constexpr bool operator>=(const ClientPriority& lhs, const ClientPriority& rhs);
  friend constexpr bool operator<(const ClientPriority& lhs, const ClientPriority& rhs);
  friend constexpr bool operator<=(const ClientPriority& lhs, const ClientPriority& rhs);

  fuchsia_hardware_display::wire::ClientPriorityValue value_;
};

constexpr inline ClientPriority ClientPriority::kInvalid(
    fuchsia_hardware_display::wire::kInvalidClientPriorityValue);
constexpr inline ClientPriority ClientPriority::kVirtcon(
    fuchsia_hardware_display::wire::kVirtconClientPriorityValue);
constexpr inline ClientPriority ClientPriority::kPrimary(
    fuchsia_hardware_display::wire::kPrimaryClientPriorityValue);

constexpr bool operator==(const ClientPriority& lhs, const ClientPriority& rhs) {
  return lhs.value_ == rhs.value_;
}

constexpr bool operator!=(const ClientPriority& lhs, const ClientPriority& rhs) {
  return !(lhs == rhs);
}

constexpr bool operator>(const ClientPriority& lhs, const ClientPriority& rhs) {
  return lhs.value_ > rhs.value_;
}

constexpr bool operator>=(const ClientPriority& lhs, const ClientPriority& rhs) {
  return lhs.value_ >= rhs.value_;
}

constexpr bool operator<(const ClientPriority& lhs, const ClientPriority& rhs) {
  return lhs.value_ < rhs.value_;
}

constexpr bool operator<=(const ClientPriority& lhs, const ClientPriority& rhs) {
  return lhs.value_ <= rhs.value_;
}

}  // namespace display

#if __cplusplus >= 202002L
template <>
struct std::formatter<display::ClientPriority> : std::formatter<uint32_t> {
  auto format(const display::ClientPriority& client_priority, std::format_context& ctx) const {
    return std::formatter<uint32_t>::format(client_priority.ValueForLogging(), ctx);
  }
};
#endif

#endif  // SRC_GRAPHICS_DISPLAY_LIB_API_TYPES_CPP_CLIENT_PRIORITY_H_
