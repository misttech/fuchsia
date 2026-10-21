// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_USB_LIB_USB_INCLUDE_USB_DESCRIPTORS_H_
#define SRC_DEVICES_USB_LIB_USB_INCLUDE_USB_DESCRIPTORS_H_

#if !defined(__cplusplus) || __cplusplus < 202002L
#error "Descriptors library requires C++20 or later."
#endif

#include <endian.h>
#include <fidl/fuchsia.hardware.usb.descriptor/cpp/fidl.h>

#include <type_traits>
#include <utility>

namespace fuchsia_hardware_usb_descriptor {
void fuchsia_hardware_usb_descriptor_adl_tag(auto);
}  // namespace fuchsia_hardware_usb_descriptor

namespace fuchsia_hardware_usb_descriptor_internal {
template <typename E>
concept IsUsbDescriptorEnum =
    !std::is_integral_v<E> && (std::is_enum_v<E> || fidl::IsFidlType<E>::value || requires(E e) {
      { fidl::ToUnderlying(e) } -> std::integral;
    }) && requires(E e) { fuchsia_hardware_usb_descriptor_adl_tag(e); };
}  // namespace fuchsia_hardware_usb_descriptor_internal

namespace fuchsia_hardware_usb_descriptor {

// Overloaded bitwise OR operators for setup request enums via IsUsbBitmaskEnum concept.
template <typename E>
concept IsUsbBitmaskEnum = std::is_same_v<E, EndpointDirection> || std::is_same_v<E, RequestType> ||
                           std::is_same_v<E, RequestRecipient>;

template <typename E>
inline constexpr auto ToUnderlyingHelper(E val) {
  if constexpr (std::is_integral_v<E>) {
    return val;
  } else if constexpr (requires { fidl::ToUnderlying(val); }) {
    return fidl::ToUnderlying(val);
  } else {
    return static_cast<std::underlying_type_t<E>>(val);
  }
}

template <IsUsbBitmaskEnum E1, IsUsbBitmaskEnum E2>
inline constexpr uint8_t operator|(E1 a, E2 b) {
  return static_cast<uint8_t>(fidl::ToUnderlying(a) | fidl::ToUnderlying(b));
}
template <IsUsbBitmaskEnum E, typename T>
  requires std::is_integral_v<T>
inline constexpr uint8_t operator|(E a, T b) {
  return static_cast<uint8_t>(fidl::ToUnderlying(a) | b);
}
template <typename T, IsUsbBitmaskEnum E>
  requires std::is_integral_v<T>
inline constexpr uint8_t operator|(T a, E b) {
  return static_cast<uint8_t>(a | fidl::ToUnderlying(b));
}

template <IsUsbBitmaskEnum E1, IsUsbBitmaskEnum E2>
inline constexpr uint8_t operator&(E1 a, E2 b) {
  return static_cast<uint8_t>(fidl::ToUnderlying(a) & fidl::ToUnderlying(b));
}
template <IsUsbBitmaskEnum E, typename T>
  requires std::is_integral_v<T>
inline constexpr uint8_t operator&(E a, T b) {
  return static_cast<uint8_t>(fidl::ToUnderlying(a) & b);
}
template <typename T, IsUsbBitmaskEnum E>
  requires std::is_integral_v<T>
inline constexpr uint8_t operator&(T a, E b) {
  return static_cast<uint8_t>(a & fidl::ToUnderlying(b));
}

// Architectural Note:
// When matching or switching on extensible FIDL class codes (e.g., UsbClass) and vendor protocols,
// driver maintainers should explicitly handle `.is_unknown()` fallback paths. This ensures wire
// compatibility with novel USB hardware and newly introduced specification codes.
template <typename E>
concept IsDescriptorEnum = fuchsia_hardware_usb_descriptor_internal::IsUsbDescriptorEnum<E>;

template <typename T, IsDescriptorEnum E>
  requires std::is_integral_v<T>
inline constexpr bool operator==(T a, E b) {
  return std::cmp_equal(a, ToUnderlyingHelper(b));
}

template <typename T, IsDescriptorEnum E>
  requires std::is_integral_v<T>
inline constexpr bool operator==(E a, T b) {
  return std::cmp_equal(ToUnderlyingHelper(a), b);
}

}  // namespace fuchsia_hardware_usb_descriptor

