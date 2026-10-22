// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_SYS_BIN_PSUTILS_MEMGRAPH_INTERNAL_H_
#define SRC_SYS_BIN_PSUTILS_MEMGRAPH_INTERNAL_H_

#include <zircon/types.h>

#include <ostream>
#include <vector>

#include <task-utils/walker.h>

class JsonTaskEnumerator final : public TaskEnumerator {
 public:
  // |self_koid| is the koid of this memgraph process, so we can
  // avoid trying to read our own VMOs (which is illegal).
  JsonTaskEnumerator(zx_koid_t self_koid, bool show_threads, bool show_vmos, bool show_handle_stats,
                     std::ostream& out);

  const std::vector<zx_status_t>& partial_failures() const;

  zx_status_t OnJob(int depth, zx_handle_t job, zx_koid_t koid, zx_koid_t parent_koid) override;
  zx_status_t OnProcess(int depth, zx_handle_t process, zx_koid_t koid,
                        zx_koid_t parent_koid) override;
  zx_status_t OnThread(int depth, zx_handle_t thread, zx_koid_t koid,
                       zx_koid_t parent_koid) override;

 protected:
  bool has_on_job() const final;
  bool has_on_process() const final;
  bool has_on_thread() const final;

 private:
  const zx_koid_t self_koid_;
  const bool show_threads_;
  const bool show_vmos_;
  const bool show_handle_stats_;
  std::ostream& out_;
  // We try to keep going despite failures, but for scripting
  // purposes it's good to indicate failure at the end.
  std::vector<zx_status_t> partial_failures_;
};

#endif  // SRC_SYS_BIN_PSUTILS_MEMGRAPH_INTERNAL_H_
