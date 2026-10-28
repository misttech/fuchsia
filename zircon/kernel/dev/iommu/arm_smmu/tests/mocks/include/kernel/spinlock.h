// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_TESTS_MOCKS_INCLUDE_KERNEL_SPINLOCK_H_
#define ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_TESTS_MOCKS_INCLUDE_KERNEL_SPINLOCK_H_

#include <lib/arch/cache.h>
#include <lib/zircon-internal/thread_annotations.h>
#include <stdarg.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <zircon/assert.h>
#include <zircon/compiler.h>
#include <zircon/types.h>

#include <algorithm>
#include <mutex>

#include <ktl/span.h>

#ifndef ASSERT
#define ASSERT(x) ZX_ASSERT(x)
#endif

#ifndef ASSERT_MSG
#define ASSERT_MSG(x, fmt, ...) ZX_ASSERT_MSG(x, fmt, ##__VA_ARGS__)
#endif

#ifndef DEBUG_ASSERT
#define DEBUG_ASSERT(x) ZX_DEBUG_ASSERT(x)
#endif

#ifndef DEBUG_ASSERT_MSG
#define DEBUG_ASSERT_MSG(x, fmt, ...) ZX_DEBUG_ASSERT_MSG(x, fmt, ##__VA_ARGS__)
#endif

#ifndef CRITICAL
#define CRITICAL 0
#endif

#ifndef ALWAYS
#define ALWAYS 0
#endif

#ifndef INFO
#define INFO 1
#endif

#ifndef SPEW
#define SPEW 2
#endif

#ifndef dprintf
#define dprintf(level, fmt, ...)  \
  do {                            \
    if (false) {                  \
      printf(fmt, ##__VA_ARGS__); \
    }                             \
  } while (0)
#endif

#ifndef LTRACEF
#define LTRACEF(fmt, ...)         \
  do {                            \
    if (false) {                  \
      printf(fmt, ##__VA_ARGS__); \
    }                             \
  } while (0)
#endif

#ifndef DRIVER_OOPS
#define DRIVER_OOPS(fmt, ...)     \
  do {                            \
    if (false) {                  \
      printf(fmt, ##__VA_ARGS__); \
    }                             \
  } while (0)
#endif

#ifndef ZX_USEC
#define ZX_USEC(n) ((zx_duration_t)(1000ULL * (n)))
#endif

struct Thread {
  struct Current {
    static void SleepRelative(zx_duration_t delay) {}
  };
};

class StringFile {
 public:
  explicit StringFile(ktl::span<char> buf) : buf_(buf) {
    if (!buf_.empty()) {
      buf_[0] = '\0';
    }
  }

  template <size_t N>
  explicit StringFile(char (&buf)[N]) : StringFile(ktl::span<char>(buf, N)) {}

  int WriteFormatted(const char* fmt, va_list ap) {
    if (buf_.empty() || offset_ >= buf_.size() - 1) {
      return 0;
    }
    int n = vsnprintf(buf_.data() + offset_, buf_.size() - offset_, fmt, ap);
    if (n > 0) {
      size_t written = static_cast<size_t>(n);
      offset_ = std::min(offset_ + written, buf_.size() - 1);
    }
    return n;
  }

  ktl::span<char> take() && { return buf_.subspan(0, offset_ + (buf_.empty() ? 0 : 1)); }

 private:
  ktl::span<char> buf_;
  size_t offset_ = 0;
};

__PRINTFLIKE(2, 3) inline int fprintf(StringFile* sfile, const char* fmt, ...) {
  va_list ap;
  va_start(ap, fmt);
  int ret = sfile->WriteFormatted(fmt, ap);
  va_end(ap);
  return ret;
}

class TA_CAP("mutex") Mutex {
 public:
  Mutex() = default;
  ~Mutex() = default;

  void Acquire() TA_ACQ() { lock_.lock(); }
  void Release() TA_REL() { lock_.unlock(); }
  void AssertHeld() const TA_ASSERT() {}

  Mutex& lock() { return *this; }
  const Mutex& lock() const { return *this; }

 private:
  std::mutex lock_;
};

class TA_CAP("mutex") SpinLock {
 public:
  SpinLock() = default;
  ~SpinLock() = default;

  void Acquire() TA_ACQ() { lock_.lock(); }
  void Release() TA_REL() { lock_.unlock(); }
  void AssertHeld() const TA_ASSERT() {}

  SpinLock& lock() { return *this; }
  const SpinLock& lock() const { return *this; }

 private:
  std::mutex lock_;
};

struct IrqSave {};
struct NoIrqSave {};

template <typename LockType, typename Option = void>
class TA_SCOPED_CAP Guard {
 public:
  explicit Guard(LockType* lock) TA_ACQ(lock) : lock_(lock) { lock_->Acquire(); }

  template <typename Tag>
  Guard(LockType* lock, Tag) TA_ACQ(lock) : lock_(lock) {
    lock_->Acquire();
  }

  ~Guard() TA_REL() {
    if (lock_) {
      lock_->Release();
    }
  }

  void Release() TA_REL() {
    if (lock_) {
      lock_->Release();
      lock_ = nullptr;
    }
  }

  template <typename F>
  void CallUnlocked(F&& func) TA_NO_THREAD_SAFETY_ANALYSIS {
    lock_->Release();
    func();
    lock_->Acquire();
  }

  Guard(const Guard&) = delete;
  Guard& operator=(const Guard&) = delete;

 private:
  LockType* lock_;
};

#define DECLARE_MUTEX(ContainingType) ::Mutex
#define DECLARE_SPINLOCK(ContainingType) ::SpinLock

#define DECLARE_SINGLETON_MUTEX(Name)                          \
  struct Name {                                                \
    static ::Mutex* Get() TA_RET_CAP(lock_) { return &lock_; } \
    static inline ::Mutex lock_;                               \
  }

#endif  // ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_TESTS_MOCKS_INCLUDE_KERNEL_SPINLOCK_H_
