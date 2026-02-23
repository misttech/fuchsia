// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_LIB_PAGE_MAP_INCLUDE_LIB_PAGE_MAP_ACCESSOR_H_
#define ZIRCON_KERNEL_LIB_PAGE_MAP_INCLUDE_LIB_PAGE_MAP_ACCESSOR_H_

#include <lib/fit/defer.h>
#include <lib/user_copy/internal.h>
#include <lib/zx/result.h>
#include <zircon/errors.h>

#include <fbl/ref_ptr.h>
#include <ktl/utility.h>

namespace page_map::internal {
class Entry;
// Release an Entry from an Accessor.  See definition for why this is not an Accessor method.
void ReleaseEntry(Entry* entry);
}  // namespace page_map::internal

namespace page_map {

class PageMap;

// Provides access to a single object of type |Object|.
//
// Use |PageMap::MakeAccessor| to create an Accessor.
//
// Instances of Accessor are not safe for concurrent use (i.e. not thread-safe).
template <typename Object>
class Accessor {
 public:
  static_assert(::internal::is_copy_allowed<Object>::value, "Type must be ABI-safe.");

  // Construct a invalid Accessor.  See |PageMap::MakeAccessor|
  Accessor() = default;

  // Because a mapping may be destroyed upon the destruction of the last Accessor that references
  // it, this may only be called from thread context, where it's safe to acquire VM locks.
  ~Accessor();

  // Returns true if this instance is valid.
  //
  // Invalid instances are akin to null pointers.  Only valid instances may be read/written.
  bool IsValid() const { return entry_ != nullptr; }

  // Accessor is a move-only type.
  //
  // The source of a move operation will be invalidated.
  Accessor(Accessor&& other);
  Accessor& operator=(Accessor&& other);
  Accessor(const Accessor&) = delete;
  Accessor& operator=(const Accessor&) = delete;

  // Read (copy) the object from the VMO into |dst|.
  //
  // It is an error to call this on an invalid instance.
  void Read(Object& dst) const;

  // Write (copy) |src| to the object.
  //
  // It is an error to call this on an invalid instance.
  void Write(const Object& src);

  // Like Read, but for reading only one field of an object.
  //
  // FieldType is a pointer-to-member.  E.g.
  //
  // struct Aggregate { int field1; int field2; };
  // Accessor a = ...;
  // int f1;
  // a.Read<&Aggregate::field1>(f1);
  //
  template <auto Field, typename FieldType = decltype(Field)>
    requires ktl::is_class_v<Object>
  void Read(FieldType& dst_field) const;

  // Like Write, but for writing only one field of an object.
  //
  // FieldType is a pointer-to-member.  E.g.
  //
  // struct Aggregate { int field1; int field2; };
  // Accessor a = ...;
  // int f1 = 42;
  // a.Write<&Aggregate::field1>(f1);
  //
  template <auto Field, typename FieldType = decltype(Field)>
    requires ktl::is_class_v<Object>
  void Write(const FieldType& src_field);

 private:
  // So |PageMap::MakeAccessor| can call the private constructor.
  friend class PageMap;

  // See below.
  template <auto field>
  struct FieldRef;

  // FieldRef provides a reference to the field of a struct.  E.g.
  //
  //   struct Aggregate { int field1; int field2; } agg;
  //   FieldRef<&Aggregate::field1>::Of(agg) = 42;
  //
  template <typename Struct, typename Field, Field Struct::* field>
  struct FieldRef<field> {
    static_assert(::internal::is_copy_allowed<Field>::value, "Type must be ABI-safe.");
    static constexpr Field& Of(Struct& instance) { return (instance.*field); }
    static constexpr const Field& Of(const Struct& instance) { return (instance.*field); }
  };

  // Construct an Accessor for the existing |entry|.
  //
  // |page_map| and |entry| must outlive this Accessor.
  Accessor(internal::Entry* entry, Object* object) : entry_{entry}, object_{object} {}

  // Invalidate this accessor and release any mapping it may retain.
  //
  // After |Invalidate|, |IsValid| will return false;
  void Invalidate() {
    if (entry_) {
      ReleaseEntry(entry_);
      entry_ = nullptr;
      object_ = nullptr;
    }
  }

  // A compiler barrier used to prevent copy elision induced TOCTOU.
  //
  // This is intended for use *after* copying to |*dst|, but *before* validating the copy.  It is
  // designed to prevent TOCTOU in the case where the compiler might otherwise elide the copy.
  //
  // By telling the compiler that this volatile assembly statement uses the object-in-memory pointed
  // to by dst as both output and input, we ensure the compiler:
  //   - cannot optimize-away a preceding copy to |*dst|
  //   - cannot reorder a validation of |*dst| occurring after the barrier with a use of |*dst|
  //     preceding the barrier.
  template <typename T>
  static void BarrierAfterCopy(T* dst) {
    __asm__ volatile("" : "+m"(*dst));
  }

  // Many Accessor instances may refer to a single Entry.  Entry must outlive is Accessors.
  //
  // May be null if this object was used as the source of a move operation (see move constructor).
  internal::Entry* entry_{};
  // May be null if this object was used as the source of a move operation (see move constructor).
  Object* object_{};
};

template <typename Object>
inline Accessor<Object>::~Accessor() {
  Invalidate();
}

template <typename Object>
inline Accessor<Object>::Accessor(Accessor&& other)
    : entry_(ktl::exchange(other.entry_, nullptr)),
      object_(ktl::exchange(other.object_, nullptr)) {}

template <typename Object>
inline Accessor<Object>& Accessor<Object>::operator=(Accessor&& other) {
  if (this != &other) {
    Invalidate();
    ktl::swap(entry_, other.entry_);
    ktl::swap(object_, other.object_);
  }
  return *this;
}

template <typename Object>
inline void Accessor<Object>::Read(Object& dst) const {
  dst = *object_;
  BarrierAfterCopy(&dst);
}

template <typename Object>
template <auto Field>
  requires ktl::is_class_v<Object>
inline void Accessor<Object>::Read(auto& dst) const {
  dst = FieldRef<Field>::Of(*object_);
  BarrierAfterCopy(&dst);
}

template <typename Object>
inline void Accessor<Object>::Write(const Object& src) {
  *object_ = src;
  BarrierAfterCopy(object_);
}

template <typename Object>
template <auto Field, typename FieldType>
  requires ktl::is_class_v<Object>
inline void Accessor<Object>::Write(const FieldType& src) {
  FieldRef<Field>::Of(*object_) = src;
  BarrierAfterCopy(object_);
}

}  // namespace page_map

#endif  // ZIRCON_KERNEL_LIB_PAGE_MAP_INCLUDE_LIB_PAGE_MAP_ACCESSOR_H_
