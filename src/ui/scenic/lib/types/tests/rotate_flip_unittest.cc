// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/types/rotate_flip.h"

#include <array>

#include <gtest/gtest.h>

namespace types {

namespace {

using fuchsia_hardware_display_types::wire::CoordinateTransformation;
using fuchsia_ui_composition::ImageFlip;
using fuchsia_ui_composition::Orientation;

TEST(RotateFlipTest, Equality) {
  // Reflexive property.
  EXPECT_EQ(RotateFlip::kIdentity(), RotateFlip::kIdentity());

  // Symmetric property.
  EXPECT_EQ(RotateFlip::kIdentity(), RotateFlip(RotateFlip::Enum::kIdentity));
  EXPECT_EQ(RotateFlip(RotateFlip::Enum::kIdentity), RotateFlip::kIdentity());

  // Transitive property.
  EXPECT_NE(RotateFlip::kIdentity(), RotateFlip::kReflectX());
  EXPECT_NE(RotateFlip(RotateFlip::Enum::kIdentity), RotateFlip::kReflectX());
}

TEST(RotateFlipTest, FromOrientationAndImageFlip) {
  // CCW_0_DEGREES
  EXPECT_EQ(RotateFlip::From(Orientation::kCcw0Degrees, ImageFlip::kNone), RotateFlip::kIdentity());
  EXPECT_EQ(RotateFlip::From(Orientation::kCcw0Degrees, ImageFlip::kLeftRight),
            RotateFlip::kReflectY());
  EXPECT_EQ(RotateFlip::From(Orientation::kCcw0Degrees, ImageFlip::kUpDown),
            RotateFlip::kReflectX());

  // CCW_90_DEGREES
  EXPECT_EQ(RotateFlip::From(Orientation::kCcw90Degrees, ImageFlip::kNone),
            RotateFlip::kRotateCcw90());
  EXPECT_EQ(RotateFlip::From(Orientation::kCcw90Degrees, ImageFlip::kLeftRight),
            RotateFlip::kRotateCcw90ReflectX());
  EXPECT_EQ(RotateFlip::From(Orientation::kCcw90Degrees, ImageFlip::kUpDown),
            RotateFlip::kRotateCcw90ReflectY());

  // CCW_180_DEGREES
  EXPECT_EQ(RotateFlip::From(Orientation::kCcw180Degrees, ImageFlip::kNone),
            RotateFlip::kRotateCcw180());
  EXPECT_EQ(RotateFlip::From(Orientation::kCcw180Degrees, ImageFlip::kLeftRight),
            RotateFlip::kReflectX());
  EXPECT_EQ(RotateFlip::From(Orientation::kCcw180Degrees, ImageFlip::kUpDown),
            RotateFlip::kReflectY());

  // CCW_270_DEGREES
  EXPECT_EQ(RotateFlip::From(Orientation::kCcw270Degrees, ImageFlip::kNone),
            RotateFlip::kRotateCcw270());
  EXPECT_EQ(RotateFlip::From(Orientation::kCcw270Degrees, ImageFlip::kLeftRight),
            RotateFlip::kRotateCcw90ReflectY());
  EXPECT_EQ(RotateFlip::From(Orientation::kCcw270Degrees, ImageFlip::kUpDown),
            RotateFlip::kRotateCcw90ReflectX());
}

TEST(RotateFlipTest, FromDisplayCoordinateTransformation) {
  EXPECT_EQ(RotateFlip::From(CoordinateTransformation::kIdentity), RotateFlip::kIdentity());
  EXPECT_EQ(RotateFlip::From(CoordinateTransformation::kReflectX), RotateFlip::kReflectX());
  EXPECT_EQ(RotateFlip::From(CoordinateTransformation::kReflectY), RotateFlip::kReflectY());
  EXPECT_EQ(RotateFlip::From(CoordinateTransformation::kRotateCcw180), RotateFlip::kRotateCcw180());
  EXPECT_EQ(RotateFlip::From(CoordinateTransformation::kRotateCcw90), RotateFlip::kRotateCcw90());
  EXPECT_EQ(RotateFlip::From(CoordinateTransformation::kRotateCcw90ReflectX),
            RotateFlip::kRotateCcw90ReflectX());
  EXPECT_EQ(RotateFlip::From(CoordinateTransformation::kRotateCcw90ReflectY),
            RotateFlip::kRotateCcw90ReflectY());
  EXPECT_EQ(RotateFlip::From(CoordinateTransformation::kRotateCcw270), RotateFlip::kRotateCcw270());
}

TEST(RotateFlipTest, ToDisplayCoordinateTransformation) {
  EXPECT_EQ(RotateFlip::kIdentity().ToDisplayCoordinateTransformation(),
            CoordinateTransformation::kIdentity);
  EXPECT_EQ(RotateFlip::kReflectX().ToDisplayCoordinateTransformation(),
            CoordinateTransformation::kReflectX);
  EXPECT_EQ(RotateFlip::kReflectY().ToDisplayCoordinateTransformation(),
            CoordinateTransformation::kReflectY);
  EXPECT_EQ(RotateFlip::kRotateCcw180().ToDisplayCoordinateTransformation(),
            CoordinateTransformation::kRotateCcw180);
  EXPECT_EQ(RotateFlip::kRotateCcw90().ToDisplayCoordinateTransformation(),
            CoordinateTransformation::kRotateCcw90);
  EXPECT_EQ(RotateFlip::kRotateCcw90ReflectX().ToDisplayCoordinateTransformation(),
            CoordinateTransformation::kRotateCcw90ReflectX);
  EXPECT_EQ(RotateFlip::kRotateCcw90ReflectY().ToDisplayCoordinateTransformation(),
            CoordinateTransformation::kRotateCcw90ReflectY);
  EXPECT_EQ(RotateFlip::kRotateCcw270().ToDisplayCoordinateTransformation(),
            CoordinateTransformation::kRotateCcw270);
}

TEST(RotateFlipTest, Accessors) {
  EXPECT_EQ(RotateFlip::kIdentity().enum_value(), RotateFlip::Enum::kIdentity);
  EXPECT_EQ(RotateFlip::kReflectX().enum_value(), RotateFlip::Enum::kReflectX);
  EXPECT_EQ(RotateFlip::kReflectY().enum_value(), RotateFlip::Enum::kReflectY);
  EXPECT_EQ(RotateFlip::kRotateCcw180().enum_value(), RotateFlip::Enum::kRotateCcw180);
  EXPECT_EQ(RotateFlip::kRotateCcw90().enum_value(), RotateFlip::Enum::kRotateCcw90);
  EXPECT_EQ(RotateFlip::kRotateCcw90ReflectX().enum_value(),
            RotateFlip::Enum::kRotateCcw90ReflectX);
  EXPECT_EQ(RotateFlip::kRotateCcw90ReflectY().enum_value(),
            RotateFlip::Enum::kRotateCcw90ReflectY);
  EXPECT_EQ(RotateFlip::kRotateCcw270().enum_value(), RotateFlip::Enum::kRotateCcw270);
}

TEST(RotateFlipTest, Hash) {
  const std::hash<RotateFlip> hasher;
  EXPECT_EQ(hasher(RotateFlip::kIdentity()), hasher(RotateFlip(RotateFlip::Enum::kIdentity)));
  EXPECT_NE(hasher(RotateFlip::kIdentity()), hasher(RotateFlip::kReflectX()));
  EXPECT_EQ(hasher(RotateFlip::kReflectX()), hasher(RotateFlip(RotateFlip::Enum::kReflectX)));
}

TEST(RotateFlipTest, RotatedBy) {
  constexpr std::array kOrientations = {Orientation::kCcw0Degrees, Orientation::kCcw90Degrees,
                                        Orientation::kCcw180Degrees, Orientation::kCcw270Degrees};
  constexpr std::array kFlips = {ImageFlip::kNone, ImageFlip::kLeftRight, ImageFlip::kUpDown};
  constexpr std::array kRotations = {RotateFlip::Enum::kIdentity, RotateFlip::Enum::kRotateCcw90,
                                     RotateFlip::Enum::kRotateCcw180,
                                     RotateFlip::Enum::kRotateCcw270};

  // RotatedBy post-composes a rotation. From(o, f) builds its transform as
  // flip-then-rotation (see its comment), and in that form a post-composed
  // rotation merges into the rotation factor: the flip stays put and the
  // quarter turns add. So From(o, f).RotatedBy(r) must equal From(o + r, f),
  // an expectation independent of the lookup table inside RotatedBy. The
  // arrays above are ordered by quarter-turn count, so the indices o and r
  // act as turn counts and the sum wraps mod 4.
  for (size_t o = 0; o < kOrientations.size(); ++o) {
    for (const ImageFlip flip : kFlips) {
      for (size_t r = 0; r < kRotations.size(); ++r) {
        const RotateFlip original = RotateFlip::From(kOrientations[o], flip);
        const RotateFlip expected = RotateFlip::From(kOrientations[(o + r) % 4], flip);
        EXPECT_EQ(original.RotatedBy(kRotations[r]), expected);
        EXPECT_EQ(original.RotatedBy(RotateFlip(kRotations[r])), expected);
      }
    }
  }
}

TEST(RotateFlipTest, RotatedByRejectsReflectionOperand) {
  constexpr std::array kReflections = {RotateFlip::Enum::kReflectX, RotateFlip::Enum::kReflectY,
                                       RotateFlip::Enum::kRotateCcw90ReflectX,
                                       RotateFlip::Enum::kRotateCcw90ReflectY};
  for (const RotateFlip::Enum reflection : kReflections) {
    EXPECT_DEATH((void)RotateFlip::kIdentity().RotatedBy(reflection), "pure rotation");
  }
}

}  // namespace
}  // namespace types
