// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT
#ifndef ZIRCON_KERNEL_LIB_BTREE_INCLUDE_LIB_BTREE_H_
#define ZIRCON_KERNEL_LIB_BTREE_INCLUDE_LIB_BTREE_H_

#include <lib/btree_node.h>
#include <lib/btree_util.h>
#include <stdio.h>
#include <zircon/types.h>

// # Design Notes
//
// BTree<> is an implementation of a variant of a b*+tree that aims to balance high density and
// operational simplicity. Btrees are are an associative container that support all the typical
// key-centric operations. Unlike, for example, a hashtable, keys in a btree are ordered and can be
// navigated between in constant time. The BTree<> is explicitly *not* intrusive, and as consequence
// key insertion may perform allocations, and may therefore fail. As a reward the BTree<> has very
// good cache locality making it quite efficient in practice to perform searches on.
//
// Like a typical b+tree data is only stored in leaf nodes, and all leaf nodes are the same depth
// from the root. Unlike a common b-tree, intermediate nodes store a key per child node, instead of
// storing a key 'between' each child node. This is a current implementation simplification (see
// comments on the Node structure).
//
// Like a b*tree, nodes are re-balanced where possible on insertion and deletion to improve density.
// Due to simplifications in the rebalancing process, as well as optimizations for tail insertion
// and head deletion, strict occupancy minimums are not observed, and nodes are allowed to be
// underfull (i.e. have less than half occupancy) at times.
//
// Some additional implementation choices made in the BTree<> are:
//  * Leaf nodes are, as is common in b+tree variants, placed in their own linked list for efficient
//    sequential traversal.
//  * Root node is variable size, both when it is a leaf and intermediate node, allowing for
//    efficiently storing small numbers of items.
//  * The left most and right most leaf nodes are stored in the root_ node (unless the root_ node
//    is the leaf node). This minimizes the size of the BTree<> structure itself, whilst still
//    allowing efficient begin() and end() traversals.
//
// A fundamental constraint of this implementation is that Nodes *must* be power of 2 size and size
// aligned in memory as the low bits of their pointers are used for storing extra data. This
// constraint is not considered onerous as an efficient implementation already would want Nodes to
// be a cache line multiple in size and cache line aligned to minimize false sharing.
//
// TODO(https://fxbug.dev/494059275): Remove some of these constraints.
// Some non-fundamental constraints that are in place until a use case needs to remove them are:
//  * Keys and values are both exactly 64-bits in size.
//  * Keys are a plain uint64_t
//  * Values are either a managed pointer (unique_ptr / RefPtr), raw pointer or a plain uint64_t.
//  * Node size classes are fixed.
//  * Key searching within a node is always linear and never a binary search.
//  * Maximum tree depth is presently fixed at 8.
//
// # Implementation details
//
// The BTree container is optimized to be as small as possible, aiming to occupy only a single
// machine word which stores a pointer to the root node.
//
// ## Node Layout and Metadata Packing
//
// Nodes store a set of key-value pairs (Items) based on their size class. To conserve space the
// node's size class, its current item count and whether it is a leaf node or not is packed into the
// lower bits of the 'prev' and 'next' pointers. This packing is possible because nodes are required
// to be power-of-two sized and aligned.
//
// Notably absent are any parent pointers. While a node can directly find its children, a node can
// only find its parent by walking from the root.
//
// ## Pointer Semantics
//
// The 'prev' and 'next' pointers serve different roles based on the node's type:
//  * Leaf Nodes: These pointers form a doubly-linked list across all leaf nodes in the tree,
//    allowing for O(1) sequential traversal in both directions.
//  * Non-Leaf Root Node: The pointers are used to store direct links to the leftmost and
//    rightmost leaf nodes. This allows `begin()` and `end()` to be implemented in constant time
//    by bypassing the tree traversal.
//  * Intermediate Nodes: These pointers are currently not used for navigation.
//
// ## Variable-Sized Root
//
// To improve memory efficiency for small collections, the root node is variable-sized. It grows
// through reallocation as items are added until it reaches the maximum size class. If the tree
// depth increases, the new root node is again initialized at the smallest size class (on depth
// growth the new root will always contain two items, the old root and its new sibling).
//
// All non-root nodes (both leaf and intermediate) are allocated at the maximum size class. This
// simplifies balancing and merging logic as it can be assumed that any time this is happening any
// nodes involved will be of the same (maximum) size.
//
// ## Operational Optimizations
//
// The implementation includes several optimizations that prioritize performance and density over
// strict B-tree invariants:
//  * Tail Insertion and Head Erasure: When the tree detects strictly ascending insertions or
//    sequential deletions from the front, it skips standard node rebalancing. For tail insertions,
//    this allows nodes to be filled to 100% capacity without the overhead of shifting items,
//    maintaining high density for common sequential workloads.
//  * Hinted Insertion: Providing an iterator hint allows the tree to attempt insertion
//    directly into a non-full leaf node, potentially avoiding a full tree walk entirely.
//
// ## Depth Limitation
//
// For simplicity and to avoid dynamic memory usage for path tracking, the tree depth is capped at
// a fixed constant (currently 8). This depth is sufficient to hold a vast number of items given
// the branching factor of the nodes. A btree, in the kernel, that would exceed this branching
// factor is, at least for all foreseeable use cases, just using too much kernel memory.
//
// ## Validation
//
// To aid debugging and development some optional validation strategies are supported as templates
// on the root BTree class. These are provided as templates to allow tests to always enable
// validation, and different btree instantiations to optionally use them depending on what they are
// doing. For example, a new usage of the BTree might want iterator validation turned out during
// development, but this does not require other BTree users to need to have it on. The two types of
// validation are.
//  * BTREE_CHECK: These are controlled by the Validator template and are either noops or expand to
//    an assertion. Intended for use when performing development on the btree itself, these perform
//    expensive validation and checks that are not suitable for regular assertions.
//  * IteratorValidation: When set to ::Tracked this causes the main BTree to maintain a
//    modification generation, and iterators to retain an additional pointer back to the btree and
//    the generation they were created with. All of this allows any usage of stale iterators, even
//    benign ones, to be caught with assertion failures.

