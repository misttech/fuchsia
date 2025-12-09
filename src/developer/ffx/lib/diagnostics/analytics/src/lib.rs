// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use discovery::query::TargetInfoQuery;
use discovery::{DiscoverySources, TargetHandle, TargetState};
use std::collections::BTreeMap;

/// An enum used for our analytics to have a well-defined way of specifying a point of failure.
/// We can assume that all previous steps that would precede our checks would have succeeded.
pub enum PointOfFailure<'a> {
    /// We've failed trying to get the target specifier.
    GetTargetSpecifier,
    /// This denotes a failure when we run `DiagnosticsResolver::discovered_targets`, specifically.
    DiscoveryFailure {
        query: TargetInfoQuery,
        discovery_sources: DiscoverySources,
    },
    /// We were not able to find any matching targets.
    NoMatchingTargets {
        query: TargetInfoQuery,
        discovery_sources: DiscoverySources,
    },
    /// We were not able to find any matching targets.
    TooManyMatchingTargets {
        query: TargetInfoQuery,
        discovery_sources: DiscoverySources,
    },
    /// A state possible if we discover a device but the device is not in the correct state.
    /// e.g. ffx_target::Resolution::from_target_handle(handle) fails.
    TargetHandleInBadState {
        state: TargetState,
    },
    /// This state occurs when the target handle is being used to connect to SSH but the state
    /// isn't correct wrt networking.
    TargetDoesntSupportNetworking {
        state: TargetState,
    },
    /// The target address could not be converted to a scoped socket addr because the link local
    /// IPv6 scope wasn't valid.
    TargetAddressBadScope,
    UnableToBuildFDomainCommand,
    /// SSH into the target failed.
    SshConnectionFailed {
        state: TargetState,
        reason: &'a ffx_target::ConnectionError,
    },

    /////////// RCS ERRORS ////////
    /// Failed to connect to RCS. Contains a stringified FDomain error.
    FailedToConnectRCS {
        error: &'a ffx_target::ConnectionError,
    },

    /// Failed to open HW info component. Contains a stringified FDomain error.
    FailedToOpenHWInfoComponent {
        moniker: &'static str,
        protocol: &'static str,
    },

    /// Failed to get info from HW info component. Contains a stringified FDomain error.
    UnableToGetInfo {
        error: &'a fidl::Error,
    },

    //////// FASTBOOT ERRORS ///////
    /// The target handle, for some reason, did not contain a fastboot handle.
    NonFastbootTargetHandle {
        handle: TargetHandle,
    },

    /// We weren't able to create a fastboot interface for querying the fastboot device.
    CreatingFastbootInterface {
        handle: TargetHandle,
    },

    /// When querying the device for a serial number, we encountered an error.
    FastbootQueryingSerialNo {
        handle: TargetHandle,
    },
}

#[derive(Default)]
pub struct CustomEvent {
    category: &'static str,
    action: Option<String>,
    custom_dimensions: BTreeMap<&'static str, analytics::GA4Value>,
}

fn format_target_state(state: &TargetState) -> String {
    match state {
        TargetState::Product { .. } => "product",
        TargetState::Fastboot(_) => "fastboot",
        TargetState::Unknown => "unknown",
        TargetState::Zedboot => "zedboot",
    }
    .to_owned()
}

/// Convenience combinators for uploading analytics when using Result<_> types. Simpler than using
/// a newtype.
#[allow(async_fn_in_trait)]
pub trait ResultExt {
    type Error;
    type Success;

    /// In the event of an error, takes `self` and returns the same value. If the value of `self`
    /// is an error, then sends the `pof` analytics failure before returning.
    async fn or_analytics(self, pof: PointOfFailure<'_>) -> Result<Self::Success, Self::Error>;

    /// In the event of an error, takes `self` and returns the same value. If the value of `self`
    /// is an error, then sends the error to a closure to construct a `PointOfFailure` struct to
    /// send to analytics.
    async fn or_else_analytics<F: FnOnce(&Self::Error) -> PointOfFailure<'_>>(
        self,
        f: F,
    ) -> Result<Self::Success, Self::Error>;
}

impl<S, E> ResultExt for Result<S, E> {
    type Error = E;
    type Success = S;

    async fn or_analytics(self, pof: PointOfFailure<'_>) -> Result<Self::Success, Self::Error> {
        match self {
            Ok(s) => Ok(s),
            Err(e) => {
                mark_point_of_failure(pof).await;
                Err(e)
            }
        }
    }

