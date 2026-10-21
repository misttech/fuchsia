// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.gpu.magma/cpp/wire.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <lib/component/incoming/cpp/service_member_watcher.h>

#include <charconv>
#include <climits>
#include <cmath>
#include <optional>
#include <print>
#include <string>
#include <string_view>
#include <vector>

#include "src/lib/fxl/command_line.h"

namespace {

// --frequency-state sends each value as a power state with the value in the
// lower 32 bits and this namespace in the upper 32 bits. b/573196006 tracks
// replacing this encoding with an explicit type.
constexpr uint64_t kFrequencyStateNamespace = 0x00000002;

// Sets the power state. `name` and `value` describe the request in the output.
// Returns the time in nanoseconds that it took to set the power state.
// Returns a negative number if there was an error.
int SetPowerState(fidl::ClientEnd<fuchsia_gpu_magma::DebugUtils>& client_end, int64_t power_state,
                  const char* name, int value) {
  auto result = fidl::WireCall(client_end)->SetPowerState(power_state);
  if (!result.ok()) {
    std::println(stderr, "magma SetPowerState transport failed: {}", result.status());
    return -1;
  }
  if (result->is_error()) {
    std::println(stderr, "magma SetPowerState failed: {}", result->error_value());
    return -1;
  }
  std::println("Setting {} to {}, took {:10} ns", name, value, result->value()->time_in_ns);
  return static_cast<int>(result->value()->time_in_ns);
}

// Parses a decimal integer. Returns std::nullopt if `s` is not one, e.g. "",
// "abc" or "3x".
std::optional<int> ParseInt(std::string_view s) {
  int value = 0;
  auto [end, ec] = std::from_chars(s.data(), s.data() + s.size(), value);
  if (ec != std::errc() || end != s.data() + s.size()) {
    return std::nullopt;
  }
  return value;
}

// Convert a comma separated list of numbers into a vector of integers.
// Eg: "1,2,34,5" => [1, 2, 34, 5]
// Returns std::nullopt if any item is not a number of at least `min`, e.g.
// "1,,2" or "1,". `name` describes the items in the error message.
std::optional<std::vector<int>> GetStates(const std::string& s, const char* name, int min) {
  std::vector<int> states;
  size_t start = 0;
  while (true) {
    size_t end = s.find(',', start);
    std::string item = s.substr(start, end == std::string::npos ? std::string::npos : end - start);
    std::optional<int> state = ParseInt(item);
    if (!state || *state < min) {
      std::println(stderr, "Invalid {} \"{}\"", name, item);
      return std::nullopt;
    }
    states.push_back(*state);
    if (end == std::string::npos) {
      return states;
    }
    start = end + 1;
  }
}

void PrintStats(const std::vector<int>& data, const char* units) {
  int max = 0;
  size_t sum = 0;
  for (int value : data) {
    sum += value;
    max = (max > value) ? max : value;
  }
  double mean = static_cast<double>(sum) / static_cast<double>(data.size());

  double sq_sum = 0.0;
  for (int value : data) {
    sq_sum += std::pow(value - mean, 2);
  }
  double variance = sq_sum / static_cast<double>(data.size() - 1);
  double std_dev = std::sqrt(variance);

  std::println("Count:   {:15}", data.size());
  std::println("Max:     {:15} {}", max, units);
  std::println("Mean:    {:15.0f} {}", mean, units);
  std::println("Std dev: {:15.0f} {}", std_dev, units);
}

}  // namespace

int main(int argc, char** argv) {
  auto command_line = fxl::CommandLineFromArgcArgv(argc, argv);

  // This CLI runs in `component explore` so we open our component's out dir.
  zx::result<fidl::ClientEnd<fuchsia_io::Directory>> svc = component::OpenServiceRoot("/out/svc");
  if (svc.is_error()) {
    std::println(stderr, "Failed to open /out/svc: {}", svc.status_string());
    return 1;
  }
  fidl::ClientEnd<fuchsia_io::Directory> svc_dir = std::move(svc).value();

  auto watcher =
      component::SyncServiceMemberWatcher<fuchsia_gpu_magma::PowerService::DebugUtils>(svc_dir);
  zx::result client_end = watcher.GetNextInstance(/*stop_at_idle=*/true);
  if (client_end.is_error()) {
    std::println("Failed to open magma device: {}", client_end.status_string());
    return -1;
  }

  static const char kCountFlag[] = "count";
  size_t count = 1;
  std::string count_string;
  if (command_line.GetOptionValue(kCountFlag, &count_string)) {
    std::optional<int> parsed_count = ParseInt(count_string);
    if (!parsed_count || *parsed_count < 1) {
      std::println(stderr, "Invalid count \"{}\"", count_string);
      return -1;
    }
    count = *parsed_count;
  }

  static const char kPowerStateFlag[] = "power-state";
  static const char kFrequencyStateFlag[] = "frequency-state";
  std::string power_state_string;
  std::string frequency_state_string;
  bool power_state_requested = command_line.GetOptionValue(kPowerStateFlag, &power_state_string);
  bool frequency_state_requested =
      command_line.GetOptionValue(kFrequencyStateFlag, &frequency_state_string);
  if (power_state_requested && frequency_state_requested) {
    std::println(stderr, "Use either --power-state or --frequency-state");
    return -1;
  }
  if (power_state_requested || frequency_state_requested) {
    const char* name = frequency_state_requested ? "frequency state" : "power state";
    std::optional<std::vector<int>> parsed_states =
        frequency_state_requested ? GetStates(frequency_state_string, name, /*min=*/INT_MIN)
                                  : GetStates(power_state_string, name, /*min=*/INT_MIN);
    if (!parsed_states) {
      return -1;
    }
    std::vector<int> states = std::move(*parsed_states);
    auto timings = std::vector<std::vector<int>>(states.size());
    for (size_t i = 0; i < count; i++) {
      for (size_t j = 0; j < states.size(); j++) {
        int64_t power_state = states[j];
        if (frequency_state_requested) {
          power_state = static_cast<int64_t>((kFrequencyStateNamespace << 32u) |
                                             static_cast<uint32_t>(states[j]));
        }
        int time = SetPowerState(client_end.value(), power_state, name, states[j]);
        if (time < 0) {
          return -1;
        }
        timings[j].push_back(time);
      }
    }

    for (size_t i = 0; i < states.size(); i++) {
      std::println("{}: {}", frequency_state_requested ? "Frequency State" : "Power State",
                   states[i]);
      PrintStats(timings[i], "ns");
      std::println("");
    }
  } else {
    std::println(stderr, "No request");
    return -1;
  }

  return 0;
}
