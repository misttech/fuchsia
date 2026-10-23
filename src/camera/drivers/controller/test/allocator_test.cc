// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/sys/cpp/component_context.h>

#include <cmath>
#include <list>

#include "src/camera/drivers/controller/ge2d_node.h"
#include "src/camera/drivers/controller/memory_allocation.h"
#include "src/camera/drivers/controller/pipeline_manager.h"
#include "src/camera/drivers/controller/sherlock/common_util.h"
#include "src/camera/drivers/controller/sherlock/monitoring_config.h"
#include "src/camera/drivers/controller/sherlock/video_conferencing_config.h"
#include "src/camera/drivers/controller/test/fake_ge2d.h"
#include "src/camera/drivers/controller/test/fake_sysmem.h"
#include "src/camera/lib/format_conversion/format_conversion.h"
#include "src/lib/testing/loop_fixture/test_loop_fixture.h"

// Relaxes memory placement constraints to be more permissive. This improves the reliability of
// allocation as it is no longer competing with other components in the system for fixed
// allocations of e.g. contiguous memory.
void RelaxMemoryConstraints(
    std::vector<fuchsia::sysmem2::BufferCollectionConstraints>& constraints) {
  for (auto& c : constraints) {
    c.mutable_buffer_memory_constraints()->set_cpu_domain_supported(true);
    c.mutable_buffer_memory_constraints()->set_ram_domain_supported(true);
    c.mutable_buffer_memory_constraints()->set_inaccessible_domain_supported(true);
    c.mutable_buffer_memory_constraints()->set_secure_required(false);
    c.mutable_buffer_memory_constraints()->set_physically_contiguous_required(false);
  }
}

// NOTE: In this test, we are actually just unit testing
// the sysmem allocation using different constraints.
namespace camera {

class ControllerMemoryAllocatorTest : public gtest::TestLoopFixture {
 public:
  ControllerMemoryAllocatorTest()
      : context_(sys::ComponentContext::CreateAndServeOutgoingDirectory()) {}

  void SetUp() override {
    ASSERT_EQ(ZX_OK, context_->svc()->Connect(sysmem_allocator_.NewRequest()));
    ASSERT_EQ(ZX_OK, zx::event::create(0, &event_));

    fuchsia::sysmem2::AllocatorSyncPtr sysmem_allocator_B;
    ASSERT_EQ(ZX_OK, context_->svc()->Connect(sysmem_allocator_B.NewRequest()));
    controller_memory_allocator_ =
        std::make_unique<ControllerMemoryAllocator>(std::move(sysmem_allocator_B));

    fuchsia::sysmem2::AllocatorSyncPtr sysmem_allocator_C;
    ASSERT_EQ(ZX_OK, context_->svc()->Connect(sysmem_allocator_C.NewRequest()));
    pipeline_manager_ = std::make_unique<PipelineManager>(
        dispatcher(), std::move(sysmem_allocator_C), isp_, gdc_, ge2d_,
        fit::bind_member(this, &ControllerMemoryAllocatorTest::LoadFirmware));
  }

  void TearDown() override {
    context_ = nullptr;
    sysmem_allocator_ = nullptr;
  }

  fpromise::result<std::pair<zx::vmo, size_t>, zx_status_t> LoadFirmware(const std::string& path) {
    constexpr size_t kVmoSize = 4096;
    zx::vmo vmo;
    zx_status_t status = zx::vmo::create(kVmoSize, 0, &vmo);
    if (status != ZX_OK) {
      return fpromise::error(status);
    }
    return fpromise::ok(std::pair{std::move(vmo), kVmoSize});
  }

