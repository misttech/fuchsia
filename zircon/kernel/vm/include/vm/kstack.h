// Copyright 2018 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT
#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_KSTACK_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_KSTACK_H_

#include <sys/types.h>
#include <zircon/types.h>

#include <kernel/ffi.h>

// KernelStack encapsulates a kernel stack.
// A kernel stack object is not valid until Init() has been successfully
// called.
//
// KernelStack is implemented in Rust (vm/kstack.rs), which shares this layout; the methods below
// forward to it.
class KernelStack {
 public:
  KernelStack();

  // Destruction will automatically call Teardown();
  ~KernelStack();

  // Disallow copy; disallow move.
  KernelStack(KernelStack&) = delete;
  KernelStack& operator=(KernelStack&) = delete;
  KernelStack(KernelStack&&) = delete;
  KernelStack& operator=(KernelStack&&) = delete;

  // Initializes a kernel stack with appropriate overrun padding.
  zx_status_t Init();

  // Logs the relevant stack memory addresses at the given debug level.
  // This is useful during a thread dump.
  void DumpInfo(int debug_level) const;

  // Returns the stack to its pre-Init() state.
  zx_status_t Teardown();

  vaddr_t base() const;
  vaddr_t top() const;
#if __has_feature(safe_stack)
  vaddr_t unsafe_base() const;
  vaddr_t unsafe_top() const;
#endif
#if __has_feature(shadow_call_stack)
  vaddr_t shadow_call_base() const;
  vaddr_t shadow_call_top() const;
#endif

 private:
  // Each Rust `Mapping` holds an `Option<RefPtr<VmAddressRegion>>`.
  using Mapping = OpaqueStorage<sizeof(void*), alignof(void*)>;

  [[maybe_unused]] Mapping main_map_;

#if __has_feature(safe_stack)
#ifndef __x86_64__
#error "SafeStack in kernel supported only for x86; use ShadowCallStack"
#endif
  [[maybe_unused]] Mapping unsafe_map_;
#endif

#if __has_feature(shadow_call_stack)
  [[maybe_unused]] Mapping shadow_call_map_;
#endif
};

extern "C" {

void rust_kernel_stack_construct(KernelStack* stack);
void rust_kernel_stack_destroy(KernelStack* stack);
zx_status_t rust_kernel_stack_init(KernelStack* stack);
void rust_kernel_stack_dump_info(const KernelStack* stack, int debug_level);
zx_status_t rust_kernel_stack_teardown(KernelStack* stack);
vaddr_t rust_kernel_stack_base(const KernelStack* stack);
vaddr_t rust_kernel_stack_top(const KernelStack* stack);
#if __has_feature(safe_stack)
vaddr_t rust_kernel_stack_unsafe_base(const KernelStack* stack);
vaddr_t rust_kernel_stack_unsafe_top(const KernelStack* stack);
#endif
#if __has_feature(shadow_call_stack)
vaddr_t rust_kernel_stack_shadow_call_base(const KernelStack* stack);
vaddr_t rust_kernel_stack_shadow_call_top(const KernelStack* stack);
#endif

}  // extern "C"

inline KernelStack::KernelStack() { rust_kernel_stack_construct(this); }

inline KernelStack::~KernelStack() { rust_kernel_stack_destroy(this); }

inline zx_status_t KernelStack::Init() { return rust_kernel_stack_init(this); }

inline void KernelStack::DumpInfo(int debug_level) const {
  rust_kernel_stack_dump_info(this, debug_level);
}

inline zx_status_t KernelStack::Teardown() { return rust_kernel_stack_teardown(this); }

inline vaddr_t KernelStack::base() const { return rust_kernel_stack_base(this); }

inline vaddr_t KernelStack::top() const { return rust_kernel_stack_top(this); }

#if __has_feature(safe_stack)
inline vaddr_t KernelStack::unsafe_base() const { return rust_kernel_stack_unsafe_base(this); }

inline vaddr_t KernelStack::unsafe_top() const { return rust_kernel_stack_unsafe_top(this); }
#endif

#if __has_feature(shadow_call_stack)
inline vaddr_t KernelStack::shadow_call_base() const {
  return rust_kernel_stack_shadow_call_base(this);
}

inline vaddr_t KernelStack::shadow_call_top() const {
  return rust_kernel_stack_shadow_call_top(this);
}
#endif

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_KSTACK_H_
