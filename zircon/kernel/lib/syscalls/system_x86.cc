// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <bits.h>
#include <lib/boot-options/boot-options.h>
#include <trace.h>

#include <arch/arch_ops.h>
#include <arch/mp.h>
#include <arch/x86/feature.h>
#include <arch/x86/platform_access.h>
#include <kernel/ffi.h>
#include <kernel/percpu.h>
#include <kernel/recurring_callback.h>
#include <kernel/timer.h>

#define LOCAL_TRACE 0

namespace {

// Intel Volume 3 Section 14.9.3.
static constexpr uint64_t kPowerLimitPl1Enable = 1ull << 15;
static constexpr uint64_t kPowerLimitPl1Clamp = 1ull << 16;
static constexpr uint64_t kPowerLimitPl2Enable = 1ull << 47;
static constexpr uint64_t kPowerLimitPl2Clamp = 1ull << 48;

// Intel Volume 4 Table 2-39
static constexpr struct {
  uint64_t bit;
  const char* str;
} kLimitReasons[] = {
    {1 << 0, "PROCHOT"},
    {1 << 1, "Thermal event"},
    {1 << 4, "Residency state regulation limit"},
    {1 << 5, "Running average thermal limit"},
    {1 << 6, "Voltage regulator (VR) thermal alert"},
    {1 << 7, "Voltage regulator (VR) thermal design current limit"},
    {1 << 8, "Other"},
    {1 << 10, "Package/platform-Level PL1"},
    {1 << 11, "Package/platform-Level PL2"},
    {1 << 12, "Max turbo limit"},
    {1 << 13, "Turbo transition attenuation"},
};

// Intel Volume 4 Table 2-39 "MSR_GRAPHICS_PERF_LIMIT_REASONS"
static constexpr struct {
  uint64_t bit;
  const char* str;
} kLimitReasonsGfx[] = {
    {1 << 0, "PROCHOT"},
    {1 << 1, "Thermal event"},
    {1 << 5, "Running average thermal limit"},
    {1 << 6, "Voltage regulator (VR) thermal alert"},
    {1 << 7, "Voltage regulator (VR) thermal design current limit"},
    {1 << 8, "Other"},
    {1 << 10, "Package/platform-Level PL1"},
    {1 << 11, "Package/platform-Level PL2"},
    {1 << 12, "Inefficient operation"},
};

static constexpr uint64_t kLimitReasonsLogShift = 16;

struct rapl_units {
  uint32_t power_mw;
  uint32_t time_us;
  uint32_t energy_uj;
};

bool LimitReasonSupported() {
  // Limit reason MSR is supported on Intel Core generations 6 through 11, Intel Xeon generations
  // 1 through 3, Intel Core i3 8th generation, and Intel Xeon E processors. See Intel Volume 4
  // Table 2-39.
  auto x86_microarch = x86_get_microarch_config()->x86_microarch;
  if ((x86_microarch != X86_MICROARCH_INTEL_SKYLAKE) &&
      (x86_microarch != X86_MICROARCH_INTEL_CANNONLAKE) &&
      (x86_microarch != X86_MICROARCH_INTEL_TIGERLAKE)) {
    return false;
  }
  return true;
}

bool PowerLimitSuported() {
  auto x86_microarch = x86_get_microarch_config()->x86_microarch;
  if ((x86_microarch != X86_MICROARCH_INTEL_SANDY_BRIDGE) &&
      (x86_microarch != X86_MICROARCH_INTEL_SILVERMONT) &&
      (x86_microarch != X86_MICROARCH_INTEL_BROADWELL) &&
      (x86_microarch != X86_MICROARCH_INTEL_HASWELL) &&
      (x86_microarch != X86_MICROARCH_INTEL_SKYLAKE)) {
    return false;
  }
  return true;
}

rapl_units GetUnits(MsrAccess* msr) {
  // MSR_RAPL_POWER_UNIT provides the following information across all RAPL domains
  // Power Units[3:0]: power info (in watts) is based on the multiplier, 1/2^PU where PU is an
  // unsigned integer represented by bits [3:0].
  //
  // Time Units[19:16]: Time info (in seconds) is based on multiplier, 1/2^TU where TU is an
  // unsigned integer represented by bits[19:16]
  //
  // Energy Units[12:8]: Energy related information (in Joules) is based on the multiplier, 1/2^ESU,
  // where ESU is an unsigned integer represented by bits 12:8.
  //
  // Based on Intel Software Manual vol 3, chapter 14.9.
  //
  // To give better precision we specify power in milliwatts, time in microseconds, and energy in
  // microjoules.
  uint64_t rapl_unit = msr->read_msr(X86_MSR_RAPL_POWER_UNIT);
  rapl_units units = {
      .power_mw = 1000u / (1 << BITS_SHIFT(rapl_unit, 3, 0)),
      .time_us = 1000000u / (1 << BITS_SHIFT(rapl_unit, 19, 16)),
      .energy_uj = 1000000u / (1 << BITS_SHIFT(rapl_unit, 12, 8)),
  };
  return units;
}

void print_limits() {
  MsrAccess msr;
  rapl_units units = GetUnits(&msr);

  uint64_t rapl = msr.read_msr(X86_MSR_PKG_POWER_LIMIT);

  // Based on Intel Software Manual vol 3, chapter 14.9,
  // Time limit = 2^Y * (1.0 + Z/4.0) * Time_Unit
  auto y = static_cast<uint32_t>(BITS_SHIFT(rapl, 21, 17));
  auto z = static_cast<uint32_t>(BITS_SHIFT(rapl, 23, 22));
  uint32_t time_window = (1 << y) * (4 + z) * units.time_us / 4;
  auto power_limit = static_cast<uint32_t>(BITS_SHIFT(rapl, 14, 0));

  printf("PL1 limit: %umW\n", power_limit * units.power_mw);
  printf("PL1 window: %uus\n", time_window);
  printf("PL1 %sabled, clamping %sabled\n", rapl & kPowerLimitPl1Enable ? "en" : "dis",
         rapl & kPowerLimitPl1Clamp ? "en" : "dis");

  // Repeat for PL2
  y = BITS_SHIFT(rapl, 53, 49);
  z = BITS_SHIFT(rapl, 55, 54);
  time_window = (1 << y) * (4 + z) * units.time_us / 4;
  power_limit = BITS_SHIFT(rapl, 46, 32);

  printf("PL2 limit: %umW\n", power_limit * units.power_mw);
  printf("PL2 window: %uus\n", time_window);
  printf("PL2 %sabled, clamping %sabled\n", rapl & kPowerLimitPl2Enable ? "en" : "dis",
         rapl & kPowerLimitPl2Clamp ? "en" : "dis");
}

void clear_limit_reason_log() {
  if (!LimitReasonSupported()) {
    printf("Limit reasons msr not supported\n");
    return;
  }

  // The limit reason log is stored in bits 29:16 and can be cleared by writing zeros.
  MsrAccess msr;
  msr.write_msr(X86_MSR_PERF_LIMIT_REASONS, 0);
  msr.write_msr(X86_MSR_GFX_PERF_LIMIT_REASONS, 0);
}

void print_limit_reasons(bool use_log) {
  if (!LimitReasonSupported()) {
    printf("Limit reasons msr not supported\n");
    return;
  }

  MsrAccess msr;
  uint64_t limit_reasons = msr.read_msr(X86_MSR_PERF_LIMIT_REASONS);

  // The log bits (29:16) are latched versions of the status bits (13:0). If we're printing the log
  // shift the register value down.
  if (use_log) {
    limit_reasons = limit_reasons >> kLimitReasonsLogShift;
  }

  bool is_limited = false;
  printf("perf limit reasons:\n");
  for (auto reason : kLimitReasons) {
    if (!(limit_reasons & reason.bit)) {
      continue;
    }
    printf("\t%s\n", reason.str);
    is_limited = true;
  }
  if (!is_limited) {
    printf("\tnone\n");
  }

  limit_reasons = msr.read_msr(X86_MSR_GFX_PERF_LIMIT_REASONS);
  if (use_log) {
    limit_reasons = limit_reasons >> kLimitReasonsLogShift;
  }

  printf("gfx perf limit reasons:\n");
  is_limited = false;
  for (auto reason : kLimitReasonsGfx) {
    if (!(limit_reasons & reason.bit)) {
      continue;
    }
    printf("\t%s\n", reason.str);
    is_limited = true;
  }
  if (!is_limited) {
    printf("\tnone\n");
  }
}

RecurringCallback g_status_callback([]() {
  MsrAccess msr;
  rapl_units units = GetUnits(&msr);

  printf("energy consumed:\n");
  {
    static uint64_t last_energy_status = 0;

    uint64_t energy_status = read_msr(X86_MSR_PKG_ENERGY_STATUS);
    uint64_t uj = (energy_status - last_energy_status) * units.energy_uj;

    printf("\tpkg: %lu uJ (%lu J) (total: %lu uJ)\n", uj, uj / 1000000,
           energy_status * units.energy_uj);

    last_energy_status = energy_status;
  }

  {
    // PP0 usually is the core
    static uint64_t last_pp0_energy_status = 0;

    uint64_t pp0_energy_status = msr.read_msr(X86_MSR_PP0_ENERGY_STATUS);
    uint64_t uj = (pp0_energy_status - last_pp0_energy_status) * units.energy_uj;

    printf("\tpp0: %lu uJ (%lu J) (total: %lu uJ)\n", uj, uj / 1000000,
           pp0_energy_status * units.energy_uj);

    last_pp0_energy_status = pp0_energy_status;
  }

  {
    // PP1 usually is graphics
    static uint64_t last_pp1_energy_status = 0;

    uint64_t pp1_energy_status = msr.read_msr(X86_MSR_PP1_ENERGY_STATUS);
    uint64_t uj = (pp1_energy_status - last_pp1_energy_status) * units.energy_uj;

    printf("\tpp1: %lu uJ (%lu J) (total: %lu uJ)\n", uj, uj / 1000000,
           pp1_energy_status * units.energy_uj);

    last_pp1_energy_status = pp1_energy_status;
  }

  print_limit_reasons(/*use_log=*/false);
});

}  // namespace

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_x86_power_limit_suported() { return PowerLimitSuported(); }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_x86_status_callback_toggle() { g_status_callback.Toggle(); }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_x86_print_limit_reasons(bool use_log) { print_limit_reasons(use_log); }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_x86_clear_limit_reason_log() { clear_limit_reason_log(); }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_x86_print_limits() { print_limits(); }

}  // extern "C"
