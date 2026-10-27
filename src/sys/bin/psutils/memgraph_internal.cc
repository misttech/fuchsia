// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "memgraph_internal.h"

#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <zircon/status.h>
#include <zircon/syscalls.h>
#include <zircon/syscalls/exception.h>
#include <zircon/syscalls/object.h>

#include <ostream>
#include <print>
#include <vector>

#include "object-utils.h"
#include "vmo-utils.h"

namespace {

// Prints info about VMOs and their relationship to a process.
// Assumes we're in the middle of dumping a process.
// TODO(https://fxbug.dev/575725855): Insert some special entry if count < avail?
void print_vmos(std::ostream& out, const zx_info_vmo_t* vmos, size_t count) {
  if (count == 0) {
    // Should never happen, but don't print anything in this case.
    return;
  }

  // List of VMOs that this task points to. Should only contain fields that
  // are fundamental parts of the VMO and do not change based on how the VMO
  // is used or referred to.
  std::println(out, ",\n   \"vmos\": [");
  for (size_t i = 0; i < count; i++) {
    const zx_info_vmo_t* vmo = vmos + i;
    char delim = i < (count - 1) ? ',' : ' ';
    std::println(out,
                 "      {{"
                 "\"koid\": {}, "
                 "\"name\": \"{}\", "  // TODO(https://fxbug.dev/575725855): Escape quotes
                 "\"size_bytes\": {}, "
                 "\"parent_koid\": {}, "
                 "\"num_children\": {}, "
                 "\"num_mappings\": {}, "
                 "\"share_count\": {}, "
                 // TODO(https://fxbug.dev/575725855): copy-on-write? phys/paged?
                 "\"committed_bytes\": {}, "
                 "\"committed_scaled_bytes\": {}}}{}",
                 vmo->koid, static_cast<const char*>(vmo->name), vmo->size_bytes, vmo->parent_koid,
                 vmo->num_children, vmo->num_mappings, vmo->share_count, vmo->committed_bytes,
                 vmo->committed_scaled_bytes, delim);
  }
  std::println(out, "   ],");
  // List of references from this task to the VMOs listed above. May include
  // information specific to this particular use of a given VMO.
  std::println(out, "   \"vmo_refs\": [");
  for (size_t i = 0; i < count; i++) {
    const zx_info_vmo_t* vmo = vmos + i;
    char delim = i < (count - 1) ? ',' : ' ';
    std::print(out,
               "      {{"
               "\"vmo_koid\": {}, "
               "\"via\": [",
               vmo->koid);
    bool need_comma = false;
    if (vmo->flags & ZX_INFO_VMO_VIA_HANDLE) {
      std::print(out, "\"HANDLE\"");
      need_comma = true;
    }
    if (vmo->flags & ZX_INFO_VMO_VIA_IOB_HANDLE) {
      std::print(out, "{}\"IOB_HANDLE\"", need_comma ? ", " : "");
      need_comma = true;
    }
    if (vmo->flags & ZX_INFO_VMO_VIA_MAPPING) {
      std::print(out, "{}\"MAPPING\"", need_comma ? ", " : "");
      // Future improvement: Could use ZX_INFO_PROCESS_MAPS to include
      // specifics of how this VMO is mapped.
    }
    std::print(out, "]");
    if (vmo->flags & (ZX_INFO_VMO_VIA_HANDLE | ZX_INFO_VMO_VIA_IOB_HANDLE)) {
      need_comma = false;
      std::print(out, ", \"handle_rights\": [");

      struct RightEntry {
        zx_rights_t right;
        const char* name;
      };
      constexpr RightEntry kRights[] = {
          {.right = ZX_RIGHT_READ, .name = "READ"},
          {.right = ZX_RIGHT_WRITE, .name = "WRITE"},
          {.right = ZX_RIGHT_EXECUTE, .name = "EXECUTE"},
          {.right = ZX_RIGHT_MAP, .name = "MAP"},
          {.right = ZX_RIGHT_DUPLICATE, .name = "DUPLICATE"},
          {.right = ZX_RIGHT_TRANSFER, .name = "TRANSFER"},
      };
      for (const auto& entry : kRights) {
        if (vmo->handle_rights & entry.right) {
          std::print(out, "{}\"{}\"", need_comma ? ", " : "", entry.name);
          need_comma = true;
        }
      }

      std::print(out, "]");
    }
    std::println(out, "}}{}", delim);
  }
  std::print(out, "   ]");
}

void GetTaskName(zx_handle_t task, zx_koid_t koid, char out_name[ZX_MAX_NAME_LEN]) {
  zx_status_t s = zx_object_get_property(task, ZX_PROP_NAME, out_name, ZX_MAX_NAME_LEN);
  if (s != ZX_OK) {
    std::println(stderr, "WARNING: failed to get name of task {}: {} ({})", koid,
                 zx_status_get_string(s), s);
    snprintf(out_name, ZX_MAX_NAME_LEN, "<UNKNOWN>");
    // This is unfortunate, but not worth a partial failure
    // since the overall structure of the output is still intact.
  }
  // TODO(https://fxbug.dev/575725855): Escape quotes in name
}

}  // namespace

