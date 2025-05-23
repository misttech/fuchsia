#include <errno.h>
#include <lib/zircon-internal/unique-backtrace.h>
#include <time.h>
#include <zircon/syscalls.h>

#include "threads_impl.h"
#include "time_conversion.h"

int __timedwait_assign_owner(atomic_int* futex, int val, clockid_t clk, const struct timespec* at,
                             zx_handle_t new_owner) {
  zx_instant_mono_t deadline = ZX_TIME_INFINITE;

  if (at) {
    int ret = __timespec_to_deadline(at, clk, &deadline);
    if (ret)
      return ret;
  }

  // zx_futex_wait will return ZX_ERR_BAD_STATE if someone modifying *addr
  // races with this call. But this is indistinguishable from
  // otherwise being woken up just before someone else changes the
  // value. Therefore this functions returns 0 in that case.
  switch (_zx_futex_wait(futex, val, new_owner, deadline)) {
    case ZX_OK:
    case ZX_ERR_BAD_STATE:
      return 0;
    case ZX_ERR_TIMED_OUT:
      return ETIMEDOUT;
    case ZX_ERR_INVALID_ARGS:
    default:
      CRASH_WITH_UNIQUE_BACKTRACE();
  }
}
