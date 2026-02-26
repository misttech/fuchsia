// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef ZIRCON_SYSTEM_ULIB_TRACE_PROVIDER_PROVIDER_IMPL_H_
#define ZIRCON_SYSTEM_ULIB_TRACE_PROVIDER_PROVIDER_IMPL_H_

#include <fidl/fuchsia.tracing.provider/cpp/wire.h>
#include <lib/trace-provider/provider.h>

// Provide a definition for the opaque type declared in provider.h.
struct trace_provider {};

namespace trace::internal {

class TraceProviderImpl final : public trace_provider_t,
#if FUCHSIA_API_LEVEL_AT_LEAST(HEAD)
                                public fidl::WireServer<fuchsia_tracing_provider::ProviderV2>
#else
                                public fidl::WireServer<fuchsia_tracing_provider::Provider>
#endif
{
 public:
  ~TraceProviderImpl() override;

#if FUCHSIA_API_LEVEL_AT_LEAST(HEAD)
  TraceProviderImpl(std::string name, async_dispatcher_t* dispatcher,
                    fidl::ServerEnd<fuchsia_tracing_provider::ProviderV2> server_end);

  void Initialize(fuchsia_tracing_provider::wire::ProviderV2InitializeRequest* request,
                  InitializeCompleter::Sync& completer) override;

  void Start(fuchsia_tracing_provider::wire::ProviderV2StartRequest* request,
             StartCompleter::Sync& completer) override;

  void NotifyBufferSaved(
      fuchsia_tracing_provider::wire::ProviderV2NotifyBufferSavedRequest* request,
      NotifyBufferSavedCompleter::Sync& completer) override;

  void Flush(FlushCompleter::Sync& completer) override;

  void handle_unknown_method(
      fidl::UnknownMethodMetadata<fuchsia_tracing_provider::ProviderV2> metadata,
      fidl::UnknownMethodCompleter::Sync& completer) override;
#else
  TraceProviderImpl(std::string name, async_dispatcher_t* dispatcher,
                    fidl::ServerEnd<fuchsia_tracing_provider::Provider> server_end);

  void Initialize(fuchsia_tracing_provider::wire::ProviderInitializeRequest* request,
                  InitializeCompleter::Sync& completer) override;

  void Start(fuchsia_tracing_provider::wire::ProviderStartRequest* request,
             StartCompleter::Sync& completer) override;
#endif

  void Stop(StopCompleter::Sync& completer) override;

  void Terminate(TerminateCompleter::Sync& completer) override;

  void GetKnownCategories(GetKnownCategoriesCompleter::Sync& completer) override;

  void SetGetKnownCategoriesCallback(GetKnownCategoriesCallback callback);

  async_dispatcher_t* dispatcher() const { return dispatcher_; }

  const ProviderConfig& GetProviderConfig() const;

 private:
  static void OnClose();

  const std::string name_;
  async_dispatcher_t* const dispatcher_;
  ProviderConfig provider_config_;
#if FUCHSIA_API_LEVEL_AT_LEAST(HEAD)
  std::optional<fidl::ServerBindingRef<fuchsia_tracing_provider::ProviderV2>> binding_;
#endif

  trace::GetKnownCategoriesCallback get_known_categories_callback_;

  TraceProviderImpl(const TraceProviderImpl&) = delete;
  TraceProviderImpl(TraceProviderImpl&&) = delete;
  TraceProviderImpl& operator=(const TraceProviderImpl&) = delete;
  TraceProviderImpl& operator=(TraceProviderImpl&&) = delete;
};

}  // namespace trace::internal

#endif  // ZIRCON_SYSTEM_ULIB_TRACE_PROVIDER_PROVIDER_IMPL_H_
