// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/lib/escher/vk/command_buffer.h"

#include <lib/fit/defer.h>

#include "src/ui/lib/escher/impl/command_buffer_pool.h"
#include "src/ui/lib/escher/renderer/buffer_cache.h"
#include "src/ui/lib/escher/renderer/frame.h"
#include "src/ui/lib/escher/test/common/gtest_escher.h"
#include "src/ui/lib/escher/test/vk/vulkan_tester.h"

#ifdef __Fuchsia__
#include <lib/zx/event.h>

#include "src/ui/lib/escher/impl/semaphore_pool.h"
#include "src/ui/lib/escher/util/fuchsia_utils.h"
#endif

namespace {
using namespace escher;

class CommandBufferTest : public ::testing::Test, public VulkanTester {
  void SetUp() override { EXPECT_TRUE(test::GetEscher()->Cleanup()); }

  void TearDown() override {
    auto escher = test::GetEscher();
    EXPECT_VK_SUCCESS(escher->vk_device().waitIdle());
    EXPECT_TRUE(escher->Cleanup());
  }
};

// Smoke-test for getting/clearing dirtiness of CommandBuffer state,
// specifically:
// - a newly-created CommandBuffer initially has all dirty bits set.
// - GetAndClearDirty() both obtains the right values, and doesn't stomp other
//   values as it clears the previously-dirty ones.
//
// TODO(https://fxbug.dev/42151125): this could be extended to test the following:
// - that various state-setters (e.g. SetCullMode()) dirty the correct bits
// - that GetAndClearDirty() can be used for multiple bits simultaneously.
VK_TEST_F(CommandBufferTest, Dirtyness) {
  auto escher = test::GetEscher();
  auto cb = CommandBuffer::NewForGraphics(escher, false);

  EXPECT_EQ(GetDirty(cb, DirtyBits::kDirtyStaticStateBit), DirtyBits::kDirtyStaticStateBit);
  EXPECT_EQ(GetDirty(cb, DirtyBits::kDirtyPipelineBit), DirtyBits::kDirtyPipelineBit);
  EXPECT_EQ(GetDirty(cb, DirtyBits::kDirtyViewportBit), DirtyBits::kDirtyViewportBit);
  EXPECT_EQ(GetDirty(cb, DirtyBits::kDirtyScissorBit), DirtyBits::kDirtyScissorBit);
  EXPECT_EQ(GetDirty(cb, DirtyBits::kDirtyDepthBiasBit), DirtyBits::kDirtyDepthBiasBit);
  EXPECT_EQ(GetDirty(cb, DirtyBits::kDirtyStencilMasksAndReferenceBit),
            DirtyBits::kDirtyStencilMasksAndReferenceBit);
  EXPECT_EQ(GetDirty(cb, DirtyBits::kDirtyStaticVertexBit), DirtyBits::kDirtyStaticVertexBit);
  EXPECT_EQ(GetDirty(cb, DirtyBits::kDirtyPushConstantsBit), DirtyBits::kDirtyPushConstantsBit);
  EXPECT_EQ(GetDirty(cb, DirtyBits::kDirtyDynamicBits), DirtyBits::kDirtyDynamicBits);

  // GetAndClearDirty() is the same as GetDirty(), except that the values are
  // also cleared.
  EXPECT_EQ(GetAndClearDirty(cb, DirtyBits::kDirtyStaticStateBit), DirtyBits::kDirtyStaticStateBit);
  EXPECT_EQ(GetAndClearDirty(cb, DirtyBits::kDirtyPipelineBit), DirtyBits::kDirtyPipelineBit);
  EXPECT_EQ(GetAndClearDirty(cb, DirtyBits::kDirtyViewportBit), DirtyBits::kDirtyViewportBit);
  EXPECT_EQ(GetAndClearDirty(cb, DirtyBits::kDirtyScissorBit), DirtyBits::kDirtyScissorBit);
  EXPECT_EQ(GetAndClearDirty(cb, DirtyBits::kDirtyDepthBiasBit), DirtyBits::kDirtyDepthBiasBit);
  EXPECT_EQ(GetAndClearDirty(cb, DirtyBits::kDirtyStencilMasksAndReferenceBit),
            DirtyBits::kDirtyStencilMasksAndReferenceBit);
  EXPECT_EQ(GetAndClearDirty(cb, DirtyBits::kDirtyStaticVertexBit),
            DirtyBits::kDirtyStaticVertexBit);
  EXPECT_EQ(GetAndClearDirty(cb, DirtyBits::kDirtyPushConstantsBit),
            DirtyBits::kDirtyPushConstantsBit);
  // All of these bits were already cleared individually above.
  EXPECT_EQ(GetAndClearDirty(cb, DirtyBits::kDirtyDynamicBits), 0u);

  // All bits were cleared above.
  EXPECT_EQ(GetDirty(cb, DirtyBits::kDirtyStaticStateBit), 0u);
  EXPECT_EQ(GetDirty(cb, DirtyBits::kDirtyPipelineBit), 0u);
  EXPECT_EQ(GetDirty(cb, DirtyBits::kDirtyViewportBit), 0u);
  EXPECT_EQ(GetDirty(cb, DirtyBits::kDirtyScissorBit), 0u);
  EXPECT_EQ(GetDirty(cb, DirtyBits::kDirtyDepthBiasBit), 0u);
  EXPECT_EQ(GetDirty(cb, DirtyBits::kDirtyStencilMasksAndReferenceBit), 0u);
  EXPECT_EQ(GetDirty(cb, DirtyBits::kDirtyStaticVertexBit), 0u);
  EXPECT_EQ(GetDirty(cb, DirtyBits::kDirtyPushConstantsBit), 0u);
  EXPECT_EQ(GetDirty(cb, DirtyBits::kDirtyDynamicBits), 0u);

  // TODO(https://fxbug.dev/42151125): ideally only submitted CommandBuffers would need to be
  // cleaned up: if a never-submitted CB is destroyed, then it shouldn't keep anything alive, and it
  // shouldn't cause problems in e.g. CommandBufferPool due to a forever-straggling buffer.
  EXPECT_TRUE(cb->Submit(nullptr));
}

// Smoke-test for CommandBufferPipelineState's bit-packing setters/getters.
VK_TEST_F(CommandBufferTest, StaticStateSetting) {
  auto escher = test::GetEscher();
  auto cb = CommandBuffer::NewForGraphics(escher, false);
  auto static_state = VulkanTester::GetStaticState(cb);

  {
    using vk::CompareOp;
    for (auto& op :
         std::vector<CompareOp>{CompareOp::eNever, CompareOp::eLess, CompareOp::eEqual,
                                CompareOp::eLessOrEqual, CompareOp::eGreater, CompareOp::eNotEqual,
                                CompareOp::eGreaterOrEqual, CompareOp::eAlways}) {
      cb->SetDepthCompareOp(op);
      EXPECT_EQ(static_state->get_depth_compare(), op);
      EXPECT_EQ(vk::CompareOp(static_state->depth_compare), op);
      EXPECT_EQ(GetAndClearDirty(cb, DirtyBits::kDirtyStaticStateBit),
                DirtyBits::kDirtyStaticStateBit);
      EXPECT_EQ(GetDirty(cb, DirtyBits::kDirtyStaticStateBit), 0u);
    }
  }

  // TODO(https://fxbug.dev/42151125): ideally only submitted CommandBuffers would need to be
  // cleaned up: if a never-submitted CB is destroyed, then it shouldn't keep anything alive, and it
  // shouldn't cause problems in e.g. CommandBufferPool due to a forever-straggling buffer.
  EXPECT_TRUE(cb->Submit(nullptr));
}

// CommandBuffer and CommandBufferPipelineState have matching setters for static state;
// the only difference is that the CommandBuffer variants set dirty bits if any changes are
// made.  This verifies that calling either variant produces the same change in the
// CommandBufferPipelineState::StaticState.
VK_TEST_F(CommandBufferTest, StaticStateSettingMatches) {
  auto escher = test::GetEscher();
  auto cb = CommandBuffer::NewForGraphics(escher, false);
  CommandBufferPipelineState cbps(nullptr);

  auto cb_static_state = VulkanTester::GetStaticState(cb);
  auto cbps_static_state = cbps.static_state();

  const std::vector<bool> kAllBools{true, false};
  const std::vector<vk::CompareOp> kAllVkCompareOps{
      vk::CompareOp::eNever,          vk::CompareOp::eLess,    vk::CompareOp::eEqual,
      vk::CompareOp::eLessOrEqual,    vk::CompareOp::eGreater, vk::CompareOp::eNotEqual,
      vk::CompareOp::eGreaterOrEqual, vk::CompareOp::eAlways};

  const std::vector<vk::BlendFactor> kAllVkBlendFactors{vk::BlendFactor::eZero,
                                                        vk::BlendFactor::eOne,
                                                        vk::BlendFactor::eSrcColor,
                                                        vk::BlendFactor::eOneMinusSrcColor,
                                                        vk::BlendFactor::eDstColor,
                                                        vk::BlendFactor::eOneMinusDstColor,
                                                        vk::BlendFactor::eSrcAlpha,
                                                        vk::BlendFactor::eOneMinusSrcAlpha,
                                                        vk::BlendFactor::eDstAlpha,
                                                        vk::BlendFactor::eOneMinusDstAlpha,
                                                        vk::BlendFactor::eConstantColor,
                                                        vk::BlendFactor::eOneMinusConstantColor,
                                                        vk::BlendFactor::eConstantAlpha,
                                                        vk::BlendFactor::eOneMinusConstantAlpha,
                                                        vk::BlendFactor::eSrcAlphaSaturate,
                                                        vk::BlendFactor::eSrc1Color,
                                                        vk::BlendFactor::eOneMinusSrc1Color,
                                                        vk::BlendFactor::eSrc1Alpha,
                                                        vk::BlendFactor::eOneMinusSrc1Alpha};
  const std::vector<vk::BlendOp> kAllVkBlendOps{vk::BlendOp::eAdd, vk::BlendOp::eSubtract,
                                                vk::BlendOp::eReverseSubtract, vk::BlendOp::eMin,
                                                vk::BlendOp::eMax};

  const std::vector<vk::StencilOp> kAllVkStencilOps{vk::StencilOp::eKeep,
                                                    vk::StencilOp::eZero,
                                                    vk::StencilOp::eReplace,
                                                    vk::StencilOp::eIncrementAndClamp,
                                                    vk::StencilOp::eDecrementAndClamp,
                                                    vk::StencilOp::eInvert,
                                                    vk::StencilOp::eIncrementAndWrap,
                                                    vk::StencilOp::eDecrementAndWrap};

  const std::vector<vk::PrimitiveTopology> kAllVkPrimitiveTopologies{
      vk::PrimitiveTopology::ePointList,
      vk::PrimitiveTopology::eLineList,
      vk::PrimitiveTopology::eLineStrip,
      vk::PrimitiveTopology::eTriangleList,
      vk::PrimitiveTopology::eTriangleStrip,
      vk::PrimitiveTopology::eTriangleFan,
      vk::PrimitiveTopology::eLineListWithAdjacency,
      vk::PrimitiveTopology::eLineStripWithAdjacency,
      vk::PrimitiveTopology::eTriangleListWithAdjacency,
      vk::PrimitiveTopology::eTriangleStripWithAdjacency,
      vk::PrimitiveTopology::ePatchList};

  const std::vector<vk::CullModeFlags> kAllVkCullModeFlags{
      vk::CullModeFlagBits::eNone, vk::CullModeFlagBits::eFront, vk::CullModeFlagBits::eBack,
      vk::CullModeFlagBits::eFrontAndBack};

  const std::vector<vk::FrontFace> kAllVkFrontFaces{vk::FrontFace::eCounterClockwise,
                                                    vk::FrontFace::eClockwise};

// Helper for StaticStateSetting test, below.  Compares setting state via 1-arg functions on both
// CommandBuffer and CommandBufferPipelineState, and verifies that both have the same effect. Also
// verifies that CommandBuffer dirty bits are set properly.
#define TEST_SETTER(SETTER_NAME, GETTER_NAME, VALUE_LIST)                \
  do {                                                                   \
    bool first = true;                                                   \
    for (const auto& val : VALUE_LIST) {                                 \
      cb->SETTER_NAME(val);                                              \
      cbps.SETTER_NAME(val);                                             \
      EXPECT_EQ(cb_static_state->GETTER_NAME(), val);                    \
      EXPECT_EQ(cbps_static_state->GETTER_NAME(), val);                  \
      EXPECT_TRUE(*cb_static_state == *cbps_static_state);               \
      if (!first) {                                                      \
        EXPECT_EQ(GetAndClearDirty(cb, DirtyBits::kDirtyStaticStateBit), \
                  DirtyBits::kDirtyStaticStateBit);                      \
        EXPECT_EQ(GetDirty(cb, DirtyBits::kDirtyStaticStateBit), 0u);    \
      }                                                                  \
      first = false;                                                     \
    }                                                                    \
  } while (0)

// Helper for StaticStateSetting test, below.  Compares setting state via 2-arg functions on both
// CommandBuffer and CommandBufferPipelineState, and verifies that both have the same effect.  Also
// verifies that CommandBuffer dirty bits are set properly.
#define TEST_SETTER2(SETTER_NAME, GETTER_NAME1, GETTER_NAME2, VALUE_LIST1, VALUE_LIST2) \
  do {                                                                                  \
    bool first = true;                                                                  \
    for (const auto& val1 : VALUE_LIST1) {                                              \
      for (const auto& val2 : VALUE_LIST2) {                                            \
        cb->SETTER_NAME(val1, val2);                                                    \
        cbps.SETTER_NAME(val1, val2);                                                   \
        EXPECT_EQ(cb_static_state->GETTER_NAME1(), val1);                               \
        EXPECT_EQ(cbps_static_state->GETTER_NAME1(), val1);                             \
        EXPECT_EQ(cb_static_state->GETTER_NAME2(), val2);                               \
        EXPECT_EQ(cbps_static_state->GETTER_NAME2(), val2);                             \
        if (!first) {                                                                   \
          EXPECT_EQ(GetAndClearDirty(cb, DirtyBits::kDirtyStaticStateBit),              \
                    DirtyBits::kDirtyStaticStateBit);                                   \
          EXPECT_EQ(GetDirty(cb, DirtyBits::kDirtyStaticStateBit), 0u);                 \
        }                                                                               \
        first = false;                                                                  \
      }                                                                                 \
    }                                                                                   \
  } while (0)

// Helper for StaticStateSetting test, below.  Compares setting state via 3-arg functions on both
// CommandBuffer and CommandBufferPipelineState, and verifies that both have the same effect.  Also
// verifies that CommandBuffer dirty bits are set properly.
#define TEST_SETTER3(SETTER_NAME, GETTER_NAME1, GETTER_NAME2, GETTER_NAME3, VALUE_LIST1, \
                     VALUE_LIST2, VALUE_LIST3)                                           \
  do {                                                                                   \
    bool first = true;                                                                   \
    for (const auto& val1 : VALUE_LIST1) {                                               \
      for (const auto& val2 : VALUE_LIST2) {                                             \
        for (const auto& val3 : VALUE_LIST3) {                                           \
          cb->SETTER_NAME(val1, val2, val3);                                             \
          cbps.SETTER_NAME(val1, val2, val3);                                            \
          EXPECT_EQ(cb_static_state->GETTER_NAME1(), val1);                              \
          EXPECT_EQ(cbps_static_state->GETTER_NAME1(), val1);                            \
          EXPECT_EQ(cb_static_state->GETTER_NAME2(), val2);                              \
          EXPECT_EQ(cbps_static_state->GETTER_NAME2(), val2);                            \
          EXPECT_EQ(cb_static_state->GETTER_NAME3(), val3);                              \
          EXPECT_EQ(cbps_static_state->GETTER_NAME3(), val3);                            \
          EXPECT_EQ(*cb_static_state, *cbps_static_state);                               \
          if (!first) {                                                                  \
            EXPECT_EQ(GetAndClearDirty(cb, DirtyBits::kDirtyStaticStateBit),             \
                      DirtyBits::kDirtyStaticStateBit);                                  \
            EXPECT_EQ(GetDirty(cb, DirtyBits::kDirtyStaticStateBit), 0u);                \
          }                                                                              \
          first = false;                                                                 \
        }                                                                                \
      }                                                                                  \
    }                                                                                    \
  } while (0)

// Helper for StaticStateSetting test, below.  Compares setting state via 4-arg functions on both
// CommandBuffer and CommandBufferPipelineState, and verifies that both have the same effect.  Also
// verifies that CommandBuffer dirty bits are set properly.
#define TEST_SETTER4(SETTER_NAME, GETTER_NAME1, GETTER_NAME2, GETTER_NAME3, GETTER_NAME4, \
                     VALUE_LIST1, VALUE_LIST2, VALUE_LIST3, VALUE_LIST4)                  \
  do {                                                                                    \
    bool first = true;                                                                    \
    for (const auto& val1 : VALUE_LIST1) {                                                \
      for (const auto& val2 : VALUE_LIST2) {                                              \
        for (const auto& val3 : VALUE_LIST3) {                                            \
          for (const auto& val4 : VALUE_LIST4) {                                          \
            cb->SETTER_NAME(val1, val2, val3, val4);                                      \
            cbps.SETTER_NAME(val1, val2, val3, val4);                                     \
            EXPECT_EQ(cb_static_state->GETTER_NAME1(), val1);                             \
            EXPECT_EQ(cbps_static_state->GETTER_NAME1(), val1);                           \
            EXPECT_EQ(cb_static_state->GETTER_NAME2(), val2);                             \
            EXPECT_EQ(cbps_static_state->GETTER_NAME2(), val2);                           \
            EXPECT_EQ(cb_static_state->GETTER_NAME3(), val3);                             \
            EXPECT_EQ(cbps_static_state->GETTER_NAME3(), val3);                           \
            EXPECT_EQ(cb_static_state->GETTER_NAME4(), val4);                             \
            EXPECT_EQ(cbps_static_state->GETTER_NAME4(), val4);                           \
            EXPECT_EQ(*cb_static_state, *cbps_static_state);                              \
            if (!first) {                                                                 \
              EXPECT_EQ(GetAndClearDirty(cb, DirtyBits::kDirtyStaticStateBit),            \
                        DirtyBits::kDirtyStaticStateBit);                                 \
              EXPECT_EQ(GetDirty(cb, DirtyBits::kDirtyStaticStateBit), 0u);               \
            }                                                                             \
            first = false;                                                                \
          }                                                                               \
        }                                                                                 \
      }                                                                                   \
    }                                                                                     \
  } while (0)

  TEST_SETTER2(SetDepthTestAndWrite, get_depth_test, get_depth_write, kAllBools, kAllBools);

  TEST_SETTER(SetWireframe, get_wireframe, kAllBools);

  TEST_SETTER(SetDepthCompareOp, get_depth_compare, kAllVkCompareOps);

  TEST_SETTER(SetBlendEnable, get_blend_enable, kAllBools);

  TEST_SETTER4(SetBlendFactors, get_src_color_blend, get_src_alpha_blend, get_dst_color_blend,
               get_dst_alpha_blend, kAllVkBlendFactors, kAllVkBlendFactors, kAllVkBlendFactors,
               kAllVkBlendFactors);

  TEST_SETTER2(SetBlendOp, get_color_blend_op, get_alpha_blend_op, kAllVkBlendOps, kAllVkBlendOps);

  TEST_SETTER(SetColorWriteMask, get_color_write_mask,
              std::vector<uint32_t>({0x0000000, 0x00000001}));

  TEST_SETTER(SetDepthBias, get_depth_bias_enable, kAllBools);
  TEST_SETTER(SetStencilTest, get_stencil_test, kAllBools);
  TEST_SETTER4(SetStencilFrontOps, get_stencil_front_compare_op, get_stencil_front_pass,
               get_stencil_front_fail, get_stencil_front_depth_fail, kAllVkCompareOps,
               kAllVkStencilOps, kAllVkStencilOps, kAllVkStencilOps);
  TEST_SETTER4(SetStencilBackOps, get_stencil_back_compare_op, get_stencil_back_pass,
               get_stencil_back_fail, get_stencil_back_depth_fail, kAllVkCompareOps,
               kAllVkStencilOps, kAllVkStencilOps, kAllVkStencilOps);

  TEST_SETTER(SetPrimitiveTopology, get_primitive_topology, kAllVkPrimitiveTopologies);

  TEST_SETTER(SetPrimitiveRestart, get_primitive_restart, kAllBools);

  TEST_SETTER3(SetMultisampleState, get_alpha_to_coverage, get_alpha_to_one, get_sample_shading,
               kAllBools, kAllBools, kAllBools);

  TEST_SETTER(SetFrontFace, get_front_face, kAllVkFrontFaces);

  TEST_SETTER(SetCullMode, get_cull_mode, kAllVkCullModeFlags);

