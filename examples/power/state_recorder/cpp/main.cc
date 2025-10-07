// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/async/cpp/task.h>
#include <lib/inspect/component/cpp/component.h>
#include <lib/inspect/cpp/inspect.h>
#include <lib/power/state_recorder/cpp/state_recorder.h>
#include <lib/sys/cpp/component_context.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/trace-provider/provider.h>
#include <lib/trace/event.h>

#include <cstdint>
#include <map>
#include <memory>

namespace {

using power_observability::StateRecorder;

enum class FanSpeed : uint8_t {
  kOff = 0,
  kLow = 1,
  kHigh = 2,
};

const std::map<FanSpeed, std::string> kFanSpeedStates = {
    {FanSpeed::kOff, "OFF"},
    {FanSpeed::kLow, "LOW"},
    {FanSpeed::kHigh, "HIGH"},
};

class ExampleComponent {
 public:
  ExampleComponent()
      : loop_(&kAsyncLoopConfigAttachToCurrentThread),
        trace_provider_(loop_.dispatcher()),
        inspector_(loop_.dispatcher(), {}),
        manager_(inspector_),
        metadata_{
            .name = "fan_speed",
            .states = kFanSpeedStates,
            .trace_category_literal = "power_example",
        },
        tick_task_(this),
        transition_task_(this) {
    auto result = StateRecorder<FanSpeed>::Create(metadata_, FanSpeed::kOff, 10, manager_);
    ZX_ASSERT_MSG(result.is_ok(), "Failed with status %s", result.status_string());
    recorder_ = std::move(result.value());
  }

  ~ExampleComponent() { loop_.Shutdown(); }

  void Run() {
    tick_task_.PostDelayed(loop_.dispatcher(), zx::msec(100));
    transition_task_.PostDelayed(loop_.dispatcher(), zx::sec(1));
    loop_.Run();
  }

 private:
  void Tick(async_dispatcher_t* dispatcher, async::TaskBase* task, zx_status_t status) {
    if (status != ZX_OK) {
      return;
    }
    TRACE_INSTANT("power_example", "tick", TRACE_SCOPE_PROCESS);
    task->PostDelayed(dispatcher, zx::msec(100));
  }

  void Transition(async_dispatcher_t* dispatcher, async::TaskBase* task, zx_status_t status) {
    if (status != ZX_OK) {
      return;
    }
    recorder_->RecordTransition(static_cast<FanSpeed>(transition_counter_ % 3));
    transition_counter_++;
    task->PostDelayed(dispatcher, zx::sec(1));
  }

  async::Loop loop_;
  trace::TraceProviderWithFdio trace_provider_;
  inspect::ComponentInspector inspector_;
  power_observability::StateRecorderManager manager_;
  power_observability::EnumStateMetadata<FanSpeed> metadata_;
  std::optional<StateRecorder<FanSpeed>> recorder_;
  uint32_t transition_counter_ = 1;

  async::TaskMethod<ExampleComponent, &ExampleComponent::Tick> tick_task_;
  async::TaskMethod<ExampleComponent, &ExampleComponent::Transition> transition_task_;
};

}  // namespace

int main(int argc, const char** argv) {
  ExampleComponent component;
  component.Run();
  return 0;
}
