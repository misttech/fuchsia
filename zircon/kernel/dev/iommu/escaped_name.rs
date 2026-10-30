// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Escaping [`core::fmt::Display`] adapter for kernel object names printed in IOMMU diagnostics.
//!
//! This file is mounted both by the kernel `iommu` module and (via `#[path]`) by the standalone
//! ARM SMMU host test crate, so it must only depend on `core`.

use core::fmt;

/// Displays a kernel object name (`ZX_PROP_NAME`), escaping any bytes which are not valid UTF-8.
///
/// Object names are arbitrary user-controlled bytes: the kernel does not validate them as UTF-8,
/// and truncating a name to `ZX_MAX_NAME_LEN - 1` bytes can split a multi-byte character. Valid
/// UTF-8 is printed as is, and each byte of an invalid sequence is printed as `\xNN`.
///
/// Because Rust formatting requires valid UTF-8, bytes that cannot be printed verbatim are
/// hex-escaped rather than dropping the entire name.
#[derive(Clone, Copy, Debug)]
pub struct EscapedName<'a>(&'a [u8]);

impl<'a> EscapedName<'a> {
    /// Wraps `name`, which is taken to end at its first NUL byte (if any).
    #[inline]
    pub fn new(name: &'a [u8]) -> Self {
        let end = name.iter().position(|&b| b == 0).unwrap_or(name.len());
        Self(&name[..end])
    }

    /// Like [`Self::new`], but substitutes `<unknown>` for an empty name.
    #[inline]
    pub fn new_or_unknown(name: &'a [u8]) -> Self {
        let this = Self::new(name);
        if this.is_empty() { Self(b"<unknown>") } else { this }
    }

    /// Returns `true` if the name has no bytes before its first NUL.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns the raw bytes of the name, up to but not including its first NUL.
    #[inline]
    pub fn as_bytes(&self) -> &'a [u8] {
        self.0
    }
}

impl fmt::Display for EscapedName<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for chunk in self.0.utf8_chunks() {
            f.write_str(chunk.valid())?;
            for &b in chunk.invalid() {
                write!(f, "\\x{:02x}", b)?;
            }
        }
        Ok(())
    }
}
