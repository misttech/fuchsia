// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//
// This header contains definition for the logger object and protocol.

#ifndef LIB_SYSLOG_LOGGER_H_
#define LIB_SYSLOG_LOGGER_H_

#include <zircon/availability.h>

#if FUCHSIA_API_LEVEL_LESS_THAN(NEXT)
#include <lib/syslog/internal/logger.h>
#endif

#endif  // LIB_SYSLOG_LOGGER_H_
