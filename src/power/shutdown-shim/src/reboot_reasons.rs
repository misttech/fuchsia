// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl::marker::SourceBreaking;
use fidl_fuchsia_hardware_power_statecontrol::{
    self as fpower, RebootReason2, ShutdownAction, ShutdownOptions, ShutdownReason,
};

/// The action and reasons of a shutdown.
///
/// This type provides translation functions for supporting deprecated enums.
// TODO(https://fxbug.dev/414413282): This type may not be necessary once `RebootReason2` is removed
// from the API.
#[derive(Debug, PartialEq, PartialOrd, Clone)]
pub struct ShutdownOptionsWrapper {
    pub action: ShutdownAction,
    pub reasons: Vec<ShutdownReason>,
}

impl ShutdownOptionsWrapper {
    /// Construct a new `ShutdownOptionsWrapper` with the given reason.
    pub fn new(action: fpower::ShutdownAction, reason: ShutdownReason) -> Self {
        Self { action, reasons: vec![reason] }
    }

    /// Construct a new `ShutdownOptionsWrapper` from the given deprecated
    /// `RebootReason`.
    // TODO(https://fxbug.dev/385742868): Remove this function once
    // `RebootReason` is removed from the API.
    pub(crate) fn from_reboot_reason_deprecated(reason: &fpower::RebootReason) -> Self {
        let reason = match reason {
            fpower::RebootReason::UserRequest => fpower::ShutdownReason::UserRequest,
            fpower::RebootReason::SystemUpdate => fpower::ShutdownReason::SystemUpdate,
            fpower::RebootReason::RetrySystemUpdate => fpower::ShutdownReason::RetrySystemUpdate,
            fpower::RebootReason::HighTemperature => fpower::ShutdownReason::HighTemperature,
            fpower::RebootReason::FactoryDataReset => fpower::ShutdownReason::FactoryDataReset,
            fpower::RebootReason::SessionFailure => fpower::ShutdownReason::SessionFailure,
            fpower::RebootReason::SysmgrFailure => {
                // sysmgr doesn't exist anymore.
                println!(
                    "[shutdown-shim]: error, unexpectedly received RebootReason::SysmgrFailure"
                );
                fpower::ShutdownReason::unknown()
            }
            fpower::RebootReason::CriticalComponentFailure => {
                fpower::ShutdownReason::CriticalComponentFailure
            }
            fpower::RebootReason::ZbiSwap => fpower::ShutdownReason::ZbiSwap,
            fpower::RebootReason::OutOfMemory => fpower::ShutdownReason::OutOfMemory,
        };
        Self::new(fpower::ShutdownAction::Reboot, reason)
    }

    /// Construct a new `ShutdownOptionsWrapper` from the given Vec of deprecated `RebootReason2`.
    pub(crate) fn from_reboot_reason2_deprecated(reasons: &Vec<RebootReason2>) -> Self {
        let reasons = reasons
            .iter()
            .map(|reason| match reason {
                RebootReason2::UserRequest => ShutdownReason::UserRequest,
                RebootReason2::DeveloperRequest => ShutdownReason::DeveloperRequest,
                RebootReason2::SystemUpdate => ShutdownReason::SystemUpdate,
                RebootReason2::RetrySystemUpdate => ShutdownReason::RetrySystemUpdate,
                RebootReason2::HighTemperature => ShutdownReason::HighTemperature,
                RebootReason2::FactoryDataReset => ShutdownReason::FactoryDataReset,
                RebootReason2::SessionFailure => ShutdownReason::SessionFailure,
                RebootReason2::SysmgrFailure => {
                    // sysmgr doesn't exist anymore.
                    println!(
                        "[shutdown-shim]: error, unexpectedly received RebootReason2::SysmgrFailure"
                    );
                    fpower::ShutdownReason::unknown()
                }
                RebootReason2::CriticalComponentFailure => ShutdownReason::CriticalComponentFailure,
                RebootReason2::ZbiSwap => ShutdownReason::ZbiSwap,
                RebootReason2::OutOfMemory => ShutdownReason::OutOfMemory,
                RebootReason2::NetstackMigration => ShutdownReason::NetstackMigration,
                RebootReason2::AndroidUnexpectedReason => ShutdownReason::AndroidUnexpectedReason,
                RebootReason2::AndroidRescueParty => ShutdownReason::AndroidRescueParty,
                RebootReason2::AndroidCriticalProcessFailure => {
                    ShutdownReason::AndroidCriticalProcessFailure
                }
                RebootReason2::__SourceBreaking { unknown_ordinal } => {
                    println!("[shutdown-shim]: error, unrecognized RebootReason2 ordinal: {unknown_ordinal}");
                    ShutdownReason::unknown()
                }
            })
            .collect();
        Self { action: ShutdownAction::Reboot, reasons }
    }

