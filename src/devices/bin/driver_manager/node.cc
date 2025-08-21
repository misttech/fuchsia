// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/bin/driver_manager/node.h"

#include <lib/driver/component/cpp/internal/start_args.h>
#include <lib/driver/component/cpp/node_add_args.h>

#include <algorithm>
#include <deque>
#include <optional>
#include <ranges>
#include <unordered_set>
#include <utility>
#include <variant>

#include <bind/fuchsia/cpp/bind.h>
#include <bind/fuchsia/platform/cpp/bind.h>

#include "src/devices/bin/driver_manager/controller_allowlist_passthrough.h"
#include "src/devices/bin/driver_manager/node_property_conversion.h"
#include "src/devices/bin/driver_manager/shutdown/node_removal_tracker.h"
#include "src/devices/lib/log/log.h"
#include "src/lib/fxl/strings/join_strings.h"

template <>
struct std::formatter<driver_manager::OfferTransport> : std::formatter<std::string_view> {
  auto format(const driver_manager::OfferTransport& transport, std::format_context& ctx) const {
    switch (transport) {
      case driver_manager::OfferTransport::ZirconTransport:
        return std::formatter<std::string_view>::format("ZirconTransport", ctx);
      case driver_manager::OfferTransport::DriverTransport:
        return std::formatter<std::string_view>::format("DriverTransport", ctx);
    }
  }
};

namespace fdf {
using namespace fuchsia_driver_framework;
}  // namespace fdf
namespace fdecl = fuchsia_component_decl;
namespace fcomponent = fuchsia_component;

