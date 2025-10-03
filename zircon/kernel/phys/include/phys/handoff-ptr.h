// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_PHYS_INCLUDE_PHYS_HANDOFF_PTR_H_
#define ZIRCON_KERNEL_PHYS_INCLUDE_PHYS_HANDOFF_PTR_H_

// Note: we refrain from using the ktl namespace as <phys/handoff.h> is
// expected to be compiled in the userboot toolchain.

#include <stddef.h>
#include <zircon/assert.h>

#include <span>
#include <string_view>
#include <utility>

struct PhysHandoff;
extern PhysHandoff* gPhysHandoff;

// PhysHandoffPtr provides a "smart pointer" style API for pointers handed off
// from physboot to the kernel proper.  A handoff pointer is only ever created
// in physboot by the HandoffPrep class.  It's only ever dereferenced (or
// converted into a raw pointer) in the kernel proper.  When converted to a
// pointer, it's only ever converted to a pointer to const.
//
// Lifetime issues for handoff data are complex.  PhysHandoffPtr is always
// treated as a traditional "owning" smart pointer and is a move-only type.
// Ordinarily, handoff pointer objects will be left in place and only have raw
// pointers extracted from them for later use.

// PhysHandoffPtr has no destructor and the "owning" pointer dying doesn't have
// any direct effect.  The lifetime of all handoff pointers is actually grouped
// in three buckets:
//
//  * Permanent handoff data will be accessible in the kernel's virtual address
//    space permanently.  This data resides on pages that the PMM has been told
//    are owned by kernel mappings.
//
//  * Pointers into the kernel's own load image.  From the kernel's perspective
//    these are just more permanent pointers.  However, in physboot they are
//    created by translating from existing locations in the kernel ELF file
//    that was mapped at a virtual address, whereas permanent handoff data is
//    in physical pages specifically allocated for that purpose by physboot.
//
//  * Temporary handoff data must be consumed only during the handoff phase,
//    which ends once EndHandoff() is called.  This data resides on pages that
//    the PMM may be told to reuse after handoff.
//
enum class PhysHandoffPtrLifetime { kPermanent, kKernelImage, kTemporary };

// Forward declaration; see below.
template <typename T, PhysHandoffPtrLifetime Lifetime>
class PhysHandoffSpan;

template <typename T, PhysHandoffPtrLifetime Lifetime>
class PhysHandoffPtr {
 public:
  // Handoff "heap" pointers are always pointers to const.  But a pointer into
  // the kernel image can have any kind of qualifiers or lack thereof.
  using value_type =
      std::conditional_t<Lifetime == PhysHandoffPtrLifetime::kKernelImage, T, const T>;

  // Handoff pointers can only be dereferenced in the kernel proper.
  static constexpr bool kCanDeref =
#ifdef HANDOFF_PTR_DEREF
      HANDOFF_PTR_DEREF
#elif defined(_KERNEL)
      true
#else
      false
#endif
      ;

  // Default-constructible, movable but not copyable (use .get() instead).
  constexpr PhysHandoffPtr() = default;
  constexpr PhysHandoffPtr(const PhysHandoffPtr&) = delete;
  constexpr PhysHandoffPtr(PhysHandoffPtr&& other) noexcept : ptr_(std::exchange(other.ptr_, {})) {}

  // In the kernel proper, pointers that are definitely into the image itself
  // can be initialized as constinit.
  consteval explicit PhysHandoffPtr(T& ref)
    requires(kCanDeref && Lifetime == PhysHandoffPtrLifetime::kKernelImage)
      : ptr_{&ref} {}

  constexpr PhysHandoffPtr& operator=(PhysHandoffPtr&& other) noexcept {
    ptr_ = std::exchange(other.ptr_, {});
    return *this;
  }

  constexpr auto operator<=>(const PhysHandoffPtr& other) const = default;

  explicit constexpr operator bool() const { return ptr_; }

  constexpr const T* get() const
    requires(kCanDeref)
  {
    if constexpr (Lifetime == PhysHandoffPtrLifetime::kTemporary) {
      ZX_DEBUG_ASSERT_MSG(gPhysHandoff,
                          "Pointer no longer valid; phys hand-off has already ended!");
    }
    return ptr_;
  }

  // This is allowed for debugging purposes in physboot.
  constexpr const T* force_get() const { return ptr_; }

  const T* release()
    requires(kCanDeref)
  {
    return std::exchange(ptr_, {});
  }

  const T& operator*() const
    requires(kCanDeref)
  {
    return *get();
  }

  const T* operator->() const
    requires(kCanDeref)
  {
    return get();
  }

  // The equivalent of reinterpret_cast can be done even when !kCanDeref.

  uintptr_t address() const {  // as if reinterpret_cast<uintptr_t>(get())
    return reinterpret_cast<uintptr_t>(ptr_);
  }

