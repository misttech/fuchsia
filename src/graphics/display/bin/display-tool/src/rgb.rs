// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::{fmt, str};

#[derive(Debug, PartialEq)]
pub enum ParseRgbError {
    UnexpectedCharacter,
    IncorrectSize(usize),
}

impl fmt::Display for ParseRgbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseRgbError::UnexpectedCharacter => write!(f, "Unexpected character"),
            ParseRgbError::IncorrectSize(size) => write!(f, "Incorrect size {}", size),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Rgb888 {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl str::FromStr for Rgb888 {
    type Err = ParseRgbError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if !s.chars().all(|x| x.is_ascii_hexdigit()) {
            return Err(ParseRgbError::UnexpectedCharacter);
        }
        if s.len() != 6 {
            return Err(ParseRgbError::IncorrectSize(s.len()));
        }
        Ok(Rgb888 {
            r: u8::from_str_radix(&s[0..2], 16).unwrap(),
            g: u8::from_str_radix(&s[2..4], 16).unwrap(),
            b: u8::from_str_radix(&s[4..6], 16).unwrap(),
        })
    }
}

/// Scales a color `channel` by `alpha`, rounding to the nearest integer.
const fn premultiply_channel(channel: u8, alpha: u8) -> u8 {
    // The quotient is at most 255, so the cast does not truncate.
    ((channel as u16 * alpha as u16 + 127) / 255) as u8
}

/// A 32-bit RGBA color with 8 bits per channel and straight (unpremultiplied) alpha.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Rgba8888 {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba8888 {
    /// Returns the pixel bytes in RGBA order (`[r, g, b, a]`).
    pub const fn to_bytes(self) -> [u8; 4] {
        [self.r, self.g, self.b, self.a]
    }
}

impl From<[u8; 4]> for Rgba8888 {
    fn from([r, g, b, a]: [u8; 4]) -> Self {
        Self { r, g, b, a }
    }
}

impl From<Rgba8888> for [u8; 4] {
    fn from(color: Rgba8888) -> Self {
        color.to_bytes()
    }
}

impl From<Rgb888> for Rgba8888 {
    fn from(color: Rgb888) -> Self {
        Self { r: color.r, g: color.g, b: color.b, a: u8::MAX }
    }
}

/// A 32-bit BGRA color with 8 bits per channel and straight (unpremultiplied) alpha.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Bgra8888 {
    pub b: u8,
    pub g: u8,
    pub r: u8,
    pub a: u8,
}

impl Bgra8888 {
    /// Returns the pixel bytes in BGRA order (`[b, g, r, a]`).
    pub const fn to_bytes(self) -> [u8; 4] {
        [self.b, self.g, self.r, self.a]
    }
}

impl From<[u8; 4]> for Bgra8888 {
    fn from([b, g, r, a]: [u8; 4]) -> Self {
        Self { b, g, r, a }
    }
}

impl From<Bgra8888> for [u8; 4] {
    fn from(color: Bgra8888) -> Self {
        color.to_bytes()
    }
}

impl From<Rgb888> for Bgra8888 {
    fn from(color: Rgb888) -> Self {
        Self { b: color.b, g: color.g, r: color.r, a: u8::MAX }
    }
}

impl From<Rgba8888> for Bgra8888 {
    fn from(color: Rgba8888) -> Self {
        Self { b: color.b, g: color.g, r: color.r, a: color.a }
    }
}

impl From<Bgra8888> for Rgba8888 {
    fn from(color: Bgra8888) -> Self {
        Self { r: color.r, g: color.g, b: color.b, a: color.a }
    }
}

/// A 32-bit RGBA color with 8 bits per channel and premultiplied alpha.
///
/// Every color channel (`r`, `g`, `b`) is guaranteed to be at most `a`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PremultipliedRgba8888 {
    r: u8,
    g: u8,
    b: u8,
    a: u8,
}

impl PremultipliedRgba8888 {
    /// Fully transparent black (`[0, 0, 0, 0]`).
    pub const TRANSPARENT: Self = Self { r: 0, g: 0, b: 0, a: 0 };

    pub const fn r(self) -> u8 {
        self.r
    }

    pub const fn g(self) -> u8 {
        self.g
    }

    pub const fn b(self) -> u8 {
        self.b
    }

    pub const fn a(self) -> u8 {
        self.a
    }

