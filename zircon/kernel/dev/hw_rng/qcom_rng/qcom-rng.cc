// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <dev/hw_rng/qcom_rng/init.h>

extern "C" {

// These assertions ensure that the size and alignment of zbi_dcfg_qcom_rng_t
// match the Rust DcfgQcomRng struct assertions in qcom_rng.rs.
static_assert(sizeof(zbi_dcfg_qcom_rng_t) == 16);
static_assert(alignof(zbi_dcfg_qcom_rng_t) == 8);

void rust_qcom_rng_init(const zbi_dcfg_qcom_rng_t& config);

}  // extern "C"

void QcomRngInit(const zbi_dcfg_qcom_rng_t& config) { rust_qcom_rng_init(config); }
