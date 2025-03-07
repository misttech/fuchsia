// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/storage/memfs/dnode.h"

#include <stdlib.h>

#include <memory>

#include "src/storage/lib/vfs/cpp/vfs.h"
#include "src/storage/memfs/vnode.h"

namespace memfs {

// Create a new dnode and attach it to a vnode
std::unique_ptr<Dnode> Dnode::Create(std::string_view name, fbl::RefPtr<Vnode> vn) {
  if ((name.length() > kDnodeNameMax) || (name.length() < 1)) {
    return nullptr;
  }

  fbl::AllocChecker ac;
  std::unique_ptr<char[]> namebuffer(new (&ac) char[name.length() + 1]);
  if (!ac.check()) {
    return nullptr;
  }
  memcpy(namebuffer.get(), name.data(), name.length());
  namebuffer[name.length()] = '\0';
  auto dn = std::unique_ptr<Dnode>(
      new Dnode(std::move(vn), std::move(namebuffer), static_cast<uint32_t>(name.length())));
  return dn;
}

std::unique_ptr<Dnode> Dnode::RemoveFromParent() {
  ZX_DEBUG_ASSERT(vnode_ != nullptr);

  std::unique_ptr<Dnode> node;
  // Detach from parent
  if (parent_) {
    node = parent_->children_.erase(*this);
    if (IsDirectory()) {
      // '..' no longer references parent.
      parent_->vnode_->link_count_--;
    }
    parent_->vnode_->UpdateModified();
    parent_ = nullptr;
    vnode_->link_count_--;
  }
  return node;
}

void Dnode::Detach() {
  ZX_DEBUG_ASSERT(children_.is_empty());
  if (vnode_ == nullptr) {  // Dnode already detached.
    return;
  }

  auto self = RemoveFromParent();
  // Detach from vnode
  self->vnode_->dnode_ = nullptr;
  self->vnode_->dnode_parent_ = nullptr;
  self->vnode_ = nullptr;
}

void Dnode::AddChild(Dnode* parent, std::unique_ptr<Dnode> child) {
  ZX_DEBUG_ASSERT(parent != nullptr);
  ZX_DEBUG_ASSERT(child != nullptr);
  ZX_DEBUG_ASSERT(child->parent_ == nullptr);  // Child shouldn't have a parent
  ZX_DEBUG_ASSERT(child.get() != parent);
  ZX_DEBUG_ASSERT(parent->IsDirectory());

  child->parent_ = parent;
  child->vnode_->dnode_parent_ = parent;
  child->vnode_->link_count_++;
  if (child->IsDirectory()) {
    // Child has '..' pointing back at parent.
    parent->vnode_->link_count_++;
  }
  // Ensure that the ordering of tokens in the children list is absolute.
  if (parent->children_.is_empty()) {
    child->ordering_token_ = 2;  // '0' for '.', '1' for '..'
  } else {
    child->ordering_token_ = parent->children_.back().ordering_token_ + 1;
  }
  parent->children_.push_back(std::move(child));
  parent->vnode_->UpdateModified();
}

zx_status_t Dnode::Lookup(std::string_view name, Dnode** out) {
  auto dn = children_.find_if([&name](const Dnode& elem) -> bool { return elem.NameMatch(name); });
  if (dn == children_.end()) {
    return ZX_ERR_NOT_FOUND;
  }

  if (out != nullptr) {
    *out = &(*dn);
  }
  return ZX_OK;
}

fbl::RefPtr<Vnode> Dnode::AcquireVnode() const { return vnode_; }

Dnode* Dnode::GetParent() const { return parent_; }

zx_status_t Dnode::CanUnlink() const {
  if (!children_.is_empty()) {
    // Cannot unlink non-empty directory
    return ZX_ERR_NOT_EMPTY;
  } else if (vnode_->IsRemote()) {
    // Cannot unlink mount points
    return ZX_ERR_UNAVAILABLE;
  }
  return ZX_OK;
}

struct dircookie_t {
  size_t order;  // Minimum 'order' of the next dnode dirent to be read.
};

static_assert(sizeof(dircookie_t) <= sizeof(fs::VdirCookie),
              "MemFS dircookie too large to fit in IO state");

// Read the canned "." and ".." entries that should
// appear at the beginning of a directory.
zx_status_t Dnode::ReaddirStart(fs::DirentFiller* df, void* cookie) {
  dircookie_t* c = static_cast<dircookie_t*>(cookie);
  zx_status_t r;

  if (c->order == 0) {
    // TODO(smklein): Return the real ino.
    uint64_t ino = fuchsia_io::wire::kInoUnknown;
    if ((r = df->Next(".", VTYPE_TO_DTYPE(V_TYPE_DIR), ino)) != ZX_OK) {
      return r;
    }
    c->order++;
  }
  return ZX_OK;
}

void Dnode::Readdir(fs::DirentFiller* df, void* cookie) const {
  dircookie_t* c = static_cast<dircookie_t*>(cookie);
  zx_status_t r = 0;

  if (c->order < 1) {
    if ((r = Dnode::ReaddirStart(df, cookie)) != ZX_OK) {
      return;
    }
  }

  for (const auto& dn : children_) {
    if (dn.ordering_token_ < c->order) {
      continue;
    }
    uint32_t vtype = dn.IsDirectory() ? V_TYPE_DIR : V_TYPE_FILE;
    if ((r = df->Next(std::string_view(dn.name_.get(), dn.NameLen()), VTYPE_TO_DTYPE(vtype),
                      dn.AcquireVnode()->ino())) != ZX_OK) {
      return;
    }
    c->order = dn.ordering_token_ + 1;
  }
}

// Answers the question: "Is dn a subdirectory of this?"
bool Dnode::IsSubdirectory(const Dnode* dn) const {
  if (IsDirectory() && dn->IsDirectory()) {
    // Iterate all the way up to root
    while (dn->parent_ != nullptr && dn->parent_ != dn) {
      if (vnode_ == dn->vnode_) {
        return true;
      }
      dn = dn->parent_;
    }
  }
  return false;
}

std::unique_ptr<char[]> Dnode::TakeName() { return std::move(name_); }

void Dnode::PutName(std::unique_ptr<char[]> name, size_t len) {
  flags_ = static_cast<uint32_t>((flags_ & ~kDnodeNameMax) | len);
  name_ = std::move(name);
}

bool Dnode::IsDirectory() const { return vnode_->IsDirectory(); }

Dnode::Dnode(fbl::RefPtr<Vnode> vn, std::unique_ptr<char[]> name, uint32_t flags)
    : vnode_(std::move(vn)),
      parent_(nullptr),
      ordering_token_(0),
      flags_(flags),
      name_(std::move(name)) {}

Dnode::~Dnode() = default;

size_t Dnode::NameLen() const { return flags_ & kDnodeNameMax; }

bool Dnode::NameMatch(std::string_view name) const {
  return name == std::string_view(name_.get(), NameLen());
}

}  // namespace memfs