using fuchsia_hardware_usb_descriptor::IsDescriptorEnum;

// maximum number of endpoints per device
#define USB_MAX_EPS 32

/* USB BCD Version encoding (e.g. USB_BCD_VERSION(2, 0, 1) -> 0x0201) */
#define USB_BCD_VERSION(major, minor, subminor) \
  (((major) << 8) | (((minor) & 0xf) << 4) | ((subminor) & 0xf))

#define USB_2_0 USB_BCD_VERSION(2, 0, 0)
#define USB_2_0_1 USB_BCD_VERSION(2, 0, 1)
#define USB_2_1 USB_BCD_VERSION(2, 1, 0)
#define USB_3_0 USB_BCD_VERSION(3, 0, 0)
#define USB_3_1 USB_BCD_VERSION(3, 1, 0)
#define USB_3_2 USB_BCD_VERSION(3, 2, 0)

using UsbMode = fuchsia_hardware_usb_descriptor::UsbMode;

static inline const char* usb_mode_to_string(UsbMode mode) {
  switch (mode) {
    case UsbMode::kNone:
      return "NONE";
    case UsbMode::kHost:
      return "HOST";
    case UsbMode::kPeripheral:
      return "PERIPHERAL";
    case UsbMode::kOtg:
      return "OTG";
    default:
      return "<unknown>";
  }
}

// TODO(https://fxbug.dev/42062723) : Some of these structs are duplicates of usb banjo. Remove and
// consolidate them.
/* general USB defines */
typedef struct {
  uint8_t bm_request_type;
  uint8_t b_request;
  uint16_t w_value;
  uint16_t w_index;
  uint16_t w_length;
} __attribute__((packed)) usb_setup_info_t;

typedef struct {
  uint8_t b_length;
  uint8_t b_descriptor_type;
} __attribute__((packed)) usb_descriptor_header_t;

typedef struct {
  uint8_t b_length;
  uint8_t b_descriptor_type;  // DescriptorType::kDevice
  uint16_t bcd_usb;
  uint8_t b_device_class;
  uint8_t b_device_sub_class;
  uint8_t b_device_protocol;
  uint8_t b_max_packet_size0;
  uint16_t id_vendor;
  uint16_t id_product;
  uint16_t bcd_device;
  uint8_t i_manufacturer;
  uint8_t i_product;
  uint8_t i_serial_number;
  uint8_t b_num_configurations;
} __attribute__((packed)) usb_device_descriptor_info_t;

typedef struct {
  uint8_t b_length;
  uint8_t b_descriptor_type;  // DescriptorType::kConfiguration
  uint16_t w_total_length;
  uint8_t b_num_interfaces;
  uint8_t b_configuration_value;
  uint8_t i_configuration;
  uint8_t bm_attributes;
  uint8_t b_max_power;
} __attribute__((packed)) usb_configuration_descriptor_t;

typedef struct {
  uint8_t b_length;
  uint8_t b_descriptor_type;  // DescriptorType::kString
  uint8_t b_string[];
} __attribute__((packed)) usb_string_descriptor_t;

typedef struct {
  uint8_t b_length;
  uint8_t b_descriptor_type;  // DescriptorType::kInterface
  uint8_t b_interface_number;
  uint8_t b_alternate_setting;
  uint8_t b_num_endpoints;
  uint8_t b_interface_class;
  uint8_t b_interface_sub_class;
  uint8_t b_interface_protocol;
  uint8_t i_interface;
} __attribute__((packed)) usb_interface_info_descriptor_t;

typedef struct {
  uint8_t b_length;
  uint8_t b_descriptor_type;  // DescriptorType::kEndpoint
  uint8_t b_endpoint_address;
  uint8_t bm_attributes;
  uint16_t w_max_packet_size;
  uint8_t b_interval;
} __attribute__((packed)) usb_endpoint_info_descriptor_t;

