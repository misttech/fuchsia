// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_LD_INIT_FINI_H_
#define LIB_LD_INIT_FINI_H_

#include <lib/elfldltl/init-fini.h>
#include <lib/elfldltl/link-map-list.h>
#include <lib/elfldltl/memory.h>
#include <lib/ld/abi.h>
#include <lib/ld/module.h>

#include <algorithm>
#include <climits>
#include <cstdint>

namespace ld {

using AbiModuleList =
    elfldltl::LinkMapList<elfldltl::DirectMemory, elfldltl::Elf<>, elfldltl::LocalAbiTraits,
                          abi::Abi<>::Module, &abi::Abi<>::Module::link_map>;

inline elfldltl::DirectMemory gLocalMemory{
    {reinterpret_cast<std::byte*>(0), SIZE_MAX},
    0,
};

inline AbiModuleList AbiModules(const abi::Abi<>& abi = abi::_ld_abi) {
  return AbiModuleList{gLocalMemory, abi.loaded_modules.address()};
}

inline void InitModule(const abi::Abi<>::Module& module) {
  module.init.CallInit(module.link_map.addr);
}

inline void FiniModule(const abi::Abi<>::Module& module) {
  module.fini.CallFini(module.link_map.addr);
}

inline void InitAbiModules() {
  AbiModuleList modules = AbiModules();
  std::for_each(modules.begin(), modules.end(), InitModule);
}

inline void FiniAbiModules() {
  AbiModuleList modules = AbiModules();
  std::for_each(modules.rbegin(), modules.rend(), FiniModule);
}

}  // namespace ld

#endif  // LIB_LD_INIT_FINI_H_
