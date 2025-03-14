// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_GRAPHICS_TESTS_VKEXT_VULKAN_EXTENSION_TEST_H_
#define SRC_GRAPHICS_TESTS_VKEXT_VULKAN_EXTENSION_TEST_H_

#include <fuchsia/sysmem2/cpp/fidl.h>

#include <gtest/gtest.h>

#include "src/graphics/tests/common/vulkan_context.h"

#include <vulkan/vulkan.hpp>

vk::ImageFormatConstraintsInfoFUCHSIA GetDefaultImageFormatConstraintsInfo(bool yuv);
vk::ImageCreateInfo GetDefaultImageCreateInfo(bool use_protected_memory, VkFormat format,
                                              uint32_t width, uint32_t height, bool linear);
vk::ImageFormatConstraintsInfoFUCHSIA GetDefaultRgbImageFormatConstraintsInfo();
vk::ImageFormatConstraintsInfoFUCHSIA GetDefaultYuvImageFormatConstraintsInfo();
fuchsia::sysmem2::ImageFormatConstraints GetDefaultSysmemImageFormatConstraints();

size_t GetImageByteOffset(size_t x, size_t y, const fuchsia::sysmem2::BufferCollectionInfo &info,
                          size_t width, size_t height);
void CheckImageFill(size_t width, size_t height, void *addr,
                    const fuchsia::sysmem2::BufferCollectionInfo &info, uint32_t fill);

class VulkanExtensionTest : public testing::Test {
 public:
  ~VulkanExtensionTest();
  bool Initialize();
  bool Exec(VkFormat format, uint32_t width, uint32_t height, bool linear,
            bool repeat_constraints_as_non_protected,
            const std::vector<fuchsia::sysmem2::ImageFormatConstraints> &format_constraints =
                std::vector<fuchsia::sysmem2::ImageFormatConstraints>());
  bool ExecBuffer(uint32_t size);

  void set_use_protected_memory(bool use) { use_protected_memory_ = use; }
  bool device_supports_protected_memory() const { return device_supports_protected_memory_; }

  VulkanContext &vulkan_context() { return *ctx_; }

  bool IsMemoryTypeCoherent(uint32_t memoryTypeIndex);
  void WriteImage(vk::DeviceMemory memory, bool is_coherent, VkDeviceSize size_in_bytes,
                  uint32_t fill);
  // Completely fill miplevel 0, array layer 0 of a 4 byte-per-pixel color image.
  void WriteLinearColorImageComplete(vk::DeviceMemory memory, vk::Image image, bool is_coherent,
                                     uint32_t width, uint32_t height, uint32_t fill);
  void CheckLinearImage(vk::Image image, vk::DeviceMemory memory, bool is_coherent, uint32_t width,
                        uint32_t height, uint32_t fill);

 protected:
  using UniqueBufferCollection =
      vk::UniqueHandle<vk::BufferCollectionFUCHSIA, vk::detail::DispatchLoaderDynamic>;

  bool InitVulkan();
  bool InitSysmemAllocator();
  std::vector<fuchsia::sysmem2::BufferCollectionTokenSyncPtr> MakeSharedCollection(
      uint32_t token_count);
  template <uint32_t token_count>
  std::array<fuchsia::sysmem2::BufferCollectionTokenSyncPtr, token_count> MakeSharedCollection() {
    auto token_vector = MakeSharedCollection(token_count);
    std::array<fuchsia::sysmem2::BufferCollectionTokenSyncPtr, token_count> array;
    for (uint32_t i = 0; i < token_vector.size(); i++) {
      array[i] = std::move(token_vector[i]);
    }
    return array;
  }

  UniqueBufferCollection CreateVkBufferCollectionForImage(
      fuchsia::sysmem2::BufferCollectionTokenSyncPtr token,
      const vk::ImageFormatConstraintsInfoFUCHSIA constraints,
      vk::ImageConstraintsInfoFlagsFUCHSIA flags = {});
  fuchsia::sysmem2::BufferCollectionInfo AllocateSysmemCollection(
      std::optional<fuchsia::sysmem2::BufferCollectionConstraints> constraints,
      fuchsia::sysmem2::BufferCollectionTokenSyncPtr token);
  bool InitializeDirectImage(vk::BufferCollectionFUCHSIA collection,
                             vk::ImageCreateInfo image_create_info);
  // Returns the memory type index if it succeeds; otherwise returns std::nullopt.
  std::optional<uint32_t> InitializeDirectImageMemory(vk::BufferCollectionFUCHSIA collection,
                                                      uint32_t expected_count = 1);
  void CheckLinearSubresourceLayout(VkFormat format, uint32_t width);
  void ValidateBufferProperties(const VkMemoryRequirements &requirements,
                                const vk::BufferCollectionFUCHSIA collection,
                                uint32_t expected_count, uint32_t *memory_type_out);

  bool is_initialized_ = false;
  bool use_protected_memory_ = false;
  bool device_supports_protected_memory_ = false;
  std::unique_ptr<VulkanContext> ctx_;

  fuchsia::sysmem2::AllocatorSyncPtr sysmem_allocator_;
  vk::UniqueImage vk_image_;
  vk::UniqueBuffer vk_buffer_;
  vk::UniqueDeviceMemory vk_device_memory_;
  vk::DeviceSize vk_device_memory_size_ = {};
  vk::detail::DispatchLoaderDynamic loader_;
};

#endif  // SRC_GRAPHICS_TESTS_VKEXT_VULKAN_EXTENSION_TEST_H_