typedef struct {
  uint8_t b_length;
  uint8_t b_descriptor_type;  // DescriptorType::kDeviceQualifier
  uint16_t bcd_usb;
  uint8_t b_device_class;
  uint8_t b_device_sub_class;
  uint8_t b_device_protocol;
  uint8_t b_max_packet_size0;
  uint8_t b_num_configurations;
  uint8_t b_reserved;
} __attribute__((packed)) usb_device_qualifier_descriptor_t;

typedef struct {
  uint8_t b_length;
  uint8_t b_descriptor_type;  // DescriptorType::kSsEpCompanion
  uint8_t b_max_burst;
  uint8_t bm_attributes;
  uint16_t w_bytes_per_interval;
} __attribute__((packed)) usb_ss_ep_comp_descriptor_info_t;
#define usb_ss_ep_comp_isoc_mult(ep) \
  ((ep)->bm_attributes & fuchsia_hardware_usb_descriptor::kSsEpCompIsochMultMask)
#define usb_ss_ep_comp_isoc_comp(ep) \
  (!!((ep)->bm_attributes & fuchsia_hardware_usb_descriptor::kSsEpCompIsochCompMask))

typedef struct {
  uint8_t b_length;
  uint8_t b_descriptor_type;  // DescriptorType::kSsIsochEpCompanion
  uint16_t w_reserved;
  uint32_t dw_bytes_per_interval;
} __attribute__((packed)) usb_ss_isoch_ep_comp_descriptor_t;

typedef struct {
  uint8_t b_length;
  uint8_t b_descriptor_type;  // DescriptorType::kInterfaceAssociation
  uint8_t b_first_interface;
  uint8_t b_interface_count;
  uint8_t b_function_class;
  uint8_t b_function_sub_class;
  uint8_t b_function_protocol;
  uint8_t i_function;
} __attribute__((packed)) usb_interface_assoc_descriptor_t;

typedef struct {
  uint8_t b_length;
  uint8_t b_descriptor_type;  // DescriptorType::kBos
  uint16_t w_total_length;
  uint8_t b_num_device_caps;
} __attribute__((packed)) usb_bos_descriptor_t;

typedef struct {
  uint8_t b_length;
  uint8_t b_descriptor_type;  // DescriptorType::kCsInterface
  uint8_t b_descriptor_sub_type;
} __attribute__((packed)) usb_cs_interface_descriptor_t;

typedef struct {
  uint8_t b_length;
  uint8_t b_descriptor_type;  // DescriptorType::kString
  uint16_t w_lang_ids[127];
} __attribute__((packed)) usb_langid_desc_t;

typedef struct {
  uint8_t b_length;
  uint8_t b_descriptor_type;  // DescriptorType::kString
  uint16_t code_points[127];
} __attribute__((packed)) usb_string_desc_t;

// Returns a host-endian uint16_t suitable for assignment to wire structures that undergo LE
// conversion.
template <typename E>
  requires std::is_integral_v<E> || IsDescriptorEnum<E>
inline constexpr uint16_t usb_descriptor_w_value(E type, uint8_t index = 0) {
  return static_cast<uint16_t>(
      (static_cast<uint16_t>(fuchsia_hardware_usb_descriptor::ToUnderlyingHelper(type) & 0xFF)
       << 8) |
      index);
}

inline constexpr fuchsia_hardware_usb_descriptor::DescriptorType usb_descriptor_type_from_w_value(
    uint16_t w_value) {
  return static_cast<fuchsia_hardware_usb_descriptor::DescriptorType>((w_value >> 8) & 0xFF);
}

inline constexpr uint8_t usb_descriptor_index_from_w_value(uint16_t w_value) {
  return static_cast<uint8_t>(w_value & 0xFF);
}

inline constexpr bool usb_request_is_in(uint8_t bm_request_type) {
  return (bm_request_type & fuchsia_hardware_usb_descriptor::kEndpointDirectionMask) ==
         fidl::ToUnderlying(fuchsia_hardware_usb_descriptor::EndpointDirection::kIn);
}

