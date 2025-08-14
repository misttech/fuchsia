// Copyright 2018 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/boot-options/boot-options.h>
#include <lib/counters.h>

#include <arch/arm64/smccc.h>

#include "driver_priv.h"

namespace {

KCOUNTER(arm_smccc_fast_calls, "arm_smccc.fast_calls")
KCOUNTER(arm_smccc_yielding_calls, "arm_smccc.yielding_calls")
KCOUNTER(arm_smccc_qcom_interrupted, "arm_smccc.qcom.interrupted")
DECLARE_SINGLETON_MUTEX(QcomSmcLock);

// Fast Call
// ARM SMCCC w0 encodes the function ID.
// ARM SMCCC w0[31] is 1 for Fast Calls.
constexpr bool IsSmcccFastCall(uint32_t function_id) { return (function_id & (1 << 31)) != 0; }

}  // namespace

zx_status_t arch_smc_call(const zx_smc_parameters_t* params, zx_smc_result_t* result) {
  const uint32_t client_and_secure_os_id =
      static_cast<uint32_t>(params->secure_os_id) << 16 | static_cast<uint32_t>(params->client_id);
  arm_smccc_result_t arm_result;

  if (IsSmcccFastCall(params->func_id)) {
    kcounter_add(arm_smccc_fast_calls, 1);
    // TODO(74553): Detect when SMC calls take too long
    arm_result = arm_smccc_smc(params->func_id, params->arg1, params->arg2, params->arg3,
                               params->arg4, params->arg5, params->arg6, client_and_secure_os_id);
  } else {
    kcounter_add(arm_smccc_yielding_calls, 1);
    if (gBootOptions->arm64_smccc_qcom) {
      Guard<Mutex> lock(QcomSmcLock::Get());
      arm_result = arm_smccc_smc(params->func_id, params->arg1, params->arg2, params->arg3,
                                 params->arg4, params->arg5, params->arg6, client_and_secure_os_id);
      constexpr uint32_t kInterrupted = 0x1;
      // When interrupted:
      //  * `arm_result.x0` will be `kInterrupted`.
      //  * `arm_result.x6` will contain a session ID.
      //
      // In order to resume, a SMC must be issued with:
      //  * `w0`(function ID) must be `kInterrupted`.
      //  * `x6` must reuse the session ID returned earlier.
      while (arm_result.x0 == kInterrupted) {
        kcounter_add(arm_smccc_qcom_interrupted, 1);
        arm_result =
            arm_smccc_smc(kInterrupted, params->arg1, params->arg2, params->arg3, params->arg4,
                          params->arg5, arm_result.x6, client_and_secure_os_id);
      }
    } else {
      arm_result = arm_smccc_smc(params->func_id, params->arg1, params->arg2, params->arg3,
                                 params->arg4, params->arg5, params->arg6, client_and_secure_os_id);
    }
  }
  result->arg0 = arm_result.x0;
  result->arg1 = arm_result.x1;
  result->arg2 = arm_result.x2;
  result->arg3 = arm_result.x3;
  result->arg6 = arm_result.x6;

  return ZX_OK;
}
