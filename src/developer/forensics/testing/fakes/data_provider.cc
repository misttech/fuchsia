// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/testing/fakes/data_provider.h"

#include <fidl/fuchsia.feedback/cpp/fidl.h>
#include <fidl/fuchsia.mem/cpp/fidl.h>
#include <lib/syslog/cpp/macros.h>

#include <map>
#include <string>
#include <utility>
#include <vector>

#include "src/developer/forensics/utils/archive.h"
#include "src/lib/fsl/vmo/sized_vmo.h"
#include "src/lib/fxl/strings/string_printf.h"

namespace forensics {
namespace fakes {
namespace {

using namespace fuchsia_feedback;

std::string AnnotationsToJSON(const std::vector<Annotation>& annotations) {
  std::string json = "{\n";
  for (const Annotation& annotation : annotations) {
    json += fxl::StringPrintf("\t\"%s\": \"%s\"\n", annotation.key().c_str(),
                              annotation.value().c_str());
  }
  json += "}\n";
  return json;
}

std::vector<Annotation> CreateAnnotations() {
  return {
      Annotation{{.key = "annotation_key_1", .value = "annotation_value_1"}},
      Annotation{{.key = "annotation_key_2", .value = "annotation_value_2"}},
      Annotation{{.key = "annotation_key_3", .value = "annotation_value_3"}},
  };
}

Attachment CreateSnapshot() {
  std::map<std::string, std::string> attachments;

  attachments["annotations.json"] = AnnotationsToJSON(CreateAnnotations());
  attachments["attachment_key"] = "attachment_value";

  fsl::SizedVmo archive;
  Archive(attachments, &archive);

  return Attachment{{
      .key = "snapshot.zip",
      .value = fuchsia_mem::Buffer{{
          .vmo = std::move(archive.vmo()),
          .size = archive.size(),
      }},
  }};
}

}  // namespace

void DataProvider::GetAnnotations(GetAnnotationsRequest& request,
                                  GetAnnotationsCompleter::Sync& completer) {
  completer.Reply(Annotations{{.annotations2 = CreateAnnotations()}});
}

void DataProvider::GetSnapshot(GetSnapshotRequest& request, GetSnapshotCompleter::Sync& completer) {
  completer.Reply(Snapshot{{
      .archive = CreateSnapshot(),
      .annotations2 = CreateAnnotations(),
  }});
}

}  // namespace fakes
}  // namespace forensics