#undef TEST_SETTER
#undef TEST_SETTER2
#undef TEST_SETTER3
#undef TEST_SETTER4

  // TODO(https://fxbug.dev/42151125): ideally only submitted CommandBuffers would need to be
  // cleaned up: if a never-submitted CB is destroyed, then it shouldn't keep anything alive, and it
  // shouldn't cause problems in e.g. CommandBufferPool due to a forever-straggling buffer.
  EXPECT_TRUE(cb->Submit(nullptr));
}

VK_TEST_F(CommandBufferTest, AddWaitSemaphoreIgnoresNullAndDeduplicatesSamePointer) {
  auto escher = test::GetEscher();
  auto sem1 = Semaphore::New(escher->vk_device());
  auto sem2 = Semaphore::New(escher->vk_device());
  EXPECT_EQ(sem1->koid(), Semaphore::kInvalidKoid);
  EXPECT_EQ(sem2->koid(), Semaphore::kInvalidKoid);

  // Submit a command buffer that signals sem1 and sem2 so that the subsequent command buffer can
  // wait on them cleanly.
  auto signal_cb = CommandBuffer::NewForGraphics(escher, false);
  signal_cb->AddSignalSemaphore(sem1);
  signal_cb->AddSignalSemaphore(sem2);
  EXPECT_TRUE(signal_cb->Submit(nullptr));

  auto cb = CommandBuffer::NewForGraphics(escher, false);
  cb->AddWaitSemaphore(SemaphorePtr(), vk::PipelineStageFlagBits::eBottomOfPipe);
  EXPECT_EQ(cb->NumWaitSemaphores(), 0u);
  EXPECT_TRUE(GetWaitSemaphores(cb).empty());

  cb->AddWaitSemaphore(sem1, vk::PipelineStageFlagBits::eVertexShader);
  EXPECT_EQ(cb->NumWaitSemaphores(), 1u);
  ASSERT_EQ(GetWaitSemaphores(cb).size(), 1u);
  EXPECT_EQ(GetWaitSemaphores(cb)[0], sem1);
  ASSERT_EQ(GetWaitSemaphoreStages(cb).size(), 1u);
  EXPECT_EQ(GetWaitSemaphoreStages(cb)[0], vk::PipelineStageFlagBits::eVertexShader);

  // Adding the same `SemaphorePtr` again must coalesce into the existing entry and bitwise-OR the
  // pipeline stage flags.
  cb->AddWaitSemaphore(sem1, vk::PipelineStageFlagBits::eFragmentShader);
  EXPECT_EQ(cb->NumWaitSemaphores(), 1u);
  ASSERT_EQ(GetWaitSemaphores(cb).size(), 1u);
  EXPECT_EQ(GetWaitSemaphores(cb)[0], sem1);
  ASSERT_EQ(GetWaitSemaphoreStages(cb).size(), 1u);
  EXPECT_EQ(GetWaitSemaphoreStages(cb)[0],
            vk::PipelineStageFlagBits::eVertexShader | vk::PipelineStageFlagBits::eFragmentShader);

  // Distinct unimported semaphores both have `koid() == Semaphore::kInvalidKoid` and must not be
  // coalesced.
  cb->AddWaitSemaphore(sem2, vk::PipelineStageFlagBits::eTransfer);
  EXPECT_EQ(cb->NumWaitSemaphores(), 2u);
  ASSERT_EQ(GetWaitSemaphores(cb).size(), 2u);
  EXPECT_EQ(GetWaitSemaphores(cb)[0], sem1);
  EXPECT_EQ(GetWaitSemaphores(cb)[1], sem2);
  ASSERT_EQ(GetWaitSemaphoreStages(cb).size(), 2u);
  EXPECT_EQ(GetWaitSemaphoreStages(cb)[1], vk::PipelineStageFlagBits::eTransfer);

  // Adding `sem2` a second time must merge into index 1 without modifying index 0.
  cb->AddWaitSemaphore(sem2, vk::PipelineStageFlagBits::eComputeShader);
  EXPECT_EQ(cb->NumWaitSemaphores(), 2u);
  ASSERT_EQ(GetWaitSemaphores(cb).size(), 2u);
  EXPECT_EQ(GetWaitSemaphores(cb)[0], sem1);
  EXPECT_EQ(GetWaitSemaphores(cb)[1], sem2);
  ASSERT_EQ(GetWaitSemaphoreStages(cb).size(), 2u);
  EXPECT_EQ(GetWaitSemaphoreStages(cb)[0],
            vk::PipelineStageFlagBits::eVertexShader | vk::PipelineStageFlagBits::eFragmentShader);
  EXPECT_EQ(GetWaitSemaphoreStages(cb)[1],
            vk::PipelineStageFlagBits::eTransfer | vk::PipelineStageFlagBits::eComputeShader);

  EXPECT_TRUE(cb->Submit(nullptr));
}

