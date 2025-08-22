// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <phys/handoff.h>
#include <vm/handoff-end.h>

void ArchPostHandoffBootstrap(const ArchPhysHandoff& arch_handoff) {
  // TODO(https://fxbug.dev/42164859): Move tail of post-kASan-setup logic in
  // start.S here.
}