    /// Convert into a deprecated `RebootReason`. It's a backwards compatible implementation.
    /// * If multiple `ShutdownReason` are provided, prefer reasons with an
    ///   equivalent deprecated `RebootReason` representation.
    /// * Then, if multiple reasons are provided, prefer the first.
    /// * Then, if the reason has no equivalent deprecated `RebootReason`, do a
    ///   best-effort translation.
    // TODO(https://fxbug.dev/385742868): Remove this function once
    // `RebootReason` is removed from the API.
    pub(crate) fn to_reboot_reason_deprecated(&self) -> fpower::RebootReason {
        enum FoldState {
            Direct(fpower::RebootReason),
            Indirect(fpower::RebootReason),
            None,
        }
        let state = self.reasons.iter().fold(FoldState::None, |state, reason| {
            match (&state, &reason) {
                // We already have a direct state; keep it.
                (FoldState::Direct(_), _) => state,
                // For reasons that have a direct backwards translation, use it.
                (_, fpower::ShutdownReason::UserRequest) => {
                    FoldState::Direct(fpower::RebootReason::UserRequest)
                }
                (_, fpower::ShutdownReason::SystemUpdate) => {
                    FoldState::Direct(fpower::RebootReason::SystemUpdate)
                }
                (_, fpower::ShutdownReason::RetrySystemUpdate) => {
                    FoldState::Direct(fpower::RebootReason::RetrySystemUpdate)
                }
                (_, fpower::ShutdownReason::HighTemperature) => {
                    FoldState::Direct(fpower::RebootReason::HighTemperature)
                }
                (_, fpower::ShutdownReason::FactoryDataReset) => {
                    FoldState::Direct(fpower::RebootReason::FactoryDataReset)
                }
                (_, fpower::ShutdownReason::SessionFailure) => {
                    FoldState::Direct(fpower::RebootReason::SessionFailure)
                }
                (_, fpower::ShutdownReason::CriticalComponentFailure) => {
                    FoldState::Direct(fpower::RebootReason::CriticalComponentFailure)
                }
                (_, fpower::ShutdownReason::ZbiSwap) => {
                    FoldState::Direct(fpower::RebootReason::ZbiSwap)
                }
                (_, fpower::ShutdownReason::OutOfMemory) => {
                    FoldState::Direct(fpower::RebootReason::OutOfMemory)
                }
                (_, fpower::ShutdownReason::AndroidUnexpectedReason) => {
                    FoldState::Direct(fpower::RebootReason::UserRequest)
                }
                (_, fpower::ShutdownReason::AndroidRescueParty) => {
                    FoldState::Direct(fpower::RebootReason::UserRequest)
                }
                (_, fpower::ShutdownReason::AndroidCriticalProcessFailure) => {
                    FoldState::Direct(fpower::RebootReason::UserRequest)
                }
                (_, fpower::ShutdownReason::DeveloperRequest) => {
                    FoldState::Direct(fpower::RebootReason::UserRequest)
                }
                // If we already have an indirect reason, don't overwrite it
                // with a new indirect reason.
                (FoldState::Indirect(_), fpower::ShutdownReason::NetstackMigration) => state,
                // Translate `NetstackMigration` to `SystemUpdate`.
                (FoldState::None, fpower::ShutdownReason::NetstackMigration) => {
                    FoldState::Indirect(fpower::RebootReason::SystemUpdate)
                }
                (_, fpower::ShutdownReason::__SourceBreaking { unknown_ordinal: _ }) => {
                    unreachable!()
                }
            }
        });
        match state {
            FoldState::Direct(reason) | FoldState::Indirect(reason) => reason,
            FoldState::None => {
                unreachable!("Called to_reboot_reason with no reason(s) specified")
            }
        }
    }

