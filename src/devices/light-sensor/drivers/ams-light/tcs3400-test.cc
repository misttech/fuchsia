// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "tcs3400.h"

#include <fidl/fuchsia.hardware.gpio/cpp/wire.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/async/default.h>
#include <lib/async_patterns/testing/cpp/dispatcher_bound.h>
#include <lib/component/outgoing/cpp/outgoing_directory.h>
#include <lib/ddk/metadata.h>
#include <lib/device-protocol/i2c-channel.h>
#include <lib/fake-i2c/fake-i2c.h>
#include <lib/inspect/cpp/inspect.h>
#include <lib/mock-i2c/mock-i2c.h>
#include <lib/zx/clock.h>

#include <ddktl/metadata/light-sensor.h>
#include <fbl/auto_lock.h>
#include <fbl/mutex.h>
#include <sdk/lib/inspect/testing/cpp/zxtest/inspect.h>
#include <zxtest/zxtest.h>

#include "lib/inspect/cpp/hierarchy.h"
#include "src/devices/gpio/testing/fake-gpio/fake-gpio.h"
#include "src/devices/testing/mock-ddk/mock-device.h"
#include "tcs3400-regs.h"

namespace tcs {

class FakeLightSensor : public fake_i2c::FakeI2c {
 public:
  uint8_t GetRegisterLastWrite(const uint8_t address) {
    fbl::AutoLock lock(&registers_lock_);
    return registers_[address].size() ? registers_[address].back() : 0;
  }
  uint8_t GetRegisterAtIndex(size_t index, const uint8_t address) {
    fbl::AutoLock lock(&registers_lock_);
    return registers_[address][index];
  }

  void SetRegister(const uint8_t address, const uint8_t value) {
    fbl::AutoLock lock(&registers_lock_);
    registers_[address].push_back(value);
  }

  sync_completion_t* read_completion() { return &read_completion_; }

  sync_completion_t* configuration_completion() { return &configuration_completion_; }

 protected:
  zx_status_t Transact(const uint8_t* write_buffer, size_t write_buffer_size, uint8_t* read_buffer,
                       size_t* read_buffer_size) override {
    if (write_buffer_size < 1) {
      return ZX_ERR_NOT_SUPPORTED;
    }

    const uint8_t address = write_buffer[0];
    write_buffer++;
    write_buffer_size--;

    // Assume that there are no multi-byte register accesses.

    if (write_buffer_size > 1) {
      return ZX_ERR_NOT_SUPPORTED;
    }

    {
      fbl::AutoLock lock(&registers_lock_);
      if (write_buffer_size == 1) {
        registers_[address].push_back(write_buffer[0]);
      }
    }
    read_buffer[0] = GetRegisterLastWrite(address);

    *read_buffer_size = 1;

    // The interrupt or timeout has been received and the driver is reading out the data registers.
    if (address == TCS_I2C_BDATAH) {
      sync_completion_signal(&read_completion_);
    } else if (!first_enable_written_ && address == TCS_I2C_ENABLE) {
      first_enable_written_ = true;
    } else if (first_enable_written_ && address == TCS_I2C_ENABLE) {
      first_enable_written_ = false;
      sync_completion_signal(&configuration_completion_);
    }

    return ZX_OK;
  }

 private:
  fbl::Mutex registers_lock_;
  std::array<std::vector<uint8_t>, UINT8_MAX> registers_ TA_GUARDED(registers_lock_) = {};
  sync_completion_t read_completion_;
  sync_completion_t configuration_completion_;
  bool first_enable_written_ = false;
};

struct IncomingNamespace {
  FakeLightSensor fake_i2c_;
  fake_gpio::FakeGpio fake_gpio_;
  component::OutgoingDirectory outgoing_{async_get_default_dispatcher()};
};

class Tcs3400Test : public inspect::InspectTestHelper, public zxtest::Test {
 public:
  void SetUp() override {
    ASSERT_OK(incoming_loop_.StartThread("incoming-ns-thread"));

    constexpr metadata::LightSensorParams kLightSensorMetadata = {
        .gain = 16,
        .integration_time_us = 615'000,
        .polling_time_us = 0,
    };

    fake_parent_->SetMetadata(DEVICE_METADATA_PRIVATE, &kLightSensorMetadata,
                              sizeof(kLightSensorMetadata));

    // Create i2c fragment.
    auto endpoints = fidl::CreateEndpoints<fuchsia_io::Directory>();
    ZX_ASSERT(endpoints.is_ok());
    incoming_.SyncCall([&endpoints](IncomingNamespace* incoming) {
      auto service_result = incoming->outgoing_.AddService<fuchsia_hardware_i2c::Service>(
          incoming->fake_i2c_.CreateInstanceHandler());
      ZX_ASSERT(service_result.is_ok());
      ZX_ASSERT(incoming->outgoing_.Serve(std::move(endpoints->server)).is_ok());
    });
    fake_parent_->AddFidlService(fuchsia_hardware_i2c::Service::Name, std::move(endpoints->client),
                                 "i2c");

    // Create gpio fragment.
    ASSERT_OK(zx::interrupt::create(zx::resource(ZX_HANDLE_INVALID), 0, ZX_INTERRUPT_VIRTUAL,
                                    &gpio_interrupt_));
    zx::interrupt gpio_interrupt;
    ASSERT_OK(gpio_interrupt_.duplicate(ZX_RIGHT_SAME_RIGHTS, &gpio_interrupt));
    endpoints = fidl::CreateEndpoints<fuchsia_io::Directory>();
    ZX_ASSERT(endpoints.is_ok());
    incoming_.SyncCall([&endpoints, &gpio_interrupt](IncomingNamespace* incoming) {
      incoming->fake_gpio_.SetInterrupt(zx::ok(std::move(gpio_interrupt)));
      auto service_result = incoming->outgoing_.AddService<fuchsia_hardware_gpio::Service>(
          incoming->fake_gpio_.CreateInstanceHandler());
      ZX_ASSERT(service_result.is_ok());
      ZX_ASSERT(incoming->outgoing_.Serve(std::move(endpoints->server)).is_ok());
    });
    fake_parent_->AddFidlService(fuchsia_hardware_gpio::Service::Name, std::move(endpoints->client),
                                 "gpio");

    auto result = fdf::RunOnDispatcherSync(dispatcher_->async_dispatcher(), [&]() {
      const auto status = Tcs3400Device::Create(nullptr, fake_parent_.get());
      ASSERT_OK(status);
    });
    EXPECT_OK(result.status_value());
    auto* child = fake_parent_->GetLatestChild();
    device_ = child->GetDeviceContext<Tcs3400Device>();

    WaitForConfiguration();

    incoming_.SyncCall([](IncomingNamespace* incoming) {
      EXPECT_EQ(incoming->fake_i2c_.GetRegisterLastWrite(TCS_I2C_ATIME), 35);
      EXPECT_EQ(incoming->fake_i2c_.GetRegisterLastWrite(TCS_I2C_CONTROL), 0x02);
    });
  }

