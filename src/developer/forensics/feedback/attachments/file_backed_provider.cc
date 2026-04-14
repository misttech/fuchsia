// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/feedback/attachments/file_backed_provider.h"

#include <lib/fpromise/promise.h>

#include <string>

#include "src/lib/files/file.h"

namespace forensics::feedback {

FileBackedProvider::FileBackedProvider(std::string path, bool warn_if_unavailable)
    : path_(std::move(path)), warn_if_unavailable_(warn_if_unavailable) {}

::fpromise::promise<AttachmentValue> FileBackedProvider::Get(const uint64_t ticket) {
  AttachmentValue data(Error::kNotSet);

  if (std::string content; files::ReadFileToString(path_, &content)) {
    if (content.empty() && warn_if_unavailable_) {
      FX_LOGS_FIRST_N(WARNING, 1) << "File content was empty: " << path_;
    }

    data = content.empty() ? AttachmentValue(Error::kMissingValue)
                           : AttachmentValue(std::move(content));
  } else {
    if (warn_if_unavailable_) {
      FX_LOGS_FIRST_N(WARNING, 1) << "Failed to read: " << path_;
    }

    data = AttachmentValue(Error::kFileReadFailure);
  }

  return fpromise::make_ok_promise(std::move(data));
}

void FileBackedProvider::ForceCompletion(const uint64_t ticket, const Error error) {}

}  // namespace forensics::feedback
