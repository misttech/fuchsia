#include "threads_impl.h"

// The compiler supports __builtin_* names that just call these.

void* __get_unsafe_stack_start(void) {
#if HAVE_UNSAFE_STACK
  return __thrd_current()->unsafe_stack.iov_base;
#endif
  return NULL;
}

void* __get_unsafe_stack_top(void) {
#if HAVE_UNSAFE_STACK
  const struct iovec* stack = &__thrd_current()->unsafe_stack;
  return stack->iov_base + stack->iov_len;
#endif
  return NULL;
}

void* __get_unsafe_stack_ptr(void) {
#if HAVE_UNSAFE_STACK
  return (void*)__thrd_current()->abi.unsafe_sp;
#endif
  return NULL;
}
