// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// TODO(https://fxbug.dev/571631316): Delete this once Fast UDP is no longer supported.

#include <stddef.h>

#include <zxtest/zxtest.h>

#include "udp_serde.h"

TEST(UdpSerde, StandaloneC) {
  Buffer buf = {
      .buf = NULL,
  };
  DeserializeSendMsgMetaResult res = deserialize_send_msg_meta(buf);
  EXPECT_EQ(res.err, DeserializeSendMsgMetaErrorInputBufferNull);
}