    /// Convert into a vector of deprecated `RebootReason2`. It's a backwards compatible
    /// implementation. If the reason has no equivalent deprecated `RebootReason2`, do a best-effort
    /// translation.
    pub(crate) fn to_reboot_reason2_deprecated(&self) -> Vec<RebootReason2> {
        self.reasons
            .iter()
            .map(|item| match item {
                ShutdownReason::UserRequest => RebootReason2::UserRequest,
                ShutdownReason::DeveloperRequest => RebootReason2::DeveloperRequest,
                ShutdownReason::SystemUpdate => RebootReason2::SystemUpdate,
                ShutdownReason::RetrySystemUpdate => RebootReason2::RetrySystemUpdate,
                ShutdownReason::HighTemperature => RebootReason2::HighTemperature,
                ShutdownReason::FactoryDataReset => RebootReason2::FactoryDataReset,
                ShutdownReason::SessionFailure => RebootReason2::SessionFailure,
                ShutdownReason::CriticalComponentFailure => RebootReason2::CriticalComponentFailure,
                ShutdownReason::ZbiSwap => RebootReason2::ZbiSwap,
                ShutdownReason::OutOfMemory => RebootReason2::OutOfMemory,
                ShutdownReason::NetstackMigration => RebootReason2::NetstackMigration,
                ShutdownReason::AndroidUnexpectedReason => RebootReason2::AndroidUnexpectedReason,
                ShutdownReason::AndroidRescueParty => RebootReason2::AndroidRescueParty,
                ShutdownReason::AndroidCriticalProcessFailure => {
                    RebootReason2::AndroidCriticalProcessFailure
                }
                ShutdownReason::__SourceBreaking { unknown_ordinal } => {
                    println!("[shutdown-shim]: error, unrecognized ShutdownReason ordinal: {unknown_ordinal}");
                    RebootReason2::unknown()
                }
            })
            .collect()
    }
}

impl From<ShutdownOptionsWrapper> for fpower::RebootOptions {
    fn from(options: ShutdownOptionsWrapper) -> Self {
        fpower::RebootOptions {
            reasons: Some(options.to_reboot_reason2_deprecated()),
            __source_breaking: SourceBreaking,
        }
    }
}

impl From<ShutdownOptionsWrapper> for ShutdownOptions {
    fn from(options: ShutdownOptionsWrapper) -> Self {
        ShutdownOptions {
            action: Some(options.action),
            reasons: Some(options.reasons),
            __source_breaking: SourceBreaking,
        }
    }
}

/// The reasons a `fpower::RebootOptions` may be invalid.
#[derive(Debug, PartialEq)]
pub enum InvalidRebootOptions {
    /// No reasons were provided.
    NoReasons,
}

impl TryFrom<fpower::RebootOptions> for ShutdownOptionsWrapper {
    type Error = InvalidRebootOptions;
    fn try_from(options: fpower::RebootOptions) -> Result<Self, Self::Error> {
        let fpower::RebootOptions { reasons, __source_breaking } = options;
        if let Some(reasons) = reasons {
            if !reasons.is_empty() {
                return Ok(ShutdownOptionsWrapper::from_reboot_reason2_deprecated(&reasons));
            }
        }

        Err(InvalidRebootOptions::NoReasons)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case(None => Err(InvalidRebootOptions::NoReasons); "no_reasons")]
    #[test_case(Some(vec![]) => Err(InvalidRebootOptions::NoReasons); "empty_reasons")]
    #[test_case(Some(vec![fpower::RebootReason2::UserRequest]) => Ok(()); "success")]
    fn reboot_reasons(
        reasons: Option<Vec<fpower::RebootReason2>>,
    ) -> Result<(), InvalidRebootOptions> {
        let options = fpower::RebootOptions { reasons, __source_breaking: SourceBreaking };
        ShutdownOptionsWrapper::try_from(options).map(|_reasons| {})
    }

    #[test_case(
        vec![fpower::RebootReason2::UserRequest, fpower::RebootReason2::SystemUpdate] =>
        fpower::RebootReason::UserRequest;
        "prefer_first_a")]
    #[test_case(
        vec![fpower::RebootReason2::SystemUpdate, fpower::RebootReason2::UserRequest] =>
        fpower::RebootReason::SystemUpdate;
        "prefer_first_b")]
    #[test_case(
        vec![fpower::RebootReason2::NetstackMigration, fpower::RebootReason2::UserRequest] =>
        fpower::RebootReason::UserRequest;
        "prefer_direct")]
    #[test_case(
        vec![fpower::RebootReason2::NetstackMigration] =>
        fpower::RebootReason::SystemUpdate;
        "netstack_migration")]
    fn reasons_to_deprecated(reasons: Vec<fpower::RebootReason2>) -> fpower::RebootReason {
        let options =
            fpower::RebootOptions { reasons: Some(reasons), __source_breaking: SourceBreaking };
        ShutdownOptionsWrapper::try_from(options).unwrap().to_reboot_reason_deprecated()
    }
}
