// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_BIN_DRIVER_MANAGER_COMPOSITE_NODE_SPEC_COMPOSITE_NODE_SPEC_H_
#define SRC_DEVICES_BIN_DRIVER_MANAGER_COMPOSITE_NODE_SPEC_COMPOSITE_NODE_SPEC_H_

#include <fidl/fuchsia.driver.development/cpp/wire.h>
#include <fidl/fuchsia.driver.index/cpp/fidl.h>

namespace driver_manager {
class Node;

using NodeWkPtr = std::weak_ptr<driver_manager::Node>;
using RemoveCompositeNodeCallback = fit::callback<void(zx::result<>)>;

struct CompositeNodeSpecCreateInfo {
  std::string name;
  std::vector<fuchsia_driver_framework::ParentSpec2> parents;
};

// This partially abstract class represents a composite node spec and is responsible for managing
// its state and composite node. The CompositeNodeSpec class will manage the state of its bound
// nodes while its subclasses manage the composite node under the spec.
class CompositeNodeSpec {
 public:
  explicit CompositeNodeSpec(CompositeNodeSpecCreateInfo create_info);

  virtual ~CompositeNodeSpec() = default;

  // Called when CompositeNodeManager receives a MatchedNodeRepresentation.
  // Returns ZX_ERR_ALREADY_BOUND if it's already bound. See BindParentImpl() for return type
  // details.
  zx::result<std::optional<NodeWkPtr>> BindParent(
      fuchsia_driver_framework::wire::CompositeParent composite_parent, const NodeWkPtr& node_ptr);

  virtual fuchsia_driver_development::wire::CompositeNodeInfo GetCompositeInfo(
      fidl::AnyArena& arena) const = 0;

  // Remove the underlying composite node and unmatch all of its parents. Called for
  // rebind.
  void Remove(RemoveCompositeNodeCallback callback);

  const std::vector<fuchsia_driver_framework::ParentSpec2>& parent_specs() const {
    return parent_specs_;
  }

  // Exposed for testing.
  const std::vector<std::optional<NodeWkPtr>>& parent_nodes() const { return parent_nodes_; }

  const std::string& name() const { return name_; }

 protected:
  // Subclass implementation for binding the NodeWkPtr to its composite.
  // If the composite is complete, it should return a pointer to the new node. Otherwise, it returns
  // a std::nullopt. The lifetime of this node object is managed by the parent nodes.
  virtual zx::result<std::optional<NodeWkPtr>> BindParentImpl(
      fuchsia_driver_framework::wire::CompositeParent composite_parent,
      const NodeWkPtr& node_ptr) = 0;

  // Subclass implementation for Remove(). Subclasses are expected to remove the underlying
  // composite node and unmatch all of the parents from it.
  virtual void RemoveImpl(RemoveCompositeNodeCallback callback) = 0;

  size_t size() const { return parent_nodes_.size(); }

 private:
  std::string name_;
  std::vector<std::optional<NodeWkPtr>> parent_nodes_;
  std::vector<fuchsia_driver_framework::ParentSpec2> parent_specs_;
};

}  // namespace driver_manager

#endif  // SRC_DEVICES_BIN_DRIVER_MANAGER_COMPOSITE_NODE_SPEC_COMPOSITE_NODE_SPEC_H_
