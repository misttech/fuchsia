// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "start-main.h"

#include <lib/ld/fuchsia-debugdata.h>
#include <lib/zx/channel.h>
#include <zircon/sanitizer.h>
#include <zircon/startup.h>

#include <cassert>
#include <mutex>

#include <runtime/thread.h>

#include "../threads/thread-list.h"
#include "src/stdlib/exit.h"
#include "threads_impl.h"
#include "zircon_impl.h"

namespace LIBC_NAMESPACE_DECL {
namespace {

void InitThreadList() {
  // Initialize the list of all threads.  Locking isn't really required since
  // there are no other threads yet, but it's harmlessly cheap and meets the
  // -Wthread-safety requirements.
  std::lock_guard lock(gAllThreadsLock);
  assert(gAllThreads == nullptr);
  pthread* self = __pthread_self();
  self->prevp = &gAllThreads;
  gAllThreads = self;
}

// If there was a deferred channel of /svc messages, forward that now.  If it
// has nowhere to go, drop it and its messages (and owned VMOs) now.
void ForwardSvc(zx::channel deferred) {
  zx::unowned_channel svc{__zircon_namespace_svc};
  if (deferred && *svc) {
    std::ignore = ld::Debugdata::Forward(svc->borrow(), std::move(deferred));
  }
}

// Let the sanitizer runtime initialize itself before constructors.
void SanitizerStartup(int argc, char** argv, char** envp) {
  const iovec& stack = __pthread_self()->safe_stack;
  __sanitizer_startup_hook(argc, argv, envp, stack.iov_base, stack.iov_len);
}

void BeforeCtors(zx::channel deferred, zx_startup_arguments_t args) {
  __environ = args.envp;

  InitThreadList();

  ForwardSvc(std::move(deferred));

  // Finish allocator setup.  Hereafter code outside libc proper can run and
  // might use most normal libc facilities, even though static constructors
  // haven't run yet.
  __libc_init_gwp_asan();

  StartupSanitizerModuleLoaded();
  SanitizerStartup(args.argc, args.argv, __environ);
}

// This does all the work of the final __libc_start_main before calling main.
// It's in a separate function so that main's direct caller will always be just
// __libc_start_main, but other calls won't be under the umbrella of the
// no_sanitizer attribute that is necessary to call main.
zx_startup_arguments_t PreMain(void* hook, zx_handle_t svc_server_end) {
  zx::channel deferred_svc{svc_server_end};

  // Now finish core libc initialization.
  zx_startup_arguments_t args = _zx_startup_get_arguments(hook);
  BeforeCtors(std::move(deferred_svc), args);

  // Do any final initialization that's contingent on the bootstrap protocol.
  _zx_startup_preinit(hook);

  // Finally run user constructors and then main.
  StartupCtors();

  return args;
}

}  // namespace

// This is called on the original machine stack, but outside the basic ABI
// bubble.  That makes it safe to use globals that might be instrumented.
void SetStartHandles(zx::process process_self, zx::vmar allocation_vmar, zx::thread thread_self) {
  __zircon_process_self = process_self.release();
  __zircon_vmar_root_self = allocation_vmar.release();

  // Minimally initialize the zxr_thread first, taking ownership of the thread
  // handle.  The locking code uses _zx_thread_self(), which fetches the handle
  // stored here.  So this is the bare minimum that must be done before more
  // normal operation, even taking locks, is possible.
  pthread* self = __pthread_self();
  zx_status_t status = zxr_thread_adopt(thread_self.release(), &self->zxr_thread);
  assert(status == ZX_OK);

  // Initialize the rest of the struct pthread now, since it's simple.
  self->locale = &libc.global_locale;

  // This is what's used to create new threads.  Each new thread inherits it
  // from the creating thread and then its slot might be reset later by
  // thrd_set_zx_process to affect the new threads it creates afterwards.
  self->process_handle = __zircon_process_self;
}

// This is called on the proper final machine stack.  The call to main doesn't
// necessarily match its actual signature as defined in the user's program
// exactly, since any of three signatures are traditionally supported (zero,
// two, or three arguments).  So `-fsanitize=function` must be suppressed.
[[clang::no_sanitize("function")]] void __libc_start_main(  //
    void* hook, zx_handle_t svc_server_end, MainFunction* main) {
  zx_startup_arguments_t args = PreMain(hook, svc_server_end);
  exit((*main)(args.argc, args.argv, __environ));
}

}  // namespace LIBC_NAMESPACE_DECL
