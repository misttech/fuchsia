// Copyright 2025 The Fuchsia Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/starnix/tests/syscalls/cpp/binder_helper.h"

#include <fcntl.h>
#include <sys/ioctl.h>

#include <fbl/unique_fd.h>
#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"

namespace starnix_binder {

fbl::unique_fd OpenBinder(std::string_view dir) {
  return fbl::unique_fd(open((std::string(dir) + "/binder").c_str(), O_RDWR | O_CLOEXEC));
}

ParsedMessage ParseMessage(const binder_uintptr_t start, const binder_size_t length) {
  ParsedMessage m;

  const binder_uintptr_t end = start + length;

  binder_uintptr_t ptr = start;
  while (ptr < end) {
    binder_driver_return_protocol returned = *(binder_driver_return_protocol*)ptr;
    if (returned != BR_NOOP) {
      m.returns_.push_back(returned);
    }
    // `binder_driver_return_protocol` values are defined with `_IOR`/`_IO`
    // macros, encoding their payload size in the command value.
    ptr += sizeof(binder_driver_return_protocol) + _IOC_SIZE(returned);
  }
  EXPECT_EQ(ptr, end) << "binder read buffer did not parse cleanly";
  return m;
}

void EnterLooper(const fbl::unique_fd& binder_fd) {
  EnterLooperWriteBuffer write_buffer;
  struct binder_write_read write_read = {
      .write_size = sizeof(write_buffer),
      .write_consumed = 0,
      .write_buffer = (binder_uintptr_t)&write_buffer,
  };

  ASSERT_THAT(ioctl(binder_fd.get(), BINDER_WRITE_READ, &write_read), SyscallSucceeds());
}

FdTransaction::FdTransaction(uint32_t target_handle, uint32_t code, int fd)
    : fd_object{
          .hdr = {.type = BINDER_TYPE_FD},
          .pad_flags = 0x7f | FLAT_BINDER_FLAG_ACCEPTS_FDS,
          .fd = static_cast<uint32_t>(fd),
      },
      offset(0),
      write_buffer{.command = BC_TRANSACTION,
                   .data = {.target = {.handle = target_handle},
                            .code = code,
                            .flags = TF_ACCEPT_FDS,
                            .data_size = sizeof(struct binder_fd_object),
                            .offsets_size = sizeof(binder_size_t),
                            .data = {.ptr = {
                                         .buffer = (binder_uintptr_t)&fd_object,
                                         .offsets = (binder_uintptr_t)&offset,
                                     }}}} {}

}  // namespace starnix_binder
