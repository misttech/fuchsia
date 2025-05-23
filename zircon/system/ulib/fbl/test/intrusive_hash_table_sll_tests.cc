// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fbl/intrusive_hash_table.h>
#include <fbl/intrusive_single_list.h>
#include <fbl/tests/intrusive_containers/associative_container_test_environment.h>
#include <fbl/tests/intrusive_containers/intrusive_hash_table_checker.h>
#include <fbl/tests/intrusive_containers/test_thunks.h>
#include <zxtest/zxtest.h>

namespace fbl {
namespace tests {
namespace intrusive_containers {

using OtherKeyType = uint16_t;
using OtherHashType = uint32_t;
static constexpr OtherHashType kOtherNumBuckets = 23;

template <typename PtrType>
struct OtherHashTraits {
  using ObjType = typename ::fbl::internal::ContainerPtrTraits<PtrType>::ValueType;
  using BucketStateType = SinglyLinkedListNodeState<PtrType>;

  // Linked List Traits
  static BucketStateType& node_state(ObjType& obj) {
    return obj.other_container_state_.bucket_state_;
  }

  // Keyed Object Traits
  static OtherKeyType GetKey(const ObjType& obj) { return obj.other_container_state_.key_; }

  static bool LessThan(const OtherKeyType& key1, const OtherKeyType& key2) { return key1 < key2; }

  static bool EqualTo(const OtherKeyType& key1, const OtherKeyType& key2) { return key1 == key2; }

  // Hash Traits
  static OtherHashType GetHash(const OtherKeyType& key) {
    return static_cast<OtherHashType>((key * 0xaee58187) % kOtherNumBuckets);
  }

  // Set key is a trait which is only used by the tests, not by the containers
  // themselves.
  static void SetKey(ObjType& obj, OtherKeyType key) { obj.other_container_state_.key_ = key; }
};

template <typename PtrType>
struct OtherHashState {
 private:
  friend struct OtherHashTraits<PtrType>;
  OtherKeyType key_;
  typename OtherHashTraits<PtrType>::BucketStateType bucket_state_;
};

template <typename PtrType, NodeOptions kNodeOptions = NodeOptions::None>
class HTSLLTraits {
 public:
  using ObjType = typename ::fbl::internal::ContainerPtrTraits<PtrType>::ValueType;

  // clang-format off
  using ContainerType           = HashTable<size_t, PtrType>;
  using ContainableBaseClass    = SinglyLinkedListable<PtrType, kNodeOptions>;
  using ContainerStateType      = SinglyLinkedListNodeState<PtrType, kNodeOptions>;
  using KeyType                 = typename ContainerType::KeyType;
  using HashType                = typename ContainerType::HashType;

  using OtherContainerTraits    = OtherHashTraits<PtrType>;
  using OtherContainerStateType = OtherHashState<PtrType>;
  using OtherBucketType         = SinglyLinkedListCustomTraits<PtrType, OtherContainerTraits>;
  using OtherContainerType      = HashTable<OtherKeyType,
                                            PtrType,
                                            OtherBucketType,
                                            OtherHashType,
                                            kOtherNumBuckets,
                                            OtherContainerTraits,
                                            OtherContainerTraits>;

  using TestObjBaseType =
      HashedTestObjBase<typename ContainerType::KeyType, typename ContainerType::HashType>;
  // clang-format on

  struct Tag1 {};
  struct Tag2 {};
  struct Tag3 {};

  using TaggedContainableBaseClasses =
      fbl::ContainableBaseClasses<TaggedSinglyLinkedListable<PtrType, Tag1>,
                                  TaggedSinglyLinkedListable<PtrType, Tag2>,
                                  TaggedSinglyLinkedListable<PtrType, Tag3>>;

  using TaggedType1 = TaggedHashTable<size_t, PtrType, Tag1>;
  using TaggedType2 = TaggedHashTable<size_t, PtrType, Tag2>;
  using TaggedType3 = TaggedHashTable<size_t, PtrType, Tag3>;
};

template <typename PtrType, NodeOptions kNodeOptions = NodeOptions::None>
class DHTSLLTraits {
 public:
  template <typename DynamicHashTableType, size_t BucketCount>
  class DynamicHashTableWrapper : public DynamicHashTableType {
   public:
    using BucketType = typename DynamicHashTableType::BucketType;
    DynamicHashTableWrapper()
        : DynamicHashTableType{std::unique_ptr<BucketType[]>(new BucketType[BucketCount]),
                               BucketCount} {}
    ~DynamicHashTableWrapper() = default;
  };