inline constexpr bool usb_request_is_out(uint8_t bm_request_type) {
  return (bm_request_type & fuchsia_hardware_usb_descriptor::kEndpointDirectionMask) ==
         fidl::ToUnderlying(fuchsia_hardware_usb_descriptor::EndpointDirection::kOut);
}

inline constexpr fuchsia_hardware_usb_descriptor::RequestType usb_request_type(
    uint8_t bm_request_type) {
  return static_cast<fuchsia_hardware_usb_descriptor::RequestType>(
      bm_request_type & fuchsia_hardware_usb_descriptor::kRequestTypeMask);
}

// Universal Setup Request Combination Constants
inline constexpr uint8_t kStandardDeviceIn =
    fuchsia_hardware_usb_descriptor::EndpointDirection::kIn |
    fuchsia_hardware_usb_descriptor::RequestType::kStandard |
    fuchsia_hardware_usb_descriptor::RequestRecipient::kDevice;
inline constexpr uint8_t kStandardDeviceOut =
    fuchsia_hardware_usb_descriptor::EndpointDirection::kOut |
    fuchsia_hardware_usb_descriptor::RequestType::kStandard |
    fuchsia_hardware_usb_descriptor::RequestRecipient::kDevice;
inline constexpr uint8_t kStandardInterfaceIn =
    fuchsia_hardware_usb_descriptor::EndpointDirection::kIn |
    fuchsia_hardware_usb_descriptor::RequestType::kStandard |
    fuchsia_hardware_usb_descriptor::RequestRecipient::kInterface;
inline constexpr uint8_t kStandardInterfaceOut =
    fuchsia_hardware_usb_descriptor::EndpointDirection::kOut |
    fuchsia_hardware_usb_descriptor::RequestType::kStandard |
    fuchsia_hardware_usb_descriptor::RequestRecipient::kInterface;
inline constexpr uint8_t kStandardEndpointIn =
    fuchsia_hardware_usb_descriptor::EndpointDirection::kIn |
    fuchsia_hardware_usb_descriptor::RequestType::kStandard |
    fuchsia_hardware_usb_descriptor::RequestRecipient::kEndpoint;
inline constexpr uint8_t kStandardEndpointOut =
    fuchsia_hardware_usb_descriptor::EndpointDirection::kOut |
    fuchsia_hardware_usb_descriptor::RequestType::kStandard |
    fuchsia_hardware_usb_descriptor::RequestRecipient::kEndpoint;

inline constexpr uint8_t kClassDeviceIn =
    fuchsia_hardware_usb_descriptor::EndpointDirection::kIn |
    fuchsia_hardware_usb_descriptor::RequestType::kClass |
    fuchsia_hardware_usb_descriptor::RequestRecipient::kDevice;
inline constexpr uint8_t kClassDeviceOut =
    fuchsia_hardware_usb_descriptor::EndpointDirection::kOut |
    fuchsia_hardware_usb_descriptor::RequestType::kClass |
    fuchsia_hardware_usb_descriptor::RequestRecipient::kDevice;
inline constexpr uint8_t kClassInterfaceIn =
    fuchsia_hardware_usb_descriptor::EndpointDirection::kIn |
    fuchsia_hardware_usb_descriptor::RequestType::kClass |
    fuchsia_hardware_usb_descriptor::RequestRecipient::kInterface;
inline constexpr uint8_t kClassInterfaceOut =
    fuchsia_hardware_usb_descriptor::EndpointDirection::kOut |
    fuchsia_hardware_usb_descriptor::RequestType::kClass |
    fuchsia_hardware_usb_descriptor::RequestRecipient::kInterface;
inline constexpr uint8_t kClassEndpointOut =
    fuchsia_hardware_usb_descriptor::EndpointDirection::kOut |
    fuchsia_hardware_usb_descriptor::RequestType::kClass |
    fuchsia_hardware_usb_descriptor::RequestRecipient::kEndpoint;
inline constexpr uint8_t kClassPortIn = fuchsia_hardware_usb_descriptor::EndpointDirection::kIn |
                                        fuchsia_hardware_usb_descriptor::RequestType::kClass |
                                        fuchsia_hardware_usb_descriptor::RequestRecipient::kOther;
