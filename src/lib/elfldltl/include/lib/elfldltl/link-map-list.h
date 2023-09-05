// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_LIB_ELFLDLTL_INCLUDE_LIB_ELFLDLTL_LINK_MAP_LIST_H_
#define SRC_LIB_ELFLDLTL_INCLUDE_LIB_ELFLDLTL_LINK_MAP_LIST_H_

#include <iterator>

#include "layout.h"
#include "memory.h"
#include "svr4-abi.h"

namespace elfldltl {

template <class Memory, class Elf = Elf<>, class AbiTraits = LocalAbiTraits,
          typename EntryType = typename Elf::template LinkMap<AbiTraits>,
          auto LinkMapMember = nullptr>
class LinkMapList {
  template <bool Reverse>
  class IteratorImpl;

 public:
  using value_type = EntryType;
  using reference = value_type&;
  using const_reference = const value_type&;
  using difference_type = ptrdiff_t;
  using size_type = size_t;

  using iterator = IteratorImpl<false>;
  using reverse_iterator = IteratorImpl<true>;
  using const_iterator = iterator;
  using const_reverse_iterator = reverse_iterator;

  constexpr LinkMapList(const LinkMapList&) = default;

  constexpr LinkMapList(Memory& memory, typename Elf::size_type map) : memory_(memory), map_(map) {}

  iterator begin() const { return iterator(memory_, map_); }

  iterator end() const { return iterator(memory_, 0); }

  reverse_iterator rbegin() const { return reverse_iterator(memory_, map_); }

  reverse_iterator rend() const { return reverse_iterator(memory_, 0); }

 private:
  static constexpr const auto& GetEntry(const value_type& value) {
    if constexpr (LinkMapMember) {
      return value.*LinkMapMember;
    } else {
      return value;
    }
  }

  Memory& memory_;
  typename Elf::size_type map_;
};

// Deduction guide.
template <class Memory>
LinkMapList(Memory&, Elf<>::size_type) -> LinkMapList<Memory>;

template <class Elf, class AbiTraits, class Memory, typename EntryType, auto LinkMapMember>
template <bool Reverse>
class LinkMapList<Elf, AbiTraits, Memory, EntryType, LinkMapMember>::IteratorImpl {
 public:
  using iterator_category = std::bidirectional_iterator_tag;

  constexpr IteratorImpl() = default;
  constexpr IteratorImpl(const IteratorImpl&) = default;

  constexpr bool operator==(const IteratorImpl& other) const { return address_ == other.address_; }

  constexpr bool operator!=(const IteratorImpl& other) const { return !(*this == other); }

  constexpr const value_type& operator*() const { return *value_; }

  constexpr IteratorImpl& operator++() {  // prefix
    Update<Reverse ? &LinkMapType::prev : &LinkMapType::next>();
    return *this;
  }

  constexpr IteratorImpl operator++(int) {  // postfix
    IteratorImpl old = *this;
    ++*this;
    return old;
  }

  constexpr IteratorImpl& operator--() {  // prefix
    Update<Reverse ? &LinkMapType::next : &LinkMapType::prev>();
    return *this;
  }

  constexpr IteratorImpl operator--(int) {  // postfix
    IteratorImpl old = *this;
    ++*this;
    return old;
  }

 private:
  using LinkMapType = std::decay_t<decltype(GetEntry(std::declval<const value_type&>()))>;

  constexpr IteratorImpl(Memory& memory, typename Elf::size_type address)
      : memory_(&memory), address_(address) {
    Update();
  }

  // Read the struct from the current address pointer into value_.
  // If the pointer can't be read, reset address_ to zero (end state).
  template <auto Member = nullptr>
  constexpr void Update() {
    if constexpr (Member) {
      address_ = GetEntry(*value_).*Member.address();
    }
    if (address_ != 0) {
      if (auto data = memory_->template ReadArray<value_type>(address_, 1)) {
        value_ = data->data();
      } else {
        value_ = nullptr;
        address_ = 0;
      }
    }
  }

  Memory* memory_ = nullptr;
  const value_type* value_ = nullptr;
  typename Elf::size_type address_ = 0;
};

}  // namespace elfldltl

#endif  // SRC_LIB_ELFLDLTL_INCLUDE_LIB_ELFLDLTL_LINK_MAP_LIST_H_