namespace driver_manager {

namespace {

const std::string kUnboundUrl = "unbound";
const std::string kOwnedByParentUrl = "owned by parent";
const std::string kCompositeParent = "owned by composite(s)";

// TODO(https://fxbug.dev/42075799): Remove this flag once composite node spec rebind once all
// clients are updated to the new Rebind() behavior and this is fully implemented on both DFv1 and
// DFv2.
constexpr bool kEnableCompositeNodeSpecRebind = false;

const char* CollectionName(Collection collection) {
  switch (collection) {
    case Collection::kNone:
      return "";
    case Collection::kBoot:
      return "boot-drivers";
    case Collection::kPackage:
      return "base-drivers";
    case Collection::kFullPackage:
      return "full-drivers";
  }
}

// Processes the offer by validating it has a source_name and adding a source ref to it.
// Returns the offer back out.
fit::result<fdf::NodeError, NodeOffer> ProcessNodeOffer(const fdf::Offer& add_offer,
                                                        Collection source_collection,
                                                        std::string_view source_name) {
  std::optional<OfferTransport> transport;
  std::optional<fdecl::Offer> fdecl_offer;

  switch (add_offer.Which()) {
    case fdf::Offer::Tag::kZirconTransport:
      fdecl_offer.emplace(add_offer.zircon_transport().value());
      transport.emplace(OfferTransport::ZirconTransport);
      break;
    case fdf::Offer::Tag::kDriverTransport:
      fdecl_offer.emplace(add_offer.driver_transport().value());
      transport.emplace(OfferTransport::DriverTransport);
      break;
    default:
      LOGF(ERROR, "Unknown offer transport type %d", add_offer.Which());
      return fit::error(fdf::NodeError::kInternal);
  }

  auto service_offer = fdecl_offer->service();
  if (!service_offer.has_value()) {
    return fit::as_error(fdf::NodeError::kUnsupportedArgs);
  }

  if (!service_offer->source_name().has_value()) {
    return fit::as_error(fdf::NodeError::kOfferSourceNameMissing);
  }

  if (service_offer->target_name().has_value() &&
      service_offer->target_name().value() != service_offer->source_name().value()) {
    return fit::as_error(fdf::NodeError::kUnsupportedArgs);
  }

  if (service_offer->source().has_value() || service_offer->target().has_value()) {
    return fit::as_error(fdf::NodeError::kOfferRefExists);
  }

  if (!service_offer->source_instance_filter().has_value()) {
    return fit::as_error(fdf::NodeError::kOfferSourceInstanceFilterMissing);
  }

  if (!service_offer->renamed_instances().has_value()) {
    return fit::as_error(fdf::NodeError::kOfferRenamedInstancesMissing);
  }

  return fit::ok(NodeOffer{
      .source_name = std::string(source_name),
      .source_collection = source_collection,
      .transport = transport.value(),
      .service_name = service_offer->source_name().value(),
      .source_instance_filter = service_offer->source_instance_filter().value(),
      .renamed_instances = service_offer->renamed_instances().value(),
  });
}

// Processes the offer by validating it has a source_name and adding a source ref to it.
// Returns a tuple containing the offer as well as node property that provides transport
// information for the offer.
fit::result<fdf::NodeError, std::tuple<NodeOffer, fdf::NodeProperty2>>
ProcessNodeOfferWithTransportProperty(const fdf::Offer& add_offer, Collection source_collection,
                                      std::string_view source_name) {
  fit::result result = ProcessNodeOffer(add_offer, source_collection, source_name);
  if (result.is_error()) {
    return result.take_error();
  }

  NodeOffer processed_offer = std::move(result.value());

  const std::string& name = processed_offer.service_name;
  auto node_property =
      fdf::MakeProperty2(name, std::format("{}.{}", name, processed_offer.transport));

  return fit::ok(std::make_tuple(std::move(processed_offer), std::move(node_property)));
}

bool IsDefaultOffer(std::string_view target_name) { return target_name == "default"; }

template <typename T>
void CloseIfExists(std::optional<fidl::ServerBinding<T>>& ref) {
  if (ref) {
    ref->Close(ZX_OK);
  }
}

fit::result<fdf::NodeError> ValidateSymbols(std::vector<fdf::NodeSymbol>& symbols) {
  std::unordered_set<std::string_view> names;
  for (auto& symbol : symbols) {
    if (!symbol.name().has_value()) {
      LOGF(ERROR, "SymbolError: a symbol is missing a name");
      return fit::error(fdf::NodeError::kSymbolNameMissing);
    }
    if (!symbol.address().has_value()) {
      LOGF(ERROR, "SymbolError: symbol '%s' is missing an address", symbol.name().value().c_str());
      return fit::error(fdf::NodeError::kSymbolAddressMissing);
    }
    auto [_, inserted] = names.emplace(symbol.name().value());
    if (!inserted) {
      LOGF(ERROR, "SymbolError: symbol '%s' already exists", symbol.name().value().c_str());
      return fit::error(fdf::NodeError::kSymbolAlreadyExists);
    }
  }
  return fit::ok();
}

}  // namespace

fdf::Offer ToFidl(const NodeOffer& offer) {
  auto service = fdecl::OfferService{{
      .source = fdecl::Ref::WithChild(fdecl::ChildRef{{
          .name = offer.source_name,
          .collection = CollectionName(offer.source_collection),
      }}),
      .source_name = offer.service_name,
      .target_name = offer.service_name,
      .source_instance_filter = offer.source_instance_filter,
      .renamed_instances = offer.renamed_instances,
  }};

  auto fdecl_offer = fdecl::Offer::WithService(service);
  switch (offer.transport) {
    case OfferTransport::ZirconTransport:
      return fdf::Offer::WithZirconTransport(std::move(fdecl_offer));
    case OfferTransport::DriverTransport:
      return fdf::Offer::WithDriverTransport(std::move(fdecl_offer));
  }
}

NodeOffer CreateCompositeOffer(const NodeOffer& offer, std::string_view parents_name,
                               bool primary_parent) {
  size_t new_instance_count = offer.renamed_instances.size();
  if (primary_parent) {
    for (const auto& instance : offer.renamed_instances) {
      if (IsDefaultOffer(instance.target_name())) {
        new_instance_count++;
      }
    }
  }

  size_t new_filter_count = offer.source_instance_filter.size();
  if (primary_parent) {
    for (const auto& filter : offer.source_instance_filter) {
      if (IsDefaultOffer(filter)) {
        new_filter_count++;
      }
    }
  }

  std::vector<fdecl::NameMapping> mappings;
  mappings.reserve(new_instance_count);
  for (const auto& instance : offer.renamed_instances) {
    // The instance is not "default", so copy it over.
    if (!IsDefaultOffer(instance.target_name())) {
      mappings.emplace_back(instance);
      continue;
    }

    // We are the primary parent, so add the "default" offer.
    if (primary_parent) {
      mappings.emplace_back(instance);
    }

    // Rename the instance to match the parent's name.
    mappings.emplace_back(instance.source_name(), std::string(parents_name));
  }
  ZX_ASSERT(mappings.size() == new_instance_count);

  std::vector<std::string> filters;
  filters.reserve(new_filter_count);
  for (const auto& filter : offer.source_instance_filter) {
    // The filter is not "default", so copy it over.
    if (!IsDefaultOffer(filter)) {
      filters.push_back(filter);
      continue;
    }

    // We are the primary parent, so add the "default" filter.
    if (primary_parent) {
      filters.push_back("default");
    }

    // Rename the filter to match the parent's name.
    filters.emplace_back(parents_name);
  }
  ZX_ASSERT(filters.size() == new_filter_count);

  return NodeOffer{
      .source_name = offer.source_name,
      .source_collection = offer.source_collection,
      .transport = offer.transport,
      .service_name = offer.service_name,
      .source_instance_filter = std::move(filters),
      .renamed_instances = std::move(mappings),
  };
}

Node::Node(std::string_view name, std::weak_ptr<Node> parent, NodeManager* node_manager,
           async_dispatcher_t* dispatcher)
    : name_(name),
      type_(Normal{.parent_ = std::move(parent)}),
      node_manager_(node_manager),
      dispatcher_(dispatcher) {
  // By default, we set `driver_host_` to match the primary parent's
  // `driver_host_`. If the node is then subsequently bound to a driver in a
  // different driver host, this value will be updated to match.
  if (auto* parent = GetPrimaryParent(); parent) {
    driver_host_ = parent->driver_host_;
  }
}

Node::Node(std::string_view name, std::vector<std::weak_ptr<Node>> parents,
           std::vector<std::string> parents_names, NodeManager* node_manager,
           async_dispatcher_t* dispatcher, uint32_t primary_index)
    : name_(name),
      type_(Composite{
          .parents_ = std::move(parents),
          .parents_names_ = std::move(parents_names),
          .primary_index_ = primary_index,
      }),
      node_manager_(node_manager),
      dispatcher_(dispatcher) {
  ZX_ASSERT(primary_index < std::get<Composite>(type_).parents_.size());

  // By default, we set `driver_host_` to match the primary parent's
  // `driver_host_`. If the node is then subsequently bound to a driver in a
  // different driver host, this value will be updated to match.
  driver_host_ = GetPrimaryParent()->driver_host_;
}

zx::result<std::shared_ptr<Node>> Node::CreateCompositeNode(
    std::string_view node_name, std::vector<std::weak_ptr<Node>> parents,
    std::vector<std::string> parents_names,
    const std::vector<fuchsia_driver_framework::NodePropertyEntry2>& parent_properties,
    NodeManager* driver_binder, async_dispatcher_t* dispatcher, uint32_t primary_index) {
  ZX_ASSERT(!parents.empty());

  if (parents.size() != parent_properties.size()) {
    LOGF(ERROR,
         "Missing parent properties. Expected %d entries, equal to the number of parents %d.",
         parents.size(), parent_properties.size());
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  if (primary_index >= parents.size()) {
    LOGF(ERROR, "Primary node index is out of bounds");
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  auto primary_node_ptr = parents[primary_index].lock();
  if (!primary_node_ptr) {
    LOGF(ERROR, "Primary node freed before use");
    return zx::error(ZX_ERR_INTERNAL);
  }
  std::shared_ptr composite =
      std::make_shared<Node>(node_name, std::move(parents), std::move(parents_names), driver_binder,
                             dispatcher, primary_index);

  composite->SetCompositeParentProperties(parent_properties);

  Node* primary = composite->GetPrimaryParent();
  // We know that our device has a parent because we're creating it.
  ZX_ASSERT(primary);

  // Copy the symbols from the primary parent.
  composite->symbols_ = primary->symbols_;

  // Copy the dictionary from the primary parent.
  composite->dictionary_ref_ = primary->dictionary_ref_;

  // Copy the offers from each parent.
  std::vector<NodeOffer> node_offers;
  size_t parent_index = 0;
  for (const std::weak_ptr<Node>& parent : std::get<Composite>(composite->type_).parents_) {
    auto parent_ptr = parent.lock();
    if (!parent_ptr) {
      LOGF(ERROR, "Composite parent node freed before use");
      return zx::error(ZX_ERR_INTERNAL);
    }
    auto parent_offers = parent_ptr->offers();
    node_offers.reserve(node_offers.size() + parent_offers.size());

    for (auto& parent_offer : parent_offers) {
      NodeOffer offer = CreateCompositeOffer(
          parent_offer, std::get<Composite>(composite->type_).parents_names_[parent_index],
          parent_index == primary_index);
      node_offers.push_back(std::move(offer));
    }
    parent_index++;
  }
  composite->offers_ = std::move(node_offers);

  composite->AddToParents();
  ZX_ASSERT_MSG(primary->devfs_device_.topological_node().has_value(), "%s",
                composite->MakeTopologicalPath().c_str());

  // TODO(https://fxbug.dev/331779666): disable controller access for composite nodes
  primary->devfs_device_.topological_node().value().add_child(
      composite->name_, std::nullopt,
      composite->CreateDevfsPassthrough(std::nullopt, std::nullopt, true, ""),
      composite->devfs_device_);
  composite->devfs_device_.publish();
  return zx::ok(std::move(composite));
}

Node::~Node() {
  // TODO(https://fxbug.dev/42085057): Notify the NodeRemovalTracker if the node is deallocated
  // before shutdown is complete.
  if (GetNodeState() != NodeState::kStopped) {
    LOGF(INFO, "Node %s deallocating while at state %s", MakeComponentMoniker().c_str(),
         GetNodeShutdownCoordinator().NodeStateAsString());
  }

  CloseIfExists(controller_ref_);
  if (auto* driver_component = std::get_if<DriverComponent>(&state_); driver_component) {
    driver_component->node_ref_.Close(ZX_OK);
  } else if (auto* node = std::get_if<OwnedByParent>(&state_); node) {
    node->node_ref_.Close(ZX_OK);
  }

  for (auto& completer : unbinding_children_completers_) {
    completer.Reply(zx::error(ZX_ERR_CANCELED));
  }

  if (pending_bind_completer_) {
    pending_bind_completer_(zx::error(ZX_ERR_CANCELED));
  }

  if (composite_rebind_completer_) {
    LOGF(WARNING, "Unable to rebind node %s since it deallocated before completing shutdown",
         MakeComponentMoniker().c_str());
    composite_rebind_completer_(zx::error(ZX_ERR_CANCELED));
  }
}

const std::string& Node::driver_url() const {
  if (auto* driver_component = std::get_if<DriverComponent>(&state_); driver_component) {
    return driver_component->driver_url;
  }

  if (auto* starting = std::get_if<Starting>(&state_); starting) {
    return starting->driver_url;
  }

  if (auto* quarantined = std::get_if<Quarantined>(&state_); quarantined) {
    return quarantined->driver_url;
  }

  if (std::holds_alternative<OwnedByParent>(state_)) {
    return kOwnedByParentUrl;
  }

  if (std::holds_alternative<CompositeParent>(state_)) {
    return kCompositeParent;
  }

  return kUnboundUrl;
}

std::string Node::MakeTopologicalPath() const {
  std::deque<std::string_view> names;
  for (auto node = this; node != nullptr; node = node->GetPrimaryParent()) {
    names.push_front(node->name());
  }
  return fxl::JoinStrings(names, "/");
}

std::string Node::MakeComponentMoniker() const {
  std::string topo_path = MakeTopologicalPath();

  // The driver's component name is based on the node name, which means that the
  // node name cam only have [a-z0-9-_.] characters. DFv1 composites contain ':'
  // which is not allowed, so replace those characters.
  // TODO(https://fxbug.dev/42062456): Migrate driver names to only use CF valid characters.
  std::replace(topo_path.begin(), topo_path.end(), ':', '_');
  // Since we use '.' to denote topology, replace them with '_'.
  std::replace(topo_path.begin(), topo_path.end(), '.', '_');
  std::replace(topo_path.begin(), topo_path.end(), '/', '.');
  return topo_path;
}

void Node::OnBind() const {
  if (controller_ref_) {
    zx::event node_token;
    zx_status_t status = std::get<DriverComponent>(state_).component_instance.duplicate(
        ZX_RIGHT_SAME_RIGHTS, &node_token);
    if (status != ZX_OK) {
      LOGF(ERROR, "Failed to send OnBind event: %s", zx_status_get_string(status));
      return;
    }

    fit::result result =
        fidl::SendEvent(*controller_ref_)->OnBind({{.node_token = std::move(node_token)}});
    if (result.is_error()) {
      LOGF(ERROR, "Failed to send OnBind event: %s",
           result.error_value().FormatDescription().c_str());
    }
  }
}

void Node::handle_unknown_method(
    fidl::UnknownMethodMetadata<fuchsia_component_runner::ComponentController> metadata,
    fidl::UnknownMethodCompleter::Sync& completer) {
  LOGF(INFO, "Unknown ComponentController method request received: %lu", metadata.method_ordinal);
}

void Node::Stop(StopCompleter::Sync& completer) {
  LOGF(DEBUG, "Calling Remove on %s because of Stop() from component framework.", name().c_str());
  Remove(RemovalSet::kAll, nullptr);
}

void Node::Kill(KillCompleter::Sync& completer) {
  LOGF(DEBUG, "Calling Remove on %s because of Kill() from component framework.", name().c_str());
  Remove(RemovalSet::kAll, nullptr);
}

void Node::CompleteBind(zx::result<> result) {
  ZX_ASSERT(!std::holds_alternative<OwnedByParent>(state_));

  if (result.is_error()) {
    LOGF(WARNING, "Bind failed for node '%s'", MakeComponentMoniker().c_str());
    if (GetNodeState() == NodeState::kRunning && !std::holds_alternative<Unbound>(state_)) {
      LOGF(DEBUG, "Quarantining node '%s'", MakeComponentMoniker().c_str());
      QuarantineNode();
    } else {
      state_ = Unbound{};
    }
  }

  if (auto* driver_component = std::get_if<DriverComponent>(&state_); driver_component) {
    ZX_ASSERT_MSG(driver_component->state == DriverState::kBinding,
                  "Node %s CompleteBind() invoked at invalid state %d", name().c_str(),
                  driver_component->state);
    driver_component->state = DriverState::kRunning;
    OnBind();
  }

  if (pending_bind_completer_) {
    pending_bind_completer_(result);
  }

  GetNodeShutdownCoordinator().CheckNodeState();
}

void Node::AddToParents() {
  auto this_node = shared_from_this();
  for (auto& parent : parents()) {
    if (auto ptr = parent.lock(); ptr) {
      ptr->children_.push_back(this_node);
      continue;
    }
    LOGF(WARNING, "Parent freed before child %s could be added to it", name().c_str());
  }
}

NodeShutdownCoordinator& Node::GetNodeShutdownCoordinator() {
  if (!node_shutdown_coordinator_) {
    bool is_shutdown_test_delay_enabled =
        node_manager_.has_value() && node_manager_.value()->IsTestShutdownDelayEnabled();
    auto shutdown_rng = node_manager_.has_value() ? node_manager_.value()->GetShutdownTestRng()
                                                  : std::weak_ptr<std::mt19937>();
    node_shutdown_coordinator_ = std::make_unique<NodeShutdownCoordinator>(
        this, dispatcher_, is_shutdown_test_delay_enabled, shutdown_rng);
  }
  return *node_shutdown_coordinator_;
}

// TODO(https://fxbug.dev/42075799): If the node invoking this function cannot multibind to
// composites, is parenting one composite node, and is not in a state for removal, then it should
// attempt to bind to something else.
void Node::RemoveChild(const std::shared_ptr<Node>& child) {
  LOGF(DEBUG, "RemoveChild %s from parent %s", child->name().c_str(), name().c_str());
  std::erase(children_, child);
  if (!unbinding_children_completers_.empty() && children_.empty()) {
    for (auto& completer : unbinding_children_completers_) {
      completer.ReplySuccess();
    }
    unbinding_children_completers_.clear();
  }
  GetNodeShutdownCoordinator().CheckNodeState();
}

void Node::FinishShutdown(fit::callback<void()> shutdown_callback) {
  ZX_ASSERT_MSG(GetNodeState() == NodeState::kWaitingOnDriverComponent,
                "FinishShutdown called in invalid node state: %s",
                GetNodeShutdownCoordinator().NodeStateAsString());
  if (shutdown_intent() == ShutdownIntent::kRestart) {
    LOGF(DEBUG, "Node: %s finishing restart", name().c_str());
    shutdown_callback();
    FinishRestart();
    return;
  }

  if (shutdown_intent() == ShutdownIntent::kQuarantine) {
    LOGF(DEBUG, "Node: %s finishing quarantine", name().c_str());
    shutdown_callback();
    FinishQuarantine();
    return;
  }

  LOGF(DEBUG, "Node: %s finishing shutdown", name().c_str());
  CloseIfExists(controller_ref_);
  if (auto* driver_component = std::get_if<DriverComponent>(&state_); driver_component) {
    driver_component->node_ref_.Close(ZX_OK);
  } else if (auto* node = std::get_if<OwnedByParent>(&state_); node) {
    node->node_ref_.Close(ZX_OK);
  }
  devfs_device_.unpublish();

  // Store a shared_ptr to ourselves so we won't be freed halfway through this function.
  std::shared_ptr this_node = shared_from_this();
  for (auto& parent : parents()) {
    if (auto ptr = parent.lock(); ptr) {
      ptr->RemoveChild(this_node);
      continue;
    }
    LOGF(WARNING, "Parent freed before child %s could be removed from it", name().c_str());
  }
  state_ = Unbound{};

  if (IsComposite()) {
    std::get<Composite>(type_).parents_.clear();
  } else {
    std::get<Normal>(type_).parent_ = {};
  }

  shutdown_callback();

  if (remove_complete_callback_) {
    remove_complete_callback_();
  }

  if (shutdown_intent() == ShutdownIntent::kRebindComposite && composite_rebind_completer_) {
    composite_rebind_completer_(zx::ok());
  }
}

void Node::FinishRestart() {
  ZX_ASSERT_MSG(shutdown_intent() == ShutdownIntent::kRestart,
                "FinishRestart called when node is not restarting.");

  GetNodeShutdownCoordinator().ResetShutdown();

  // Store previous url before we reset the state_.
  std::string previous_url = driver_url();

  // Perform cleanups for previous driver before we try to start the next driver.
  if (auto* driver_component = std::get_if<DriverComponent>(&state_); driver_component) {
    driver_component->node_ref_.Close(ZX_OK);
  } else if (auto* node = std::get_if<OwnedByParent>(&state_); node) {
    node->node_ref_.Close(ZX_OK);
  }
  state_ = Unbound{};

  if (restart_driver_url_suffix_.has_value()) {
    auto tracker = CreateBindResultTracker();
    node_manager_.value()->BindToUrl(*this, restart_driver_url_suffix_.value(), std::move(tracker));
    restart_driver_url_suffix_.reset();
    return;
  }

  zx::result start_result =
      node_manager_.value()->StartDriver(*this, previous_url, driver_package_type_);
  if (start_result.is_error()) {
    LOGF(ERROR, "Failed to start driver '%s': %s", name().c_str(), start_result.status_string());
  }
}

void Node::FinishQuarantine() {
  ZX_ASSERT_MSG(shutdown_intent() == ShutdownIntent::kQuarantine,
                "FinishQuarantine called when node is not quarantining.");

  GetNodeShutdownCoordinator().ResetShutdown();

  // |QuarantineNode()| sets this.
  ZX_ASSERT_MSG(std::holds_alternative<Quarantined>(state_),
                "Node::state_ was not set to Quarantined");
}

void Node::ClearHostDriver() {
  if (auto* driver_component = std::get_if<DriverComponent>(&state_); driver_component) {
    driver_component->driver = {};
  }
}

// State table for package driver:
//                                   Initial States
//                 Running | Prestop|  WoC   | WoDriver | Stopping
// Remove(kPkg)      WoC   |  WoC   | Ignore |  Error!  |  Error!
// Remove(kAll)      WoC   |  WoC   |  WoC   |  Error!  |  Error!
// children empty    N/A   |  N/A   |WoDriver|  Error!  |  Error!
// Driver exit       WoC   |  WoC   |  WoC   | Stopping |  Error!
//
// State table for boot driver:
//                                   Initial States
//                  Running | Prestop |  WoC   | WoDriver | Stopping
// Remove(kPkg)     Prestop | Ignore  | Ignore |  Ignore  |  Ignore
// Remove(kAll)      WoC    |   WoC   | Ignore |  Ignore  |  Ignore
// children empty    N/A    |   N/A   |WoDriver|  Ignore  |  Ignore
// Driver exit       WoC    |   WoC   |  WoC   | Stopping |  Ignore
// Boot drivers go into the Prestop state when Remove(kPackage) is set, to signify that
// a removal is taking place, but this node will not be removed yet, even if all its children
// are removed.
void Node::Remove(RemovalSet removal_set, NodeRemovalTracker* removal_tracker) {
  GetNodeShutdownCoordinator().Remove(shared_from_this(), removal_set, removal_tracker);
}

void Node::RestartNode() {
  GetNodeShutdownCoordinator().set_shutdown_intent(ShutdownIntent::kRestart);
  Remove(RemovalSet::kAll, nullptr);
}

void Node::QuarantineNode() {
  // Perform cleanups for previous driver.
  if (auto* driver_component = std::get_if<DriverComponent>(&state_); driver_component) {
    driver_component->node_ref_.Close(ZX_OK);
  } else {
    ZX_ASSERT(std::holds_alternative<Starting>(state_));
  }
  state_ = Quarantined{.driver_url = driver_url()};

  GetNodeShutdownCoordinator().set_shutdown_intent(ShutdownIntent::kQuarantine);
  Remove(RemovalSet::kAll, nullptr);
}

// TODO(https://fxbug.dev/42082343): Handle the case in which this function is called during node
// removal.
void Node::RestartNodeWithRematch(std::optional<std::string> restart_driver_url_suffix,
                                  fit::callback<void(zx::result<>)> completer) {
  if (pending_bind_completer_) {
    completer(zx::error(ZX_ERR_ALREADY_EXISTS));
    return;
  }

  pending_bind_completer_ = std::move(completer);
  restart_driver_url_suffix_ = std::move(restart_driver_url_suffix);
  RestartNode();
}

void Node::RestartNodeWithRematch() {
  RestartNodeWithRematch("", [](zx::result<> result) {});
}

// TODO(https://fxbug.dev/42082343): Handle the case in which this function is called during node
// removal.
void Node::RemoveCompositeNodeForRebind(fit::callback<void(zx::result<>)> completer) {
  if (composite_rebind_completer_) {
    completer(zx::error(ZX_ERR_ALREADY_EXISTS));
    return;
  }

  if (!IsComposite()) {
    completer(zx::error(ZX_ERR_NOT_SUPPORTED));
    return;
  }

  composite_rebind_completer_ = std::move(completer);
  GetNodeShutdownCoordinator().set_shutdown_intent(ShutdownIntent::kRebindComposite);
  Remove(RemovalSet::kAll, nullptr);
}

std::shared_ptr<BindResultTracker> Node::CreateBindResultTracker() {
  return std::make_shared<BindResultTracker>(
      1, [weak_self = weak_from_this()](
             fidl::VectorView<fuchsia_driver_development::wire::NodeBindingInfo> info) {
        std::shared_ptr self = weak_self.lock();
        if (!self) {
          return;
        }
        // We expect a single successful "bind". If we don't get it, we can assume the bind
        // request failed. If we do get it, we will continue to wait for the driver's start hook
        // to complete, which will only occur after the successful bind. The remaining flow will
        // be similar to the RestartNode flow.
        if (info.size() < 1) {
          // Failed binding attempt should make the node have an unbound url. Reset this in case
          // there was a previous driver on this node that had failed to start and was stored
          // as Quarantined{} in state_ as part of the node quarantining.
          self->state_ = Unbound{};

          self->CompleteBind(zx::error(ZX_ERR_NOT_FOUND));
        } else if (info.size() > 1) {
          LOGF(ERROR, "Unexpectedly bound multiple drivers to a single node");
          self->CompleteBind(zx::error(ZX_ERR_BAD_STATE));
        }
      });
}

std::vector<Node::Property> Node::ToProperty(std::span<const fdf::NodeProperty2> properties) {
  std::vector<Property> node_properties;
  node_properties.reserve(properties.size());
  std::ranges::transform(properties, std::back_inserter(node_properties), [](const auto& property) {
    if (const auto& val = property.value().string_value(); val) {
      return Property{
          .key = property.key(),
          .value = val.value(),
      };
    }
    if (const auto& val = property.value().bool_value(); val) {
      return Property{
          .key = property.key(),
          .value = val.value(),
      };
    }
    if (const auto& val = property.value().int_value(); val) {
      return Property{
          .key = property.key(),
          .value = val.value(),
      };
    }
    if (const auto& val = property.value().enum_value(); val) {
      return Property{
          .key = property.key(),
          .value = EnumValue{.value = val.value()},
      };
    }
    return Property{
        .key = property.key(),
        .value = std::monostate(),
    };
  });
  return node_properties;
}

std::vector<fdf::NodeProperty2> Node::PropertyToFidl(std::span<const Property> properties) {
  std::vector<fdf::NodeProperty2> node_properties;
  node_properties.reserve(properties.size());
  std::ranges::transform(properties, std::back_inserter(node_properties), [](const auto& property) {
    if (const auto* val = std::get_if<std::string>(&property.value); val) {
      return fdf::NodeProperty2{{
          .key = property.key,
          .value = fdf::NodePropertyValue::WithStringValue(*val),
      }};
    }
    if (const auto* val = std::get_if<bool>(&property.value); val) {
      return fdf::NodeProperty2{{
          .key = property.key,
          .value = fdf::NodePropertyValue::WithBoolValue(*val),
      }};
    }
    if (const auto* val = std::get_if<uint32_t>(&property.value); val) {
      return fdf::NodeProperty2{{
          .key = property.key,
          .value = fdf::NodePropertyValue::WithIntValue(*val),
      }};
    }
    if (const auto* val = std::get_if<EnumValue>(&property.value); val) {
      return fdf::NodeProperty2{{
          .key = property.key,
          .value = fdf::NodePropertyValue::WithEnumValue(val->value),
      }};
    }
    return fdf::NodeProperty2{{
        .key = property.key,
        .value = fdf::NodePropertyValue::WithStringValue("UNKNOWN"),
    }};
  });

  return node_properties;
}

void Node::SetNonCompositeProperties(std::span<const fdf::NodeProperty2> properties) {
  properties_ = std::vector<PropertiesEntry>{
      PropertiesEntry{
          .name = "default",
          .properties = ToProperty(properties),
      },
  };
}

void Node::SetCompositeParentProperties(const fdf::NodePropertyDictionary2& parent_properties) {
  std::vector<PropertiesEntry> properties;
  properties.reserve(parent_properties.size() + 1);
  std::ranges::transform(parent_properties, std::back_inserter(properties), [](const auto& entry) {
    return PropertiesEntry{
        .name = entry.name(),
        .properties = ToProperty(entry.properties()),
    };
  });

  auto& composite = std::get<Composite>(type_);
  ZX_ASSERT(composite.primary_index_ < composite.parents_.size());
  const auto& default_node_properties = parent_properties[composite.primary_index_].properties();
  properties.emplace_back(PropertiesEntry{
      .name = "default",
      .properties = ToProperty(default_node_properties),
  });

  properties_ = std::move(properties);
}

std::vector<fdf::BusInfo> Node::GetBusTopology() const {
  std::vector<fdf::BusInfo> segments;
  for (auto node = this; node != nullptr; node = node->GetPrimaryParent()) {
    if (node->bus_info_.has_value()) {
      segments.push_back(node->bus_info_.value());
    }
  }
  std::ranges::reverse(segments);
  return segments;
}

Node::OwnedByParent::OwnedByParent(fidl::ServerEnd<fdf::Node> node, Node* child)
    : node_ref_(child->dispatcher_, std::move(node), child,
                [](Node* node, fidl::UnbindInfo info) { node->OnNodeServerUnbound(info); }) {}

fit::result<fdf::NodeError, std::shared_ptr<Node>> Node::AddChildHelper(
    fuchsia_driver_framework::NodeAddArgs args,
    fidl::ServerEnd<fuchsia_driver_framework::NodeController> controller,
    fidl::ServerEnd<fuchsia_driver_framework::Node> node) {
  if (!unbinding_children_completers_.empty()) {
    LOGF(ERROR, "Failed to add node: Node is currently unbinding all of its children");
    return fit::as_error(fdf::NodeError::kUnbindChildrenInProgress);
  }
  if (node_manager_ == nullptr) {
    LOGF(WARNING, "Failed to add Node, as this Node '%s' was removed", name().data());
    return fit::as_error(fdf::NodeError::kNodeRemoved);
  }
  if (GetNodeShutdownCoordinator().IsShuttingDown()) {
    LOGF(WARNING, "Failed to add Node, as this Node '%s' is being removed", name().c_str());
    return fit::as_error(fdf::NodeError::kNodeRemoved);
  }
  if (!args.name().has_value()) {
    LOGF(ERROR, "Failed to add Node, a name must be provided");
    return fit::as_error(fdf::NodeError::kNameMissing);
  }
  std::string_view name = args.name().value();
  for (auto& child : children_) {
    if (child->name() == name) {
      LOGF(ERROR, "Failed to add Node '%.*s', name already exists among siblings",
           static_cast<int>(name.size()), name.data());
      return fit::as_error(fdf::NodeError::kNameAlreadyExists);
    }
  };
  std::shared_ptr child =
      std::make_shared<Node>(name, weak_from_this(), *node_manager_, dispatcher_);

  auto& fdf_offers = args.offers2();
  std::vector<fuchsia_driver_framework::NodeProperty2> properties;

  const auto& arg_properties = args.properties2();
  if (arg_properties.has_value()) {
    properties = arg_properties.value();
  }

  const auto& arg_deprecated_properties = args.properties();
  if (arg_deprecated_properties.has_value()) {
    if (arg_properties.has_value()) {
      LOGF(
          ERROR,
          "Failed to add Node '%.*s'. Found values for both properties and properties2 are set. Only one of the fields can be set.",
          static_cast<int>(name.size()), name.data());
      return fit::as_error(fdf::NodeError::kUnsupportedArgs);
    }

    properties.reserve(arg_deprecated_properties->size());
    for (auto& property : arg_deprecated_properties.value()) {
      if (property.key().Which() == fuchsia_driver_framework::NodePropertyKey::Tag::kIntValue) {
        LOGF(ERROR,
             "Failed to add Node '%.*s'. Found integer-based key %zu which is no longer supported.",
             static_cast<int>(name.size()), name.data(), property.key().int_value().value());
        return fit::as_error(fdf::NodeError::kUnsupportedArgs);
      }
      properties.emplace_back(ToProperty2(property));
    }
  }

  if (fdf_offers.has_value()) {
    size_t n = 0;
    if (fdf_offers.has_value()) {
      n = fdf_offers.value().size();
    }
    child->offers_.reserve(n);

    // Find a parent node with a collection. This indicates that a driver has
    // been bound to the node, and the driver is running within the collection.
    Node* source_node = this;
    while (source_node && source_node->collection_ == Collection::kNone) {
      source_node = source_node->GetPrimaryParent();
    }
    std::string source_name = source_node->MakeComponentMoniker();
    Collection source_collection = source_node->collection_;

    if (fdf_offers.has_value()) {
      for (auto& fdf_offer : fdf_offers.value()) {
        fit::result new_offer =
            ProcessNodeOfferWithTransportProperty(fdf_offer, source_collection, source_name);
        if (new_offer.is_error()) {
          LOGF(ERROR, "Failed to add Node '%s': Bad add offer: %d",
               child->MakeTopologicalPath().c_str(), new_offer.error_value());
          return new_offer.take_error();
        }
        auto [processed_offer, property] = std::move(new_offer.value());
        child->offers_.emplace_back(processed_offer);
        properties.emplace_back(property);
      }
    }
  }

  child->bus_info_ = std::move(args.bus_info());

  // Copy the dictionary of a parent node down to the child.
  child->dictionary_ref_ = dictionary_ref_;

  child->SetNonCompositeProperties(properties);

  if (auto& symbols = args.symbols(); symbols.has_value()) {
    auto is_valid = ValidateSymbols(symbols.value());
    if (is_valid.is_error()) {
      LOGF(ERROR, "Failed to add Node '%.*s', bad symbols", static_cast<int>(name.size()),
           name.data());
      return fit::as_error(is_valid.error_value());
    }

    child->symbols_ = std::move(args.symbols().value());
  }

  Devnode::Target devfs_target;
  std::optional<std::string_view> devfs_class_path;
  std::string class_name = "Unknown_Class_name";
  auto& devfs_args = args.devfs_args();
  if (devfs_args.has_value()) {
    if (devfs_args->class_name().has_value()) {
      devfs_class_path = devfs_args->class_name();
      class_name = std::string(devfs_args->class_name().value());
    }
    // We do not populate the connection to the controller unless it is specifically
    // supported through the connector_supports argument.
    bool allow_controller_connection = (devfs_args->connector_supports().has_value() &&
                                        (devfs_args->connector_supports().value() &
                                         fuchsia_device_fs::ConnectionType::kController));
    if (allow_controller_connection && !devfs_args->class_name().has_value()) {
      class_name = "No_class_name_but_driver_url_is_" + driver_url();
    }

    devfs_target = child->CreateDevfsPassthrough(std::move(devfs_args->connector()),
                                                 std::move(devfs_args->controller_connector()),
                                                 allow_controller_connection, class_name);
  } else {
    devfs_target = child->CreateDevfsPassthrough(std::nullopt, std::nullopt, false, class_name);
  }
  ZX_ASSERT(devfs_device_.topological_node().has_value());
  zx_status_t status = devfs_device_.topological_node()->add_child(
      child->name_, devfs_class_path, std::move(devfs_target), child->devfs_device_);
  ZX_ASSERT_MSG(status == ZX_OK, "%s failed to export: %s", child->MakeTopologicalPath().c_str(),
                zx_status_get_string(status));
  ZX_ASSERT(child->devfs_device_.topological_node().has_value());
  child->devfs_device_.publish();

  if (controller.is_valid()) {
    child->controller_ref_.emplace(dispatcher_, std::move(controller), child.get(),
                                   fidl::kIgnoreBindingClosure);
  }
  if (node.is_valid()) {
    child->state_.emplace<OwnedByParent>(std::move(node), child.get());
  } else {
    // We don't care about tracking binds here, sending nullptr is fine.
    (*node_manager_)->Bind(*child, nullptr);
  }

  child->AddToParents();
  return fit::ok(child);
}

void Node::WaitForChildToExit(std::string_view name,
                              fit::callback<void(fit::result<fdf::NodeError>)> callback) {
  for (auto& child : children_) {
    if (child->name() != name) {
      continue;
    }
    if (!child->GetNodeShutdownCoordinator().IsShuttingDown()) {
      LOGF(ERROR, "Failed to add Node '%.*s', name already exists among siblings",
           static_cast<int>(name.size()), name.data());
      callback(fit::as_error(fdf::NodeError::kNameAlreadyExists));
      return;
    }
    if (child->remove_complete_callback_) {
      LOGF(ERROR,
           "Failed to add Node '%.*s': Node with name already exists and is marked to be replaced.",
           static_cast<int>(name.size()), name.data());
      callback(fit::as_error(fdf::NodeError::kNameAlreadyExists));
      return;
    }
    child->remove_complete_callback_ = [callback = std::move(callback)]() mutable {
      callback(fit::success());
    };
    return;
  };
  callback(fit::success());
}

void Node::AddChild(fuchsia_driver_framework::NodeAddArgs args,
                    fidl::ServerEnd<fuchsia_driver_framework::NodeController> controller,
                    fidl::ServerEnd<fuchsia_driver_framework::Node> node,
                    AddNodeResultCallback callback) {
  if (!args.name().has_value()) {
    LOGF(ERROR, "Failed to add Node, a name must be provided");
    callback(fit::as_error(fdf::NodeError::kNameMissing));
    return;
  }

  // Verify the properties.
  if (args.properties().has_value() && args.properties2().has_value()) {
    LOGF(ERROR, "Failed to add Node, both properties and properties2 fields were set");
    callback(fit::as_error(fdf::NodeError::kUnsupportedArgs));
    return;
  }

  // Only check for unique property keys for properties2 since properties is deprecated.
  if (args.properties2().has_value()) {
    std::unordered_set<std::string> property_keys;
    for (auto& property : args.properties2().value()) {
      if (property_keys.contains(property.key())) {
        LOGF(ERROR,
             "Failed to add Node since properties2 contain multiple properties with the same key");
        callback(fit::as_error(fdf::NodeError::kDuplicatePropertyKeys));
        return;
      }
      property_keys.insert(property.key());
    }
  }

  std::string name = args.name().value();
  WaitForChildToExit(
      name, [self = shared_from_this(), args = std::move(args), controller = std::move(controller),
             node = std::move(node),
             callback = std::move(callback)](fit::result<fdf::NodeError> result) mutable {
        if (result.is_error()) {
          callback(result.take_error());
          return;
        }
        callback(self->AddChildHelper(std::move(args), std::move(controller), std::move(node)));
      });
}

void Node::OnNodeServerUnbound(fidl::UnbindInfo info) {
  // If the unbind is initiated from us, we don't need to do anything to handle
  // the closure.
  if (info.is_user_initiated()) {
    return;
  }

  // If the driver fails to bind to the node, don't remove the node.
  if (auto* driver_component = std::get_if<DriverComponent>(&state_);
      driver_component && driver_component->state == DriverState::kBinding) {
    LOGF(WARNING, "The driver for node %s failed to bind.", name().c_str());
    return;
  }

  if (GetNodeState() == NodeState::kRunning) {
    // If the node is running but this node closure has happened, then we want to restart
    // the node if it has the host_restart_on_crash_ enabled on it.
    if (host_restart_on_crash_) {
      LOGF(INFO, "Restarting node %s due to node closure while running.", name().c_str());
      RestartNode();
      return;
    }

    LOGF(WARNING, "fdf::Node binding for node %s closed while the node was running: %s",
         name().c_str(), info.FormatDescription().c_str());
  }

  Remove(RemovalSet::kAll, nullptr);
}

void Node::Remove(RemoveCompleter::Sync& completer) {
  LOGF(DEBUG, "Remove() Fidl call for %s", name().c_str());
  Remove(RemovalSet::kAll, nullptr);
}

void Node::RequestBind(RequestBindRequestView request, RequestBindCompleter::Sync& completer) {
  bool force_rebind = false;
  if (request->has_force_rebind()) {
    force_rebind = request->force_rebind();
  }

  std::optional<std::string> driver_url_suffix;
  if (request->has_driver_url_suffix()) {
    driver_url_suffix = std::string(request->driver_url_suffix().get());
  }

  BindHelper(force_rebind, std::move(driver_url_suffix),
             [completer = completer.ToAsync()](zx_status_t status) mutable {
               if (status == ZX_OK) {
                 completer.ReplySuccess();
               } else {
                 completer.ReplyError(status);
               }
             });
}

void Node::BindHelper(bool force_rebind, std::optional<std::string> driver_url_suffix,
                      fit::callback<void(zx_status_t)> on_bind_complete) {
  if (std::holds_alternative<DriverComponent>(state_) && !force_rebind) {
    on_bind_complete(ZX_ERR_ALREADY_BOUND);
    return;
  }

  if (pending_bind_completer_) {
    on_bind_complete(ZX_ERR_ALREADY_EXISTS);
    return;
  }

  auto completer_wrapper = [on_bind_complete =
                                std::move(on_bind_complete)](zx::result<> result) mutable {
    on_bind_complete(result.status_value());
  };

  if (std::holds_alternative<DriverComponent>(state_)) {
    RestartNodeWithRematch(driver_url_suffix, std::move(completer_wrapper));
    return;
  }

  pending_bind_completer_ = std::move(completer_wrapper);
  auto tracker = CreateBindResultTracker();
  if (driver_url_suffix.has_value()) {
    node_manager_.value()->BindToUrl(*this, driver_url_suffix.value(), std::move(tracker));
  } else {
    node_manager_.value()->Bind(*this, std::move(tracker));
  }
}

void Node::handle_unknown_method(
    fidl::UnknownMethodMetadata<fuchsia_driver_framework::NodeController> metadata,
    fidl::UnknownMethodCompleter::Sync& completer) {
  std::string method_type;
  switch (metadata.unknown_method_type) {
    case fidl::UnknownMethodType::kOneWay:
      method_type = "one-way";
      break;
    case fidl::UnknownMethodType::kTwoWay:
      method_type = "two-way";
      break;
  };

  LOGF(WARNING, "fdf::NodeController received unknown %s method. Ordinal: %lu", method_type.c_str(),
       metadata.method_ordinal);
}

void Node::AddChild(AddChildRequestView request, AddChildCompleter::Sync& completer) {
  AddChild(fidl::ToNatural(request->args), std::move(request->controller), std::move(request->node),
           [completer = completer.ToAsync()](
               fit::result<fdf::NodeError, std::shared_ptr<Node>> result) mutable {
             if (result.is_error()) {
               completer.Reply(result.take_error());
             } else {
               completer.ReplySuccess();
             }
           });
}

void Node::handle_unknown_method(
    fidl::UnknownMethodMetadata<fuchsia_driver_framework::Node> metadata,
    fidl::UnknownMethodCompleter::Sync& completer) {
  std::string method_type;
  switch (metadata.unknown_method_type) {
    case fidl::UnknownMethodType::kOneWay:
      method_type = "one-way";
      break;
    case fidl::UnknownMethodType::kTwoWay:
      method_type = "two-way";
      break;
  };

  LOGF(WARNING, "fdf::Node received unknown %s method. Ordinal: %lu", method_type.c_str(),
       metadata.method_ordinal);
}

void Node::StartDriver(fuchsia_component_runner::wire::ComponentStartInfo start_info,
                       fidl::ServerEnd<fuchsia_component_runner::ComponentController> controller,
                       fit::callback<void(zx::result<>)> cb) {
  auto url = start_info.resolved_url().get();
  bool colocate =
      fdf_internal::ProgramValue(start_info.program(), "colocate").value_or("") == "true";
  bool host_restart_on_crash =
      fdf_internal::ProgramValue(start_info.program(), "host_restart_on_crash").value_or("") ==
      "true";
  bool use_next_vdso =
      fdf_internal::ProgramValue(start_info.program(), "use_next_vdso").value_or("") == "true";
  bool use_dynamic_linker =
      fdf_internal::ProgramValue(start_info.program(), "use_dynamic_linker").value_or("") == "true";

  state_ = Starting{.driver_url = std::string(url)};

  if (host_restart_on_crash && colocate) {
    LOGF(ERROR,
         "Failed to start driver '%.*s'. Both host_restart_on_crash and colocate cannot be true.",
         static_cast<int>(url.size()), url.data());
    cb(zx::error(ZX_ERR_INVALID_ARGS));
    return;
  }

  host_restart_on_crash_ = host_restart_on_crash;

  if (colocate && !driver_host_) {
    LOGF(ERROR,
         "Failed to start driver '%.*s', driver is colocated but does not have a prent with a "
         "driver host",
         static_cast<int>(url.size()), url.data());
    cb(zx::error(ZX_ERR_INVALID_ARGS));
    return;
  }

  fidl::Arena arena;

  auto symbols = fidl::VectorView<fdf::wire::NodeSymbol>();
  if (colocate) {
    symbols = fidl::ToWire(arena, this->symbols());
  }

  std::vector<fdf::Offer> natural_offers;
  natural_offers.reserve(offers_.size());
  std::ranges::transform(offers_, std::back_inserter(natural_offers), ToFidl);
  auto offers = fidl::ToWire(arena, natural_offers);
  auto properties = fidl::ToWire(arena, GetNodePropertyDict());

  if (colocate) {
    // Whether dynamic linking is enabled for a driver host is determined by the first driver in the
    // host. Otherwise for colocated drivers, we need to match what has been set for the driver
    // host.
    if (use_dynamic_linker != driver_host()->IsDynamicLinkingEnabled()) {
      LOGF(ERROR,
           "Failed to start driver '%.*s', driver is colocated and set use_dynamic_linker=%s "
           "but its driver host is not configured for this",
           static_cast<int>(url.size()), url.data(), use_dynamic_linker ? "true" : "false");
      cb(zx::error(ZX_ERR_INVALID_ARGS));
      return;
    }
  }
  std::optional<DriverHost::DriverLoadArgs> dynamic_linker_load_args;
  std::optional<DriverHost::DriverStartArgs> dynamic_linker_start_args;
  if (use_dynamic_linker) {
    auto result = DriverHost::DriverLoadArgs::Create(start_info);
    if (result.is_error()) {
      cb(result.take_error());
      return;
    }
    dynamic_linker_load_args = std::move(*result);
    dynamic_linker_start_args =
        DriverHost::DriverStartArgs(properties, symbols, offers, start_info);
  }

  // Launch a driver host if we are not colocated.
  if (!colocate) {
    if (use_dynamic_linker) {
      (*node_manager_)
          ->CreateDriverHostDynamicLinker([weak_self = weak_from_this(), name = name_,
                                           load_args = std::move(dynamic_linker_load_args),
                                           start_args = std::move(dynamic_linker_start_args),
                                           url = std::string(url),
                                           controller = std::move(controller), cb = std::move(cb)](
                                              zx::result<DriverHost*> driver_host) mutable {
            auto node_ptr = weak_self.lock();
            if (!node_ptr) {
              LOGF(WARNING, "Node '%s' freed before it is used", name.c_str());
              cb(zx::error(ZX_ERR_BAD_STATE));
              return;
            }

            if (driver_host.is_error()) {
              cb(driver_host.take_error());
              return;
            }
            node_ptr->driver_host_ = driver_host.value();
            node_ptr->StartDriverWithDynamicLinker(std::move(*load_args), std::move(*start_args),
                                                   url, std::move(controller), std::move(cb));
          });
      return;
    }
    auto result = (*node_manager_)->CreateDriverHost(use_next_vdso);
    if (result.is_error()) {
      cb(result.take_error());
      return;
    }
    driver_host_ = result.value();
  }

  if (use_dynamic_linker) {
    StartDriverWithDynamicLinker(std::move(*dynamic_linker_load_args),
                                 std::move(*dynamic_linker_start_args), url, std::move(controller),
                                 std::move(cb));
    return;
  }
  // Bind the Node associated with the driver.
  auto [client_end, server_end] = fidl::Endpoints<fdf::Node>::Create();

  LOGF(INFO, "Binding %.*s to %s", static_cast<int>(url.size()), url.data(), name().c_str());
  // Start the driver within the driver host.
  auto driver_endpoints = fidl::Endpoints<fuchsia_driver_host::Driver>::Create();

  zx::event node_token;
  if (start_info.has_component_instance()) {
    node_token = std::move(start_info.component_instance());
  } else {
    LOGF(WARNING, "Component instance not provided in start request");
    ZX_ASSERT(zx::event::create(0, &node_token) == ZX_OK);
  }

  zx::event node_token_dup;
  ZX_ASSERT(node_token.duplicate(ZX_RIGHT_SAME_RIGHTS, &node_token_dup) == ZX_OK);

  state_.emplace<DriverComponent>(*this, std::string(url), std::move(controller),
                                  std::move(server_end), std::move(driver_endpoints.client),
                                  std::move(node_token_dup));
  driver_host_.value()->Start(std::move(client_end), name_, properties, symbols, offers, start_info,
                              std::move(node_token), std::move(driver_endpoints.server),
                              [weak_self = weak_from_this(), name = name_,
                               cb = std::move(cb)](zx::result<> result) mutable {
                                auto node_ptr = weak_self.lock();
                                if (!node_ptr) {
                                  LOGF(WARNING, "Node '%s' freed before it is used", name.c_str());
                                  cb(result);
                                  return;
                                }

                                if (result.is_error()) {
                                  LOGF(WARNING, "Failed to start driver host for %s",
                                       node_ptr->MakeComponentMoniker().c_str());
                                }
                                cb(result);

                                // If the node set in the process of shutting down, shut down now.
                              });
}

void Node::StartDriverWithDynamicLinker(
    DriverHost::DriverLoadArgs load_args, DriverHost::DriverStartArgs start_args,
    std::string_view url, fidl::ServerEnd<fuchsia_component_runner::ComponentController> controller,
    fit::callback<void(zx::result<>)> cb) {
  auto [client_end, server_end] = fidl::Endpoints<fdf::Node>::Create();

  auto driver_endpoints = fidl::Endpoints<fuchsia_driver_host::Driver>::Create();

  zx::event node_token, node_token_dup;
  if (start_args.start_info_.component_instance().has_value()) {
    node_token = std::move(start_args.start_info_.component_instance().value());
  } else {
    ZX_ASSERT(zx::event::create(0, &node_token) == ZX_OK);
  }
  ZX_ASSERT(node_token.duplicate(ZX_RIGHT_SAME_RIGHTS, &node_token_dup) == ZX_OK);

  state_.emplace<DriverComponent>(*this, std::string(url), std::move(controller),
                                  std::move(server_end), std::move(driver_endpoints.client),
                                  std::move(node_token_dup));
  driver_host_.value()->StartWithDynamicLinker(std::move(client_end), name_, std::move(load_args),
                                               std::move(start_args), std::move(node_token),
                                               std::move(driver_endpoints.server), std::move(cb));
}

bool Node::EvaluateRematchFlags(fuchsia_driver_development::RestartRematchFlags rematch_flags,
                                std::string_view requested_url) const {
  if (IsComposite() &&
      !(rematch_flags & fuchsia_driver_development::RestartRematchFlags::kCompositeSpec)) {
    return false;
  }

  if (driver_url() == requested_url &&
      !(rematch_flags & fuchsia_driver_development::RestartRematchFlags::kRequested)) {
    return false;
  }

  if (driver_url() != requested_url &&
      !(rematch_flags & fuchsia_driver_development::RestartRematchFlags::kNonRequested)) {
    return false;
  }

  return true;
}

NodeInfo Node::GetRemovalTrackerInfo() {
  return NodeInfo{
      .name = MakeComponentMoniker(),
      .driver_url = driver_url(),
      .collection = collection_,
      .state = GetNodeState(),
  };
}

void Node::StopDriver() {
  ZX_ASSERT_MSG(GetNodeState() == NodeState::kWaitingOnChildren,
                "StopDriverComponent called in invalid node state: %s",
                GetNodeShutdownCoordinator().NodeStateAsString());
  if (!HasDriver()) {
    return;
  }
  auto& driver_component = std::get<DriverComponent>(state_);

  if (driver_component.state == DriverState::kBinding) {
    LOGF(WARNING, "Stopping driver '%s' for node '%s' while bind is in process",
         driver_component.driver_url.c_str(), MakeComponentMoniker().c_str());
    return;
  }

  fidl::OneWayStatus result = driver_component.driver->Stop();
  if (result.ok()) {
    return;  // We'll now wait for the channel to close
  }

  LOGF(ERROR, "Node: %s failed to stop driver: %s", name().c_str(),
       result.FormatDescription().data());
  // Continue to clear out the driver, since we can't talk to it.
  ClearHostDriver();
}

void Node::StopDriverComponent() {
  ZX_ASSERT_MSG(GetNodeState() == NodeState::kWaitingOnDriver,
                "StopDriverComponent called in invalid node state: %s",
                GetNodeShutdownCoordinator().NodeStateAsString());
  auto* driver_component = std::get_if<DriverComponent>(&state_);
  if (!driver_component) {
    return;
  }

  // Send an epitaph to the component manager and close the connection. The
  // server of a `ComponentController` protocol is expected to send an epitaph
  // before closing the associated connection.
  auto this_node = shared_from_this();
  driver_component->component_controller_ref.Close(ZX_OK);
  if (!node_manager_.has_value()) {
    return;
  }
  node_manager_.value()->DestroyDriverComponent(
      *this_node,
      [self = this_node](fidl::WireUnownedResult<fcomponent::Realm::DestroyChild>& result) {
        if (!result.ok()) {
          auto error = result.error().FormatDescription();
          LOGF(ERROR, "Node: %s: Failed to send request to destroy component: %.*s",
               self->name_.c_str(), static_cast<int>(error.size()), error.data());
        }
        if (result->is_error() &&
            result->error_value() != fcomponent::wire::Error::kInstanceNotFound) {
          LOGF(ERROR, "Node: %.*s: Failed to destroy driver component: %u",
               static_cast<int>(self->name_.size()), self->name_.data(), result->error_value());
        }

        LOGF(DEBUG, "Destroyed driver component for %s", self->MakeComponentMoniker().c_str());
        if (auto* driver_component = std::get_if<DriverComponent>(&self->state_);
            driver_component) {
          driver_component->state = DriverState::kStopped;
        }
        self->GetNodeShutdownCoordinator().CheckNodeState();
      });
}

void Node::on_fidl_error(fidl::UnbindInfo info) {
  ClearHostDriver();

  // The only valid way a driver host should shut down the Driver channel
  // is with the ZX_OK epitaph.
  // TODO(b/322235974): Increase the log severity to ERROR once we resolve the component shutdown
  // order in DriverTestRealm.
  if (info.reason() != fidl::Reason::kPeerClosedWhileReading || info.status() != ZX_OK) {
    LOGF(WARNING, "Node: %s: driver channel shutdown with: %s", name().c_str(),
         info.FormatDescription().data());
  }

  if (GetNodeState() == NodeState::kWaitingOnDriver) {
    LOGF(DEBUG, "Node: %s: realm channel had expected shutdown.", MakeComponentMoniker().c_str());
    GetNodeShutdownCoordinator().CheckNodeState();
    return;
  }

  if (GetNodeState() == NodeState::kWaitingOnDriverComponent) {
    LOGF(DEBUG, "Node: %s: driver channel had expected shutdown.", name().c_str());
    if (auto* driver_component = std::get_if<DriverComponent>(&state_); driver_component) {
      driver_component->state = DriverState::kStopped;
    }
    GetNodeShutdownCoordinator().CheckNodeState();
    return;
  }

  if (host_restart_on_crash_) {
    LOGF(WARNING, "Restarting node %s because of unexpected driver channel shutdown.",
         name().c_str());
    RestartNode();
    return;
  }

  // If the driver fails to bind to the node, don't remove the node.
  if (IsPendingBind()) {
    LOGF(DEBUG, "Node: %s: driver channel closed during binding.", MakeComponentMoniker().c_str());
    return;
  }

  LOGF(WARNING, "Removing node %s because of unexpected driver channel shutdown.", name().c_str());
  Remove(RemovalSet::kAll, nullptr);
}

std::optional<std::vector<fdf::NodeProperty2>> Node::GetNodeProperties(
    std::string_view parent_name) const {
  for (const auto& entry : properties_) {
    if (entry.name == parent_name) {
      return PropertyToFidl(entry.properties);
    }
  }
  return std::nullopt;
}

fdf::NodePropertyDictionary2 Node::GetNodePropertyDict() const {
  fdf::NodePropertyDictionary2 dict;
  dict.reserve(properties_.size());
  std::ranges::transform(properties_, std::back_inserter(dict), [this](const auto& entry) {
    return fdf::NodePropertyEntry2{{
        .name = entry.name,
        .properties = std::move(GetNodeProperties(entry.name).value()),
    }};
  });
  return dict;
}

Node::DriverComponent::DriverComponent(
    Node& node, std::string url,
    fidl::ServerEnd<fuchsia_component_runner::ComponentController> controller,
    fidl::ServerEnd<fuchsia_driver_framework::Node> node_server,
    fidl::ClientEnd<fuchsia_driver_host::Driver> driver, zx::event component_inst)
    : component_controller_ref(
          node.dispatcher_, std::move(controller), &node,
          [](Node* node, fidl::UnbindInfo info) {
            if (!info.is_user_initiated()) {
              LOGF(WARNING, "Removing node %s because of ComponentController binding closed: %s",
                   node->name().c_str(), info.FormatDescription().c_str());
              node->Remove(RemovalSet::kAll, nullptr);
            }
          }),
      node_ref_(node.dispatcher_, std::move(node_server), &node,
                [](Node* node, fidl::UnbindInfo info) { node->OnNodeServerUnbound(info); }),
      driver(std::move(driver), node.dispatcher_, &node),
      driver_url(std::move(url)),
      component_instance(std::move(component_inst)) {
  zx_info_handle_basic_t info;
  ZX_ASSERT(component_instance.get_info(ZX_INFO_HANDLE_BASIC, &info, sizeof(info), nullptr,
                                        nullptr) == ZX_OK);
  component_instance_koid = info.koid;
}

void Node::ConnectToDeviceFidl(ConnectToDeviceFidlRequestView request,
                               ConnectToDeviceFidlCompleter::Sync& completer) {
  zx_status_t status = ConnectDeviceInterface(std::move(request->server));
  if (status != ZX_OK) {
    LOGF(ERROR, "%s: Failed to connect to device fidl: ", zx_status_get_string(status));
  }
}

void Node::ConnectToController(ConnectToControllerRequestView request,
                               ConnectToControllerCompleter::Sync& completer) {
  ConnectControllerInterface(
      fidl::ServerEnd<fuchsia_device::Controller>{std::move(request->server)});
}

void Node::Bind(BindRequestView request, BindCompleter::Sync& completer) {
  BindHelper(false, {{request->driver.data(), request->driver.size()}},
             [completer = completer.ToAsync()](zx_status_t status) mutable {
               if (status == ZX_OK) {
                 completer.ReplySuccess();
               } else {
                 completer.ReplyError(status);
               }
             });
}

void Node::Rebind(RebindRequestView request, RebindCompleter::Sync& completer) {
  std::optional<std::string> url;
  if (!request->driver.is_null() && !request->driver.empty()) {
    url = std::string(request->driver.get());
  }

  auto rebind_callback = [completer = completer.ToAsync()](zx::result<> result) mutable {
    if (result.is_ok()) {
      completer.ReplySuccess();
    } else {
      completer.ReplyError(result.error_value());
    }
  };

  if (kEnableCompositeNodeSpecRebind && IsComposite()) {
    node_manager_.value()->RebindComposite(name_, url, std::move(rebind_callback));
    return;
  }

  RestartNodeWithRematch(url, std::move(rebind_callback));
}

void Node::UnbindChildren(UnbindChildrenCompleter::Sync& completer) {
  if (children_.empty()) {
    completer.ReplySuccess();
    return;
  }

  unbinding_children_completers_.emplace_back(completer.ToAsync());
  if (unbinding_children_completers_.size() == 1) {
    // Iterate over a copy of `children_` because `children_` may be modified during `Node::Remove`
    // which would mess up the for loop.
    std::vector<std::shared_ptr<Node>> children{children_.begin(), children_.end()};
    for (const auto& child : children) {
      child->Remove(RemovalSet::kAll, nullptr);
    }
  }
}

void Node::ScheduleUnbind(ScheduleUnbindCompleter::Sync& completer) {
  Remove(RemovalSet::kAll, nullptr);
  completer.ReplySuccess();
}
void Node::GetTopologicalPath(GetTopologicalPathCompleter::Sync& completer) {
  completer.ReplySuccess(fidl::StringView::FromExternal("/" + MakeTopologicalPath()));
}

zx_status_t Node::ConnectControllerInterface(
    fidl::ServerEnd<fuchsia_device::Controller> server_end) {
  // This should never be called
  ZX_ASSERT_MSG(false,
                "Connect To controller should never be called in node.cc,"
                " as it is intercepted by the ControllerAllowlistPassthrough");
  return ZX_OK;
}

zx_status_t Node::ConnectDeviceInterface(zx::channel channel) {
  if (!devfs_connector_.has_value()) {
    return ZX_ERR_INTERNAL;
  }
  return fidl::WireCall(devfs_connector_.value())->Connect(std::move(channel)).status();
}

Devnode::Target Node::CreateDevfsPassthrough(
    std::optional<fidl::ClientEnd<fuchsia_device_fs::Connector>> connector,
    std::optional<fidl::ClientEnd<fuchsia_device_fs::Connector>> controller_connector,
    bool allow_controller_connection, const std::string& class_name) {
  controller_allowlist_passthrough_ = ControllerAllowlistPassthrough::Create(
      std::move(controller_connector), weak_from_this(), dispatcher_, class_name);
  devfs_connector_ = std::move(connector);
  return Devnode::PassThrough(
      [node = weak_from_this(), node_name = name_](zx::channel server_end) {
        std::shared_ptr locked_node = node.lock();
        if (!locked_node) {
          LOGF(ERROR, "Node was freed before it was used for %s.", node_name.c_str());
          return ZX_ERR_BAD_STATE;
        }
        return locked_node->ConnectDeviceInterface(std::move(server_end));
      },
      [node = weak_from_this(), allow_controller_connection,
       node_name = name_](fidl::ServerEnd<fuchsia_device::Controller> server_end) {
        if (!allow_controller_connection) {
          LOGF(WARNING,
               "Connection to %s controller interface failed, as that node did not"
               " include controller support in its DevAddArgs",
               node_name.c_str());
          return ZX_ERR_PROTOCOL_NOT_SUPPORTED;
        }
        std::shared_ptr locked_node = node.lock();
        if (!locked_node) {
          LOGF(ERROR, "Node was freed before it was used for %s.", node_name.c_str());
          return ZX_ERR_BAD_STATE;
        }
        return locked_node->controller_allowlist_passthrough_->Connect(std::move(server_end));
      });
}

}  // namespace driver_manager