  void TearDown() override {
    auto result = fdf::RunOnDispatcherSync(dispatcher_->async_dispatcher(), [&]() {
      device_async_remove(device_->zxdev());
      EXPECT_OK(mock_ddk::ReleaseFlaggedDevices(fake_parent_.get()));
    });
    EXPECT_OK(result.status_value());
  }

  fidl::ClientEnd<fuchsia_input_report::InputDevice> FidlClient() {
    auto endpoints = fidl::CreateEndpoints<fuchsia_input_report::InputDevice>();
    EXPECT_OK(endpoints);

    fidl::BindServer(dispatcher_->async_dispatcher(), std::move(endpoints->server), device_);
    return std::move(endpoints->client);
  }

 protected:
  static void GetFeatureReport(fidl::WireSyncClient<fuchsia_input_report::InputDevice>& client,
                               Tcs3400FeatureReport* const out_report) {
    const auto response = client->GetFeatureReport();
    ASSERT_TRUE(response.ok());
    ASSERT_FALSE(response->is_error());
    ASSERT_TRUE(response->value()->report.has_sensor());

    const auto& report = response->value()->report.sensor();
    EXPECT_TRUE(report.has_report_interval());
    ASSERT_TRUE(report.has_reporting_state());

    ASSERT_TRUE(report.has_sensitivity());
    ASSERT_EQ(report.sensitivity().count(), 1);

    ASSERT_TRUE(report.has_threshold_high());
    ASSERT_EQ(report.threshold_high().count(), 1);

    ASSERT_TRUE(report.has_threshold_low());
    ASSERT_EQ(report.threshold_low().count(), 1);

    ASSERT_TRUE(report.has_sampling_rate());

    out_report->report_interval_us = report.report_interval();
    out_report->reporting_state = report.reporting_state();
    out_report->sensitivity = report.sensitivity()[0];
    out_report->threshold_high = report.threshold_high()[0];
    out_report->threshold_low = report.threshold_low()[0];
    out_report->integration_time_us = report.sampling_rate();
  }

  static auto SetFeatureReport(fidl::WireSyncClient<fuchsia_input_report::InputDevice>& client,
                               const Tcs3400FeatureReport& report) {
    fidl::Arena<512> allocator;
    fidl::VectorView<int64_t> sensitivity(allocator, 1);
    sensitivity[0] = report.sensitivity;

    fidl::VectorView<int64_t> threshold_high(allocator, 1);
    threshold_high[0] = report.threshold_high;

    fidl::VectorView<int64_t> threshold_low(allocator, 1);
    threshold_low[0] = report.threshold_low;

    const auto set_sensor_report =
        fuchsia_input_report::wire::SensorFeatureReport::Builder(allocator)
            .report_interval(report.report_interval_us)
            .reporting_state(report.reporting_state)
            .sensitivity(sensitivity)
            .threshold_high(threshold_high)
            .threshold_low(threshold_low)
            .sampling_rate(report.integration_time_us)
            .Build();

    const auto set_report = fuchsia_input_report::wire::FeatureReport::Builder(allocator)
                                .sensor(set_sensor_report)
                                .Build();

    return client->SetFeatureReport(set_report);
  }

  void SetLightDataRegisters(uint16_t illuminance, uint16_t red, uint16_t green, uint16_t blue) {
    incoming_.SyncCall([&](IncomingNamespace* incoming) {
      incoming->fake_i2c_.SetRegister(TCS_I2C_CDATAL, illuminance & 0xff);
      incoming->fake_i2c_.SetRegister(TCS_I2C_CDATAH, illuminance >> 8);

      incoming->fake_i2c_.SetRegister(TCS_I2C_RDATAL, red & 0xff);
      incoming->fake_i2c_.SetRegister(TCS_I2C_RDATAH, red >> 8);

      incoming->fake_i2c_.SetRegister(TCS_I2C_GDATAL, green & 0xff);
      incoming->fake_i2c_.SetRegister(TCS_I2C_GDATAH, green >> 8);

      incoming->fake_i2c_.SetRegister(TCS_I2C_BDATAL, blue & 0xff);
      incoming->fake_i2c_.SetRegister(TCS_I2C_BDATAH, blue >> 8);
    });
  }

  void WaitForLightDataRead() {
    sync_completion_t* completion;
    incoming_.SyncCall([&completion](IncomingNamespace* incoming) {
      completion = incoming->fake_i2c_.read_completion();
    });

    sync_completion_wait(completion, ZX_TIME_INFINITE);
    sync_completion_reset(completion);
  }

  void WaitForConfiguration() {
    sync_completion_t* completion;
    incoming_.SyncCall([&completion](IncomingNamespace* incoming) {
      completion = incoming->fake_i2c_.configuration_completion();
    });

    sync_completion_wait(completion, ZX_TIME_INFINITE);
    sync_completion_reset(completion);
  }

 private:
  std::shared_ptr<MockDevice> fake_parent_ = MockDevice::FakeRootParent();
  fdf::UnownedSynchronizedDispatcher dispatcher_ =
      fdf_testing::DriverRuntime::GetInstance()->StartBackgroundDispatcher();

  async::Loop incoming_loop_{&kAsyncLoopConfigNoAttachToCurrentThread};