  using ObjType = typename ::fbl::internal::ContainerPtrTraits<PtrType>::ValueType;

  // clang-format off
  using ContainerType           = DynamicHashTableWrapper<
                                    HashTable<size_t,
                                              PtrType,
                                              SinglyLinkedList<PtrType>,
                                              size_t,
                                              kDynamicBucketCount>,
                                    37>;
  using ContainableBaseClass    = SinglyLinkedListable<PtrType, kNodeOptions>;
  using ContainerStateType      = SinglyLinkedListNodeState<PtrType, kNodeOptions>;
  using KeyType                 = typename ContainerType::KeyType;
  using HashType                = typename ContainerType::HashType;

  using OtherContainerTraits    = OtherHashTraits<PtrType>;
  using OtherContainerStateType = OtherHashState<PtrType>;
  using OtherBucketType         = SinglyLinkedListCustomTraits<PtrType, OtherContainerTraits>;
  using OtherContainerType      = DynamicHashTableWrapper<
                                    HashTable<OtherKeyType,
                                              PtrType,
                                              OtherBucketType,
                                              OtherHashType,
                                              kDynamicBucketCount,
                                              OtherContainerTraits,
                                              OtherContainerTraits>,
                                    kOtherNumBuckets>;

  using TestObjBaseType =
      HashedTestObjBase<typename ContainerType::KeyType, typename ContainerType::HashType>;
  // clang-format on

  struct Tag1 {};
  struct Tag2 {};
  struct Tag3 {};

  using TaggedContainableBaseClasses =
      fbl::ContainableBaseClasses<TaggedSinglyLinkedListable<PtrType, Tag1>,
                                  TaggedSinglyLinkedListable<PtrType, Tag2>,
                                  TaggedSinglyLinkedListable<PtrType, Tag3>>;

  using TaggedType1 = TaggedHashTable<size_t, PtrType, Tag1>;
  using TaggedType2 = TaggedHashTable<size_t, PtrType, Tag2>;
  using TaggedType3 = TaggedHashTable<size_t, PtrType, Tag3>;
};

// Traits for a HashTable with a dynamic number of SinglyLinkedList buckets,
// defined at after construction time but before use (eg; DelayedInit).
template <typename PtrType, NodeOptions kNodeOptions = NodeOptions::None>
class DIDHTSLLTraits {
 public:
  template <typename DynamicHashTableType, size_t BucketCount>
  class DynamicHashTableWrapper : public DynamicHashTableType {
   public:
    using BucketType = typename DynamicHashTableType::BucketType;
    DynamicHashTableWrapper() : DynamicHashTableType{HashTableOption::DelayedInit} {
      this->Init(std::unique_ptr<BucketType[]>(new BucketType[BucketCount]), BucketCount);
    }
    ~DynamicHashTableWrapper() = default;
  };

  // clang-format off
  using ContainerType           = DynamicHashTableWrapper<
                                    HashTable<size_t,
                                              PtrType,
                                              SinglyLinkedList<PtrType>,
                                              size_t,
                                              kDynamicBucketCount>,
                                    37>;
  using ContainableBaseClass    = SinglyLinkedListable<PtrType, kNodeOptions>;
  using ContainerStateType      = SinglyLinkedListNodeState<PtrType, kNodeOptions>;
  using KeyType                 = typename ContainerType::KeyType;
  using HashType                = typename ContainerType::HashType;

  using OtherContainerTraits    = OtherHashTraits<PtrType>;
  using OtherContainerStateType = OtherHashState<PtrType>;
  using OtherBucketType         = SinglyLinkedListCustomTraits<PtrType, OtherContainerTraits>;
  using OtherContainerType      = DynamicHashTableWrapper<
                                    HashTable<OtherKeyType,
                                              PtrType,
                                              OtherBucketType,
                                              OtherHashType,
                                              kDynamicBucketCount,
                                              OtherContainerTraits,
                                              OtherContainerTraits>,
                                    kOtherNumBuckets>;

  using TestObjBaseType =
      HashedTestObjBase<typename ContainerType::KeyType, typename ContainerType::HashType>;
  // clang-format on

  struct Tag1 {};
  struct Tag2 {};
  struct Tag3 {};