VK_TEST_F(CommandBufferTest, AddSignalSemaphoreIgnoresNullAndDeduplicatesSamePointer) {
  auto escher = test::GetEscher();
  auto cb = CommandBuffer::NewForGraphics(escher, false);

  auto sem1 = Semaphore::New(escher->vk_device());
  auto sem2 = Semaphore::New(escher->vk_device());
  EXPECT_EQ(sem1->koid(), Semaphore::kInvalidKoid);
  EXPECT_EQ(sem2->koid(), Semaphore::kInvalidKoid);

  cb->AddSignalSemaphore(SemaphorePtr());
  EXPECT_EQ(cb->NumSignalSemaphores(), 0u);
  EXPECT_FALSE(cb->ContainsSignalSemaphore(SemaphorePtr()));

  cb->AddSignalSemaphore(sem1);
  EXPECT_EQ(cb->NumSignalSemaphores(), 1u);
  EXPECT_FALSE(cb->ContainsSignalSemaphore(SemaphorePtr()));
  EXPECT_TRUE(cb->ContainsSignalSemaphore(sem1));
  EXPECT_FALSE(cb->ContainsSignalSemaphore(sem2));

  cb->AddSignalSemaphore(sem1);
  EXPECT_EQ(cb->NumSignalSemaphores(), 1u);

  // Distinct unimported semaphores both have `koid() == Semaphore::kInvalidKoid` and must not be
  // coalesced.
  cb->AddSignalSemaphore(sem2);
  EXPECT_EQ(cb->NumSignalSemaphores(), 2u);
  EXPECT_TRUE(cb->ContainsSignalSemaphore(sem2));

  EXPECT_TRUE(cb->Submit(nullptr));
}

