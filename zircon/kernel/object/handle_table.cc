// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "object/handle_table.h"

static_assert(sizeof(HandleTable) == kHandleTableStorageSize);
static_assert(alignof(HandleTable) == kHandleTableStorageAlign);
