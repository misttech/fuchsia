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

#include "object-utils.h"
#include "vmo-utils.h"

namespace {

// Prints info about VMOs and their relationship to a process.
// Assumes we're in the middle of dumping a process.
// TODO(dbort): Insert some special entry if count < avail?
void print_vmos(FILE* out, const zx_info_vmo_t* vmos, size_t count) {
  if (count == 0) {
    // Should never happen, but don't print anything in this case.
    return;
  }

  // List of VMOs that this task points to. Should only contain fields that
  // are fundamental parts of the VMO and do not change based on how the VMO
  // is used or referred to.
  fprintf(out, ",\n   \"vmos\": [\n");
  for (size_t i = 0; i < count; i++) {
    const zx_info_vmo_t* vmo = vmos + i;
    char delim = i < (count - 1) ? ',' : ' ';
    fprintf(out,
            "      {"
            "\"koid\": %" PRIu64
            ", "
            "\"name\": \"%s\", "  // TODO(dbort): Escape quotes
            "\"size_bytes\": %" PRIu64
            ", "
            "\"parent_koid\": %" PRIu64
            ", "
            "\"num_children\": %zu, "
            "\"num_mappings\": %zu, "
            "\"share_count\": %zu, "
            // TODO(dbort): copy-on-write? phys/paged?
            "\"committed_bytes\": %" PRIu64 "}%c\n",
            vmo->koid, vmo->name, vmo->size_bytes, vmo->parent_koid, vmo->num_children,
            vmo->num_mappings, vmo->share_count, vmo->committed_bytes, delim);
  }
  fprintf(out, "   ],\n");
  // List of references from this task to the VMOs listed above. May include
  // information specific to this particular use of a given VMO.
  fprintf(out, "   \"vmo_refs\": [\n");
  for (size_t i = 0; i < count; i++) {
    const zx_info_vmo_t* vmo = vmos + i;
    char delim = i < (count - 1) ? ',' : ' ';
    fprintf(out,
            "      {"
            "\"vmo_koid\": %" PRIu64
            ", "
            "\"via\": [",
            vmo->koid);
    bool need_comma = false;
    if (vmo->flags & ZX_INFO_VMO_VIA_HANDLE) {
      fprintf(out, "\"HANDLE\"");
      need_comma = true;
    }
    if (vmo->flags & ZX_INFO_VMO_VIA_MAPPING) {
      fprintf(out, "%s\"MAPPING\"", need_comma ? ", " : "");
      // Future improvement: Could use ZX_INFO_PROCESS_MAPS to include
      // specifics of how this VMO is mapped.
    }
    fprintf(out, "]");
    if (vmo->flags & ZX_INFO_VMO_VIA_HANDLE) {
      need_comma = false;
      fprintf(out, ", \"handle_rights\": [");

#define PRINT_RIGHT(r)                                      \
  do {                                                      \
    if (vmo->handle_rights & ZX_RIGHT_##r) {                \
      fprintf(out, "%s\"" #r "\"", need_comma ? ", " : ""); \
      need_comma = true;                                    \
    }                                                       \
  } while (false)

      PRINT_RIGHT(READ);
      PRINT_RIGHT(WRITE);
      PRINT_RIGHT(EXECUTE);
      PRINT_RIGHT(MAP);
      PRINT_RIGHT(DUPLICATE);
      PRINT_RIGHT(TRANSFER);

#undef PRINT_RIGHT

      fprintf(out, "]");
    }
    fprintf(out, "}%c\n", delim);
  }
  fprintf(out, "   ]");
}

}  // namespace

JsonTaskEnumerator::JsonTaskEnumerator(zx_koid_t self_koid, bool show_threads, bool show_vmos,
                                       bool show_handle_stats, FILE* out)
    : self_koid_(self_koid),
      show_threads_(show_threads),
      show_vmos_(show_vmos),
      show_handle_stats_(show_handle_stats),
      out_(out) {}

zx_status_t JsonTaskEnumerator::partial_failure() const { return partial_failure_; }

bool JsonTaskEnumerator::has_on_job() const { return true; }

bool JsonTaskEnumerator::has_on_process() const { return true; }

bool JsonTaskEnumerator::has_on_thread() const { return show_threads_; }

void JsonTaskEnumerator::set_partial_failure(zx_status_t status) {
  if (partial_failure_ == ZX_OK) {
    partial_failure_ = status;
  }
}

void JsonTaskEnumerator::GetTaskName(zx_handle_t task, zx_koid_t koid,
                                     char out_name[ZX_MAX_NAME_LEN]) {
  zx_status_t s = zx_object_get_property(task, ZX_PROP_NAME, out_name, ZX_MAX_NAME_LEN);
  if (s != ZX_OK) {
    fprintf(stderr, "WARNING: failed to get name of task %" PRIu64 ": %s (%d)\n", koid,
            zx_status_get_string(s), s);
    snprintf(out_name, ZX_MAX_NAME_LEN, "<UNKNOWN>");
    // This is unfortunate, but not worth a partial failure
    // since the overall structure of the output is still intact.
  }
  // TODO(dbort): Escape quotes in name
}

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

