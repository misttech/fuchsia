// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// TODO(https://fxbug.dev/513286064): Remove these when converting is supported.

use fidl_fuchsia_data as fdata;
use fidl_fuchsia_driver_framework as fidl_fdf;
use fidl_next_fuchsia_component_decl as fidl_next_fdecl;
use fidl_next_fuchsia_component_runner as fidl_next_fcr;
use fidl_next_fuchsia_data as fidl_next_fdata;
use fidl_next_fuchsia_driver_framework as fidl_next_fdf;

fn convert_offer(offer: fidl_fdf::Offer) -> fidl_next_fdf::Offer {
    match offer {
        fidl_fdf::Offer::ZirconTransport(o) => {
            fidl_next_fdf::Offer::ZirconTransport(convert_decl_offer(o))
        }
        fidl_fdf::Offer::DriverTransport(o) => {
            fidl_next_fdf::Offer::DriverTransport(convert_decl_offer(o))
        }
        fidl_fdf::Offer::DictionaryOffer(o) => {
            fidl_next_fdf::Offer::DictionaryOffer(convert_decl_offer(o))
        }
        _ => panic!("Unknown Offer variant"),
    }
}

fn convert_decl_offer(offer: fidl_fuchsia_component_decl::Offer) -> fidl_next_fdecl::Offer {
    match offer {
        fidl_fuchsia_component_decl::Offer::Protocol(o) => {
            fidl_next_fdecl::Offer::Protocol(convert_offer_protocol(o))
        }
        fidl_fuchsia_component_decl::Offer::Service(o) => {
            fidl_next_fdecl::Offer::Service(convert_offer_service(o))
        }
        fidl_fuchsia_component_decl::Offer::Directory(o) => {
            fidl_next_fdecl::Offer::Directory(convert_offer_directory(o))
        }
        fidl_fuchsia_component_decl::Offer::Storage(o) => {
            fidl_next_fdecl::Offer::Storage(convert_offer_storage(o))
        }
        fidl_fuchsia_component_decl::Offer::Runner(o) => {
            fidl_next_fdecl::Offer::Runner(convert_offer_runner(o))
        }
        fidl_fuchsia_component_decl::Offer::Resolver(o) => {
            fidl_next_fdecl::Offer::Resolver(convert_offer_resolver(o))
        }
        fidl_fuchsia_component_decl::Offer::EventStream(o) => {
            fidl_next_fdecl::Offer::EventStream(convert_offer_event_stream(o))
        }
        fidl_fuchsia_component_decl::Offer::Dictionary(o) => {
            fidl_next_fdecl::Offer::Dictionary(convert_offer_dictionary_decl(o))
        }
        fidl_fuchsia_component_decl::Offer::Config(o) => {
            fidl_next_fdecl::Offer::Config(convert_offer_config(o))
        }
        _ => panic!("Unknown variant in flexible union Offer"),
    }
}

fn convert_offer_directory(
    o: fidl_fuchsia_component_decl::OfferDirectory,
) -> fidl_next_fdecl::OfferDirectory {
    fidl_next_fdecl::OfferDirectory {
        source: o.source.map(convert_ref),
        source_name: o.source_name,
        source_dictionary: o.source_dictionary,
        target: o.target.map(convert_ref),
        target_name: o.target_name,
        rights: o.rights.map(|r| fidl_next_fuchsia_io::Operations::from_bits_retain(r.bits())),
        subdir: o.subdir,
        dependency_type: o.dependency_type.map(convert_dependency_type),
        availability: o.availability.map(convert_availability),
    }
}

fn convert_offer_storage(
    o: fidl_fuchsia_component_decl::OfferStorage,
) -> fidl_next_fdecl::OfferStorage {
    fidl_next_fdecl::OfferStorage {
        source_name: o.source_name,
        source: o.source.map(convert_ref),
        target: o.target.map(convert_ref),
        target_name: o.target_name,
        availability: o.availability.map(convert_availability),
    }
}

