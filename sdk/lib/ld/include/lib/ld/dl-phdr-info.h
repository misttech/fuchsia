// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_LD_DL_PHDR_INFO_H_
#define LIB_LD_DL_PHDR_INFO_H_

#include <link.h>

#include "module.h"

namespace ld {

template <class Elf, class AbiTraits>
constexpr dl_phdr_info MakeDlPhdrInfo(const abi::Abi<Elf, AbiTraits>& abi,
                                      const typename abi::Abi<Elf, AbiTraits>::Module& module,
                                      void* tls_data, uint64_t adds = 0, uint64_t subs = 0) {
  return {
      .dlpi_addr = module.link_map.addr,
      .dlpi_name = module.link_map.name,
      .dlpi_phdr = module.phdrs.data(),
      .dlpi_phnum = static_cast<uint16_t>(module.phdrs.size()),
      .dlpi_adds = adds,
      .dlpi_subs = subs,
      .dlpi_tls_modid = module.tls_modid,
      .dlpi_tls_data = tls_data,
  };
}

}  // namespace ld

#endif  // LIB_LD_DL_PHDR_INFO_H_
