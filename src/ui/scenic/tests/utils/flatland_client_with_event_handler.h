// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_SCENIC_TESTS_UTILS_FLATLAND_CLIENT_WITH_EVENT_HANDLER_H_
#define SRC_UI_SCENIC_TESTS_UTILS_FLATLAND_CLIENT_WITH_EVENT_HANDLER_H_

#include <fidl/fuchsia.ui.composition/cpp/fidl.h>
#include <lib/async/default.h>
#include <lib/syslog/cpp/macros.h>

namespace integration_tests {

// Bundles a fidl::Client<Flatland> together with a fidl::AsyncEventHandler<Flatland>.
// Each of the different event types is handled by a separate, settable closure.
class FlatlandClientWithEventHandler
    : protected fidl::AsyncEventHandler<fuchsia_ui_composition::Flatland> {
 public:
  using OnFramePresentedEvent = fidl::Event<fuchsia_ui_composition::Flatland::OnFramePresented>;
  using OnFramePresentedHandler = fit::function<void(OnFramePresentedEvent&)>;
  using OnNextFrameBeginEvent = fidl::Event<fuchsia_ui_composition::Flatland::OnNextFrameBegin>;
  using OnNextFrameBeginHandler = fit::function<void(OnNextFrameBeginEvent&)>;
  using OnErrorEvent = fidl::Event<fuchsia_ui_composition::Flatland::OnError>;
  using OnErrorHandler = fit::function<void(OnErrorEvent&)>;

  // Not moveable, not copyable.
  FlatlandClientWithEventHandler(const FlatlandClientWithEventHandler& other) = delete;
  FlatlandClientWithEventHandler(FlatlandClientWithEventHandler&& other) = delete;
  FlatlandClientWithEventHandler& operator=(const FlatlandClientWithEventHandler& other) = delete;
  FlatlandClientWithEventHandler& operator=(FlatlandClientWithEventHandler&& other) = delete;

  FlatlandClientWithEventHandler(fidl::ClientEnd<fuchsia_ui_composition::Flatland> client_end,
                                 async_dispatcher_t* dispatcher)
      : dispatcher_(dispatcher),
        flatland_(std::move(client_end), dispatcher,
                  static_cast<fidl::AsyncEventHandler<fuchsia_ui_composition::Flatland>*>(this)) {
    FX_CHECK(dispatcher_);
    FX_CHECK(dispatcher_ == async_get_default_dispatcher());
  }

  ~FlatlandClientWithEventHandler() { FX_CHECK(dispatcher_ == async_get_default_dispatcher()); }

  // Allow conveniently calling though to Flatland methods.
  fidl::Client<fuchsia_ui_composition::Flatland>& operator->() { return flatland_; }

  // Configure handling of Flatland::OnFramePresented event.
  void set_on_frame_presented(OnFramePresentedHandler handler) {
    // This assertion enforces the rule: you cannot set a handler if one is already set.
    FX_CHECK(!on_frame_presented_.has_value()) << "OnFramePresented handler is already set.";
    on_frame_presented_ = std::move(handler);
  }
  void reset_on_frame_presented() { on_frame_presented_.reset(); }

  // Configure handling of Flatland::OnNextFrameBegin event.
  void set_on_next_frame_begin(OnNextFrameBeginHandler handler) {
    FX_CHECK(!on_next_frame_begin_.has_value()) << "OnNextFrameBegin handler is already set.";
    on_next_frame_begin_ = std::move(handler);
  }
  void reset_on_next_frame_begin() { on_next_frame_begin_.reset(); }

  // Configure handling of Flatland::OnError event.
  void set_on_error(OnErrorHandler handler) {
    FX_CHECK(!on_error_.has_value()) << "OnError handler is already set.";
    on_error_ = std::move(handler);
  }
  void reset_on_error() { on_error_.reset(); }

 protected:
  // fidl::AsyncEventHandler<fuchsia_ui_composition::Flatland>
  void OnFramePresented(OnFramePresentedEvent& event) override {
    if (on_frame_presented_) {
      (*on_frame_presented_)(event);
    }
  }

  // fidl::AsyncEventHandler<fuchsia_ui_composition::Flatland>
  void OnNextFrameBegin(OnNextFrameBeginEvent& event) override {
    if (on_next_frame_begin_) {
      (*on_next_frame_begin_)(event);
    }
  }

  // fidl::AsyncEventHandler<fuchsia_ui_composition::Flatland>
  void OnError(OnErrorEvent& event) override {
    if (on_error_) {
      (*on_error_)(event);
    }
  }

 private:
  async_dispatcher_t* dispatcher_;

  std::optional<OnFramePresentedHandler> on_frame_presented_;
  std::optional<OnNextFrameBeginHandler> on_next_frame_begin_;
  std::optional<OnErrorHandler> on_error_;

  // MUST be destructed first, therefore it is the last field.
  fidl::Client<fuchsia_ui_composition::Flatland> flatland_;
};

}  // namespace integration_tests

#endif  // SRC_UI_SCENIC_TESTS_UTILS_FLATLAND_CLIENT_WITH_EVENT_HANDLER_H_
