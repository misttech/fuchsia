// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_STARNIX_TESTS_SYSCALLS_CPP_IO_URING_HELPER_H_
#define SRC_STARNIX_TESTS_SYSCALLS_CPP_IO_URING_HELPER_H_

#include <lib/fit/result.h>
#include <sys/syscall.h>
#include <unistd.h>

#include <atomic>
#include <memory>

#include <fbl/unique_fd.h>
#include <linux/io_uring.h>

#include "src/starnix/tests/syscalls/cpp/test_helper.h"

#ifndef IORING_SETUP_COOP_TASKRUN
#define IORING_SETUP_COOP_TASKRUN (1U << 8)
#endif

#ifndef IORING_SETUP_SINGLE_ISSUER
#define IORING_SETUP_SINGLE_ISSUER (1U << 12)
#endif

#ifndef IORING_SETUP_DEFER_TASKRUN
#define IORING_SETUP_DEFER_TASKRUN (1U << 13)
#endif

namespace io_uring_helper {

int io_uring_setup(uint32_t entries, io_uring_params* params);
int io_uring_enter(int fd, int to_submit, int min_complete, int flags, sigset_t* sigset);

// An RAII helper class that manages the lifetime and memory mappings of an io_uring instance.
class IoUring {
 public:
  // Returns the instance or the system errno on failure.
  static fit::result<int, std::unique_ptr<IoUring>> Create(uint32_t entries,
                                                           io_uring_params params = {});

  IoUring(const IoUring&) = delete;
  IoUring& operator=(const IoUring&) = delete;
  IoUring(IoUring&&) = delete;
  IoUring& operator=(IoUring&&) = delete;

  int fd() const { return ring_fd_.get(); }
  const io_uring_params& params() const { return params_; }

  io_uring_sqe* sqes() const { return sqes_; }
  io_uring_cqe* cqes() const { return cqes_; }
  uint32_t* sq_array() const { return sq_array_; }
  std::atomic<uint32_t>* sq_tail_ptr() const { return sq_tail_ptr_; }
  std::atomic<uint32_t>* cq_head_ptr() const { return cq_head_ptr_; }
  std::atomic<uint32_t>* cq_tail_ptr() const { return cq_tail_ptr_; }

 private:
  IoUring(fbl::unique_fd ring_fd, io_uring_params params, test_helper::ScopedMMap sq_mapping,
          test_helper::ScopedMMap cqe_mapping, test_helper::ScopedMMap sqe_mapping,
          io_uring_sqe* sqes, uint32_t* sq_array, std::atomic<uint32_t>* sq_tail_ptr,
          std::atomic<uint32_t>* cq_head_ptr, std::atomic<uint32_t>* cq_tail_ptr,
          io_uring_cqe* cqes);

  fbl::unique_fd ring_fd_;
  io_uring_params params_;
  test_helper::ScopedMMap sq_mapping_;
  test_helper::ScopedMMap cqe_mapping_;
  test_helper::ScopedMMap sqe_mapping_;
  io_uring_sqe* sqes_ = nullptr;
  io_uring_cqe* cqes_ = nullptr;
  uint32_t* sq_array_ = nullptr;
  std::atomic<uint32_t>* sq_tail_ptr_ = nullptr;
  std::atomic<uint32_t>* cq_head_ptr_ = nullptr;
  std::atomic<uint32_t>* cq_tail_ptr_ = nullptr;
};

}  // namespace io_uring_helper

#endif  // SRC_STARNIX_TESTS_SYSCALLS_CPP_IO_URING_HELPER_H_