    /// Returns the pixel bytes in RGBA order (`[r, g, b, a]`).
    pub const fn to_bytes(self) -> [u8; 4] {
        [self.r, self.g, self.b, self.a]
    }
}

impl From<PremultipliedRgba8888> for [u8; 4] {
    fn from(color: PremultipliedRgba8888) -> Self {
        color.to_bytes()
    }
}

impl From<Rgba8888> for PremultipliedRgba8888 {
    fn from(color: Rgba8888) -> Self {
        Self {
            r: premultiply_channel(color.r, color.a),
            g: premultiply_channel(color.g, color.a),
            b: premultiply_channel(color.b, color.a),
            a: color.a,
        }
    }
}

impl From<Bgra8888> for PremultipliedRgba8888 {
    fn from(color: Bgra8888) -> Self {
        Self::from(Rgba8888::from(color))
    }
}

impl From<Rgb888> for PremultipliedRgba8888 {
    fn from(color: Rgb888) -> Self {
        Self { r: color.r, g: color.g, b: color.b, a: u8::MAX }
    }
}

/// A 32-bit BGRA color with 8 bits per channel and premultiplied alpha.
///
/// Every color channel (`b`, `g`, `r`) is guaranteed to be at most `a`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PremultipliedBgra8888 {
    b: u8,
    g: u8,
    r: u8,
    a: u8,
}

impl PremultipliedBgra8888 {
    /// Fully transparent black (`[0, 0, 0, 0]`).
    pub const TRANSPARENT: Self = Self { b: 0, g: 0, r: 0, a: 0 };

    pub const fn b(self) -> u8 {
        self.b
    }

    pub const fn g(self) -> u8 {
        self.g
    }

    pub const fn r(self) -> u8 {
        self.r
    }

    pub const fn a(self) -> u8 {
        self.a
    }

    /// Returns the pixel bytes in BGRA order (`[b, g, r, a]`).
    pub const fn to_bytes(self) -> [u8; 4] {
        [self.b, self.g, self.r, self.a]
    }
}

impl From<PremultipliedBgra8888> for [u8; 4] {
    fn from(color: PremultipliedBgra8888) -> Self {
        color.to_bytes()
    }
}

impl From<Bgra8888> for PremultipliedBgra8888 {
    fn from(color: Bgra8888) -> Self {
        Self {
            b: premultiply_channel(color.b, color.a),
            g: premultiply_channel(color.g, color.a),
            r: premultiply_channel(color.r, color.a),
            a: color.a,
        }
    }
}

impl From<Rgba8888> for PremultipliedBgra8888 {
    fn from(color: Rgba8888) -> Self {
        Self::from(Bgra8888::from(color))
    }
}

impl From<Rgb888> for PremultipliedBgra8888 {
    fn from(color: Rgb888) -> Self {
        Self { b: color.b, g: color.g, r: color.r, a: u8::MAX }
    }
}

impl From<PremultipliedRgba8888> for PremultipliedBgra8888 {
    fn from(color: PremultipliedRgba8888) -> Self {
        Self { b: color.b, g: color.g, r: color.r, a: color.a }
    }
}