inline constexpr uint8_t kClassPortOut = fuchsia_hardware_usb_descriptor::EndpointDirection::kOut |
                                         fuchsia_hardware_usb_descriptor::RequestType::kClass |
                                         fuchsia_hardware_usb_descriptor::RequestRecipient::kOther;

inline constexpr uint8_t kVendorDeviceIn =
    fuchsia_hardware_usb_descriptor::EndpointDirection::kIn |
    fuchsia_hardware_usb_descriptor::RequestType::kVendor |
    fuchsia_hardware_usb_descriptor::RequestRecipient::kDevice;
inline constexpr uint8_t kVendorDeviceOut =
    fuchsia_hardware_usb_descriptor::EndpointDirection::kOut |
    fuchsia_hardware_usb_descriptor::RequestType::kVendor |
    fuchsia_hardware_usb_descriptor::RequestRecipient::kDevice;

// Descriptor support functions.
namespace fuchsia_hardware_usb_descriptor_internal {

template <typename T>
concept HasEpAddress =
    std::is_integral_v<T> || requires(const T& ep) { ep.b_endpoint_address(); } ||
    requires(const T& ep) { ep->b_endpoint_address; } ||
    requires(const T& ep) { ep.b_endpoint_address; };

template <typename T>
concept HasEpAttributes = std::is_integral_v<T> || requires(const T& ep) {
  ep.bm_attributes();
} || requires(const T& ep) { ep->bm_attributes; } || requires(const T& ep) { ep.bm_attributes; };

template <typename T>
concept HasEpMaxPacketSize = requires(const T& ep) { ep.w_max_packet_size(); } ||
                             requires(const T& ep) { ep->w_max_packet_size; } ||
                             requires(const T& ep) { ep.w_max_packet_size; };

template <HasEpAddress T>
inline constexpr uint8_t ep_address(const T& ep) {
  if constexpr (std::is_integral_v<T>) {
    return static_cast<uint8_t>(ep);
  } else if constexpr (requires { ep.b_endpoint_address(); }) {
    return ep.b_endpoint_address();
  } else if constexpr (requires { ep->b_endpoint_address; }) {
    return ep->b_endpoint_address;
  } else {
    return ep.b_endpoint_address;
  }
}

template <HasEpAttributes T>
inline constexpr uint8_t ep_attributes(const T& ep) {
  if constexpr (std::is_integral_v<T>) {
    return static_cast<uint8_t>(ep);
  } else if constexpr (requires { ep.bm_attributes(); }) {
    return ep.bm_attributes();
  } else if constexpr (requires { ep->bm_attributes; }) {
    return ep->bm_attributes;
  } else {
    return ep.bm_attributes;
  }
}

template <HasEpMaxPacketSize T>
inline constexpr uint16_t ep_max_packet_raw(const T& ep) {
  if constexpr (requires { ep.w_max_packet_size(); }) {
    return ep.w_max_packet_size();
  } else if constexpr (requires { ep->w_max_packet_size; }) {
    return le16toh(ep->w_max_packet_size);
  } else {
    return le16toh(ep.w_max_packet_size);
  }
}

}  // namespace fuchsia_hardware_usb_descriptor_internal

template <fuchsia_hardware_usb_descriptor_internal::HasEpAddress T>
inline constexpr uint8_t usb_ep_num(const T& ep) {
  return fuchsia_hardware_usb_descriptor_internal::ep_address(ep) &
         fuchsia_hardware_usb_descriptor::kEndpointNumberMask;
}

template <fuchsia_hardware_usb_descriptor_internal::HasEpAddress T>
inline constexpr fuchsia_hardware_usb_descriptor::EndpointDirection usb_ep_direction(const T& ep) {
  return static_cast<fuchsia_hardware_usb_descriptor::EndpointDirection>(
      fuchsia_hardware_usb_descriptor_internal::ep_address(ep) &
      fuchsia_hardware_usb_descriptor::kEndpointDirectionMask);
}

