// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_LIB_ELFLDLTL_INCLUDE_LIB_ELFLDLTL_RESOLVE_H_
#define SRC_LIB_ELFLDLTL_INCLUDE_LIB_ELFLDLTL_RESOLVE_H_

#include <memory>
#include <type_traits>
#include <utility>

#include "diagnostics.h"
#include "link.h"
#include "symbol.h"

namespace elfldltl {

// This type implements a Definition which can be used as the return type for
// the `resolve` parameter for RelocateSymbolic. See link.h for more details.
// The Module type must have the following methods:
//
//  * const SymbolInfo& symbol_info() const
//    Returns the SymbolInfo type associated with this module. This is used
//    to call SymbolInfo::Lookup().
//
//  * size_type load_bias() const
//    Returns the load bias for symbol addresses in this module.
//
//  * size_type tls_module_id() const
//    Returns the TLS module ID number for this module.
//    This will be zero for a module with no PT_TLS segment.
//    It's always one in the main executable if has a PT_TLS segment,
//    but may be one in a different module if the main executable has none.
//
//  * bool uses_static_tls() const
//    This module may have TLS relocations for IE or LE model accesses.
//
//  * size_type static_tls_bias() const
//    Returns the static TLS layout bias for the defining module.
//
//  * size_type tls_desc_hook(const Sym&), tls_desc_value(const Sym&) const
//    Returns the two values for the TLSDESC resolution.
//
template <class Module>
struct ResolverDefinition {
  using Sym = typename std::decay_t<decltype(std::declval<Module>().symbol_info())>::Sym;

  // TODO(fxbug.dev/120388): preferably, this would just be a constexpr static variable
  // but clang can't compile that.
  static constexpr ResolverDefinition UndefinedWeak() {
    static_assert(ResolverDefinition{}.undefined_weak());
    return {};
  }

  // This should be called before any other method to check if this Definition is valid.
  constexpr bool undefined_weak() const { return !symbol_; }

  constexpr const Sym& symbol() const { return *symbol_; }
  constexpr auto bias() const { return module_->load_bias(); }

  constexpr auto tls_module_id() const { return module_->tls_module_id(); }
  constexpr auto static_tls_bias() const { return module_->static_tls_bias(); }
  constexpr auto tls_desc_hook() const { return module_->tls_desc_hook(*symbol_); }
  constexpr auto tls_desc_value() const { return module_->tls_desc_value(*symbol_); }

  const Sym* symbol_ = nullptr;
  const Module* module_ = nullptr;
};

// Returns a callable object which can be used for RelocateSymbolic's `resolve`
// argument. This takes a SymbolInfo object which is used for finding the name
// of the symbol given by RelocateSymbolic. The `modules` argument is a list of
// modules from where symbolic definitions can be resolved, this list is in
// order of precedence. The ModuleList type is a forward iterable range or
// container. diag is a diagnostics object for reporting errors. All references
// passed to MakeSymbolResolver should outlive the returned object.
template <class SymbolInfo, class ModuleList, class Diagnostics>
constexpr auto MakeSymbolResolver(const SymbolInfo& ref_info, const ModuleList& modules,
                                  Diagnostics& diag) {
  using Module = std::decay_t<decltype(*std::declval<ModuleList>().begin())>;
  using Definition = ResolverDefinition<Module>;

  return [&](const auto& ref, elfldltl::RelocateTls tls_type) -> std::optional<Definition> {
    elfldltl::SymbolName name{ref_info, ref};

    if (name.empty()) [[unlikely]] {
      diag.FormatError("Symbol had invalid st_name");
      return std::nullopt;
    }

    for (const auto& module : modules) {
      if (const auto* sym = name.Lookup(module.symbol_info())) {
        switch (tls_type) {
          case RelocateTls::kNone:
            if (sym->type() == ElfSymType::kTls) [[unlikely]] {
              diag.FormatError("non-TLS relocation resolves to STT_TLS symbol ", name);
              return std::nullopt;
            }
            break;
          case RelocateTls::kStatic:
            if (!module.uses_static_tls()) [[unlikely]] {
              diag.FormatError(
                  "TLS Initial Exec relocation resolves to STT_TLS symbol in module without DF_STATIC_TLS: ",
                  name);
              return std::nullopt;
            }
            [[fallthrough]];
          case RelocateTls::kDynamic:
          case RelocateTls::kDesc:
            if (sym->type() != ElfSymType::kTls) [[unlikely]] {
              diag.FormatError("TLS relocation resolves to non-STT_TLS symbol: ", name);
              return std::nullopt;
            }
            break;
        }
        return Definition{sym, std::addressof(module)};
      }
    }

    if (ref.bind() == ElfSymBind::kWeak) {
      return Definition::UndefinedWeak();
    }

    diag.UndefinedSymbol(name);
    return std::nullopt;
  };
}

}  // namespace elfldltl

#endif  // SRC_LIB_ELFLDLTL_INCLUDE_LIB_ELFLDLTL_RESOLVE_H_
