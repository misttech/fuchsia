// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT
#ifndef ZIRCON_KERNEL_LIB_BTREE_INCLUDE_LIB_BTREE_NODE_H_
#define ZIRCON_KERNEL_LIB_BTREE_INCLUDE_LIB_BTREE_NODE_H_

#include <lib/btree_util.h>

#include <algorithm>
#include <array>
#include <bit>

#include <fbl/packed_pointer.h>

namespace btree {

// BTree node implementation.
//
// This is a B-tree variant that resembles a B+tree in that all values are stored in the leaf
// nodes. Intermediate nodes store keys that represent the *minimum* possible key for each of their
// child subtrees.
//
// A key property of this representation is that the first key (at index 0) of every non-leaf node
// must be 0 (or more generally, the minimum possible key value). This 0 key effectively represents
// -infinity and ensures that any key less than the first explicit key in the second slot will
// correctly traverse into the first child. This differs from standard B-trees where keys are stored
// 'between' child pointers.
//
// Because intermediate nodes store minimum keys rather than exact keys, symmetric lower_bound
// operation across both leaf and intermediate nodes is not possible. Search operations therefore
// always use upper_bound to find the correct subtree.
template <typename NodeSizeParams, TreeValidation Validator>
class Node {
 public:
  // All node constructors need to be told what size they are and whether or not they are a leaf
  // type. Additional constructors also take various initial items or nodes to copy from.
  Node(uint32_t size_class, bool leaf) : Node(size_class, nullptr, nullptr, leaf, 0) {}
  Node(uint32_t size_class, Node&& node, Item item, uint32_t index)
      : Node(size_class, node.prev(), node.next(), node.is_leaf(), node.count() + 1) {
    BTREE_CHECK(size_class > node.size_class());
    copy_insert(0, index, item, node, 0, node.count());
  }
  Node(uint32_t size_class, bool leaf, Item item) : Node(size_class, nullptr, nullptr, leaf, 1) {
    items_[0] = item;
  }
  Node(uint32_t size_class, bool leaf, Item item1, Item item2)
      : Node(size_class, nullptr, nullptr, leaf, 2) {
    items_[0] = item1;
    items_[1] = item2;
  }

  ~Node() = default;

  Node() = delete;
  Node(const Node&) = delete;
  Node(Node&&) = delete;
  Node& operator=(const Node&) = delete;
  Node& operator=(Node&&) = delete;

  bool is_full() const { return count() == max_count(); }
  bool is_empty() const { return count() == 0; }

  bool valid_index(uint32_t index) const { return index < count(); }

  Item get(uint32_t index) const {
    BTREE_CHECK(valid_index(index));
    return items_[index];
  }

  // Updates the key at the given index. It is the callers responsibility to ensure this results in
  // a valid tree.
  void update_key(uint32_t index, uint64_t key) {
    ZX_DEBUG_ASSERT(valid_index(index) && (index == 0 || items_[index - 1].key < key) &&
                    (index == count() - 1 || key < items_[index + 1].key));
    items_[index].key = key;
  }

  // Returns the index of the upper bound of key, or count() if key is beyond this node. Due to how
  // intermediate nodes are represented there is no equivalent lower_bound operation as it does not
  // have a symmetric implementation for both intermediate and leaf nodes.
  uint32_t upper_bound(uint64_t key) {
    const uint32_t c = count();
    for (uint32_t i = 0; i < c; i++) {
      if (key < items_[i].key) {
        return i;
      }
    }
    return c;
  }

  // Inserts an item at the specified index, which must either be a valid_index, or at the end of
  // the valid indexes and shifts the rest of the items up and updates the count.
  void insert(uint32_t index, Item item) {
    // This will not catch all duplicate key insertions, since the duplicate key could be in a
    // neighboring (full) node, but should catch most accidents.
    ZX_DEBUG_ASSERT((index == 0 || items_[index - 1].key < item.key) &&
                    (index == count() || item.key < items_[index].key));
    expand_at(index, 1);
    items_[index] = item;
  }