namespace btree {

template <typename ValueType, typename Allocator = GlobalSlabAllocator,
          typename Traits = DefaultTypeTraits<ValueType>,
          IteratorValidation Validation = IteratorValidation::Untracked,
          TreeValidation Validator = TreeValidation::None>
class BTree {
 public:
  using ContainerType = BTree<ValueType, Allocator, Traits, Validation, Validator>;
  using NodeSizeParams = DefaultNodeSizeParams;
  using TreeNode = Node<NodeSizeParams, Validator>;
  using Path = PathTracker<TreeNode>;

  template <typename>
  class iterator_impl;
  using iterator = iterator_impl<typename Traits::RawType>;
  using const_iterator = iterator_impl<typename Traits::ConstRawType>;

  explicit BTree(Allocator allocator) : allocator_(std::move(allocator)) {}
  BTree() = default;
  ~BTree() {
    if (!is_empty()) {
      clear();
    }
  }
  BTree(const BTree&) = delete;
  BTree(BTree&& other) : allocator_(std::move(other.allocator_)) {
    root_ = other.root_;
    other.root_ = nullptr;
    other.generation_++;
  }
  BTree& operator=(const BTree&) = delete;
  BTree& operator=(BTree&& other) {
    if (!is_empty()) {
      clear();
    }
    generation_++;
    root_ = other.root_;
    allocator_ = std::move(other.allocator_);
    other.generation_++;
    other.root_ = nullptr;
    return *this;
  }

  bool is_empty() const { return !root_; }

  const_iterator begin() const { return const_iterator(this, left_most_leaf(), 0); }
  iterator begin() { return iterator(this, left_most_leaf(), 0); }
  const_iterator end() const {
    TreeNode* node = right_most_leaf();
    if (node) {
      return const_iterator(this, node, TreeNode::kMaxValues);
    }
    return const_iterator(this);
  }
  iterator end() {
    TreeNode* node = right_most_leaf();
    if (node) {
      return iterator(this, node, TreeNode::kMaxValues);
    }
    return iterator(this);
  }

  // Inserts the provided value using the specified key. It is an error to insert a duplicate key
  // and attempting to do so will either cause a runtime assertion failure or datastructure
  // corruption.
  //
  // If internal nodes needed to, but could not be, allocated then the end() iterator is returned,
  // otherwise an iterator to the new item is returned. If end() is returned, and tree is holding
  // managed pointers, then the managed pointer is released and not returned. In all cases if end()
  // is returned the tree is left in an unmodified state.
  //
  // Insert takes an option |hint| iterator to an item to insert near. The provided iterator can be
  // to an invalid location (i.e. end()), but may not be invalid due to becoming stale from insert
  // or erase.
  //
  // After an insertion all iterators, except the one returned, are stale and must not be used.
  iterator insert(uint64_t key, ValueType&& value) __WARN_UNUSED_RESULT {
    generation_++;
    typename Traits::RawType raw = Traits::Leak(value);
    iterator ret =
        iterator(this, insert_internal(Item{.key = key, .value = std::bit_cast<uint64_t>(raw)}));
    if (unlikely(!ret.IsValid())) {
      Traits::Reclaim(raw);
    } else {
      BTREE_CHECK(ret == find(key));
    }
    return ret;
  }
  iterator insert(iterator hint, uint64_t key, ValueType&& value) __WARN_UNUSED_RESULT {
    // Invalid iterators are allowed for the hint, but if valid then the generation should be valid.
    // Calling IsValid, and discarding the return result, serves to check the generation if tracking
    // is enabled.
    hint.IsValid();
    // As duplicate keys are not allowed, if the iterator is valid it shouldn't be to something with
    // the same key.
    ZX_DEBUG_ASSERT(!hint.IsValid() || (*hint).first != key);
    generation_++;
    typename Traits::RawType raw = Traits::Leak(value);
    iterator ret = iterator(
        this, insert_hint_internal(hint, Item{.key = key, .value = std::bit_cast<uint64_t>(raw)}));
    if (unlikely(!ret.IsValid())) {
      Traits::Reclaim(raw);
    } else {
      BTREE_CHECK(ret == find(key));
    }
    return ret;
  }
  iterator insert(uint64_t key, const ValueType& value) __WARN_UNUSED_RESULT {
    return insert(key, ValueType(value));
  }
  iterator insert(iterator hint, uint64_t key, const ValueType& value) __WARN_UNUSED_RESULT {
    return insert(hint, key, ValueType(value));
  }
  // Removes the item (key/value pair) referenced by the iterator. If storing managed pointers the
  // item is released.
  //
  // Returns an iterator to the item following |iter|.
  //
  // After an erase all iterators, except the one returned, are stale and must not be used.
  iterator erase(iterator iter) {
    auto [k, v] = iter.get();
    generation_++;
    Traits::Reclaim(std::bit_cast<typename Traits::RawType>(v));
    iter = iterator(this, erase_internal(iter));
    BTREE_CHECK(iter == upper_bound(k));
    return iter;
  }
  // Similar to |erase|, but returns ownership of the stored item.
  std::pair<uint64_t, ValueType> take(iterator iter) {
    auto [k, v] = iter.get();
    generation_++;
    iter = iterator(this, erase_internal(iter));
    BTREE_CHECK(iter == upper_bound(k));
    return std::make_pair(k, Traits::Reclaim(std::bit_cast<typename Traits::RawType>(v)));
  }
  // Return an iterator to the first item whose key is strictly greater than |key|, or end() if no
  // such item.
  const_iterator upper_bound(uint64_t key) const {
    if (!root_) {
      return const_iterator(this);
    }
    const_iterator it(this, upper_bound_slot_internal(key, nullptr, nullptr));
    // upper_bound_slot_internal may return an invalid index to a leaf node, in which case need to
    // update to make it valid. See the helper method for more details.
    it.wrap_to_next();
    return it;
  }
  iterator upper_bound(uint64_t key) {
    const_iterator citer = static_cast<const ContainerType*>(this)->upper_bound(key);
    return iterator(std::move(citer));
  }

