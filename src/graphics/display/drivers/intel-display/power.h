// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_GRAPHICS_DISPLAY_DRIVERS_INTEL_DISPLAY_POWER_H_
#define SRC_GRAPHICS_DISPLAY_DRIVERS_INTEL_DISPLAY_POWER_H_

#include <lib/mmio/mmio.h>

#include <unordered_map>

#include "src/graphics/display/drivers/intel-display/registers-ddi.h"
#include "src/graphics/display/drivers/intel-display/registers-pipe.h"

namespace intel_display {

class Power;
class PowerTest;
class Controller;

enum class PowerWellId {
  PG1 = 0,
  PG2 = 1,
  PG3 = 2,
  PG4 = 3,
  PG5 = 4,
};

struct PowerWellInfo {
  // Name of power well. For debug purpose only.
  const char* name = "";

  // The power well is always turned on and driver should not modify its power
  // status.
  bool always_on = false;

  // Index of the power well's state bit in the PWR_WELL_CTL register.
  size_t state_bit_index;
  // Index of the power well's request bit in the PWR_WELL_CTL register.
  size_t request_bit_index;
  // Index of the the status of fuse distribution to this power well in the
  // FUSE_STATUS register.
  size_t fuse_dist_bit_index;

  // The parent power well this power well depends on. If the power well doesn't
  // depend on any other power well, the value of |parent| will be the power
  // well itself.
  PowerWellId parent;
};
using PowerWellInfoMap = std::unordered_map<PowerWellId, PowerWellInfo>;

class PowerWellRef {
 public:
  PowerWellRef();
  ~PowerWellRef();

  PowerWellRef(Power* power, PowerWellId power_well);

  // Copying is not allowed.
  PowerWellRef(const PowerWellRef&) = delete;
  PowerWellRef& operator=(const PowerWellRef&) = delete;

  PowerWellRef(PowerWellRef&& o);
  PowerWellRef& operator=(PowerWellRef&& o);

 private:
  Power* power_ = nullptr;
  PowerWellId power_well_;
};

class Power {
 public:
  explicit Power(fdf::MmioBuffer* mmio_space, const PowerWellInfoMap* power_well_info);
  virtual ~Power() = default;

  // Copying and moving are not allowed.
  Power(const Power&) = delete;
  Power& operator=(const Power&) = delete;
  Power(Power&&) = delete;
  Power& operator=(Power&&) = delete;

  static std::unique_ptr<Power> New(fdf::MmioBuffer* mmio_space, uint16_t device_id);

  virtual PowerWellRef GetCdClockPowerWellRef() = 0;
  virtual PowerWellRef GetPipePowerWellRef(PipeId pipe_id) = 0;
  virtual PowerWellRef GetDdiPowerWellRef(DdiId ddi_id) = 0;

  // TODO(https://fxbug.dev/42182480): Support Thunderbolt. Currently the API assumes all
  // Type-C DDIs use USB-C IO.
  virtual bool GetDdiIoPowerState(DdiId ddi_id) = 0;
  virtual void SetDdiIoPowerState(DdiId ddi_id, bool enable) = 0;

  // TODO(https://fxbug.dev/42182480): Support Thunderbolt. Currently the API assumes all
  // Type-C DDIs use USB-C IO.
  virtual bool GetAuxIoPowerState(DdiId ddi_id) = 0;
  virtual void SetAuxIoPowerState(DdiId ddi_id, bool enable) = 0;

  virtual void Resume() = 0;

 protected:
  const fdf::MmioBuffer* mmio_space() const { return mmio_space_; }
  const PowerWellInfoMap* power_well_info_map() const { return power_well_info_map_; }
  const std::unordered_map<PowerWellId, size_t>& ref_count() const { return ref_count_; }

 private:
  void IncRefCount(PowerWellId power_well);
  void DecRefCount(PowerWellId power_well);

  virtual void SetPowerWell(PowerWellId power_well, bool enable) = 0;

  fdf::MmioBuffer* mmio_space_;
  std::unordered_map<PowerWellId, size_t> ref_count_;
  const PowerWellInfoMap* power_well_info_map_ = nullptr;

  friend PowerWellRef;
};

}  // namespace intel_display

#endif  // SRC_GRAPHICS_DISPLAY_DRIVERS_INTEL_DISPLAY_POWER_H_
