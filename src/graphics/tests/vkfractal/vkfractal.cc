// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <algorithm>
#include <array>
#include <atomic>
#include <chrono>
#include <cmath>
#include <complex>
#include <cstdint>
#include <format>
#include <limits>
#include <numbers>
#include <optional>
#include <print>
#include <string>
#include <utility>
#include <vector>

#include "src/lib/fxl/command_line.h"

#include <vulkan/vulkan.hpp>

// Generated at build time from shaders/ by glslangValidator.
#include "shaders/fractal_frag_spv.h"
#include "shaders/fullscreen_vert_spv.h"
#include "shaders/scale_frag_spv.h"
#include "shaders/scale_vert_spv.h"

namespace {

constexpr char kSwapchainLayerName[] = "VK_LAYER_FUCHSIA_imagepipe_swapchain_fb";
constexpr char kValidationLayerName[] = "VK_LAYER_KHRONOS_validation";

// A width and height in the complex plane.
struct ComplexSize {
  double width;
  double height;
};

// LINT.IfChange(push_constants)
struct PushConstants {
  float half_extent[2];
  float step[2];
  float c[2];
  uint32_t max_iterations;
  std::array<uint32_t, 21> palette;
};
static_assert(sizeof(PushConstants) == 112);
// LINT.ThenChange(//src/graphics/tests/vkfractal/shaders/fractal.frag:push_constants)
// Vulkan guarantees at least 128 bytes of push constants.
static_assert(sizeof(PushConstants) <= 128);

// How rendered frames reach the display.
enum class DisplayMode {
  // --offscreen: nothing is presented.
  kNone,
  // The fractal shader writes the swapchain images.
  kDirect,
  // The fractal shader writes an image whose size differs from the display's, or the display
  // is taller than it is wide, and a bilinear sampling pass stretches the image (rotated 90
  // degrees clockwise on portrait displays) over the swapchain image.
  kScale,
};

struct Options {
  // Frames per loop. Every loop renders the same frames.
  uint32_t frames = 1200;
  // Timed loops, or std::nullopt to loop until killed.
  std::optional<uint32_t> loops = 1;
  // Untimed frames rendered before the timed ones.
  uint32_t warmup_frames = 10;
  uint32_t max_iterations = 256;
  // Iterate every pixel max_iterations times instead of stopping when it escapes.
  bool fixed_iterations = false;
  uint32_t frames_in_flight = 3;
  bool validation = true;
  // Also time each loop on the GPU with timestamp queries. Off by default because timestamp
  // queries crash some drivers.
  bool gpu_timing = false;
  // Render without a display or swapchain; every frame goes to an offscreen image.
  bool offscreen = false;
  // The fractal's resolution. Defaults to the display's size (swapped on portrait displays, which
  // show the image rotated), or to kDefaultOffscreenWidth x kDefaultOffscreenHeight with
  // --offscreen.
  std::optional<vk::Extent2D> resolution;
};

constexpr uint32_t kDefaultOffscreenWidth = 1024;
constexpr uint32_t kDefaultOffscreenHeight = 1024;
constexpr uint32_t kMaxFrames = 1'000'000;
// Each frame in flight owns an offscreen image, so keep the count modest.
constexpr uint32_t kMaxFramesInFlight = 16;

constexpr char kUsage[] =
    "Usage: vkfractal [options]\n"
    "  --help                Print this message.\n"
    "  --frames=N            Frames per loop (default 1200, at most 1000000).\n"
    "  --loops=N             Loops to render and time, or -1 to loop until killed (default 1).\n"
    "  --warmup-frames=N     Untimed frames rendered first (default 10).\n"
    "  --max-iterations=N    Iteration cap per pixel (default 256).\n"
    "  --fixed-iterations=BOOL\n"
    "                        Run --max-iterations iterations on every pixel, even after it\n"
    "                        escapes (default false).\n"
    "  --frames-in-flight=N  Frames kept queued on the GPU (default 3, at most 16).\n"
    "  --validation=BOOL     Enable Vulkan validation layers (default true).\n"
    "  --gpu-timing=BOOL     Also time loops on the GPU with timestamp queries (default false).\n"
    "  --offscreen=BOOL      Render into offscreen images only; no display or swapchain\n"
    "                        (default false).\n"
    "  --resolution=WxH      Fractal resolution: how many points of the complex region are\n"
    "                        iterated. Defaults to the display's size, or 1024x1024 with\n"
    "                        --offscreen. Frames are stretched to fill the display, and rotated\n"
    "                        90 degrees clockwise if it is taller than it is wide.\n";

bool Fail(const char* what, vk::Result result) {
  std::println(stderr, "vkfractal: {} failed: {}", what, vk::to_string(result));
  return false;
}

bool Fail(const char* message) {
  std::println(stderr, "vkfractal: {}", message);
  return false;
}

// Parses a decimal integer in [min, max]. Rejects leading whitespace and signs, which strtoull
// would accept.
std::optional<uint32_t> ParseUint32(const std::string& text, uint32_t min = 0,
                                    uint32_t max = std::numeric_limits<uint32_t>::max()) {
  if (text.empty() || text[0] < '0' || text[0] > '9') {
    return std::nullopt;
  }
  char* end = nullptr;
  errno = 0;
  const unsigned long long value = strtoull(text.c_str(), &end, 10);
  if (*end != '\0' || errno != 0 || value < min || value > max) {
    return std::nullopt;
  }
  return static_cast<uint32_t>(value);
}

std::optional<bool> ParseBool(const std::string& text) {
  if (text.empty() || text == "true") {
    return true;
  }
  if (text == "false") {
    return false;
  }
  return std::nullopt;
}

// Splits "<first><separator><second>" at the first |separator|.
std::optional<std::pair<std::string, std::string>> SplitPair(const std::string& text,
                                                             char separator) {
  const size_t position = text.find(separator);
  if (position == std::string::npos) {
    return std::nullopt;
  }
  return std::make_pair(text.substr(0, position), text.substr(position + 1));
}

// Parses "WxH" with positive integers, e.g. "1024x768".
std::optional<vk::Extent2D> ParseResolution(const std::string& text) {
  const std::optional<std::pair<std::string, std::string>> parts = SplitPair(text, 'x');
  if (!parts) {
    return std::nullopt;
  }
  const std::optional<uint32_t> width = ParseUint32(parts->first, 1);
  const std::optional<uint32_t> height = ParseUint32(parts->second, 1);
  if (!width || !height) {
    return std::nullopt;
  }
  return vk::Extent2D(*width, *height);
}

// Returns the size of the complex plane in view for an image of |aspect| (width / height).
ComplexSize Extents(double aspect) {
  // Stretches the loop's Julia sets to stay kMargin of the image's shorter side from each edge.
  // They reach |Re z| = (1 + sqrt(6)) / 2, where the set for c = -1.25 touches its escape radius,
  // and |Im z| = 1.152, measured near c = 0.364 + 0.304i.
  constexpr double kReachX = 1.7247;
  constexpr double kReachY = 1.152;
  constexpr double kMargin = 0.05;
  const double margin = 2.0 * kMargin * std::min(aspect, 1.0);
  return {
      .width = 2.0 * kReachX * aspect / (aspect - margin),
      .height = 2.0 * kReachY / (1.0 - margin),
  };
}

// Returns the Julia set parameter for each frame of a loop of |frames|.
std::vector<std::complex<double>> JuliaPath(uint32_t frames) {
  // Loops clockwise around the edge of the Mandelbrot set's main cardioid (c = lambda / 2 -
  // lambda^2 / 4, |lambda| = 1), dipping inside near the cusp at lambda = 1 to pass through
  // 1 - kDip, and detours counterclockwise around the edges of the 2/3, 1/2, and 1/3 bulbs
  // (|mu| = 1 in each bulb's multiplier).
  constexpr double kTau = 2.0 * std::numbers::pi;
  constexpr double kDip = 0.06;
  constexpr double kDipWidth = 0.3;
  constexpr double kAlpha = 0.75;
  constexpr size_t kSamples = 4096;

  const auto lambda_to_c = [](std::complex<double> lambda) {
    return lambda / 2.0 - lambda * lambda / 4.0;
  };
  const auto gauss = [](double x) { return std::exp(-x * x / (2.0 * kDipWidth * kDipWidth)); };
  const auto cardioid_radius = [&](double theta) {
    return 1.0 - (kDip * (gauss(theta) + gauss(theta - kTau)));
  };

  std::vector<std::complex<double>> mu(kSamples + 1);
  for (size_t j = 0; j <= kSamples; ++j) {
    mu[j] = std::polar(1.0, kTau * static_cast<double>(j) / kSamples);
  }

  // The 1/3 bulb (rabbit): solve f_c^3(z) = z and (f_c^3)'(z) = mu by Newton's method on (z, c),
  // continuing from the bulb's nucleus. At the root mu = 1 the 3-cycle collides with the fixed
  // point and the Jacobian is singular, so set the root from the cardioid instead.
  std::vector<std::complex<double>> c13(kSamples + 1);
  c13[0] = c13[kSamples] = lambda_to_c(std::polar(1.0, kTau / 3.0));
  std::complex<double> z = 0.0;
  std::complex<double> c(-0.122561, 0.744862);
  for (int i = 0; i < 100; ++i) {
    std::complex<double> zk = 0.0;
    std::complex<double> dz = 0.0;
    for (int k = 0; k < 3; ++k) {
      dz = 2.0 * zk * dz + 1.0;
      zk = zk * zk + c;
    }
    const std::complex<double> step = zk / dz;
    c -= step;
    if (std::abs(step) < 1e-15) {
      break;
    }
  }
  const auto solve_period3 = [&](std::complex<double> target_mu) {
    for (int i = 0; i < 50; ++i) {
      std::complex<double> zk = z;
      std::complex<double> a = 1.0;  // dzk/dz
      std::complex<double> b = 0.0;  // dzk/dc
      std::complex<double> d = 0.0;  // da/dz
      std::complex<double> e = 0.0;  // da/dc
      for (int k = 0; k < 3; ++k) {
        const std::complex<double> next_d = 2.0 * (a * a + zk * d);
        const std::complex<double> next_e = 2.0 * (b * a + zk * e);
        a = 2.0 * zk * a;
        b = 2.0 * zk * b + 1.0;
        d = next_d;
        e = next_e;
        zk = zk * zk + c;
      }
      const std::complex<double> f1 = zk - z;
      const std::complex<double> f2 = a - target_mu;
      const std::complex<double> det = (a - 1.0) * e - b * d;
      const std::complex<double> dz = (e * f1 - b * f2) / det;
      const std::complex<double> dc = ((a - 1.0) * f2 - d * f1) / det;
      z -= dz;
      c -= dc;
      if (std::abs(dz) + std::abs(dc) < 1e-14) {
        break;
      }
    }
  };
  for (int s = 1; s <= 80; ++s) {
    const double u = 1.0 - (static_cast<double>(s) / 80.0);
    solve_period3(mu[1] * (1.0 - (u * u)));
  }
  for (size_t j = 1; j < kSamples; ++j) {
    solve_period3(mu[j]);
    c13[j] = c;
  }

  std::vector<std::complex<double>> c12(kSamples + 1);
  std::vector<std::complex<double>> c23(kSamples + 1);
  for (size_t j = 0; j <= kSamples; ++j) {
    c12[j] = -1.0 + mu[j] / 4.0;
    c23[j] = std::conj(c13[kSamples - j]);
  }

  // Build a dense polyline of the loop, dropping each piece's last sample since it equals the next
  // piece's first, and measure arc length s in each piece's multiplier coordinate.
  std::vector<double> arc;
  std::vector<std::complex<double>> samples;
  double s = 0.0;
  const auto add_piece = [&](const std::vector<std::complex<double>>& mult,
                             const std::vector<std::complex<double>>& cs) {
    for (size_t j = 0; j + 1 < mult.size(); ++j) {
      arc.push_back(s);
      samples.push_back(cs[j]);
      s += std::abs(mult[j + 1] - mult[j]);
    }
  };
  const auto add_cardioid = [&](size_t n, double theta0, double theta1) {
    std::vector<std::complex<double>> lambda(n + 1);
    std::vector<std::complex<double>> cs(n + 1);
    for (size_t j = 0; j <= n; ++j) {
      const double theta =
          theta0 + ((theta1 - theta0) * static_cast<double>(j) / static_cast<double>(n));
      lambda[j] = std::polar(cardioid_radius(theta), -theta);
      cs[j] = lambda_to_c(lambda[j]);
    }
    add_piece(lambda, cs);
  };
  add_cardioid(kSamples / 3, 0.0, kTau / 3.0);
  add_piece(mu, c23);
  add_cardioid(kSamples / 6, kTau / 3.0, std::numbers::pi);
  add_piece(mu, c12);
  add_cardioid(kSamples / 6, std::numbers::pi, 2.0 * kTau / 3.0);
  add_piece(mu, c13);
  add_cardioid(kSamples / 3, 2.0 * kTau / 3.0, kTau);
  arc.push_back(s);
  samples.push_back(lambda_to_c(cardioid_radius(kTau)));

  // Time runs as |dc|^kAlpha * |ds|^(1 - kAlpha) so that entering the smaller bulbs does not
  // sharply slow the visual motion.
  std::vector<double> time(samples.size(), 0.0);
  for (size_t j = 1; j < samples.size(); ++j) {
    const double dc = std::abs(samples[j] - samples[j - 1]);
    const double ds = arc[j] - arc[j - 1];
    time[j] = time[j - 1] + (std::pow(dc, kAlpha) * std::pow(ds, 1.0 - kAlpha));
  }

  std::vector<std::complex<double>> path(frames);
  for (uint32_t k = 0; k < frames; ++k) {
    const double target = static_cast<double>(k) * time.back() / frames;
    size_t lo = 0;
    size_t hi = time.size() - 1;
    while (hi - lo > 1) {
      const size_t mid = (lo + hi) / 2;
      if (time[mid] <= target) {
        lo = mid;
      } else {
        hi = mid;
      }
    }
    const double t = (target - time[lo]) / (time[hi] - time[lo]);
    path[k] = samples[lo] + (samples[hi] - samples[lo]) * t;
  }
  return path;
}

// Stores |value| in |out| if it is set, and returns whether it was.
template <typename T>
bool StoreIfSet(const std::optional<T>& value, T* out) {
  if (!value) {
    return false;
  }
  *out = *value;
  return true;
}

std::optional<Options> ParseOptions(const fxl::CommandLine& command_line) {
  if (!command_line.positional_args().empty()) {
    std::print(stderr, "vkfractal: unexpected argument {}\n{}", command_line.positional_args()[0],
               kUsage);
    return std::nullopt;
  }
  Options options;
  for (const fxl::CommandLine::Option& option : command_line.options()) {
    bool ok = true;
    if (option.name == "frames") {
      ok = StoreIfSet(ParseUint32(option.value, 1, kMaxFrames), &options.frames);
    } else if (option.name == "loops") {
      if (option.value == "-1") {
        options.loops = std::nullopt;
      } else {
        options.loops = ParseUint32(option.value, 1);
        ok = options.loops.has_value();
      }
    } else if (option.name == "warmup-frames") {
      ok = StoreIfSet(ParseUint32(option.value), &options.warmup_frames);
    } else if (option.name == "max-iterations") {
      ok = StoreIfSet(ParseUint32(option.value, 1), &options.max_iterations);
    } else if (option.name == "fixed-iterations") {
      ok = StoreIfSet(ParseBool(option.value), &options.fixed_iterations);
    } else if (option.name == "frames-in-flight") {
      ok = StoreIfSet(ParseUint32(option.value, 1, kMaxFramesInFlight), &options.frames_in_flight);
    } else if (option.name == "validation") {
      ok = StoreIfSet(ParseBool(option.value), &options.validation);
    } else if (option.name == "gpu-timing") {
      ok = StoreIfSet(ParseBool(option.value), &options.gpu_timing);
    } else if (option.name == "offscreen") {
      ok = StoreIfSet(ParseBool(option.value), &options.offscreen);
    } else if (option.name == "resolution") {
      options.resolution = ParseResolution(option.value);
      ok = options.resolution.has_value();
    } else {
      ok = false;
    }
    if (!ok) {
      std::print(stderr, "vkfractal: invalid option --{}={}\n{}", option.name, option.value,
                 kUsage);
      return std::nullopt;
    }
  }
  return options;
}

// |user_data| is the std::atomic<uint32_t> that counts validation errors.
VKAPI_ATTR VkBool32 VKAPI_CALL DebugCallback(VkDebugUtilsMessageSeverityFlagBitsEXT severity,
                                             VkDebugUtilsMessageTypeFlagsEXT types,
                                             const VkDebugUtilsMessengerCallbackDataEXT* data,
                                             void* user_data) {
  if (severity & VK_DEBUG_UTILS_MESSAGE_SEVERITY_ERROR_BIT_EXT) {
    ++*static_cast<std::atomic<uint32_t>*>(user_data);
  }
  std::println(stderr, "vkfractal: validation: {}", data->pMessage);
  return VK_FALSE;
}

// Reports warnings and errors to DebugCallback, which counts errors in |error_count|.
VkDebugUtilsMessengerCreateInfoEXT DebugMessengerCreateInfo(std::atomic<uint32_t>* error_count) {
  return {
      .sType = VK_STRUCTURE_TYPE_DEBUG_UTILS_MESSENGER_CREATE_INFO_EXT,
      .messageSeverity = VK_DEBUG_UTILS_MESSAGE_SEVERITY_WARNING_BIT_EXT |
                         VK_DEBUG_UTILS_MESSAGE_SEVERITY_ERROR_BIT_EXT,
      .messageType = VK_DEBUG_UTILS_MESSAGE_TYPE_GENERAL_BIT_EXT |
                     VK_DEBUG_UTILS_MESSAGE_TYPE_VALIDATION_BIT_EXT |
                     VK_DEBUG_UTILS_MESSAGE_TYPE_PERFORMANCE_BIT_EXT,
      .pfnUserCallback = DebugCallback,
      .pUserData = error_count,
  };
}

// Owns a VK_EXT_debug_utils messenger. Uses the C API because the entry points are only
// reachable through vkGetInstanceProcAddr.
class DebugMessenger {
 public:
  DebugMessenger() = default;
  DebugMessenger(const DebugMessenger&) = delete;
  DebugMessenger& operator=(const DebugMessenger&) = delete;
  ~DebugMessenger() {
    if (messenger_ != VK_NULL_HANDLE) {
      destroy_(instance_, messenger_, nullptr);
    }
  }