JsonTaskEnumerator::JsonTaskEnumerator(zx_koid_t self_koid, bool show_threads, bool show_vmos,
                                       bool show_handle_stats, std::ostream& out)
    : self_koid_(self_koid),
      show_threads_(show_threads),
      show_vmos_(show_vmos),
      show_handle_stats_(show_handle_stats),
      out_(out) {}

const std::vector<zx_status_t>& JsonTaskEnumerator::partial_failures() const {
  return partial_failures_;
}

bool JsonTaskEnumerator::has_on_job() const { return true; }

bool JsonTaskEnumerator::has_on_process() const { return true; }

bool JsonTaskEnumerator::has_on_thread() const { return show_threads_; }

zx_status_t JsonTaskEnumerator::OnJob(int depth, zx_handle_t job, zx_koid_t koid,
                                      zx_koid_t parent_koid) {
  char name[ZX_MAX_NAME_LEN];
  GetTaskName(job, koid, name);

  char parent_id[ZX_MAX_NAME_LEN + 16];
  if (parent_koid == 0) {
    // This is the root job, which we treat as a child of the
    // system VMO arena node.
    snprintf(parent_id, sizeof(parent_id), "kernel/vmo");
  } else {
    snprintf(parent_id, sizeof(parent_id), "j/%" PRIu64, parent_koid);
  }

  std::println(out_,
               "  {{"
               "\"id\": \"j/{}\", "
               "\"type\": \"j\", "
               "\"koid\": {}, "
               "\"parent\": \"{}\", "
               "\"name\": \"{}\""
               "}},",
               koid, koid, static_cast<const char*>(parent_id), static_cast<const char*>(name));

  return ZX_OK;
}

