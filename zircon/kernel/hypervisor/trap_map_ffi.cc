// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <stddef.h>

#include <new>

#include <fbl/arena.h>
#include <hypervisor/state_invalidator.h>
#include <hypervisor/trap_map.h>
#include <kernel/ffi.h>
#include <kernel/mutex.h>

namespace hypervisor {

using PortPacketArena = fbl::TypedArena<PortPacket, Mutex>;

// Base class providing the C++ PortAllocator vtable for Rust's BlockingPortAllocator.
class RustPortAllocator final : public PortAllocator {
 public:
  RustPortAllocator() = default;
  ~RustPortAllocator() override = default;
  PortPacket* Alloc() override;
  void Free(PortPacket* port_packet) override;
};

static_assert(sizeof(RustPortAllocator) == 8);
static_assert(alignof(RustPortAllocator) == 8);

constexpr size_t kPortPacketArenaSize =
    (kSchedulerLockSpinTracingEnabled || kLockNameTracingEnabled) ? 280 : 272;
static_assert(sizeof(PortPacketArena) == kPortPacketArenaSize);
static_assert(alignof(PortPacketArena) == 8);

}  // namespace hypervisor

extern "C" {

PortPacket* rust_blocking_port_allocator_alloc(hypervisor::RustPortAllocator* allocator);
void rust_blocking_port_allocator_free(hypervisor::RustPortAllocator* allocator,
                                       PortPacket* port_packet);

zx_status_t rust_trap_queue(const hypervisor::Trap* trap, const zx_port_packet_t* packet,
                            hypervisor::StateInvalidator* invalidator);
bool rust_trap_has_port(const hypervisor::Trap* trap);
uint32_t rust_trap_kind(const hypervisor::Trap* trap);
uint64_t rust_trap_key(const hypervisor::Trap* trap);

void rust_trap_map_new(hypervisor::TrapMap* trap_map);
void rust_trap_map_destroy(hypervisor::TrapMap* trap_map);
zx_status_t rust_trap_map_insert_trap(hypervisor::TrapMap* trap_map, uint32_t kind,
                                      zx_gpaddr_t addr, size_t len, PortDispatcher* port,
                                      uint64_t key);
zx_status_t rust_trap_map_find_trap(const hypervisor::TrapMap* trap_map, uint32_t kind,
                                    zx_gpaddr_t addr, hypervisor::Trap** out);

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_state_invalidator_invalidate(hypervisor::StateInvalidator* invalidator) {
  invalidator->Invalidate();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_rust_port_allocator_init(
    ffi::Uninitialized<hypervisor::RustPortAllocator>* allocator) {
  allocator->Initialize();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_port_packet_arena_init(
    ffi::Uninitialized<hypervisor::PortPacketArena>* arena) {
  arena->Initialize();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_port_packet_arena_destroy(hypervisor::PortPacketArena* arena) {
  arena->~TypedArena();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_port_packet_arena_init_pool(hypervisor::PortPacketArena* arena,
                                                              const char* name, size_t max_count) {
  return arena->Init(name, max_count);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE PortPacket* cpp_port_packet_arena_new(hypervisor::PortPacketArena* arena,
                                                        const void* handle,
                                                        PortAllocator* allocator) {
  return arena->New(handle, allocator);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_port_packet_arena_delete(hypervisor::PortPacketArena* arena,
                                                    PortPacket* port_packet) {
  arena->Delete(port_packet);
}

}  // extern "C"

namespace hypervisor {

PortPacket* RustPortAllocator::Alloc() { return rust_blocking_port_allocator_alloc(this); }

void RustPortAllocator::Free(PortPacket* port_packet) {
  rust_blocking_port_allocator_free(this, port_packet);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx::result<> Trap::Queue(const zx_port_packet_t& packet,
                                           StateInvalidator* invalidator) {
  return zx::make_result(rust_trap_queue(this, &packet, invalidator));
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool Trap::HasPort() const { return rust_trap_has_port(this); }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE uint32_t Trap::kind() const { return rust_trap_kind(this); }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE uint64_t Trap::key() const { return rust_trap_key(this); }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE TrapMap::TrapMap() { rust_trap_map_new(this); }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE TrapMap::~TrapMap() { rust_trap_map_destroy(this); }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx::result<> TrapMap::InsertTrap(uint32_t kind, zx_gpaddr_t addr, size_t len,
                                                   fbl::RefPtr<PortDispatcher> port, uint64_t key) {
  return zx::make_result(
      rust_trap_map_insert_trap(this, kind, addr, len, fbl::ExportToRawPtr(&port), key));
}

zx::result<Trap*> TrapMap::FindTrap(uint32_t kind, zx_gpaddr_t addr) {
  Trap* out = nullptr;
  zx_status_t status = rust_trap_map_find_trap(this, kind, addr, &out);
  if (status != ZX_OK) {
    return zx::error(status);
  }
  return zx::ok(out);
}

}  // namespace hypervisor