fn convert_offer_runner(
    o: fidl_fuchsia_component_decl::OfferRunner,
) -> fidl_next_fdecl::OfferRunner {
    fidl_next_fdecl::OfferRunner {
        source: o.source.map(convert_ref),
        source_name: o.source_name,
        source_dictionary: o.source_dictionary,
        target: o.target.map(convert_ref),
        target_name: o.target_name,
    }
}

fn convert_offer_resolver(
    o: fidl_fuchsia_component_decl::OfferResolver,
) -> fidl_next_fdecl::OfferResolver {
    fidl_next_fdecl::OfferResolver {
        source: o.source.map(convert_ref),
        source_name: o.source_name,
        source_dictionary: o.source_dictionary,
        target: o.target.map(convert_ref),
        target_name: o.target_name,
    }
}

fn convert_offer_event_stream(
    o: fidl_fuchsia_component_decl::OfferEventStream,
) -> fidl_next_fdecl::OfferEventStream {
    fidl_next_fdecl::OfferEventStream {
        source: o.source.map(convert_ref),
        source_name: o.source_name,
        scope: o.scope.map(|v| v.into_iter().map(convert_ref).collect()),
        target: o.target.map(convert_ref),
        target_name: o.target_name,
        availability: o.availability.map(convert_availability),
    }
}

fn convert_offer_dictionary_decl(
    o: fidl_fuchsia_component_decl::OfferDictionary,
) -> fidl_next_fdecl::OfferDictionary {
    fidl_next_fdecl::OfferDictionary {
        source: o.source.map(convert_ref),
        source_name: o.source_name,
        source_dictionary: o.source_dictionary,
        target: o.target.map(convert_ref),
        target_name: o.target_name,
        dependency_type: o.dependency_type.map(convert_dependency_type),
        availability: o.availability.map(convert_availability),
    }
}

fn convert_offer_config(
    o: fidl_fuchsia_component_decl::OfferConfiguration,
) -> fidl_next_fdecl::OfferConfiguration {
    fidl_next_fdecl::OfferConfiguration {
        source: o.source.map(convert_ref),
        source_name: o.source_name,
        target: o.target.map(convert_ref),
        target_name: o.target_name,
        availability: o.availability.map(convert_availability),
        source_dictionary: o.source_dictionary,
    }
}

fn convert_offer_protocol(
    o: fidl_fuchsia_component_decl::OfferProtocol,
) -> fidl_next_fdecl::OfferProtocol {
    fidl_next_fdecl::OfferProtocol {
        source: o.source.map(convert_ref),
        source_name: o.source_name,
        source_dictionary: o.source_dictionary,
        target: o.target.map(convert_ref),
        target_name: o.target_name,
        dependency_type: o.dependency_type.map(convert_dependency_type),
        availability: o.availability.map(convert_availability),
    }
}

fn convert_offer_service(
    o: fidl_fuchsia_component_decl::OfferService,
) -> fidl_next_fdecl::OfferService {
    fidl_next_fdecl::OfferService {
        source: o.source.map(convert_ref),
        source_name: o.source_name,
        source_dictionary: o.source_dictionary,
        target: o.target.map(convert_ref),
        target_name: o.target_name,
        source_instance_filter: o.source_instance_filter,
        renamed_instances: o.renamed_instances.map(|v| {
            v.into_iter()
                .map(|m| fidl_next_fdecl::NameMapping {
                    source_name: m.source_name,
                    target_name: m.target_name,
                })
                .collect()
        }),
        availability: o.availability.map(convert_availability),
        dependency_type: o.dependency_type.map(convert_dependency_type),
    }
}