template <fuchsia_hardware_usb_descriptor_internal::HasEpAddress T>
inline constexpr bool usb_ep_is_in(const T& ep) {
  return usb_ep_direction(ep) == fuchsia_hardware_usb_descriptor::EndpointDirection::kIn;
}

template <fuchsia_hardware_usb_descriptor_internal::HasEpAddress T>
inline constexpr bool usb_ep_is_out(const T& ep) {
  return usb_ep_direction(ep) == fuchsia_hardware_usb_descriptor::EndpointDirection::kOut;
}

template <fuchsia_hardware_usb_descriptor_internal::HasEpAttributes T>
inline constexpr fuchsia_hardware_usb_descriptor::EndpointType usb_ep_type(const T& ep) {
  return static_cast<fuchsia_hardware_usb_descriptor::EndpointType>(
      fuchsia_hardware_usb_descriptor_internal::ep_attributes(ep) &
      fuchsia_hardware_usb_descriptor::kEndpointTypeMask);
}

template <fuchsia_hardware_usb_descriptor_internal::HasEpAttributes T>
inline constexpr bool usb_ep_is_bulk(const T& ep) {
  return usb_ep_type(ep) == fuchsia_hardware_usb_descriptor::EndpointType::kBulk;
}

template <fuchsia_hardware_usb_descriptor_internal::HasEpAttributes T>
inline constexpr bool usb_ep_is_int(const T& ep) {
  return usb_ep_type(ep) == fuchsia_hardware_usb_descriptor::EndpointType::kInterrupt;
}

template <fuchsia_hardware_usb_descriptor_internal::HasEpAttributes T>
inline constexpr bool usb_ep_is_isoch(const T& ep) {
  return usb_ep_type(ep) == fuchsia_hardware_usb_descriptor::EndpointType::kIsochronous;
}

template <fuchsia_hardware_usb_descriptor_internal::HasEpAttributes T>
inline constexpr bool usb_ep_is_ctrl(const T& ep) {
  return usb_ep_type(ep) == fuchsia_hardware_usb_descriptor::EndpointType::kControl;
}

template <fuchsia_hardware_usb_descriptor_internal::HasEpAttributes T>
inline constexpr fuchsia_hardware_usb_descriptor::SynchronizationType usb_ep_sync_type(
    const T& ep) {
  return static_cast<fuchsia_hardware_usb_descriptor::SynchronizationType>(
      (fuchsia_hardware_usb_descriptor_internal::ep_attributes(ep) &
       fuchsia_hardware_usb_descriptor::kSynchronizationTypeMask) >>
      2);
}

template <fuchsia_hardware_usb_descriptor_internal::HasEpMaxPacketSize T>
inline constexpr uint16_t usb_ep_max_packet(const T& ep) {
  return fuchsia_hardware_usb_descriptor_internal::ep_max_packet_raw(ep) &
         fuchsia_hardware_usb_descriptor::kEndpointMaxPacketSizeMask;
}

static_assert(sizeof(usb_configuration_descriptor_t) == 9);
static_assert(sizeof(usb_device_qualifier_descriptor_t) == 10);
static_assert(sizeof(usb_bos_descriptor_t) == 5);
static_assert(sizeof(usb_setup_info_t) == 8);
static_assert(sizeof(usb_descriptor_header_t) == 2);
static_assert(sizeof(usb_device_descriptor_info_t) == 18);
static_assert(sizeof(usb_string_descriptor_t) == 2);
static_assert(sizeof(usb_interface_info_descriptor_t) == 9);
static_assert(sizeof(usb_endpoint_info_descriptor_t) == 7);
static_assert(sizeof(usb_ss_ep_comp_descriptor_info_t) == 6);
static_assert(sizeof(usb_ss_isoch_ep_comp_descriptor_t) == 8);
static_assert(sizeof(usb_interface_assoc_descriptor_t) == 8);
static_assert(sizeof(usb_cs_interface_descriptor_t) == 3);
static_assert(sizeof(usb_langid_desc_t) == 256);
static_assert(sizeof(usb_string_desc_t) == 256);

#endif  // SRC_DEVICES_USB_LIB_USB_INCLUDE_USB_DESCRIPTORS_H_
