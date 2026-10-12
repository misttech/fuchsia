// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/bin/driver_manager/power/all_drivers_element.h"

#include <fidl/fuchsia.power.broker/cpp/fidl.h>
#include <zircon/assert.h>

#include <unordered_set>

#include <gtest/gtest.h>

#include "src/devices/bin/driver_manager/power/power_manager.h"
#include "src/devices/bin/driver_manager/tests/driver_manager_test_base.h"

namespace driver_manager {

class FakeTopology : public fidl::Server<fuchsia_power_broker::Topology> {
 public:
  void Lease(LeaseRequest& request, LeaseCompleter::Sync& completer) override {
    lease_requests_.push_back(std::move(request));
    completer.Reply(fit::ok());
  }

  void AddElement(AddElementRequest& request, AddElementCompleter::Sync& completer) override {
    completer.Reply(fit::ok());
  }

  void handle_unknown_method(fidl::UnknownMethodMetadata<fuchsia_power_broker::Topology> metadata,
                             fidl::UnknownMethodCompleter::Sync& completer) override {}

  std::vector<LeaseRequest> lease_requests_;
};

class FakeElementControl : public fidl::Server<fuchsia_power_broker::ElementControl> {
 public:
  void RegisterDependencyToken(RegisterDependencyTokenRequest& request,
                               RegisterDependencyTokenCompleter::Sync& completer) override {
    completer.Reply(fit::ok());
  }

  void UnregisterDependencyToken(UnregisterDependencyTokenRequest& request,
                                 UnregisterDependencyTokenCompleter::Sync& completer) override {
    completer.Reply(fit::ok());
  }

  void AddDependency(AddDependencyRequest& request,
                     AddDependencyCompleter::Sync& completer) override {
    // Like Power Broker, reject a dependency on an element that already has one. The required
    // element is identified by the koid of its power token.
    ZX_ASSERT(request.requires_token().has_value());
    if (!dependency_koids_.insert(GetKoid(*request.requires_token())).second) {
      already_exists_count_++;
      completer.Reply(fit::error(fuchsia_power_broker::ModifyDependencyError::kAlreadyExists));
      return;
    }
    added_dependencies_.push_back(std::move(request));
    if (auto_reply_add_dependency_) {
      completer.Reply(fit::ok());
    } else {
      add_dependency_completers_.push_back(completer.ToAsync());
    }
  }

  // Simulates Power Broker removing the dependency on the element that |token| belongs to. Power
  // Broker does this when the element is removed (e.g. when its node restarts), because the
  // dependency is added with |remove_with_required_element|.
  void RemoveDependencyOn(const zx::event& token) { dependency_koids_.erase(GetKoid(token)); }

  void ReplyAddDependencies() {
    for (auto& comp : add_dependency_completers_) {
      comp.Reply(fit::ok());
    }
    add_dependency_completers_.clear();
  }

  void OpenStatusChannel(OpenStatusChannelRequest& request,
                         OpenStatusChannelCompleter::Sync& completer) override {}

  void handle_unknown_method(
      fidl::UnknownMethodMetadata<fuchsia_power_broker::ElementControl> metadata,
      fidl::UnknownMethodCompleter::Sync& completer) override {}

  bool auto_reply_add_dependency_ = true;
  std::vector<AddDependencyCompleter::Async> add_dependency_completers_;
  // The dependencies that were accepted, in the order they were added.
  std::vector<fuchsia_power_broker::LevelDependency> added_dependencies_;
  // The number of AddDependency requests rejected because the dependency already existed.
  size_t already_exists_count_ = 0;

 private:
  static zx_koid_t GetKoid(const zx::event& event) {
    zx_info_handle_basic_t info;
    ZX_ASSERT(event.get_info(ZX_INFO_HANDLE_BASIC, &info, sizeof(info), nullptr, nullptr) == ZX_OK);
    return info.koid;
  }