fn convert_ref(r: fidl_fuchsia_component_decl::Ref) -> fidl_next_fdecl::Ref {
    match r {
        fidl_fuchsia_component_decl::Ref::Parent(_) => fidl_next_fdecl::Ref::Parent(()),
        fidl_fuchsia_component_decl::Ref::Self_(_) => fidl_next_fdecl::Ref::Self_(()),
        fidl_fuchsia_component_decl::Ref::Child(c) => {
            fidl_next_fdecl::Ref::Child(fidl_next_fdecl::ChildRef {
                name: c.name,
                collection: c.collection,
            })
        }
        fidl_fuchsia_component_decl::Ref::Collection(c) => {
            fidl_next_fdecl::Ref::Collection(fidl_next_fdecl::CollectionRef { name: c.name })
        }
        fidl_fuchsia_component_decl::Ref::Framework(_) => fidl_next_fdecl::Ref::Framework(()),
        fidl_fuchsia_component_decl::Ref::Capability(c) => {
            fidl_next_fdecl::Ref::Capability(fidl_next_fdecl::CapabilityRef { name: c.name })
        }
        _ => panic!("Unknown Ref variant"),
    }
}

fn convert_dependency_type(
    d: fidl_fuchsia_component_decl::DependencyType,
) -> fidl_next_fdecl::DependencyType {
    match d {
        fidl_fuchsia_component_decl::DependencyType::Strong => {
            fidl_next_fdecl::DependencyType::Strong
        }
        fidl_fuchsia_component_decl::DependencyType::Weak => fidl_next_fdecl::DependencyType::Weak,
    }
}

fn convert_availability(
    a: fidl_fuchsia_component_decl::Availability,
) -> fidl_next_fdecl::Availability {
    match a {
        fidl_fuchsia_component_decl::Availability::Required => {
            fidl_next_fdecl::Availability::Required
        }
        fidl_fuchsia_component_decl::Availability::Optional => {
            fidl_next_fdecl::Availability::Optional
        }
        fidl_fuchsia_component_decl::Availability::SameAsTarget => {
            fidl_next_fdecl::Availability::SameAsTarget
        }
        fidl_fuchsia_component_decl::Availability::Transitional => {
            fidl_next_fdecl::Availability::Transitional
        }
    }
}

fn convert_dictionary(dict: fdata::Dictionary) -> fidl_next_fdata::Dictionary {
    let entries = dict.entries.map(|entries| {
        entries
            .into_iter()
            .map(|entry| fidl_next_fdata::DictionaryEntry {
                key: entry.key,
                value: entry.value.map(|v| Box::new(convert_dictionary_value(*v))),
            })
            .collect()
    });
    fidl_next_fdata::Dictionary { entries }
}

fn convert_dictionary_value(value: fdata::DictionaryValue) -> fidl_next_fdata::DictionaryValue {
    match value {
        fdata::DictionaryValue::Str(s) => fidl_next_fdata::DictionaryValue::Str(s),
        fdata::DictionaryValue::StrVec(v) => fidl_next_fdata::DictionaryValue::StrVec(v),
        fdata::DictionaryValue::ObjVec(v) => fidl_next_fdata::DictionaryValue::ObjVec(
            v.into_iter().map(convert_dictionary).collect(),
        ),
        _ => panic!("Unknown DictionaryValue variant"),
    }
}

fn convert_node_property_value(v: fidl_fdf::NodePropertyValue) -> fidl_next_fdf::NodePropertyValue {
    match v {
        fidl_fdf::NodePropertyValue::IntValue(v) => fidl_next_fdf::NodePropertyValue::IntValue(v),
        fidl_fdf::NodePropertyValue::StringValue(v) => {
            fidl_next_fdf::NodePropertyValue::StringValue(v)
        }
        fidl_fdf::NodePropertyValue::BoolValue(v) => fidl_next_fdf::NodePropertyValue::BoolValue(v),
        fidl_fdf::NodePropertyValue::EnumValue(v) => fidl_next_fdf::NodePropertyValue::EnumValue(v),
        _ => panic!("Unknown NodePropertyValue variant"),
    }
}

fn convert_node_property_2(p: fidl_fdf::NodeProperty2) -> fidl_next_fdf::NodeProperty2 {
    fidl_next_fdf::NodeProperty2 { key: p.key, value: convert_node_property_value(p.value) }
}