    async fn or_else_analytics<F: FnOnce(&Self::Error) -> PointOfFailure<'_>>(
        self,
        f: F,
    ) -> Result<Self::Success, Self::Error> {
        match self {
            Ok(s) => Ok(s),
            Err(e) => {
                mark_point_of_failure(f(&e)).await;
                Err(e)
            }
        }
    }
}

fn sanitize_connection_error(error: &ffx_target::ConnectionError) -> String {
    use ffx_target::ConnectionError::*;
    // Depending on the frequency with which these errors are hit, it may be necessary to dive
    // deeper into the actual reason, but we want to avoid leaking PII by accident. A lot of
    // connectivity error code contains hostnames, addresses, etc.
    match error {
        ConnectionStartError(_dbg, reason) => {
            format!("ConnectionStartError: {reason}")
        }
        InternalError(_) => "InternalError".to_owned(),
        KnockError(_) => "KnockError".to_owned(),
        OvernetUnsupported => "OvernetUnsupported".to_owned(),
    }
}

impl From<PointOfFailure<'_>> for CustomEvent {
    fn from(pof: PointOfFailure<'_>) -> Self {
        use PointOfFailure::*;
        match pof {
            GetTargetSpecifier => {
                CustomEvent { category: "get_target_specifier", ..Default::default() }
            }
            DiscoveryFailure { query, discovery_sources } => CustomEvent {
                category: "discovery_failed",
                action: Some(ffx_diagnostics_formatting::format_query(&query).kind.to_owned()),
                custom_dimensions: [(
                    "discovery_sources",
                    (discovery_sources.bits() as u64).into(),
                )]
                .into_iter()
                .collect(),
            },
            NoMatchingTargets { query, discovery_sources } => CustomEvent {
                category: "no_matching_targets",
                action: Some(ffx_diagnostics_formatting::format_query(&query).kind.to_owned()),
                custom_dimensions: [(
                    "discovery_sources",
                    (discovery_sources.bits() as u64).into(),
                )]
                .into_iter()
                .collect(),
            },
            TooManyMatchingTargets { query, discovery_sources } => CustomEvent {
                category: "too_many_matching_targets",
                action: Some(ffx_diagnostics_formatting::format_query(&query).kind.to_owned()),
                custom_dimensions: [(
                    "discovery_sources",
                    (discovery_sources.bits() as u64).into(),
                )]
                .into_iter()
                .collect(),
            },
            TargetHandleInBadState { state } => CustomEvent {
                category: "target_hdl_in_bad_state",
                custom_dimensions: [("target_state", format_target_state(&state).into())]
                    .into_iter()
                    .collect(),
                ..Default::default()
            },
            TargetDoesntSupportNetworking { state } => CustomEvent {
                category: "target_no_netwrk",
                custom_dimensions: [("target_state", format_target_state(&state).into())]
                    .into_iter()
                    .collect(),
                ..Default::default()
            },
            TargetAddressBadScope => {
                CustomEvent { category: "target_addr_bad_scope", ..Default::default() }
            }
            UnableToBuildFDomainCommand => {
                CustomEvent { category: "build_fdomain_cmd", ..Default::default() }
            }
            SshConnectionFailed { state, reason } => CustomEvent {
                category: "ssh_connection",
                custom_dimensions: [
                    ("target_state", format_target_state(&state).into()),
                    ("error", sanitize_connection_error(reason).into()),
                ]
                .into_iter()
                .collect(),
                ..Default::default()
            },
            FailedToConnectRCS { error } => CustomEvent {
                category: "connect_rcs",
                custom_dimensions: [("error", sanitize_connection_error(error).into())]
                    .into_iter()
                    .collect(),
                ..Default::default()
            },
            FailedToOpenHWInfoComponent { moniker, protocol } => CustomEvent {
                category: "open_hwinfo_comp",
                custom_dimensions: [("error", format!("{moniker}:{protocol}").into())]
                    .into_iter()
                    .collect(),
                ..Default::default()
            },
            UnableToGetInfo { error } => CustomEvent {
                category: "hwinfo_getinfo",
                custom_dimensions: [("error", format!("{error}").into())].into_iter().collect(),
                ..Default::default()
            },
            NonFastbootTargetHandle { handle } => CustomEvent {
                category: "non_fastboot_target_hdl",
                custom_dimensions: [("target_state", format_target_state(&handle.state).into())]
                    .into_iter()
                    .collect(),
                ..Default::default()
            },
            CreatingFastbootInterface { handle } => CustomEvent {
                category: "create_fastboot_iface",
                custom_dimensions: [("target_state", format_target_state(&handle.state).into())]
                    .into_iter()
                    .collect(),
                ..Default::default()
            },
            FastbootQueryingSerialNo { handle } => CustomEvent {
                category: "query_serialno",
                custom_dimensions: [("target_state", format_target_state(&handle.state).into())]
                    .into_iter()
                    .collect(),
                ..Default::default()
            },
        }
    }
}

