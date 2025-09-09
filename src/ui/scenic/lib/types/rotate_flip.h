// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_SCENIC_LIB_TYPES_ROTATE_FLIP_H_
#define SRC_UI_SCENIC_LIB_TYPES_ROTATE_FLIP_H_

#include <fidl/fuchsia.hardware.display.types/cpp/wire.h>
#include <fidl/fuchsia.ui.composition/cpp/fidl.h>
#include <lib/syslog/cpp/macros.h>

#include <cstdint>

#include "src/ui/scenic/lib/types/util/hash_combine.h"

namespace types {

// Covers the rotate/flip permutations supported by
// `fuchsia.hardware.display.types/CoordinateTransformation`.
class RotateFlip {
 public:
  enum class Enum : uint8_t {
    // Image pixels are passed through without any change.
    kIdentity = 0,

    // Image pixels are reflected across a line meeting the image's center, parallel to the X axis.
    //
    // This enum member's numeric value has a single bit set to 1. Any
    // transformation whose value has this bit set involves an X reflection.
    //
    // This transformation is also called an "X flip".
    //
    // Example:
    // |a b c d|      |i j k l|
    // |e f g h|  ->  |e f g h|
    // |i j k l|      |a b c d|
    kReflectX = 1,

    // Image pixels are reflected across a line meeting the image's center, parallel to the Y axis.
    //
    // This enum member's numeric value has a single bit set to 1. Any
    // transformation whose value has this bit set involves an Y reflection.
    //
    // This transformation is also called an "Y flip".
    //
    // Example:
    // |a b c d|      |d c b a|
    // |e f g h|  ->  |h g f e|
    // |i j k l|      |l k j i|
    kReflectY = 2,

    // TODO(https://fxbug.dev/356385730): Switch the convention for rotations
    // from CCW (counter-clockwise) to CW (clockwise).

    // Image pixels are rotated around the image's center counter-clockwise by 180 degrees.
    //
    // This is equivalent to applying the `REFLECT_X` and `REFLECT_Y`
    // transforms. `REFLECT_X` and `REFLECT_Y` are commutative, so their
    // ordering doesn't matter.
    //
    // Example:
    // |a b c d|      |l k j i|
    // |e f g h|  ->  |h g f e|
    // |i j k l|      |d c b a|
    kRotateCcw180 = 3,

    // Image pixels are rotated around the image's center counter-clockwise by 90 degrees.
    //
    // The image produced by this transformation has different dimensions from
    // the input image.
    //
    // This enum member's numeric value has a single bit set to 1. Any
    // transformation whose value has this bit set involves a 90-degree
    // counter-clockwise rotation.
    //
    // Example:
    // |a b c d|      |d h l|
    // |e f g h|  ->  |c g k|
    // |i j k l|      |b f j|
    //                |a e i|
    kRotateCcw90 = 4,

    // Image pixels are transformed using `ROTATE_CCW_90`, followed by `REFLECT_X`.
    //
    // The image produced by this transformation has different dimensions from
    // the input image.
    //
    // Example:
    // |a b c d|      |a e i|
    // |e f g h|  ->  |b f k|
    // |i j k l|      |c g k|
    //                |d h l|
    kRotateCcw90ReflectX = 5,

    // Image pixels are transformed using `ROTATE_CCW_90`, followed by `REFLECT_Y`.
    //
    // The image produced by this transformation has different dimensions from
    // the input image.
    //
    // Example:
    // |a b c d|      |l h d|
    // |e f g h|  ->  |k g c|
    // |i j k l|      |j f b|
    //                |i e a|
    kRotateCcw90ReflectY = 6,

    // Image pixels are rotated around the image's center counter-clockwise by 270 degrees.
    //
    // The image produced by this transformation has different dimensions from
    // the input image.
    //
    // This is equivalent to applying the `ROTATE_CCW_90` transform, followed
    // by `REFLECT_X` and `REFLECT_Y`. `REFLECT_X` and `REFLECT_Y` are
    // commutative, so their ordering doesn't matter.
    //
    // Example:
    // |a b c d|      |i e a|
    // |e f g h|  ->  |j f b|
    // |i j k l|      |k g c|
    //                |l h d|
    kRotateCcw270 = 7,
  };

  // Constructors.  All arguments must be valid; use `IsValid()` to validate if you're not sure.
  [[nodiscard]] static constexpr RotateFlip From(
      const fuchsia_ui_composition::Orientation& orientation,
      const fuchsia_ui_composition::ImageFlip& image_flip);
  [[nodiscard]] static constexpr RotateFlip From(
      const fuchsia_hardware_display_types::wire::CoordinateTransformation& fidl_mode);

