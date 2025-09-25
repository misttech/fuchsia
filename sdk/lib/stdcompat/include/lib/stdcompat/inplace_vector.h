// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_STDCOMPAT_INPLACE_VECTOR_H_
#define LIB_STDCOMPAT_INPLACE_VECTOR_H_

#include <algorithm>
#include <cassert>
#include <compare>
#include <cstddef>
#include <cstdlib>
#include <initializer_list>
#include <iterator>
#include <memory>
#include <ranges>
#include <stdexcept>
#include <type_traits>
#include <utility>

#include "internal/span.h"

namespace cpp23 {

#if __cpp_lib_containers_ranges >= 202202L
// Use the standard from_range_t if available.
using std::from_range;
using std::from_range_t;
#else
// Polyfill for C++23 std::from_range_t.
struct from_range_t {
  explicit from_range_t() = default;
};
inline constexpr from_range_t from_range{};
#endif

}  // namespace cpp23

namespace cpp26 {

namespace internal {

template <typename R, typename T>
using container_compatible_range_t =
    std::enable_if_t<std::is_convertible_v<decltype(*std::begin(std::declval<R>())), T>>;

[[noreturn]] inline void inplace_vector_abort() {
#if __cpp_exceptions
  throw std::bad_alloc();
#else
  __builtin_abort();
#endif
}

[[noreturn]] inline void throw_abort_out_of_range() {
#if __cpp_exceptions
  throw std::out_of_range();
#else
  __builtin_abort();
#endif
}

}  // namespace internal

// Polyfill for C++26 std::inplace_vector.
// See: https://wg21.link/P0843R8
template <typename T, std::size_t N>
class inplace_vector {
 public:
  using value_type = T;
  using pointer = T*;
  using const_pointer = const T*;
  using reference = value_type&;
  using const_reference = const value_type&;
  using size_type = size_t;
  using difference_type = ptrdiff_t;
  using iterator = cpp20::internal::span_iterator<value_type>;
  using const_iterator = cpp20::internal::span_iterator<const value_type>;
  using reverse_iterator = std::reverse_iterator<iterator>;
  using const_reverse_iterator = std::reverse_iterator<const_iterator>;

  constexpr inplace_vector() noexcept : size_(0) {}
  constexpr explicit inplace_vector(size_type n) : size_(0) {
    if (n > N) {
      internal::inplace_vector_abort();
    }
    for (size_type i = 0; i < n; ++i) {
      emplace_back();
    }
  }

  constexpr inplace_vector(size_type n, const T& value) : size_(0) {
    if (n > N) {
      internal::inplace_vector_abort();
    }
    for (size_type i = 0; i < n; ++i) {
      emplace_back(value);
    }
  }
  template <class InputIterator, typename = std::enable_if_t<!std::is_integral_v<InputIterator>>>
  constexpr inplace_vector(InputIterator first, InputIterator last) : size_(0) {
    for (auto it = first; it != last; ++it) {
      emplace_back(*it);
    }
  }
  template <typename R, typename = internal::container_compatible_range_t<R, T>>
  constexpr inplace_vector(cpp23::from_range_t, R&& rg) : size_(0) {
    for (auto&& elem : rg) {
      emplace_back(std::forward<decltype(elem)>(elem));
    }
  }
  constexpr inplace_vector(std::initializer_list<T> ilist) : size_(0) {
    if (ilist.size() > N) {
      internal::inplace_vector_abort();
    }
    for (const T& v : ilist) {
      emplace_back(v);
    }
  }
  constexpr inplace_vector(const inplace_vector& other) : inplace_vector() { *this = other; }
  constexpr inplace_vector& operator=(const inplace_vector& other) {
    if (other.size() > N) {
      internal::inplace_vector_abort();
    }
    if (this == &other) {
      return *this;
    }
    clear();
    for (const T& v : other) {
      emplace_back(v);
    }
    return *this;
  }
  constexpr inplace_vector(inplace_vector&& other) noexcept(
      N == 0 || (std::is_nothrow_swappable_v<T> && std::is_nothrow_move_constructible_v<T> &&
                 std::is_nothrow_destructible_v<T>))
      : size_(0) {
    if (other.size_ > N) {
      internal::inplace_vector_abort();
    }
    this->swap(other);
    other.clear();
  }
  constexpr inplace_vector& operator=(inplace_vector&& other) noexcept(
      N == 0 || (std::is_nothrow_swappable_v<T> && std::is_nothrow_move_constructible_v<T> &&
                 std::is_nothrow_destructible_v<T>)) {
    if (this == &other) {
      return *this;
    }
    if (other.size_ > N) {
      internal::inplace_vector_abort();
    }
    this->clear();
    this->swap(other);
    return *this;
  }
  ~inplace_vector() { clear(); }

