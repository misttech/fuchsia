// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "thread-storage.h"

#include <zircon/assert.h>

#include <array>
#include <utility>

#include "threads_impl.h"

namespace LIBC_NAMESPACE_DECL {

void ThreadStorage::FreeStacks() {
  auto unmap = [this, block_size = stack_size_ + guard_size_](uintptr_t base) {
    if (base != 0) {
      assert(thread_block_.vmar());
      zx::result result = zx::make_result(thread_block_.vmar().unmap(base, block_size.get()));
      ZX_ASSERT_MSG(result.is_ok(), "zx_vmar_unmap: %s", result.status_string());
    }
  };
  unmap(machine_stack_);
  unmap(unsafe_stack_);
  OnStack(shadow_call_stack_, unmap);
}

// Translate from the legacy C struct representation for ownership.
ThreadStorage ThreadStorage::FromThread(Thread& thread, zx::unowned_vmar vmar) {
  using Sizes = std::array<size_t, 2>;  // Stack size, guard size.
  constexpr auto infer_sizes = [](iovec stack, iovec region, bool grows_up = false) -> Sizes {
    assert(PageRoundedSize{stack.iov_len}.get() == stack.iov_len);
    assert(PageRoundedSize{region.iov_len}.get() == region.iov_len);
    assert(stack.iov_len <= region.iov_len);
    assert(stack.iov_base >= region.iov_base);
    if (stack.iov_base == region.iov_base) {
      assert(grows_up || stack.iov_len == region.iov_len);
    } else {
      assert(!grows_up);
      assert(reinterpret_cast<uintptr_t>(stack.iov_base) -
                 reinterpret_cast<uintptr_t>(region.iov_base) ==
             region.iov_len - stack.iov_len);
    }
    return {stack.iov_len, region.iov_len - stack.iov_len};
  };

  constexpr auto take_stack = [](iovec& stack, iovec& region) -> uintptr_t {
    stack = {};
    return reinterpret_cast<uintptr_t>(std::exchange(region, {}).iov_base);
  };

  Sizes stack_sizes = infer_sizes(thread.safe_stack, thread.safe_stack_region);
  assert(infer_sizes(thread.unsafe_stack, thread.unsafe_stack_region) == stack_sizes);
#if HAVE_SHADOW_CALL_STACK
  assert(infer_sizes(thread.shadow_call_stack, thread.shadow_call_stack_region) == stack_sizes);
#endif

  assert(*vmar);
  ThreadStorage result;
  result.thread_block_ = {std::exchange(thread.tcb_region, {}), vmar->borrow()};
  std::tie(result.stack_size_.rounded_size_, result.guard_size_.rounded_size_) = stack_sizes;
  result.machine_stack_ = take_stack(thread.safe_stack, thread.safe_stack_region);
  result.unsafe_stack_ = take_stack(thread.unsafe_stack, thread.unsafe_stack_region);
#if HAVE_SHADOW_CALL_STACK
  result.shadow_call_stack_ = take_stack(thread.shadow_call_stack, thread.shadow_call_stack_region);
#endif

  return result;
}

void ThreadStorage::ToThread(Thread& thread) && {
  auto take_stack = [this](iovec& stack, iovec& region, uintptr_t& base, bool grows_up = false) {
    assert(!stack.iov_base);
    assert(stack.iov_len == 0);
    assert(!region.iov_base);
    assert(region.iov_len == 0);
    region = {
        .iov_base = reinterpret_cast<void*>(base),
        .iov_len = (stack_size_ + guard_size_).get(),
    };
    stack = {
        .iov_base = reinterpret_cast<void*>(base + (grows_up ? 0 : guard_size_.get())),
        .iov_len = stack_size_.get(),
    };
    base = 0;
  };

  thread.tcb_region = std::move(thread_block_).TakeIovec();

  take_stack(thread.safe_stack, thread.safe_stack_region, machine_stack_);
  take_stack(thread.unsafe_stack, thread.unsafe_stack_region, unsafe_stack_);
#if HAVE_SHADOW_CALL_STACK
  take_stack(thread.shadow_call_stack, thread.shadow_call_stack_region, shadow_call_stack_, true);
#endif
}

}  // namespace LIBC_NAMESPACE_DECL
