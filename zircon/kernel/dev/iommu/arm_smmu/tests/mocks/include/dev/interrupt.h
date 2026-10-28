// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_TESTS_MOCKS_INCLUDE_DEV_INTERRUPT_H_
#define ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_TESTS_MOCKS_INCLUDE_DEV_INTERRUPT_H_

#include <stdint.h>
#include <sys/types.h>
#include <zircon/compiler.h>
#include <zircon/types.h>

#include <mutex>
#include <unordered_map>

using interrupt_vector_t = uint32_t;

struct interrupt_handler_t {
  void* cookie = nullptr;
  void (*fn)(void*) = nullptr;

  interrupt_handler_t() = default;
  interrupt_handler_t(decltype(nullptr)) : cookie(nullptr), fn(nullptr) {}
  interrupt_handler_t(void (*f)(void*)) : cookie(nullptr), fn(f) {}
  interrupt_handler_t(void* c, void (*f)(void*)) : cookie(c), fn(f) {}

  explicit operator bool() const { return fn != nullptr; }
};

enum class interrupt_trigger_mode : uint32_t {
  EDGE,
  LEVEL,
};

enum class interrupt_polarity : uint32_t {
  HIGH,
  LOW,
};

struct RegisteredIrqState {
  interrupt_trigger_mode trigger_mode = interrupt_trigger_mode::EDGE;
  interrupt_polarity polarity = interrupt_polarity::HIGH;
  bool configured = false;
  bool masked = true;
  interrupt_handler_t handler{};
};

class InterruptMock {
 public:
  static InterruptMock& Get() {
    static InterruptMock instance;
    return instance;
  }

  void Reset() {
    std::lock_guard<std::mutex> guard(lock_);
    irqs_.clear();
  }

  zx_status_t Configure(interrupt_vector_t vector, interrupt_trigger_mode tm,
                        interrupt_polarity pol) {
    std::lock_guard<std::mutex> guard(lock_);
    auto& state = irqs_[vector];
    state.trigger_mode = tm;
    state.polarity = pol;
    state.configured = true;
    return ZX_OK;
  }

  zx_status_t Mask(interrupt_vector_t vector) {
    std::lock_guard<std::mutex> guard(lock_);
    irqs_[vector].masked = true;
    return ZX_OK;
  }

  zx_status_t Unmask(interrupt_vector_t vector) {
    std::lock_guard<std::mutex> guard(lock_);
    irqs_[vector].masked = false;
    return ZX_OK;
  }

  zx_status_t RegisterHandler(interrupt_vector_t vector, interrupt_handler_t handler) {
    std::lock_guard<std::mutex> guard(lock_);
    irqs_[vector].handler = handler;
    return ZX_OK;
  }

  bool IsMasked(interrupt_vector_t vector) {
    std::lock_guard<std::mutex> guard(lock_);
    auto it = irqs_.find(vector);
    return (it == irqs_.end()) ? true : it->second.masked;
  }

  bool TriggerIrq(interrupt_vector_t vector) {
    interrupt_handler_t handler;
    {
      std::lock_guard<std::mutex> guard(lock_);
      auto it = irqs_.find(vector);
      if (it == irqs_.end() || !it->second.handler) {
        return false;
      }
      handler = it->second.handler;
    }
    handler.fn(handler.cookie);
    return true;
  }

 private:
  std::mutex lock_;
  std::unordered_map<interrupt_vector_t, RegisteredIrqState> irqs_;
};

inline zx_status_t configure_interrupt(interrupt_vector_t vector, interrupt_trigger_mode tm,
                                       interrupt_polarity pol) {
  return InterruptMock::Get().Configure(vector, tm, pol);
}

inline zx_status_t mask_interrupt(interrupt_vector_t vector) {
  return InterruptMock::Get().Mask(vector);
}

inline zx_status_t unmask_interrupt(interrupt_vector_t vector) {
  return InterruptMock::Get().Unmask(vector);
}

inline zx_status_t register_int_handler(interrupt_vector_t vector, interrupt_handler_t handler) {
  return InterruptMock::Get().RegisterHandler(vector, handler);
}

inline zx_status_t unregister_int_handler(interrupt_vector_t vector) {
  return register_int_handler(vector, nullptr);
}

#endif  // ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_TESTS_MOCKS_INCLUDE_DEV_INTERRUPT_H_