  template <class InputIterator, typename = std::enable_if_t<!std::is_integral_v<InputIterator>>>
  constexpr void assign(InputIterator first, InputIterator last) {
    clear();
    for (auto it = first; it != last; ++it) {
      emplace_back(*it);
    }
  }
  template <typename R, typename = internal::container_compatible_range_t<R, T>>
  constexpr void assign_range(R&& rg) {
    clear();
    for (auto&& elem : rg) {
      emplace_back(std::forward<decltype(elem)>(elem));
    }
  }
  constexpr void assign(size_type n, const_reference value) {
    if (n > N) {
      internal::inplace_vector_abort();
    }
    clear();
    for (size_type i = 0; i < n; ++i) {
      emplace_back(value);
    }
  }
  constexpr void assign(std::initializer_list<T> ilist) {
    if (ilist.size() > N) {
      internal::inplace_vector_abort();
    }
    clear();
    for (const T& v : ilist) {
      emplace_back(v);
    }
  }

  constexpr size_type size() const noexcept { return size_; }
  static constexpr size_type capacity() noexcept { return N; }
  static constexpr size_type max_size() noexcept { return N; }
  [[nodiscard]] constexpr bool empty() const noexcept { return size_ == 0; }

  constexpr void resize(size_type n) { resize(n, T()); }
  constexpr void resize(size_type n, const T& c) {
    if (n > N) {
      internal::inplace_vector_abort();
    }
    // n < size_ or does nothing.
    for (size_type i = n; i < size_; ++i) {
      data()[i].~T();
    }
    // n > size_ or does nothing.
    for (size_type i = size_; i < n; ++i) {
      new (data() + i) T(c);
    }
    size_ = n;
  }

  constexpr void reserve(size_type n) {
    if (n > N)
      internal::inplace_vector_abort();
    // For inplace_vector, reserve is a no-op if n <= N.
  }

  constexpr void shrink_to_fit() {
    // For inplace_vector, this is always a no-op.
  }

  constexpr reference operator[](size_type i) {
    assert(i < size_);
    return data()[i];
  }
  constexpr const_reference operator[](size_type i) const {
    assert(i < size_);
    return data()[i];
  }
  constexpr reference at(size_type i) {
    if (i >= size_) {
      internal::throw_abort_out_of_range();
    }
    return data()[i];
  }
  constexpr const_reference at(size_type i) const {
    if (i >= size_) {
      internal::throw_abort_out_of_range();
    }
    return data()[i];
  }

  constexpr reference front() {
    assert(size_ > 0);
    return data()[0];
  }
  constexpr const_reference front() const {
    assert(size_ > 0);
    return data()[0];
  }
  constexpr reference back() {
    assert(size_ > 0);
    return data()[size_ - 1];
  }
  constexpr const_reference back() const {
    assert(size_ > 0);
    return data()[size_ - 1];
  }

  constexpr T* data() noexcept { return reinterpret_cast<T*>(buffer_); }
  constexpr const T* data() const noexcept { return reinterpret_cast<const T*>(buffer_); }

  constexpr iterator begin() noexcept { return iterator(data()); }
  constexpr const_iterator begin() const noexcept { return const_iterator(data()); }
  constexpr const_iterator cbegin() const noexcept { return const_iterator(data()); }
  constexpr iterator end() noexcept { return iterator(data() + size_); }
  constexpr const_iterator end() const noexcept { return const_iterator(data() + size_); }
  constexpr const_iterator cend() const noexcept { return const_iterator(data() + size_); }

  constexpr reverse_iterator rbegin() noexcept { return reverse_iterator(end()); }
  constexpr const_reverse_iterator rbegin() const noexcept { return const_reverse_iterator(end()); }
  constexpr const_reverse_iterator crbegin() const noexcept {
    return const_reverse_iterator(end());
  }
  constexpr reverse_iterator rend() noexcept { return reverse_iterator(begin()); }
  constexpr const_reverse_iterator rend() const noexcept { return const_reverse_iterator(begin()); }
  constexpr const_reverse_iterator crend() const noexcept {
    return const_reverse_iterator(begin());
  }

  constexpr void clear() noexcept {
    for (size_type i = 0; i < size_; ++i) {
      data()[i].~T();
    }
    size_ = 0;
  }

  constexpr T& push_back(const T& value) {
    if (T* result = try_push_back(value)) {
      return *result;
    }
    internal::inplace_vector_abort();
  }
  constexpr T& push_back(T&& value) {
    static_assert(std::is_move_constructible_v<T>);
    if (T* result = try_push_back(std::move(value))) {
      return *result;
    }
    internal::inplace_vector_abort();
  }

  constexpr T* try_push_back(const T& value) noexcept(std::is_nothrow_copy_constructible_v<T>) {
    return try_emplace_back(value);
  }
  constexpr T* try_push_back(T&& value) noexcept(std::is_nothrow_move_constructible_v<T>) {
    return try_emplace_back(std::move(value));
  }