  bool Init(VkInstance instance, const VkDebugUtilsMessengerCreateInfoEXT& create_info) {
    auto create = reinterpret_cast<PFN_vkCreateDebugUtilsMessengerEXT>(
        vkGetInstanceProcAddr(instance, "vkCreateDebugUtilsMessengerEXT"));
    auto destroy = reinterpret_cast<PFN_vkDestroyDebugUtilsMessengerEXT>(
        vkGetInstanceProcAddr(instance, "vkDestroyDebugUtilsMessengerEXT"));
    if (!create || !destroy) {
      return Fail("VK_EXT_debug_utils entry points not found");
    }
    VkResult result = create(instance, &create_info, nullptr, &messenger_);
    if (result != VK_SUCCESS) {
      return Fail("vkCreateDebugUtilsMessengerEXT", vk::Result(result));
    }
    instance_ = instance;
    destroy_ = destroy;
    return true;
  }

 private:
  VkInstance instance_ = VK_NULL_HANDLE;
  VkDebugUtilsMessengerEXT messenger_ = VK_NULL_HANDLE;
  PFN_vkDestroyDebugUtilsMessengerEXT destroy_ = nullptr;
};

class Fractal {
 public:
  // |validation_errors| counts validation errors and must outlive the Fractal.
  Fractal(const Options& options, std::atomic<uint32_t>* validation_errors)
      : options_(options), validation_errors_(validation_errors) {}
  Fractal(const Fractal&) = delete;
  Fractal& operator=(const Fractal&) = delete;
  ~Fractal() {
    if (device_) {
      if (vk::Result result = device_->waitIdle(); result != vk::Result::eSuccess) {
        Fail("vkDeviceWaitIdle", result);
      }
    }
  }