  // Return an iterator to the first item whose key is greater than or equal to |key|, or end() if
  // no such item.
  const_iterator lower_bound(uint64_t key) const {
    if (!root_) {
      return const_iterator(this);
    }

    const_iterator it(this, upper_bound_slot_internal(key, nullptr, nullptr));
    // In the case where the key exists we may have overshot with upperbound, so check the previous
    // slot (if it exists).
    it--;
    if (!it.IsValid() || (*it).first < key) {
      it++;
    }
    return it;
  }

  iterator lower_bound(uint64_t key) {
    const_iterator citer = static_cast<const ContainerType*>(this)->lower_bound(key);
    return iterator(std::move(citer));
  }

  // Return an iterator to the item whose key is exactly |key|, or end() if no such item.
  const_iterator find(uint64_t key) const {
    // Could inline lower_bound and make some extremely trivial micro-optimizations, but these are
    // things like allowing end() to know that right_leaf would not be null etc and not worthwhile.
    const_iterator it = lower_bound(key);
    if (it && (*it).first == key) {
      return it;
    }
    return end();
  }
  iterator find(uint64_t key) {
    const_iterator citer = static_cast<const ContainerType*>(this)->find(key);
    return iterator(std::move(citer));
  }

  // Clears all content from the tree, returning it to the empty state. Any managed pointers will be
  // released and all iterators are invalidated.
  void clear();

  struct Utilization {
    // The size class of the root node. Uses a signed type because -1 is used
    // to indicate that the tree is empty and has no root node.
    int32_t root_size_class;
    uint32_t num_non_root_nodes;
    size_t stored_values;

    // Helper to calculate, in bytes, the total size of all nodes in the tree. This serves to
    // represent all outstanding allocations against the Allocator interface.
    uint64_t nodes_in_bytes() const {
      uint64_t bytes = 0;
      if (root_size_class >= 0) {
        bytes += NodeSizeParams::kSizeClasses[root_size_class];
      }
      bytes += static_cast<uint64_t>(num_non_root_nodes) *
               NodeSizeParams::kSizeClasses[(std::size(NodeSizeParams::kSizeClasses) - 1)];
      return bytes;
    }
  };
  // Walks the tree and counts how many nodes there are and how utilized they are. This method is
  // essentially O(N) as all nodes must be walked.
  // TODO(https://fxbug.dev/494059275): Provide a template option to select between persistently
  // storing this utilization, at the cost of increasing the storage of the BTree class, and having
  // this be a slow method.
  Utilization calculate_utilization_slow() const;

  // Print a representation of the tree to stdout. Is implemented via recursion and may use
  // arbitrary stack.
  void dump() const { dump(root_, 0); }

  // Debug helper that checks if a tree is valid, intended for use in unittests and/or during
  // development. Is implemented using recursion and will only return false if using a
  // TreeValidation::None, otherwise it will trigger an assertion failure on an invalid tree.
  bool debug_validate_tree() const;

  template <typename RefType>
  class iterator_impl : private iterator_base<TreeNode> {
   public:
    bool operator==(const iterator_impl& right) const {
      return this->node_ == right.node_ && this->index_ == right.index_;
    }
    std::pair<uint64_t, RefType> get() const {
      BTREE_CHECK(IsValid());
      generation_.validate();
      auto [key, value] = this->node_->get(this->index_);
      return std::make_pair(key, std::bit_cast<RefType>(value));
    }
    // Returns whether the iterator is one that can be dereferenced, i.e. is not the end() iterator,
    // a default constructed one, or begin() of an empty tree. If the iterator is stale due to
    // |insert| or |erase| having been called then the return value is undefined, and if iterator
    // validation is enabled this method counts as an access and will trigger an error.
    bool IsValid() const {
      if (!this->node_) {
        return false;
      }
      generation_.validate();
      return !this->end_sentinel();
    }
    iterator_impl& operator++() {
      if (this->node_) {
        generation_.validate();
        // In the case where this was the reverse end iterator (i.e. index_ was UINT32_MAX) then
        // this wraps it around to 0 making it the correct begin().
        this->index_++;
        this->wrap_to_next();
      }
      return *this;
    }
    iterator_impl operator++(int) {
      iterator_impl ret(*this);
      ++(*this);
      return ret;
    }
    iterator_impl& operator--() {
      if (this->node_) {
        generation_.validate();
        if (this->index_ == 0) {
          if (TreeNode* prev = this->node_->prev(); prev) {
            this->node_ = prev;
            this->index_ = this->node_->count();
          }
          // If no prev we intentionally allow index_ decrement and wrap around to UINT32_MAX to
          // indicate the reverse end iterator.
        } else if (this->index_ == TreeNode::kMaxValues) {
          this->index_ = this->node_->count();
        }
        this->index_--;
      }
      return *this;
    }
    iterator_impl operator--(int) {
      iterator_impl ret(*this);
      --(*this);
      return ret;
    }
    std::pair<uint64_t, RefType> operator*() const { return get(); }
    explicit operator bool() const { return IsValid(); }
    iterator_impl() = default;

