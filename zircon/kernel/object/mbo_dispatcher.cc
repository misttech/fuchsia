// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "object/mbo_dispatcher.h"

zx_status_t MBODispatcher::Create(KernelHandle<MBODispatcher>* handle, zx_rights_t* rights) {
  fbl::AllocChecker ac;
  KernelHandle mbo(fbl::AdoptRef(new (&ac) MBODispatcher()));
  if (!ac.check())
    return ZX_ERR_NO_MEMORY;

  *rights = default_rights();
  *handle = ktl::move(mbo);
  return ZX_OK;
}