  template <class... Args, typename = std::enable_if_t<std::is_constructible_v<T, Args...>>>
  constexpr reference emplace_back(Args&&... args) {
    if (T* result = try_emplace_back(std::forward<Args>(args)...)) {
      return *result;
    }
    internal::inplace_vector_abort();
  }
  template <class... Args, typename = std::enable_if_t<std::is_constructible_v<T, Args...>>>
  constexpr T* try_emplace_back(Args&&... args) noexcept(
      std::is_nothrow_constructible_v<T, Args...>) {
    if (size_ >= N) {
      return nullptr;
    }
    new (data() + size_) T(std::forward<Args>(args)...);
    ++size_;
    return &back();
  }

  template <typename R, typename = internal::container_compatible_range_t<R, T>>
  constexpr void append_range(R&& rg) {
    for (auto&& elem : rg) {
      emplace_back(std::forward<decltype(elem)>(elem));
    }
  }

  template <class... Args, typename = std::enable_if_t<std::is_constructible_v<T, Args...>>>
  constexpr T& unchecked_emplace_back(Args&&... args) {
    assert(size_ < N);
    new (data() + size_) T(std::forward<Args>(args)...);
    ++size_;
    return back();
  }

  constexpr T& unchecked_push_back(const T& value) {
    assert(size_ < N);
    new (data() + size_) T(value);
    ++size_;
    return back();
  }
  constexpr T& unchecked_push_back(T&& value) {
    assert(size_ < N);
    new (data() + size_) T(std::move(value));
    ++size_;
    return back();
  }

  constexpr void pop_back() {
    assert(!empty());
    --size_;
    data()[size_].~T();
  }

  template <class... Args, typename = std::enable_if_t<std::is_constructible_v<T, Args...>>>
  constexpr iterator emplace(const_iterator position, Args&&... args) {
    if (size_ >= N) {
      internal::inplace_vector_abort();
    }
    auto pos = begin() + (position - cbegin());
    auto insert_index = static_cast<size_type>(pos - begin());
    if (insert_index < size_) {
      for (size_type i = size_; i > insert_index; --i) {
        new (data() + i) T(std::move(data()[i - 1]));
        data()[i - 1].~T();
      }
    }
    new (data() + insert_index) T(std::forward<Args>(args)...);
    ++size_;
    return begin() + insert_index;
  }

  constexpr iterator insert(const_iterator position, const T& x) { return emplace(position, x); }
  constexpr iterator insert(const_iterator position, T&& x) {
    return emplace(position, std::move(x));
  }
  constexpr iterator insert(const_iterator position, size_type n, const_reference x) {
    return batch_insert_impl(
        position, n, [&x](T* dest, size_type count) { std::uninitialized_fill_n(dest, count, x); });
  }
  template <class InputIterator, typename = std::enable_if_t<!std::is_integral_v<InputIterator>>>
  constexpr iterator insert(const_iterator position, InputIterator first, InputIterator last) {
    size_type count = static_cast<size_type>(std::distance(first, last));
    return batch_insert_impl(position, count, [first, last](T* dest, size_type) {
      std::uninitialized_copy(first, last, dest);
    });
  }
  template <typename R, typename = internal::container_compatible_range_t<R, T>>
  constexpr iterator insert_range(const_iterator position, R&& rg) {
    size_type count = static_cast<size_type>(std::distance(std::begin(rg), std::end(rg)));
    return batch_insert_impl(position, count, [&rg](T* dest, size_type) {
      std::uninitialized_copy(std::begin(rg), std::end(rg), dest);
    });
  }
  constexpr iterator insert(const_iterator position, std::initializer_list<value_type> ilist) {
    return insert_range(position, ilist);
  }

  constexpr iterator erase(const_iterator pos) {
    auto position = begin() + (pos - cbegin());
    if (position >= end()) {
      return end();
    }
    for (auto it = position; it + 1 != end(); ++it) {
      *it = std::move(*(it + 1));
    }
    --size_;
    data()[size_].~T();
    return position;
  }
  constexpr iterator erase(const_iterator first, const_iterator last) {
    auto start_pos = begin() + (first - cbegin());
    auto end_pos = begin() + (last - cbegin());
    if (start_pos >= end() || first == last) {
      return end();
    }
    size_type erase_count = end_pos - start_pos;
    for (auto it = start_pos; end_pos + (it - start_pos) != end(); ++it) {
      *it = std::move(*(end_pos + (it - start_pos)));
    }
    for (size_type i = 0; i < erase_count; ++i) {
      --size_;
      data()[size_].~T();
    }
    return start_pos;
  }

