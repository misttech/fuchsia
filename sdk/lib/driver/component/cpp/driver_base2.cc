// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.driver.framework/cpp/driver/wire.h>
#include <fidl/fuchsia.driver.framework/cpp/type_conversions.h>
#include <lib/driver/component/cpp/driver_base2.h>
#include <lib/driver/component/cpp/internal/start_args.h>
#include <lib/driver/logging/cpp/logger.h>
#include <lib/fdf/cpp/arena.h>
#include <lib/inspect/component/cpp/component.h>

#include <atomic>

namespace fdf {

DriverContext::DriverContext(fuchsia_driver_framework::DriverStartArgs start_args)
    : start_args_(std::move(start_args)) {
  incoming_ = std::make_unique<Namespace>([ns = std::move(start_args_.incoming())]() mutable {
    ZX_ASSERT(ns.has_value());
    zx::result incoming = Namespace::Create(ns.value());
    ZX_ASSERT_MSG(incoming.is_ok(), "%s", incoming.status_string());
    return std::move(incoming.value());
  }());

  zx::result val = fdf_internal::ProgramValue(program(), "service_connect_validation");
  if (val.is_ok() && val.value() == "true") {
    EnableServiceValidator();
  }
}

void DriverContext::EnableServiceValidator() {
  if (start_args_.node_offers().has_value()) {
    incoming_->SetServiceValidator(
        std::make_optional<ServiceValidator>(start_args_.node_offers().value()));
  } else {
    fdf::info("No node_offers available, not able to enable service validation.");
  }
}

namespace {

const char* HealthStatusToString(std::optional<fuchsia_driver_framework::HealthStatus> status) {
  if (!status.has_value()) {
    return "UNKNOWN";
  }
  switch (status.value()) {
    case fuchsia_driver_framework::HealthStatus::kOk:
      return "OK";
    case fuchsia_driver_framework::HealthStatus::kDegraded:
      return "DEGRADED";
    case fuchsia_driver_framework::HealthStatus::kUnhealthy:
      return "UNHEALTHY";
  }
}

// Records `items` as properties of a new child named `name` under `parent`.
void RecordDiagnostics(inspect::Node& parent, const char* name,
                       const std::vector<fuchsia_driver_framework::HealthDiagnosticItem>& items) {
  inspect::Node diag_node = parent.CreateChild(name);
  for (const auto& item : items) {
    // FIDL natural union accessors return fidl::internal::UnionMemberView, which requires
    // .value() to unwrap the underlying alternative.
    switch (item.value().Which()) {
      case fuchsia_driver_framework::HealthDiagnosticValue::Tag::kStringValue:
        diag_node.RecordString(item.key(), item.value().string_value().value());
        break;
      case fuchsia_driver_framework::HealthDiagnosticValue::Tag::kIntValue:
        diag_node.RecordInt(item.key(), item.value().int_value().value());
        break;
      case fuchsia_driver_framework::HealthDiagnosticValue::Tag::kUintValue:
        diag_node.RecordUint(item.key(), item.value().uint_value().value());
        break;
      case fuchsia_driver_framework::HealthDiagnosticValue::Tag::kBoolValue:
        diag_node.RecordBool(item.key(), item.value().bool_value().value());
        break;
      case fuchsia_driver_framework::HealthDiagnosticValue::Tag::kTimestamp:
        diag_node.RecordInt(item.key(), item.value().timestamp().value().get());
        break;
      default:
        break;
    }
  }
  parent.Record(std::move(diag_node));
}

fuchsia_driver_framework::HealthStatus DeriveHealthStatus(
    const fuchsia_driver_framework::HealthCheckResponse& response) {
  auto overall = fuchsia_driver_framework::HealthStatus::kOk;
  if (response.conditions().has_value()) {
    for (const auto& condition : response.conditions().value()) {
      auto status = condition.status().value_or(fuchsia_driver_framework::HealthStatus::kUnhealthy);
      if (status == fuchsia_driver_framework::HealthStatus::kDegraded) {
        overall = fuchsia_driver_framework::HealthStatus::kDegraded;
      } else if (status != fuchsia_driver_framework::HealthStatus::kOk) {
        return fuchsia_driver_framework::HealthStatus::kUnhealthy;
      }
    }
  }
  return overall;
}

// Builds the root/driver_health_check node for `result` under `root`.
inspect::Node BuildHealthCheckNode(
    inspect::Node& root, std::optional<fuchsia_driver_framework::HealthCheckRequest> level,
    const fit::result<fuchsia_driver_framework::HealthCheckError,
                      fuchsia_driver_framework::HealthCheckResponse>& result) {
  inspect::Node node = root.CreateChild("driver_health_check");
  // Property names and strings match the Rust mirror in fdf_component.
  if (level.has_value()) {
    const char* level_str = "UNKNOWN";
    switch (*level) {
      case fuchsia_driver_framework::HealthCheckRequest::kPing:
        level_str = "PING";
        break;
      case fuchsia_driver_framework::HealthCheckRequest::kFast:
        level_str = "FAST";
        break;
      case fuchsia_driver_framework::HealthCheckRequest::kDeep:
        level_str = "DEEP";
        break;
    }
    node.RecordString("level", level_str);
  }
  if (result.is_error()) {
    const char* error_str = "UNKNOWN";
    switch (result.error_value()) {
      case fuchsia_driver_framework::HealthCheckError::kTimeout:
        error_str = "TIMEOUT";
        break;
      case fuchsia_driver_framework::HealthCheckError::kNotSupported:
        error_str = "NOT_SUPPORTED";
        break;
      case fuchsia_driver_framework::HealthCheckError::kInternalError:
        error_str = "INTERNAL_ERROR";
        break;
      case fuchsia_driver_framework::HealthCheckError::kInvalidState:
        error_str = "INVALID_STATE";
        break;
      case fuchsia_driver_framework::HealthCheckError::kUnavailable:
        error_str = "UNAVAILABLE";
        break;
      case fuchsia_driver_framework::HealthCheckError::kCommunicationError:
        error_str = "COMMUNICATION_ERROR";
        break;
      case fuchsia_driver_framework::HealthCheckError::kNotFound:
        error_str = "NOT_FOUND";
        break;
      default:
        break;
    }
    node.RecordString("status", "ERROR");
    node.RecordString("error", error_str);
    return node;
  }
  const fuchsia_driver_framework::HealthCheckResponse& response = result.value();
  node.RecordString("status", HealthStatusToString(DeriveHealthStatus(response)));
  // The diagnostics and conditions children are only created when non-empty, matching the
  // records driver_manager publishes under root/health_checks.
  if (response.diagnostics().has_value() && !response.diagnostics()->empty()) {
    RecordDiagnostics(node, "diagnostics", response.diagnostics().value());
  }
  if (response.conditions().has_value() && !response.conditions()->empty()) {
    inspect::Node conditions_node = node.CreateChild("conditions");
    for (const auto& condition : response.conditions().value()) {
      inspect::Node condition_node =
          conditions_node.CreateChild(condition.reason_code().value_or("UNKNOWN"));
      condition_node.RecordString("status", HealthStatusToString(condition.status()));
      if (condition.message().has_value()) {
        condition_node.RecordString("message", condition.message().value());
      }
      if (condition.first_observed().has_value()) {
        condition_node.RecordInt("first_observed", condition.first_observed().value().get());
      }
      if (condition.last_observed().has_value()) {
        condition_node.RecordInt("last_observed", condition.last_observed().value().get());
      }
      if (condition.occurrence_count().has_value()) {
        condition_node.RecordUint("occurrence_count", condition.occurrence_count().value());
      }
      if (condition.diagnostics().has_value() && !condition.diagnostics()->empty()) {
        RecordDiagnostics(condition_node, "diagnostics", condition.diagnostics().value());
      }
      conditions_node.Record(std::move(condition_node));
    }
    node.Record(std::move(conditions_node));
  }
  return node;
}

using BuildHealthCheckNodeFn =
    inspect::Node (*)(inspect::Node&, std::optional<fuchsia_driver_framework::HealthCheckRequest>,
                      const fit::result<fuchsia_driver_framework::HealthCheckError,
                                        fuchsia_driver_framework::HealthCheckResponse>&);

// Populated by DriverContext::CreateInspector so BuildHealthCheckNode (and its libinspect
// dependencies) can be dead-stripped by --gc-sections from drivers that never call CreateInspector.
std::atomic<BuildHealthCheckNodeFn> g_build_health_check_node = nullptr;

}  // namespace

inspect::ComponentInspector DriverContext::CreateInspector(DriverBase2* driver,
                                                           inspect::Inspector inspector) const {
  g_build_health_check_node.store(&BuildHealthCheckNode, std::memory_order_relaxed);
  driver->health_check_inspector_ = inspector;
  return inspect::ComponentInspector(
      driver->dispatcher(),
      inspect::PublishOptions{
          .inspector = std::move(inspector),
          .tree_name = {std::string(driver->name())},
          .client_end = incoming().Connect<fuchsia_inspect::InspectSink>().value(),
      });
}

cpp20::span<const fuchsia_driver_framework::NodeProperty2> DriverContext::node_properties(
    const std::string& parent_node_name) const {
  const auto& node_properties = start_args_.node_properties_2();
  if (node_properties.has_value()) {
    for (const auto& entry : node_properties.value()) {
      if (entry.name() == parent_node_name) {
        return entry.properties();
      }
    }
  }
  return {};
}

zx::event DriverContext::take_node_token() {
  ZX_ASSERT(start_args_.node_token().has_value());
  return *std::move(start_args_.node_token());
}

#if FUCHSIA_API_LEVEL_AT_LEAST(HEAD)
std::optional<fidl::ServerEnd<fuchsia_power_broker::ElementRunner>>
DriverContext::take_power_element_runner() {
  if (!start_args_.power_element_args().has_value() ||
      !start_args_.power_element_args()->runner_server().has_value()) {
    return std::nullopt;
  }

  std::optional result = std::move(start_args_.power_element_args()->runner_server());
  start_args_.power_element_args()->runner_server().reset();
  return result;
}

std::optional<fidl::ClientEnd<fuchsia_power_broker::Lessor>>
DriverContext::take_power_element_lessor() {
  if (!start_args_.power_element_args().has_value() ||
      !start_args_.power_element_args()->lessor_client().has_value()) {
    return std::nullopt;
  }

  std::optional result = std::move(start_args_.power_element_args()->lessor_client());
  start_args_.power_element_args()->lessor_client().reset();
  return result;
}

std::optional<fuchsia_power_broker::DependencyToken> DriverContext::power_element_token() {
  if (!start_args_.power_element_args().has_value() ||
      !start_args_.power_element_args()->token().has_value()) {
    return std::nullopt;
  }

  zx::event copy;
  ZX_ASSERT(start_args_.power_element_args()->token()->duplicate(ZX_RIGHT_SAME_RIGHTS, &copy) ==
            ZX_OK);

  return fuchsia_power_broker::DependencyToken(std::move(copy));
}

bool DriverContext::has_power_args() { return start_args_.power_element_args().has_value(); }
#endif

DriverBase2::DriverBase2(std::string_view name)
    : name_(name), driver_dispatcher_(fdf_dispatcher_get_current_dispatcher()) {}

void DriverBase2::DriverBaseInternalInit(DriverContext& context,
                                         fdf::UnownedSynchronizedDispatcher driver_dispatcher) {
  node_ = std::move(context.start_args_.node().value());
  driver_dispatcher_ = std::move(driver_dispatcher);
  logger_ = Logger::Create2(context.incoming(), dispatcher(), name_, FUCHSIA_LOG_INFO
#if FUCHSIA_API_LEVEL_AT_LEAST(32)
                            ,
                            std::move(context.start_args_.log_sink())
#endif
  );
  Logger::SetGlobalInstance(logger_.get());
  std::optional outgoing_request = std::move(context.start_args_.outgoing_dir());
  ZX_ASSERT(outgoing_request.has_value());
  outgoing_ =
      std::make_shared<OutgoingDirectory>(OutgoingDirectory::Create(driver_dispatcher_->get()));
  ZX_ASSERT(outgoing_->Serve(std::move(outgoing_request.value())).is_ok());
}

zx::result<OwnedChildNode> DriverBase2::AddOwnedChild(std::string_view node_name) {
  return fdf::AddOwnedChild(node(), logger(), node_name);
}

zx::result<fidl::ClientEnd<fuchsia_driver_framework::NodeController>> DriverBase2::AddChild(
    std::string_view node_name,
    cpp20::span<const fuchsia_driver_framework::NodeProperty2> properties,
    cpp20::span<const fuchsia_driver_framework::Offer> offers) {
  return fdf::AddChild(node(), logger(), node_name, properties, offers);
}

zx::result<OwnedChildNode> DriverBase2::AddOwnedChild(
    std::string_view node_name, fuchsia_driver_framework::DevfsAddArgs& devfs_args) {
  return fdf::AddOwnedChild(node(), logger(), node_name, devfs_args);
}

zx::result<fidl::ClientEnd<fuchsia_driver_framework::NodeController>> DriverBase2::AddChild(
    std::string_view node_name, fuchsia_driver_framework::DevfsAddArgs& devfs_args,
    cpp20::span<const fuchsia_driver_framework::NodeProperty2> properties,
    cpp20::span<const fuchsia_driver_framework::Offer> offers) {
  return fdf::AddChild(node(), logger(), node_name, devfs_args, properties, offers);
}

#if FUCHSIA_API_LEVEL_AT_LEAST(HEAD)
zx::result<fidl::ClientEnd<fuchsia_driver_framework::ResourceController>>
DriverBase2::ProvideResource(std::string_view node_name,
                             cpp20::span<const fuchsia_driver_framework::NodeProperty2> properties,
                             cpp20::span<const fuchsia_driver_framework::Offer> offers) {
  return fdf::ProvideResource(node(), logger(), node_name, properties, offers);
}
#endif

void DriverBase2::RecordHealthCheckToInspect(
    std::optional<fuchsia_driver_framework::HealthCheckRequest> level,
    const fit::result<fuchsia_driver_framework::HealthCheckError,
                      fuchsia_driver_framework::HealthCheckResponse>& result) {
  BuildHealthCheckNodeFn build_node = g_build_health_check_node.load(std::memory_order_relaxed);
  if (!health_check_inspector_.has_value() || build_node == nullptr) {
    return;
  }
  // Destroy the previous record before creating the replacement so the two never coexist.
  health_check_node_.reset();
  health_check_node_ = build_node(health_check_inspector_->GetRoot(), level, result);
}

void DriverBase2::ReportHealthAlert(fuchsia_driver_framework::HealthAlert alert) {
  // The binding is cleared once the driver has been asked to stop (or the server is destroyed),
  // making this a no-op from then on.
  if (health_alert_binding_ == nullptr) {
    return;
  }
  if (alert.Which() == fuchsia_driver_framework::HealthAlert::Tag::kSelfFault) {
    // Self-reported faults do not come from a health check request, so no level is recorded.
    RecordHealthCheckToInspect(std::nullopt, fit::ok(alert.self_fault().value()));
  }
  fdf::Arena arena('HLTH');
  fidl::OneWayStatus status = fdf::WireSendEvent(*health_alert_binding_)
                                  .buffer(arena)
                                  ->OnHealthAlert(fidl::ToWire(arena, alert));
  // The logger only exists once Start has run.
  if (!status.ok() && logger_) {
    FDF_LOGL(DEBUG, logger(), "Failed to send OnHealthAlert: %s",
             status.FormatDescription().c_str());
  }
}

DriverBase2::~DriverBase2() { Logger::SetGlobalInstance(nullptr); }

}  // namespace fdf
