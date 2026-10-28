// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! ARM SMMU operating mode string parser and validator.

#[cfg(any(ktest, not(is_kernel)))]
use core::ffi::{CStr, c_char};

use super::constants::ArmSmmuMode;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum ParseBehavior {
    /// Return `None` if any segment of the mode string is malformed.
    Validate,
    /// Attempt to parse segments and return a result even if some `addr=mode` segments are
    /// malformed.
    TolerateErrors,
}

#[inline]
const fn is_ascii_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

#[inline]
fn trim_ascii_space(mut bytes: &[u8]) -> &[u8] {
    while let Some((&first, rest)) = bytes.split_first() {
        if is_ascii_space(first) {
            bytes = rest;
        } else {
            break;
        }
    }
    while let Some((&last, rest)) = bytes.split_last() {
        if is_ascii_space(last) {
            bytes = rest;
        } else {
            break;
        }
    }
    bytes
}

#[inline]
fn parse_mode_token(token: &[u8]) -> Option<ArmSmmuMode> {
    if token.eq_ignore_ascii_case(b"disabled") {
        Some(ArmSmmuMode::Disabled)
    } else if token.eq_ignore_ascii_case(b"passthru") {
        Some(ArmSmmuMode::Passthru)
    } else if token.eq_ignore_ascii_case(b"enforced") {
        Some(ArmSmmuMode::Enforced)
    } else {
        None
    }
}

/// Parses an unsigned 64-bit integer from `token`, matching what the original C++ parser
/// (`smmu_mode.cc`) accepted via the kernel's `strtoul(ptr, &endptr, 0)`
/// (`//zircon/kernel/lib/libc/strtoul.cc`), and requiring the entire `token` to be consumed.
///
/// `token` must already have leading and trailing ASCII whitespace removed.
///
/// The accepted grammar deliberately reproduces the kernel `strtoul` behavior exactly, including
/// its quirks, so that a given `"kernel.arm-smmu-mode"` string selects the same mode as it did
/// before the driver was ported:
///
/// - An optional leading `+` or `-`. A `-` negates the result in two's complement (unless the
///   value saturated).
/// - A lowercase `0x` prefix selects hexadecimal. An uppercase `0X` prefix is *not* recognized:
///   the leading `0` selects octal and parsing stops at the `X`, so the token is rejected.
/// - Otherwise a leading `0` selects octal, and anything else decimal.
/// - Values which overflow 64 bits saturate to `u64::MAX` instead of being rejected.
/// - A sign and/or prefix with no digits after it (`+`, `-`, `0x`) parses as `0`.
///
/// Rust's `core` integer parsers (`u64::from_str_radix`) require an explicit radix and do not
/// implement any of the above, so the parse is implemented directly here.
fn parse_base_addr_token(token: &[u8]) -> Option<u64> {
    // Number of bytes of `token` consumed so far.
    let mut pos = 0usize;

    let mut neg = false;
    match token.first() {
        Some(b'+') => pos += 1,
        Some(b'-') => {
            neg = true;
            pos += 1;
        }
        _ => {}
    }

    let radix: u64 = if token.get(pos) == Some(&b'0') && token.get(pos + 1) == Some(&b'x') {
        pos += 2;
        16
    } else if token.get(pos) == Some(&b'0') {
        pos += 1;
        8
    } else {
        10
    };

    let mut ret: u64 = 0;
    while let Some(&b) = token.get(pos) {
        let digit = match b {
            b'0'..=b'9' => (b - b'0') as u64,
            b'a'..=b'z' => (b - b'a' + 10) as u64,
            b'A'..=b'Z' => (b - b'A' + 10) as u64,
            _ => break,
        };
        if digit >= radix {
            break;
        }

        // Saturate on overflow, and stay saturated once we get there.
        let new_ret = ret.wrapping_mul(radix);
        if new_ret / radix != ret || new_ret.wrapping_add(digit) < new_ret || ret == u64::MAX {
            ret = u64::MAX;
        } else {
            ret = new_ret + digit;
        }
        pos += 1;
    }

    // The C++ parser rejected the segment if `strtoul` consumed nothing at all (`endptr == ptr`),
    // or if anything other than whitespace remained between the end of the number and the `=`.
    if pos == 0 || pos != token.len() {
        return None;
    }

    if neg && ret != u64::MAX {
        ret = ret.wrapping_neg();
    }

    Some(ret)
}

