// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_HYPERVISOR_INCLUDE_HYPERVISOR_CPU_H_
#define ZIRCON_KERNEL_HYPERVISOR_INCLUDE_HYPERVISOR_CPU_H_

#include <lib/zx/result.h>
#include <zircon/types.h>

#include <kernel/cpu.h>

extern "C" cpu_mask_t rust_hypervisor_percpu_exec(zx_status_t (*task)(void* context,
                                                                      cpu_num_t cpu_num),
                                                  void* context);

namespace hypervisor {

using percpu_task_t = zx::result<> (*)(void* context, cpu_num_t cpu_num);

// Executes a task on each online CPU, and returns a CPU mask containing each
// CPU the task was successfully run on.
inline cpu_mask_t percpu_exec(percpu_task_t task, void* context) {
  struct TaskContext {
    percpu_task_t task;
    void* context;
  } ctx{task, context};
  return rust_hypervisor_percpu_exec(
      [](void* raw_ctx, cpu_num_t cpu_num) -> zx_status_t {
        auto* c = static_cast<TaskContext*>(raw_ctx);
        return c->task(c->context, cpu_num).status_value();
      },
      &ctx);
}

}  // namespace hypervisor

#endif  // ZIRCON_KERNEL_HYPERVISOR_INCLUDE_HYPERVISOR_CPU_H_
