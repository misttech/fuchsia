// Copyright 2024 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_LIB_POWER_MANAGEMENT_INCLUDE_LIB_POWER_MANAGEMENT_KERNEL_REGISTRY_H_
#define ZIRCON_KERNEL_LIB_POWER_MANAGEMENT_INCLUDE_LIB_POWER_MANAGEMENT_KERNEL_REGISTRY_H_

#include <lib/power-management/energy-model.h>
#include <lib/zircon-internal/thread_annotations.h>

#include <fbl/ref_ptr.h>
#include <kernel/mutex.h>

namespace power_management {

// Manages a singleton instance of PowerDomainRegistry and the plumbing
// necessary for the kernel environment.
class KernelPowerDomainRegistry {
  DECLARE_SINGLETON_MUTEX(Lock);

 public:
  // Registers the given power domain, replacing the power domain with the same
  // domain id if necessary.
  static zx::result<> Register(const fbl::RefPtr<PowerDomain>& domain) TA_EXCL(Lock::Get()) {
    Guard<Mutex> guard(Lock::Get());
    return registry_.Register(domain);
  }

  // Unregisters the power domain with the given domain id.
  static zx::result<> Unregister(uint32_t domain_id) TA_EXCL(Lock::Get()) {
    Guard<Mutex> guard(Lock::Get());
    return registry_.Unregister(domain_id);
  }

  // Updates the power level for the given domain.
  static zx::result<> UpdatePowerLevel(uint32_t domain_id, uint64_t controller_id,
                                       power_management::ControlInterface interface, uint64_t arg)
      TA_EXCL(Lock::Get());

  template <typename V>
  static void Visit(V&& v) {
    Guard<Mutex> guard(Lock::Get());
    registry_.Visit(ktl::forward<V>(v));
  }

 private:
  static void UpdateAllCpuPowerDomainSets(const PowerDomainSet& power_domain_set);

  TA_GUARDED(Lock::Get())
  inline static PowerDomainRegistry registry_{UpdateAllCpuPowerDomainSets};
};

}  // namespace power_management

#endif  // ZIRCON_KERNEL_LIB_POWER_MANAGEMENT_INCLUDE_LIB_POWER_MANAGEMENT_KERNEL_REGISTRY_H_
