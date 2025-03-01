// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <dirent.h>
#include <fuchsia/io/cpp/fidl.h>
#include <lib/fdio/directory.h>
#include <lib/fit/defer.h>
#include <lib/sys/service/cpp/service_aggregate.h>

#include <vector>

namespace sys {

std::vector<std::string> ServiceAggregateBase::ListInstances() const {
  std::vector<std::string> instances;

  fidl::InterfaceHandle<fuchsia::unknown::Cloneable> handle;
#if FUCHSIA_API_LEVEL_AT_LEAST(26)
  zx_status_t status = dir_->Clone(handle.NewRequest());
#else
  zx_status_t status = dir_->Clone2(handle.NewRequest());
#endif
  if (status != ZX_OK) {
    return instances;
  }

  int fd;
  status = fdio_fd_create(handle.TakeChannel().release(), &fd);
  if (status != ZX_OK) {
    return instances;
  }
  auto defer_close = fit::defer([fd] { close(fd); });

  DIR* dir = fdopendir(fd);
  if (dir == nullptr) {
    return instances;
  }
  auto defer_closedir = fit::defer([dir] { closedir(dir); });

  for (dirent* entry; (entry = readdir(dir)) != nullptr;) {
    if (strcmp(entry->d_name, ".") != 0) {
      instances.emplace_back(entry->d_name);
    }
  }

  return instances;
}

fidl::InterfaceHandle<fuchsia::io::Directory> OpenNamedServiceAggregateAt(
    const fidl::InterfaceHandle<fuchsia::io::Directory>& handle, const std::string& service_path) {
  if (service_path.compare(0, 1, "/") == 0) {
    return nullptr;
  }

  fidl::InterfaceHandle<fuchsia::io::Directory> dir;
  zx_status_t status =
      fdio_open3_at(handle.channel().get(), service_path.data(),
                    uint64_t{fuchsia::io::PERM_READABLE}, dir.NewRequest().TakeChannel().release());
  if (status != ZX_OK) {
    return nullptr;
  }
  return dir;
}

fidl::InterfaceHandle<fuchsia::io::Directory> OpenNamedServiceAggregateIn(
    fdio_ns_t* ns, const std::string& service_path) {
  std::string path;
  if (service_path.compare(0, 1, "/") != 0) {
    path = "/svc/";
  }
  path += service_path;

  fidl::InterfaceHandle<fuchsia::io::Directory> dir;
  zx_status_t status = fdio_ns_open3(ns, path.data(), uint64_t{fuchsia::io::PERM_READABLE},
                                     dir.NewRequest().TakeChannel().release());
  if (status != ZX_OK) {
    return nullptr;
  }
  return dir;
}

fidl::InterfaceHandle<fuchsia::io::Directory> OpenNamedServiceAggregate(
    const std::string& service_path) {
  fdio_ns_t* ns;
  zx_status_t status = fdio_ns_get_installed(&ns);
  if (status != ZX_OK) {
    return nullptr;
  }
  return OpenNamedServiceAggregateIn(ns, service_path);
}

}  // namespace sys