  // Erase the range items, shifting down the remainder and updating the count.
  void erase(uint32_t index, uint32_t amount) {
    const uint32_t c = count();
    BTREE_CHECK(amount > 0 && (index + amount) <= c);
    move_down(index, index + amount, c - (index + amount));
    set_count(c - amount);
  }

  // Rebalance this node with the right sibling by shifting items to the right to make space for
  // an insertion at node_insert_index.
  void insert_rebalance_right(Node* __restrict right, uint32_t node_insert_index, Item item,
                              iterator_base<Node>* fixup) __restrict {
    const uint32_t node_count = count();
    const uint32_t right_count = right->count();
    BTREE_CHECK(node_count == kMaxValues);
    BTREE_CHECK(right_count < kMaxValues);
    ZX_DEBUG_ASSERT((node_insert_index == 0 || items_[node_insert_index - 1].key < item.key) &&
                    (node_insert_index == node_count || item.key < items_[node_insert_index].key));
    ZX_DEBUG_ASSERT(right_count == 0 || items_[node_count - 1].key < right->items_[0].key ||
                    (node_insert_index == node_count && item.key < right->items_[0].key));

    const uint32_t total_items = node_count + right_count + 1;
    const uint32_t target_right_count = total_items / 2;
    const uint32_t items_to_move = target_right_count - right_count;
    BTREE_CHECK(items_to_move > 0);

    // Make space at the beginning of the right sibling.
    right->expand_at(0, items_to_move);

    // split_index is the index in the original node (plus the new item) where we split.
    // Items at and after split_index go to the right sibling.
    const uint32_t split_index = total_items - target_right_count;

    if (node_insert_index >= split_index) {
      // New item goes into the right sibling.
      const uint32_t right_insert_index = node_insert_index - split_index;
      const uint32_t num_from_this = items_to_move - 1;
      const uint32_t src_start_index = node_count - num_from_this;

      right->copy_insert(0, right_insert_index, item, *this, src_start_index, num_from_this);
      set_count(src_start_index);

      if (fixup) {
        BTREE_CHECK(fixup->node_ == this && fixup->index_ == node_insert_index);
        *fixup = iterator_base(right, right_insert_index);
      }
    } else {
      // New item stays in this node.
      const uint32_t src_start_index = node_count - items_to_move;
      right->copy(0, *this, src_start_index, items_to_move);
      set_count(src_start_index);
      insert(node_insert_index, item);
      BTREE_CHECK(!fixup || (fixup->node_ == this && fixup->index_ == node_insert_index));
    }
  }

  // Rebalance this node with the left sibling by shifting items to the left to make space for
  // an insertion at node_insert_index.
  void insert_rebalance_left(Node* __restrict left, uint32_t node_insert_index, Item item,
                             iterator_base<Node>* fixup) __restrict {
    const uint32_t node_count = count();
    const uint32_t left_count = left->count();
    BTREE_CHECK(node_count == kMaxValues);
    BTREE_CHECK(left_count < kMaxValues);
    ZX_DEBUG_ASSERT((node_insert_index == 0 || items_[node_insert_index - 1].key < item.key) &&
                    (node_insert_index == node_count || item.key < items_[node_insert_index].key));
    ZX_DEBUG_ASSERT(left_count == 0 || left->items_[left_count - 1].key < items_[0].key ||
                    (node_insert_index == 0 && left->items_[left_count - 1].key < item.key));

    const uint32_t total_items = node_count + left_count + 1;
    const uint32_t target_left_count = total_items / 2;
    const uint32_t items_to_move = target_left_count - left_count;
    BTREE_CHECK(items_to_move > 0);

    if (node_insert_index < items_to_move) {
      // New item goes into the left sibling.
      const uint32_t left_insert_index = left_count + node_insert_index;
      const uint32_t num_from_this = items_to_move - 1;

      left->set_count(target_left_count);
      left->copy_insert(left_count, left_insert_index, item, *this, 0, num_from_this);
      if (num_from_this > 0) {
        erase(0, num_from_this);
      }

      if (fixup) {
        BTREE_CHECK(fixup->node_ == this && fixup->index_ == node_insert_index);
        *fixup = iterator_base(left, left_insert_index);
      }
    } else {
      // New item stays in this node.
      left->set_count(target_left_count);
      left->copy(left_count, *this, 0, items_to_move);
      erase(0, items_to_move);
      insert(node_insert_index - items_to_move, item);

      if (fixup) {
        BTREE_CHECK(fixup->node_ == this && fixup->index_ == node_insert_index);
        fixup->index_ = node_insert_index - items_to_move;
      }
    }
  }

