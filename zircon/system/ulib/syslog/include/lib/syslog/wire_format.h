// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
//
// This header file defines wire format to transfer logs to listening service.

#ifndef LIB_SYSLOG_WIRE_FORMAT_H_
#define LIB_SYSLOG_WIRE_FORMAT_H_

#include <zircon/availability.h>

#if FUCHSIA_API_LEVEL_LESS_THAN(NEXT)
#include <lib/syslog/internal/wire_format.h>
#endif

#endif  // LIB_SYSLOG_WIRE_FORMAT_H_