  bool Init() {
    return CreateInstance() && ChoosePhysicalDevice() && ChooseExtent() && ChooseFormat() &&
           CreateDevice() && CreateQueryPool() && CreateSwapchain() && CreatePipeline() &&
           CreateScalePipeline() && CreateTargets();
  }

  bool Run();

 private:
  // Per frames-in-flight slot. The fence guards every object in the slot.
  struct FrameSlot {
    vk::UniqueCommandBuffer command_buffer;
    // Scale mode only. Submitted after |command_buffer| in the same vkQueueSubmit, and only this
    // batch waits for the swapchain image, so the fractal never waits for the display.
    vk::UniqueCommandBuffer scale_command_buffer;
    vk::UniqueSemaphore acquire_semaphore;
    vk::UniqueFence fence;
    // Rendered into when no swapchain image is free (the result is discarded), or in scale mode,
    // on every frame.
    vk::UniqueImage offscreen_image;
    vk::UniqueDeviceMemory offscreen_memory;
  };

  // An image the shader writes: a swapchain image or a slot's offscreen image.
  struct Target {
    vk::Image image;
    vk::UniqueImageView view;
    vk::UniqueFramebuffer framebuffer;
    // Scale mode, offscreen targets only: samples |image|. Freed with scale_descriptor_pool_.
    vk::DescriptorSet scale_descriptor_set;
  };

  // A swapchain image the scale pass draws into.
  struct ScaleTarget {
    vk::UniqueImageView view;
    vk::UniqueFramebuffer framebuffer;
  };

  bool CreateInstance();
  bool ChoosePhysicalDevice();
  bool ChooseExtent();
  // Fails if the device cannot render the fractal at |extent_|.
  bool CheckResolution() const;
  bool ChooseFormat();
  bool CreateDevice();
  bool CreateQueryPool();
  bool CreateSwapchain();
  bool CreatePipeline();
  bool CreateScalePipeline();
  // A render pass with one color attachment in COLOR_ATTACHMENT_OPTIMAL before and after.
  bool CreateColorRenderPass(vk::AttachmentLoadOp load_op, vk::UniqueRenderPass* out);
  bool CreateShaderModule(const uint32_t* code, size_t size, vk::UniqueShaderModule* out);
  // A pipeline that draws one full-screen triangle into |viewport_rect|.
  bool CreateFullscreenPipeline(vk::ShaderModule vertex, vk::ShaderModule fragment,
                                vk::RenderPass render_pass, vk::PipelineLayout layout,
                                vk::Rect2D viewport_rect, vk::UniquePipeline* out,
                                const vk::SpecializationInfo* vertex_specialization = nullptr,
                                const vk::SpecializationInfo* fragment_specialization = nullptr);
  bool CreateTargets();
  bool CreateFrameSlots();
  bool CreateTarget(vk::Image image, Target* target);
  bool CreateScaleTargets();
  bool CreateOffscreenImage(FrameSlot* slot);
  std::optional<uint32_t> FindMemoryType(uint32_t type_bits,
                                         vk::MemoryPropertyFlags properties) const;
  // Renders a frame with slot |slot_index| and presents it if a swapchain image is free, setting
  // |presented| to whether it was. |loop_start| and |loop_end| mark a timed loop's first and last
  // frames.
  bool RenderFrame(size_t slot_index, const PushConstants& push_constants, bool loop_start,
                   bool loop_end, bool* presented);
  // Renders into |target|. If |present| is set, |target| is a swapchain image, which is left
  // ready to present. |loop_start| and |loop_end| mark a timed loop's first and last batches.
  bool RecordFrame(vk::CommandBuffer command_buffer, const Target& target, bool present,
                   const PushConstants& push_constants, bool loop_start, bool loop_end);
  // Stretches |source|, which the previous batch rendered, over swapchain image |image_index|.
  bool RecordScale(vk::CommandBuffer command_buffer, const Target& source, uint32_t image_index,
                   bool loop_end);

  const Options options_;
  std::atomic<uint32_t>* const validation_errors_;

