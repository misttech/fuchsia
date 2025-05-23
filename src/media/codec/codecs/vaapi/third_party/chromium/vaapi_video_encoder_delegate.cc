// Copyright 2018 The Chromium Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "vaapi_video_encoder_delegate.h"

#include <va/va.h>

// Fuchsia change.
// #include "base/memory/ref_counted_memory.h"
// #include "media/base/video_frame.h"
// #include "media/gpu/codec_picture.h"
#include "src/media/third_party/chromium_media/media/gpu/gpu_video_encode_accelerator_helpers.h"
// #include "media/gpu/macros.h"
// #include "media/gpu/vaapi/va_surface.h"
// #include "media/gpu/vaapi/vaapi_utils.h"
// #include "media/gpu/vaapi/vaapi_wrapper.h"
#include "src/media/third_party/chromium_media/media/video/video_encode_accelerator.h"
#include "vaapi_wrapper.h"

namespace media {

VaapiVideoEncoderDelegate::EncodeJob::EncodeJob(scoped_refptr<VideoFrame> input_frame,
                                                bool keyframe, VASurfaceID input_surface_id,
                                                const gfx::Size& input_surface_size,
                                                scoped_refptr<CodecPicture> picture,
                                                std::unique_ptr<ScopedVABuffer> coded_buffer)
    : input_frame_(input_frame),
      keyframe_(keyframe),
      input_surface_id_(input_surface_id),
      input_surface_size_(input_surface_size),
      picture_(std::move(picture)),
      coded_buffer_(std::move(coded_buffer)) {
  DCHECK(picture_);
  DCHECK(coded_buffer_);
}

VaapiVideoEncoderDelegate::EncodeJob::EncodeJob(scoped_refptr<VideoFrame> input_frame,
                                                bool keyframe)
    : input_frame_(input_frame), keyframe_(keyframe), input_surface_id_(VA_INVALID_ID) {}

VaapiVideoEncoderDelegate::EncodeJob::~EncodeJob() = default;

std::unique_ptr<VaapiVideoEncoderDelegate::EncodeResult>
VaapiVideoEncoderDelegate::EncodeJob::CreateEncodeResult(
    const BitstreamBufferMetadata& metadata) && {
  return std::make_unique<EncodeResult>(std::move(coded_buffer_), metadata);
}

base::TimeDelta VaapiVideoEncoderDelegate::EncodeJob::timestamp() const {
  return input_frame_->timestamp();
}

const scoped_refptr<VideoFrame>& VaapiVideoEncoderDelegate::EncodeJob::input_frame() const {
  return input_frame_;
}

VABufferID VaapiVideoEncoderDelegate::EncodeJob::coded_buffer_id() const {
  return coded_buffer_->id();
}

VASurfaceID VaapiVideoEncoderDelegate::EncodeJob::input_surface_id() const {
  return input_surface_id_;
}

const gfx::Size& VaapiVideoEncoderDelegate::EncodeJob::input_surface_size() const {
  return input_surface_size_;
}

const scoped_refptr<CodecPicture>& VaapiVideoEncoderDelegate::EncodeJob::picture() const {
  return picture_;
}

VaapiVideoEncoderDelegate::EncodeResult::EncodeResult(std::unique_ptr<ScopedVABuffer> coded_buffer,
                                                      const BitstreamBufferMetadata& metadata)
    : coded_buffer_(std::move(coded_buffer)), metadata_(metadata) {}

VaapiVideoEncoderDelegate::EncodeResult::~EncodeResult() = default;

VABufferID VaapiVideoEncoderDelegate::EncodeResult::coded_buffer_id() const {
  return coded_buffer_->id();
}

const BitstreamBufferMetadata& VaapiVideoEncoderDelegate::EncodeResult::metadata() const {
  return metadata_;
}

VaapiVideoEncoderDelegate::VaapiVideoEncoderDelegate(scoped_refptr<VaapiWrapper> vaapi_wrapper,
                                                     base::RepeatingClosure error_cb)
    : vaapi_wrapper_(vaapi_wrapper), error_cb_(std::move(error_cb)) {
  DETACH_FROM_SEQUENCE(sequence_checker_);
}

VaapiVideoEncoderDelegate::~VaapiVideoEncoderDelegate() = default;

size_t VaapiVideoEncoderDelegate::GetBitstreamBufferSize() const {
  DCHECK_CALLED_ON_VALID_SEQUENCE(sequence_checker_);

  return GetEncodeBitstreamBufferSize(GetCodedSize());
}

void VaapiVideoEncoderDelegate::BitrateControlUpdate(uint64_t encoded_chunk_size_bytes) {
  DCHECK_CALLED_ON_VALID_SEQUENCE(sequence_checker_);
}

BitstreamBufferMetadata VaapiVideoEncoderDelegate::GetMetadata(const EncodeJob& encode_job,
                                                               size_t payload_size) {
  DCHECK_CALLED_ON_VALID_SEQUENCE(sequence_checker_);

  return BitstreamBufferMetadata(payload_size, encode_job.IsKeyframeRequested(),
                                 encode_job.timestamp());
}

bool VaapiVideoEncoderDelegate::Encode(EncodeJob& encode_job) {
  if (!PrepareEncodeJob(encode_job)) {
    FX_LOGS(DEBUG) << "Failed preparing an encode job";
    return false;
  }

  const VASurfaceID va_surface_id = encode_job.input_surface_id();
  if (!native_input_mode_ &&
      !vaapi_wrapper_->UploadVideoFrameToSurface(*encode_job.input_frame(), va_surface_id,
                                                 encode_job.input_surface_size())) {
    FX_LOGS(DEBUG) << "Failed to upload frame";
    return false;
  }

  if (!vaapi_wrapper_->ExecuteAndDestroyPendingBuffers(va_surface_id)) {
    FX_LOGS(DEBUG) << "Failed to execute encode";
    return false;
  }

  return true;
}

std::unique_ptr<VaapiVideoEncoderDelegate::EncodeResult> VaapiVideoEncoderDelegate::GetEncodeResult(
    std::unique_ptr<EncodeJob> encode_job) {
  const VASurfaceID va_surface_id = encode_job->input_surface_id();
  const uint64_t encoded_chunk_size =
      vaapi_wrapper_->GetEncodedChunkSize(encode_job->coded_buffer_id(), va_surface_id);
  if (encoded_chunk_size == 0) {
    FX_LOGS(DEBUG) << "Invalid encoded chunk size";
    return nullptr;
  }

  BitrateControlUpdate(encoded_chunk_size);

  auto metadata = GetMetadata(*encode_job, encoded_chunk_size);
  return std::move(*encode_job).CreateEncodeResult(metadata);
}

}  // namespace media