    template <typename T>
    explicit iterator_impl(iterator_impl<T>&& other)
        : iterator_base<TreeNode>(other), generation_(other.generation_) {}
    template <typename T>
    explicit iterator_impl(const iterator_impl<T>& other)
        : iterator_base<TreeNode>(other), generation_(other.generation_) {}

   private:
    friend BTree;
    iterator_impl(const BTree* tree, TreeNode* node, uint32_t index)
        : iterator_base<TreeNode>(node, index), generation_(&tree->generation_) {}
    iterator_impl(const BTree* tree, const iterator_base<TreeNode>& other)
        : iterator_base<TreeNode>(other), generation_(&tree->generation_) {}
    iterator_impl(const BTree* tree, iterator_base<TreeNode>&& other)
        : iterator_base<TreeNode>(other), generation_(&tree->generation_) {}
    explicit iterator_impl(const BTree* tree) : generation_(&tree->generation_) {}
    void reset_generation() { generation_.reset(); }
    [[no_unique_address]] IteratorGeneration<Validation> generation_;
  };

 private:
  // If the root node exists, and is not a leaf node, then the otherwise unused left/right pointers
  // are used to store the left most and right most leaf nodes, of which helpers to retrieve are
  // provided here.
  TreeNode* left_most_leaf() const {
    if (!root_) {
      return nullptr;
    }
    if (root_->is_leaf()) {
      // Note that a non-null root implies 'this' is not a const object (we are just a const
      // reference to said object) and so a const_cast is legal.
      return const_cast<TreeNode*>(root_);
    }
    TreeNode* left = root_->prev();
    BTREE_CHECK(left && left->is_leaf() && !left->prev());
    return left;
  }
  TreeNode* right_most_leaf() const {
    if (!root_) {
      return nullptr;
    }
    if (root_->is_leaf()) {
      return const_cast<TreeNode*>(root_);
    }
    TreeNode* right = root_->next();
    BTREE_CHECK(right && right->is_leaf() && !right->next());
    return right;
  }
  TreeNode* left_node(TreeNode* node, uint32_t index) {
    BTREE_CHECK(!node->is_leaf());
    if (index == 0) {
      return nullptr;
    }
    return std::bit_cast<TreeNode*>(node->get(index - 1).value);
  }

  TreeNode* right_node(TreeNode* node, uint32_t index) {
    BTREE_CHECK(!node->is_leaf());
    if (!node->valid_index(index + 1)) {
      return nullptr;
    }
    return std::bit_cast<TreeNode*>(node->get(index + 1).value);
  }

  void empty_leaf(TreeNode* leaf) {
    for (uint32_t i = 0; i < leaf->count(); i++) {
      auto [k, v] = leaf->get(i);
      Traits::Reclaim(std::bit_cast<typename Traits::RawType>(v));
    }
    leaf->set_count(0);
  }

  // Helper that searches the tree for an upper bound target. Due to how intermediate nodes work
  // this will always return the node that key is either in, or should be inserted into. This can
  // result in an index that is one past the end of the items in the node (i.e. an invalid index)
  // and the caller is responsible for fixing / handling this.
  // Accepts an optional target node to cease traversing at (this allows finding the parent of
  // another node), and an optional PathTracker to record walk for more efficient parent finding.
  iterator_base<TreeNode> upper_bound_slot_internal(uint64_t key, TreeNode* target,
                                                    Path* path) const;

  void rotate_right(TreeNode* parent, uint32_t index, TreeNode* left, TreeNode* node,
                    uint32_t count) {
    left->rotate_right(node, count);
    parent->update_key(index, node->get(0).key);
  }
  void rotate_left(TreeNode* parent, uint32_t index, TreeNode* left, TreeNode* node,
                   uint32_t count) {
    node->rotate_left(left, count);
    parent->update_key(index, node->get(0).key);
  }
  bool should_rebalance(TreeNode* a, TreeNode* b) {
    return a->count() + b->count() >= TreeNode::kTargetMinValues * 2;
  }
  template <typename T>
  uint32_t rebalance_amount(T* above, T* below) {
    BTREE_CHECK(above->count() > below->count());
    return (above->count() - below->count()) / 2;
  }
  void reduce_root() {
    BTREE_CHECK(root_->count() == 1);
    TreeNode* next_root = reinterpret_cast<TreeNode*>(root_->get(0).value);
    // If reducing to another intermediate node then propagate down our knowledge of the left most
    // and right most leaf nodes.
    if (!next_root->is_leaf()) {
      next_root->set_prev(root_->prev());
      next_root->set_next(root_->next());
    }
    free_node(root_);
    root_ = next_root;
  }
  iterator_base<TreeNode> erase_internal(iterator_base<TreeNode> iter);
  // Insert a new leaf node into the leaf list to the right of an existing node.
  void insert_leaf_list(TreeNode* existing, TreeNode* new_right) {
    BTREE_CHECK(existing->is_leaf() && new_right->is_leaf());
    TreeNode* next = existing->next();
    new_right->set_next(next);
    existing->set_next(new_right);
    if (next) {
      next->set_prev(new_right);
    } else {
      // This is a new right most node, update the right most pointer in the root.
      root_->set_next(new_right);
    }
    new_right->set_prev(existing);
  }
  // Remove a leaf node from the leaf list. This needs to both update the immediate next/prev nodes
  // to point around the node being removed, but also update the left most and right most pointers
  // in the root_ node if this was either of those nodes.
  void erase_leaf_list(TreeNode* node) {
    BTREE_CHECK(node->is_leaf());
    if (TreeNode* next = node->next(); next) {
      next->set_prev(node->prev());
    } else {
      root_->set_next(node->prev());
    }
    if (TreeNode* prev = node->prev(); prev) {
      prev->set_next(node->next());
    } else {
      root_->set_prev(node->next());
    }
    node->set_next(nullptr);
    node->set_prev(nullptr);
  }
  iterator_base<TreeNode> insert_internal(Item item);

