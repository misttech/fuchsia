// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_LIB_FBL_INCLUDE_FBL_PAGED_VIEW_H_
#define ZIRCON_KERNEL_LIB_FBL_INCLUDE_FBL_PAGED_VIEW_H_

#include <zircon/listnode.h>

#include <cassert>
#include <cstddef>
#include <cstdint>

#include <fbl/algorithm.h>
#include <vm/page.h>
#include <vm/physmap.h>

namespace fbl {

// PagedView is an adapter for a set of PMM memory pages that provides typed "views" into these
// pages. The view does not own the lifetime of these pages, so it should not be used once the pages
// have been deallocated.
//
// Internally, it operates page-at-a-time, so it is safe to deallocate and unlink pages
// above or below the current page (as returned by the current_page() method).
//
// There are three different interfaces, which can be mixed as needed:
//
//  // Assume for the examples below:
//  assert(ZX_OK == pmm_alloc_pages(num_pages, &page_list));
//  struct MyNode { int foo; };
//  fbl::PagedView<MyNode> paged_view(page_list);
//
//  1. Record-at-a-time: Good for building data when the producer drives the loop.
//     The pattern is:
//
//    for ( /* producer loop */ ) {
//      paged_view.record().foo = x;
//      if (!paged_view.next_record()) {
//        // Reached the end of the allocated pages. This is either a logical error (not enough
//        // pages were allocated) or is by design, and we need to "consume" the data and
//        // somehow resume the producer loop.
//      }
//    }
//
//  2. Page-at-a-time: Good for consuming the data in the pages. The pattern is:
//
//    do {
//      for (auto& node : paged_view.span()) {
//        consume_data(.., node.foo, ..);
//      }
//    } while(paged_view.next_page());
//
//    Note that span() returns all the memory of that page. If a particular page
//    has not been fully initialized, you should use the marker mode below.
//
//  3. Marker mode: Allows saving a cursor and returning to it. Applicable to either of
//     the above forms but easier to see in the second case:
//
//    // Assume we allocated exactly the number of pages needed.
//    for ( /* producer loop */ ) {
//      paged_view.record().foo = x;
//      assert(paged_view.next_record());
//    }
//    // The producer is done. Now we need to consume however many nodes have been added;
//    // however, the last page might not be full. We can handle that by getting a marker
//    // and resetting the internal cursor.
//
//    auto marker = paged_view.reset();
//
//    do {
//      for (auto& node : paged_view.span(marker)) {
//        consume_data(.., node.foo, ..);
//      }
//      if (paged_view.marker_in_page(marker)) break;
//    } while(paged_view.next_page());
//
//    // Note the use of span(marker) rather than just span(), so that for the last
//    // page, the correct number of records are returned.
//
//  Marker Direction
//  The markers come in two flavors, referred to below as "direction of travel". The reset() method
//  returns a "from-start" marker that can be used to iterate from the first record to the last
//  record that was current at the reset() callsite. This is what the last example above shows.
//
//  Also, you can use get_marker_to_end() to obtain a "to-end" marker, which can be used to iterate
//  from the currently active record to the last record.
//
//  The correct use of both these markers is guaranteed by using the for_each_page() method, or its
//  source can be inspected to roll your own version.
//
template <typename Record>
class PagedView {
 public:
  static constexpr uint32_t kMaxCount = PAGE_SIZE / fbl::round_up(sizeof(Record), alignof(Record));

  // A marker stores the cursor position of a PagedView and the direction of travel, which can be
  // from the start of the first page to the cursor, or from the cursor to the end of the last page.
  class Marker {
   public:
    enum Direction { kFromStart, kToEnd };
    bool is_null() const { return (page_ == nullptr); }
    Direction direction() const { return dir_; }

   private:
    friend class PagedView<Record>;

    Marker() = default;
    Marker(vm_page_t* page, uint32_t index, Direction part)
        : page_(page), index_(index), dir_(part) {}

    vm_page_t* page_ = nullptr;
    uint32_t index_ = 0;
    const Direction dir_ = kFromStart;
  };

  // Creates the paged view. An assert will fire if the list is empty.
  explicit PagedView(list_node* list) : current_page_(first_page(list)), list_(list) {
    top_record_ = page_as_records();
  }

  // Returns the current record.
  Record& record() { return top_record_[index_]; }

  // Moves the internal cursor to the next record. Returns false if the cursor has moved past the
  // last record, true otherwise.
  [[nodiscard]] bool next_record() {
    index_ += 1;
    if (index_ < kMaxCount) {
      return true;
    }
    return next_page() != nullptr;
  }

  // Returns a span to the entire current page.
  ktl::span<Record> span() { return {top_record_, kMaxCount}; }
  // Returns a span that covers from the start of the current page to the cursor.
  ktl::span<Record> span_first() { return span().first(index_); }
  // Returns a span that covers from the cursor to the end of the page.
  ktl::span<Record> span_last() { return span().last(kMaxCount - index_); }

  // Returns a span modulated by the |marker|. If the marker points to a different page, then the
  // span returned is the full span of the current page. If the marker points to the current page,
  // then the span returned is from the top of the page if Marker::direction() is kFromStart, or it
  // returns the span from the marker to the end of the page if the direction is kToEnd.
  ktl::span<Record> span(const Marker& marker) {
    if (marker.page_ != current_page_) {
      return span();
    }
    return (marker.dir_ == Marker::kFromStart) ? span().first(marker.index_)
                                               : span().last(kMaxCount - marker.index_);
  }

