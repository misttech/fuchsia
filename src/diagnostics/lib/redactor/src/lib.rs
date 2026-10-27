// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Shared `#![no_std]` in-process redaction and hashing utilities for network
//! identifiers (MAC addresses, BSSIDs, and SSIDs).
//!
//! This crate is `#![no_std]` so that platform-agnostic base crates such as
//! `net-types` and `ieee80211` can implement [`Redactable`] directly and share
//! a single process-global [`Redactor`] state machine.
//!
//! Responsibilities are split in two:
//! - A [`Redactor`] only *obscures data*: it maps the 6 octets of a MAC/BSSID
//!   to 6 octets safe to display, and an SSID to a fixed-size digest. It never
//!   decides how those bytes are rendered.
//! - A [`Redactable`] type owns its string representation. It provides its
//!   raw formatting in [`Redactable::fmt_unredacted`] and formats the bytes
//!   returned by a [`Redactor`] in [`Redactable::fmt_redacted`]; its
//!   `Display`/`Debug` impls (see [`impl_display_via_redactable!`]) call
//!   [`Redactable::fmt_dispatch`], which picks one depending on whether a
//!   global redactor has been installed.
//!
//! By default (such as on `eng` builds, in unit tests, and in host tools), no
//! global redactor is installed and `fmt_dispatch` writes the raw value.
//!
//! When initialized at component startup on `user` and `userdebug` builds:
//! - [`set_masking_redactor`] synchronously installs [`MaskingRedactor`], which
//!   preserves the first 3 OUI octets of a MAC and zeroes everything else,
//!   before the per-boot salt arrives.
//! - [`set_salted_redactor`] transitions the global redactor to
//!   [`SaltedRedactor`], which preserves the first 3 OUI octets of a MAC/BSSID
//!   and replaces the last 3 octets (and the SSID) with a salted SHA-256
//!   digest keyed by a 32-byte per-boot random salt.

#![deny(missing_docs)]
#![cfg_attr(not(test), no_std)]

use core::fmt;
use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use sha2::{Digest as _, Sha256};

/// Length in bytes of the per-boot random salt.
pub const SALT_LEN: usize = 32;

/// Length in bytes of an IEEE 802.3 / 802.11 MAC address or BSSID.
pub const MAC_LEN: usize = 6;

/// Length in bytes of the digest a [`Redactor`] produces for an SSID.
pub const SSID_DIGEST_LEN: usize = 8;

/// Suffix that [`Redactable::fmt_redacted`] implementations append so a
/// redacted identifier is distinguishable at a glance from a raw one (e.g.
/// `00:1a:2b:9f:3e:c1*`). Shared so every crate emits the same marker and
/// cross-component correlation keeps working.
pub const REDACTED_MARKER: &str = "*";

/// Domain separation tag for MAC/BSSID hashing.
const MAC_DOMAIN_TAG: &[u8] = b"fuchsia-diagnostics-redact-mac-v1";

/// Domain separation tag for SSID hashing.
const SSID_DOMAIN_TAG: &[u8] = b"fuchsia-diagnostics-redact-ssid-v1";

/// States of the global redactor state machine, modeled after
/// `log::set_logger`.
///
/// `State` knows nothing about where it is stored; see [`redactor_state`],
/// [`set_redactor_state`] and [`maybe_change_redactor_state`] for the global.
#[repr(usize)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// No redactor installed; `fmt_dispatch` writes raw values.
    Uninitialized = 0,
    /// A writer holds exclusive access to the globals; readers mask.
    Initializing = 1,
    /// `MaskingRedactor` is active while the salt is being fetched.
    Masking = 2,
    /// A final redactor (`SaltedRedactor` or custom) is installed.
    Salted = 3,
}

impl TryFrom<usize> for State {
    type Error = usize;

    fn try_from(value: usize) -> Result<Self, usize> {
        match value {
            0 => Ok(State::Uninitialized),
            1 => Ok(State::Initializing),
            2 => Ok(State::Masking),
            3 => Ok(State::Salted),
            other => Err(other),
        }
    }
}

/// Backing storage for the global [`State`]. Only ever holds a `State`
/// discriminant; a `usize` so transitions can be done with atomic
/// compare-and-swap in `no_std`.
static REDACTOR_STATE: AtomicUsize = AtomicUsize::new(State::Uninitialized as usize);

