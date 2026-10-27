// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <vm/evictor.h>

extern "C" {

void rust_evictor_init(Evictor* evictor);
void rust_evictor_init_with_test_functions(Evictor* evictor, void* ctx,
                                           Evictor::TestReclaimFunction reclaim_fn,
                                           Evictor::TestFreePagesFunction free_pages_fn);
void rust_evictor_destroy(Evictor* evictor);
void rust_evictor_enable_eviction(Evictor* evictor, bool use_compression);
void rust_evictor_evict_from_external_target(Evictor* evictor,
                                             const Evictor::EvictionTarget* target,
                                             Evictor::EvictionResult* out_result);
void rust_evictor_debug_get_eviction_target(const Evictor* evictor,
                                            Evictor::EvictionTarget* out_target);
void rust_evictor_combine_eviction_target(Evictor* evictor, const Evictor::EvictionTarget* target);
void rust_evictor_evict_from_preloaded_target(Evictor* evictor,
                                              Evictor::EvictedPageCounts* out_counts);

}  // extern "C"

Evictor::Evictor() { rust_evictor_init(this); }

Evictor::Evictor(void* ctx, TestReclaimFunction reclaim_function,
                 TestFreePagesFunction free_pages_function) {
  rust_evictor_init_with_test_functions(this, ctx, reclaim_function, free_pages_function);
}

Evictor::~Evictor() { rust_evictor_destroy(this); }

void Evictor::EnableEviction(bool use_compression) {
  rust_evictor_enable_eviction(this, use_compression);
}

Evictor::EvictionResult Evictor::EvictFromExternalTarget(Evictor::EvictionTarget target) {
  EvictionResult result;
  rust_evictor_evict_from_external_target(this, &target, &result);
  return result;
}

Evictor::EvictionTarget Evictor::DebugGetEvictionTarget() const {
  EvictionTarget target;
  rust_evictor_debug_get_eviction_target(this, &target);
  return target;
}

void Evictor::CombineEvictionTarget(EvictionTarget target) {
  rust_evictor_combine_eviction_target(this, &target);
}

Evictor::EvictedPageCounts Evictor::EvictFromPreloadedTarget() {
  EvictedPageCounts counts;
  rust_evictor_evict_from_preloaded_target(this, &counts);
  return counts;
}
