// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_TESTS_LIBDRIVER_INTEGRATION_TEST_MOCK_DEVICE_H_
#define SRC_DEVICES_TESTS_LIBDRIVER_INTEGRATION_TEST_MOCK_DEVICE_H_

#include <fuchsia/device/mock/cpp/fidl.h>
#include <lib/fidl/cpp/binding.h>
#include <lib/fidl/cpp/message.h>
#include <lib/zx/channel.h>

#include <memory>
#include <utility>

#include "action-list.h"
#include "mock-device-hooks.h"

namespace libdriver_integration_test {

class MockDevice : public fuchsia::device::mock::MockDevice {
 public:
  using Interface = fuchsia::device::mock::MockDevice;
  using HookInvocation = fuchsia::device::mock::HookInvocation;

  MockDevice(fidl::InterfaceRequest<Interface> request, async_dispatcher_t* dispatcher,
             std::string device_path);
  ~MockDevice() override = default;

  void set_hooks(std::unique_ptr<MockDeviceHooks> hooks) {
    hooks->set_action_list_finalizer(fit::bind_member(this, &MockDevice::FinalizeActionList));
    hooks_ = std::move(hooks);
  }

  // Path relative to the devmgr's devfs that can be opened to get a
  // connection to this device.
  const std::string& path() const { return path_; }

  void Bind(HookInvocation record, BindCallback callback) override;
  void Release(HookInvocation record) override;
  void GetProtocol(HookInvocation record, uint32_t protocol_id,
                   GetProtocolCallback callback) override;
  void Unbind(HookInvocation record, UnbindCallback callback) override;
  void Suspend(HookInvocation record, uint8_t requested_state, bool enable_wake,
               uint8_t suspend_reason, SuspendCallback callback) override;
  void Resume(HookInvocation record, uint32_t requested_perf_state,
              ResumeCallback callback) override;
  void Message(HookInvocation record, MessageCallback callback) override;
  void Rxrpc(HookInvocation record, RxrpcCallback callback) override;

  void AddDeviceDone(uint64_t action_id) override;
  void UnbindReplyDone(uint64_t action_id) override;
  void SuspendReplyDone(uint64_t action_id) override;
  void ResumeReplyDone(uint64_t action_id) override;

 private:
  // The buffers inside of |msg_out| must be allocated by the caller.
  zx_status_t Dispatch(fidl::HLCPPOutgoingMessage* msg, fidl::HLCPPIncomingMessage* msg_out);

  // Walks the action list and patches up any action_ids before converting it
  // to a vector
  std::vector<ActionList::Action> FinalizeActionList(ActionList actions);

  fidl::Binding<Interface> binding_;
  std::unique_ptr<MockDeviceHooks> hooks_;
  std::string path_;

  // Completers for pending add/remove actions, so we can signal when the
  // operations are finished.
  std::map<uint64_t, fpromise::completer<void, std::string>> pending_actions_;
  uint64_t next_action_id_ = 0;
};

}  // namespace libdriver_integration_test

#endif  // SRC_DEVICES_TESTS_LIBDRIVER_INTEGRATION_TEST_MOCK_DEVICE_H_
