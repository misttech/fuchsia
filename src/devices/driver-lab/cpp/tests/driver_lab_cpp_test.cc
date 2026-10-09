// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.driver.lab/cpp/wire.h>
#include <lib/driver_lab/driver_lab.h>
#include <lib/driver_lab/driver_lab_c.h>
#include <lib/zx/vmo.h>
#include <zircon/time.h>

#include <atomic>
#include <string>
#include <vector>

#include <gtest/gtest.h>

namespace driver_lab {
namespace {

namespace flab = fuchsia_driver_lab;

TEST(DriverLabCppTest, DescribeReadWriteStateVmoBankAndAuditOverFidl) {
  // 1. Create a simulated MMIO VMO with live register values:
  //    0x00 = DEVICE_ID (0xD1A60001)
  //    0x04 = STATUS    (0x00000007)
  //    0x08 = CTRL      (0x00000000, writable)
  //    0x40 = FIFO_DATA (0xDEADBEEF, hard-denied clear-on-read register)
  zx::vmo mmio_vmo;
  ASSERT_EQ(zx::vmo::create(4096, 0, &mmio_vmo), ZX_OK);
  const uint32_t kDeviceId = 0xD1A60001u;
  const uint32_t kStatusVal = 0x00000007u;
  const uint32_t kFifoVal = 0xDEADBEEFu;
  ASSERT_EQ(mmio_vmo.write(&kDeviceId, 0x00, sizeof(kDeviceId)), ZX_OK);
  ASSERT_EQ(mmio_vmo.write(&kStatusVal, 0x04, sizeof(kStatusVal)), ZX_OK);
  ASSERT_EQ(mmio_vmo.write(&kFifoVal, 0x40, sizeof(kFifoVal)), ZX_OK);

  // 2. Create a StateVmoBank registered as the global C state/knob bank:
  //    0x00 = C++ telemetry state word (0x11223344)
  //    0x04 = Host-writable fault-injection knob (initial 100)
  //    0x08 = Legacy C telemetry state word (0x55667788)
  auto bank_res = StateVmoBank::Create(4096, /*register_global=*/true);
  ASSERT_TRUE(bank_res.is_ok()) << bank_res.status_string();
  StateVmoBank bank = std::move(*bank_res);

  bank.SetState32(0x00, 0x11223344u);
  bank.SetKnob32(0x04, 100u);
  driver_lab_global_set_state_u32(0x08, 0x55667788u);

  EXPECT_EQ(bank.GetState32(0x00), 0x11223344u);
  EXPECT_EQ(bank.GetState32(0x08), 0x55667788u);
  EXPECT_EQ(driver_lab_global_get_knob_u32(0x04, 0u), 100u);

  // 3. Configure Builder with MMIO VMO, StateVmoBank, interrupt tap, and quiesce hook.
  std::atomic<bool> quiesce_callback_state{false};
  std::atomic<int> quiesce_transitions{0};

  Builder builder("cpp-test-node");
  builder.SetEnabled(true);

  auto regs_id_res = builder.AddMmioVmo("regs", mmio_vmo.borrow(), 0, 0x100);
  ASSERT_TRUE(regs_id_res.is_ok()) << regs_id_res.status_string();
  const uint32_t regs_id = *regs_id_res;
  EXPECT_EQ(regs_id, 0u);
  builder.SetWritableRegisters(regs_id, {0x08});
  builder.SetHardDeniedRanges(regs_id, {{0x40, 0x44}});

  auto state_id_res = builder.AddStateVmoBank("state", bank, {0x04});
  ASSERT_TRUE(state_id_res.is_ok()) << state_id_res.status_string();
  const uint32_t state_id = *state_id_res;
  EXPECT_EQ(state_id, 1u);

  const uint32_t irq_id = builder.AddInterrupt("irq0");
  EXPECT_EQ(irq_id, 2u);

  builder.SetQuiesceHook([&](bool paused) {
    quiesce_callback_state.store(paused);
    quiesce_transitions.fetch_add(1);
  });

  auto server_res = builder.Build();
  ASSERT_TRUE(server_res.is_ok()) << server_res.status_string();
  EmbeddedServer server = std::move(*server_res);
  EXPECT_TRUE(server.is_enabled());
  EXPECT_FALSE(server.is_quiesced());

  // 4. Connect a synchronous FIDL Proxy client.
  auto proxy_endpoints = fidl::Endpoints<flab::Proxy>::Create();
  ASSERT_TRUE(server.ServeProxy(std::move(proxy_endpoints.server)).is_ok());
  fidl::WireSyncClient<flab::Proxy> proxy_client(std::move(proxy_endpoints.client));

  // 5. Verify Describe() returns all resources and valid SHA-256 digests.
  auto desc_res = proxy_client->Describe();
  ASSERT_TRUE(desc_res.ok()) << desc_res.FormatDescription();
  const auto& desc = *desc_res;

  ASSERT_TRUE(desc.has_protocol_major());
  EXPECT_EQ(desc.protocol_major(), 1u);
  ASSERT_TRUE(desc.has_boot_id());
  EXPECT_FALSE(desc.boot_id().empty());
  ASSERT_TRUE(desc.has_resource_digest());
  EXPECT_TRUE(std::string_view(desc.resource_digest().get()).starts_with("sha256:"));
  ASSERT_TRUE(desc.has_policy_digest());
  EXPECT_TRUE(std::string_view(desc.policy_digest().get()).starts_with("sha256:"));
  ASSERT_TRUE(desc.has_resources());
  ASSERT_EQ(desc.resources().size(), 3u);
  EXPECT_EQ(desc.resources()[0].name().get(), "regs");
  EXPECT_EQ(desc.resources()[1].name().get(), "state");
  EXPECT_EQ(desc.resources()[2].name().get(), "irq0");

  // 6. Open a Mutating session authorizing reads on regs/state and writes on knob 0x04.
  fidl::Arena arena;
  auto context = flab::wire::RunContext::Builder(arena)
                     .run_id("run-cs35")
                     .case_id("case-cs35")
                     .plan_digest("sha256:test")
                     .host_tool_version("0.1.0")
                     .Build();
  auto expectations = flab::wire::Expectations::Builder(arena)
                          .boot_id(desc.boot_id())
                          .proxy_generation(desc.proxy_generation())
                          .resource_digest(desc.resource_digest())
                          .policy_digest(desc.policy_digest())
                          .Build();

  std::vector<flab::wire::AccessRule> rules = {
      {
          .resource = regs_id,
          .offset = 0x00,
          .width = 4,
          .class_ = flab::wire::AccessClass::kReadOnce,
      },
      {
          .resource = regs_id,
          .offset = 0x08,
          .width = 4,
          .class_ = flab::wire::AccessClass::kWrite,
      },
      {
          .resource = state_id,
          .offset = 0x00,
          .width = 4,
          .class_ = flab::wire::AccessClass::kReadOnce,
      },
      {
          .resource = state_id,
          .offset = 0x04,
          .width = 4,
          .class_ = flab::wire::AccessClass::kWrite,
      },
      {
          .resource = state_id,
          .offset = 0x08,
          .width = 4,
          .class_ = flab::wire::AccessClass::kReadOnce,
      },
      {
          .resource = irq_id,
          .offset = 0,
          .width = 0,
          .class_ = flab::wire::AccessClass::kInterrupt,
      },
  };

  auto session_endpoints = fidl::Endpoints<flab::Session>::Create();
  auto open_res =
      proxy_client->OpenSession(context, flab::wire::SessionMode::kMutating, expectations,
                                fidl::VectorView<flab::wire::AccessRule>::FromExternal(rules),
                                std::move(session_endpoints.server));
  ASSERT_TRUE(open_res.ok()) << open_res.FormatDescription();
  ASSERT_TRUE(open_res->is_ok());

  // Verify the mutating session engaged the quiesce hook.
  EXPECT_TRUE(server.is_quiesced());
  EXPECT_TRUE(quiesce_callback_state.load());
  EXPECT_EQ(quiesce_transitions.load(), 1);

  fidl::WireSyncClient<flab::Session> session_client(std::move(session_endpoints.client));

  // 7. Read32 on MMIO VMO and StateVmoBank.
  {
    auto read_res = session_client->Read32(regs_id, 0x00);
    ASSERT_TRUE(read_res.ok()) << read_res.FormatDescription();
    ASSERT_TRUE(read_res->is_ok());
    EXPECT_EQ(read_res->value()->value, 0xD1A60001u);
  }
  {
    auto read_res = session_client->Read32(state_id, 0x00);
    ASSERT_TRUE(read_res.ok()) << read_res.FormatDescription();
    ASSERT_TRUE(read_res->is_ok());
    EXPECT_EQ(read_res->value()->value, 0x11223344u);
  }
  {
    auto read_res = session_client->Read32(state_id, 0x08);
    ASSERT_TRUE(read_res.ok()) << read_res.FormatDescription();
    ASSERT_TRUE(read_res->is_ok());
    EXPECT_EQ(read_res->value()->value, 0x55667788u);
  }

  // 8. Write32 on the writable knob at 0x04 in StateVmoBank and verify C++/C visibility.
  {
    auto write_res =
        session_client->Write32(state_id, 0x04, 0xCAFEBABEu, 0xFFFFFFFFu, nullptr, false);
    ASSERT_TRUE(write_res.ok()) << write_res.FormatDescription();
    ASSERT_TRUE(write_res->is_ok());
    EXPECT_EQ(bank.GetKnob32(0x04), 0xCAFEBABEu);
    EXPECT_EQ(driver_lab_global_get_knob_u32(0x04, 0u), 0xCAFEBABEu);
  }

  // 9. Tap interrupt from C++ and verify WaitForInterrupt receives it.
  server.NotifyInterrupt(irq_id);
  {
    auto irq_res = session_client->WaitForInterrupt(irq_id, 0, ZX_SEC(1));
    ASSERT_TRUE(irq_res.ok()) << irq_res.FormatDescription();
    ASSERT_TRUE(irq_res->is_ok());
    EXPECT_EQ(irq_res->value()->sequence, 1u);
  }

  // 10. ReadAudit() and confirm allowed operations are recorded.
  {
    auto audit_res = session_client->ReadAudit(0, 64);
    ASSERT_TRUE(audit_res.ok()) << audit_res.FormatDescription();
    const auto& entries = audit_res->entries;
    EXPECT_GE(entries.size(), 6u);

    bool saw_write = false;
    for (const auto& entry : entries) {
      if (entry.has_operation() && entry.operation().get() == "write32") {
        saw_write = true;
        ASSERT_TRUE(entry.has_decision());
        EXPECT_EQ(entry.decision(), flab::wire::AuditDecision::kAllowed);
      }
    }
    EXPECT_TRUE(saw_write);
  }
}

TEST(DriverLabCppTest, RejectsHardDeniedRangeInSessionAllowlist) {
  zx::vmo mmio_vmo;
  ASSERT_EQ(zx::vmo::create(4096, 0, &mmio_vmo), ZX_OK);

  Builder builder("cpp-deny-test");
  builder.SetEnabled(true);
  auto regs_id_res = builder.AddMmioVmo("regs", mmio_vmo.borrow(), 0, 0x100);
  ASSERT_TRUE(regs_id_res.is_ok());
  builder.SetHardDeniedRanges(*regs_id_res, {{0x40, 0x44}});

  auto server_res = builder.Build();
  ASSERT_TRUE(server_res.is_ok());
  EmbeddedServer server = std::move(*server_res);

  auto proxy_endpoints = fidl::Endpoints<flab::Proxy>::Create();
  ASSERT_TRUE(server.ServeProxy(std::move(proxy_endpoints.server)).is_ok());
  fidl::WireSyncClient<flab::Proxy> proxy_client(std::move(proxy_endpoints.client));

  auto desc_res = proxy_client->Describe();
  ASSERT_TRUE(desc_res.ok());
  const auto& desc = *desc_res;

  fidl::Arena arena;
  auto context = flab::wire::RunContext::Builder(arena)
                     .run_id("run-deny")
                     .case_id("case-deny")
                     .plan_digest("sha256:deny")
                     .host_tool_version("0.1.0")
                     .Build();
  auto expectations = flab::wire::Expectations::Builder(arena)
                          .boot_id(desc.boot_id())
                          .proxy_generation(desc.proxy_generation())
                          .resource_digest(desc.resource_digest())
                          .policy_digest(desc.policy_digest())
                          .Build();

  // Requesting ReadOnce on hard-denied offset 0x40 must fail with kRejectedAllowlist.
  std::vector<flab::wire::AccessRule> rules = {
      {
          .resource = *regs_id_res,
          .offset = 0x40,
          .width = 4,
          .class_ = flab::wire::AccessClass::kReadOnce,
      },
  };
  auto session_endpoints = fidl::Endpoints<flab::Session>::Create();
  auto open_res =
      proxy_client->OpenSession(context, flab::wire::SessionMode::kReadOnly, expectations,
                                fidl::VectorView<flab::wire::AccessRule>::FromExternal(rules),
                                std::move(session_endpoints.server));
  ASSERT_TRUE(open_res.ok()) << open_res.FormatDescription();
  ASSERT_TRUE(open_res->is_error());
  EXPECT_EQ(open_res->error_value(), flab::wire::OpenSessionError::kRejectedAllowlist);
}

TEST(DriverLabCppTest, MaskedWritableRegisterLimitsWriteMask) {
  // CTRL at 0x10 holds a run/stop bit (31) next to a host-tunable field in
  // bits [11:8].
  constexpr uint64_t kCtrl = 0x10;
  constexpr uint32_t kTunableField = 0x0000'0F00u;
  constexpr uint32_t kCtrlInitial = 0x8000'0001u;
  zx::vmo mmio_vmo;
  ASSERT_EQ(zx::vmo::create(4096, 0, &mmio_vmo), ZX_OK);
  ASSERT_EQ(mmio_vmo.write(&kCtrlInitial, kCtrl, sizeof(kCtrlInitial)), ZX_OK);

  Builder builder("cpp-masked-test");
  builder.SetEnabled(true);
  auto regs_id_res = builder.AddMmioVmo("regs", mmio_vmo.borrow(), 0, 0x100);
  ASSERT_TRUE(regs_id_res.is_ok());
  const uint32_t regs_id = *regs_id_res;
  builder.SetMaskedWritableRegisters(regs_id, {{.offset = kCtrl, .allow_mask = kTunableField}});

  auto server_res = builder.Build();
  ASSERT_TRUE(server_res.is_ok());
  EmbeddedServer server = std::move(*server_res);

  auto proxy_endpoints = fidl::Endpoints<flab::Proxy>::Create();
  ASSERT_TRUE(server.ServeProxy(std::move(proxy_endpoints.server)).is_ok());
  fidl::WireSyncClient<flab::Proxy> proxy_client(std::move(proxy_endpoints.client));

  auto desc_res = proxy_client->Describe();
  ASSERT_TRUE(desc_res.ok());
  const auto& desc = *desc_res;

  fidl::Arena arena;
  auto context = flab::wire::RunContext::Builder(arena)
                     .run_id("run-masked")
                     .case_id("case-masked")
                     .plan_digest("sha256:masked")
                     .host_tool_version("0.1.0")
                     .Build();
  auto expectations = flab::wire::Expectations::Builder(arena)
                          .boot_id(desc.boot_id())
                          .proxy_generation(desc.proxy_generation())
                          .resource_digest(desc.resource_digest())
                          .policy_digest(desc.policy_digest())
                          .Build();
  std::vector<flab::wire::AccessRule> rules = {
      {
          .resource = regs_id,
          .offset = kCtrl,
          .width = 4,
          .class_ = flab::wire::AccessClass::kWrite,
      },
  };
  auto session_endpoints = fidl::Endpoints<flab::Session>::Create();
  auto open_res =
      proxy_client->OpenSession(context, flab::wire::SessionMode::kMutating, expectations,
                                fidl::VectorView<flab::wire::AccessRule>::FromExternal(rules),
                                std::move(session_endpoints.server));
  ASSERT_TRUE(open_res.ok()) << open_res.FormatDescription();
  ASSERT_TRUE(open_res->is_ok());
  fidl::WireSyncClient<flab::Session> session_client(std::move(session_endpoints.client));

  auto read_ctrl = [&]() {
    uint32_t value = 0;
    EXPECT_EQ(mmio_vmo.read(&value, kCtrl, sizeof(value)), ZX_OK);
    return value;
  };

  // A write within the allowed field is applied as read-modify-write and
  // preserves the other bits.
  {
    auto write_res =
        session_client->Write32(regs_id, kCtrl, 0x0000'0500u, kTunableField, nullptr, false);
    ASSERT_TRUE(write_res.ok()) << write_res.FormatDescription();
    ASSERT_TRUE(write_res->is_ok());
    EXPECT_EQ(read_ctrl(), 0x8000'0501u);
  }

  // Writes that touch bits outside the allowed field are denied, including a
  // full-width write, and leave the register unchanged.
  for (uint32_t write_mask : {0x8000'0000u, kTunableField | 0x1u, 0xFFFF'FFFFu}) {
    auto write_res = session_client->Write32(regs_id, kCtrl, 0u, write_mask, nullptr, false);
    ASSERT_TRUE(write_res.ok()) << write_res.FormatDescription();
    ASSERT_TRUE(write_res->is_error()) << "write_mask=" << write_mask;
    EXPECT_EQ(write_res->error_value(), flab::wire::OperationError::kWriteNotPermitted);
    EXPECT_EQ(read_ctrl(), 0x8000'0501u);
  }
}

}  // namespace
}  // namespace driver_lab