/// Reads the global [`State`] with `Acquire` ordering, which pairs with the
/// `Release` in [`set_redactor_state`] so that anything published before a
/// state change is visible to readers that observe it.
fn redactor_state() -> State {
    State::try_from(REDACTOR_STATE.load(Ordering::Acquire)).expect("invalid redactor state")
}

/// Unconditionally publishes `state` with `Release` ordering.
///
/// Only call this from the writer that currently holds `State::Initializing`.
fn set_redactor_state(state: State) {
    REDACTOR_STATE.store(state as usize, Ordering::Release);
}

/// Atomically moves the global [`State`] from `expected` to `target`.
/// Returns `false` if the state was not `expected`.
fn maybe_change_redactor_state(expected: State, target: State) -> bool {
    REDACTOR_STATE
        .compare_exchange(expected as usize, target as usize, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
}
/// The per-boot salt, valid once the state is `Salted`.
///
/// Written byte-by-byte by the single writer that holds `Initializing` before
/// it publishes `Salted` with `Release`; read with `Relaxed` after an
/// `Acquire` load of the state observed `Salted`, so no `unsafe` or `static
/// mut` is needed.
static GLOBAL_SALT: [AtomicU8; SALT_LEN] = [const { AtomicU8::new(0) }; SALT_LEN];

/// Strategy for obscuring MAC addresses, BSSIDs, and SSIDs.
///
/// A redactor transforms data; it does not format it. The [`Redactable`] type
/// that owns the value decides how the returned bytes are rendered.
pub trait Redactor: Send + Sync {
    /// Returns the 6 octets that may be displayed in place of `octets`.
    ///
    /// Implementations preserve the first 3 octets (the hardware vendor OUI
    /// prefix) so vendor debugging context survives redaction.
    fn redact_mac(&self, octets: [u8; MAC_LEN]) -> [u8; MAC_LEN];

    /// Returns a fixed-size digest that may be displayed in place of `ssid`.
    fn redact_ssid(&self, ssid: &[u8]) -> [u8; SSID_DIGEST_LEN];
}

/// A [`Redactor`] that preserves the first 3 OUI vendor octets of a MAC and
/// zeroes the remaining 3 octets, and zeroes SSID digests. Used in the window
/// between component startup and the per-boot salt becoming available.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MaskingRedactor;

impl Redactor for MaskingRedactor {
    fn redact_mac(&self, octets: [u8; MAC_LEN]) -> [u8; MAC_LEN] {
        [octets[0], octets[1], octets[2], 0, 0, 0]
    }

    fn redact_ssid(&self, _ssid: &[u8]) -> [u8; SSID_DIGEST_LEN] {
        [0; SSID_DIGEST_LEN]
    }
}

/// A [`Redactor`] that hashes identifiers using a 32-byte per-boot secret salt.
///
/// For MAC addresses and BSSIDs, the first 3 octets (hardware vendor OUI
/// prefix) are preserved and the last 3 octets are replaced with the first
/// 3 octets of the salted SHA-256 digest of the full 6-octet address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SaltedRedactor {
    salt: [u8; SALT_LEN],
}

impl SaltedRedactor {
    /// Creates a new [`SaltedRedactor`] with the given 32-byte `salt`.
    pub const fn new(salt: [u8; SALT_LEN]) -> Self {
        Self { salt }
    }

    /// Returns a reference to the 32-byte salt.
    pub const fn salt(&self) -> &[u8; SALT_LEN] {
        &self.salt
    }
}

impl Redactor for SaltedRedactor {
    fn redact_mac(&self, octets: [u8; MAC_LEN]) -> [u8; MAC_LEN] {
        let mut hasher = Sha256::new();
        hasher.update(MAC_DOMAIN_TAG);
        hasher.update(self.salt);
        hasher.update(octets);
        let digest = hasher.finalize();
        [octets[0], octets[1], octets[2], digest[0], digest[1], digest[2]]
    }

    fn redact_ssid(&self, ssid: &[u8]) -> [u8; SSID_DIGEST_LEN] {
        let mut hasher = Sha256::new();
        hasher.update(SSID_DOMAIN_TAG);
        hasher.update(self.salt);
        hasher.update(ssid);
        let digest = hasher.finalize();
        let mut out = [0; SSID_DIGEST_LEN];
        out.copy_from_slice(&digest[..SSID_DIGEST_LEN]);
        out
    }
}