fn convert_node_property_entry_2(
    e: fidl_fdf::NodePropertyEntry2,
) -> fidl_next_fdf::NodePropertyEntry2 {
    fidl_next_fdf::NodePropertyEntry2 {
        name: e.name,
        properties: e.properties.into_iter().map(convert_node_property_2).collect(),
    }
}

pub(crate) fn convert_start_args(
    args: fidl_fdf::DriverStartArgs,
) -> fidl_next_fdf::DriverStartArgs {
    fidl_next_fdf::DriverStartArgs {
        url: args.url,
        program: args.program.map(convert_dictionary),
        incoming: args.incoming.map(|inc| {
            inc.into_iter()
                .map(|e| fidl_next_fcr::ComponentNamespaceEntry {
                    path: e.path,
                    directory: e
                        .directory
                        .map(|d| fidl_next::ClientEnd::from_untyped(d.into_channel())),
                })
                .collect()
        }),
        symbols: args.symbols.map(|syms| {
            syms.into_iter()
                .map(|s| fidl_next_fdf::NodeSymbol {
                    name: s.name,
                    address: s.address,
                    module_name: s.module_name,
                })
                .collect()
        }),
        node_token: args.node_token,
        node_offers: args.node_offers.map(|offers| offers.into_iter().map(convert_offer).collect()),
        node: args.node.map(|c| fidl_next::ClientEnd::from_untyped(c.into_channel())),
        node_name: args.node_name,
        outgoing_dir: args
            .outgoing_dir
            .map(|s| fidl_next::ServerEnd::from_untyped(s.into_channel())),
        config: args.config,
        vmar: args.vmar,
        power_element_args: args.power_element_args.map(|p| fidl_next_fdf::PowerElementArgs {
            control_client: p
                .control_client
                .map(|c| fidl_next::ClientEnd::from_untyped(c.into_channel())),
            runner_server: p
                .runner_server
                .map(|s| fidl_next::ServerEnd::from_untyped(s.into_channel())),
            lessor_client: p
                .lessor_client
                .map(|c| fidl_next::ClientEnd::from_untyped(c.into_channel())),
            token: p.token,
        }),
        log_sink: args.log_sink.map(|c| fidl_next::ClientEnd::from_untyped(c.into_channel())),
        node_properties_2: args
            .node_properties_2
            .map(|props| props.into_iter().map(convert_node_property_entry_2).collect()),
    }
}

// The health enums below are flexible on both sides with identical wire values, so converting
// through the primitive preserves unknown ordinals instead of collapsing them to a guess.
pub(crate) fn convert_health_check_request(
    req: fidl_fdf::HealthCheckRequest,
) -> fidl_next_fdf::HealthCheckRequest {
    fidl_next_fdf::HealthCheckRequest::from(req.into_primitive())
}

fn convert_health_status(status: fidl_next_fdf::HealthStatus) -> fidl_fdf::HealthStatus {
    fidl_fdf::HealthStatus::from_primitive_allow_unknown(u32::from(status))
}

fn convert_health_diagnostics(
    items: Vec<fidl_next_fdf::HealthDiagnosticItem>,
) -> Vec<fidl_fdf::HealthDiagnosticItem> {
    items
        .into_iter()
        .filter_map(|item| {
            let value = match item.value {
                fidl_next_fdf::HealthDiagnosticValue::StringValue(s) => {
                    fidl_fdf::HealthDiagnosticValue::StringValue(s)
                }
                fidl_next_fdf::HealthDiagnosticValue::IntValue(i) => {
                    fidl_fdf::HealthDiagnosticValue::IntValue(i)
                }
                fidl_next_fdf::HealthDiagnosticValue::UintValue(u) => {
                    fidl_fdf::HealthDiagnosticValue::UintValue(u)
                }
                fidl_next_fdf::HealthDiagnosticValue::BoolValue(b) => {
                    fidl_fdf::HealthDiagnosticValue::BoolValue(b)
                }
                fidl_next_fdf::HealthDiagnosticValue::Timestamp(t) => {
                    fidl_fdf::HealthDiagnosticValue::Timestamp(fidl::BootInstant::from_nanos(t))
                }
                _ => return None,
            };
            Some(fidl_fdf::HealthDiagnosticItem { key: item.key, value })
        })
        .collect()
}

