// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/flatland/uber_struct_system.h"

#include <lib/syslog/cpp/macros.h>
#include <lib/trace/event.h>

#include <stack>

#include "src/ui/scenic/lib/utils/helpers.h"
#include "src/ui/scenic/lib/utils/logging.h"

namespace flatland {

// |UberStructSystem| implementations.

TransformHandle::InstanceId UberStructSystem::GetNextInstanceId() {
  // |latest_instance_id_| is only used for tests, but returning a member value can result in
  // threads "stealing" instance IDs from each other, so we return a local value here instead,
  // which does not have the same risk.
  auto next_instance_id = scheduling::GetNextSessionId();
  latest_instance_id_ = next_instance_id;
  return next_instance_id;
}

std::shared_ptr<UberStructSystem::UberStructQueue> UberStructSystem::AllocateQueueForSession(
    scheduling::SessionId session_id) {
  FX_DCHECK(!pending_structs_queues_.contains(session_id));

  auto [queue_kv, success] =
      pending_structs_queues_.emplace(session_id, std::make_shared<UberStructQueue>());
  FX_DCHECK(success);

  return queue_kv->second;
}

void UberStructSystem::RemoveSession(scheduling::SessionId session_id) {
  FX_DCHECK(pending_structs_queues_.contains(session_id));

  pending_structs_queues_.erase(session_id);
  snapshot_.map.erase(session_id);
}

UberStructSystem::UpdateResults UberStructSystem::UpdateInstances(
    const std::unordered_map<scheduling::SessionId, scheduling::PresentId>& instances_to_update) {
  TRACE_DURATION("gfx", "UberStructSystem::UpdateInstances");
  FLATLAND_VERBOSE_LOG << "UberStructSystem::UpdateSessions for " << instances_to_update.size()
                       << " sessions.";
  UpdateResults results;
  bool recompute_view_tree = false;
  for (const auto& [session_id, present_id] : instances_to_update) {
    // Find the queue associated with this SessonId. It may not exist if the SessionId is
    // associated with a GFX session instead of a Flatland one.
    auto queue_kv = pending_structs_queues_.find(session_id);
    if (queue_kv == pending_structs_queues_.end()) {
      continue;
    }

    bool successful_update = false;
    uint32_t present_credits_returned = 0;

    // Pop entries from that queue until the correct PresentId is found, then commit that
    // UberStruct to the snapshot. If the next pending UberStruct has a PresentId greater than the
    // target one, the update has failed because PresentIds are strictly increasing.
    auto pending_struct = queue_kv->second->Pop();
    while (pending_struct.has_value()) {
      ++present_credits_returned;

      // We may squash some UberStructs together; we must recompute the view tree if any of them
      // required recomputation.
      recompute_view_tree = recompute_view_tree || pending_struct->recompute_view_tree;

      if (pending_struct->present_id == present_id) {
        FLATLAND_VERBOSE_LOG << "    Updating UberStruct for session_id=" << session_id
                             << " present_id=" << present_id;
        snapshot_.map[session_id] = std::move(pending_struct->uber_struct);
        successful_update = true;
        break;
      }
      if (pending_struct->present_id > present_id) {
        FX_LOGS(WARNING) << "Popped past the target present_id (target=" << present_id
                         << " found=" << pending_struct->present_id << ")";
        break;
      }

      pending_struct = queue_kv->second->Pop();
    }

    FX_DCHECK(successful_update) << "No UberStruct found for session_id=" << session_id
                                 << " present_id=" << present_id;
    results.present_credits_returned[session_id] = present_credits_returned;
  }
  snapshot_.recompute_view_tree = recompute_view_tree;
  return results;
}

void UberStructSystem::ForceUpdateAllSessions(size_t max_updates_per_queue) {
  // Pop entries from each queue until empty.
  bool recompute_view_tree = false;
  for (auto& [session_id, queue] : pending_structs_queues_) {
    size_t update_count = 0;
    while (auto pending_struct = queue->Pop()) {
      snapshot_.map[session_id] = std::move(pending_struct->uber_struct);
      recompute_view_tree = recompute_view_tree || pending_struct->recompute_view_tree;
      if (++update_count == max_updates_per_queue) {
        break;
      }
    }
  }
  snapshot_.recompute_view_tree = recompute_view_tree;
}

const UberStructSnapshot& UberStructSystem::Snapshot() { return snapshot_; }

size_t UberStructSystem::GetSessionCount() { return pending_structs_queues_.size(); }

TransformHandle::InstanceId UberStructSystem::GetLatestInstanceId() const {
  return latest_instance_id_;
}

// |UberStructSystem::Queue| implementations.

void UberStructSystem::UberStructQueue::Push(scheduling::PresentId present_id,
                                             std::unique_ptr<UberStruct> uber_struct,
                                             bool recompute_view_tree) {
  FX_DCHECK(uber_struct);
#ifndef NDEBUG
  // PresentIds must be strictly increasing
  FX_DCHECK(!last_present_id_.load() || last_present_id_.load() < present_id);
  last_present_id_.store(present_id);
#endif

  pending_structs_.Push(PendingUberStruct{.present_id = present_id,
                                          .uber_struct = std::move(uber_struct),
                                          .recompute_view_tree = recompute_view_tree});
}

std::optional<UberStructSystem::PendingUberStruct> UberStructSystem::UberStructQueue::Pop() {
  return pending_structs_.Pop();
}

}  // namespace flatland

namespace {

struct Indenter {
  size_t depth;
};

}  // namespace

namespace std {

ostream& operator<<(ostream& out, const Indenter& indenter) {
  size_t depth = indenter.depth;
  while (depth-- > 0) {
    out << " ";
  }
  return out;
}

ostream& operator<<(ostream& out, const flatland::UberStruct& us) {
  if (us.view_ref) {
    out << *us.view_ref << "\n";
  }

  auto& topology = us.local_topology;

  size_t index = 0;
  std::stack<uint64_t> children_remaining;
  children_remaining.push(1);  // The root of the topology.

  while (index < topology.size()) {
    auto& handle = topology[index].handle;

    out << Indenter{children_remaining.size()} << handle;

    {
      auto it = us.images.find(handle);
      if (it != us.images.end()) {
        out << "  image(" << it->second.width << "x" << it->second.height << ")";
        out << " blend_mode=" << it->second.blend_mode;
      }
    }

    {
      auto it = us.local_image_sample_regions.find(handle);
      if (it != us.local_image_sample_regions.end()) {
        out << "  sample_region=" << it->second;
      }
    }

    {
      auto it = us.local_opacity_values.find(handle);
      if (it != us.local_opacity_values.end()) {
        out << "  opacity=" << it->second;
      }
    }

    {
      auto it = us.local_clip_regions.find(handle);
      if (it != us.local_clip_regions.end()) {
        out << "  clip_region=" << it->second;
      }
    }

    out << "\n";

    FX_DCHECK(!children_remaining.empty() && children_remaining.top() > 0);
    --children_remaining.top();

    if (topology[index].child_count > 0) {
      children_remaining.push(topology[index].child_count);
    }

    while (!children_remaining.empty() && children_remaining.top() == 0) {
      children_remaining.pop();
    }

    ++index;
  }

  return out;
}

}  // namespace std