#ifdef __Fuchsia__
VK_TEST_F(CommandBufferTest, AddWaitSemaphoreDeduplicatesSameKoidAndMergesStages) {
  auto escher = test::GetEscher();
  auto cb = CommandBuffer::NewForGraphics(escher, false);

  zx::event event1;
  ASSERT_EQ(zx::event::create(0u, &event1), ZX_OK);
  zx::event event1_dup1;
  zx::event event1_dup2;
  ASSERT_EQ(event1.duplicate(ZX_RIGHT_SAME_RIGHTS, &event1_dup1), ZX_OK);
  ASSERT_EQ(event1.duplicate(ZX_RIGHT_SAME_RIGHTS, &event1_dup2), ZX_OK);

  zx::event event2;
  ASSERT_EQ(zx::event::create(0u, &event2), ZX_OK);
  zx::event event2_dup;
  ASSERT_EQ(event2.duplicate(ZX_RIGHT_SAME_RIGHTS, &event2_dup), ZX_OK);

  // Import two duplicate handles to `event1` into two distinct `Semaphore` objects.
  auto sem1_a = escher->semaphore_pool()->AllocateAndImport(std::move(event1_dup1));
  auto sem1_b = escher->semaphore_pool()->AllocateAndImport(std::move(event1_dup2));
  auto sem2 = escher->semaphore_pool()->AllocateAndImport(std::move(event2_dup));

  ASSERT_NE(sem1_a, sem1_b);
  EXPECT_NE(sem1_a->vk_semaphore(), sem1_b->vk_semaphore());
  EXPECT_NE(sem1_a->koid(), Semaphore::kInvalidKoid);
  EXPECT_EQ(sem1_a->koid(), sem1_b->koid());
  EXPECT_NE(sem1_a->koid(), sem2->koid());

  cb->AddWaitSemaphore(sem1_a, vk::PipelineStageFlagBits::eColorAttachmentOutput);
  EXPECT_EQ(cb->NumWaitSemaphores(), 1u);
  ASSERT_EQ(GetWaitSemaphores(cb).size(), 1u);
  EXPECT_EQ(GetWaitSemaphores(cb)[0], sem1_a);

  // Adding a distinct `Semaphore` wrapping the same underlying Zircon event must coalesce into the
  // existing wait entry and merge pipeline stages, preventing a double-wait deadlock on submit.
  cb->AddWaitSemaphore(sem1_b, vk::PipelineStageFlagBits::eTransfer);
  EXPECT_EQ(cb->NumWaitSemaphores(), 1u);
  ASSERT_EQ(GetWaitSemaphores(cb).size(), 1u);
  EXPECT_EQ(GetWaitSemaphores(cb)[0], sem1_a);
  ASSERT_EQ(GetWaitSemaphoreStages(cb).size(), 1u);
  EXPECT_EQ(GetWaitSemaphoreStages(cb)[0], vk::PipelineStageFlagBits::eColorAttachmentOutput |
                                               vk::PipelineStageFlagBits::eTransfer);

  cb->AddWaitSemaphore(sem2, vk::PipelineStageFlagBits::eFragmentShader);
  EXPECT_EQ(cb->NumWaitSemaphores(), 2u);
  ASSERT_EQ(GetWaitSemaphores(cb).size(), 2u);
  EXPECT_EQ(GetWaitSemaphores(cb)[0], sem1_a);
  EXPECT_EQ(GetWaitSemaphores(cb)[1], sem2);

  // The wait count and merged stage assertions above are what this test verifies; signal both
  // events so `Submit()` and `TearDown()` complete cleanly.
  ASSERT_EQ(event1.signal(0u, ZX_EVENT_SIGNALED), ZX_OK);
  ASSERT_EQ(event2.signal(0u, ZX_EVENT_SIGNALED), ZX_OK);
  EXPECT_TRUE(cb->Submit(nullptr));
}