impl From<PremultipliedBgra8888> for PremultipliedRgba8888 {
    fn from(color: PremultipliedBgra8888) -> Self {
        Self { r: color.r, g: color.g, b: color.b, a: color.a }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use googletest::{expect_eq, expect_that, gtest, matchers};
    use std::str::FromStr;

    #[gtest]
    #[fuchsia::test]
    fn rgb_from_str_invalid() {
        expect_that!(
            &Rgb888::from_str("zz00ff"),
            matchers::err(matchers::eq(&ParseRgbError::UnexpectedCharacter))
        );
        expect_that!(
            &Rgb888::from_str("0000vv"),
            matchers::err(matchers::eq(&ParseRgbError::UnexpectedCharacter))
        );
        expect_that!(
            &Rgb888::from_str("0x010101"),
            matchers::err(matchers::eq(&ParseRgbError::UnexpectedCharacter))
        );

        expect_that!(
            &Rgb888::from_str(""),
            matchers::err(matchers::eq(&ParseRgbError::IncorrectSize(0))),
        );
        expect_that!(
            &Rgb888::from_str("10101"),
            matchers::err(matchers::eq(&ParseRgbError::IncorrectSize(5)))
        );
        expect_that!(
            &Rgb888::from_str("1010111"),
            matchers::err(matchers::eq(&ParseRgbError::IncorrectSize(7)))
        );
    }

    #[gtest]
    #[fuchsia::test]
    fn rgb_from_str_valid() {
        expect_that!(
            &Rgb888::from_str("ef0000"),
            matchers::ok(matchers::eq(&Rgb888 { r: 0xef, g: 0x00, b: 0x00 }))
        );
        expect_that!(
            &Rgb888::from_str("00ab00"),
            matchers::ok(matchers::eq(&Rgb888 { r: 0x00, g: 0xab, b: 0x00 }))
        );
        expect_that!(
            &Rgb888::from_str("0000cd"),
            matchers::ok(matchers::eq(&Rgb888 { r: 0x00, g: 0x00, b: 0xcd }))
        );
        expect_that!(
            &Rgb888::from_str("012345"),
            matchers::ok(matchers::eq(&Rgb888 { r: 0x01, g: 0x23, b: 0x45 }))
        );
        expect_that!(
            &Rgb888::from_str("000000"),
            matchers::ok(matchers::eq(&Rgb888 { r: 0x00, g: 0x00, b: 0x00 }))
        );
        expect_that!(
            &Rgb888::from_str("ffffff"),
            matchers::ok(matchers::eq(&Rgb888 { r: 0xff, g: 0xff, b: 0xff }))
        );
    }

    #[gtest]
    #[fuchsia::test]
    fn rgb888_to_rgba8888_and_bgra8888() {
        let rgb = Rgb888 { r: 0x12, g: 0x34, b: 0x56 };
        let rgba = Rgba8888::from(rgb);
        let bgra = Bgra8888::from(rgb);

        expect_eq!(rgba, Rgba8888 { r: 0x12, g: 0x34, b: 0x56, a: 0xff });
        expect_eq!(rgba.to_bytes(), [0x12, 0x34, 0x56, 0xff]);
        expect_eq!(bgra, Bgra8888 { b: 0x56, g: 0x34, r: 0x12, a: 0xff });
        expect_eq!(bgra.to_bytes(), [0x56, 0x34, 0x12, 0xff]);
        expect_eq!(Bgra8888::from(rgba), bgra);
        expect_eq!(Rgba8888::from(bgra), rgba);
    }

    #[gtest]
    #[fuchsia::test]
    fn premultiply_bgra8888_scales_color_channels_by_alpha() {
        expect_eq!(
            PremultipliedBgra8888::from(Bgra8888 { b: 100, g: 255, r: 0, a: 150 }).to_bytes(),
            [59, 150, 0, 150]
        );
        expect_eq!(
            PremultipliedBgra8888::from(Bgra8888 { b: 128, g: 128, r: 128, a: 150 }).to_bytes(),
            [75, 75, 75, 150]
        );
        expect_eq!(
            PremultipliedBgra8888::from(Bgra8888 { b: 255, g: 100, r: 0, a: 255 }).to_bytes(),
            [255, 100, 0, 255]
        );
        expect_eq!(
            PremultipliedBgra8888::from(Bgra8888 { b: 255, g: 255, r: 255, a: 0 }).to_bytes(),
            [0, 0, 0, 0]
        );
    }

    #[gtest]
    #[fuchsia::test]
    fn premultiply_rgba8888_and_cross_format_conversions() {
        let rgba = Rgba8888 { r: 255, g: 100, b: 50, a: 150 };
        let bgra = Bgra8888::from(rgba);

        let premul_rgba = PremultipliedRgba8888::from(rgba);
        let premul_bgra = PremultipliedBgra8888::from(bgra);

        expect_eq!(premul_rgba.to_bytes(), [150, 59, 29, 150]);
        expect_eq!(premul_bgra.to_bytes(), [29, 59, 150, 150]);
        expect_eq!(PremultipliedBgra8888::from(rgba), premul_bgra);
        expect_eq!(PremultipliedRgba8888::from(bgra), premul_rgba);
        expect_eq!(PremultipliedBgra8888::from(premul_rgba), premul_bgra);
        expect_eq!(PremultipliedRgba8888::from(premul_bgra), premul_rgba);
    }

    #[gtest]
    #[fuchsia::test]
    fn premultiplied_from_rgb888_is_opaque() {
        let rgb = Rgb888 { r: 10, g: 20, b: 30 };
        expect_eq!(PremultipliedRgba8888::from(rgb).to_bytes(), [10, 20, 30, 255]);
        expect_eq!(PremultipliedBgra8888::from(rgb).to_bytes(), [30, 20, 10, 255]);
    }
}
