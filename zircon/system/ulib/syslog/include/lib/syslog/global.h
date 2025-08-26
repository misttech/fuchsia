// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_SYSLOG_GLOBAL_H_
#define LIB_SYSLOG_GLOBAL_H_

#include <zircon/availability.h>

#if FUCHSIA_API_LEVEL_LESS_THAN(NEXT)
#include <lib/syslog/internal/global.h>
#endif

#endif  // LIB_SYSLOG_GLOBAL_H_