VK_TEST_F(CommandBufferTest, AddSignalSemaphoreDeduplicatesSameKoid) {
  auto escher = test::GetEscher();
  auto cb = CommandBuffer::NewForGraphics(escher, false);

  zx::event event1;
  ASSERT_EQ(zx::event::create(0u, &event1), ZX_OK);
  zx::event event1_dup1;
  zx::event event1_dup2;
  ASSERT_EQ(event1.duplicate(ZX_RIGHT_SAME_RIGHTS, &event1_dup1), ZX_OK);
  ASSERT_EQ(event1.duplicate(ZX_RIGHT_SAME_RIGHTS, &event1_dup2), ZX_OK);

  zx::event event2;
  ASSERT_EQ(zx::event::create(0u, &event2), ZX_OK);
  zx::event event2_dup;
  ASSERT_EQ(event2.duplicate(ZX_RIGHT_SAME_RIGHTS, &event2_dup), ZX_OK);

  // Import one handle via `SemaphorePool::AllocateAndImport` and one via `GetSemaphoreForEvent` to
  // verify that both import paths record the event KOID.
  auto sem1_a = escher->semaphore_pool()->AllocateAndImport(std::move(event1_dup1));
  auto sem1_b = GetSemaphoreForEvent(escher->device(), std::move(event1_dup2));
  auto sem2 = escher->semaphore_pool()->AllocateAndImport(std::move(event2_dup));

  ASSERT_NE(sem1_a, sem1_b);
  EXPECT_TRUE(sem1_b->is_imported());
  EXPECT_NE(sem1_a->koid(), Semaphore::kInvalidKoid);
  EXPECT_EQ(sem1_a->koid(), sem1_b->koid());
  EXPECT_NE(sem1_a->koid(), sem2->koid());

  cb->AddSignalSemaphore(sem1_a);
  EXPECT_EQ(cb->NumSignalSemaphores(), 1u);
  EXPECT_TRUE(cb->ContainsSignalSemaphore(sem1_a));
  EXPECT_TRUE(cb->ContainsSignalSemaphore(sem1_b));
  EXPECT_FALSE(cb->ContainsSignalSemaphore(sem2));

  cb->AddSignalSemaphore(sem1_b);
  EXPECT_EQ(cb->NumSignalSemaphores(), 1u);

  cb->AddSignalSemaphore(sem2);
  EXPECT_EQ(cb->NumSignalSemaphores(), 2u);
  EXPECT_TRUE(cb->ContainsSignalSemaphore(sem2));

  EXPECT_TRUE(cb->Submit(nullptr));
  EXPECT_EQ(event1.wait_one(ZX_EVENT_SIGNALED, zx::deadline_after(zx::sec(30)), nullptr), ZX_OK);
  EXPECT_EQ(event2.wait_one(ZX_EVENT_SIGNALED, zx::deadline_after(zx::sec(30)), nullptr), ZX_OK);
}
#endif

