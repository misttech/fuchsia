// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "codec_factory_policy.h"

#include "codec_factory_app.h"

CodecFactoryPolicy::CodecFactoryPolicy(CodecFactoryApp* app) : app_(app) {
  ZX_DEBUG_ASSERT(hw_policies_.empty());
  FX_LOGS(DEBUG) << "board_name: " << app_->board_name();
  // When/if an admission policy needs to be added, it can be added like this:
  //
  // // Please don't delete this example; currently we have zero admission policies but this seems
  // // fairly likely to change again in future.
  // const std::string& board_name = app_->board_name();
  // if (board_name == "astro") {
  //   FX_LOGS(INFO) << "board name is astro";
  //   hw_policies_.emplace_back(
  //       std::unique_ptr<CodecFactoryHwPolicy>(new CodecFactoryHwPolicyAstro(this)));
  // }
}

bool CodecFactoryPolicy::AdmitHwDecoder(const fuchsia::mediacodec::CreateDecoder_Params& params,
                                        std::vector<zx::eventpair>* lifetime_codec_eventpairs) {
  ZX_DEBUG_ASSERT(lifetime_codec_eventpairs);
  for (auto& hw_policy : hw_policies_) {
    if (!hw_policy->AdmitHwDecoder(params)) {
      return false;
    }
  }
  for (auto& hw_policy : hw_policies_) {
    zx::eventpair lifetime_codec_end = hw_policy->TrackHwDecoder(params);
    if (lifetime_codec_end) {
      lifetime_codec_eventpairs->emplace_back(std::move(lifetime_codec_end));
    }
  }
  return true;
}

bool CodecFactoryPolicy::AdmitHwEncoder(const fuchsia::mediacodec::CreateEncoder_Params& params,
                                        std::vector<zx::eventpair>* lifetime_codec_eventpairs) {
  ZX_DEBUG_ASSERT(lifetime_codec_eventpairs);
  for (auto& hw_policy : hw_policies_) {
    if (!hw_policy->AdmitHwEncoder(params)) {
      return false;
    }
  }
  for (auto& hw_policy : hw_policies_) {
    zx::eventpair lifetime_codec_end = hw_policy->TrackHwEncoder(params);
    if (lifetime_codec_end) {
      lifetime_codec_eventpairs->emplace_back(std::move(lifetime_codec_end));
    }
  }
  return true;
}

async_dispatcher_t* CodecFactoryPolicy::dispatcher() { return app_->dispatcher(); }
