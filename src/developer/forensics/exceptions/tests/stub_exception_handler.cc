// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be found in the LICENSE file.

#include <fidl/fuchsia.exception.internal/cpp/fidl.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/fit/function.h>
#include <zircon/processargs.h>

#include <cstdlib>
#include <utility>

namespace {

// Handle an exception by immediately executing the received callback.
class StubCrashReporter : public fidl::Server<fuchsia_exception_internal::CrashReporter> {
 public:
  explicit StubCrashReporter(fit::closure on_done) : on_done_(std::move(on_done)) {}

  void Send(SendRequest& request, SendCompleter::Sync& completer) override {
    completer.Reply({{.moniker = "unused moniker"}});
    on_done_();
  }

 private:
  // Function to execute after Send executes |completer|.
  ::fit::closure on_done_;
};

}  // namespace

int main(int argc, char** argv) {
  async::Loop loop(&kAsyncLoopConfigAttachToCurrentThread);

  zx::channel channel(zx_take_startup_handle(PA_HND(PA_USER0, 0)));
  if (!channel.is_valid()) {
    return EXIT_FAILURE;
  }

  // Shutdown the loop immediately after executing |Send|.
  StubCrashReporter stub_crash_reporter([&] { loop.Shutdown(); });

  fidl::ServerBinding<fuchsia_exception_internal::CrashReporter> crash_reporter_binding(
      loop.dispatcher(),
      fidl::ServerEnd<fuchsia_exception_internal::CrashReporter>(std::move(channel)),
      &stub_crash_reporter, fidl::kIgnoreBindingClosure);

  loop.Run();

  return EXIT_SUCCESS;
}
