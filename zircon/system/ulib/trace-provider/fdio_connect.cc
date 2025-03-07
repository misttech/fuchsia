// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// A helper library for connecting to the trace manager via fdio.

#include <lib/fdio/directory.h>
#include <lib/fdio/fd.h>
#include <lib/fdio/fdio.h>
#include <lib/trace-provider/fdio_connect.h>
#include <lib/zx/channel.h>

#include "export.h"

const char kServicePath[] = "/svc/fuchsia.tracing.provider.Registry";

EXPORT zx_status_t trace_provider_connect_with_fdio(zx_handle_t* out_client) {
  // NOTE: We clearly make this distinction (that this method uses fdio) as some tracing clients
  // (i.e. magma) want to use tracing, but also do not want to take a dependency on fdio.
  //
  // Most non magma clients are happy to take the fdio dependency in exchange for not needing to do
  // manual fidl channel handling so we also expose this helper method.
  //
  // We could use component::Connect here, but we'd take an additional dependency on the component
  // library.
  zx::channel registry_client, registry_service;
  zx_status_t status = zx::channel::create(0u, &registry_client, &registry_service);
  if (status != ZX_OK) {
    return status;
  }

  status = fdio_service_connect(kServicePath,
                                registry_service.release());  // takes ownership
  if (status != ZX_OK) {
    return status;
  }

  *out_client = registry_client.release();
  return ZX_OK;
}
