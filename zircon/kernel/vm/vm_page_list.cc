// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT
#include "vm/vm_page_list.h"

#include <trace.h>
#include <zircon/types.h>

#include <ktl/memory.h>
#include <ktl/utility.h>
#include <vm/page_slab_allocator.h>
#include <vm/vm.h>

#include "vm_priv.h"

#include <ktl/enforce.h>

#define LOCAL_TRACE VM_GLOBAL_TRACE(0)

struct VmPageOrMarkerSlotAndBool {
  VmPageOrMarker* slot;
  bool is_interval;
};

struct VmPageOrMarkerConstSlotAndOffset {
  const VmPageOrMarker* slot;
  uint64_t offset;
};

// Trampolines into the Rust VmPageList implementation. The Rust `VmPageList` has the same layout as
// this class, so `this` is passed through directly. The contracts of these functions are documented
// at their definitions in vm_page_list.rs.
extern "C" {
bool rust_vm_page_list_has_no_page_or_ref(const VmPageList* list);
bool rust_vm_page_list_has_no_page_ref_or_marker(const VmPageList* list);
void rust_vm_page_list_return_empty_slot(VmPageList* list, uint64_t offset);
void rust_vm_page_list_remove_content(VmPageList* list, uint64_t offset, VmPageOrMarker* out);
bool rust_vm_page_list_is_offset_in_zero_interval(const VmPageList* list, uint64_t offset);
bool rust_vm_page_list_is_offset_in_interval(const VmPageList* list, uint64_t offset);
void rust_vm_page_list_return_interval_slot(VmPageList* list, uint64_t offset);
zx_status_t rust_vm_page_list_populate_slots_in_interval(VmPageList* list, uint64_t start_offset,
                                                         uint64_t end_offset);
zx_status_t rust_vm_page_list_clip_interval_start(VmPageList* list, uint64_t interval_start,
                                                  uint64_t len);
zx_status_t rust_vm_page_list_clip_interval_end(VmPageList* list, uint64_t interval_end,
                                                uint64_t len);
VmPageOrMarker* rust_vm_page_list_lookup_or_allocate_internal(VmPageList* list, uint64_t offset);
VmPageOrMarkerSlotAndBool rust_vm_page_list_lookup_or_allocate_check_for_interval(
    VmPageList* list, uint64_t offset, bool split_interval);
VmPageOrMarkerConstSlotAndOffset rust_vm_page_list_find_interval_start_for_end(
    const VmPageList* list, uint64_t end_offset);
VmPageOrMarkerConstSlotAndOffset rust_vm_page_list_find_interval_end_for_start(
    const VmPageList* list, uint64_t start_offset);
zx_status_t rust_vm_page_list_add_zero_interval_internal(VmPageList* list, uint64_t start_offset,
                                                         uint64_t end_offset, uint32_t dirty_state,
                                                         uint64_t awaiting_clean_len,
                                                         bool replace_existing_slot);
vm_page_t* rust_vm_page_list_replace_page_with_zero_interval(VmPageList* list, uint64_t offset,
                                                             uint32_t dirty_state);
zx_status_t rust_vm_page_list_overwrite_zero_interval(VmPageList* list, uint64_t old_start_offset,
                                                      uint64_t old_end_offset,
                                                      uint64_t new_start_offset,
                                                      uint64_t new_end_offset,
                                                      uint32_t new_dirty_state);
VmPageOrMarker* rust_vm_page_list_batch_inserter_lookup_or_allocate(
    VmPageList::BatchInserter* inserter, uint64_t offset);
}  // extern "C"

namespace {

DECLARE_SINGLETON_CRITICAL_MUTEX(SlabLock);
// Slab used for allocating all the page list nodes.
PageSlabAllocator<sizeof(VmPageListNode)> pln_slab_ TA_GUARDED(SlabLock::Get());

// Assert the size of the page list node to prevent accidental size changes.
static_assert(sizeof(VmPageListNode) == 64);
}  // namespace

void VmPageListNodeDeleter::operator()(VmPageListNode* node) {
  ktl::destroy_at(node);
  Guard<CriticalMutex> guard{SlabLock::Get()};
  pln_slab_.deallocate_bytes(node);
}

VmPlnOwner VmPageListNode::Create() {
  VmPageListNode* node;
  {
    Guard<CriticalMutex> guard{SlabLock::Get()};
    node = pln_slab_.allocate_object<VmPageListNode>();
  }
  if (!node) {
    return nullptr;
  }
  ktl::construct_at(node);
  return VmPlnOwner(node);
}

VmPageListNode::~VmPageListNode() { DEBUG_ASSERT(HasNoPageOrRef()); }

VmPageList::VmPageList() { LTRACEF("%p\n", this); }

VmPageList::VmPageList(VmPageList&& other) : list_(ktl::move(other.list_)) {
  LTRACEF("%p\n", this);
}

VmPageList::~VmPageList() {
  LTRACEF("%p\n", this);
  DEBUG_ASSERT(HasNoPageRefOrMarker());
}

VmPageList& VmPageList::operator=(VmPageList&& other) {
  list_ = ktl::move(other.list_);
  return *this;
}