/// Trait implemented by identifier types (such as MAC addresses, BSSIDs, and
/// SSIDs) whose `Display` and `Debug` representations support redaction via the
/// global [`Redactor`].
///
/// Implementors provide the raw formatting in [`Redactable::fmt_unredacted`]
/// and the redacted formatting in [`Redactable::fmt_redacted`]; their
/// `Display`/`Debug` impls should call [`Redactable::fmt_dispatch`], which
/// picks one based on whether a global redactor has been installed.
pub trait Redactable {
    /// Formats the raw, unredacted value of `self`.
    ///
    /// Used when no global redactor is installed (e.g., on `eng` builds, in
    /// unit tests, and in host tools).
    fn fmt_unredacted(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result;

    /// Formats `self` after obscuring it with the provided [`Redactor`].
    ///
    /// Implementations should pass their data through [`Redactor::redact_mac`]
    /// or [`Redactor::redact_ssid`], render the result the same way
    /// [`Redactable::fmt_unredacted`] renders the raw data, and append
    /// [`REDACTED_MARKER`].
    fn fmt_redacted(&self, redactor: &dyn Redactor, f: &mut fmt::Formatter<'_>) -> fmt::Result;

    /// Formats `self` through the active global [`Redactor`] if one has been
    /// installed, or via [`Redactable::fmt_unredacted`] otherwise.
    fn fmt_dispatch(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match redactor_state() {
            State::Uninitialized => self.fmt_unredacted(f),
            State::Masking | State::Initializing => self.fmt_redacted(&MaskingRedactor, f),
            State::Salted => self.fmt_redacted(&SaltedRedactor::new(load_salt()), f),
        }
    }
}

/// A `Display` adapter that always formats a [`Redactable`] value in its raw,
/// unredacted form, bypassing the global redactor.
///
/// Intended for the rare places where the string is *data* rather than
/// diagnostics (e.g. serialization to persistent storage or a wire format)
/// and must round-trip. Do not use it for logs or Inspect.
pub struct Unredacted<'a, T: ?Sized>(pub &'a T);

impl<T: Redactable + ?Sized> fmt::Display for Unredacted<'_, T> {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt_unredacted(f)
    }
}

/// Error returned when attempting to install a final global [`Redactor`] after
/// one has already been installed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SetRedactorError;

impl fmt::Display for SetRedactorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "a global redactor has already been installed")
    }
}

impl core::error::Error for SetRedactorError {}

/// Synchronously transitions the global redactor from `Uninitialized` to
/// [`MaskingRedactor`] while the per-boot salt is being fetched asynchronously.
///
/// If a final redactor has already been installed, this is a no-op.
pub fn set_masking_redactor() {
    let _ = maybe_change_redactor_state(State::Uninitialized, State::Masking);
}

/// Installs a [`SaltedRedactor`] with the given `salt` as the global redactor.
///
/// Supports transitioning from either `Uninitialized` or `Masking` (after
/// [`set_masking_redactor`]).
///
/// # Errors
///
/// Returns [`SetRedactorError`] if a salted redactor has already been
/// installed.
pub fn set_salted_redactor(salt: [u8; SALT_LEN]) -> Result<(), SetRedactorError> {
    loop {
        let current = redactor_state();
        if current != State::Uninitialized && current != State::Masking {
            return Err(SetRedactorError);
        }
        if maybe_change_redactor_state(current, State::Initializing) {
            // We are the only writer while the state is `Initializing`;
            // readers mask in the meantime and only read `GLOBAL_SALT` after
            // observing `Salted` (published with `Release` below).
            for (dst, src) in GLOBAL_SALT.iter().zip(salt) {
                dst.store(src, Ordering::Relaxed);
            }
            set_redactor_state(State::Salted);
            return Ok(());
        }
    }
}

/// Reads the salt published by [`set_salted_redactor`]. Only meaningful after
/// [`redactor_state`] returned `Salted`, whose `Acquire` load pairs with the
/// `Release` store that published the salt.
fn load_salt() -> [u8; SALT_LEN] {
    core::array::from_fn(|i| GLOBAL_SALT[i].load(Ordering::Relaxed))
}