  // Static "constructors".
  [[nodiscard]] static constexpr RotateFlip kIdentity();
  [[nodiscard]] static constexpr RotateFlip kReflectX();
  [[nodiscard]] static constexpr RotateFlip kReflectY();
  [[nodiscard]] static constexpr RotateFlip kRotateCcw180();
  [[nodiscard]] static constexpr RotateFlip kRotateCcw90();
  [[nodiscard]] static constexpr RotateFlip kRotateCcw90ReflectX();
  [[nodiscard]] static constexpr RotateFlip kRotateCcw90ReflectY();
  [[nodiscard]] static constexpr RotateFlip kRotateCcw270();

  RotateFlip() = delete;
  explicit constexpr RotateFlip(RotateFlip::Enum val);
  constexpr RotateFlip(const RotateFlip&) noexcept = default;
  constexpr RotateFlip(RotateFlip&&) noexcept = default;
  constexpr RotateFlip& operator=(const RotateFlip&) noexcept = default;
  constexpr RotateFlip& operator=(RotateFlip&&) noexcept = default;
  ~RotateFlip() = default;

  friend constexpr bool operator==(const RotateFlip& lhs, const RotateFlip& rhs);
  friend constexpr bool operator!=(const RotateFlip& lhs, const RotateFlip& rhs);

  constexpr fuchsia_hardware_display_types::wire::CoordinateTransformation
  ToDisplayCoordinateTransformation() const;

  // Used for hashing/printing/etc; not useful for general users.
  constexpr Enum enum_value() const { return val_; }