  // Pushes a single item onto the end of the node.
  void push(Item item) {
    BTREE_CHECK(!is_full());
    const uint32_t c = count();
    ZX_DEBUG_ASSERT(c == 0 || items_[c - 1].key < item.key);
    items_[c] = item;
    set_count(c + 1);
  }

  // Moves all the items from |other| onto the end of |this|.
  void merge_from(Node& __restrict other) __restrict {
    const uint32_t c = count();
    const uint32_t oc = other.count();
    ZX_DEBUG_ASSERT(c == 0 || oc == 0 || items_[c - 1].key < other.items_[0].key);
    copy(c, other, 0, oc);
    set_count(c + oc);
    other.set_count(0);
  }

  // Moves |num| items from the start of this node onto the end of |left|.
  void rotate_left(Node* __restrict left, uint32_t num) __restrict {
    const uint32_t lc = left->count();
    left->copy(lc, *this, 0, num);
    left->set_count(lc + num);
    erase(0, num);
  }

  // Moves as many items as possible from the start of this node onto the end of |left|.
  void rotate_left_max(Node* __restrict left) __restrict {
    const uint32_t available = left->max_count() - left->count();
    BTREE_CHECK(available > 0);
    rotate_left(left, std::min(available, count()));
  }

  // Moves |num| items from the end of this node into the start of |right|.
  void rotate_right(Node* __restrict right, uint32_t num) __restrict {
    const uint32_t c = count();
    right->expand_at(0, num);
    right->copy(0, *this, c - num, num);
    set_count(c - num);
  }

  Node* prev() const { return prev_.ptr(); }
  Node* next() const { return next_.ptr(); }

  void set_next(Node* next) { next_.set_ptr(next); }
  void set_prev(Node* prev) { prev_.set_ptr(prev); }

  size_t size_bytes() const { return NodeSizeParams::kSizeClasses[size_class()]; }
  uint32_t size_class() const { return prev_.data() & kSizeClassMask; }

  // Number of bits needed to represent the size class.
  static constexpr size_t kSizeClassSizeBits =
      std::bit_width(std::size(NodeSizeParams::kSizeClasses));
  static constexpr size_t kSizeClassMask = (1ul << kSizeClassSizeBits) - 1;
  // Use the next bit as the 'is_leaf' flag. This is the 0-based index of the leaf bit.
  static constexpr size_t kIsLeafBit = kSizeClassSizeBits;
  static constexpr size_t kIsLeafMask = (1ul << kIsLeafBit);

  // The size class and is_leaf flag are stored with the prev_ pointer and must fit into the
  // pointer to every size class, i.e. at least a pointer to the smallest class.
  static constexpr size_t kPrevDataBits = kIsLeafBit + 1;
  static_assert((1ul << kPrevDataBits) <= NodeSizeParams::kSizeClasses[0]);

  // Calculates our maximum count given a size in bytes.
  static constexpr uint32_t MaxCountFromSize(size_t bytes) {
    static_assert(sizeof(Item) == sizeof(uint64_t) * 2);
    return static_cast<uint32_t>((bytes / sizeof(uint64_t) / 2) - 1);
  }
  // Given a size class (index into kSizeClasses) returns the maximum count.
  static constexpr uint32_t MaxCountFromClass(uint32_t size_class) {
    return MaxCountFromSize(NodeSizeParams::kSizeClasses[size_class]);
  }

