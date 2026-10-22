// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// memgraph prints system-wide task and memory information as JSON.
// See memgraph-schema.json for the schema.

#include <getopt.h>
#include <stdint.h>
#include <stdio.h>
#include <time.h>
#include <zircon/process.h>
#include <zircon/status.h>
#include <zircon/syscalls.h>
#include <zircon/syscalls/object.h>

#include <iostream>
#include <print>

// Defines kMemgraphSchema containing the contents of memgraph-schema.json
#include "memgraph-schema.h"
#include "memgraph_internal.h"
#include "resources.h"

namespace {

void print_kernel_json(const char* name, const char* parent, uint64_t size_bytes) {
  std::println(
      "  {{"
      "\"id\": \"kernel/{}\", "
      "\"type\": \"kernel\", "
      "\"parent\": \"{}\", "
      "\"name\": \"{}\", "
      "\"size_bytes\": {}"
      "}},",
      name, parent, name, size_bytes);
}

zx_status_t dump_kernel_memory() {
  zx_handle_t info_resource;
  zx_status_t s = get_info_resource(&info_resource);
  if (s != ZX_OK) {
    return s;
  }
  zx_info_kmem_stats_t stats;
  s = zx_object_get_info(info_resource, ZX_INFO_KMEM_STATS, &stats, sizeof(stats), nullptr,
                         nullptr);
  zx_handle_close(info_resource);
  if (s != ZX_OK) {
    std::println(stderr, "WARNING: failed to get kernel memory stats: {} ({})",
                 zx_status_get_string(s), s);
    return s;
  }

  print_kernel_json("physmem", "", stats.total_bytes);
  print_kernel_json("free", "kernel/physmem", stats.free_bytes);
  print_kernel_json("vmo", "kernel/physmem", stats.vmo_bytes);
  print_kernel_json("heap", "kernel/physmem", stats.total_heap_bytes);
  print_kernel_json("heap/allocated", "kernel/heap",
                    stats.total_heap_bytes - stats.free_heap_bytes);
  print_kernel_json("heap/free", "kernel/heap", stats.free_heap_bytes);
  print_kernel_json("wired", "kernel/physmem", stats.wired_bytes);
  print_kernel_json("mmu", "kernel/physmem", stats.mmu_overhead_bytes);
  print_kernel_json("other", "kernel/physmem", stats.other_bytes);

  return ZX_OK;
}

void print_help(FILE* f) {
  std::println(f, "Usage: memgraph [options]");
  std::println(f, "  Prints system-wide task and memory info as JSON.");
  std::println(f, "Options:");
  std::println(f, " -t|--threads  Include threads in the output");
  std::println(f, " -v|--vmos     Include VMOs in the output");
  std::println(f, " -H|--handles  Include handle stats in the output");
  std::println(f, " -S|--schema   Print the schema for the JSON output format");
  std::println(f, " -h|--help     Display this message");
}

}  // namespace

int main(int argc, char** argv) {
  bool show_threads = false;
  bool show_vmos = false;
  bool show_handle_stats = false;
  while (true) {
    static constexpr option kOptions[] = {
        {.name = "threads", .has_arg = no_argument, .flag = nullptr, .val = 't'},
        {.name = "vmos", .has_arg = no_argument, .flag = nullptr, .val = 'v'},
        {.name = "handles", .has_arg = no_argument, .flag = nullptr, .val = 'H'},
        {.name = "schema", .has_arg = no_argument, .flag = nullptr, .val = 'S'},
        {.name = "help", .has_arg = no_argument, .flag = nullptr, .val = 'h'},
        {.name = nullptr, .has_arg = 0, .flag = nullptr, .val = 0},
    };
    int option_index = 0;
    int c = getopt_long(argc, argv, "tvHSh", kOptions, &option_index);
    if (c < 0) {
      break;
    }
    switch (c) {
      case 't':
        show_threads = true;
        break;
      case 'v':
        show_vmos = true;
        break;
      case 'H':
        show_handle_stats = true;
        break;
      case 'S':
        std::print("{}", kMemgraphSchema);
        return 0;
      case 'h':
      default:
        print_help(c == 'h' ? stdout : stderr);
        return c == 'h' ? 0 : 1;
    }
  }
  if (optind < argc) {
    std::print(stderr, "{}: unrecognized extra arguments:", argv[0]);
    while (optind < argc) {
      std::print(stderr, " {}", argv[optind++]);
    }
    std::println(stderr, "");
    print_help(stderr);
    return 1;
  }

  // Get our own koid so we can avoid (illegally) reading this process's VMOs.
  zx_info_handle_basic_t info;
  zx_status_t s = zx_object_get_info(zx_process_self(), ZX_INFO_HANDLE_BASIC, &info, sizeof(info),
                                     nullptr, nullptr);
  if (s != ZX_OK) {
    // This will probably result in a partial failure when we try to read
    // our own VMOs, but keep going.
    std::println(stderr, "WARNING: could not find our own koid: {} ({})", zx_status_get_string(s),
                 s);
    info = {};
    info.koid = 0;
  }

  // Grab the time when we start.
  struct timespec now;
  timespec_get(&now, TIME_UTC);

  std::println("[");

  zx_status_t ks = dump_kernel_memory();

  JsonTaskEnumerator jte(info.koid, show_threads, show_vmos, show_handle_stats, std::cout);
  s = jte.WalkRootJobTree();
  if (s != ZX_OK) {
    std::println(stderr, "ERROR: {} ({})", zx_status_get_string(s), s);
    return 1;
  }

  // Add a final entry with metadata. Also lets us avoid tracking commas
  // above.
  // Print the time as an ISO 8601 string.
  struct tm nowtm;
  gmtime_r(&now.tv_sec, &nowtm);
  char tbuf[40];
  strftime(tbuf, sizeof(tbuf), "%FT%T", &nowtm);
  std::println(
      "  {{"
      "\"type\": \"__META\", "
      "\"timestamp\": \"{}.{:03}Z\"}}",
      static_cast<const char*>(tbuf), now.tv_nsec / (1000L * 1000L));
  std::println("]");

  // Exit with an error status if we hit any partial failures.
  bool had_partial_failure = false;
  if (ks != ZX_OK) {
    std::println(stderr, "ERROR: delayed exit after partial failure: {} ({})",
                 zx_status_get_string(ks), ks);
    had_partial_failure = true;
  }
  for (zx_status_t failure : jte.partial_failures()) {
    std::println(stderr, "ERROR: delayed exit after partial failure: {} ({})",
                 zx_status_get_string(failure), failure);
    had_partial_failure = true;
  }
  if (had_partial_failure) {
    return 1;
  }
  return 0;
}
