// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_SSHD_HOST_AUTHORIZED_KEYS_H_
#define SRC_DEVELOPER_SSHD_HOST_AUTHORIZED_KEYS_H_

#include <fidl/fuchsia.boot/cpp/fidl.h>
#include <zircon/status.h>

namespace sshd_host {

// This function is primarily for debugging. In some cases the system can get
// into a state where we can't access the ssh keys, so we want to explicitly log
// when this is the case.
void check_authorized_keys();

zx_status_t provision_authorized_keys_from_bootloader_file(
    fidl::SyncClient<fuchsia_boot::Items>& boot_items);

}  // namespace sshd_host

#endif  // SRC_DEVELOPER_SSHD_HOST_AUTHORIZED_KEYS_H_