fn parse_smmu_mode_common(
    mode_bytes: &[u8],
    base_addr: Option<u64>,
    behavior: ParseBehavior,
) -> Option<ArmSmmuMode> {
    // Only examine bytes up to the first NUL terminator if present.
    let bytes = match mode_bytes.iter().position(|&b| b == 0) {
        Some(nul_idx) => &mode_bytes[..nul_idx],
        None => mode_bytes,
    };

    // If no string is passed, or the string is empty, fail.
    if trim_ascii_space(bytes).is_empty() {
        return None;
    }

    // Find the default mode token and parse it. The default mode token is the
    // first token in the string, and is terminated by either a comma or the end
    // of the string.
    let mut segments = bytes.split(|&b| b == b',');
    let first_seg = segments.next()?;
    let default_mode = parse_mode_token(trim_ascii_space(first_seg))?;

    // If we are tolerating errors and don't have a base address to match,
    // we can just return the default mode immediately without parsing segments.
    if behavior == ParseBehavior::TolerateErrors && base_addr.is_none() {
        return Some(default_mode);
    }

    // Parse all of the base address / mode segments.
    for seg in segments {
        // Find the next comma (if any) and the equals sign in this segment. If the
        // equals sign is not found, or if the next comma occurs before the equals
        // sign, then this is not a valid segment.
        let Some(eq_pos) = seg.iter().position(|&b| b == b'=') else {
            if behavior == ParseBehavior::Validate {
                return None;
            }
            continue;
        };

        // Parse the base address token.
        // End of the address token must reach the '=' (after skipping whitespace) to ensure
        // clean separation of the address token and the mode token.
        let addr_token = trim_ascii_space(&seg[..eq_pos]);
        let Some(parsed_addr) = parse_base_addr_token(addr_token) else {
            if behavior == ParseBehavior::Validate {
                return None;
            }
            continue;
        };

        // Now attempt to parse the mode token.
        let mode_token = trim_ascii_space(&seg[(eq_pos + 1)..]);
        let mode_opt = parse_mode_token(mode_token);

        match behavior {
            ParseBehavior::Validate => {
                mode_opt?;
            }
            ParseBehavior::TolerateErrors => {
                debug_assert!(base_addr.is_some());
                if Some(parsed_addr) == base_addr {
                    return mode_opt;
                }
            }
        }

        // We didn't match this segment (or we are just validating), so move on to the next one.
    }

    Some(default_mode)
}

/// Parses `mode_bytes` and returns the effective [`ArmSmmuMode`] for `base_addr` (or the default
/// mode if `base_addr` is `None` or not overridden).
pub fn get_smmu_mode_bytes(mode_bytes: &[u8], base_addr: Option<u64>) -> Option<ArmSmmuMode> {
    parse_smmu_mode_common(mode_bytes, base_addr, ParseBehavior::TolerateErrors)
}

/// Determines the proper [`ArmSmmuMode`] if specified, or simply returns the default mode
/// otherwise.
///
/// Which mode to choose is determined by `mode_string`. It is a string with the following form,
/// given in a pseudo-regular expression notation:
///
/// `<mode>(,<base_addr>=<mode>)*`
///
/// `"<mode>"` is one of three tokens, `"disabled"`, `"passthru"` or `"enforced"`, corresponding to
/// each of the three defined [`ArmSmmuMode`] variants. The first mode listed is mandatory and
/// specifies the default operating mode. This is followed by a comma separated list of zero or
/// more modes for specific SMMU instances in the form `"<base_addr>=<mode>"`. `"<base_addr>"` is a
/// 64-bit unsigned integer identifying a specific instance of an SMMU for which the given mode
/// should be used.
///
/// When [`get_smmu_mode`] is called, a `base_addr` may optionally be passed. When no base address
/// is provided, [`get_smmu_mode`] returns the default operating mode. When a base address is
/// passed, [`get_smmu_mode`] returns the mode for the first optional entry which matches that base
/// address. If no entry is found which matches the base address, the default mode is returned
/// instead. If a chosen mode token string is invalid, `None` is returned so the caller can print a
/// warning and fall back to [`ArmSmmuMode::Disabled`].
pub fn get_smmu_mode(mode_string: &str, base_addr: Option<u64>) -> Option<ArmSmmuMode> {
    get_smmu_mode_bytes(mode_string.as_bytes(), base_addr)
}

/// Parses a NUL-terminated C string `mode_string` and returns the effective [`ArmSmmuMode`] for
/// `base_addr`. Returns `None` if `mode_string` is null.
///
/// # Safety
///
/// If non-null, `mode_string` must point to a valid NUL-terminated C string.
#[cfg(any(ktest, not(is_kernel)))]
pub unsafe fn get_smmu_mode_cstr(
    mode_string: *const c_char,
    base_addr: Option<u64>,
) -> Option<ArmSmmuMode> {
    if mode_string.is_null() {
        return None;
    }
    // SAFETY: Caller guarantees `mode_string` is a non-null, NUL-terminated C string.
    let bytes = unsafe { CStr::from_ptr(mode_string).to_bytes() };
    get_smmu_mode_bytes(bytes, base_addr)
}

/// Returns `true` if `mode_bytes` is a syntactically valid `"kernel.arm-smmu-mode"` specification.
pub fn validate_smmu_mode_bytes(mode_bytes: &[u8]) -> bool {
    parse_smmu_mode_common(mode_bytes, None, ParseBehavior::Validate).is_some()
}

/// Returns `true` if `mode_string` is a syntactically valid `"kernel.arm-smmu-mode"` specification.
pub fn validate_smmu_mode_string(mode_string: &str) -> bool {
    validate_smmu_mode_bytes(mode_string.as_bytes())
}

/// Returns `true` if the NUL-terminated C string `mode_string` is a syntactically valid
/// `"kernel.arm-smmu-mode"` specification.
///
/// # Safety
///
/// If non-null, `mode_string` must point to a valid NUL-terminated C string.
#[cfg(any(ktest, not(is_kernel)))]
pub unsafe fn validate_smmu_mode_cstr(mode_string: *const c_char) -> bool {
    if mode_string.is_null() {
        return false;
    }
    // SAFETY: Caller guarantees `mode_string` is a non-null, NUL-terminated C string.
    let bytes = unsafe { CStr::from_ptr(mode_string).to_bytes() };
    validate_smmu_mode_bytes(bytes)
}
