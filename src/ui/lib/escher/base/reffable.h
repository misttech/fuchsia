// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_LIB_ESCHER_BASE_REFFABLE_H_
#define SRC_UI_LIB_ESCHER_BASE_REFFABLE_H_

#include <lib/syslog/cpp/macros.h>

#include <atomic>
#include <cstdint>

#include "src/lib/fxl/memory/ref_ptr.h"
#include "src/ui/lib/escher/base/make.h"

namespace escher {

// Reffable is a threadsafe ref-counted base class that is suitable for use with fxl::RefPtr.
// It provides a virtual OnZeroRefCount() method that subclasses can override to avoid immediate
// destruction when their ref-count becomes zero.
//
// Use this class similarly to fxl::RefCountedThreadSafe.  For example, instead
// of:
//    class Foo : public RefCountedThreadSafe<Foo> { ...
// simply say:
//    class Foo : public Reffable { ...
class Reffable {
 public:
  Reffable() = default;
  virtual ~Reffable();

  // Return the number of references to this object.
  uint64_t ref_count() const { return ref_count_.load(std::memory_order_relaxed); }

 protected:
  // Return true if the object should be destroyed immediately, or false if its
  // destruction should be deferred.  Subclass that override this method to
  // return false are responsible for ensuring that the object is eventually
  // destroyed.
  virtual bool OnZeroRefCount() { return true; }

 private:
  template <typename T>
  friend class fxl::RefPtr;

  // Called by fxl::RefPtr.
  void Release() {
    uint64_t prev = ref_count_.fetch_sub(1u, std::memory_order_release);
    FX_DCHECK(prev > 0u);
    if (prev == 1u) {
      std::atomic_thread_fence(std::memory_order_acquire);
      if (OnZeroRefCount()) {
        delete this;
      }
    }
  }

  // Called by fxl::RefPtr.
  void AddRef() const {
#ifndef NDEBUG
    FX_DCHECK(!adoption_required_);
#endif
    ref_count_.fetch_add(1u, std::memory_order_relaxed);
  }

  mutable std::atomic_uint64_t ref_count_ = 1u;

#ifndef NDEBUG
  // Called by fxl::RefPtr, but only in debug builds.
  template <typename U>
  friend fxl::RefPtr<U> fxl::AdoptRef(U*);
  void Adopt();
  bool adoption_required_ = true;
#endif

  FXL_DISALLOW_COPY_AND_ASSIGN(Reffable);
};

}  // namespace escher

#endif  // SRC_UI_LIB_ESCHER_BASE_REFFABLE_H_