  // Members are destroyed in reverse declaration order.
  vk::UniqueInstance instance_;
  DebugMessenger debug_messenger_;
  vk::UniqueSurfaceKHR surface_;
  vk::PhysicalDevice physical_device_;
  std::string device_name_;
  uint32_t queue_family_ = 0;
  // Valid bits in the queue's timestamps, or 0 if it has none.
  uint32_t timestamp_valid_bits_ = 0;
  // Nanoseconds per timestamp tick.
  float timestamp_period_ = 0.0f;
  // The fractal's resolution.
  vk::Extent2D extent_;
  // The swapchain images' size: the display's. Unused offscreen.
  vk::Extent2D display_extent_;
  // The display is taller than it is wide, so the image is rotated 90 degrees clockwise onto it.
  bool rotate_ = false;
  // The size of the complex plane in view.
  ComplexSize extents_;
  vk::SurfaceFormatKHR surface_format_;
  DisplayMode display_mode_ = DisplayMode::kNone;
  vk::UniqueDevice device_;
  vk::Queue queue_;
  // With --gpu-timing, measures each timed loop on the GPU.
  vk::UniqueQueryPool query_pool_;
  vk::UniqueSwapchainKHR swapchain_;
  vk::UniqueRenderPass render_pass_;
  vk::UniquePipelineLayout pipeline_layout_;
  vk::UniquePipeline pipeline_;
  // Scale mode only.
  vk::UniqueSampler scale_sampler_;
  vk::UniqueDescriptorSetLayout scale_descriptor_set_layout_;
  vk::UniqueRenderPass scale_render_pass_;
  vk::UniquePipelineLayout scale_pipeline_layout_;
  vk::UniquePipeline scale_pipeline_;
  vk::UniqueCommandPool command_pool_;
  std::vector<FrameSlot> slots_;
  vk::UniqueDescriptorPool scale_descriptor_pool_;  // Scale mode only.
  std::vector<vk::Image> swapchain_images_;
  std::vector<Target> swapchain_targets_;                // Direct mode only.
  std::vector<ScaleTarget> scale_targets_;               // Scale mode only.
  std::vector<Target> offscreen_targets_;                // One per slot.
  std::vector<vk::UniqueSemaphore> present_semaphores_;  // One per swapchain image.
};

bool Fractal::CreateInstance() {
  std::vector<const char*> layers;
  std::vector<const char*> extensions;
  if (!options_.offscreen) {
    layers.push_back(kSwapchainLayerName);
    extensions.push_back(VK_KHR_SURFACE_EXTENSION_NAME);
    extensions.push_back(VK_FUCHSIA_IMAGEPIPE_SURFACE_EXTENSION_NAME);
  }
  if (options_.validation) {
    layers.push_back(kValidationLayerName);
    extensions.push_back(VK_EXT_DEBUG_UTILS_EXTENSION_NAME);
  }

  auto [layers_result, available_layers] = vk::enumerateInstanceLayerProperties();
  if (layers_result != vk::Result::eSuccess) {
    return Fail("vkEnumerateInstanceLayerProperties", layers_result);
  }
  for (const char* layer : layers) {
    if (std::none_of(available_layers.begin(), available_layers.end(),
                     [layer](const vk::LayerProperties& properties) {
                       return strcmp(properties.layerName, layer) == 0;
                     })) {
      std::println(stderr, "vkfractal: required layer {} is not available", layer);
      return false;
    }
  }

  vk::ApplicationInfo app_info;
  app_info.setPApplicationName("vkfractal").setApiVersion(VK_API_VERSION_1_1);
  vk::InstanceCreateInfo create_info;
  create_info.setPApplicationInfo(&app_info)
      .setPEnabledLayerNames(layers)
      .setPEnabledExtensionNames(extensions);
  const VkDebugUtilsMessengerCreateInfoEXT messenger_info =
      DebugMessengerCreateInfo(validation_errors_);
  if (options_.validation) {
    // Reports messages from vkCreateInstance and vkDestroyInstance, which |debug_messenger_|
    // cannot see.
    create_info.setPNext(&messenger_info);
  }
  auto [instance_result, instance] = vk::createInstanceUnique(create_info);
  if (instance_result != vk::Result::eSuccess) {
    return Fail("vkCreateInstance", instance_result);
  }
  instance_ = std::move(instance);

  if (options_.validation && !debug_messenger_.Init(*instance_, messenger_info)) {
    return false;
  }

  if (options_.offscreen) {
    return true;
  }

  // The swapchain layer connects to the display coordinator at test-utility priority, above
  // Scenic, so creating this surface and its swapchain takes over the display.
  auto [surface_result, surface] =
      instance_->createImagePipeSurfaceFUCHSIAUnique(vk::ImagePipeSurfaceCreateInfoFUCHSIA());
  if (surface_result != vk::Result::eSuccess) {
    return Fail("vkCreateImagePipeSurfaceFUCHSIA", surface_result);
  }
  surface_ = std::move(surface);
  return true;
}

bool Fractal::ChoosePhysicalDevice() {
  auto [devices_result, physical_devices] = instance_->enumeratePhysicalDevices();
  if (devices_result != vk::Result::eSuccess) {
    return Fail("vkEnumeratePhysicalDevices", devices_result);
  }
  for (const vk::PhysicalDevice& physical_device : physical_devices) {
    const vk::PhysicalDeviceProperties properties = physical_device.getProperties();
    // The shaders are compiled for Vulkan 1.1 (SPIR-V 1.3).
    if (properties.apiVersion < VK_API_VERSION_1_1) {
      continue;
    }
    const std::vector<vk::QueueFamilyProperties> families =
        physical_device.getQueueFamilyProperties();
    for (uint32_t i = 0; i < families.size(); ++i) {
      // Everything is recorded on one graphics queue.
      if (!(families[i].queueFlags & vk::QueueFlagBits::eGraphics)) {
        continue;
      }
      bool supported = true;
      if (!options_.offscreen) {
        auto [support_result, can_present] = physical_device.getSurfaceSupportKHR(i, *surface_);
        supported = support_result == vk::Result::eSuccess && can_present;
      }
      if (supported) {
        physical_device_ = physical_device;
        queue_family_ = i;
        timestamp_valid_bits_ = families[i].timestampValidBits;
        timestamp_period_ = properties.limits.timestampPeriod;
        device_name_ = properties.deviceName.data();
        return true;
      }
    }
  }
  std::println(stderr, "vkfractal: no Vulkan 1.1 physical device has a graphics queue{}",
               options_.offscreen ? "" : " that can present to the display");
  return false;
}

bool Fractal::ChooseExtent() {
  if (options_.offscreen) {
    extent_ =
        options_.resolution.value_or(vk::Extent2D(kDefaultOffscreenWidth, kDefaultOffscreenHeight));
    extents_ = Extents(static_cast<double>(extent_.width) / extent_.height);
    return CheckResolution();
  }
  auto [caps_result, caps] = physical_device_.getSurfaceCapabilitiesKHR(*surface_);
  if (caps_result != vk::Result::eSuccess) {
    return Fail("vkGetPhysicalDeviceSurfaceCapabilitiesKHR", caps_result);
  }
  if (caps.currentExtent.width == std::numeric_limits<uint32_t>::max()) {
    return Fail("the display surface has no fixed size");
  }
  display_extent_ = caps.currentExtent;
  // Rotating a portrait display's image lets the default resolution map 1:1 onto its pixels.
  rotate_ = display_extent_.height > display_extent_.width;
  extent_ = options_.resolution.value_or(
      rotate_ ? vk::Extent2D(display_extent_.height, display_extent_.width) : display_extent_);
  extents_ = Extents(static_cast<double>(extent_.width) / extent_.height);
  return CheckResolution();
}

bool Fractal::CheckResolution() const {
  const vk::PhysicalDeviceLimits limits = physical_device_.getProperties().limits;
  const uint32_t max_width = std::min(limits.maxImageDimension2D, limits.maxFramebufferWidth);
  const uint32_t max_height = std::min(limits.maxImageDimension2D, limits.maxFramebufferHeight);
  if (extent_.width > max_width || extent_.height > max_height) {
    std::println(stderr, "vkfractal: resolution {}x{} exceeds this device's limit of {}x{}",
                 extent_.width, extent_.height, max_width, max_height);
    return false;
  }
  return true;
}

bool Fractal::ChooseFormat() {
  // Vulkan requires R8G8B8A8_UNORM and B8G8R8A8_UNORM images with optimal tiling to support
  // rendering and linear sampling, so no format features are checked.
  if (options_.offscreen) {
    surface_format_.format = vk::Format::eR8G8B8A8Unorm;
    display_mode_ = DisplayMode::kNone;
    return true;
  }
  auto [formats_result, surface_formats] = physical_device_.getSurfaceFormatsKHR(*surface_);
  if (formats_result != vk::Result::eSuccess) {
    return Fail("vkGetPhysicalDeviceSurfaceFormatsKHR", formats_result);
  }
  // The display swapchain lists its preferred format first, and it's always one of those two.
  surface_format_ = surface_formats[0];
  display_mode_ =
      rotate_ || extent_ != display_extent_ ? DisplayMode::kScale : DisplayMode::kDirect;
  return true;
}

bool Fractal::CreateDevice() {
  const float queue_priority = 1.0f;
  vk::DeviceQueueCreateInfo queue_info;
  queue_info.setQueueFamilyIndex(queue_family_).setQueuePriorities(queue_priority);

  std::vector<const char*> extensions;
  if (!options_.offscreen) {
    extensions.push_back(VK_KHR_SWAPCHAIN_EXTENSION_NAME);
  }
  vk::DeviceCreateInfo create_info;
  create_info.setQueueCreateInfos(queue_info).setPEnabledExtensionNames(extensions);
  auto [device_result, device] = physical_device_.createDeviceUnique(create_info);
  if (device_result != vk::Result::eSuccess) {
    return Fail("vkCreateDevice", device_result);
  }
  device_ = std::move(device);
  queue_ = device_->getQueue(queue_family_, 0);
  return true;
}

bool Fractal::CreateQueryPool() {
  if (!options_.gpu_timing) {
    return true;
  }
  if (timestamp_valid_bits_ == 0) {
    return Fail("--gpu-timing needs a queue with timestamps");
  }
  // A timed loop's first and last timestamps.
  vk::QueryPoolCreateInfo pool_info;
  pool_info.setQueryType(vk::QueryType::eTimestamp).setQueryCount(2);
  auto [pool_result, pool] = device_->createQueryPoolUnique(pool_info);
  if (pool_result != vk::Result::eSuccess) {
    return Fail("vkCreateQueryPool", pool_result);
  }
  query_pool_ = std::move(pool);
  return true;
}

bool Fractal::CreateSwapchain() {
  if (options_.offscreen) {
    return true;
  }

  auto [caps_result, caps] = physical_device_.getSurfaceCapabilitiesKHR(*surface_);
  if (caps_result != vk::Result::eSuccess) {
    return Fail("vkGetPhysicalDeviceSurfaceCapabilitiesKHR", caps_result);
  }
  // Presents are not queued FIFO on the display path: each one hands its image to the display
  // coordinator right away, and at every vsync the coordinator shows the newest image whose
  // rendering has finished, skipping older ones. The layer releases the previously shown image and
  // any skipped ones at that vsync. So when a frame is submitted, images can be held by the
  // frames still queued before it, by one finished frame waiting for vsync, and by the image on
  // screen. With this many, every frame gets an image when rendering is slower than the display.
  // The display coordinator queues at most 10 waiting images per layer, so stay within that.
  constexpr uint32_t kMaxSwapchainImages = 10;
  uint32_t image_count =
      std::max(caps.minImageCount, std::min(options_.frames_in_flight + 2, kMaxSwapchainImages));
  if (caps.maxImageCount != 0) {
    image_count = std::min(image_count, caps.maxImageCount);
  }
  // Every pixel the display shows has alpha 1, so all composite alpha modes look the same.
  std::optional<vk::CompositeAlphaFlagBitsKHR> composite_alpha;
  for (vk::CompositeAlphaFlagBitsKHR candidate :
       {vk::CompositeAlphaFlagBitsKHR::eOpaque, vk::CompositeAlphaFlagBitsKHR::ePreMultiplied,
        vk::CompositeAlphaFlagBitsKHR::ePostMultiplied, vk::CompositeAlphaFlagBitsKHR::eInherit}) {
    if (caps.supportedCompositeAlpha & candidate) {
      composite_alpha = candidate;
      break;
    }
  }
  if (!composite_alpha) {
    return Fail("the display surface supports no composite alpha mode");
  }

  // FIFO is the only mode the swapchain layer offers on the display path, but as described above
  // the coordinator behaves like mailbox. Rendering is never paced by vsync because RenderFrame()
  // acquires with a zero timeout.
  vk::SwapchainCreateInfoKHR create_info;
  create_info.setSurface(*surface_)
      .setMinImageCount(image_count)
      .setImageFormat(surface_format_.format)
      .setImageColorSpace(surface_format_.colorSpace)
      .setImageExtent(display_extent_)
      .setImageArrayLayers(1)
      .setImageUsage(vk::ImageUsageFlagBits::eColorAttachment)
      .setImageSharingMode(vk::SharingMode::eExclusive)
      .setPreTransform(caps.currentTransform)
      .setCompositeAlpha(*composite_alpha)
      .setPresentMode(vk::PresentModeKHR::eFifo)
      .setClipped(VK_TRUE);
  auto [swapchain_result, swapchain] = device_->createSwapchainKHRUnique(create_info);
  if (swapchain_result != vk::Result::eSuccess) {
    return Fail("vkCreateSwapchainKHR", swapchain_result);
  }
  swapchain_ = std::move(swapchain);
  return true;
}

bool Fractal::CreateColorRenderPass(vk::AttachmentLoadOp load_op, vk::UniqueRenderPass* out) {
  // The recorder transitions the attachment to COLOR_ATTACHMENT_OPTIMAL before the render pass
  // and away from it afterwards, so the pass itself does no layout transitions.
  vk::AttachmentDescription attachment;
  attachment.setFormat(surface_format_.format)
      .setSamples(vk::SampleCountFlagBits::e1)
      .setLoadOp(load_op)
      .setStoreOp(vk::AttachmentStoreOp::eStore)
      .setStencilLoadOp(vk::AttachmentLoadOp::eDontCare)
      .setStencilStoreOp(vk::AttachmentStoreOp::eDontCare)
      .setInitialLayout(vk::ImageLayout::eColorAttachmentOptimal)
      .setFinalLayout(vk::ImageLayout::eColorAttachmentOptimal);
  vk::AttachmentReference color_reference;
  color_reference.setAttachment(0).setLayout(vk::ImageLayout::eColorAttachmentOptimal);
  vk::SubpassDescription subpass;
  subpass.setPipelineBindPoint(vk::PipelineBindPoint::eGraphics)
      .setColorAttachments(color_reference);
  vk::RenderPassCreateInfo render_pass_info;
  render_pass_info.setAttachments(attachment).setSubpasses(subpass);
  auto [render_pass_result, render_pass] = device_->createRenderPassUnique(render_pass_info);
  if (render_pass_result != vk::Result::eSuccess) {
    return Fail("vkCreateRenderPass", render_pass_result);
  }
  *out = std::move(render_pass);
  return true;
}

bool Fractal::CreateShaderModule(const uint32_t* code, size_t size, vk::UniqueShaderModule* out) {
  vk::ShaderModuleCreateInfo module_info;
  module_info.setCodeSize(size).setPCode(code);
  auto [module_result, shader_module] = device_->createShaderModuleUnique(module_info);
  if (module_result != vk::Result::eSuccess) {
    return Fail("vkCreateShaderModule", module_result);
  }
  *out = std::move(shader_module);
  return true;
}

bool Fractal::CreateFullscreenPipeline(vk::ShaderModule vertex, vk::ShaderModule fragment,
                                       vk::RenderPass render_pass, vk::PipelineLayout layout,
                                       vk::Rect2D viewport_rect, vk::UniquePipeline* out,
                                       const vk::SpecializationInfo* vertex_specialization,
                                       const vk::SpecializationInfo* fragment_specialization) {
  std::array<vk::PipelineShaderStageCreateInfo, 2> stages;
  stages[0]
      .setStage(vk::ShaderStageFlagBits::eVertex)
      .setModule(vertex)
      .setPName("main")
      .setPSpecializationInfo(vertex_specialization);
  stages[1]
      .setStage(vk::ShaderStageFlagBits::eFragment)
      .setModule(fragment)
      .setPName("main")
      .setPSpecializationInfo(fragment_specialization);

  // The vertex shader generates a full-screen triangle from gl_VertexIndex.
  vk::PipelineVertexInputStateCreateInfo vertex_input;
  vk::PipelineInputAssemblyStateCreateInfo input_assembly;
  input_assembly.setTopology(vk::PrimitiveTopology::eTriangleList);

  vk::Viewport viewport;
  viewport.setX(static_cast<float>(viewport_rect.offset.x))
      .setY(static_cast<float>(viewport_rect.offset.y))
      .setWidth(static_cast<float>(viewport_rect.extent.width))
      .setHeight(static_cast<float>(viewport_rect.extent.height))
      .setMinDepth(0.0f)
      .setMaxDepth(1.0f);
  vk::PipelineViewportStateCreateInfo viewport_state;
  viewport_state.setViewports(viewport).setScissors(viewport_rect);

  vk::PipelineRasterizationStateCreateInfo rasterization;
  rasterization.setPolygonMode(vk::PolygonMode::eFill)
      .setCullMode(vk::CullModeFlagBits::eNone)
      .setFrontFace(vk::FrontFace::eCounterClockwise)
      .setLineWidth(1.0f);
  vk::PipelineMultisampleStateCreateInfo multisample;
  multisample.setRasterizationSamples(vk::SampleCountFlagBits::e1);
  vk::PipelineColorBlendAttachmentState blend_attachment;
  blend_attachment.setColorWriteMask(
      vk::ColorComponentFlagBits::eR | vk::ColorComponentFlagBits::eG |
      vk::ColorComponentFlagBits::eB | vk::ColorComponentFlagBits::eA);
  vk::PipelineColorBlendStateCreateInfo color_blend;
  color_blend.setAttachments(blend_attachment);

  vk::GraphicsPipelineCreateInfo pipeline_info;
  pipeline_info.setStages(stages)
      .setPVertexInputState(&vertex_input)
      .setPInputAssemblyState(&input_assembly)
      .setPViewportState(&viewport_state)
      .setPRasterizationState(&rasterization)
      .setPMultisampleState(&multisample)
      .setPColorBlendState(&color_blend)
      .setLayout(layout)
      .setRenderPass(render_pass)
      .setSubpass(0);
  auto [pipeline_result, pipeline] = device_->createGraphicsPipelineUnique(nullptr, pipeline_info);
  if (pipeline_result != vk::Result::eSuccess) {
    return Fail("vkCreateGraphicsPipelines", pipeline_result);
  }
  *out = std::move(pipeline);
  return true;
}

bool Fractal::CreatePipeline() {
  // Every pixel is written, so the previous contents are not loaded.
  if (!CreateColorRenderPass(vk::AttachmentLoadOp::eDontCare, &render_pass_)) {
    return false;
  }

  vk::PushConstantRange push_constant_range;
  push_constant_range.setStageFlags(vk::ShaderStageFlagBits::eFragment)
      .setOffset(0)
      .setSize(sizeof(PushConstants));
  vk::PipelineLayoutCreateInfo layout_info;
  layout_info.setPushConstantRanges(push_constant_range);
  auto [layout_result, layout] = device_->createPipelineLayoutUnique(layout_info);
  if (layout_result != vk::Result::eSuccess) {
    return Fail("vkCreatePipelineLayout", layout_result);
  }
  pipeline_layout_ = std::move(layout);

  vk::UniqueShaderModule vertex_module;
  vk::UniqueShaderModule fragment_module;
  if (!CreateShaderModule(fullscreen_vert_spv, sizeof(fullscreen_vert_spv), &vertex_module) ||
      !CreateShaderModule(fractal_frag_spv, sizeof(fractal_frag_spv), &fragment_module)) {
    return false;
  }
  // fractal.frag's kFixedIterations (constant_id 0) is a bool, which is 32 bits in SPIR-V.
  const VkBool32 fixed_iterations = options_.fixed_iterations ? VK_TRUE : VK_FALSE;
  vk::SpecializationMapEntry map_entry;
  map_entry.setConstantID(0).setOffset(0).setSize(sizeof(fixed_iterations));
  vk::SpecializationInfo specialization;
  specialization.setMapEntries(map_entry)
      .setDataSize(sizeof(fixed_iterations))
      .setPData(&fixed_iterations);
  vk::Rect2D viewport_rect;
  viewport_rect.setExtent(extent_);
  return CreateFullscreenPipeline(*vertex_module, *fragment_module, *render_pass_,
                                  *pipeline_layout_, viewport_rect, &pipeline_,
                                  /*vertex_specialization=*/nullptr, &specialization);
}

bool Fractal::CreateScalePipeline() {
  if (display_mode_ != DisplayMode::kScale) {
    return true;
  }

  vk::SamplerCreateInfo sampler_info;
  sampler_info.setMagFilter(vk::Filter::eLinear)
      .setMinFilter(vk::Filter::eLinear)
      .setMipmapMode(vk::SamplerMipmapMode::eNearest)
      .setAddressModeU(vk::SamplerAddressMode::eClampToEdge)
      .setAddressModeV(vk::SamplerAddressMode::eClampToEdge)
      .setAddressModeW(vk::SamplerAddressMode::eClampToEdge)
      .setMaxLod(0.0f);
  auto [sampler_result, sampler] = device_->createSamplerUnique(sampler_info);
  if (sampler_result != vk::Result::eSuccess) {
    return Fail("vkCreateSampler", sampler_result);
  }
  scale_sampler_ = std::move(sampler);

  vk::DescriptorSetLayoutBinding binding;
  binding.setBinding(0)
      .setDescriptorType(vk::DescriptorType::eCombinedImageSampler)
      .setStageFlags(vk::ShaderStageFlagBits::eFragment)
      .setImmutableSamplers(*scale_sampler_);
  vk::DescriptorSetLayoutCreateInfo set_layout_info;
  set_layout_info.setBindings(binding);
  auto [set_layout_result, set_layout] = device_->createDescriptorSetLayoutUnique(set_layout_info);
  if (set_layout_result != vk::Result::eSuccess) {
    return Fail("vkCreateDescriptorSetLayout", set_layout_result);
  }
  scale_descriptor_set_layout_ = std::move(set_layout);

  vk::PipelineLayoutCreateInfo layout_info;
  layout_info.setSetLayouts(*scale_descriptor_set_layout_);
  auto [layout_result, layout] = device_->createPipelineLayoutUnique(layout_info);
  if (layout_result != vk::Result::eSuccess) {
    return Fail("vkCreatePipelineLayout", layout_result);
  }
  scale_pipeline_layout_ = std::move(layout);

  // The image is stretched over the whole display, so every pixel is written.
  if (!CreateColorRenderPass(vk::AttachmentLoadOp::eDontCare, &scale_render_pass_)) {
    return false;
  }

  vk::UniqueShaderModule vertex_module;
  vk::UniqueShaderModule fragment_module;
  if (!CreateShaderModule(scale_vert_spv, sizeof(scale_vert_spv), &vertex_module) ||
      !CreateShaderModule(scale_frag_spv, sizeof(scale_frag_spv), &fragment_module)) {
    return false;
  }
  // scale.vert's kRotateClockwise (constant_id 0) is a bool, which is 32 bits in SPIR-V.
  const VkBool32 rotate_clockwise = rotate_ ? VK_TRUE : VK_FALSE;
  vk::SpecializationMapEntry map_entry;
  map_entry.setConstantID(0).setOffset(0).setSize(sizeof(rotate_clockwise));
  vk::SpecializationInfo specialization;
  specialization.setMapEntries(map_entry)
      .setDataSize(sizeof(rotate_clockwise))
      .setPData(&rotate_clockwise);
  vk::Rect2D viewport_rect;
  viewport_rect.setExtent(display_extent_);
  return CreateFullscreenPipeline(*vertex_module, *fragment_module, *scale_render_pass_,
                                  *scale_pipeline_layout_, viewport_rect, &scale_pipeline_,
                                  &specialization);
}

std::optional<uint32_t> Fractal::FindMemoryType(uint32_t type_bits,
                                                vk::MemoryPropertyFlags properties) const {
  const vk::PhysicalDeviceMemoryProperties memory_properties =
      physical_device_.getMemoryProperties();
  for (uint32_t i = 0; i < memory_properties.memoryTypeCount; ++i) {
    if ((type_bits & (1u << i)) &&
        (memory_properties.memoryTypes[i].propertyFlags & properties) == properties) {
      return i;
    }
  }
  return std::nullopt;
}

bool Fractal::CreateOffscreenImage(FrameSlot* slot) {
  vk::ImageUsageFlags usage = vk::ImageUsageFlagBits::eColorAttachment;
  if (display_mode_ == DisplayMode::kScale) {
    usage |= vk::ImageUsageFlagBits::eSampled;
  }

  vk::Extent3D extent;
  extent.setWidth(extent_.width).setHeight(extent_.height).setDepth(1);
  vk::ImageCreateInfo image_info;
  image_info.setImageType(vk::ImageType::e2D)
      .setFormat(surface_format_.format)
      .setExtent(extent)
      .setMipLevels(1)
      .setArrayLayers(1)
      .setSamples(vk::SampleCountFlagBits::e1)
      .setTiling(vk::ImageTiling::eOptimal)
      .setUsage(usage)
      .setSharingMode(vk::SharingMode::eExclusive)
      .setInitialLayout(vk::ImageLayout::eUndefined);
  auto [image_result, image] = device_->createImageUnique(image_info);
  if (image_result != vk::Result::eSuccess) {
    return Fail("vkCreateImage", image_result);
  }

  const vk::MemoryRequirements requirements = device_->getImageMemoryRequirements(*image);
  std::optional<uint32_t> memory_type =
      FindMemoryType(requirements.memoryTypeBits, vk::MemoryPropertyFlagBits::eDeviceLocal);
  if (!memory_type) {
    memory_type = FindMemoryType(requirements.memoryTypeBits, {});
  }
  if (!memory_type) {
    return Fail("no memory type for the offscreen image");
  }
  vk::MemoryAllocateInfo allocate_info;
  allocate_info.setAllocationSize(requirements.size).setMemoryTypeIndex(*memory_type);
  auto [memory_result, memory] = device_->allocateMemoryUnique(allocate_info);
  if (memory_result != vk::Result::eSuccess) {
    return Fail("vkAllocateMemory", memory_result);
  }
  if (vk::Result bind_result = device_->bindImageMemory(*image, *memory, 0);
      bind_result != vk::Result::eSuccess) {
    return Fail("vkBindImageMemory", bind_result);
  }
  slot->offscreen_image = std::move(image);
  slot->offscreen_memory = std::move(memory);
  return true;
}

bool Fractal::CreateTarget(vk::Image image, Target* target) {
  vk::ImageSubresourceRange range;
  range.setAspectMask(vk::ImageAspectFlagBits::eColor).setLevelCount(1).setLayerCount(1);
  vk::ImageViewCreateInfo view_info;
  view_info.setImage(image)
      .setViewType(vk::ImageViewType::e2D)
      .setFormat(surface_format_.format)
      .setSubresourceRange(range);
  auto [view_result, view] = device_->createImageViewUnique(view_info);
  if (view_result != vk::Result::eSuccess) {
    return Fail("vkCreateImageView", view_result);
  }
  target->image = image;
  target->view = std::move(view);

  vk::FramebufferCreateInfo framebuffer_info;
  framebuffer_info.setRenderPass(*render_pass_)
      .setAttachments(*target->view)
      .setWidth(extent_.width)
      .setHeight(extent_.height)
      .setLayers(1);
  auto [framebuffer_result, framebuffer] = device_->createFramebufferUnique(framebuffer_info);
  if (framebuffer_result != vk::Result::eSuccess) {
    return Fail("vkCreateFramebuffer", framebuffer_result);
  }
  target->framebuffer = std::move(framebuffer);
  return true;
}

bool Fractal::CreateFrameSlots() {
  vk::CommandPoolCreateInfo pool_info;
  pool_info.setFlags(vk::CommandPoolCreateFlagBits::eResetCommandBuffer)
      .setQueueFamilyIndex(queue_family_);
  auto [pool_result, pool] = device_->createCommandPoolUnique(pool_info);
  if (pool_result != vk::Result::eSuccess) {
    return Fail("vkCreateCommandPool", pool_result);
  }
  command_pool_ = std::move(pool);

  // Scale mode records the scale pass into a second command buffer per slot.
  const bool scale = display_mode_ == DisplayMode::kScale;
  vk::CommandBufferAllocateInfo allocate_info;
  allocate_info.setCommandPool(*command_pool_)
      .setLevel(vk::CommandBufferLevel::ePrimary)
      .setCommandBufferCount(options_.frames_in_flight * (scale ? 2 : 1));
  auto [buffers_result, command_buffers] = device_->allocateCommandBuffersUnique(allocate_info);
  if (buffers_result != vk::Result::eSuccess) {
    return Fail("vkAllocateCommandBuffers", buffers_result);
  }

  slots_.resize(options_.frames_in_flight);
  for (uint32_t i = 0; i < options_.frames_in_flight; ++i) {
    FrameSlot& slot = slots_[i];
    slot.command_buffer = std::move(command_buffers[i]);
    if (scale) {
      slot.scale_command_buffer = std::move(command_buffers[options_.frames_in_flight + i]);
    }

    auto [semaphore_result, semaphore] = device_->createSemaphoreUnique({});
    if (semaphore_result != vk::Result::eSuccess) {
      return Fail("vkCreateSemaphore", semaphore_result);
    }
    slot.acquire_semaphore = std::move(semaphore);

    vk::FenceCreateInfo fence_info;
    fence_info.setFlags(vk::FenceCreateFlagBits::eSignaled);
    auto [fence_result, fence] = device_->createFenceUnique(fence_info);
    if (fence_result != vk::Result::eSuccess) {
      return Fail("vkCreateFence", fence_result);
    }
    slot.fence = std::move(fence);

    if (!CreateOffscreenImage(&slot)) {
      return false;
    }
  }
  return true;
}

bool Fractal::CreateTargets() {
  if (swapchain_) {
    auto [images_result, swapchain_images] = device_->getSwapchainImagesKHR(*swapchain_);
    if (images_result != vk::Result::eSuccess) {
      return Fail("vkGetSwapchainImagesKHR", images_result);
    }
    swapchain_images_ = std::move(swapchain_images);
  }

  // Offscreen images live in the frame slots, so create the slots first.
  if (!CreateFrameSlots()) {
    return false;
  }

  const size_t swapchain_target_count =
      display_mode_ == DisplayMode::kDirect ? swapchain_images_.size() : 0;
  swapchain_targets_.resize(swapchain_target_count);
  for (size_t i = 0; i < swapchain_target_count; ++i) {
    if (!CreateTarget(swapchain_images_[i], &swapchain_targets_[i])) {
      return false;
    }
  }
  for (size_t i = 0; i < swapchain_images_.size(); ++i) {
    auto [semaphore_result, semaphore] = device_->createSemaphoreUnique({});
    if (semaphore_result != vk::Result::eSuccess) {
      return Fail("vkCreateSemaphore", semaphore_result);
    }
    present_semaphores_.push_back(std::move(semaphore));
  }

  offscreen_targets_.resize(slots_.size());
  for (size_t i = 0; i < slots_.size(); ++i) {
    if (!CreateTarget(*slots_[i].offscreen_image, &offscreen_targets_[i])) {
      return false;
    }
  }
  return CreateScaleTargets();
}

bool Fractal::CreateScaleTargets() {
  if (display_mode_ != DisplayMode::kScale) {
    return true;
  }

  vk::ImageSubresourceRange range;
  range.setAspectMask(vk::ImageAspectFlagBits::eColor).setLevelCount(1).setLayerCount(1);
  scale_targets_.resize(swapchain_images_.size());
  for (size_t i = 0; i < swapchain_images_.size(); ++i) {
    vk::ImageViewCreateInfo view_info;
    view_info.setImage(swapchain_images_[i])
        .setViewType(vk::ImageViewType::e2D)
        .setFormat(surface_format_.format)
        .setSubresourceRange(range);
    auto [view_result, view] = device_->createImageViewUnique(view_info);
    if (view_result != vk::Result::eSuccess) {
      return Fail("vkCreateImageView", view_result);
    }
    scale_targets_[i].view = std::move(view);

    vk::FramebufferCreateInfo framebuffer_info;
    framebuffer_info.setRenderPass(*scale_render_pass_)
        .setAttachments(*scale_targets_[i].view)
        .setWidth(display_extent_.width)
        .setHeight(display_extent_.height)
        .setLayers(1);
    auto [framebuffer_result, framebuffer] = device_->createFramebufferUnique(framebuffer_info);
    if (framebuffer_result != vk::Result::eSuccess) {
      return Fail("vkCreateFramebuffer", framebuffer_result);
    }
    scale_targets_[i].framebuffer = std::move(framebuffer);
  }

  // One descriptor set per offscreen image, which the scale pass samples.
  const uint32_t set_count = static_cast<uint32_t>(offscreen_targets_.size());
  vk::DescriptorPoolSize pool_size;
  pool_size.setType(vk::DescriptorType::eCombinedImageSampler).setDescriptorCount(set_count);
  vk::DescriptorPoolCreateInfo pool_info;
  pool_info.setMaxSets(set_count).setPoolSizes(pool_size);
  auto [pool_result, pool] = device_->createDescriptorPoolUnique(pool_info);
  if (pool_result != vk::Result::eSuccess) {
    return Fail("vkCreateDescriptorPool", pool_result);
  }
  scale_descriptor_pool_ = std::move(pool);

  for (Target& target : offscreen_targets_) {
    vk::DescriptorSetAllocateInfo allocate_info;
    allocate_info.setDescriptorPool(*scale_descriptor_pool_)
        .setSetLayouts(*scale_descriptor_set_layout_);
    auto [sets_result, sets] = device_->allocateDescriptorSets(allocate_info);
    if (sets_result != vk::Result::eSuccess) {
      return Fail("vkAllocateDescriptorSets", sets_result);
    }
    // The sampler is immutable, so only the view and layout are written.
    vk::DescriptorImageInfo image_descriptor;
    image_descriptor.setImageView(*target.view)
        .setImageLayout(vk::ImageLayout::eShaderReadOnlyOptimal);
    vk::WriteDescriptorSet write;
    write.setDstSet(sets[0])
        .setDstBinding(0)
        .setDescriptorType(vk::DescriptorType::eCombinedImageSampler)
        .setImageInfo(image_descriptor);
    device_->updateDescriptorSets(write, nullptr);
    target.scale_descriptor_set = sets[0];
  }
  return true;
}

bool Fractal::RecordFrame(vk::CommandBuffer command_buffer, const Target& target, bool present,
                          const PushConstants& push_constants, bool loop_start, bool loop_end) {
  vk::CommandBufferBeginInfo begin_info;
  begin_info.setFlags(vk::CommandBufferUsageFlagBits::eOneTimeSubmit);
  if (vk::Result result = command_buffer.begin(begin_info); result != vk::Result::eSuccess) {
    return Fail("vkBeginCommandBuffer", result);
  }

  // A timed loop's GPU time runs from the start of its first frame to the end of its last.
  if (query_pool_ && loop_start) {
    command_buffer.resetQueryPool(*query_pool_, 0, 2);
    command_buffer.writeTimestamp(vk::PipelineStageFlagBits::eTopOfPipe, *query_pool_, 0);
  }

  vk::ImageSubresourceRange range;
  range.setAspectMask(vk::ImageAspectFlagBits::eColor).setLevelCount(1).setLayerCount(1);

  // Discard the previous contents. A swapchain image's transition must chain after the acquire
  // semaphore wait (at COLOR_ATTACHMENT_OUTPUT). An offscreen image's previous use already
  // completed (its slot fence was waited on), so it waits for nothing and consecutive frames can
  // overlap.
  vk::ImageMemoryBarrier to_write;
  to_write.setDstAccessMask(vk::AccessFlagBits::eColorAttachmentWrite)
      .setOldLayout(vk::ImageLayout::eUndefined)
      .setNewLayout(vk::ImageLayout::eColorAttachmentOptimal)
      .setSrcQueueFamilyIndex(VK_QUEUE_FAMILY_IGNORED)
      .setDstQueueFamilyIndex(VK_QUEUE_FAMILY_IGNORED)
      .setImage(target.image)
      .setSubresourceRange(range);
  command_buffer.pipelineBarrier(present ? vk::PipelineStageFlagBits::eColorAttachmentOutput
                                         : vk::PipelineStageFlagBits::eTopOfPipe,
                                 vk::PipelineStageFlagBits::eColorAttachmentOutput, {}, nullptr,
                                 nullptr, to_write);

  command_buffer.bindPipeline(vk::PipelineBindPoint::eGraphics, *pipeline_);
  command_buffer.pushConstants(*pipeline_layout_, vk::ShaderStageFlagBits::eFragment, 0,
                               sizeof(push_constants), &push_constants);
  vk::Rect2D render_area;
  render_area.setExtent(extent_);
  vk::RenderPassBeginInfo render_pass_begin;
  render_pass_begin.setRenderPass(*render_pass_)
      .setFramebuffer(*target.framebuffer)
      .setRenderArea(render_area);
  command_buffer.beginRenderPass(render_pass_begin, vk::SubpassContents::eInline);
  command_buffer.draw(3, 1, 0, 0);
  command_buffer.endRenderPass();

  if (present) {
    vk::ImageMemoryBarrier to_present;
    to_present.setSrcAccessMask(vk::AccessFlagBits::eColorAttachmentWrite)
        .setOldLayout(vk::ImageLayout::eColorAttachmentOptimal)
        .setNewLayout(vk::ImageLayout::ePresentSrcKHR)
        .setSrcQueueFamilyIndex(VK_QUEUE_FAMILY_IGNORED)
        .setDstQueueFamilyIndex(VK_QUEUE_FAMILY_IGNORED)
        .setImage(target.image)
        .setSubresourceRange(range);
    command_buffer.pipelineBarrier(vk::PipelineStageFlagBits::eColorAttachmentOutput,
                                   vk::PipelineStageFlagBits::eBottomOfPipe, {}, nullptr, nullptr,
                                   to_present);
  }

  if (query_pool_ && loop_end) {
    command_buffer.writeTimestamp(vk::PipelineStageFlagBits::eBottomOfPipe, *query_pool_, 1);
  }

  if (vk::Result result = command_buffer.end(); result != vk::Result::eSuccess) {
    return Fail("vkEndCommandBuffer", result);
  }
  return true;
}

bool Fractal::RecordScale(vk::CommandBuffer command_buffer, const Target& source,
                          uint32_t image_index, bool loop_end) {
  vk::CommandBufferBeginInfo begin_info;
  begin_info.setFlags(vk::CommandBufferUsageFlagBits::eOneTimeSubmit);
  if (vk::Result result = command_buffer.begin(begin_info); result != vk::Result::eSuccess) {
    return Fail("vkBeginCommandBuffer", result);
  }

  vk::ImageSubresourceRange range;
  range.setAspectMask(vk::ImageAspectFlagBits::eColor).setLevelCount(1).setLayerCount(1);
  const vk::Image display_image = swapchain_images_[image_index];

  // Make the fractal, rendered by the previous batch, readable by the scale pass. The swapchain
  // image's transition chains after the acquire semaphore wait (at COLOR_ATTACHMENT_OUTPUT).
  std::array<vk::ImageMemoryBarrier, 2> to_scale;
  to_scale[0]
      .setSrcAccessMask(vk::AccessFlagBits::eColorAttachmentWrite)
      .setDstAccessMask(vk::AccessFlagBits::eShaderRead)
      .setOldLayout(vk::ImageLayout::eColorAttachmentOptimal)
      .setNewLayout(vk::ImageLayout::eShaderReadOnlyOptimal)
      .setSrcQueueFamilyIndex(VK_QUEUE_FAMILY_IGNORED)
      .setDstQueueFamilyIndex(VK_QUEUE_FAMILY_IGNORED)
      .setImage(source.image)
      .setSubresourceRange(range);
  to_scale[1]
      .setDstAccessMask(vk::AccessFlagBits::eColorAttachmentWrite)
      .setOldLayout(vk::ImageLayout::eUndefined)
      .setNewLayout(vk::ImageLayout::eColorAttachmentOptimal)
      .setSrcQueueFamilyIndex(VK_QUEUE_FAMILY_IGNORED)
      .setDstQueueFamilyIndex(VK_QUEUE_FAMILY_IGNORED)
      .setImage(display_image)
      .setSubresourceRange(range);
  command_buffer.pipelineBarrier(vk::PipelineStageFlagBits::eColorAttachmentOutput,
                                 vk::PipelineStageFlagBits::eFragmentShader |
                                     vk::PipelineStageFlagBits::eColorAttachmentOutput,
                                 {}, nullptr, nullptr, to_scale);

  vk::Rect2D render_area;
  render_area.setExtent(display_extent_);
  vk::RenderPassBeginInfo render_pass_begin;
  render_pass_begin.setRenderPass(*scale_render_pass_)
      .setFramebuffer(*scale_targets_[image_index].framebuffer)
      .setRenderArea(render_area);
  command_buffer.beginRenderPass(render_pass_begin, vk::SubpassContents::eInline);
  command_buffer.bindPipeline(vk::PipelineBindPoint::eGraphics, *scale_pipeline_);
  command_buffer.bindDescriptorSets(vk::PipelineBindPoint::eGraphics, *scale_pipeline_layout_, 0,
                                    source.scale_descriptor_set, nullptr);
  command_buffer.draw(3, 1, 0, 0);
  command_buffer.endRenderPass();

  vk::ImageMemoryBarrier to_present;
  to_present.setSrcAccessMask(vk::AccessFlagBits::eColorAttachmentWrite)
      .setOldLayout(vk::ImageLayout::eColorAttachmentOptimal)
      .setNewLayout(vk::ImageLayout::ePresentSrcKHR)
      .setSrcQueueFamilyIndex(VK_QUEUE_FAMILY_IGNORED)
      .setDstQueueFamilyIndex(VK_QUEUE_FAMILY_IGNORED)
      .setImage(display_image)
      .setSubresourceRange(range);
  command_buffer.pipelineBarrier(vk::PipelineStageFlagBits::eColorAttachmentOutput,
                                 vk::PipelineStageFlagBits::eBottomOfPipe, {}, nullptr, nullptr,
                                 to_present);

  if (query_pool_ && loop_end) {
    command_buffer.writeTimestamp(vk::PipelineStageFlagBits::eBottomOfPipe, *query_pool_, 1);
  }

  if (vk::Result result = command_buffer.end(); result != vk::Result::eSuccess) {
    return Fail("vkEndCommandBuffer", result);
  }
  return true;
}

bool Fractal::RenderFrame(size_t slot_index, const PushConstants& push_constants, bool loop_start,
                          bool loop_end, bool* presented) {
  // The first write to the swapchain image, in direct and scale modes.
  const vk::PipelineStageFlags acquire_wait_stage =
      vk::PipelineStageFlagBits::eColorAttachmentOutput;
  FrameSlot& slot = slots_[slot_index];

  // The only CPU wait: for this slot's previous frame on the GPU. The other slots stay
  // queued, so the GPU always has work.
  if (vk::Result result = device_->waitForFences(*slot.fence, VK_TRUE, UINT64_MAX);
      result != vk::Result::eSuccess) {
    return Fail("vkWaitForFences", result);
  }
  if (vk::Result result = device_->resetFences(*slot.fence); result != vk::Result::eSuccess) {
    return Fail("vkResetFences", result);
  }

  // Never block on the display: take a swapchain image only if one is already free.
  // Note: the spec leaves the semaphore untouched on VK_NOT_READY, but the swapchain layer may
  // already have imported a signaled payload into it. The layer's next successful acquire
  // replaces that payload, so reusing the semaphore is harmless with this layer.
  bool to_display = false;
  uint32_t image_index = 0;
  if (swapchain_) {
    auto [acquire_result, acquired_index] =
        device_->acquireNextImageKHR(*swapchain_, 0, *slot.acquire_semaphore, nullptr);
    if (acquire_result == vk::Result::eSuccess || acquire_result == vk::Result::eSuboptimalKHR) {
      to_display = true;
      image_index = acquired_index;
    } else if (acquire_result != vk::Result::eNotReady && acquire_result != vk::Result::eTimeout) {
      return Fail("vkAcquireNextImageKHR", acquire_result);
    }
  }

  const bool scale = display_mode_ == DisplayMode::kScale;
  const bool direct = to_display && !scale;
  const Target& target = direct ? swapchain_targets_[image_index] : offscreen_targets_[slot_index];
  if (!RecordFrame(*slot.command_buffer, target, direct, push_constants, loop_start,
                   loop_end && !(to_display && scale))) {
    return false;
  }
  if (to_display && scale &&
      !RecordScale(*slot.scale_command_buffer, target, static_cast<uint32_t>(image_index),
                   loop_end)) {
    return false;
  }

  // In scale mode the fractal and the scale pass are separate batches, and only the second one
  // waits for the swapchain image. The fence covers both.
  std::array<vk::SubmitInfo, 2> submit_infos;
  uint32_t submit_count = 1;
  submit_infos[0].setCommandBuffers(*slot.command_buffer);
  if (to_display) {
    vk::SubmitInfo& present_submit = scale ? submit_infos[submit_count++] : submit_infos[0];
    if (scale) {
      present_submit.setCommandBuffers(*slot.scale_command_buffer);
    }
    present_submit.setWaitSemaphores(*slot.acquire_semaphore)
        .setWaitDstStageMask(acquire_wait_stage)
        .setSignalSemaphores(*present_semaphores_[image_index]);
  }
  if (vk::Result result = queue_.submit(
          vk::ArrayProxy<const vk::SubmitInfo>(submit_count, submit_infos.data()), *slot.fence);
      result != vk::Result::eSuccess) {
    return Fail("vkQueueSubmit", result);
  }

  if (to_display) {
    // Does not wait for vsync: the swapchain layer sends a one-way CommitConfig to the
    // display coordinator, which shows the image once its rendering completes.
    vk::PresentInfoKHR present_info;
    present_info.setWaitSemaphores(*present_semaphores_[image_index])
        .setSwapchains(*swapchain_)
        .setImageIndices(image_index);
    vk::Result present_result = queue_.presentKHR(present_info);
    if (present_result != vk::Result::eSuccess && present_result != vk::Result::eSuboptimalKHR) {
      return Fail("vkQueuePresentKHR", present_result);
    }
  }
  *presented = to_display;
  return true;
}

bool Fractal::Run() {
  std::string display;
  switch (display_mode_) {
    case DisplayMode::kNone:
      display = "offscreen";
      break;
    case DisplayMode::kDirect:
      display = "direct";
      break;
    case DisplayMode::kScale:
      display = std::string(rotate_ ? "rotated clockwise and stretched" : "stretched") + " to a " +
                std::to_string(display_extent_.width) + "x" +
                std::to_string(display_extent_.height) + " display";
      break;
  }
  const std::string loops = options_.loops
                                ? std::format("{} loop{} of {} frames", *options_.loops,
                                              *options_.loops == 1 ? "" : "s", options_.frames)
                                : std::format("loops of {} frames forever", options_.frames);
  std::println(
      "vkfractal: {}, {}x{} {} ({}), {} warm-up frames, then {}, max iterations {}, {}, {} frames "
      "in flight, validation {}, timing: {}",
      device_name_, extent_.width, extent_.height, vk::to_string(surface_format_.format), display,
      options_.warmup_frames, loops, options_.max_iterations,
      options_.fixed_iterations ? "fixed iterations" : "early exit", options_.frames_in_flight,
      options_.validation ? "on" : "off", query_pool_ ? "CPU and GPU" : "CPU only");
  std::println("vkfractal: extents {:.3f}x{:.3f}", extents_.width, extents_.height);

  // The magma colormap at 0, 0.1, ..., 1, then back down to 0, as 0xRRGGBB.
  constexpr std::array<uint32_t, 21> kPalette = {
      0x000004, 0x140e36, 0x3b0f70, 0x641a80, 0x8c2981, 0xb73779, 0xde4968,
      0xf7705c, 0xfe9f6d, 0xfecf92, 0xfcfdbf, 0xfecf92, 0xfe9f6d, 0xf7705c,
      0xde4968, 0xb73779, 0x8c2981, 0x641a80, 0x3b0f70, 0x140e36, 0x000004,
  };
  // Complex units per pixel. They differ between the axes when the image's aspect ratio differs
  // from the extents'; the image is then stretched back onto the display.
  const float step_x = static_cast<float>(extents_.width / extent_.width);
  const float step_y = static_cast<float>(extents_.height / extent_.height);
  PushConstants push_constants = {
      .half_extent = {extent_.width / 2.0f, extent_.height / 2.0f},
      .step = {step_x, step_y},
      .c = {0.0f, 0.0f},
      .max_iterations = options_.max_iterations,
      .palette = kPalette,
  };
  // unpackUnorm4x8 takes red from the lowest byte.
  for (uint32_t& color : push_constants.palette) {
    color = (color >> 16u) | (color & 0xff00u) | ((color & 0xffu) << 16u);
  }
  const std::vector<std::complex<double>> path = JuliaPath(options_.frames);
  const auto set_frame = [&](uint32_t frame) {
    push_constants.c[0] = static_cast<float>(path[frame].real());
    push_constants.c[1] = static_cast<float>(path[frame].imag());
  };
  // Prints "<frames> frames (<presented> presented) in ..." after |prefix|.
  const auto print_times = [](const std::string& prefix, uint64_t frames, uint64_t presented,
                              double seconds, std::optional<double> gpu_seconds) {
    const double count = static_cast<double>(frames);
    std::string gpu;
    if (gpu_seconds) {
      gpu = std::format("; GPU {:.3f} s: {:.3f} ms/frame", *gpu_seconds,
                        1000.0 * *gpu_seconds / count);
    }
    std::println("vkfractal: {}{} frames ({} presented) in {:.3f} s: {:.3f} ms/frame, {:.1f} fps{}",
                 prefix, frames, presented, seconds, 1000.0 * seconds / count, count / seconds,
                 gpu);
    fflush(stdout);
  };

  // Frames take the slots in turn.
  uint64_t submitted = 0;
  bool frame_presented = false;

  // The warm-up frames render frame 0 and are not timed, so that one-time driver work on the first
  // submissions is not measured.
  set_frame(0);
  for (uint32_t i = 0; i < options_.warmup_frames; ++i) {
    if (!RenderFrame(submitted++ % slots_.size(), push_constants, false, false, &frame_presented)) {
      return false;
    }
  }

  uint64_t total_frames = 0;
  uint64_t total_presented = 0;
  double total_seconds = 0.0;
  double total_gpu_seconds = 0.0;
  for (uint64_t loop = 1; !options_.loops || loop <= *options_.loops; ++loop) {
    // Time each loop from an idle GPU until the GPU is idle again, so that the CPU and GPU clocks
    // time the same frames.
    if (vk::Result result = device_->waitIdle(); result != vk::Result::eSuccess) {
      return Fail("vkDeviceWaitIdle", result);
    }
    const auto start = std::chrono::steady_clock::now();
    uint64_t presented = 0;
    for (uint32_t frame = 0; frame < options_.frames; ++frame) {
      set_frame(frame);
      if (!RenderFrame(submitted++ % slots_.size(), push_constants, frame == 0,
                       frame == options_.frames - 1, &frame_presented)) {
        return false;
      }
      if (frame_presented) {
        ++presented;
      }
    }
    if (vk::Result result = device_->waitIdle(); result != vk::Result::eSuccess) {
      return Fail("vkDeviceWaitIdle", result);
    }
    const std::chrono::duration<double> elapsed = std::chrono::steady_clock::now() - start;

    std::optional<double> gpu_seconds;
    if (query_pool_) {
      auto [query_result, timestamps] = device_->getQueryPoolResults<uint64_t>(
          *query_pool_, 0, 2, 2 * sizeof(uint64_t), sizeof(uint64_t),
          vk::QueryResultFlagBits::e64 | vk::QueryResultFlagBits::eWait);
      if (query_result != vk::Result::eSuccess) {
        return Fail("vkGetQueryPoolResults", query_result);
      }
      // Only the low timestamp_valid_bits_ bits count, so the difference wraps around there.
      const uint64_t mask = timestamp_valid_bits_ >= 64
                                ? std::numeric_limits<uint64_t>::max()
                                : (uint64_t{1} << timestamp_valid_bits_) - 1;
      const uint64_t ticks = (timestamps[1] - timestamps[0]) & mask;
      gpu_seconds = static_cast<double>(ticks) * timestamp_period_ / 1e9;
      total_gpu_seconds += *gpu_seconds;
    }
    total_frames += options_.frames;
    total_presented += presented;
    total_seconds += elapsed.count();
    if (options_.loops != 1u) {
      print_times(std::format("loop {}: ", loop), options_.frames, presented, elapsed.count(),
                  gpu_seconds);
    }
  }
  print_times("", total_frames, total_presented, total_seconds,
              query_pool_ ? std::optional(total_gpu_seconds) : std::nullopt);
  return true;
}

}  // namespace

int main(int argc, char** argv) {
  const fxl::CommandLine command_line = fxl::CommandLineFromArgcArgv(argc, argv);
  if (command_line.HasOption("help")) {
    std::print("{}", kUsage);
    return EXIT_SUCCESS;
  }
  std::optional<Options> options = ParseOptions(command_line);
  if (!options) {
    return EXIT_FAILURE;
  }
  // Outlives |fractal|, so that validation errors reported by vkDestroyInstance are counted too.
  std::atomic<uint32_t> validation_errors{0};
  {
    Fractal fractal(*options, &validation_errors);
    if (!fractal.Init() || !fractal.Run()) {
      return EXIT_FAILURE;
    }
  }
  if (validation_errors > 0) {
    std::println(stderr, "vkfractal: {} validation errors", validation_errors.load());
    return EXIT_FAILURE;
  }
  return EXIT_SUCCESS;
}