// Verifies that calling `Cleanup()` from within a `CommandBufferFinishedCallback` runs `cb1`'s
// callback once, keeps an unsubmitted `cb2` pending in `pending_buffers_`, and lets a later
// `Cleanup()` drain `cb2` after it is submitted (https://fxbug.dev/519646989).
VK_TEST_F(CommandBufferTest, ReentrantCleanupDoesNotCorruptPendingQueue) {
  auto escher = test::GetEscher();
  auto cb1 = CommandBuffer::NewForGraphics(escher, false);
  // Allocate `cb2` before submitting `cb1` so `cb2` sits right behind `cb1` in `pending_buffers_`
  // in an unsubmitted (not-ready-to-retire) state.
  auto cb2 = CommandBuffer::NewForGraphics(escher, false);

  int cb1_callback_count = 0;
  int cb2_callback_count = 0;

  EXPECT_TRUE(cb1->Submit([&]() {
    ++cb1_callback_count;
    // Nested `Cleanup()` returns false immediately; the second call checks that the re-entrancy
    // guard remains set after the first nested call returns.
    EXPECT_FALSE(escher->Cleanup());
    EXPECT_FALSE(escher->Cleanup());
  }));

  EXPECT_VK_SUCCESS(escher->vk_device().waitIdle());
  // `cb1` should retire and run its callback once, while `cb2` remains pending because it has not
  // been submitted yet.
  EXPECT_FALSE(escher->Cleanup());
  EXPECT_EQ(cb1_callback_count, 1);
  EXPECT_EQ(cb2_callback_count, 0);

  // Now submit `cb2` and verify a subsequent `Cleanup()` drains it cleanly.
  EXPECT_TRUE(cb2->Submit([&]() { ++cb2_callback_count; }));
  EXPECT_VK_SUCCESS(escher->vk_device().waitIdle());
  EXPECT_TRUE(escher->Cleanup());
  EXPECT_EQ(cb1_callback_count, 1);
  EXPECT_EQ(cb2_callback_count, 1);
}