  using TaggedContainableBaseClasses =
      fbl::ContainableBaseClasses<TaggedSinglyLinkedListable<PtrType, Tag1>,
                                  TaggedSinglyLinkedListable<PtrType, Tag2>,
                                  TaggedSinglyLinkedListable<PtrType, Tag3>>;

  using TaggedType1 = TaggedHashTable<size_t, PtrType, Tag1>;
  using TaggedType2 = TaggedHashTable<size_t, PtrType, Tag2>;
  using TaggedType3 = TaggedHashTable<size_t, PtrType, Tag3>;
};

// Negative compilation test which make sure that we cannot try to use a node
// flagged with AllowRemoveFromContainer with a hashtable with singly linked
// list buckets.
TEST(SinglyLinkedHashTableTest, NoRemoveFromContainer) {
  struct Obj : public SinglyLinkedListable<Obj*, NodeOptions::AllowRemoveFromContainer> {
    uintptr_t GetKey() const { return reinterpret_cast<uintptr_t>(this); }
  };
#if TEST_WILL_NOT_COMPILE || 0
  [[maybe_unused]] fbl::HashTable<uintptr_t, Obj*, fbl::SinglyLinkedList<Obj*>> hashtable;
#endif
}

// Small helper which will generate tests for both the static and dynamic
// versions of the HashTable
#define RUN_HT_ZXTEST(_group, _flavor, _test) \
  RUN_ZXTEST(_group, _flavor, _test)          \
  RUN_ZXTEST(_group, D##_flavor, _test)       \
  RUN_ZXTEST(_group, DID##_flavor, _test)

// clang-format off
// Statically sized hashtable
DEFINE_TEST_OBJECTS(HTSLL);
using UMTE   = DEFINE_TEST_THUNK(Associative, HTSLL, Unmanaged);
using UPDDTE = DEFINE_TEST_THUNK(Associative, HTSLL, UniquePtrDefaultDeleter);
using UPCDTE = DEFINE_TEST_THUNK(Associative, HTSLL, UniquePtrCustomDeleter);
using RPTE   = DEFINE_TEST_THUNK(Associative, HTSLL, RefPtr);

// Dynamically sized hashtable
DEFINE_TEST_OBJECTS(DHTSLL);
using DUMTE   = DEFINE_TEST_THUNK(Associative, DHTSLL, Unmanaged);
using DUPDDTE = DEFINE_TEST_THUNK(Associative, DHTSLL, UniquePtrDefaultDeleter);
using DUPCDTE = DEFINE_TEST_THUNK(Associative, DHTSLL, UniquePtrCustomDeleter);
using DRPTE   = DEFINE_TEST_THUNK(Associative, DHTSLL, RefPtr);

// Dynamically sized hashtable, with delayed initialization
DEFINE_TEST_OBJECTS(DIDHTSLL);
using DIDUMTE   = DEFINE_TEST_THUNK(Associative, DIDHTSLL, Unmanaged);
using DIDUPDDTE = DEFINE_TEST_THUNK(Associative, DIDHTSLL, UniquePtrDefaultDeleter);
using DIDUPCDTE = DEFINE_TEST_THUNK(Associative, DIDHTSLL, UniquePtrCustomDeleter);
using DIDRPTE   = DEFINE_TEST_THUNK(Associative, DIDHTSLL, RefPtr);

// Versions of the test objects which support clear_unsafe.
template <typename PtrType>
using CU_HTSLLTraits = HTSLLTraits<PtrType, fbl::NodeOptions::AllowClearUnsafe>;
DEFINE_TEST_OBJECTS(CU_HTSLL);
using CU_UMTE   = DEFINE_TEST_THUNK(Associative, CU_HTSLL, Unmanaged);
using CU_UPDDTE = DEFINE_TEST_THUNK(Associative, CU_HTSLL, UniquePtrDefaultDeleter);

//////////////////////////////////////////
// General container specific tests.
//////////////////////////////////////////
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UMTE,     Clear)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UPDDTE,   Clear)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UPCDTE,   Clear)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, RPTE,     Clear)

#if TEST_WILL_NOT_COMPILE || 0
// Won't compile because node lacks AllowClearUnsafe option.
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UMTE,     ClearUnsafe)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UPDDTE,   ClearUnsafe)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UPCDTE,   ClearUnsafe)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, RPTE,     ClearUnsafe)
#endif

