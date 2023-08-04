// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef ZIRCON_SYSCALLS_MULTIOP_H_
#define ZIRCON_SYSCALLS_MULTIOP_H_

#include <zircon/compiler.h>
#include <zircon/types.h>

__BEGIN_CDECLS

typedef struct zx_mbmq_read_results {
  uint32_t actual_bytes;
  uint32_t actual_handles;
} zx_mbmq_read_results_t;

typedef struct zx_mbmq_multiop {
  int send_reply;
  // Used for mbo_write + channel_write_mbo, or for calleesref_send_reply.
  zx_handle_t mbo;
  // Used for channel_write_mbo.
  zx_handle_t channel;

  // Used for msgqueue_wait.
  zx_handle_t msgqueue;
  zx_handle_t calleesref;

  uint32_t pad;

  // Used for mbo_write and mbo_read.  We reuse zx_channel_call_args_t
  // for this because it contains the fields we want.
  zx_channel_call_args_t messages;

  // Used for mbo_read.
  zx_mbmq_read_results_t results;
} zx_mbmq_multiop_t;

__END_CDECLS

#endif  // ZIRCON_SYSCALLS_MULTIOP_H_