// Verifies that `CommandBufferPool::Cleanup()` returns a retired buffer to `free_buffers_` and
// notifies `CommandBufferSequencer` before running the buffer's finished callback.
VK_TEST_F(CommandBufferTest, CleanupCompletesRetirementBeforeRunningCallback) {
  auto escher = test::GetEscher();
  BufferCache buffer_cache(escher->GetWeakPtr());
  impl::CommandBufferPool pool(
      escher->vk_device(), escher->device()->vk_main_queue(),
      escher->device()->vk_main_queue_family(), escher->command_buffer_sequencer(),
      /*supports_graphics_and_compute=*/true, /*use_protected_memory=*/false);

  BufferPtr host_buffer = buffer_cache.NewHostBuffer(256);
  ASSERT_TRUE(host_buffer);
  EXPECT_EQ(buffer_cache.free_buffer_count(), 0u);

  impl::CommandBuffer* first_cb = pool.GetCommandBuffer();
  first_cb->KeepAlive(host_buffer);

  impl::CommandBuffer* reused_cb = nullptr;
  bool second_callback_ran = false;

  EXPECT_TRUE(
      first_cb->Submit(pool.queue(), [&, captured_buffer = std::move(host_buffer)]() mutable {
        // Even though `first_cb` was the only pending buffer and has already been popped from
        // `pending_buffers_`, the initial `pool.Cleanup()` call is still running this callback and
        // has not finished yet, so a nested `pool.Cleanup()` call must return false.
        EXPECT_FALSE(pool.Cleanup());

        // Because `CommandBufferFinished()` ran before this callback, dropping the last reference
        // to `captured_buffer` recycles it immediately into `buffer_cache` without deferring.
        EXPECT_EQ(buffer_cache.free_buffer_count(), 0u);
        captured_buffer.reset();
        EXPECT_EQ(buffer_cache.free_buffer_count(), 1u);

        // Because `first_cb` was pushed to `free_buffers_` before this callback ran, requesting a
        // new `CommandBuffer` reuses `first_cb` instead of allocating a second Vulkan command
        // buffer.
        reused_cb = pool.GetCommandBuffer();
        EXPECT_EQ(reused_cb, first_cb);
        EXPECT_TRUE(reused_cb->Submit(pool.queue(), [&]() { second_callback_ran = true; }));
        EXPECT_VK_SUCCESS(escher->vk_device().waitIdle());
      }));

  EXPECT_VK_SUCCESS(escher->vk_device().waitIdle());
  EXPECT_TRUE(pool.Cleanup());
  EXPECT_EQ(reused_cb, first_cb);
  EXPECT_TRUE(second_callback_ran);
  EXPECT_EQ(buffer_cache.free_buffer_count(), 1u);
}

// Verifies that when a `CommandBufferFinishedCallback` submits a new command buffer (`cb3`) and
// calls `Cleanup()`, already-pending buffers (`cb2`) and `cb3` are deferred to the active
// `Cleanup()` call on the stack and run their callbacks in FIFO order after `cb1`'s callback
// returns.
VK_TEST_F(CommandBufferTest, ReentrantCleanupPreservesFifoOrder) {
  auto escher = test::GetEscher();
  auto cb1 = CommandBuffer::NewForGraphics(escher, false);
  auto cb2 = CommandBuffer::NewForGraphics(escher, false);

  std::vector<int> execution_order;
  CommandBufferPtr cb3;

  EXPECT_TRUE(cb1->Submit([&]() {
    execution_order.push_back(1);
    cb3 = CommandBuffer::NewForGraphics(escher, false);
    EXPECT_TRUE(cb3->Submit([&]() { execution_order.push_back(3); }));
    EXPECT_VK_SUCCESS(escher->vk_device().waitIdle());
    // Nested `Cleanup()` returns false without retiring `cb2` or `cb3`; the ongoing `Cleanup()`
    // call will continue draining `pending_buffers_` in FIFO order (`cb2`, then `cb3`) after this
    // callback returns.
    EXPECT_FALSE(escher->Cleanup());
    EXPECT_EQ(execution_order, std::vector<int>{1});
  }));
  EXPECT_TRUE(cb2->Submit([&]() { execution_order.push_back(2); }));

  EXPECT_VK_SUCCESS(escher->vk_device().waitIdle());
  EXPECT_TRUE(escher->Cleanup());

  const std::vector<int> expected_order = {1, 2, 3};
  EXPECT_EQ(execution_order, expected_order);
}

// Verifies that when CommandBuffer::Submit() fails, Retire() cleans up the buffer and releases
// captured resources without invoking the finished callback (https://fxbug.dev/519645546).
VK_TEST_F(CommandBufferTest, FailedSubmitSkipsCallbackAndRecyclesResources) {
  auto escher = test::GetEscher();
  BufferCache buffer_cache(escher->GetWeakPtr());
  impl::CommandBufferPool pool(
      escher->vk_device(), escher->device()->vk_main_queue(),
      escher->device()->vk_main_queue_family(), escher->command_buffer_sequencer(),
      /*supports_graphics_and_compute=*/true, /*use_protected_memory=*/false);

  BufferPtr host_buffer = buffer_cache.NewHostBuffer(256);
  ASSERT_TRUE(host_buffer);
  EXPECT_EQ(buffer_cache.free_buffer_count(), 0u);

  impl::CommandBuffer* first_cb = pool.GetCommandBuffer();
  first_cb->KeepAlive(host_buffer);

  bool failed_callback_ran = false;
  bool captured_closure_destroyed = false;
  EXPECT_FALSE(first_cb->Submit(vk::Queue(), [&, captured_buffer = std::move(host_buffer),
                                              on_destroy = fit::defer([&]() {
                                                captured_closure_destroyed = true;
                                              })]() { failed_callback_ran = true; }));

  // Submit() must retain `callback_` until Retire() rather than destroying it immediately.
  EXPECT_FALSE(captured_closure_destroyed);
  EXPECT_EQ(buffer_cache.free_buffer_count(), 0u);

  // Cleanup() retires `first_cb`, drops its callback without running it, and recycles
  // `captured_buffer` into `buffer_cache`.
  EXPECT_TRUE(pool.Cleanup());
  EXPECT_FALSE(failed_callback_ran);
  EXPECT_TRUE(captured_closure_destroyed);
  EXPECT_EQ(buffer_cache.free_buffer_count(), 1u);

  // Verify `first_cb` was reset and returned to `free_buffers_` with no stale callback retained.
  impl::CommandBuffer* reused_cb = pool.GetCommandBuffer();
  EXPECT_EQ(reused_cb, first_cb);
  bool reused_callback_ran = false;
  EXPECT_TRUE(reused_cb->Submit(pool.queue(), [&]() { reused_callback_ran = true; }));
  EXPECT_VK_SUCCESS(escher->vk_device().waitIdle());
  EXPECT_TRUE(pool.Cleanup());
  EXPECT_FALSE(failed_callback_ran);
  EXPECT_TRUE(reused_callback_ran);
}