 protected:
  zx::interrupt gpio_interrupt_;
  Tcs3400Device* device_ = nullptr;
  async_patterns::TestDispatcherBound<IncomingNamespace> incoming_{incoming_loop_.dispatcher(),
                                                                   std::in_place};
};

TEST_F(Tcs3400Test, GetInputReport) {
  fidl::WireSyncClient<fuchsia_input_report::InputDevice> client(FidlClient());
  ASSERT_TRUE(client.client_end().is_valid());

  SetLightDataRegisters(0x1772, 0x95fa, 0xb263, 0x2f32);

  constexpr Tcs3400FeatureReport kEnableAllEvents = {
      .report_interval_us = 1'000,
      .reporting_state = fuchsia_input_report::wire::SensorReportingState::kReportAllEvents,
      .sensitivity = 16,
      .threshold_high = 0x8000,
      .threshold_low = 0x1000,
      .integration_time_us = 615'000,
  };

  {
    const auto response = SetFeatureReport(client, kEnableAllEvents);
    ASSERT_TRUE(response.ok());
    EXPECT_FALSE(response->is_error());
  }

  WaitForLightDataRead();

  for (;;) {
    // Wait for the driver's stored values to be updated.
    const auto response = client->GetInputReport(fuchsia_input_report::wire::DeviceType::kSensor);
    ASSERT_TRUE(response.ok());
    if (response->is_error()) {
      continue;
    }

    const auto& report = response->value()->report;

    ASSERT_TRUE(report.has_sensor());
    ASSERT_TRUE(report.sensor().has_values());
    ASSERT_EQ(report.sensor().values().count(), 4);

    EXPECT_EQ(report.sensor().values()[0], 0x1772);
    EXPECT_EQ(report.sensor().values()[1], 0x95fa);
    EXPECT_EQ(report.sensor().values()[2], 0xb263);
    EXPECT_EQ(report.sensor().values()[3], 0x2f32);
    break;
  }

  constexpr Tcs3400FeatureReport kEnableThresholdEvents = {
      .report_interval_us = 0,
      .reporting_state = fuchsia_input_report::wire::SensorReportingState::kReportThresholdEvents,
      .sensitivity = 16,
      .threshold_high = 0x8000,
      .threshold_low = 0x1000,
      .integration_time_us = 615'000,
  };

  {
    const auto response = SetFeatureReport(client, kEnableThresholdEvents);
    ASSERT_TRUE(response.ok());
    EXPECT_FALSE(response->is_error());
  }

  {
    const auto response = client->GetInputReport(fuchsia_input_report::wire::DeviceType::kSensor);
    ASSERT_TRUE(response.ok());
    // Not supported when only threshold events are enabled.
    EXPECT_TRUE(response->is_error());
  }

  constexpr Tcs3400FeatureReport kDisableEvents = {
      .report_interval_us = 0,
      .reporting_state = fuchsia_input_report::wire::SensorReportingState::kReportNoEvents,
      .sensitivity = 16,
      .threshold_high = 0x8000,
      .threshold_low = 0x1000,
      .integration_time_us = 615'000,
  };

  {
    const auto response = SetFeatureReport(client, kDisableEvents);
    ASSERT_TRUE(response.ok());
    EXPECT_FALSE(response->is_error());
  }

  {
    const auto response = client->GetInputReport(fuchsia_input_report::wire::DeviceType::kSensor);
    ASSERT_TRUE(response.ok());
    EXPECT_TRUE(response->is_error());
  }
}

TEST_F(Tcs3400Test, GetInputReports) {
  fidl::WireSyncClient<fuchsia_input_report::InputDevice> client(FidlClient());
  ASSERT_TRUE(client.client_end().is_valid());

  constexpr Tcs3400FeatureReport kEnableThresholdEvents = {
      .report_interval_us = 0,
      .reporting_state = fuchsia_input_report::wire::SensorReportingState::kReportThresholdEvents,
      .sensitivity = 16,
      .threshold_high = 0x8000,
      .threshold_low = 0x1000,
      .integration_time_us = 615'000,
  };

  {
    const auto response = SetFeatureReport(client, kEnableThresholdEvents);
    ASSERT_TRUE(response.ok());
    EXPECT_FALSE(response->is_error());
  }

  auto endpoints = fidl::Endpoints<fuchsia_input_report::InputReportsReader>::Create();
  fidl::WireSyncClient reader(std::move(endpoints.client));
  auto result = client->GetInputReportsReader(std::move(endpoints.server));
  ASSERT_OK(result.status());
  device_->WaitForNextReader();

  SetLightDataRegisters(0x00f8, 0xe79d, 0xa5e4, 0xfb1b);

  EXPECT_OK(gpio_interrupt_.trigger(0, zx::clock::get_boot()));

  // Wait for the driver to read out the data registers. At this point the interrupt has been ack'd
  // and it is safe to trigger again.
  WaitForLightDataRead();

  {
    const auto response = reader->ReadInputReports();
    ASSERT_TRUE(response.ok());
    ASSERT_TRUE(response->is_ok());

    const auto& reports = response->value()->reports;

    ASSERT_EQ(reports.count(), 1);
    ASSERT_TRUE(reports[0].has_sensor());
    ASSERT_TRUE(reports[0].sensor().has_values());
    ASSERT_EQ(reports[0].sensor().values().count(), 4);

    EXPECT_EQ(reports[0].sensor().values()[0], 0x00f8);
    EXPECT_EQ(reports[0].sensor().values()[1], 0xe79d);
    EXPECT_EQ(reports[0].sensor().values()[2], 0xa5e4);
    EXPECT_EQ(reports[0].sensor().values()[3], 0xfb1b);
  }

  SetLightDataRegisters(0x67f3, 0xbe39, 0x21e9, 0x319a);
  EXPECT_OK(gpio_interrupt_.trigger(0, zx::clock::get_boot()));
  WaitForLightDataRead();

  SetLightDataRegisters(0xa5df, 0x0101, 0xc776, 0xc531);
  EXPECT_OK(gpio_interrupt_.trigger(0, zx::clock::get_boot()));
  WaitForLightDataRead();

  // The previous illuminance value did not cross a threshold, so there should only be one report to
  // read out.
  {
    const auto response = reader->ReadInputReports();
    ASSERT_TRUE(response.ok());
    ASSERT_TRUE(response->is_ok());

    const auto& reports = response->value()->reports;

    ASSERT_EQ(reports.count(), 1);
    ASSERT_TRUE(reports[0].has_sensor());
    ASSERT_TRUE(reports[0].sensor().has_values());
    ASSERT_EQ(reports[0].sensor().values().count(), 4);

    EXPECT_EQ(reports[0].sensor().values()[0], 0xa5df);
    EXPECT_EQ(reports[0].sensor().values()[1], 0x0101);
    EXPECT_EQ(reports[0].sensor().values()[2], 0xc776);
    EXPECT_EQ(reports[0].sensor().values()[3], 0xc531);
  }

  SetLightDataRegisters(0x1772, 0x95fa, 0xb263, 0x2f32);

  constexpr Tcs3400FeatureReport kEnableAllEvents = {
      .report_interval_us = 1'000,
      .reporting_state = fuchsia_input_report::wire::SensorReportingState::kReportAllEvents,
      .sensitivity = 16,
      .threshold_high = 0x8000,
      .threshold_low = 0x1000,
      .integration_time_us = 615'000,
  };

  {
    const auto response = SetFeatureReport(client, kEnableAllEvents);
    ASSERT_TRUE(response.ok());
    EXPECT_FALSE(response->is_error());
  }

  for (uint32_t report_count = 0; report_count < 10;) {
    const auto response = reader->ReadInputReports();
    ASSERT_TRUE(response.ok());
    ASSERT_TRUE(response->is_ok());

    for (const auto& report : response->value()->reports) {
      ASSERT_TRUE(report.has_sensor());
      ASSERT_TRUE(report.sensor().has_values());
      ASSERT_EQ(report.sensor().values().count(), 4);

      EXPECT_EQ(report.sensor().values()[0], 0x1772);
      EXPECT_EQ(report.sensor().values()[1], 0x95fa);
      EXPECT_EQ(report.sensor().values()[2], 0xb263);
      EXPECT_EQ(report.sensor().values()[3], 0x2f32);
      report_count++;
    }
  }
}

TEST_F(Tcs3400Test, GetMultipleInputReports) {
  fidl::WireSyncClient<fuchsia_input_report::InputDevice> client(FidlClient());
  ASSERT_TRUE(client.client_end().is_valid());

  constexpr Tcs3400FeatureReport kEnableThresholdEvents = {
      .report_interval_us = 0,
      .reporting_state = fuchsia_input_report::wire::SensorReportingState::kReportThresholdEvents,
      .sensitivity = 16,
      .threshold_high = 0x8000,
      .threshold_low = 0x1000,
      .integration_time_us = 615'000,
  };

  const auto response = SetFeatureReport(client, kEnableThresholdEvents);
  ASSERT_TRUE(response.ok());
  EXPECT_FALSE(response->is_error());

  WaitForConfiguration();

  auto endpoints = fidl::Endpoints<fuchsia_input_report::InputReportsReader>::Create();
  fidl::WireSyncClient reader(std::move(endpoints.client));
  auto result = client->GetInputReportsReader(std::move(endpoints.server));
  ASSERT_OK(result.status());
  device_->WaitForNextReader();

  constexpr uint16_t kExpectedLightValues[][4] = {
      {0x00f8, 0xe79d, 0xfb1b, 0xa5e4},
      {0x87f3, 0xbe39, 0x319a, 0x21e9},
      {0xa772, 0x95fa, 0x2f32, 0xb263},
  };

  for (const auto& values : kExpectedLightValues) {
    SetLightDataRegisters(values[0], values[1], values[2], values[3]);
    EXPECT_OK(gpio_interrupt_.trigger(0, zx::clock::get_boot()));
    WaitForLightDataRead();
  }

  for (size_t i = 0; i < std::size(kExpectedLightValues);) {
    const auto response = reader->ReadInputReports();
    ASSERT_TRUE(response.ok());
    ASSERT_TRUE(response->is_ok());

    for (const auto& report : response->value()->reports) {
      ASSERT_TRUE(report.has_sensor());
      ASSERT_TRUE(report.sensor().has_values());
      ASSERT_EQ(report.sensor().values().count(), 4);

      EXPECT_EQ(report.sensor().values()[0], kExpectedLightValues[i][0]);
      EXPECT_EQ(report.sensor().values()[1], kExpectedLightValues[i][1]);
      EXPECT_EQ(report.sensor().values()[2], kExpectedLightValues[i][2]);
      EXPECT_EQ(report.sensor().values()[3], kExpectedLightValues[i][3]);
      i++;
    }
  }
}

TEST_F(Tcs3400Test, GetInputReportsMultipleReaders) {
  fidl::WireSyncClient<fuchsia_input_report::InputDevice> client(FidlClient());
  ASSERT_TRUE(client.client_end().is_valid());

  constexpr Tcs3400FeatureReport kEnableThresholdEvents = {
      .report_interval_us = 0,
      .reporting_state = fuchsia_input_report::wire::SensorReportingState::kReportThresholdEvents,
      .sensitivity = 16,
      .threshold_high = 0x8000,
      .threshold_low = 0x1000,
      .integration_time_us = 615'000,
  };

  const auto response = SetFeatureReport(client, kEnableThresholdEvents);
  ASSERT_TRUE(response.ok());
  EXPECT_FALSE(response->is_error());

  constexpr size_t kReaderCount = 5;

  fidl::WireSyncClient<fuchsia_input_report::InputReportsReader> readers[kReaderCount];
  for (auto& reader : readers) {
    auto endpoints = fidl::Endpoints<fuchsia_input_report::InputReportsReader>::Create();
    reader.Bind(std::move(endpoints.client));
    auto result = client->GetInputReportsReader(std::move(endpoints.server));
    ASSERT_OK(result.status());
    device_->WaitForNextReader();
  }

  SetLightDataRegisters(0x00f8, 0xe79d, 0xa5e4, 0xfb1b);

  EXPECT_OK(gpio_interrupt_.trigger(0, zx::clock::get_boot()));

  for (auto& reader : readers) {
    const auto response = reader->ReadInputReports();
    ASSERT_TRUE(response.ok());
    ASSERT_TRUE(response->is_ok());

    const auto& reports = response->value()->reports;

    ASSERT_EQ(reports.count(), 1);
    ASSERT_TRUE(reports[0].has_sensor());
    ASSERT_TRUE(reports[0].sensor().has_values());
    ASSERT_EQ(reports[0].sensor().values().count(), 4);

    EXPECT_EQ(reports[0].sensor().values()[0], 0x00f8);
    EXPECT_EQ(reports[0].sensor().values()[1], 0xe79d);
    EXPECT_EQ(reports[0].sensor().values()[2], 0xa5e4);
    EXPECT_EQ(reports[0].sensor().values()[3], 0xfb1b);
  }
}

TEST_F(Tcs3400Test, InputReportSaturatedSensor) {
  fidl::WireSyncClient<fuchsia_input_report::InputDevice> client(FidlClient());
  ASSERT_TRUE(client.client_end().is_valid());

  constexpr Tcs3400FeatureReport kEnableThresholdEvents = {
      .report_interval_us = 0,
      .reporting_state = fuchsia_input_report::wire::SensorReportingState::kReportAllEvents,
      .sensitivity = 16,
      .threshold_high = 0x8000,
      .threshold_low = 0x1000,
      .integration_time_us = 615'000,
  };

  {
    const auto response = SetFeatureReport(client, kEnableThresholdEvents);
    ASSERT_TRUE(response.ok());
    EXPECT_FALSE(response->is_error());
  }

  auto endpoints = fidl::Endpoints<fuchsia_input_report::InputReportsReader>::Create();
  fidl::WireSyncClient reader(std::move(endpoints.client));
  auto result = client->GetInputReportsReader(std::move(endpoints.server));
  ASSERT_OK(result.status());
  device_->WaitForNextReader();

  // Set normal value so we can be sure status register is causing saturation.
  SetLightDataRegisters(0x0010, 0x0010, 0x0010, 0x0010);
  incoming_.SyncCall([&](IncomingNamespace* incoming) {
    incoming->fake_i2c_.SetRegister(TCS_I2C_STATUS, 0x0 | TCS_I2C_STATUS_ASAT);
  });

  EXPECT_OK(gpio_interrupt_.trigger(0, zx::clock::get_boot()));

  WaitForLightDataRead();

  const auto response = reader->ReadInputReports();
  ASSERT_TRUE(response.ok());
  ASSERT_TRUE(response->is_ok());

  const auto& reports = response->value()->reports;

  ASSERT_EQ(reports.count(), 1);
  ASSERT_TRUE(reports[0].has_sensor());
  ASSERT_TRUE(reports[0].sensor().has_values());
  ASSERT_EQ(reports[0].sensor().values().count(), 4);

  EXPECT_EQ(reports[0].sensor().values()[0], 65085);
  EXPECT_EQ(reports[0].sensor().values()[1], 21067);
  EXPECT_EQ(reports[0].sensor().values()[2], 20395);
  EXPECT_EQ(reports[0].sensor().values()[3], 20939);

  incoming_.SyncCall([&](IncomingNamespace* incoming) {
    EXPECT_EQ(incoming->fake_i2c_.GetRegisterLastWrite(TCS_I2C_CICLEAR), 0x00);
  });
}

TEST_F(Tcs3400Test, GetDescriptor) {
  fidl::WireSyncClient<fuchsia_input_report::InputDevice> client(FidlClient());
  ASSERT_TRUE(client.client_end().is_valid());

  const auto response = client->GetDescriptor();
  ASSERT_TRUE(response.ok());
  ASSERT_TRUE(response.value().descriptor.has_device_information());
  ASSERT_TRUE(response.value().descriptor.has_sensor());
  ASSERT_TRUE(response.value().descriptor.sensor().has_input());
  ASSERT_EQ(response.value().descriptor.sensor().input().count(), 1);
  ASSERT_TRUE(response.value().descriptor.sensor().input()[0].has_values());
  ASSERT_EQ(response.value().descriptor.sensor().input()[0].values().count(), 4);

  EXPECT_EQ(response.value().descriptor.device_information().vendor_id(),
            static_cast<uint32_t>(fuchsia_input_report::wire::VendorId::kGoogle));
  EXPECT_EQ(
      response.value().descriptor.device_information().product_id(),
      static_cast<uint32_t>(fuchsia_input_report::wire::VendorGoogleProductId::kAmsLightSensor));

  const auto& sensor_axes = response.value().descriptor.sensor().input()[0].values();
  EXPECT_EQ(sensor_axes[0].type, fuchsia_input_report::wire::SensorType::kLightIlluminance);
  EXPECT_EQ(sensor_axes[1].type, fuchsia_input_report::wire::SensorType::kLightRed);
  EXPECT_EQ(sensor_axes[2].type, fuchsia_input_report::wire::SensorType::kLightGreen);
  EXPECT_EQ(sensor_axes[3].type, fuchsia_input_report::wire::SensorType::kLightBlue);

  for (const auto& axis : sensor_axes) {
    EXPECT_EQ(axis.axis.range.min, 0);
    EXPECT_EQ(axis.axis.range.max, UINT16_MAX);
    EXPECT_EQ(axis.axis.unit.type, fuchsia_input_report::wire::UnitType::kOther);
    EXPECT_EQ(axis.axis.unit.exponent, 0);
  }

  ASSERT_TRUE(response.value().descriptor.sensor().has_feature());
  ASSERT_EQ(response.value().descriptor.sensor().feature().count(), 1);
  const auto& feature_descriptor = response.value().descriptor.sensor().feature()[0];

  ASSERT_TRUE(feature_descriptor.has_report_interval());
  ASSERT_TRUE(feature_descriptor.has_supports_reporting_state());

  ASSERT_TRUE(feature_descriptor.has_sensitivity());
  ASSERT_EQ(feature_descriptor.sensitivity().count(), 1);

  ASSERT_TRUE(feature_descriptor.has_threshold_high());
  ASSERT_EQ(feature_descriptor.threshold_high().count(), 1);

  ASSERT_TRUE(feature_descriptor.has_threshold_low());
  ASSERT_EQ(feature_descriptor.threshold_low().count(), 1);

  EXPECT_EQ(feature_descriptor.report_interval().range.min, 0);
  EXPECT_EQ(feature_descriptor.report_interval().unit.type,
            fuchsia_input_report::wire::UnitType::kSeconds);
  EXPECT_EQ(feature_descriptor.report_interval().unit.exponent, -6);

  EXPECT_TRUE(feature_descriptor.supports_reporting_state());

  EXPECT_EQ(feature_descriptor.sensitivity()[0].type,
            fuchsia_input_report::wire::SensorType::kLightIlluminance);
  EXPECT_EQ(feature_descriptor.sensitivity()[0].axis.range.min, 1);
  EXPECT_EQ(feature_descriptor.sensitivity()[0].axis.range.max, 64);
  EXPECT_EQ(feature_descriptor.sensitivity()[0].axis.unit.type,
            fuchsia_input_report::wire::UnitType::kOther);
  EXPECT_EQ(feature_descriptor.sensitivity()[0].axis.unit.exponent, 0);

  EXPECT_EQ(feature_descriptor.threshold_high()[0].type,
            fuchsia_input_report::wire::SensorType::kLightIlluminance);
  EXPECT_EQ(feature_descriptor.threshold_high()[0].axis.range.min, 0);
  EXPECT_EQ(feature_descriptor.threshold_high()[0].axis.range.max, UINT16_MAX);
  EXPECT_EQ(feature_descriptor.threshold_high()[0].axis.unit.type,
            fuchsia_input_report::wire::UnitType::kOther);
  EXPECT_EQ(feature_descriptor.threshold_high()[0].axis.unit.exponent, 0);

  EXPECT_EQ(feature_descriptor.threshold_low()[0].type,
            fuchsia_input_report::wire::SensorType::kLightIlluminance);
  EXPECT_EQ(feature_descriptor.threshold_low()[0].axis.range.min, 0);
  EXPECT_EQ(feature_descriptor.threshold_low()[0].axis.range.max, UINT16_MAX);
  EXPECT_EQ(feature_descriptor.threshold_low()[0].axis.unit.type,
            fuchsia_input_report::wire::UnitType::kOther);
  EXPECT_EQ(feature_descriptor.threshold_low()[0].axis.unit.exponent, 0);
}

TEST_F(Tcs3400Test, FeatureReport) {
  fidl::WireSyncClient<fuchsia_input_report::InputDevice> client(FidlClient());
  ASSERT_TRUE(client.client_end().is_valid());

  Tcs3400FeatureReport report;
  ASSERT_NO_FATAL_FAILURE(GetFeatureReport(client, &report));

  // Check the default report values.
  EXPECT_EQ(report.reporting_state,
            fuchsia_input_report::wire::SensorReportingState::kReportAllEvents);
  EXPECT_EQ(report.threshold_high, 0xffff);
  EXPECT_EQ(report.threshold_low, 0x0000);
  EXPECT_EQ(report.integration_time_us, 614'380);

  // These values are passed in through metadata.
  EXPECT_EQ(report.report_interval_us, 0);
  EXPECT_EQ(report.sensitivity, 16);

  // Inspect report should match.
  ASSERT_NO_FATAL_FAILURE(ReadInspect(device_->inspect().DuplicateVmo()));
  auto* root = hierarchy().GetByPath({"feature_report", "1"});
  ASSERT_FALSE(root);

  incoming_.SyncCall([&](IncomingNamespace* incoming) {
    incoming->fake_i2c_.SetRegister(TCS_I2C_ENABLE, 0);
    incoming->fake_i2c_.SetRegister(TCS_I2C_AILTL, 0);
    incoming->fake_i2c_.SetRegister(TCS_I2C_AILTH, 0);
    incoming->fake_i2c_.SetRegister(TCS_I2C_AIHTL, 0);
    incoming->fake_i2c_.SetRegister(TCS_I2C_AIHTH, 0);
    incoming->fake_i2c_.SetRegister(TCS_I2C_PERS, 0);
    incoming->fake_i2c_.SetRegister(TCS_I2C_CONTROL, 0);
    incoming->fake_i2c_.SetRegister(TCS_I2C_ATIME, 0);
  });

  constexpr Tcs3400FeatureReport kNewFeatureReport = {
      .report_interval_us = 1'000,
      .reporting_state = fuchsia_input_report::wire::SensorReportingState::kReportAllEvents,
      .sensitivity = 64,
      .threshold_high = 0xabcd,
      .threshold_low = 0x1234,
      .integration_time_us = 278'000,
  };
  const auto response = SetFeatureReport(client, kNewFeatureReport);
  ASSERT_TRUE(response.ok());
  EXPECT_FALSE(response->is_error());

  WaitForConfiguration();

  incoming_.SyncCall([&](IncomingNamespace* incoming) {
    EXPECT_EQ(incoming->fake_i2c_.GetRegisterAtIndex(0, TCS_I2C_ENABLE), 0b0001'0001);
    EXPECT_EQ(incoming->fake_i2c_.GetRegisterLastWrite(TCS_I2C_AILTL), 0x34);
    EXPECT_EQ(incoming->fake_i2c_.GetRegisterLastWrite(TCS_I2C_AILTH), 0x12);
    EXPECT_EQ(incoming->fake_i2c_.GetRegisterLastWrite(TCS_I2C_AIHTL), 0xcd);
    EXPECT_EQ(incoming->fake_i2c_.GetRegisterLastWrite(TCS_I2C_AIHTH), 0xab);
    EXPECT_EQ(incoming->fake_i2c_.GetRegisterLastWrite(TCS_I2C_CONTROL), 3);
    EXPECT_EQ(incoming->fake_i2c_.GetRegisterLastWrite(TCS_I2C_ATIME), 156);
    EXPECT_EQ(incoming->fake_i2c_.GetRegisterAtIndex(1, TCS_I2C_ENABLE), 0b0001'0011);
  });

  ASSERT_NO_FATAL_FAILURE(GetFeatureReport(client, &report));
  EXPECT_EQ(report.report_interval_us, 1'000);
  EXPECT_EQ(report.reporting_state,
            fuchsia_input_report::wire::SensorReportingState::kReportAllEvents);
  EXPECT_EQ(report.sensitivity, 64);
  EXPECT_EQ(report.threshold_high, 0xabcd);
  EXPECT_EQ(report.threshold_low, 0x1234);
  EXPECT_EQ(report.integration_time_us, 278'000);

  // Inspect report should match.
  ASSERT_NO_FATAL_FAILURE(ReadInspect(device_->inspect().DuplicateVmo()));
  root = hierarchy().GetByPath({"feature_reports", "1"});
  ASSERT_TRUE(root);
  ASSERT_NO_FATAL_FAILURE(
      CheckProperty(root->node(), "report_interval_us", inspect::UintPropertyValue(1'000)));
  ASSERT_NO_FATAL_FAILURE(
      CheckProperty(root->node(), "reporting_state", inspect::StringPropertyValue("AllEvents")));
  ASSERT_NO_FATAL_FAILURE(
      CheckProperty(root->node(), "sensitivity", inspect::UintPropertyValue(64)));
  ASSERT_NO_FATAL_FAILURE(
      CheckProperty(root->node(), "threshold_high", inspect::UintPropertyValue(0xabcd)));
  ASSERT_NO_FATAL_FAILURE(
      CheckProperty(root->node(), "threshold_low", inspect::UintPropertyValue(0x1234)));
  ASSERT_NO_FATAL_FAILURE(
      CheckProperty(root->node(), "integration_time_us", inspect::UintPropertyValue(278'000)));
}

TEST_F(Tcs3400Test, SetInvalidFeatureReport) {
  fidl::WireSyncClient<fuchsia_input_report::InputDevice> client(FidlClient());
  ASSERT_TRUE(client.client_end().is_valid());

  constexpr Tcs3400FeatureReport kInvalidReportInterval = {
      .report_interval_us = -1,
      .reporting_state = fuchsia_input_report::wire::SensorReportingState::kReportAllEvents,
      .sensitivity = 1,
  };

  {
    const auto response = SetFeatureReport(client, kInvalidReportInterval);
    ASSERT_TRUE(response.ok());
    EXPECT_TRUE(response->is_error());
  }

  Tcs3400FeatureReport report;
  ASSERT_NO_FATAL_FAILURE(GetFeatureReport(client, &report));
  // Make sure the feature report wasn't affected by the bad call.
  EXPECT_EQ(report.sensitivity, 16);
  EXPECT_EQ(report.report_interval_us, 0);

  constexpr Tcs3400FeatureReport kInvalidSensitivity = {
      .reporting_state = fuchsia_input_report::wire::SensorReportingState::kReportAllEvents,
      .sensitivity = 50,
  };

  {
    const auto response = SetFeatureReport(client, kInvalidSensitivity);
    ASSERT_TRUE(response.ok());
    EXPECT_TRUE(response->is_error());
  }

  ASSERT_NO_FATAL_FAILURE(GetFeatureReport(client, &report));
  EXPECT_EQ(report.sensitivity, 16);

  constexpr Tcs3400FeatureReport kInvalidThresholdHigh = {
      .reporting_state = fuchsia_input_report::wire::SensorReportingState::kReportAllEvents,
      .sensitivity = 1,
      .threshold_high = 0x10000,
  };

  {
    const auto response = SetFeatureReport(client, kInvalidThresholdHigh);
    ASSERT_TRUE(response.ok());
    EXPECT_TRUE(response->is_error());
  }

  ASSERT_NO_FATAL_FAILURE(GetFeatureReport(client, &report));
  EXPECT_EQ(report.threshold_high, 0xffff);
  EXPECT_EQ(report.sensitivity, 16);

  // Make sure the call fails if a field is omitted.
  fidl::Arena<512> allocator;
  fidl::VectorView<int64_t> sensitivity(allocator, 1);
  sensitivity[0] = 1;

  fidl::VectorView<int64_t> threshold_high(allocator, 1);
  threshold_high[0] = 0;

  const auto set_sensor_report =
      fuchsia_input_report::wire::SensorFeatureReport::Builder(allocator)
          .report_interval(report.report_interval_us)
          .reporting_state(fuchsia_input_report::wire::SensorReportingState::kReportAllEvents)
          .sensitivity(sensitivity)
          .threshold_high(threshold_high)
          .Build();

  const auto set_report = fuchsia_input_report::wire::FeatureReport::Builder(allocator)
                              .sensor(set_sensor_report)
                              .Build();

  {
    const auto response = client->SetFeatureReport(set_report);
    ASSERT_TRUE(response.ok());
    EXPECT_TRUE(response->is_error());
  }

  ASSERT_NO_FATAL_FAILURE(GetFeatureReport(client, &report));
  EXPECT_EQ(report.threshold_high, 0xffff);
  EXPECT_EQ(report.threshold_low, 0x0000);
  EXPECT_EQ(report.sensitivity, 16);
  EXPECT_EQ(report.report_interval_us, 0);
  EXPECT_EQ(report.reporting_state,
            fuchsia_input_report::wire::SensorReportingState::kReportAllEvents);
}

class Tcs3400MetadataTest : public zxtest::Test {
 protected:
  void SetGainTest(uint8_t gain, uint8_t again_register) {
    // integration_time_us = 612'000 for atime = 36.
    SetGainAndIntegrationTest(gain, 612'000, again_register, 36);
  }

  void SetIntegrationTest(uint32_t integration_time_us, uint8_t atime_register) {
    // gain = 1 for again = 0x00.
    SetGainAndIntegrationTest(1, integration_time_us, 0x00, atime_register);
  }

  void SetGainAndIntegrationTest(uint8_t gain, uint32_t integration_time_us, uint8_t again_register,
                                 uint8_t atime_register) {
    const metadata::LightSensorParams metadata = {
        .gain = gain,
        .integration_time_us = integration_time_us,
    };

    std::shared_ptr<MockDevice> fake_parent = MockDevice::FakeRootParent();
    fdf::UnownedSynchronizedDispatcher dispatcher =
        fdf_testing::DriverRuntime::GetInstance()->StartBackgroundDispatcher();
    fake_parent->SetMetadata(DEVICE_METADATA_PRIVATE, &metadata, sizeof(metadata));

    async::Loop incoming_loop{&kAsyncLoopConfigNoAttachToCurrentThread};
    EXPECT_OK(incoming_loop.StartThread("incoming-ns-thread"));
    async_patterns::TestDispatcherBound<IncomingNamespace> incoming{incoming_loop.dispatcher(),
                                                                    std::in_place};

    // Create i2c fragment.
    auto endpoints = fidl::CreateEndpoints<fuchsia_io::Directory>();
    ZX_ASSERT(endpoints.is_ok());
    incoming.SyncCall([&](IncomingNamespace* incoming) {
      auto service_result = incoming->outgoing_.AddService<fuchsia_hardware_i2c::Service>(
          fuchsia_hardware_i2c::Service::InstanceHandler(
              {.device = incoming->fake_i2c_.bind_handler(async_get_default_dispatcher())}));
      ZX_ASSERT(service_result.is_ok());
      ZX_ASSERT(incoming->outgoing_.Serve(std::move(endpoints->server)).is_ok());
    });
    fake_parent->AddFidlService(fuchsia_hardware_i2c::Service::Name, std::move(endpoints->client),
                                "i2c");
    // Create gpio fragment.
    endpoints = fidl::CreateEndpoints<fuchsia_io::Directory>();
    ZX_ASSERT(endpoints.is_ok());
    incoming.SyncCall([&](IncomingNamespace* incoming) {
      auto service_result = incoming->outgoing_.AddService<fuchsia_hardware_gpio::Service>(
          fuchsia_hardware_gpio::Service::InstanceHandler(
              {.device = incoming->fake_gpio_.bind_handler(async_get_default_dispatcher())}));
      ZX_ASSERT(service_result.is_ok());
      ZX_ASSERT(incoming->outgoing_.Serve(std::move(endpoints->server)).is_ok());
    });
    fake_parent->AddFidlService(fuchsia_hardware_gpio::Service::Name, std::move(endpoints->client),
                                "gpio");

    incoming.SyncCall([](IncomingNamespace* incoming) {
      incoming->fake_i2c_.SetRegister(TCS_I2C_ATIME, 0xff);
      incoming->fake_i2c_.SetRegister(TCS_I2C_CONTROL, 0xff);
    });

    auto result = fdf::RunOnDispatcherSync(dispatcher->async_dispatcher(), [&]() {
      const auto status = Tcs3400Device::Create(nullptr, fake_parent.get());
      ASSERT_OK(status);
    });
    ASSERT_OK(result);
    auto* child = fake_parent->GetLatestChild();

    sync_completion_t* completion;
    incoming.SyncCall([&completion](IncomingNamespace* incoming) {
      completion = incoming->fake_i2c_.configuration_completion();
    });
    sync_completion_wait(completion, ZX_TIME_INFINITE);
    sync_completion_reset(completion);

    incoming.SyncCall([atime_register, again_register](IncomingNamespace* incoming) {
      EXPECT_EQ(incoming->fake_i2c_.GetRegisterLastWrite(TCS_I2C_ATIME), atime_register);
      EXPECT_EQ(incoming->fake_i2c_.GetRegisterLastWrite(TCS_I2C_CONTROL), again_register);
    });

    result = fdf::RunOnDispatcherSync(dispatcher->async_dispatcher(), [&]() {
      device_async_remove(child);
      EXPECT_OK(mock_ddk::ReleaseFlaggedDevices(fake_parent.get()));
    });
    ASSERT_OK(result);
  }
};

TEST_F(Tcs3400MetadataTest, Gain) {
  SetGainTest(99, 0x00);  // Invalid gain sets again = 0 (gain = 1).
  SetGainTest(1, 0x00);
  SetGainTest(4, 0x01);
  SetGainTest(16, 0x02);
  SetGainTest(64, 0x03);
}

TEST_F(Tcs3400MetadataTest, IntegrationTime) {
  SetIntegrationTest(750'000, 0x01);  // Invalid integration time sets atime = 1.
  SetIntegrationTest(708'900, 0x01);
  SetIntegrationTest(706'120, 0x02);
  SetIntegrationTest(703'340, 0x03);
  SetIntegrationTest(2'780, 0xFF);
}

TEST(Tcs3400Test, TooManyI2cErrors) {
  std::shared_ptr<MockDevice> fake_parent = MockDevice::FakeRootParent();
  metadata::LightSensorParams parameters = {};
  parameters.gain = 64;
  parameters.integration_time_us = 708'900;  // For atime = 0x01.

  async::Loop incoming_loop{&kAsyncLoopConfigNoAttachToCurrentThread};
  ASSERT_OK(incoming_loop.StartThread("incoming-ns-thread"));
  struct TestNamespace {
    mock_i2c::MockI2c mock_i2c;
    fake_gpio::FakeGpio fake_gpio;
  };
  async_patterns::TestDispatcherBound<TestNamespace> incoming{incoming_loop.dispatcher(),
                                                              std::in_place};

  auto i2c_endpoints = fidl::CreateEndpoints<fuchsia_hardware_i2c::Device>();
  EXPECT_TRUE(i2c_endpoints.is_ok());
  incoming.SyncCall([&i2c_endpoints](TestNamespace* test) {
    test->mock_i2c
        .ExpectWriteStop({0x81, 0x01}, ZX_ERR_INTERNAL)   // error, will retry.
        .ExpectWriteStop({0x81, 0x01}, ZX_ERR_INTERNAL)   // error, will retry.
        .ExpectWriteStop({0x81, 0x01}, ZX_ERR_INTERNAL);  // error, we are done.
    fidl::BindServer(async_get_default_dispatcher(), std::move(i2c_endpoints->server),
                     &test->mock_i2c);
  });

  auto gpio_endpoints = fidl::CreateEndpoints<fuchsia_hardware_gpio::Gpio>();
  ASSERT_TRUE(gpio_endpoints.is_ok());
  incoming.SyncCall([&gpio_endpoints](TestNamespace* test) {
    fidl::BindServer(async_get_default_dispatcher(), std::move(gpio_endpoints->server),
                     &test->fake_gpio);
  });

  Tcs3400Device device(fake_parent.get(), nullptr, std::move(i2c_endpoints->client),
                       std::move(gpio_endpoints->client));

  fake_parent->SetMetadata(DEVICE_METADATA_PRIVATE, &parameters,
                           sizeof(metadata::LightSensorParams));
  EXPECT_NOT_OK(device.InitMetadata());
}

}  // namespace tcs