  // The koids of the power tokens of the elements that currently have an accepted dependency.
  std::unordered_set<zx_koid_t> dependency_koids_;
};

class AllDriversElementTest : public DriverManagerTestBase {
 public:
  NodeManager* GetNodeManager() override { return &node_manager_; }

  static void CompleteStartupTransition(AllDriversElement& element) {
    element.CompleteStartupTransition();
  }

  static void SetCurrentLevel(AllDriversElement& element, uint32_t level) {
    element.current_level_ = level;
  }

  static void SetTransitionComplete(AllDriversElement& element, bool complete) {
    element.transition_complete_ = complete;
  }

 private:
  TestNodeManagerBase node_manager_;
};

}  // namespace driver_manager

namespace driver_manager {
namespace {

TEST_F(AllDriversElementTest, TestSetLevel) {
  AllDriversElement element(nullptr, root());
  auto [runner_client, runner_server] =
      fidl::Endpoints<fuchsia_power_broker::ElementRunner>::Create();
  auto [control_client, control_server] =
      fidl::Endpoints<fuchsia_power_broker::ElementControl>::Create();
  auto [lessor_client, lessor_server] = fidl::Endpoints<fuchsia_power_broker::Lessor>::Create();

  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  bindings.AddBinding(dispatcher(), std::move(control_server), &fake_control,
                      fidl::kIgnoreBindingClosure);

  PowerElementHandles handles{
      .element_control = fidl::Client<fuchsia_power_broker::ElementControl>(
          std::move(control_client), dispatcher()),
      .element_runner = std::move(runner_server),
      .lessor = fidl::Client<fuchsia_power_broker::Lessor>(std::move(lessor_client), dispatcher()),
  };

  element.AttachElement(dispatcher(), std::move(handles));
  SetTransitionComplete(element, true);
  SetCurrentLevel(element, 1);

  fidl::WireClient<fuchsia_power_broker::ElementRunner> client(std::move(runner_client),
                                                               dispatcher());

  // Create a bound leaf node and set its startup lease.
  auto child = CreateNode("child_node");
  child->set_bound_for_testing(true);
  zx::eventpair startup_lease, lease_peer;
  ASSERT_EQ(zx::eventpair::create(0, &startup_lease, &lease_peer), ZX_OK);
  child->set_startup_lease_for_testing(std::move(startup_lease));

  element.OnNodeBound(child);
  RunLoopUntilIdle();

  // SetLevel(1)
  {
    bool called = false;
    client->SetLevel(1).Then(
        [&called](fidl::WireUnownedResult<fuchsia_power_broker::ElementRunner::SetLevel>& result) {
          EXPECT_TRUE(result.ok());
          called = true;
        });

    RunLoopUntilIdle();
    EXPECT_TRUE(called);
  }

  // SetLevel(0)
  {
    bool called = false;
    client->SetLevel(0).Then(
        [&called](fidl::WireUnownedResult<fuchsia_power_broker::ElementRunner::SetLevel>& result) {
          EXPECT_TRUE(result.ok());
          called = true;
        });

    RunLoopUntilIdle();
    EXPECT_TRUE(called);
  }
}

// Test CompleteStartupTransition on a Linear Chain topology: root -> parent -> child.
// After transition, only the child (the leaf) should add a dependency, and both startup
// leases should be released/dropped.
TEST_F(AllDriversElementTest, TestCompleteStartupTransitionLinearChain) {
  auto parent = CreateNode("parent", root());
  auto child = CreateNode("child", parent);

  parent->set_bound_for_testing(true);
  child->set_bound_for_testing(true);

  zx::eventpair lease_a, lease_a_peer;
  ASSERT_EQ(zx::eventpair::create(0, &lease_a, &lease_a_peer), ZX_OK);
  parent->set_startup_lease_for_testing(std::move(lease_a));

  zx::eventpair lease_b, lease_b_peer;
  ASSERT_EQ(zx::eventpair::create(0, &lease_b, &lease_b_peer), ZX_OK);
  child->set_startup_lease_for_testing(std::move(lease_b));

  AllDriversElement element(nullptr, root());
  auto [control_client, control_server] =
      fidl::Endpoints<fuchsia_power_broker::ElementControl>::Create();
  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  bindings.AddBinding(dispatcher(), std::move(control_server), &fake_control,
                      fidl::kIgnoreBindingClosure);

  PowerElementHandles handles{
      .element_control = fidl::Client<fuchsia_power_broker::ElementControl>(
          std::move(control_client), dispatcher()),
  };
  element.AttachElement(dispatcher(), std::move(handles));
  SetCurrentLevel(element, 1);

  CompleteStartupTransition(element);
  RunLoopUntilIdle();

  // Only child should add a dependency.
  ASSERT_EQ(fake_control.added_dependencies_.size(), 1u);

  // Both startup leases should be taken/released from nodes.
  auto lease_a_taken = parent->TakeStartupLease();
  EXPECT_TRUE(!lease_a_taken.has_value() || !lease_a_taken->is_valid());

  auto lease_b_taken = child->TakeStartupLease();
  EXPECT_TRUE(!lease_b_taken.has_value() || !lease_b_taken->is_valid());

  zx_signals_t pending = 0;
  EXPECT_EQ(lease_a_peer.wait_one(ZX_EVENTPAIR_PEER_CLOSED, zx::time::infinite_past(), &pending),
            ZX_OK);
  EXPECT_EQ(lease_b_peer.wait_one(ZX_EVENTPAIR_PEER_CLOSED, zx::time::infinite_past(), &pending),
            ZX_OK);
}

// Test CompleteStartupTransition with a Branching Tree topology: root -> child_a, root -> child_b.
// After transition, both child_a and child_b should add dependencies and have leases dropped.
TEST_F(AllDriversElementTest, TestCompleteStartupTransitionBranchingTree) {
  auto child_a = CreateNode("child_a", root());
  auto child_b = CreateNode("child_b", root());

  child_a->set_bound_for_testing(true);
  child_b->set_bound_for_testing(true);

  zx::eventpair lease_a, lease_a_peer;
  ASSERT_EQ(zx::eventpair::create(0, &lease_a, &lease_a_peer), ZX_OK);
  child_a->set_startup_lease_for_testing(std::move(lease_a));

  zx::eventpair lease_b, lease_b_peer;
  ASSERT_EQ(zx::eventpair::create(0, &lease_b, &lease_b_peer), ZX_OK);
  child_b->set_startup_lease_for_testing(std::move(lease_b));

  AllDriversElement element(nullptr, root());
  auto [control_client, control_server] =
      fidl::Endpoints<fuchsia_power_broker::ElementControl>::Create();
  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  bindings.AddBinding(dispatcher(), std::move(control_server), &fake_control,
                      fidl::kIgnoreBindingClosure);

  PowerElementHandles handles{
      .element_control = fidl::Client<fuchsia_power_broker::ElementControl>(
          std::move(control_client), dispatcher()),
  };
  element.AttachElement(dispatcher(), std::move(handles));
  SetCurrentLevel(element, 1);

  CompleteStartupTransition(element);
  RunLoopUntilIdle();

  EXPECT_EQ(fake_control.added_dependencies_.size(), 2u);

  auto lease_a_taken = child_a->TakeStartupLease();
  EXPECT_TRUE(!lease_a_taken.has_value() || !lease_a_taken->is_valid());

  auto lease_b_taken = child_b->TakeStartupLease();
  EXPECT_TRUE(!lease_b_taken.has_value() || !lease_b_taken->is_valid());

  zx_signals_t pending = 0;
  EXPECT_EQ(lease_a_peer.wait_one(ZX_EVENTPAIR_PEER_CLOSED, zx::time::infinite_past(), &pending),
            ZX_OK);
  EXPECT_EQ(lease_b_peer.wait_one(ZX_EVENTPAIR_PEER_CLOSED, zx::time::infinite_past(), &pending),
            ZX_OK);
}

// Test CompleteStartupTransition with a Diamond DAG topology:
// root -> parent_a -> composite, root -> parent_b -> composite.
// After transition, only the composite node should add a dependency and leases dropped.
TEST_F(AllDriversElementTest, TestCompleteStartupTransitionDiamondDAG) {
  auto parent_a = CreateNode("parent_a", root());
  auto parent_b = CreateNode("parent_b", root());

  std::vector<std::weak_ptr<Node>> parents = {parent_a, parent_b};
  auto composite = CreateCompositeNode("composite", parents, {{}, {}});

  parent_a->set_bound_for_testing(true);
  parent_b->set_bound_for_testing(true);
  composite->set_bound_for_testing(true);

  zx::eventpair lease_a, lease_a_peer;
  ASSERT_EQ(zx::eventpair::create(0, &lease_a, &lease_a_peer), ZX_OK);
  parent_a->set_startup_lease_for_testing(std::move(lease_a));

  zx::eventpair lease_b, lease_b_peer;
  ASSERT_EQ(zx::eventpair::create(0, &lease_b, &lease_b_peer), ZX_OK);
  parent_b->set_startup_lease_for_testing(std::move(lease_b));

  zx::eventpair lease_c, lease_c_peer;
  ASSERT_EQ(zx::eventpair::create(0, &lease_c, &lease_c_peer), ZX_OK);
  composite->set_startup_lease_for_testing(std::move(lease_c));

  AllDriversElement element(nullptr, root());
  auto [control_client, control_server] =
      fidl::Endpoints<fuchsia_power_broker::ElementControl>::Create();
  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  bindings.AddBinding(dispatcher(), std::move(control_server), &fake_control,
                      fidl::kIgnoreBindingClosure);

  PowerElementHandles handles{
      .element_control = fidl::Client<fuchsia_power_broker::ElementControl>(
          std::move(control_client), dispatcher()),
  };
  element.AttachElement(dispatcher(), std::move(handles));
  SetCurrentLevel(element, 1);

  CompleteStartupTransition(element);
  RunLoopUntilIdle();

  EXPECT_EQ(fake_control.added_dependencies_.size(), 1u);

  auto lease_a_taken = parent_a->TakeStartupLease();
  EXPECT_TRUE(!lease_a_taken.has_value() || !lease_a_taken->is_valid());

  auto lease_b_taken = parent_b->TakeStartupLease();
  EXPECT_TRUE(!lease_b_taken.has_value() || !lease_b_taken->is_valid());

  auto lease_c_taken = composite->TakeStartupLease();
  EXPECT_TRUE(!lease_c_taken.has_value() || !lease_c_taken->is_valid());

  zx_signals_t pending = 0;
  EXPECT_EQ(lease_a_peer.wait_one(ZX_EVENTPAIR_PEER_CLOSED, zx::time::infinite_past(), &pending),
            ZX_OK);
  EXPECT_EQ(lease_b_peer.wait_one(ZX_EVENTPAIR_PEER_CLOSED, zx::time::infinite_past(), &pending),
            ZX_OK);
  EXPECT_EQ(lease_c_peer.wait_one(ZX_EVENTPAIR_PEER_CLOSED, zx::time::infinite_past(), &pending),
            ZX_OK);
}

TEST_F(AllDriversElementTest, TestOnNodeBoundLeaf) {
  AllDriversElement element(nullptr, root());
  auto [runner_client, runner_server] =
      fidl::Endpoints<fuchsia_power_broker::ElementRunner>::Create();
  auto [control_client, control_server] =
      fidl::Endpoints<fuchsia_power_broker::ElementControl>::Create();
  auto [lessor_client, lessor_server] = fidl::Endpoints<fuchsia_power_broker::Lessor>::Create();

  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  bindings.AddBinding(dispatcher(), std::move(control_server), &fake_control,
                      fidl::kIgnoreBindingClosure);

  PowerElementHandles handles{
      .element_control = fidl::Client<fuchsia_power_broker::ElementControl>(
          std::move(control_client), dispatcher()),
      .element_runner = std::move(runner_server),
      .lessor = fidl::Client<fuchsia_power_broker::Lessor>(std::move(lessor_client), dispatcher()),
  };
  element.AttachElement(dispatcher(), std::move(handles));
  SetTransitionComplete(element, true);

  // Create a bound leaf node
  auto child = CreateNode("child_node");
  child->set_bound_for_testing(true);

  element.OnNodeBound(child);

  RunLoopUntilIdle();

  ASSERT_EQ(fake_control.added_dependencies_.size(), 1u);
  EXPECT_EQ(fake_control.added_dependencies_[0].dependent_level(), 1u);
  EXPECT_TRUE(fake_control.added_dependencies_[0].remove_with_required_element().value_or(false));

  zx::event expected_token = child->DuplicatePowerToken();
  ASSERT_TRUE(expected_token.is_valid());

  // Verify requires_token is matching by comparing koids
  zx_info_handle_basic_t actual_info, expected_info;
  ASSERT_TRUE(fake_control.added_dependencies_[0].requires_token().has_value());
  ASSERT_EQ(fake_control.added_dependencies_[0].requires_token()->get_info(
                ZX_INFO_HANDLE_BASIC, &actual_info, sizeof(actual_info), nullptr, nullptr),
            ZX_OK);
  ASSERT_EQ(expected_token.get_info(ZX_INFO_HANDLE_BASIC, &expected_info, sizeof(expected_info),
                                    nullptr, nullptr),
            ZX_OK);
  EXPECT_EQ(actual_info.koid, expected_info.koid);
}

TEST_F(AllDriversElementTest, TestOnNodeBoundNonLeaf) {
  AllDriversElement element(nullptr, root());
  auto [runner_client, runner_server] =
      fidl::Endpoints<fuchsia_power_broker::ElementRunner>::Create();
  auto [control_client, control_server] =
      fidl::Endpoints<fuchsia_power_broker::ElementControl>::Create();
  auto [lessor_client, lessor_server] = fidl::Endpoints<fuchsia_power_broker::Lessor>::Create();

  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  bindings.AddBinding(dispatcher(), std::move(control_server), &fake_control,
                      fidl::kIgnoreBindingClosure);

  PowerElementHandles handles{
      .element_control = fidl::Client<fuchsia_power_broker::ElementControl>(
          std::move(control_client), dispatcher()),
      .element_runner = std::move(runner_server),
      .lessor = fidl::Client<fuchsia_power_broker::Lessor>(std::move(lessor_client), dispatcher()),
  };
  element.AttachElement(dispatcher(), std::move(handles));
  SetTransitionComplete(element, true);

  // Create a parent node (non-leaf because it will have a bound child)
  auto parent = CreateNode("parent_node");
  parent->set_bound_for_testing(true);

  auto child = CreateNode("child_node", parent);
  child->set_bound_for_testing(true);

  element.OnNodeBound(parent);

  RunLoopUntilIdle();

  // Since parent has a bound descendant (child), it is not a leaf and shouldn't add a dependency.
  EXPECT_TRUE(fake_control.added_dependencies_.empty());
}

TEST_F(AllDriversElementTest, TestCompleteStartupTransitionAddsDependencies) {
  auto parent = CreateNode("parent", root());
  auto child = CreateNode("child", parent);

  parent->set_bound_for_testing(true);
  child->set_bound_for_testing(true);

  AllDriversElement element(nullptr, root());
  auto [runner_client, runner_server] =
      fidl::Endpoints<fuchsia_power_broker::ElementRunner>::Create();
  auto [control_client, control_server] =
      fidl::Endpoints<fuchsia_power_broker::ElementControl>::Create();
  auto [lessor_client, lessor_server] = fidl::Endpoints<fuchsia_power_broker::Lessor>::Create();

  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  bindings.AddBinding(dispatcher(), std::move(control_server), &fake_control,
                      fidl::kIgnoreBindingClosure);

  PowerElementHandles handles{
      .element_control = fidl::Client<fuchsia_power_broker::ElementControl>(
          std::move(control_client), dispatcher()),
      .element_runner = std::move(runner_server),
      .lessor = fidl::Client<fuchsia_power_broker::Lessor>(std::move(lessor_client), dispatcher()),
  };
  element.AttachElement(dispatcher(), std::move(handles));
  SetCurrentLevel(element, 1);

  CompleteStartupTransition(element);
  RunLoopUntilIdle();

  // Only leaf node (child) should have added a dependency
  ASSERT_EQ(fake_control.added_dependencies_.size(), 1u);
  EXPECT_EQ(fake_control.added_dependencies_[0].dependent_level(), 1u);

  // A node that was already bound when CompleteStartupTransition() ran can still have OnNodeBound()
  // called for it afterwards. The second request is rejected with ALREADY_EXISTS, which is ignored.
  element.OnNodeBound(child);
  RunLoopUntilIdle();
  EXPECT_EQ(fake_control.added_dependencies_.size(), 1u);
  EXPECT_EQ(fake_control.already_exists_count_, 1u);
}

TEST_F(AllDriversElementTest, TestOnNodeBoundAfterRestartAddsDependencyAgain) {
  AllDriversElement element(nullptr, root());
  auto [runner_client, runner_server] =
      fidl::Endpoints<fuchsia_power_broker::ElementRunner>::Create();
  auto [control_client, control_server] =
      fidl::Endpoints<fuchsia_power_broker::ElementControl>::Create();
  auto [lessor_client, lessor_server] = fidl::Endpoints<fuchsia_power_broker::Lessor>::Create();

  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  bindings.AddBinding(dispatcher(), std::move(control_server), &fake_control,
                      fidl::kIgnoreBindingClosure);

  PowerElementHandles handles{
      .element_control = fidl::Client<fuchsia_power_broker::ElementControl>(
          std::move(control_client), dispatcher()),
      .element_runner = std::move(runner_server),
      .lessor = fidl::Client<fuchsia_power_broker::Lessor>(std::move(lessor_client), dispatcher()),
  };
  element.AttachElement(dispatcher(), std::move(handles));
  SetTransitionComplete(element, true);

  auto child = CreateNode("child_node");
  child->set_bound_for_testing(true);
  zx::eventpair first_lease, first_lease_peer;
  ASSERT_EQ(zx::eventpair::create(0, &first_lease, &first_lease_peer), ZX_OK);
  child->set_startup_lease_for_testing(std::move(first_lease));

  element.OnNodeBound(child);
  RunLoopUntilIdle();
  ASSERT_EQ(fake_control.added_dependencies_.size(), 1u);

  // Simulate the node restarting. Its old power element is removed, and Power Broker removes the
  // dependency on it along with the element. The node keeps its topological path and power token,
  // but gets a new power element and a new startup lease.
  fake_control.RemoveDependencyOn(child->DuplicatePowerToken());
  zx::eventpair second_lease, second_lease_peer;
  ASSERT_EQ(zx::eventpair::create(0, &second_lease, &second_lease_peer), ZX_OK);
  child->set_startup_lease_for_testing(std::move(second_lease));

  element.OnNodeBound(child);
  RunLoopUntilIdle();

  // The restarted node gets its own dependency, which Power Broker accepts.
  EXPECT_EQ(fake_control.added_dependencies_.size(), 2u);
  EXPECT_EQ(fake_control.already_exists_count_, 0u);

  // The new startup lease is dropped once the dependency has been added.
  zx_signals_t pending = 0;
  EXPECT_EQ(
      second_lease_peer.wait_one(ZX_EVENTPAIR_PEER_CLOSED, zx::time::infinite_past(), &pending),
      ZX_OK);
}

TEST_F(AllDriversElementTest, TestLeaseAllDrivers) {
  auto [topology_client, topology_server] =
      fidl::Endpoints<fuchsia_power_broker::Topology>::Create();

  FakeTopology fake_topology;
  fidl::ServerBindingGroup<fuchsia_power_broker::Topology> bindings;
  bindings.AddBinding(dispatcher(), std::move(topology_server), &fake_topology,
                      fidl::kIgnoreBindingClosure);

  PowerManager power_manager(dispatcher(), std::move(topology_client), std::nullopt, false);

  // Setup a topology: parent_node (unbound) -> child_a (bound), child_b (unbound) -> grandchild
  // (bound)
  auto parent = CreateNode("parent_node");
  auto child_a = CreateNode("child_a", parent);
  child_a->set_bound_for_testing(true);

  auto child_b = CreateNode("child_b", parent);
  child_b->set_bound_for_testing(false);

  auto grandchild = CreateNode("grandchild", child_b);
  grandchild->set_bound_for_testing(true);

  // Call LeaseAllDrivers
  power_manager.LeaseAllDrivers(parent, [] {});

  RunLoopUntilIdle();

  // It should have acquired leases for child_a and grandchild (both are bound nodes in the
  // topology)
  ASSERT_EQ(fake_topology.lease_requests_.size(), 2u);

  // Collect lease names to verify
  std::unordered_set<std::string> leased_names;
  for (const auto& req : fake_topology.lease_requests_) {
    ASSERT_TRUE(req.lease_name().has_value());
    leased_names.insert(*req.lease_name());
  }

  EXPECT_TRUE(leased_names.contains("child_a"));
  EXPECT_TRUE(leased_names.contains("grandchild"));
  EXPECT_FALSE(leased_names.contains("parent_node"));
  EXPECT_FALSE(leased_names.contains("child_b"));
}

TEST_F(AllDriversElementTest, TestStartupLeaseHeldUntilAddDependencyCompletes) {
  AllDriversElement element(nullptr, root());
  auto [runner_client, runner_server] =
      fidl::Endpoints<fuchsia_power_broker::ElementRunner>::Create();
  auto [control_client, control_server] =
      fidl::Endpoints<fuchsia_power_broker::ElementControl>::Create();
  auto [lessor_client, lessor_server] = fidl::Endpoints<fuchsia_power_broker::Lessor>::Create();

  FakeElementControl fake_control;
  fake_control.auto_reply_add_dependency_ = false;

  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  bindings.AddBinding(dispatcher(), std::move(control_server), &fake_control,
                      fidl::kIgnoreBindingClosure);

  PowerElementHandles handles{
      .element_control = fidl::Client<fuchsia_power_broker::ElementControl>(
          std::move(control_client), dispatcher()),
      .element_runner = std::move(runner_server),
      .lessor = fidl::Client<fuchsia_power_broker::Lessor>(std::move(lessor_client), dispatcher()),
  };
  element.AttachElement(dispatcher(), std::move(handles));
  SetTransitionComplete(element, true);

  auto child = CreateNode("child_node");
  child->set_bound_for_testing(true);
  zx::eventpair startup_lease, lease_peer;
  ASSERT_EQ(zx::eventpair::create(0, &startup_lease, &lease_peer), ZX_OK);
  child->set_startup_lease_for_testing(std::move(startup_lease));

  element.OnNodeBound(child);
  RunLoopUntilIdle();

  // AddDependency request has been received, but not yet replied to.
  ASSERT_EQ(fake_control.added_dependencies_.size(), 1u);

  // The startup lease should still be held by the in-flight AddDependency request.
  zx_signals_t pending = 0;
  EXPECT_EQ(lease_peer.wait_one(ZX_EVENTPAIR_PEER_CLOSED, zx::time::infinite_past(), &pending),
            ZX_ERR_TIMED_OUT);

  // Complete the AddDependency call.
  fake_control.ReplyAddDependencies();
  RunLoopUntilIdle();

  // The startup lease should now be dropped.
  EXPECT_EQ(lease_peer.wait_one(ZX_EVENTPAIR_PEER_CLOSED, zx::time::infinite_past(), &pending),
            ZX_OK);
}

}  // namespace
}  // namespace driver_manager
