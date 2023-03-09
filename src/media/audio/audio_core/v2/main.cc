// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/async/cpp/task.h>
#include <lib/sys/cpp/component_context.h>
#include <lib/syslog/cpp/macros.h>

#include "src/lib/fxl/command_line.h"
#include "src/media/audio/audio_core/v2/audio_core_component.h"
#include "src/media/audio/services/common/fidl_thread.h"
#include "src/media/audio/audio_core/shared/profile_acquirer.h"

namespace {

void SetDispatcherRole() {
  // We receive audio payloads over FIDL, which means the FIDL thread has real time requirements
  // just like the mixing threads.
  // TODO(fxbug.dev/98652): the mixer service's graph threads should do this too
  auto result =
      media::audio::AcquireSchedulerRole(zx::thread::self(), "fuchsia.media.audio.core.dispatch");
  if (result.is_error()) {
    FX_PLOGS(ERROR, result.status_value())
        << "Unable to acquire scheduler role for the audio_core FIDL thread";
  }
}

}  // namespace

int main(int argc, const char** argv) {
  FX_LOGS(INFO) << "AudioCore starting up";

  auto cl = fxl::CommandLineFromArgcArgv(argc, argv);
  const auto enable_cobalt = !cl.HasOption("disable-cobalt");

  async::Loop fidl_loop{&kAsyncLoopConfigAttachToCurrentThread};
  async::Loop io_loop{&kAsyncLoopConfigNoAttachToCurrentThread};

  auto component_context = sys::ComponentContext::CreateAndServeOutgoingDirectory();
  media_audio::AudioCoreComponent component(*component_context, fidl_loop.dispatcher(),
                                            io_loop.dispatcher(), enable_cobalt);

  // Run io on a bg thread and fidl on the main thread.
  io_loop.StartThread("io");

  async::PostTask(fidl_loop.dispatcher(), [] { SetDispatcherRole(); });
  fidl_loop.Run();

  // Join.
  async::PostTask(io_loop.dispatcher(), [&io_loop] { io_loop.Quit(); });
  io_loop.JoinThreads();

  return 0;
}