/// Takes an error, and a "point of failure," and uploads the analytics for this specific type of
/// failure for tracking.
pub async fn mark_point_of_failure(failure_point: PointOfFailure<'_>) {
    let CustomEvent { category, action, mut custom_dimensions } = failure_point.into();
    if !ffx_command::send_enhanced_analytics().await {
        return;
    }
    let mut client = match analytics::ga4_metrics().await {
        Ok(a) => a,
        _ => return,
    };
    let category_name_key: &'static str = "category_name";
    if let Some(v) = custom_dimensions.insert(category_name_key, category.into()) {
        panic!(
            "{category_name_key:?} somehow already set to {v:?}. This is a bug. Please report this to {}",
            errors::BUG_REPORT_URL
        );
    }
    for (_k, v) in custom_dimensions.iter_mut() {
        if let analytics::GA4Value::Str(s) = v {
            *v = analytics::redact_host_and_user_from(s).into()
        }
    }
    match client
        .add_custom_event(
            None,
            action.as_deref(),
            None,
            custom_dimensions,
            Some("ffx_diagnostics_failure"),
        )
        .await
    {
        Ok(_) => {}
        Err(e) => {
            log::warn!("Unable to stage analytics: {e}");
            // No sense in sending analytics if we were unable to stage them.
            return;
        }
    }
    // If this occurs frequently enough, we may wish to inform the user via notifier that
    // analytics failed to send.
    let _ = client.send_events().await.map_err(|e| log::warn!("Unable to send analytics: {e}"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn test_get_target_specifier_conversion() {
        let pof = PointOfFailure::GetTargetSpecifier;
        let event: CustomEvent = pof.into();
        assert_eq!(event.category, "get_target_specifier");
        assert_eq!(event.action, None);
        assert!(event.custom_dimensions.is_empty());
    }

    #[test]
    fn test_discovery_failure_conversion() {
        let query = TargetInfoQuery::from("some-nodename");
        let pof = PointOfFailure::DiscoveryFailure {
            query: query.clone(),
            discovery_sources: DiscoverySources::all(),
        };
        let event: CustomEvent = pof.into();
        assert_eq!(event.category, "discovery_failed");
        assert_eq!(event.action, Some("nodename or serial".to_owned()));
        assert_eq!(
            event.custom_dimensions.get("discovery_sources"),
            Some(&(DiscoverySources::all().bits() as u64).into())
        );
    }

    #[test]
    fn test_no_matching_targets_conversion() {
        let query = TargetInfoQuery::from("some-nodename");
        let pof = PointOfFailure::NoMatchingTargets {
            query: query.clone(),
            discovery_sources: DiscoverySources::all(),
        };
        let event: CustomEvent = pof.into();
        assert_eq!(event.category, "no_matching_targets");
        assert_eq!(event.action, Some("nodename or serial".to_owned()));
        assert_eq!(
            event.custom_dimensions.get("discovery_sources"),
            Some(&(DiscoverySources::all().bits() as u64).into())
        );
    }

    #[test]
    fn test_too_many_matching_targets_conversion() {
        let query = TargetInfoQuery::from("some-nodename");
        let pof = PointOfFailure::TooManyMatchingTargets {
            query: query.clone(),
            discovery_sources: DiscoverySources::all(),
        };
        let event: CustomEvent = pof.into();
        assert_eq!(event.category, "too_many_matching_targets");
        assert_eq!(event.action, Some("nodename or serial".to_owned()));
        assert_eq!(
            event.custom_dimensions.get("discovery_sources"),
            Some(&(DiscoverySources::all().bits() as u64).into())
        );
    }

    #[test]
    fn test_target_handle_in_bad_state_conversion() {
        let pof = PointOfFailure::TargetHandleInBadState { state: TargetState::Zedboot };
        let event: CustomEvent = pof.into();
        assert_eq!(event.category, "target_hdl_in_bad_state");
        assert_eq!(event.action, None);
        assert_eq!(event.custom_dimensions.get("target_state"), Some(&"zedboot".to_owned().into()));
    }

    #[test]
    fn test_target_doesnt_support_networking_conversion() {
        let pof = PointOfFailure::TargetDoesntSupportNetworking { state: TargetState::Unknown };
        let event: CustomEvent = pof.into();
        assert_eq!(event.category, "target_no_netwrk");
        assert_eq!(event.action, None);
        assert_eq!(event.custom_dimensions.get("target_state"), Some(&"unknown".to_owned().into()));
    }

    #[test]
    fn test_target_address_bad_scope_conversion() {
        let pof = PointOfFailure::TargetAddressBadScope;
        let event: CustomEvent = pof.into();
        assert_eq!(event.category, "target_addr_bad_scope");
        assert_eq!(event.action, None);
        assert!(event.custom_dimensions.is_empty());
    }

    #[test]
    fn test_unable_to_build_fdomain_command_conversion() {
        let pof = PointOfFailure::UnableToBuildFDomainCommand;
        let event: CustomEvent = pof.into();
        assert_eq!(event.category, "build_fdomain_cmd");
        assert_eq!(event.action, None);
        assert!(event.custom_dimensions.is_empty());
    }

    #[test]
    fn test_ssh_connection_failed_conversion() {
        let reason =
            ffx_target::ConnectionError::ConnectionStartError("foo".to_owned(), "bar".to_owned());
        let pof =
            PointOfFailure::SshConnectionFailed { state: TargetState::Zedboot, reason: &reason };
        let event: CustomEvent = pof.into();
        assert_eq!(event.category, "ssh_connection");
        assert_eq!(event.action, None);
        assert_eq!(event.custom_dimensions.get("target_state"), Some(&"zedboot".to_owned().into()));
        assert_eq!(
            event.custom_dimensions.get("error"),
            Some(&sanitize_connection_error(&reason).into())
        );
    }

    #[test]
    fn test_failed_to_connect_rcs_conversion() {
        let error =
            ffx_target::ConnectionError::ConnectionStartError("foo".to_owned(), "bar".to_owned());
        let pof = PointOfFailure::FailedToConnectRCS { error: &error };
        let event: CustomEvent = pof.into();
        assert_eq!(event.category, "connect_rcs");
        assert_eq!(event.action, None);
        assert_eq!(
            event.custom_dimensions.get("error"),
            Some(&sanitize_connection_error(&error).into())
        );
    }

    #[test]
    fn test_failed_to_open_hw_info_component_conversion() {
        let pof = PointOfFailure::FailedToOpenHWInfoComponent {
            moniker: "foo/bar",
            protocol: "foo.bar.Baz",
        };
        let event: CustomEvent = pof.into();
        assert_eq!(event.category, "open_hwinfo_comp");
        assert_eq!(event.action, None);
        assert_eq!(event.custom_dimensions.get("error"), Some(&"foo/bar:foo.bar.Baz".into()));
    }

    #[test]
    fn test_unable_to_get_info_conversion() {
        let error = fidl::Error::ClientChannelClosed {
            status: fidl::Status::PEER_CLOSED,
            protocol_name: "something-made-up",
            epitaph: Some(23u32),
            reason: None,
        };
        let pof = PointOfFailure::UnableToGetInfo { error: &error };
        let event: CustomEvent = pof.into();
        assert_eq!(event.category, "hwinfo_getinfo");
        assert_eq!(event.action, None);
        assert_eq!(event.custom_dimensions.get("error"), Some(&format!("{error}").into()));
    }

    #[test]
    fn test_non_fastboot_target_handle_conversion() {
        let handle = TargetHandle { node_name: None, state: TargetState::Unknown, manual: false };
        let pof = PointOfFailure::NonFastbootTargetHandle { handle };
        let event: CustomEvent = pof.into();
        assert_eq!(event.category, "non_fastboot_target_hdl");
        assert_eq!(event.action, None);
        assert_eq!(event.custom_dimensions.get("target_state"), Some(&"unknown".to_owned().into()));
    }

    #[test]
    fn test_creating_fastboot_interface_conversion() {
        let handle = TargetHandle { node_name: None, state: TargetState::Unknown, manual: false };
        let pof = PointOfFailure::CreatingFastbootInterface { handle };
        let event: CustomEvent = pof.into();
        assert_eq!(event.category, "create_fastboot_iface");
        assert_eq!(event.action, None);
        assert_eq!(event.custom_dimensions.get("target_state"), Some(&"unknown".to_owned().into()));
    }

    #[test]
    fn test_fastboot_querying_serial_no_conversion() {
        let handle = TargetHandle { node_name: None, state: TargetState::Unknown, manual: false };
        let pof = PointOfFailure::FastbootQueryingSerialNo { handle };
        let event: CustomEvent = pof.into();
        assert_eq!(event.category, "query_serialno");
        assert_eq!(event.action, None);
        assert_eq!(event.custom_dimensions.get("target_state"), Some(&"unknown".to_owned().into()));
    }
}