  fprintf(out_,
          "  {"
          "\"id\": \"j/%" PRIu64
          "\", "
          "\"type\": \"j\", "
          "\"koid\": %" PRIu64
          ", "
          "\"parent\": \"%s\", "
          "\"name\": \"%s\""
          "},\n",
          koid, koid, parent_id, name);

  return ZX_OK;
}

zx_status_t JsonTaskEnumerator::OnProcess(int depth, zx_handle_t process, zx_koid_t koid,
                                          zx_koid_t parent_koid) {
  char name[ZX_MAX_NAME_LEN];
  GetTaskName(process, koid, name);

  // Print basic info.
  fprintf(out_,
          "  {"
          "\"id\": \"p/%" PRIu64
          "\", "
          "\"type\": \"p\", "
          "\"koid\": %" PRIu64
          ", "
          "\"parent\": \"j/%" PRIu64
          "\", "
          "\"name\": \"%s\"",
          koid, koid, parent_koid, name);

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
    fprintf(stderr, "WARNING: failed to get mem stats for process %" PRIu64 ": %s (%d)\n", koid,
            zx_status_get_string(s), s);
    set_partial_failure(s);
  } else {
    fprintf(out_,
            ", "
            "\"private_bytes\": %zu, "
            "\"shared_bytes\": %zu, "
            "\"pss_bytes\": %zu",
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
    if (s != ZX_OK) {
      fprintf(stderr, "WARNING: failed to read VMOs for process %" PRIu64 ": %s (%d)\n", koid,
              zx_status_get_string(s), s);
      set_partial_failure(s);
    } else {
      if (count < avail) {
        fprintf(stderr,
                "WARNING: failed to read all VMOs for process "
                "%" PRIu64 ": count %zu < avail %zu\n",
                koid, count, avail);
        set_partial_failure(ZX_ERR_BUFFER_TOO_SMALL);
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
      fprintf(stderr,
              "WARNING: failed to read handle stats for process "
              "%" PRIu64 ": %s (%d)\n",
              koid, zx_status_get_string(s), s);
      set_partial_failure(s);
    } else {
      fprintf(out_, ",\n   \"handle_stats\": {");
      size_t count = 0;
      for (zx_obj_type_t i = 0; i < ZX_OBJ_TYPE_UPPER_BOUND; i++) {
        if (!info.handle_count[i]) {
          continue;
        }
        if (count++ > 0) {
          fprintf(out_, ",");
        }
        fprintf(out_, "\n      \"%s\": %u", obj_type_get_name(i), info.handle_count[i]);
      }
      fprintf(out_, "\n   }");
    }
  }

  fprintf(out_, "},\n");

  return ZX_OK;
}

zx_status_t JsonTaskEnumerator::OnThread(int depth, zx_handle_t thread, zx_koid_t koid,
                                         zx_koid_t parent_koid) {
  char name[ZX_MAX_NAME_LEN];
  GetTaskName(thread, koid, name);

  // Print basic info.
  fprintf(out_,
          "  {"
          "\"id\": \"t/%" PRIu64
          "\", "
          "\"type\": \"t\", "
          "\"koid\": %" PRIu64
          ", "
          "\"parent\": \"p/%" PRIu64
          "\", "
          "\"name\": \"%s\"",
          koid, koid, parent_koid, name);

  // Print state.
  zx_info_thread_t info;
  zx_status_t s = zx_object_get_info(thread, ZX_INFO_THREAD, &info, sizeof(info), nullptr, nullptr);
  if (s != ZX_OK) {
    fprintf(stderr, "WARNING: failed to get info for thread %" PRIu64 ": %s (%d)\n", koid,
            zx_status_get_string(s), s);
    set_partial_failure(s);
  } else {
    const char* state = "<UNKNOWN>";
    if (ZX_THREAD_STATE_BASIC(info.state) == ZX_THREAD_STATE_BLOCKED_EXCEPTION &&
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
      }
    }
    fprintf(out_, ", \"state\": \"%s\"", state);
  }
  fprintf(out_, "},\n");
  return ZX_OK;
}
