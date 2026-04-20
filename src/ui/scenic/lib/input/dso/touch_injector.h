// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_SCENIC_LIB_INPUT_DSO_TOUCH_INJECTOR_H_
#define SRC_UI_SCENIC_LIB_INPUT_DSO_TOUCH_INJECTOR_H_

#include "src/ui/scenic/lib/input/dso/injector.h"

namespace scenic_impl::input_dso {

using input::InternalTouchEvent;

// Implementation of the |fuchsia_ui_pointerinjector_dso::Device| interface. One instance per
// channel.
class TouchInjector : public Injector {
 public:
  TouchInjector(inspect::Node inspect_node, InjectorSettings settings, Viewport viewport,
                fdf::ServerEnd<fuchsia_ui_pointerinjector_dso::Device> device,
                fit::function<bool(/*descendant*/ zx_koid_t, /*ancestor*/ zx_koid_t)>
                    is_descendant_and_connected,
                fit::function<void(InternalTouchEvent, StreamId stream_id)> inject,
                fit::function<void()> on_channel_closed, async_dispatcher_t* dispatcher);

 protected:
  // |Injector|
  void ForwardEvent(fuchsia_ui_pointerinjector::wire::Event& event, StreamId stream_id,
                    uint64_t trace_flow_id) override;
  // |Injector|
  void CancelStream(uint32_t pointer_id, StreamId stream_id) override;

 private:
  InternalTouchEvent PointerInjectorEventToInternalTouchEvent(
      fuchsia_ui_pointerinjector::wire::Event& event, uint64_t trace_flow_id) const;
  // Used to inject the event into InputSystem for dispatch to clients.
  const fit::function<void(InternalTouchEvent, StreamId)> inject_;
};

}  // namespace scenic_impl::input_dso

#endif  // SRC_UI_SCENIC_LIB_INPUT_DSO_TOUCH_INJECTOR_H_