  zx::event event_;
  std::unique_ptr<sys::ComponentContext> context_;
  fuchsia::sysmem2::AllocatorSyncPtr sysmem_allocator_;
  std::unique_ptr<ControllerMemoryAllocator> controller_memory_allocator_;
  std::unique_ptr<camera::PipelineManager> pipeline_manager_;
  ddk::IspProtocolClient isp_;
  ddk::GdcProtocolClient gdc_;
  ddk::Ge2dProtocolClient ge2d_;
};

// Validate FR --> GDC1 --> OutputStreamMLDS
// Buffer collection constraints.
TEST_F(ControllerMemoryAllocatorTest, MonitorConfigFR) {
  auto internal_config = MonitorConfigFullRes();
  auto& fr_constraints = *internal_config.output_constraints;
  auto& gdc1_constraints = *internal_config.child_nodes[0].child_nodes[1].input_constraints;
  BufferCollection buffer_collection;
  std::vector<fuchsia::sysmem2::BufferCollectionConstraints> constraints;
  constraints.emplace_back(fidl::Clone(fr_constraints));
  constraints.emplace_back(fidl::Clone(gdc1_constraints));
  RelaxMemoryConstraints(constraints);
  ASSERT_EQ(ZX_OK, controller_memory_allocator_->AllocateSharedMemory(
                       constraints, buffer_collection, "TestMonitorConfigFR"));
  EXPECT_GT(buffer_collection.buffers.settings().buffer_settings().size_bytes(),
            kOutputStreamMlFRHeight * kOutputStreamMlFRWidth);
  EXPECT_TRUE(buffer_collection.buffers.settings().has_image_format_constraints());
  EXPECT_EQ(fuchsia::images2::PixelFormat::NV12,
            buffer_collection.buffers.settings().image_format_constraints().pixel_format());
  EXPECT_EQ(
      kOutputStreamMlFRHeight,
      buffer_collection.buffers.settings().image_format_constraints().required_max_size().height);
  EXPECT_EQ(
      kOutputStreamMlFRWidth,
      buffer_collection.buffers.settings().image_format_constraints().required_max_size().width);
  EXPECT_EQ(
      kIspBytesPerRowDivisor,
      buffer_collection.buffers.settings().image_format_constraints().bytes_per_row_divisor());
  for (uint32_t i = 0; i < buffer_collection.buffers.buffers().size(); i++) {
    EXPECT_TRUE(buffer_collection.buffers.buffers().at(i).vmo().is_valid());
  }
}

// Validate FR --> GDC1
TEST_F(ControllerMemoryAllocatorTest, VideoConfigFRGDC1) {
  auto internal_config = VideoConfigFullRes(false);
  auto& fr_constraints = *internal_config.output_constraints;
  auto& gdc1_constraints = *internal_config.child_nodes[0].input_constraints;
  BufferCollection buffer_collection;
  std::vector<fuchsia::sysmem2::BufferCollectionConstraints> constraints;
  constraints.emplace_back(fidl::Clone(fr_constraints));
  constraints.emplace_back(fidl::Clone(gdc1_constraints));
  RelaxMemoryConstraints(constraints);
  ASSERT_EQ(ZX_OK, controller_memory_allocator_->AllocateSharedMemory(
                       constraints, buffer_collection, "TestVideoConfigFRGDC1"));
  EXPECT_GT(buffer_collection.buffers.settings().buffer_settings().size_bytes(),
            kIspFRWidth * kIspFRHeight);
  EXPECT_TRUE(buffer_collection.buffers.settings().has_image_format_constraints());
  EXPECT_EQ(fuchsia::images2::PixelFormat::NV12,
            buffer_collection.buffers.settings().image_format_constraints().pixel_format());
  EXPECT_EQ(
      kIspFRHeight,
      buffer_collection.buffers.settings().image_format_constraints().required_max_size().height);
  EXPECT_EQ(
      kIspFRWidth,
      buffer_collection.buffers.settings().image_format_constraints().required_max_size().width);
  EXPECT_EQ(
      kIspBytesPerRowDivisor,
      buffer_collection.buffers.settings().image_format_constraints().bytes_per_row_divisor());
  for (uint32_t i = 0; i < buffer_collection.buffers.buffers().size(); i++) {
    EXPECT_TRUE(buffer_collection.buffers.buffers().at(i).vmo().is_valid());
  }
}

// Validate GDC1 ---> GDC2
//               |
//               ---> GE2D
TEST_F(ControllerMemoryAllocatorTest, VideoConfigGDC1GDC2) {
  auto input_node = VideoConfigFullRes(false);
  auto& gdc1_node = input_node.child_nodes[0];
  auto& gdc2_node = gdc1_node.child_nodes[0];
  auto& ge2d_node = gdc1_node.child_nodes[1];
  auto& gdc1_constraints = *gdc1_node.output_constraints;
  auto& gdc2_constraints = *gdc2_node.input_constraints;
  auto& ge2d_constraints = *ge2d_node.input_constraints;
  BufferCollection buffer_collection;
  std::vector<fuchsia::sysmem2::BufferCollectionConstraints> constraints;
  constraints.emplace_back(fidl::Clone(gdc1_constraints));
  constraints.emplace_back(fidl::Clone(gdc2_constraints));
  constraints.emplace_back(fidl::Clone(ge2d_constraints));
  RelaxMemoryConstraints(constraints);
  ASSERT_EQ(ZX_OK, controller_memory_allocator_->AllocateSharedMemory(
                       constraints, buffer_collection, "TestVideoConfigGDC1GDC2"));
  EXPECT_GT(buffer_collection.buffers.settings().buffer_settings().size_bytes(),
            kGdcFRWidth * kGdcFRHeight);
  EXPECT_TRUE(buffer_collection.buffers.settings().has_image_format_constraints());
  EXPECT_EQ(fuchsia::images2::PixelFormat::NV12,
            buffer_collection.buffers.settings().image_format_constraints().pixel_format());
  EXPECT_EQ(
      kGdcFRHeight,
      buffer_collection.buffers.settings().image_format_constraints().required_max_size().height);
  EXPECT_EQ(
      kGdcFRWidth,
      buffer_collection.buffers.settings().image_format_constraints().required_max_size().width);
  EXPECT_EQ(
      kGe2dBytesPerRowDivisor,
      buffer_collection.buffers.settings().image_format_constraints().bytes_per_row_divisor());
  for (uint32_t i = 0; i < buffer_collection.buffers.buffers().size(); i++) {
    EXPECT_TRUE(buffer_collection.buffers.buffers().at(i).vmo().is_valid());
  }
}

// Validate DS --> GDC2 --> (GE2D) --> OutputStreamMonitoring
// This validates only DS --> GDC2
// Buffer collection constraints.
TEST_F(ControllerMemoryAllocatorTest, MonitorConfigDS) {
  auto internal_config = MonitorConfigDownScaledRes();
  auto& ds_constraints = *internal_config.output_constraints;
  auto& gdc2_constraints = *internal_config.child_nodes[0].input_constraints;
  BufferCollection buffer_collection;
  std::vector<fuchsia::sysmem2::BufferCollectionConstraints> constraints;
  constraints.emplace_back(fidl::Clone(ds_constraints));
  constraints.emplace_back(fidl::Clone(gdc2_constraints));
  RelaxMemoryConstraints(constraints);
  ASSERT_EQ(ZX_OK, controller_memory_allocator_->AllocateSharedMemory(
                       constraints, buffer_collection, "TestMonitorConfigDS"));
  EXPECT_GT(buffer_collection.buffers.settings().buffer_settings().size_bytes(),
            kOutputStreamDSHeight * kOutputStreamDSWidth);
  EXPECT_TRUE(buffer_collection.buffers.settings().has_image_format_constraints());
  EXPECT_EQ(fuchsia::images2::PixelFormat::NV12,
            buffer_collection.buffers.settings().image_format_constraints().pixel_format());
  EXPECT_EQ(
      kIspBytesPerRowDivisor,
      buffer_collection.buffers.settings().image_format_constraints().bytes_per_row_divisor());
  for (uint32_t i = 0; i < buffer_collection.buffers.buffers().size(); i++) {
    EXPECT_TRUE(buffer_collection.buffers.buffers().at(i).vmo().is_valid());
  }
}

TEST_F(ControllerMemoryAllocatorTest, ConvertImageFormat2TypeTest) {
  auto internal_config = MonitorConfigFullRes();
  auto& vector_image_formats = internal_config.image_formats;
  fuchsia::images2::ImageFormat& hlcpp_image_format = vector_image_formats[0];

  fuchsia_sysmem::wire::ImageFormat2 c_image_format = ConvertV2ToV1WireType(hlcpp_image_format);

  EXPECT_EQ(c_image_format.pixel_format.type,
            static_cast<const fuchsia_sysmem::wire::PixelFormatType>(
                static_cast<uint32_t>(hlcpp_image_format.pixel_format())));
  EXPECT_EQ(c_image_format.pixel_format.has_format_modifier,
            hlcpp_image_format.has_pixel_format_modifier());
  if (hlcpp_image_format.has_pixel_format_modifier()) {
    EXPECT_EQ(c_image_format.pixel_format.format_modifier.value,
              hlcpp_image_format.pixel_format_modifier());
  }

  EXPECT_EQ(c_image_format.coded_width, hlcpp_image_format.size().width);
  EXPECT_EQ(c_image_format.coded_height, hlcpp_image_format.size().height);
  EXPECT_EQ(c_image_format.bytes_per_row, hlcpp_image_format.bytes_per_row());
  EXPECT_EQ(c_image_format.display_width, hlcpp_image_format.display_rect().width);
  EXPECT_EQ(c_image_format.display_height, hlcpp_image_format.display_rect().height);
  EXPECT_EQ(c_image_format.layers, 1u);
  EXPECT_EQ(c_image_format.has_pixel_aspect_ratio, hlcpp_image_format.has_pixel_aspect_ratio());
  EXPECT_EQ(c_image_format.pixel_aspect_ratio_width, hlcpp_image_format.pixel_aspect_ratio().width);
  EXPECT_EQ(c_image_format.pixel_aspect_ratio_height,
            hlcpp_image_format.pixel_aspect_ratio().height);
}

std::list<std::reference_wrapper<const InternalConfigNode>> GetPathICNodes(
    const std::vector<InternalConfigNode>& roots, const std::vector<uint8_t>& path);

TEST_F(ControllerMemoryAllocatorTest, GetPathICNodesOutOfBounds) {
  auto internal_config = MonitorConfigFullRes();
  std::vector<InternalConfigNode> roots;
  roots.push_back(internal_config);

  // Valid path (size 1)
  std::vector<uint8_t> valid_path = {0};
  auto valid_nodes = GetPathICNodes(roots, valid_path);
  EXPECT_EQ(valid_nodes.size(), 1u);

  // Out of bounds root index
  std::vector<uint8_t> oob_root_path = {1};
  auto oob_root_nodes = GetPathICNodes(roots, oob_root_path);
  EXPECT_TRUE(oob_root_nodes.empty());

  // Out of bounds child index
  // Assuming roots[0] has fewer than 10 children
  std::vector<uint8_t> oob_child_path = {0, 10};
  auto oob_child_nodes = GetPathICNodes(roots, oob_child_path);
  EXPECT_TRUE(oob_child_nodes.empty());
}

TEST_F(ControllerMemoryAllocatorTest, PipelineManagerShutdownSyncTest) {
  auto internal_config = MonitorConfigFullRes();
  std::vector<InternalConfigNode> roots;
  roots.push_back(internal_config);
  pipeline_manager_->SetRoots(roots);

  bool shutdown_complete = false;
  pipeline_manager_->Shutdown([&]() { shutdown_complete = true; });
  RunLoopUntilIdle();
  EXPECT_TRUE(shutdown_complete);
}

namespace {

class TestGe2dNode : public Ge2dNode {
 public:
  using Ge2dNode::Ge2dNode;
  void ShutdownImpl(fit::closure callback) override { callback(); }
};

}  // namespace

TEST_F(ControllerMemoryAllocatorTest, Ge2dNodeCropRectValidation) {
  FakeGe2d fake_ge2d;
  camera::InternalConfigNode internal_ge2d_node;
  internal_ge2d_node.ge2d_info.config_type = camera::Ge2DConfig::GE2D_RESIZE;
  internal_ge2d_node.output_constraints.emplace();

  fuchsia::images2::ImageFormat input_format;
  input_format.mutable_size()->width = 1000;
  input_format.mutable_size()->height = 1000;
  std::vector<fuchsia::images2::ImageFormat> input_formats;
  input_formats.push_back(std::move(input_format));

  ProcessNode::BufferAttachments attachments;
  attachments.input_formats = std::cref(input_formats);

  ProcessNode::FrameCallback frame_callback = [](ProcessNode::FrameToken token,
                                                 frame_metadata_t metadata) {};

  TestGe2dNode ge2d_node(dispatcher(), std::move(attachments), std::move(frame_callback),
                         fake_ge2d.client(), internal_ge2d_node);

  // Test invalid crop max < min bounds
  EXPECT_EQ(ZX_ERR_INVALID_ARGS, ge2d_node.SetCropRect(0.5f, 0.1f, 0.2f, 0.9f));
  EXPECT_EQ(ZX_ERR_INVALID_ARGS, ge2d_node.SetCropRect(0.1f, 0.6f, 0.9f, 0.4f));

  // Test invalid crop max == min bounds
  EXPECT_EQ(ZX_ERR_INVALID_ARGS, ge2d_node.SetCropRect(0.1f, 0.2f, 0.1f, 0.9f));
  EXPECT_EQ(ZX_ERR_INVALID_ARGS, ge2d_node.SetCropRect(0.1f, 0.4f, 0.9f, 0.4f));

  // Test NaN values
  EXPECT_EQ(ZX_ERR_INVALID_ARGS, ge2d_node.SetCropRect(NAN, 0.1f, 0.9f, 0.9f));
  EXPECT_EQ(ZX_ERR_INVALID_ARGS, ge2d_node.SetCropRect(0.1f, NAN, 0.9f, 0.9f));
  EXPECT_EQ(ZX_ERR_INVALID_ARGS, ge2d_node.SetCropRect(0.1f, 0.1f, NAN, 0.9f));
  EXPECT_EQ(ZX_ERR_INVALID_ARGS, ge2d_node.SetCropRect(0.1f, 0.1f, 0.9f, NAN));

  // Test floating point quantization to zero width and height
  // 0.0001f * 1000.0f = 0.1f, + 0.5f = 0.6f -> uint32_t is 0. width = 0.
  EXPECT_EQ(ZX_ERR_INVALID_ARGS, ge2d_node.SetCropRect(0.0f, 0.0f, 0.0001f, 1.0f));
  EXPECT_EQ(ZX_ERR_INVALID_ARGS, ge2d_node.SetCropRect(0.0f, 0.0f, 1.0f, 0.0001f));

  // Test valid crop rect succeeds
  EXPECT_EQ(ZX_OK, ge2d_node.SetCropRect(0.1f, 0.1f, 0.9f, 0.9f));

  // Cleanly shut down node before destruction to satisfy ProcessNode lifecycle contract.
  bool shutdown_done = false;
  ge2d_node.Shutdown([&] { shutdown_done = true; });
  EXPECT_TRUE(shutdown_done);
}

}  // namespace camera