zx_status_t JsonTaskEnumerator::OnProcess(int depth, zx_handle_t process, zx_koid_t koid,
                                          zx_koid_t parent_koid) {
  char name[ZX_MAX_NAME_LEN];
  GetTaskName(process, koid, name);

  // Print basic info.
  std::print(out_,
             "  {{"
             "\"id\": \"p/{}\", "
             "\"type\": \"p\", "
             "\"koid\": {}, "
             "\"parent\": \"j/{}\", "
             "\"name\": \"{}\"",
             koid, koid, parent_koid, static_cast<const char*>(name));

  // Print memory usage summaries.
  zx_info_task_stats_t info;
  zx_status_t s =
      zx_object_get_info(process, ZX_INFO_TASK_STATS, &info, sizeof(info), nullptr, nullptr);
  if (s == ZX_ERR_BAD_STATE) {
    // Process has exited, but has not been destroyed.
    // Default to zero for all sizes.
    info = {};
    s = ZX_OK;
  }
  if (s != ZX_OK) {
    std::println(stderr, "WARNING: failed to get mem stats for process {}: {} ({})", koid,
                 zx_status_get_string(s), s);
    partial_failures_.push_back(s);
  } else {
    std::print(out_,
               ", "
               "\"private_bytes\": {}, "
               "\"shared_bytes\": {}, "
               "\"pss_bytes\": {}",
               info.mem_private_bytes, info.mem_shared_bytes,
               info.mem_private_bytes + info.mem_scaled_shared_bytes);
  }

  // Print the process's VMOs. The same VMO may appear several
  // times in this list; it's up to the consumer of this output
  // to de-duplicate.
  if (show_vmos_ && koid != self_koid_) {
    zx_info_vmo_t* vmos;
    size_t count = 0;
    size_t avail = 0;
    s = get_vmos(process, &vmos, &count, &avail);
    if (s == ZX_ERR_BAD_STATE) {
      // Process has exited, but has not been destroyed.
    } else if (s != ZX_OK) {
      std::println(stderr, "WARNING: failed to read VMOs for process {}: {} ({})", koid,
                   zx_status_get_string(s), s);
      partial_failures_.push_back(s);
    } else {
      if (count < avail) {
        std::println(stderr, "WARNING: failed to read all VMOs for process {}: count {} < avail {}",
                     koid, count, avail);
        partial_failures_.push_back(ZX_ERR_BUFFER_TOO_SMALL);
        // Keep going with the truncated list.
      }
      print_vmos(out_, vmos, count);
      free(vmos);
    }
  }

  if (show_handle_stats_) {
    zx_info_process_handle_stats_t info = {};
    s = zx_object_get_info(process, ZX_INFO_PROCESS_HANDLE_STATS, &info, sizeof(info), nullptr,
                           nullptr);
    if (s != ZX_OK) {
      std::println(stderr, "WARNING: failed to read handle stats for process {}: {} ({})", koid,
                   zx_status_get_string(s), s);
      partial_failures_.push_back(s);
    } else {
      std::print(out_, ",\n   \"handle_stats\": {{");
      size_t count = 0;
      for (zx_obj_type_t i = 0; i < ZX_OBJ_TYPE_UPPER_BOUND; i++) {
        if (!info.handle_count[i]) {
          continue;
        }
        if (count++ > 0) {
          std::print(out_, ",");
        }
        std::print(out_, "\n      \"{}\": {}", obj_type_get_name(i), info.handle_count[i]);
      }
      std::print(out_, "\n   }}");
    }
  }

  std::println(out_, "}},");

  return ZX_OK;
}

zx_status_t JsonTaskEnumerator::OnThread(int depth, zx_handle_t thread, zx_koid_t koid,
                                         zx_koid_t parent_koid) {
  char name[ZX_MAX_NAME_LEN];
  GetTaskName(thread, koid, name);

  // Print basic info.
  std::print(out_,
             "  {{"
             "\"id\": \"t/{}\", "
             "\"type\": \"t\", "
             "\"koid\": {}, "
             "\"parent\": \"p/{}\", "
             "\"name\": \"{}\"",
             koid, koid, parent_koid, static_cast<const char*>(name));

  // Print state.
  zx_info_thread_t info;
  zx_status_t s = zx_object_get_info(thread, ZX_INFO_THREAD, &info, sizeof(info), nullptr, nullptr);
  if (s != ZX_OK) {
    std::println(stderr, "WARNING: failed to get info for thread {}: {} ({})", koid,
                 zx_status_get_string(s), s);
    partial_failures_.push_back(s);
  } else {
    const char* state = "<UNKNOWN>";
    if (info.state == ZX_THREAD_STATE_BLOCKED_EXCEPTION &&
        info.wait_exception_channel_type != ZX_EXCEPTION_CHANNEL_TYPE_NONE) {
      state = "EXCEPTION";
    } else {
      switch (ZX_THREAD_STATE_BASIC(info.state)) {
        case ZX_THREAD_STATE_NEW:
          state = "NEW";
          break;
        case ZX_THREAD_STATE_RUNNING:
          state = "RUNNING";
          break;
        case ZX_THREAD_STATE_SUSPENDED:
          state = "SUSPENDED";
          break;
        case ZX_THREAD_STATE_BLOCKED:
          state = "BLOCKED";
          break;
        case ZX_THREAD_STATE_DYING:
          state = "DYING";
          break;
        case ZX_THREAD_STATE_DEAD:
          state = "DEAD";
          break;
        default:
          break;
      }
    }
    std::print(out_, ", \"state\": \"{}\"", state);
  }
  std::println(out_, "}},");
  return ZX_OK;
}