fn convert_health_condition(
    condition: fidl_next_fdf::HealthCondition,
) -> fidl_fdf::HealthCondition {
    fidl_fdf::HealthCondition {
        status: condition.status.map(convert_health_status),
        reason_code: condition.reason_code,
        message: condition.message,
        diagnostics: condition.diagnostics.map(convert_health_diagnostics),
        first_observed: condition.first_observed.map(fidl::BootInstant::from_nanos),
        last_observed: condition.last_observed.map(fidl::BootInstant::from_nanos),
        occurrence_count: condition.occurrence_count,
        ..Default::default()
    }
}

pub(crate) fn convert_health_check_response(
    resp: fidl_next_fdf::HealthCheckResponse,
) -> fidl_fdf::HealthCheckResponse {
    fidl_fdf::HealthCheckResponse {
        diagnostics: resp.diagnostics.map(convert_health_diagnostics),
        conditions: resp
            .conditions
            .map(|conditions| conditions.into_iter().map(convert_health_condition).collect()),
        ..Default::default()
    }
}

pub(crate) fn convert_health_check_error(
    err: fidl_next_fdf::HealthCheckError,
) -> fidl_fdf::HealthCheckError {
    fidl_fdf::HealthCheckError::from_primitive_allow_unknown(u32::from(err))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[fuchsia::test]
    fn test_convert_health_check_request() {
        assert_eq!(
            convert_health_check_request(fidl_fdf::HealthCheckRequest::Ping),
            fidl_next_fdf::HealthCheckRequest::Ping
        );
        assert_eq!(
            convert_health_check_request(fidl_fdf::HealthCheckRequest::Fast),
            fidl_next_fdf::HealthCheckRequest::Fast
        );
        assert_eq!(
            convert_health_check_request(fidl_fdf::HealthCheckRequest::Deep),
            fidl_next_fdf::HealthCheckRequest::Deep
        );

        // Unknown levels round-trip losslessly instead of being coerced to FAST.
        let unknown = convert_health_check_request(
            fidl_fdf::HealthCheckRequest::from_primitive_allow_unknown(99),
        );
        assert_eq!(unknown, fidl_next_fdf::HealthCheckRequest::from(99));
    }

    #[fuchsia::test]
    fn test_convert_health_check_response_and_error() {
        let next_resp = fidl_next_fdf::HealthCheckResponse {
            conditions: Some(vec![fidl_next_fdf::HealthCondition {
                status: Some(fidl_next_fdf::HealthStatus::Degraded),
                reason_code: Some("FIFO_UNDERRUN".to_string()),
                message: Some("Recovered".to_string()),
                diagnostics: Some(vec![fidl_next_fdf::HealthDiagnosticItem {
                    key: "underruns".to_string(),
                    value: fidl_next_fdf::HealthDiagnosticValue::UintValue(2),
                }]),
                first_observed: Some(100),
                last_observed: Some(200),
                occurrence_count: Some(2),
            }]),
            diagnostics: Some(vec![
                fidl_next_fdf::HealthDiagnosticItem {
                    key: "str_k".to_string(),
                    value: fidl_next_fdf::HealthDiagnosticValue::StringValue("val".to_string()),
                },
                fidl_next_fdf::HealthDiagnosticItem {
                    key: "int_k".to_string(),
                    value: fidl_next_fdf::HealthDiagnosticValue::IntValue(-7),
                },
                fidl_next_fdf::HealthDiagnosticItem {
                    key: "uint_k".to_string(),
                    value: fidl_next_fdf::HealthDiagnosticValue::UintValue(99),
                },
                fidl_next_fdf::HealthDiagnosticItem {
                    key: "bool_k".to_string(),
                    value: fidl_next_fdf::HealthDiagnosticValue::BoolValue(true),
                },
                fidl_next_fdf::HealthDiagnosticItem {
                    key: "ts_k".to_string(),
                    value: fidl_next_fdf::HealthDiagnosticValue::Timestamp(123),
                },
            ]),
        };
        let converted = convert_health_check_response(next_resp);
        let diags = converted.diagnostics.expect("diagnostics");
        assert_eq!(diags.len(), 5);
        assert_eq!(diags[0].value, fidl_fdf::HealthDiagnosticValue::StringValue("val".to_string()));
        assert_eq!(diags[1].value, fidl_fdf::HealthDiagnosticValue::IntValue(-7));
        assert_eq!(diags[2].value, fidl_fdf::HealthDiagnosticValue::UintValue(99));
        assert_eq!(diags[3].value, fidl_fdf::HealthDiagnosticValue::BoolValue(true));
        assert_eq!(
            diags[4].value,
            fidl_fdf::HealthDiagnosticValue::Timestamp(fidl::BootInstant::from_nanos(123))
        );
        let conditions = converted.conditions.expect("conditions");
        assert_eq!(conditions.len(), 1);
        assert_eq!(conditions[0].status, Some(fidl_fdf::HealthStatus::Degraded));
        assert_eq!(conditions[0].reason_code.as_deref(), Some("FIFO_UNDERRUN"));
        assert_eq!(conditions[0].message.as_deref(), Some("Recovered"));
        assert_eq!(conditions[0].first_observed, Some(fidl::BootInstant::from_nanos(100)));
        assert_eq!(conditions[0].last_observed, Some(fidl::BootInstant::from_nanos(200)));
        assert_eq!(conditions[0].occurrence_count, Some(2));
        let cond_diags = conditions[0].diagnostics.as_ref().expect("condition diagnostics");
        assert_eq!(cond_diags.len(), 1);
        assert_eq!(cond_diags[0].value, fidl_fdf::HealthDiagnosticValue::UintValue(2));

        assert_eq!(
            convert_health_check_error(fidl_next_fdf::HealthCheckError::Timeout),
            fidl_fdf::HealthCheckError::Timeout
        );
        assert_eq!(
            convert_health_check_error(fidl_next_fdf::HealthCheckError::InternalError),
            fidl_fdf::HealthCheckError::InternalError
        );
        assert_eq!(
            convert_health_check_error(fidl_next_fdf::HealthCheckError::NotSupported),
            fidl_fdf::HealthCheckError::NotSupported
        );
        assert_eq!(
            convert_health_check_error(fidl_next_fdf::HealthCheckError::InvalidState),
            fidl_fdf::HealthCheckError::InvalidState
        );
        assert_eq!(
            convert_health_check_error(fidl_next_fdf::HealthCheckError::Unavailable),
            fidl_fdf::HealthCheckError::Unavailable
        );
        assert_eq!(
            convert_health_check_error(fidl_next_fdf::HealthCheckError::CommunicationError),
            fidl_fdf::HealthCheckError::CommunicationError
        );
        assert_eq!(
            convert_health_check_error(fidl_next_fdf::HealthCheckError::NotFound),
            fidl_fdf::HealthCheckError::NotFound
        );

        // Unknown ordinals round-trip as unknown rather than collapsing to a known member.
        let unknown_status = convert_health_check_response(fidl_next_fdf::HealthCheckResponse {
            diagnostics: None,
            conditions: Some(vec![fidl_next_fdf::HealthCondition {
                status: Some(fidl_next_fdf::HealthStatus::from(99)),
                reason_code: None,
                message: None,
                diagnostics: None,
                first_observed: None,
                last_observed: None,
                occurrence_count: None,
            }]),
        })
        .conditions
        .expect("conditions")[0]
            .status
            .expect("status");
        assert_eq!(unknown_status, fidl_fdf::HealthStatus::from_primitive_allow_unknown(99));
        assert!(unknown_status.is_unknown());
        let unknown_err = convert_health_check_error(fidl_next_fdf::HealthCheckError::from(99));
        assert_eq!(unknown_err, fidl_fdf::HealthCheckError::from_primitive_allow_unknown(99));
        assert!(unknown_err.is_unknown());
    }
}