  template <typename Other>  // as if reinterpret_cast<Other*>(get())
    requires(std::is_const_v<Other> == std::is_const_v<T>)
  PhysHandoffPtr<Other, Lifetime> Reinterpret() const {
    using OtherPtr = PhysHandoffPtr<Other, Lifetime>;
    PhysHandoffPtr<Other, Lifetime> other;
    other.ptr_ = reinterpret_cast<OtherPtr::value_type*>(ptr_);
    return other;
  }

 private:
  friend class HandoffPrep;
  friend class PhysHandoffSpan<T, Lifetime>;

  value_type* ptr_ = nullptr;
};

// PhysHandoffSpan<T> is to std::span<const T> as PhysHandoffPtr<T> is to const
// T*.  It has get() and release() methods that return std::span<const T>.

template <typename T, PhysHandoffPtrLifetime Lifetime>
class PhysHandoffSpan {
 public:
  using Ptr = PhysHandoffPtr<T, Lifetime>;
  using value_type = const T;

  constexpr PhysHandoffSpan() = default;
  PhysHandoffSpan(const PhysHandoffSpan&) = delete;
  constexpr PhysHandoffSpan(PhysHandoffSpan&&) noexcept = default;

  constexpr PhysHandoffSpan(Ptr ptr, size_t size) : ptr_(std::move(ptr)), size_(size) {}

  constexpr PhysHandoffSpan& operator=(PhysHandoffSpan&&) noexcept = default;

  constexpr auto operator<=>(const PhysHandoffSpan& other) const = default;

  constexpr size_t size() const { return size_; }

  constexpr bool empty() const { return size() == 0; }

  constexpr std::span<value_type> get() const
    requires(Ptr::kCanDeref)
  {
    return force_get();
  }

  // This is allowed for debugging purposes in physboot.
  constexpr std::span<value_type> force_get() const { return {ptr_.force_get(), size_}; }

  constexpr std::span<value_type> release()
    requires(Ptr::kCanDeref)
  {
    return {ptr_.release(), size_};
  }

  PhysHandoffSpan subspan(size_t offset, size_t count) const {
    PhysHandoffSpan result;
    assert(offset <= size_);
    result.ptr_.ptr_ = ptr_.ptr_ + offset;
    result.size_ = count;
    return result;
  }

 private:
  friend class HandoffPrep;

  Ptr ptr_;
  size_t size_ = 0;
};

// PhysHandoffString is stored just the same as PhysHandoffSpan<const char>,
// but its get() and release() methods yield std::string_view.
template <PhysHandoffPtrLifetime Lifetime>
class PhysHandoffString : public PhysHandoffSpan<const char, Lifetime> {
 public:
  using Base = PhysHandoffSpan<const char, Lifetime>;

  constexpr PhysHandoffString() = default;
  constexpr PhysHandoffString(PhysHandoffString&&) noexcept = default;
  constexpr PhysHandoffString& operator=(PhysHandoffString&&) noexcept = default;

  PhysHandoffString substr(size_t offset, size_t count) const {
    PhysHandoffString result;
    result.Base::operator=(Base::subspan(offset, count));
    return result;
  }

  constexpr std::string_view get() const
    requires(Base::Ptr::kCanDeref)
  {
    std::span str = Base::get();
    return {str.data(), str.size()};
  }

  constexpr std::string_view release()
    requires(Base::Ptr::kCanDeref)
  {
    std::span str = Base::release();
    return {str.data(), str.size()};
  }
};

// Convenience aliases used in the PhysHandoff declaration.

template <typename T>
using PhysHandoffTemporaryPtr = PhysHandoffPtr<T, PhysHandoffPtrLifetime::kTemporary>;

template <typename T>
using PhysHandoffTemporarySpan = PhysHandoffSpan<T, PhysHandoffPtrLifetime::kTemporary>;

using PhysHandoffTemporaryString = PhysHandoffString<PhysHandoffPtrLifetime::kTemporary>;

template <typename T>
using PhysHandoffPermanentPtr = PhysHandoffPtr<T, PhysHandoffPtrLifetime::kPermanent>;

template <typename T>
using PhysHandoffPermanentSpan = PhysHandoffSpan<T, PhysHandoffPtrLifetime::kPermanent>;

using PhysHandoffPermanentString = PhysHandoffString<PhysHandoffPtrLifetime::kPermanent>;

template <typename T>
using PhysHandoffKernelImagePtr = PhysHandoffPtr<T, PhysHandoffPtrLifetime::kKernelImage>;

template <typename T>
using PhysHandoffKernelImageSpan = PhysHandoffSpan<T, PhysHandoffPtrLifetime::kKernelImage>;

using PhysHandoffKernelImageString = PhysHandoffString<PhysHandoffPtrLifetime::kKernelImage>;

#endif  // ZIRCON_KERNEL_PHYS_INCLUDE_PHYS_HANDOFF_PTR_H_