  // Resets the PagedView internal cursor to the |start| + |start_index| position and returns
  // a from-start marker to the previous cursor. The |start| page must be in the list of
  // pages given to the constructor of PagedView.
  Marker reset(vm_page_t* start = nullptr, uint32_t start_index = 0) {
    if (start_index > kMaxCount) {
      return Marker();
    }
    auto marker = get_marker_from_start();
    current_page_ = (start == nullptr) ? first_page(list_) : start;
    top_record_ = page_as_records();
    index_ = start_index;
    return marker;
  }

  // Resets the PagedView to the location indicated by |marker| and returns a from-start marker
  // to the previous cursor position. The direction of travel of |marker| is ignored.
  Marker reset(const Marker& marker) {
    ASSERT(!marker.is_null());
    return reset(marker.page_, marker.index_);
  }

  // Gets a marker to the current position, of type from-start, that can be passed to reset() or
  // to span().
  Marker get_marker_from_start() const {
    return (index_ < kMaxCount) ? Marker(current_page_, index_, Marker::kFromStart) : Marker();
  }

  // Gets a marker to the current position, of type to-end, that can be passed to reset() or to
  // span().
  Marker get_marker_to_end(uint32_t skip = 0) const {
    return (index_ < kMaxCount) ? Marker(current_page_, index_, Marker::kToEnd) : Marker();
  }

  // Returns true if the marker points to the PagedView's current page.
  bool marker_in_page(const Marker& marker) { return (current_page_ == marker.page_); }

  // Returns the current page. Do not free this page if you are going to call other methods.
  vm_page_t* current_page() const { return current_page_; }
  // Returns the current index, relative to the start of current_page().
  uint32_t current_index() const { return index_; }

  // Moves to the next page. Returns the new current page, or null if the end of the list is
  // reached.
  [[nodiscard]] vm_page_t* next_page() {
    advance_page();
    return current_page_;
  }

  // Repeatedly calls |fn| with the full span of each page.
  template <typename Fn>
  void for_each_page(Fn fn) {
    do {
      fn(span());
    } while (next_page());
  }

  // Repeatedly calls |fn| with the pages indicated by |marker|. If the marker is from-start,
  // fn is called with all the pages from the first page to the marker. If the marker is to-end, fn
  // is called with all the pages from the cursor to the last page.
  template <typename Fn>
  void for_each_page(const Marker& marker, Fn fn) {
    if (marker.direction() == Marker::kToEnd) {
      // Call from the marker to (and including) the last page.
      reset(marker);
      do {
        fn(span(marker));
      } while (next_page());
    } else {
      // Call fn from the start to (and including) the page with the marker.
      do {
        fn(span(marker));
        if (marker_in_page(marker))
          break;
      } while (next_page());
    }
  }

  // Moves the internal cursor forward or backward by |count| records. The runtime is O(n) in the
  // number of pages. On success, it returns the page that is now current. Returns null if moving
  // past the last record or before the first record.
  vm_page_t* move(int32_t count) {
    vm_page_t* np = current_page_;
    auto ni = static_cast<int64_t>(index_) + count;
    if (count > 0) {
      while (ni >= kMaxCount) {
        np = next_page(list_, np);
        if (np == nullptr)
          return nullptr;
        ni -= kMaxCount;
      }
    } else if (count < 0) {
      while (ni < 0) {
        np = prev_page(list_, np);
        if (np == nullptr)
          return nullptr;
        ni += kMaxCount;
      }
    }
    current_page_ = np;
    index_ = static_cast<uint32_t>(ni);
    return current_page_;
  }

 private:
  Record* page_as_records() {
    if (current_page_ == nullptr) {
      return nullptr;
    }
    return reinterpret_cast<Record*>(paddr_to_physmap(current_page_->paddr()));
  }

  static vm_page_t* first_page(list_node_t* list) {
    ASSERT(!list_is_empty(list));
    return list_peek_head_type(list, vm_page_t, queue_node);
  }

  static vm_page_t* next_page(list_node_t* list, vm_page_t* current) {
    return list_next_type(list, &current->queue_node, vm_page_t, queue_node);
  }

  static vm_page_t* prev_page(list_node_t* list, vm_page_t* current) {
    return list_prev_type(list, &current->queue_node, vm_page_t, queue_node);
  }

  void advance_page() {
    index_ = 0;
    current_page_ = next_page(list_, current_page_);
    top_record_ = page_as_records();
  }

  uint32_t index_ = 0;
  // Note that |top_record| and |current_page| are derivable from each other, but going
  // from a Record to a page is somewhat costly, and going from |current_page| to a Record would
  // happen quite frequently if the record-at-a-time interface is used, so we keep both
  // here. If many instances of this class are going to be stored, it might be worth
  // removing |top_record| to save space.
  Record* top_record_ = nullptr;
  vm_page_t* current_page_;
  list_node_t* const list_;
};

}  // namespace fbl

#endif  // ZIRCON_KERNEL_LIB_FBL_INCLUDE_FBL_PAGED_VIEW_H_