  constexpr void swap(inplace_vector& other) noexcept(std::is_nothrow_swappable_v<T> &&
                                                      std::is_nothrow_move_constructible_v<T> &&
                                                      std::is_nothrow_destructible_v<T>) {
    if constexpr (N == 0) {
      return;
    } else {
      if (this == &other) {
        return;
      }
      if (size_ > other.size_) {
        other.swap(*this);
        return;
      }
      // Swap elements.
      T* this_data = data();
      T* other_data = other.data();
      for (size_type i = 0; i < size_; ++i) {
        using std::swap;
        swap(this_data[i], other_data[i]);
      }
      // Move additional elements from other to this.
      for (size_type i = size_; i < other.size_; ++i) {
        new (this_data + i) T(std::move(other_data[i]));
        other_data[i].~T();
      }
      std::swap(size_, other.size_);
    }
  }

  constexpr friend bool operator==(const inplace_vector& lhs, const inplace_vector& rhs) {
    return std::equal(lhs.begin(), lhs.end(), rhs.begin(), rhs.end());
  }
  constexpr friend bool operator!=(const inplace_vector& lhs, const inplace_vector& rhs) {
    return !(lhs == rhs);
  }

  constexpr friend void swap(inplace_vector& x, inplace_vector& y) noexcept(noexcept(x.swap(y))) {
    x.swap(y);
  }

#if __cpp_lib_three_way_comparison >= 201907L
  // C++20 three-way comparison operator.
  constexpr friend auto operator<=>(const inplace_vector& lhs, const inplace_vector& rhs) {
    return std::lexicographical_compare_three_way(lhs.begin(), lhs.end(), rhs.begin(), rhs.end());
  }
#else
  // Traditional comparison operators for broader compatibility.
  constexpr friend bool operator<(const inplace_vector& lhs, const inplace_vector& rhs) {
    return std::lexicographical_compare(lhs.begin(), lhs.end(), rhs.begin(), rhs.end());
  }

  constexpr friend bool operator<=(const inplace_vector& lhs, const inplace_vector& rhs) {
    return !(rhs < lhs);
  }
  constexpr friend bool operator>(const inplace_vector& lhs, const inplace_vector& rhs) {
    return rhs < lhs;
  }
  constexpr friend bool operator>=(const inplace_vector& lhs, const inplace_vector& rhs) {
    return !(lhs < rhs);
  }
#endif

 private:
  template <typename Constructor>
  constexpr iterator batch_insert_impl(const_iterator position, size_type count,
                                       Constructor&& ctor) {
    if (count > N - size_) {
      internal::inplace_vector_abort();
    }
    if (count == 0) {
      return begin() + (position - cbegin());
    }
    auto insert_index = static_cast<size_type>(position - cbegin());
    if (insert_index < size_) {
      for (size_type i = size_; i > insert_index; --i) {
        new (data() + i + count - 1) T(std::move(data()[i - 1]));
        data()[i - 1].~T();
      }
    }
    ctor(data() + insert_index, count);
    size_ += count;
    return begin() + insert_index;
  }

  alignas(T) unsigned char buffer_[sizeof(T) * N];
  size_type size_;
};

template <class T, std::size_t N>
constexpr std::enable_if_t<N == 0 || std::is_swappable_v<T>, void> swap(
    inplace_vector<T, N>& x, inplace_vector<T, N>& y) noexcept(noexcept(x.swap(y))) {
  x.swap(y);
}

template <class T, std::size_t N, class U>
constexpr typename inplace_vector<T, N>::size_type erase(inplace_vector<T, N>& c, const U& value) {
  auto it = std::remove(c.begin(), c.end(), value);
  auto r = std::distance(it, c.end());
  c.erase(it, c.end());
  return r;
}
template <class T, std::size_t N, class Predicate>
constexpr typename inplace_vector<T, N>::size_type erase_if(inplace_vector<T, N>& c,
                                                            Predicate pred) {
  auto it = std::remove_if(c.begin(), c.end(), pred);
  auto r = std::distance(it, c.end());
  c.erase(it, c.end());
  return r;
}

}  // namespace cpp26

namespace stdcompat {

template <class T, std::size_t N, class U>
constexpr typename cpp26::inplace_vector<T, N>::size_type erase(cpp26::inplace_vector<T, N>& c,
                                                                const U& value) {
  auto it = std::remove(c.begin(), c.end(), value);
  auto r = std::distance(it, c.end());
  c.erase(it, c.end());
  return r;
}
template <class T, std::size_t N, class Predicate>
constexpr typename cpp26::inplace_vector<T, N>::size_type erase_if(cpp26::inplace_vector<T, N>& c,
                                                                   Predicate pred) {
  auto it = std::remove_if(c.begin(), c.end(), pred);
  auto r = std::distance(it, c.end());
  c.erase(it, c.end());
  return r;
}

}  // namespace stdcompat

#endif  // LIB_STDCOMPAT_INPLACE_VECTOR_H_