/// Implements `core::fmt::Display` for a type that implements [`Redactable`]
/// by delegating to [`Redactable::fmt_dispatch`].
///
/// Using this macro (rather than a hand-written `Display`) guarantees every
/// `{}` of the type goes through the global redactor.
#[macro_export]
macro_rules! impl_display_via_redactable {
    ($ty:ty) => {
        impl ::core::fmt::Display for $ty {
            #[inline]
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                $crate::Redactable::fmt_dispatch(self, f)
            }
        }
    };
}

/// Implements `core::fmt::Debug` for a type that implements [`Redactable`]
/// by delegating to [`Redactable::fmt_dispatch`], so `{:?}` produces the same
/// (possibly redacted) output as `{}`.
#[macro_export]
macro_rules! impl_debug_via_redactable {
    ($ty:ty) => {
        impl ::core::fmt::Debug for $ty {
            #[inline]
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                $crate::Redactable::fmt_dispatch(self, f)
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_SALT_1: [u8; SALT_LEN] = [0x11; SALT_LEN];
    const TEST_SALT_2: [u8; SALT_LEN] = [0x22; SALT_LEN];
    const TEST_MAC_1: [u8; MAC_LEN] = [0x00, 0x1a, 0x2b, 0x3c, 0x4d, 0x5e];
    const TEST_MAC_2: [u8; MAC_LEN] = [0x00, 0x1a, 0x2b, 0xff, 0xee, 0xdd];

    struct TestMac([u8; MAC_LEN]);

    fn fmt_mac(m: [u8; MAC_LEN], f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5])
    }

    impl Redactable for TestMac {
        fn fmt_unredacted(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            fmt_mac(self.0, f)
        }

        fn fmt_redacted(&self, r: &dyn Redactor, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            fmt_mac(r.redact_mac(self.0), f)?;
            f.write_str(REDACTED_MARKER)
        }
    }

    crate::impl_display_via_redactable!(TestMac);
    crate::impl_debug_via_redactable!(TestMac);

    struct TestSsid<'a>(&'a [u8]);

    fn fmt_ssid(bytes: &[u8], f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<ssid-")?;
        for byte in bytes {
            write!(f, "{byte:02x}")?;
        }
        write!(f, ">")
    }

    impl Redactable for TestSsid<'_> {
        fn fmt_unredacted(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            fmt_ssid(self.0, f)
        }

        fn fmt_redacted(&self, r: &dyn Redactor, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            fmt_ssid(&r.redact_ssid(self.0), f)?;
            f.write_str(REDACTED_MARKER)
        }
    }

    crate::impl_display_via_redactable!(TestSsid<'_>);

    fn format_with_redactor<T: Redactable>(item: &T, r: &dyn Redactor) -> String {
        struct Adapter<'a, T>(&'a T, &'a dyn Redactor);
        impl<T: Redactable> fmt::Display for Adapter<'_, T> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt_redacted(self.1, f)
            }
        }
        format!("{}", Adapter(item, r))
    }

    fn format_unredacted<T: Redactable>(item: &T) -> String {
        struct Adapter<'a, T>(&'a T);
        impl<T: Redactable> fmt::Display for Adapter<'_, T> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt_unredacted(f)
            }
        }
        format!("{}", Adapter(item))
    }

    #[test]
    fn masking_redactor_preserves_oui_and_zeroes_suffix_and_ssid() {
        assert_eq!(MaskingRedactor.redact_mac(TEST_MAC_1), [0x00, 0x1a, 0x2b, 0, 0, 0]);
        assert_eq!(MaskingRedactor.redact_ssid(b"foo"), [0; SSID_DIGEST_LEN]);
        assert_eq!(
            format_with_redactor(&TestMac(TEST_MAC_1), &MaskingRedactor),
            "00:1a:2b:00:00:00*"
        );
        assert_eq!(
            format_with_redactor(&TestSsid(b"foo"), &MaskingRedactor),
            "<ssid-0000000000000000>*"
        );
    }

    #[test]
    fn salted_redactor_preserves_oui_and_hashes_nic() {
        let r1 = SaltedRedactor::new(TEST_SALT_1);
        let r2 = SaltedRedactor::new(TEST_SALT_2);
        assert_eq!(r1.salt(), &TEST_SALT_1);

        let hashed_1 = format_with_redactor(&TestMac(TEST_MAC_1), &r1);
        assert!(hashed_1.starts_with("00:1a:2b:"));
        assert!(hashed_1.ends_with(REDACTED_MARKER));
        assert_eq!(hashed_1.len(), "00:1a:2b:3c:4d:5e*".len());
        assert_ne!(hashed_1, "00:1a:2b:3c:4d:5e*");

        // Deterministic for the same salt and MAC.
        assert_eq!(hashed_1, format_with_redactor(&TestMac(TEST_MAC_1), &r1));

        // Different MAC with the same OUI has the same OUI prefix but a different hashed NIC.
        let hashed_2 = format_with_redactor(&TestMac(TEST_MAC_2), &r1);
        assert!(hashed_2.starts_with("00:1a:2b:"));
        assert_ne!(hashed_1, hashed_2);

        // Different salt produces a different hashed NIC.
        let hashed_diff_salt = format_with_redactor(&TestMac(TEST_MAC_1), &r2);
        assert!(hashed_diff_salt.starts_with("00:1a:2b:"));
        assert_ne!(hashed_1, hashed_diff_salt);
    }

    #[test]
    fn salted_redactor_hashes_ssid_deterministically() {
        let r1 = SaltedRedactor::new(TEST_SALT_1);
        let r2 = SaltedRedactor::new(TEST_SALT_2);

        let ssid_1 = TestSsid(b"CorpWiFi");
        let ssid_2 = TestSsid(b"GuestWiFi");

        let hashed_1 = format_with_redactor(&ssid_1, &r1);
        assert!(hashed_1.starts_with("<ssid-"));
        assert!(hashed_1.ends_with(">*"));
        assert_ne!(hashed_1, format_unredacted(&ssid_1));
        assert_eq!(hashed_1, format_with_redactor(&ssid_1, &r1));
        assert_ne!(hashed_1, format_with_redactor(&ssid_2, &r1));
        assert_ne!(hashed_1, format_with_redactor(&ssid_1, &r2));
    }

    // The redactor state is process-global and Rust runs tests in parallel in
    // one process, so everything that touches the global lives in this single
    // test. Do not add a second test that calls `set_*_redactor`.
    #[test]
    fn global_state_machine_transitions() {
        // Uninitialized -> Masking -> Salted.
        set_redactor_state(State::Uninitialized);
        let mac = TestMac(TEST_MAC_1);
        let ssid = TestSsid(b"CorpWiFi");

        // Uninitialized -> dispatch writes the type's own unredacted format.
        assert_eq!(redactor_state(), State::Uninitialized);
        assert_eq!(format!("{mac}"), "00:1a:2b:3c:4d:5e");
        assert_eq!(format!("{mac:?}"), "00:1a:2b:3c:4d:5e");
        assert_eq!(format!("{ssid}"), "<ssid-436f727057694669>");

        // Transition to MASKING.
        set_masking_redactor();
        assert_eq!(redactor_state(), State::Masking);
        assert_eq!(format!("{mac}"), "00:1a:2b:00:00:00*");
        assert_eq!(format!("{ssid}"), "<ssid-0000000000000000>*");

        // Transition from MASKING to SALTED.
        assert_eq!(set_salted_redactor(TEST_SALT_1), Ok(()));
        let expected_mac = format_with_redactor(&mac, &SaltedRedactor::new(TEST_SALT_1));
        let expected_ssid = format_with_redactor(&ssid, &SaltedRedactor::new(TEST_SALT_1));
        assert_eq!(format!("{mac}"), expected_mac);
        assert_eq!(format!("{ssid}"), expected_ssid);

        // Once SALTED, further calls to set_salted_redactor or set_masking_redactor do not overwrite.
        assert_eq!(set_salted_redactor(TEST_SALT_2), Err(SetRedactorError));
        set_masking_redactor();
        assert_eq!(format!("{mac}"), expected_mac);

        // Direct Uninitialized -> Salted (no masking step) is also supported.
        set_redactor_state(State::Uninitialized);
        assert_eq!(format!("{mac}"), "00:1a:2b:3c:4d:5e");
        assert_eq!(set_salted_redactor(TEST_SALT_2), Ok(()));
        assert_eq!(format!("{mac}"), format_with_redactor(&mac, &SaltedRedactor::new(TEST_SALT_2)));
    }
}