#if TEST_WILL_NOT_COMPILE || 0
// Won't compile because pointer type is managed.
RUN_ZXTEST(SinglyLinkedHashTableTest, CU_UPDDTE,  ClearUnsafe)
#endif

RUN_ZXTEST(SinglyLinkedHashTableTest, CU_UMTE,  ClearUnsafe)

RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UMTE,     IsEmpty)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UPDDTE,   IsEmpty)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UPCDTE,   IsEmpty)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, RPTE,     IsEmpty)

RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UMTE,     Iterate)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UPDDTE,   Iterate)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UPCDTE,   Iterate)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, RPTE,     Iterate)

// Hashtables with singly linked list bucket can perform direct
// iterator/reference erase operations, but the operations will be O(n)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UMTE,     IterErase)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UPDDTE,   IterErase)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UPCDTE,   IterErase)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, RPTE,     IterErase)

RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UMTE,     DirectErase)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UPDDTE,   DirectErase)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UPCDTE,   DirectErase)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, RPTE,     DirectErase)

RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UMTE,     MakeIterator)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UPDDTE,   MakeIterator)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UPCDTE,   MakeIterator)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, RPTE,     MakeIterator)

// HashTables with SinglyLinkedList buckets cannot iterate backwards (because
// their buckets cannot iterate backwards)
#if TEST_WILL_NOT_COMPILE || 0
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UMTE,     ReverseIterErase)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UPDDTE,   ReverseIterErase)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UPCDTE,   ReverseIterErase)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, RPTE,     ReverseIterErase)

RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UMTE,     ReverseIterate)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UPDDTE,   ReverseIterate)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UPCDTE,   ReverseIterate)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, RPTE,     ReverseIterate)
#endif

// Hash tables do not support swapping or Rvalue operations (Assignment or
// construction) as doing so would be an O(n) operation (With 'n' == to the
// number of buckets in the hashtable)
#if TEST_WILL_NOT_COMPILE || 0
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UMTE,     Swap)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UPDDTE,   Swap)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UPCDTE,   Swap)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, RPTE,     Swap)

RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UMTE,     RvalueOps)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UPDDTE,   RvalueOps)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UPCDTE,   RvalueOps)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, RPTE,     RvalueOps)
#endif

RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UPDDTE,   Scope)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UPCDTE,   Scope)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, RPTE,     Scope)

RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UMTE,     TwoContainer)
#if TEST_WILL_NOT_COMPILE || 0
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UPDDTE,   TwoContainer)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UPCDTE,   TwoContainer)
#endif
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, RPTE,     TwoContainer)

RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UMTE,     ThreeContainerHelper)
#if TEST_WILL_NOT_COMPILE || 0
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UPDDTE,   ThreeContainerHelper)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UPCDTE,   ThreeContainerHelper)
#endif
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, RPTE,     ThreeContainerHelper)

RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UMTE,     IterCopyPointer)
#if TEST_WILL_NOT_COMPILE || 0
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UPDDTE,   IterCopyPointer)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UPCDTE,   IterCopyPointer)
#endif
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, RPTE,     IterCopyPointer)

RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UMTE,     EraseIf)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UPDDTE,   EraseIf)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UPCDTE,   EraseIf)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, RPTE,     EraseIf)

RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UMTE,     FindIf)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UPDDTE,   FindIf)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UPCDTE,   FindIf)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, RPTE,     FindIf)

//////////////////////////////////////////
// Associative container specific tests.
//////////////////////////////////////////
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UMTE,     InsertByKey)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UPDDTE,   InsertByKey)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UPCDTE,   InsertByKey)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, RPTE,     InsertByKey)

RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UMTE,     FindByKey)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UPDDTE,   FindByKey)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UPCDTE,   FindByKey)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, RPTE,     FindByKey)

RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UMTE,     EraseByKey)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UPDDTE,   EraseByKey)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UPCDTE,   EraseByKey)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, RPTE,     EraseByKey)

RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UMTE,     InsertOrFind)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UPDDTE,   InsertOrFind)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UPCDTE,   InsertOrFind)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, RPTE,     InsertOrFind)

RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UMTE,     InsertOrReplace)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UPDDTE,   InsertOrReplace)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, UPCDTE,   InsertOrReplace)
RUN_HT_ZXTEST(SinglyLinkedHashTableTest, RPTE,     InsertOrReplace)
// clang-format on

}  // namespace intrusive_containers
}  // namespace tests
}  // namespace fbl