 private:
  Enum val_;
};

constexpr RotateFlip::RotateFlip(RotateFlip::Enum val) : val_(val) {}

// static
constexpr RotateFlip RotateFlip::From(const fuchsia_ui_composition::Orientation& orientation,
                                      const fuchsia_ui_composition::ImageFlip& image_flip) {
  using fuchsia_ui_composition::ImageFlip;
  using fuchsia_ui_composition::Orientation;

  // For flatland, image flips occur before any parent Transform geometric attributes (such as
  // rotation). However, for the display controller, the reflection specified in the Transform is
  // applied after rotation. The flatland transformations must be converted to the equivalent
  // display controller transform.
  switch (orientation) {
    case Orientation::kCcw0Degrees:
      switch (image_flip) {
        case ImageFlip::kNone:
          return RotateFlip::kIdentity();
        case ImageFlip::kLeftRight:
          return RotateFlip::kReflectY();
        case ImageFlip::kUpDown:
          return RotateFlip::kReflectX();
      }

    case Orientation::kCcw90Degrees:
      switch (image_flip) {
        case ImageFlip::kNone:
          return RotateFlip::kRotateCcw90();
        case ImageFlip::kLeftRight:
          // Left-right flip + 90Ccw is equivalent to 90Ccw + up-down flip.
          return RotateFlip::kRotateCcw90ReflectX();
        case ImageFlip::kUpDown:
          // Up-down flip + 90Ccw is equivalent to 90Ccw + left-right flip.
          return RotateFlip::kRotateCcw90ReflectY();
      }

    case Orientation::kCcw180Degrees:
      switch (image_flip) {
        case ImageFlip::kNone:
          return RotateFlip::kRotateCcw180();
        case ImageFlip::kLeftRight:
          // Left-right flip + 180 degree rotation is equivalent to up-down flip.
          return RotateFlip::kReflectX();
        case ImageFlip::kUpDown:
          // Up-down flip + 180 degree rotation is equivalent to left-right flip.
          return RotateFlip::kReflectY();
      }

    case Orientation::kCcw270Degrees:
      switch (image_flip) {
        case ImageFlip::kNone:
          return RotateFlip::kRotateCcw270();
        case ImageFlip::kLeftRight:
          // Left-right flip + 270Ccw is equivalent to 270Ccw + up-down flip, which in turn is
          // equivalent to 90Ccw + left-right flip.
          return RotateFlip::kRotateCcw90ReflectY();
        case ImageFlip::kUpDown:
          // Up-down flip + 270Ccw is equivalent to 270Ccw + left-right flip, which in turn is
          // equivalent to 90Ccw + up-down flip.
          return RotateFlip::kRotateCcw90ReflectX();
      }
  }

  FX_NOTREACHED();
}

// static
constexpr RotateFlip RotateFlip::From(
    const fuchsia_hardware_display_types::wire::CoordinateTransformation& coordinate_transform) {
  uint8_t val = static_cast<uint8_t>(coordinate_transform);
  return RotateFlip(RotateFlip::Enum(val));
}

// static
constexpr RotateFlip RotateFlip::kIdentity() { return RotateFlip(Enum::kIdentity); }

// static
constexpr RotateFlip RotateFlip::kReflectX() { return RotateFlip(Enum::kReflectX); }

// static
constexpr RotateFlip RotateFlip::kReflectY() { return RotateFlip(Enum::kReflectY); }

// static
constexpr RotateFlip RotateFlip::kRotateCcw180() { return RotateFlip(Enum::kRotateCcw180); }

// static
constexpr RotateFlip RotateFlip::kRotateCcw90() { return RotateFlip(Enum::kRotateCcw90); }

// static
constexpr RotateFlip RotateFlip::kRotateCcw90ReflectX() {
  return RotateFlip(Enum::kRotateCcw90ReflectX);
}

// static
constexpr RotateFlip RotateFlip::kRotateCcw90ReflectY() {
  return RotateFlip(Enum::kRotateCcw90ReflectY);
}

// static
constexpr RotateFlip RotateFlip::kRotateCcw270() { return RotateFlip(Enum::kRotateCcw270); }

constexpr bool operator==(const RotateFlip& lhs, const RotateFlip& rhs) {
  return lhs.val_ == rhs.val_;
}

constexpr bool operator!=(const RotateFlip& lhs, const RotateFlip& rhs) { return !(lhs == rhs); }

constexpr fuchsia_hardware_display_types::wire::CoordinateTransformation
RotateFlip::ToDisplayCoordinateTransformation() const {
  using WireCoordinateTransformation =
      fuchsia_hardware_display_types::wire::CoordinateTransformation;
  static_assert(static_cast<uint8_t>(RotateFlip::Enum::kIdentity) ==
                static_cast<uint8_t>(WireCoordinateTransformation::kIdentity));
  static_assert(static_cast<uint8_t>(RotateFlip::Enum::kReflectX) ==
                static_cast<uint8_t>(WireCoordinateTransformation::kReflectX));
  static_assert(static_cast<uint8_t>(RotateFlip::Enum::kReflectY) ==
                static_cast<uint8_t>(WireCoordinateTransformation::kReflectY));
  static_assert(static_cast<uint8_t>(RotateFlip::Enum::kRotateCcw180) ==
                static_cast<uint8_t>(WireCoordinateTransformation::kRotateCcw180));
  static_assert(static_cast<uint8_t>(RotateFlip::Enum::kRotateCcw90) ==
                static_cast<uint8_t>(WireCoordinateTransformation::kRotateCcw90));
  static_assert(static_cast<uint8_t>(RotateFlip::Enum::kRotateCcw90ReflectX) ==
                static_cast<uint8_t>(WireCoordinateTransformation::kRotateCcw90ReflectX));
  static_assert(static_cast<uint8_t>(RotateFlip::Enum::kRotateCcw90ReflectY) ==
                static_cast<uint8_t>(WireCoordinateTransformation::kRotateCcw90ReflectY));
  static_assert(static_cast<uint8_t>(RotateFlip::Enum::kRotateCcw270) ==
                static_cast<uint8_t>(WireCoordinateTransformation::kRotateCcw270));

  return static_cast<WireCoordinateTransformation>(static_cast<uint8_t>(val_));
}

inline std::ostream& operator<<(std::ostream& str, const RotateFlip& rf) {
  switch (rf.enum_value()) {
    case RotateFlip::Enum::kIdentity:
      str << "IDENTITY";
      break;
    case RotateFlip::Enum::kReflectX:
      str << "REFLECT_X";
      break;
    case RotateFlip::Enum::kReflectY:
      str << "REFLECT_Y";
      break;
    case RotateFlip::Enum::kRotateCcw180:
      str << "ROTATE_CCW_180";
      break;
    case RotateFlip::Enum::kRotateCcw90:
      str << "ROTATE_CCW_90";
      break;
    case RotateFlip::Enum::kRotateCcw90ReflectX:
      str << "ROTATE_CCW_90_REFLECT_X";
      break;
    case RotateFlip::Enum::kRotateCcw90ReflectY:
      str << "ROTATE_CCW_90_REFLECT_Y";
      break;
    case RotateFlip::Enum::kRotateCcw270:
      str << "ROTATE_CCW_270";
      break;
  }
  return str;
}

}  // namespace types

namespace std {

template <>
struct hash<types::RotateFlip> {
  std::size_t operator()(const types::RotateFlip& rf) const {
    // Random seed (`openssl rand -hex 8`) avoids collisions with types with the same memory layout.
    std::size_t seed = 0x0286902ad3d71bda;
    types::hash_combine(seed, rf.enum_value());
    return seed;
  }
};

}  // namespace std

#endif  // SRC_UI_SCENIC_LIB_TYPES_ROTATE_FLIP_H_