// Verifies that when a command buffer fails Submit() behind an earlier pending buffer in the same
// pool, its captured resources remain alive until the earlier buffer has retired.
VK_TEST_F(CommandBufferTest, FailedSubmitRetainsCapturesUntilPriorPendingBuffersRetire) {
  auto escher = test::GetEscher();
  auto cb1 = CommandBuffer::NewForGraphics(escher, false);
  auto cb2 = CommandBuffer::NewForGraphics(escher, false);

  bool cb1_callback_ran = false;
  bool cb2_callback_ran = false;
  bool cb2_captures_destroyed = false;

  // Fail submission of `cb2` while `cb1` is still unsubmitted at the front of `pending_buffers_`.
  EXPECT_FALSE(cb2->Submit(vk::Queue(), [&, on_destroy = fit::defer([&]() {
                                              cb2_captures_destroyed = true;
                                            })]() { cb2_callback_ran = true; }));
  EXPECT_FALSE(cb2_captures_destroyed);

  // Because `cb1` has not retired yet, Cleanup() must not retire `cb2` or destroy its captures.
  EXPECT_FALSE(escher->Cleanup());
  EXPECT_FALSE(cb2_callback_ran);
  EXPECT_FALSE(cb2_captures_destroyed);

  // Once `cb1` is submitted and finishes, Cleanup() retires both `cb1` and `cb2` in FIFO order,
  // running `cb1`'s callback and destroying `cb2`'s captures without invoking `cb2`'s callback.
  EXPECT_TRUE(cb1->Submit([&]() {
    cb1_callback_ran = true;
    // `cb2` sits behind `cb1`, so `cb2`'s captures must still be alive while `cb1`'s callback runs.
    EXPECT_FALSE(cb2_captures_destroyed);
  }));
  EXPECT_VK_SUCCESS(escher->vk_device().waitIdle());
  EXPECT_TRUE(escher->Cleanup());
  EXPECT_TRUE(cb1_callback_ran);
  EXPECT_FALSE(cb2_callback_ran);
  EXPECT_TRUE(cb2_captures_destroyed);
}

// Verifies that a failed submission on the transfer pool waiting on a semaphore from the graphics
// pool does not run its completion callback when cleaned up.
VK_TEST_F(CommandBufferTest, FailedSubmitWithWaitSemaphoreDoesNotRunCallback) {
  auto escher = test::GetEscher();
  auto sem = Semaphore::New(escher->vk_device());

  auto graphics_cb = CommandBuffer::NewForGraphics(escher, false);
  graphics_cb->AddSignalSemaphore(sem);

  auto transfer_cb = CommandBuffer::NewForTransfer(escher);
  transfer_cb->AddWaitSemaphore(sem, vk::PipelineStageFlagBits::eTransfer);

  bool transfer_callback_ran = false;
  EXPECT_FALSE(transfer_cb->Submit(vk::Queue(), [&]() { transfer_callback_ran = true; }));

  // Even if Cleanup() runs before `graphics_cb` is submitted or completed, `transfer_cb`'s
  // callback must not be invoked.
  EXPECT_FALSE(escher->Cleanup());
  EXPECT_FALSE(transfer_callback_ran);

  // Submit and retire `graphics_cb` so all pending buffers are cleaned up, and verify that
  // `transfer_cb`'s callback still never ran.
  bool graphics_callback_ran = false;
  EXPECT_TRUE(graphics_cb->Submit([&]() { graphics_callback_ran = true; }));
  EXPECT_VK_SUCCESS(escher->vk_device().waitIdle());
  EXPECT_TRUE(escher->Cleanup());
  EXPECT_TRUE(graphics_callback_ran);
  EXPECT_FALSE(transfer_callback_ran);
}

// Verifies that Frame::EndFrame() skips `frame_retired_callback` on a failed submission while still
// resetting Frame's internal state and releasing `keep_alive_` resources upon retirement.
VK_TEST_F(CommandBufferTest, FrameEndFrameFailedSubmitSkipsCallbackAndCleansUpFrame) {
  auto escher = test::GetEscher();
  BufferCache buffer_cache(escher->GetWeakPtr());
  BufferPtr host_buffer = buffer_cache.NewHostBuffer(256);
  ASSERT_TRUE(host_buffer);

  bool frame_retired_callback_ran = false;
  {
    auto frame = escher->NewFrame("test_frame", 0, /*enable_gpu_logging=*/false);
    // Submit a valid partial frame first and allocate a uniform buffer so `frame` has in-flight
    // resources in `keep_alive_` when EndFrame() is called.
    frame->AllocateUniform(64, 16);
    frame->SubmitPartialFrame(SemaphorePtr());

    frame->cmds()->KeepAlive(host_buffer);
    host_buffer = nullptr;
    EXPECT_EQ(buffer_cache.free_buffer_count(), 0u);

    // Force the final submission in EndFrame() to fail, and skip immediate Cleanup() so `frame`
    // leaves scope while the command buffers are still unretired.
    SetQueueForTest(frame, vk::Queue());
    frame->EndFrame(
        SemaphorePtr(), [&]() { frame_retired_callback_ran = true; },
        /*skip_escher_cleanup=*/true);
  }

  // Until Cleanup() retires the command buffers, `this_frame` inside the failed command buffer's
  // deferred cleanup action keeps `Frame` and its `keep_alive_` resources alive, and `host_buffer`
  // is not recycled into `buffer_cache`.
  EXPECT_FALSE(frame_retired_callback_ran);
  EXPECT_EQ(buffer_cache.free_buffer_count(), 0u);

  EXPECT_VK_SUCCESS(escher->vk_device().waitIdle());
  EXPECT_TRUE(escher->Cleanup());
  EXPECT_FALSE(frame_retired_callback_ran);
  EXPECT_EQ(buffer_cache.free_buffer_count(), 1u);
}

}  // namespace