  iterator_base<TreeNode> insert_hint_internal(iterator_base<TreeNode> hint, Item item);
  void free_node(TreeNode* node) {
    const size_t size = node->size_bytes();
    std::destroy_at(node);
    allocator_.deallocate(size, node);
  }

  // Helper for dumping a tree to stdout.
  static void dump(TreeNode* node, uint32_t depth = 0) {
    if (!node) {
      return;
    }
    for (uint32_t i = 0; i < depth; i++) {
      printf("  ");
    }
    printf("%p (%u/%u)\n", node, node->count(), node->max_count());
    for (uint32_t i = 0; i < node->count(); i++) {
      auto [key, value] = node->get(i);
      for (uint32_t j = 0; j < depth; j++) {
        printf("  ");
      }
      printf("[%u:%lu]: %p\n", i, key, (void*)value);
      if (!node->is_leaf()) {
        dump(std::bit_cast<TreeNode*>(value), depth + 1);
      }
    }
  }

  // Debugging helper that checks if a given subtree is 'valid', i.e. has keys and its own subtrees
  // in sorted order.
  static bool subtree_valid(TreeNode* node, uint64_t lower_bound, uint64_t upper_bound);

  // The largest possible size class for the root node and the size class to allocate any non-root
  // node.
  static constexpr uint32_t kLargestNodeSizeClass = std::size(NodeSizeParams::kSizeClasses) - 1;

