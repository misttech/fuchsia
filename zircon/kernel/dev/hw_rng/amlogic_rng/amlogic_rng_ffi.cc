// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <dev/hw_rng/amlogic_rng/init.h>

extern "C" {

static_assert(sizeof(zbi_dcfg_amlogic_rng_driver_t) == 24,
              "zbi_dcfg_amlogic_rng_driver_t size mismatch");
static_assert(alignof(zbi_dcfg_amlogic_rng_driver_t) == 8,
              "zbi_dcfg_amlogic_rng_driver_t alignment mismatch");

void rust_amlogic_rng_init(const zbi_dcfg_amlogic_rng_driver_t& config);

}  // extern "C"

void AmlogicRngInit(const zbi_dcfg_amlogic_rng_driver_t& config) { rust_amlogic_rng_init(config); }
