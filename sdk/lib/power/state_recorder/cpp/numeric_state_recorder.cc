// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "lib/power/state_recorder/cpp/numeric_state_recorder.h"

#include <string>

namespace power_observability {

namespace {

std::string ToString(DecimalPrefix prefix) {
  switch (prefix) {
    case DecimalPrefix::Nano:
      return "n";
    case DecimalPrefix::Micro:
      return "u";
    case DecimalPrefix::Milli:
      return "m";
    case DecimalPrefix::Centi:
      return "c";
    case DecimalPrefix::Deci:
      return "d";
    case DecimalPrefix::Kilo:
      return "k";
    case DecimalPrefix::Mega:
      return "M";
    case DecimalPrefix::Giga:
      return "G";
  }
  __builtin_unreachable();
}

}  // namespace

std::string Units::ToString(BaseUnit base) {
  switch (base) {
    case BaseUnit::Amps:
      return "A";
    case BaseUnit::Hertz:
      return "Hz";
    case BaseUnit::Joules:
      return "J";
    case BaseUnit::Watts:
      return "W";
    case BaseUnit::Volts:
      return "V";
    case BaseUnit::Celsius:
      return "C";
    case BaseUnit::Number:
      return "#";
    case BaseUnit::Percent:
      return "%";
  }
  __builtin_unreachable();
}

std::string Units::ToString() const {
  if (prefix_.has_value()) {
    return std::format("{}{}", power_observability::ToString(prefix_.value()), ToString(base_));
  }
  return ToString(base_);
}

}  // namespace power_observability