  // If the tree is empty the root_ is always a nullptr, otherwise root_ can point to one of:
  //  * A leaf node of varying size.
  //  * An intermediate node of varying size.
  // Whether the root_ is a leaf or intermediate node is known by querying root_->is_leaf(). Nodes
  // (intermediate or leaf) in the tree are never empty, and intermediate nodes always hold at least
  // two items (as holding one item is redundant). All non root nodes are of the same final (i.e.
  // largest) size.
  //
  // As this is a B+tree variant all leaf nodes are at the same 'depth' in the tree, and only leaf
  // nodes hold user supplied data. The actual depth of the tree is not recorded, and traversal
  // instead knows when it has found the leaf by checking is_leaf() on the node.
  //
  // The Nodes store a prev/next pointer that can take on different meanings. For leaf nodes, all
  // leaf nodes participate in being part of a doubly linked list through these pointers. This
  // allows for efficient forwards/backwards traversal of iterators.
  // For the root node, when it is not a leaf, the prev/next pointers are used to store the left
  // most and right most leaf nodes respectively. This allows for begin() and end() to not require
  // walking through the tree. Storing them in the root node, although requiring an extra
  // indirection, saves space.
  // Other (non root) intermediate nodes do not use the prev/next pointers.
  TreeNode* root_ = nullptr;
  [[no_unique_address]] Allocator allocator_;
  [[no_unique_address]] BTreeGeneration<Validation> generation_;
};

template <typename ValueType, typename Allocator, typename Traits, IteratorValidation Validation,
          TreeValidation Validator>
void BTree<ValueType, Allocator, Traits, Validation, Validator>::clear() {
  generation_++;

  if (!root_) {
    return;
  }
  if (root_->is_leaf()) {
    empty_leaf(root_);
    free_node(root_);
    root_ = nullptr;
    return;
  }

  // To avoid recursion we clear from right->left as this allows for gradually erasing elements
  // without needing to repeatedly shuffle the remaining values down each time one is erased.
  // Shuffling down would otherwise be necessary as there is no guarantee that the |path| does not
  // overflow, in which case we need the tree to be valid (enough) to perform a re-walk to find
  // our parent.
  TreeNode* cur = root_;
  __UNINITIALIZED Path path;
  while (true) {
    // Walk down and to the right until we found a leaf node.
    while (!cur->is_leaf()) {
      uint32_t index = cur->count() - 1;
      path.push({cur, index});
      cur = std::bit_cast<TreeNode*>(cur->get(index).value);
    }
    empty_leaf(cur);
    // Walk up erasing empty parents.
    do {
      if (cur == root_) {
        free_node(cur);
        root_ = nullptr;
        return;
      }

      auto [parent, parent_index] = path.pop();
      ZX_DEBUG_ASSERT(parent);
      BTREE_CHECK(cur->is_empty());
      free_node(cur);
      parent->erase(parent_index, 1);
      cur = parent;
    } while (cur->is_empty());
  }
}

template <typename ValueType, typename Allocator, typename Traits, IteratorValidation Validation,
          TreeValidation Validator>
bool BTree<ValueType, Allocator, Traits, Validation, Validator>::debug_validate_tree() const {
  if (!root_) {
    return true;
  }
  if (!root_->is_leaf() && root_->count() > 0) {
    if (root_->get(0).key != 0) {
      BTREE_CHECK(root_->get(0).key == 0);
      return false;
    }
  }
  return subtree_valid(root_, 0, UINT64_MAX);
}

template <typename ValueType, typename Allocator, typename Traits, IteratorValidation Validation,
          TreeValidation Validator>
iterator_base<typename BTree<ValueType, Allocator, Traits, Validation, Validator>::TreeNode>
BTree<ValueType, Allocator, Traits, Validation, Validator>::upper_bound_slot_internal(
    uint64_t key, TreeNode* target, Path* path) const {
  TreeNode* cur = root_;
  while (true) {
    uint32_t index = cur->upper_bound(key);
    if (cur == target || cur->is_leaf()) {
      return {cur, index};
    }
    index--;
    if (path) {
      path->push({cur, index});
    }
    cur = std::bit_cast<TreeNode*>(cur->get(index).value);
  }
}

template <typename ValueType, typename Allocator, typename Traits, IteratorValidation Validation,
          TreeValidation Validator>
iterator_base<typename BTree<ValueType, Allocator, Traits, Validation, Validator>::TreeNode>
BTree<ValueType, Allocator, Traits, Validation, Validator>::erase_internal(
    iterator_base<TreeNode> iter) {
  __UNINITIALIZED Path path;
  if (iter.node_ != root_) {
    [[maybe_unused]] iterator_base<TreeNode> self =
        upper_bound_slot_internal(iter.node_->get(iter.index_).key, iter.node_, &path);
    BTREE_CHECK(self.node_ == iter.node_);
  }

  TreeNode* node = iter.node_;
  uint32_t index = iter.index_;
  bool needs_fixup = true;

  while (true) {
    node->erase(index, 1);
    if (needs_fixup) {
      iter.wrap_to_next();
    }
    needs_fixup = false;

    // Root nodes do not underflow and merge, but can need removing.
    if (node == root_) {
      if (node->is_leaf()) {
        if (node->count() == 0) {
          BTREE_CHECK(iter.node_ == root_);
          // Tree is now empty.
          free_node(root_);
          root_ = nullptr;
          return end();
        }
        iter.wrap_to_next();
      } else if (node->count() == 1) {
        reduce_root();
      }
      return iter;
    }
    // If we haven't underflowed then can return.
    if (node->count() >= TreeNode::kTargetMinValues) {
      return iter;
    }

    // We need to either rebalance with a sibling or merge.
    auto [parent, parent_index] = path.pop();
    BTREE_CHECK(parent);
    TreeNode* left = left_node(parent, parent_index);
    TreeNode* right = right_node(parent, parent_index);

    // If the node is empty, we can remove it from the parent and continue erasing up the
    // tree. An empty node can occur when erasing from an underfull node created from optimized
    // tail insertion.
    if (node->count() == 0) {
      if (node->is_leaf()) {
        erase_leaf_list(node);
        // If the iterator is still pointing at the removed node then we had just erased the last
        // item, in which case end() is the next.
        if (iter.node_ == node) {
          iter = end();
        }
      }
      free_node(node);
      node = parent;
      index = parent_index;
      continue;
    }

    // Try to merge with the left sibling.
    if (left && node->count() + left->count() <= TreeNode::kMaxValues) {
      const uint32_t left_count_before_merge = left->count();
      left->merge_from(*node);
      if (node->is_leaf()) {
        erase_leaf_list(node);
        // Update the iterator to point to the equivalent location in the left node. In the case
        // where we erased the last item point to end().
        if (iter.node_ == node) {
          if (!iter.end_sentinel()) {
            iter = iterator(this, left, iter.index_ + left_count_before_merge);
          } else {
            iter = end();
          }
        }
      }
      free_node(node);
      node = parent;
      index = parent_index;
      continue;
    }

    // Try to merge with the right sibling.
    if (right && node->count() + right->count() <= TreeNode::kMaxValues) {
      iter.wrap_to_next();
      const uint32_t node_count_before_merge = node->count();
      node->merge_from(*right);
      if (right->is_leaf()) {
        erase_leaf_list(right);
        if (right == iter.node_) {
          iter = iterator(this, node, iter.index_ + node_count_before_merge);
        }
      }
      free_node(right);
      node = parent;
      index = parent_index + 1;
      continue;
    }

    // If we can't merge, try to steal from siblings to rebalance.
    if (right && right->count() > TreeNode::kTargetMinValues) {
      iter.wrap_to_next();
      const uint32_t shift = rebalance_amount(right, node);
      if (right == iter.node_ && !iter.end_sentinel()) {
        if (iter.index_ >= shift) {
          iter.index_ -= shift;
        } else {
          iter = iterator(this, node, node->count() + iter.index_);
        }
      }
      rotate_left(parent, parent_index + 1, node, right, shift);
      return iter;
    }

    if (left && left->count() > TreeNode::kTargetMinValues) {
      iter.wrap_to_next();
      const uint32_t shift = rebalance_amount(left, node);
      BTREE_CHECK(iter.node_ != left);
      rotate_right(parent, parent_index, left, node, shift);
      if (node == iter.node_ && !iter.end_sentinel()) {
        iter.index_ += shift;
      }
      return iter;
    }

    // If we can't merge or rebalance, we just leave the node underfull.
    // In B-trees this is usually avoided, but here we allow it for simplicity as long as
    // the node is not empty.
    if (node == iter.node_) {
      iter.wrap_to_next();
    }
    return iter;
  }
}

template <typename ValueType, typename Allocator, typename Traits, IteratorValidation Validation,
          TreeValidation Validator>
iterator_base<typename BTree<ValueType, Allocator, Traits, Validation, Validator>::TreeNode>
BTree<ValueType, Allocator, Traits, Validation, Validator>::insert_internal(Item item) {
  // Handle insertion into an empty tree.
  if (is_empty()) {
    // Start the root node at the smallest size.
    TreeNode* leaf =
        reinterpret_cast<TreeNode*>(allocator_.allocate(NodeSizeParams::kSizeClasses[0]));
    if (unlikely(!leaf)) {
      // Nothing modified yet, can just abort the operation.
      return {nullptr, 0};
    }
    root_ = std::construct_at(leaf, 0, true, item);
    return {root_, 0};
  }
  // Search for the insertion slot.
  __UNINITIALIZED Path path;
  iterator_base<TreeNode> target = upper_bound_slot_internal(item.key, nullptr, &path);
  if (!target.node_->is_full()) {
    target.node_->insert(target.index_, item);
    return target;
  }
  // Pre-allocate our nodes.
  Allocations<TreeNode, NodeSizeParams, Allocator> allocations(allocator_);
  {
    TreeNode* node = target.node_;
    // This loop holds the equivalent conditional logic as the actual insertion path, except it
    // only records what allocations would happen.
    // TODO(https://fxbug.dev/494059275): Have a templated method that can be instantiated to either
    // record allocations or perform the insertions to avoid this fragile logic duplication.
    while (node->is_full()) {
      if (node == root_) {
        // Can the root still be expanded?
        if (node->size_class() < kLargestNodeSizeClass) {
          if (!allocations.reserve(node->size_class() + 1)) {
            return {right_most_leaf(), TreeNode::kMaxValues};
          }
        } else {
          // Increasing the depth of the tree requires another slot in the path tracker.
          if (path.is_full()) {
            return {right_most_leaf(), TreeNode::kMaxValues};
          }
          if (!allocations.reserve(kLargestNodeSizeClass) ||
              !allocations.reserve(NodeSizeParams::kFirstNonLeafRootClass)) {
            return {right_most_leaf(), TreeNode::kMaxValues};
          }
        }
        break;
      }
      auto [parent, parent_index] = path.pop();
      TreeNode* right = right_node(parent, parent_index);
      if (right && !right->is_full()) {
        break;
      }
      TreeNode* left = left_node(parent, parent_index);
      if (left && !left->is_full()) {
        break;
      }
      if (!allocations.reserve(kLargestNodeSizeClass)) {
        return {right_most_leaf(), TreeNode::kMaxValues};
      }
      node = parent;
    }
    // Put the path back to the start so it can be traversed again below.
    path.reset_path();
  }

  bool leaf_insert = true;
  iterator_base<TreeNode> ret(target);
  do {
    if (!target.node_->is_full()) {
      BTREE_CHECK(!target.node_->is_leaf());
      BTREE_CHECK(!leaf_insert);
      target.node_->insert(target.index_, item);
      return ret;
    }
    iterator_base<TreeNode> parent;
    if (target.node_ == root_) {
      // Check if the root can be expanded.
      if (root_->size_class() < kLargestNodeSizeClass) {
        TreeNode* new_root =
            allocations.take_next(root_->size_class() + 1, std::move(*root_), item, target.index_);
        if (leaf_insert) {
          ret.node_ = new_root;
        }
        free_node(root_);
        root_ = new_root;
        return ret;
      }
    } else {
      // Can re-balance?
      parent = path.pop();
      TreeNode* right = right_node(parent.node_, parent.index_);
      if (right && !right->is_full()) {
        target.node_->insert_rebalance_right(right, target.index_, item,
                                             leaf_insert ? &ret : nullptr);
        parent.node_->update_key(parent.index_ + 1, right->get(0).key);
        return ret;
      }
      TreeNode* left = left_node(parent.node_, parent.index_);
      if (left && !left->is_full()) {
        if (target.index_ == TreeNode::kMaxValues && leaf_insert && !target.node_->next()) {
          // If this is a tail insertion then make as much as space as possible under the
          // assumption of future tail insertions.
          target.node_->rotate_left_max(left);
          ret.index_ = target.node_->count();
          target.node_->push(item);
        } else {
          target.node_->insert_rebalance_left(left, target.index_, item,
                                              leaf_insert ? &ret : nullptr);
        }
        parent.node_->update_key(parent.index_, target.node_->get(0).key);
        return ret;
      }
    }

    // Need to allocate a new node. This is not the root node and so is always the largest size.
    TreeNode* new_right = allocations.take_next(kLargestNodeSizeClass, leaf_insert);
    if (leaf_insert) {
      insert_leaf_list(target.node_, new_right);
    }
    // In the case of tail insertion do not rebalance.
    if (target.index_ == TreeNode::kMaxValues && leaf_insert && !new_right->next()) {
      ret = iterator_base(new_right, 0);
      new_right->push(item);
    } else {
      target.node_->insert_rebalance_right(new_right, target.index_, item,
                                           leaf_insert ? &ret : nullptr);
    }
    leaf_insert = false;

    // Loop around to insert into parent (if we have one).
    item = Item{.key = new_right->get(0).key, .value = std::bit_cast<uint64_t>(new_right)};
    target = parent;
    target.index_++;
  } while (target.node_);

  // Need to increase the depth with a new root.
  TreeNode* old_root = root_;
  Item left_node{.key = 0, .value = std::bit_cast<uint64_t>(old_root)};
  root_ = allocations.take_next(NodeSizeParams::kFirstNonLeafRootClass, false, left_node, item);
  // The root_, being a non-leaf node, needs to track the left most and right most leaves. These
  // are either inherited from the old root (if it was an intermediate), or constructed as the
  // two leaf nodes we have.
  if (old_root->is_leaf()) {
    root_->set_prev(old_root);
    root_->set_next(std::bit_cast<TreeNode*>(item.value));
  } else {
    root_->set_prev(old_root->prev());
    root_->set_next(old_root->next());
  }
  return ret;
}

template <typename ValueType, typename Allocator, typename Traits, IteratorValidation Validation,
          TreeValidation Validator>
iterator_base<typename BTree<ValueType, Allocator, Traits, Validation, Validator>::TreeNode>
BTree<ValueType, Allocator, Traits, Validation, Validator>::insert_hint_internal(
    iterator_base<TreeNode> hint, Item item) {
  // Skip empty trees and full nodes.
  if (!hint.node_ || hint.node_->is_full()) {
    return insert_internal(item);
  }
  BTREE_CHECK(!hint.node_->is_empty());
  // For simplicity clamp the index to the valid range, and test for tail insertion at the same
  // time.
  const uint32_t c = hint.node_->count();
  if (hint.index_ >= c - 1) {
    if (hint.node_->get(c - 1).key < item.key) {
      if (!hint.node_->next()) {
        hint.node_->push(item);
        return {hint.node_, c};
      }
      return insert_internal(item);
    }
    hint.index_ = c - 1;
  }
  // Test for head insertion.
  if (hint.index_ <= 1 && hint.node_->get(0).key > item.key) {
    if (!hint.node_->prev()) {
      hint.node_->insert(0, item);
      return {hint.node_, 0};
    }
    return insert_internal(item);
  }
  // Slide to the right?
  if (hint.node_->get(hint.index_).key < item.key) {
    hint.index_++;
  }
  // Inserting in place?
  if (hint.node_->get(hint.index_).key > item.key &&
      hint.node_->get(hint.index_ - 1).key < item.key) {
    hint.node_->insert(hint.index_, item);
    return hint;
  }

  return insert_internal(item);
}

template <typename ValueType, typename Allocator, typename Traits, IteratorValidation Validation,
          TreeValidation Validator>
BTree<ValueType, Allocator, Traits, Validation, Validator>::Utilization
BTree<ValueType, Allocator, Traits, Validation, Validator>::calculate_utilization_slow() const {
  __UNINITIALIZED Path path;
  if (!root_) {
    return Utilization{-1, 0, 0};
  }
  Utilization util{0, 0, 0};
  util.root_size_class = root_->size_class();

  if (root_->is_leaf()) {
    util.stored_values += root_->count();
    return util;
  }

  uint32_t index = 0;
  TreeNode* cur = root_;

  do {
    // Walk down and left to the next leaf.
    while (!cur->is_leaf()) {
      path.push({cur, index});
      cur = std::bit_cast<TreeNode*>(cur->get(index).value);
      util.num_non_root_nodes++;
      index = 0;
    }
    util.stored_values += cur->count();
    // Walk up and right until we find a valid slot.
    do {
      auto [parent, parent_index] = path.pop();
      cur = parent;
      index = parent_index + 1;
    } while (cur != root_ && !cur->valid_index(index));
  } while (cur->valid_index(index));
  return util;
}

template <typename ValueType, typename Allocator, typename Traits, IteratorValidation Validation,
          TreeValidation Validator>
bool BTree<ValueType, Allocator, Traits, Validation, Validator>::subtree_valid(
    TreeNode* node, uint64_t lower_bound, uint64_t upper_bound) {
  if (!node) {
    return true;
  }
  // Validate bounds.
  for (uint32_t i = 0; i < node->count(); i++) {
    if (node->get(i).key < lower_bound) {
      BTREE_CHECK(node->get(i).key >= lower_bound);
      return false;
    }
    if (node->get(i).key >= upper_bound) {
      BTREE_CHECK(node->get(i).key < upper_bound);
      return false;
    }
  }
  // Validate the keys
  if (node->count() > 0) {
    for (uint32_t i = 0; i < node->count() - 1; i++) {
      if (node->get(i + 1).key <= node->get(i).key) {
        BTREE_CHECK(node->get(i + 1).key > node->get(i).key);
        return false;
      }
    }
  }
  if (!node->is_leaf()) {
    BTREE_CHECK(node->count() >= 2);
    for (uint32_t i = 0; i < node->count(); i++) {
      uint64_t next_lower = i == 0 ? lower_bound : node->get(i).key;
      uint64_t next_upper = node->valid_index(i + 1) ? node->get(i + 1).key : upper_bound;
      if (!subtree_valid(std::bit_cast<TreeNode*>(node->get(i).value), next_lower, next_upper)) {
        return false;
      }
    }
  }
  return true;
}

}  // namespace btree

#endif  // ZIRCON_KERNEL_LIB_BTREE_INCLUDE_LIB_BTREE_H_