  // Similarly the maximum count must fit into a pointer to every size class. This requirement
  // avoids having to dynamically size the mask of the count based on the current size size class.
  static constexpr uint32_t kMaxValues =
      MaxCountFromClass((std::size(NodeSizeParams::kSizeClasses) - 1));
  static constexpr size_t kNextDataBits = std::bit_width(kMaxValues);
  static_assert((1ul << kNextDataBits) <= NodeSizeParams::kSizeClasses[0]);

  // The target min values only applies to non-root nodes, and so is derived from kMaxValues.
  static constexpr uint32_t kTargetMinValues = kMaxValues / 2;

  bool is_leaf() const { return prev_.data() & kIsLeafMask; }
  uint32_t max_count() const { return MaxCountFromClass(size_class()); }
  uint32_t count() const { return static_cast<uint32_t>(next_.data()); }
  void set_count(uint32_t count) {
    BTREE_CHECK(count <= max_count());
    next_.set_data(count);
  }

 private:
  // All other constructors route here. The size_class indirectly tells us how large the items_
  // array is by informing us of the total size of the allocation we are situated in. This is stored
  // with the prev_ pointer along with whether or not this is a leaf node.
  Node(uint32_t size_class, Node* prev, Node* next, bool is_leaf, size_t initial_count)
      : prev_(prev, size_class | (is_leaf ? kIsLeafMask : 0)), next_(next, initial_count) {}

  // Creates |amount| uninitialized slots at |index| by shifting up the existing items. Also updates
  // the count. Caller is responsible for filling in the slots.
  void expand_at(uint32_t index, uint32_t amount) {
    const uint32_t c = count();
    BTREE_CHECK(index <= c);
    std::move_backward(items_ + index, items_ + c, items_ + c + amount);
    set_count(c + amount);
  }

  // Shift a range of slots down. This does not update the count and will overwrite target slots and
  // leave src slots as duplicated. Caller is responsible for updating the src slots with new items.
  void move_down(uint32_t dst_index, uint32_t src_index, uint32_t amount) {
    BTREE_CHECK(dst_index < src_index);
    BTREE_CHECK(src_index + amount <= count());
    std::move(items_ + src_index, items_ + src_index + amount, items_ + dst_index);
  }

  void copy(uint32_t dst_index, Node& __restrict src, uint32_t src_index,
            uint32_t count) __restrict {
    BTREE_CHECK(&src != this);
    BTREE_CHECK(dst_index + count <= max_count());
    BTREE_CHECK(src_index + count <= src.max_count());

    std::copy(src.items_ + src_index, src.items_ + src_index + count, items_ + dst_index);
  }

  void copy_insert(uint32_t dst_index, uint32_t dst_insert_index, Item insert_item,
                   Node& __restrict src, uint32_t src_index, uint32_t copy_count) __restrict {
    copy(dst_index, src, src_index, dst_insert_index - dst_index);
    items_[dst_insert_index] = insert_item;
    copy(dst_insert_index + 1, src, src_index + (dst_insert_index - dst_index),
         copy_count - (dst_insert_index - dst_index));
  }

  // TODO(https://fxbug.dev/494059275): The node knows if it is a leaf node or not and could avoid
  // using this extraneous left most key, avoiding the need to keep it consistent. Actually reusing
  // that memory to store an additional pointer in intermediate nodes would be more challenging.

  fbl::PackedPointer<Node, kPrevDataBits, false> prev_;
  fbl::PackedPointer<Node, kNextDataBits, false> next_;

  // The Node will be allocated in block of memory of varying sizes, which determines how many
  // items_ we are storing and so a flexible array memory is used to support this dynamism. The size
  // of this array can be inferred from the |size_class| passed into the constructor, which gets
  // stored as part of the packed data in the prev_ pointer (this is what the max_count helper
  // does).
  // |Item| itself must be trivially constructible and destructible as the items are implicitly
  // constructed via assignment when growing the number of active elements in the node, and are
  // implicitly destructed when the node goes away.
  static_assert(std::is_trivially_constructible_v<Item>);
  static_assert(std::is_trivially_destructible_v<Item>);
  Item items_[];
};

}  // namespace btree

#endif  // ZIRCON_KERNEL_LIB_BTREE_INCLUDE_LIB_BTREE_NODE_H_
