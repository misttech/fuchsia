// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/media/audio/audio_core/shared/profile_acquirer.h"

#include <fuchsia/scheduler/cpp/fidl.h>
#include <lib/fdio/directory.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/trace/event.h>
#include <lib/zx/channel.h>

#include <cstdlib>
#include <memory>

#include "src/media/audio/audio_core/shared/mix_profile_config.h"

namespace media::audio {

namespace {

using UniqueProfileProviderProxy = std::unique_ptr<fuchsia::scheduler::ProfileProvider_SyncProxy>;

zx::result<UniqueProfileProviderProxy> ConnectToProfileProvider() {
  zx::channel ch0, ch1;
  zx_status_t res = zx::channel::create(0u, &ch0, &ch1);
  if (res != ZX_OK) {
    FX_LOGS(ERROR) << "Failed to create channel, res=" << res;
    return zx::error(res);
  }

  res = fdio_service_connect(
      (std::string("/svc/") + fuchsia::scheduler::ProfileProvider::Name_).c_str(), ch0.release());
  if (res != ZX_OK) {
    FX_LOGS(WARNING) << "Failed to connect to ProfileProvider, res=" << res;
    return zx::error(res);
  }

  return zx::ok(std::make_unique<fuchsia::scheduler::ProfileProvider_SyncProxy>(std::move(ch1)));
}

}  // namespace

zx::result<> AcquireSchedulerRole(zx::unowned_thread thread, const std::string& role) {
  TRACE_DURATION("audio", "AcquireSchedulerRole", "role", TA_STRING(role.c_str()));

  zx::result<UniqueProfileProviderProxy> client = ConnectToProfileProvider();
  if (client.is_error()) {
    return client.take_error();
  }

  zx::thread dup_thread;
  const zx_status_t dup_status = thread->duplicate(ZX_RIGHT_SAME_RIGHTS, &dup_thread);
  if (dup_status != ZX_OK) {
    FX_PLOGS(ERROR, dup_status) << "Failed to connect to duplicate thread handle";
    return zx::error(dup_status);
  }
  int32_t fidl_status;
  const zx_status_t status = client->SetProfileByRole(std::move(dup_thread), role, &fidl_status);
  if (status != ZX_OK) {
    FX_LOGS(ERROR) << "Failed to call SetProfileByRole, error=" << status;
    return zx::error(status);
  }
  if (fidl_status != ZX_OK) {
    FX_PLOGS(ERROR, fidl_status) << "Failed to set role";
    return zx::error(fidl_status);
  }
  return zx::ok();
}

}  // namespace media::audio
