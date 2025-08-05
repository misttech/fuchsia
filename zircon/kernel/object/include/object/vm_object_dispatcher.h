// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_VM_OBJECT_DISPATCHER_H_
#define ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_VM_OBJECT_DISPATCHER_H_

#include <lib/user_copy/user_iovec.h>
#include <lib/user_copy/user_ptr.h>
#include <lib/zx/result.h>
#include <sys/types.h>
#include <zircon/rights.h>
#include <zircon/types.h>

#include <fbl/canary.h>
#include <fbl/intrusive_container_utils.h>
#include <fbl/intrusive_double_list.h>
#include <ktl/atomic.h>
#include <ktl/limits.h>
#include <object/dispatcher.h>
#include <object/handle.h>
#include <vm/content_size_manager.h>
#include <vm/vm_object.h>

class VmObjectDispatcher final : public SoloDispatcher<VmObjectDispatcher, ZX_DEFAULT_VMO_RIGHTS>,
                                 public VmObjectChildObserver {
 public:
  enum class InitialMutability { kMutable, kImmutable };

  struct CreateStats {
    uint32_t flags;
    size_t size;
  };

  static zx::result<CreateStats> parse_create_syscall_flags(uint32_t flags, size_t size);

  static zx_status_t CreateWithCsm(fbl::RefPtr<VmObject> vmo,
                                   fbl::RefPtr<ContentSizeManager> content_size_manager,
                                   InitialMutability initial_mutability,
                                   KernelHandle<VmObjectDispatcher>* handle, zx_rights_t* rights);

  static zx_status_t Create(fbl::RefPtr<VmObject> vmo, uint64_t content_size,
                            InitialMutability initial_mutability,
                            KernelHandle<VmObjectDispatcher>* handle, zx_rights_t* rights);
  ~VmObjectDispatcher() final;

  // VmObjectChildObserver implementation.
  void OnZeroChild() final;

  // SoloDispatcher implementation.
  zx_obj_type_t get_type() const final { return ZX_OBJ_TYPE_VMO; }
  [[nodiscard]] zx_status_t get_name(char (&out_name)[ZX_MAX_NAME_LEN]) const final;
  [[nodiscard]] zx_status_t set_name(const char* name, size_t len) final;

  // Dispatcher implementation.
  void on_zero_handles() final;

  zx::result<fbl::RefPtr<ContentSizeManager>> content_size_manager() TA_EXCL(get_lock());

  // VmObjectDispatcher own methods.
  ktl::pair<zx_status_t, size_t> Read(user_out_ptr<char> user_data, uint64_t offset, size_t length);
  ktl::pair<zx_status_t, size_t> Write(
      user_in_ptr<const char> user_data, uint64_t offset, size_t length,
      VmObject::OnWriteBytesTransferredCallback on_bytes_transferred = nullptr);
  zx_status_t SetSize(uint64_t);
  zx_status_t GetSize(uint64_t* size);
  zx_status_t RangeOp(uint32_t op, uint64_t offset, uint64_t size, user_inout_ptr<void> buffer,
                      size_t buffer_size, zx_rights_t rights);
  zx_status_t CreateChild(uint32_t options, uint64_t offset, uint64_t size, bool copy_name,
                          fbl::RefPtr<VmObject>* child_vmo);

  zx_status_t SetMappingCachePolicy(uint32_t cache_policy);

  zx_info_vmo_t GetVmoInfo(zx_rights_t rights);

  zx_status_t SetContentSize(uint64_t);
  zx_status_t SetStreamSize(uint64_t);
  uint64_t GetContentSize() const;

  const fbl::RefPtr<VmObject>& vmo() const { return vmo_; }
  zx_koid_t pager_koid() const { return vmo_->GetPageSourceKoid().value_or(ZX_KOID_INVALID); }

 private:
  explicit VmObjectDispatcher(fbl::RefPtr<VmObject> vmo,
                              fbl::RefPtr<ContentSizeManager> content_size_manager,
                              InitialMutability initial_mutability);

  zx_status_t CreateChildInternal(uint32_t options, uint64_t offset, uint64_t size, bool copy_name,
                                  fbl::RefPtr<VmObject>* child_vmo) TA_REQ(get_lock());

  // The 'const' here is load bearing; we give a raw pointer to
  // ourselves to |vmo_| so we have to ensure we don't reset vmo_
  // except during destruction.
  fbl::RefPtr<VmObject> const vmo_;

  // Manages the content size associated with this VMO. The content size is used by streams created
  // against this VMO. The content size manager is lazily created, hence this field is guarded by
  // the lock, however once created it can be assumed to be constant.
  // Creating the content size manager can be deferred as long as the content is exactly the vmo
  // size, and there are no streams or other operations that implicitly require a content size
  // manager to exist.
  fbl::RefPtr<ContentSizeManager> content_size_mgr_ TA_GUARDED(get_lock());

  // Indicates whether the VMO was immutable at creation time.
  const InitialMutability initial_mutability_;
};

enum class VmoOwnership { kHandle, kMapping, kIoBuffer };
zx_info_vmo_t VmoToInfoEntry(const VmObject* vmo, VmoOwnership ownership,
                             zx_rights_t handle_rights);

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_VM_OBJECT_DISPATCHER_H_
