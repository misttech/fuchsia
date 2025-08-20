// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.driver.test/cpp/fidl.h>
#include <fidl/fuchsia.examples.metadata/cpp/fidl.h>
#include <lib/component/incoming/cpp/service_member_watcher.h>
#include <lib/device-watcher/cpp/device-watcher.h>
#include <lib/driver_test_realm/realm_builder/cpp/lib.h>
#include <lib/fdio/fd.h>

#include "src/lib/testing/loop_fixture/test_loop_fixture.h"
#include "src/lib/testing/predicates/status.h"

namespace metadata_test {

class MetadataTest : public gtest::TestLoopFixture {
 public:
  void SetUp() override {
    // Create and build the realm.
    auto realm_builder = component_testing::RealmBuilder::Create();
    driver_test_realm::Setup(realm_builder);
    const std::vector<fuchsia_component_test::Capability> exposes = {
        {
            fuchsia_component_test::Capability::WithService(fuchsia_component_test::Service(
                {.name = fuchsia_examples_metadata::RetrieverService::Name})),
        },
        {
            fuchsia_component_test::Capability::WithService(fuchsia_component_test::Service(
                {.name = fuchsia_examples_metadata::SenderService::Name})),
        }};
    driver_test_realm::AddDtrExposes(realm_builder, exposes);
    realm_.emplace(realm_builder.Build(dispatcher()));

    // Start DriverTestRealm.
    zx::result result = realm_->component().Connect<fuchsia_driver_test::Realm>();
    ASSERT_OK(result);
    fidl::SyncClient<fuchsia_driver_test::Realm> driver_test_realm(std::move(result.value()));
    fidl::Result start_result = driver_test_realm->Start(fuchsia_driver_test::RealmArgs(
        {.root_driver = "fuchsia-boot:///dtr#meta/sender.cm", .dtr_exposes = exposes}));
    ASSERT_OK(start_result);

    // Connect to sender driver.
    component::SyncServiceMemberWatcher<fuchsia_examples_metadata::SenderService::Device> watcher(
        SvcDir());
    zx::result sender = watcher.GetNextInstance(false);
    ASSERT_OK(sender);
    sender_.Bind(std::move(sender.value()));
  }

 protected:
  fidl::SyncClient<fuchsia_examples_metadata::Sender>& sender() { return sender_; }

  fidl::UnownedClientEnd<fuchsia_io::Directory> SvcDir() {
    return fidl::UnownedClientEnd<fuchsia_io::Directory>(
        realm_->component().exposed().unowned_channel()->get());
  }

 private:
  std::optional<component_testing::RealmRoot> realm_;
  fidl::SyncClient<fuchsia_examples_metadata::Sender> sender_;
};

TEST_F(MetadataTest, TransferMetadata) {
  const char* kMetadataPropertyValue = "test property value";

  // Serve the metadata of the sender driver and offer it to its child node (which the forwarder
  // driver binds to).
  {
    fuchsia_examples_metadata::Metadata metadata({.test_property = kMetadataPropertyValue});
    ASSERT_OK(sender()->ServeMetadata(std::move(metadata)));
  }

  // Connect to retriever driver.
  component::SyncServiceMemberWatcher<fuchsia_examples_metadata::RetrieverService::Device> watcher(
      SvcDir());
  zx::result retriever_client_end = watcher.GetNextInstance(false);
  ASSERT_OK(retriever_client_end);
  fidl::SyncClient retriever(std::move(retriever_client_end.value()));

  // Retrieve the metadata from the retriever driver's parent driver (the forwarder driver).
  // This verifies that:
  //   * The `sender` driver sent the correct metadata.
  //   * The `forwarder` driver forwarded the correct metadata.
  //   * The `retriever` driver retrieved the correct metadata.
  {
    fidl::Result result = retriever->GetMetadata();
    ASSERT_OK(result);
    auto metadata = std::move(result.value().metadata());
    ASSERT_EQ(metadata.test_property(), kMetadataPropertyValue);
  }
}

}  // namespace metadata_test
