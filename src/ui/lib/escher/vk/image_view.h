// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_LIB_ESCHER_VK_IMAGE_VIEW_H_
#define SRC_UI_LIB_ESCHER_VK_IMAGE_VIEW_H_

#include "src/ui/lib/escher/forward_declarations.h"
#include "src/ui/lib/escher/resources/resource.h"
#include "src/ui/lib/escher/util/trace_macros.h"
#include "src/ui/lib/escher/vk/image.h"
#include "src/ui/lib/escher/vk/sampler.h"

namespace escher {

class ImageView : public Resource {
 public:
  static const ResourceTypeInfo kTypeInfo;
  const ResourceTypeInfo& type_info() const override { return kTypeInfo; }

  // Construct an ImageView, which encapsulates a newly-created VkImageView.
  // `aspect_mask` is used to create the VkImageView, and `resource_recycler`
  // guarantees that the underlying Vulkan resources are not destroyed while
  // still referenced by a pending command buffer. If `sampler` has extension
  // data (e.g., VkSamplerYcbcrConversionInfo), it is passed via pNext during
  // VkImageView creation and `sampler` is retained so the conversion outlives
  // the VkImageView.
  ImageView(ResourceRecycler* resource_recycler, ImagePtr image,
            vk::ImageAspectFlags aspect_mask = vk::ImageAspectFlags(),
            SamplerPtr sampler = nullptr);
  ~ImageView() override;

  static ImageViewPtr New(ImagePtr image, vk::ImageAspectFlags aspect_mask = vk::ImageAspectFlags(),
                          SamplerPtr sampler = nullptr);

  // TODO(https://fxbug.dev/42151125): unfortunately we can't just get the recycler from
  // image->escher(), because that is null for Vulkan swapchain images.
  static ImageViewPtr New(ResourceRecycler* resource_recycler, ImagePtr image,
                          vk::ImageAspectFlags aspect_mask = vk::ImageAspectFlags(),
                          SamplerPtr sampler = nullptr);

  const ImagePtr& image() const { return image_; }
  const SamplerPtr& sampler() const { return sampler_; }
  vk::ImageView vk() const { return image_view_; }

  // TODO(https://fxbug.dev/42151125): for a depth-stencil texture, we may want to sample the
  // depth as floating point and the stencil as integer.  In such cases, we
  // could return a separate view for each, e.g.
  //   return float_image_view_ ? float_image_view_ : image_view_.
  vk::ImageView vk_float_view() const { return image_view_; }
  vk::ImageView vk_integer_view() const { return image_view_; }

  uint32_t width() const { return width_; }
  uint32_t height() const { return height_; }

 private:
  // `image_` and `sampler_` are destroyed after ~ImageView() destroys `image_view_`,
  // ensuring that the underlying VkImage and any VkSamplerYcbcrConversion owned by
  // `sampler_` outlive the VkImageView.
  ImagePtr image_;
  SamplerPtr sampler_;
  vk::ImageView image_view_;
  uint32_t width_;
  uint32_t height_;

  FXL_DISALLOW_COPY_AND_ASSIGN(ImageView);
};

typedef fxl::RefPtr<ImageView> ImageViewPtr;

}  // namespace escher

#endif  // SRC_UI_LIB_ESCHER_VK_IMAGE_VIEW_H_
