// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <kernel/ffi.h>
#include <object/dispatcher.h>

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_dispatcher_on_zero_handles(Dispatcher* disp) { disp->on_zero_handles(); }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_dispatcher_clear_signals(Dispatcher* disp, zx_signals_t signals) {
  disp->ClearSignals(signals);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_dispatcher_update_state(Dispatcher* disp, zx_signals_t clear_mask,
                                                   zx_signals_t set_mask,
                                                   zx_signals_t strobe_mask) {
  disp->UpdateState(clear_mask, set_mask, strobe_mask);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_dispatcher_update_state_locked(
    Dispatcher* disp, zx_signals_t clear_mask, zx_signals_t set_mask,
    zx_signals_t strobe_mask) TA_NO_THREAD_SAFETY_ANALYSIS {
  disp->UpdateStateLocked(clear_mask, set_mask, strobe_mask);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_signals_t cpp_dispatcher_raise_signals_locked(
    Dispatcher* disp, zx_signals_t signals) TA_NO_THREAD_SAFETY_ANALYSIS {
  return disp->RaiseSignalsLocked(signals);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_dispatcher_notify_observers_locked(
    Dispatcher* disp, zx_signals_t signals, void* queue_to_own) TA_NO_THREAD_SAFETY_ANALYSIS {
  disp->NotifyObserversLocked(signals, static_cast<OwnedWaitQueue*>(queue_to_own));
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_signals_t cpp_dispatcher_signals_state_locked(const Dispatcher* disp)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  return disp->GetSignalsStateLocked();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void* cpp_dispatcher_get_ref_counted(const Dispatcher* disp) {
  return disp->get_ref_counted_base();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_obj_type_t cpp_dispatcher_get_type(const Dispatcher* disp) {
  return disp->get_type();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_koid_t cpp_dispatcher_get_koid(const Dispatcher* disp) {
  return disp->get_koid();
}

void cpp_dispatcher_recycle(Dispatcher* disp) {
  fbl::internal::recycler<Dispatcher>::recycle(disp);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_koid_t cpp_dispatcher_get_related_koid(const Dispatcher* disp) {
  return disp->get_related_koid();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_dispatcher_add_observer(Dispatcher* dispatcher,
                                                          SignalObserver* observer,
                                                          const void* handle,
                                                          zx_signals_t signals) {
  return dispatcher->AddObserver(observer, handle, signals);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_dispatcher_remove_observer(Dispatcher* dispatcher,
                                                      SignalObserver* observer,
                                                      zx_signals_t* out_signals) {
  return dispatcher->RemoveObserver(observer, out_signals);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_dispatcher_get_name(const Dispatcher* disp,
                                                      char out_name[ZX_MAX_NAME_LEN]) {
  return disp->get_name(*reinterpret_cast<char (*)[ZX_MAX_NAME_LEN]>(out_name));
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_dispatcher_set_name(Dispatcher* disp, const char* name,
                                                      size_t len) {
  return disp->set_name(name, len);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE uint32_t cpp_dispatcher_current_handle_count(const Dispatcher* disp) {
  return disp->current_handle_count();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_dispatcher_is_waitable(const Dispatcher* disp) {
  return disp->is_waitable();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_dispatcher_cancel(Dispatcher* disp, const void* handle) {
  disp->Cancel(handle);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_dispatcher_cancel_by_key(Dispatcher* disp, const void* handle,
                                                    const void* port, uint64_t key) {
  return disp->CancelByKey(handle, port, key);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_dispatcher_user_signal_self(Dispatcher* disp, uint32_t clear_mask,
                                                              uint32_t set_mask) {
  return disp->user_signal_self(clear_mask, set_mask);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_dispatcher_user_signal_peer(Dispatcher* disp, uint32_t clear_mask,
                                                              uint32_t set_mask) {
  return disp->user_signal_peer(clear_mask, set_mask);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_dispatcher_make_port_observer(Dispatcher* disp, uint32_t options,
                                                                const void* handle,
                                                                zx_signals_t signals, uint64_t key,
                                                                PortDispatcher* port) {
  return disp->MakePortObserver(options, static_cast<const Handle*>(handle), signals, key, port);
}

}  // extern "C"