VmPageOrMarker* VmPageList::LookupOrAllocateInternal(uint64_t offset) {
  return rust_vm_page_list_lookup_or_allocate_internal(this, offset);
}

VmPageOrMarker* VmPageList::BatchInserter::LookupOrAllocate(uint64_t offset) {
  return rust_vm_page_list_batch_inserter_lookup_or_allocate(this, offset);
}

void VmPageList::ReturnEmptySlot(uint64_t offset) {
  rust_vm_page_list_return_empty_slot(this, offset);
}

VmPageOrMarker VmPageList::RemoveContent(uint64_t offset) {
  VmPageOrMarker result;
  rust_vm_page_list_remove_content(this, offset, &result);
  return result;
}

bool VmPageList::HasNoPageOrRef() const { return rust_vm_page_list_has_no_page_or_ref(this); }

bool VmPageList::HasNoPageRefOrMarker() const {
  return rust_vm_page_list_has_no_page_ref_or_marker(this);
}

ktl::pair<const VmPageOrMarker*, uint64_t> VmPageList::FindIntervalStartForEnd(
    uint64_t end_offset) const {
  auto [slot, start_offset] = rust_vm_page_list_find_interval_start_for_end(this, end_offset);
  return {slot, start_offset};
}

ktl::pair<const VmPageOrMarker*, uint64_t> VmPageList::FindIntervalEndForStart(
    uint64_t start_offset) const {
  auto [slot, end_offset] = rust_vm_page_list_find_interval_end_for_start(this, start_offset);
  return {slot, end_offset};
}

ktl::pair<VmPageOrMarker*, bool> VmPageList::LookupOrAllocateCheckForInterval(uint64_t offset,
                                                                              bool split_interval) {
  auto [slot, is_interval] =
      rust_vm_page_list_lookup_or_allocate_check_for_interval(this, offset, split_interval);
  return {slot, is_interval};
}

void VmPageList::ReturnIntervalSlot(uint64_t offset) {
  rust_vm_page_list_return_interval_slot(this, offset);
}

zx_status_t VmPageList::PopulateSlotsInInterval(uint64_t start_offset, uint64_t end_offset) {
  return rust_vm_page_list_populate_slots_in_interval(this, start_offset, end_offset);
}

bool VmPageList::IsOffsetInZeroInterval(uint64_t offset) const {
  return rust_vm_page_list_is_offset_in_zero_interval(this, offset);
}

bool VmPageList::IsOffsetInInterval(uint64_t offset) const {
  return rust_vm_page_list_is_offset_in_interval(this, offset);
}

zx_status_t VmPageList::AddZeroIntervalInternal(uint64_t start_offset, uint64_t end_offset,
                                                VmPageOrMarker::IntervalDirtyState dirty_state,
                                                uint64_t awaiting_clean_len,
                                                bool replace_existing_slot) {
  return rust_vm_page_list_add_zero_interval_internal(this, start_offset, end_offset,
                                                      static_cast<uint32_t>(dirty_state),
                                                      awaiting_clean_len, replace_existing_slot);
}

vm_page_t* VmPageList::ReplacePageWithZeroInterval(uint64_t offset,
                                                   VmPageOrMarker::IntervalDirtyState dirty_state) {
  return rust_vm_page_list_replace_page_with_zero_interval(this, offset,
                                                           static_cast<uint32_t>(dirty_state));
}

zx_status_t VmPageList::OverwriteZeroInterval(uint64_t old_start_offset, uint64_t old_end_offset,
                                              uint64_t new_start_offset, uint64_t new_end_offset,
                                              VmPageOrMarker::IntervalDirtyState new_dirty_state) {
  return rust_vm_page_list_overwrite_zero_interval(this, old_start_offset, old_end_offset,
                                                   new_start_offset, new_end_offset,
                                                   static_cast<uint32_t>(new_dirty_state));
}

zx_status_t VmPageList::ClipIntervalStart(uint64_t interval_start, uint64_t len) {
  return rust_vm_page_list_clip_interval_start(this, interval_start, len);
}

zx_status_t VmPageList::ClipIntervalEnd(uint64_t interval_end, uint64_t len) {
  return rust_vm_page_list_clip_interval_end(this, interval_end, len);
}

// Trampolines into the Rust VmPageSpliceList implementation, which likewise shares the layout of
// the C++ class, so `this` is passed through directly. The contracts of these functions are
// documented at their definitions in vm_page_list.rs.
extern "C" {
void rust_vm_page_splice_list_destroy(VmPageSpliceList* list);
zx_status_t rust_vm_page_splice_list_insert(VmPageSpliceList* list, uint64_t offset,
                                            VmPageOrMarker* content);
void rust_vm_page_splice_list_pop(VmPageSpliceList* list, VmPageOrMarker* out);
}  // extern "C"

VmPageSpliceList::~VmPageSpliceList() { rust_vm_page_splice_list_destroy(this); }

zx_status_t VmPageSpliceList::Insert(uint64_t offset, VmPageOrMarker content) {
  return rust_vm_page_splice_list_insert(this, offset, &content);
}

VmPageOrMarker VmPageSpliceList::Pop() {
  VmPageOrMarker res;
  rust_vm_page_splice_list_pop(this, &res);
  return res;
}
