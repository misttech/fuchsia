// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/lib/escher/vk/shader_module_template.h"

#include "src/ui/lib/escher/shaders/util/spirv_file_util.h"
#include "src/ui/lib/escher/util/hasher.h"
#include "src/ui/lib/escher/util/trace_macros.h"

#if ESCHER_USE_RUNTIME_GLSL
#include <shaderc/shaderc.hpp>  // nogncheck
#endif

namespace escher {

namespace {

#if ESCHER_USE_RUNTIME_GLSL
shaderc_shader_kind ShaderStageToKind(ShaderStage stage) {
  switch (stage) {
    case ShaderStage::kVertex:
      return shaderc_glsl_vertex_shader;
    case ShaderStage::kTessellationControl:
      return shaderc_tess_control_shader;
    case ShaderStage::kTessellationEvaluation:
      return shaderc_tess_evaluation_shader;
    case ShaderStage::kGeometry:
      return shaderc_geometry_shader;
    case ShaderStage::kFragment:
      return shaderc_fragment_shader;
    case ShaderStage::kCompute:
      return shaderc_compute_shader;
    case ShaderStage::kEnumCount:
      FX_CHECK(false) << "Invalid ShaderStage: kEnumCount.";
      return shaderc_glsl_infer_from_source;
  }
}

class Includer : public shaderc::CompileOptions::IncluderInterface {
 public:
  explicit Includer(HackFilesystemWatcher* filesystem_watcher)
      : filesystem_watcher_(filesystem_watcher) {}

  ~Includer() override {
    FX_DCHECK(result_map_.empty())
        << "Included destroyed before all ResultRecords have been released.";
  }

  struct ResultRecord {
    shaderc_include_result result;
    HackFilePath file_path;
    HackFileContents file_contents;
    std::string error_msg;
  };

  // |shaderc::CompileOptions::IncluderInterface|.
  shaderc_include_result* GetInclude(const char* requested_source, shaderc_include_type type,
                                     const char* requesting_source, size_t include_depth) override {
    // Create a Result and stash it in result_map_, where it will stay until
    // released.  Keep a direct pointer to it, to use for the rest of this
    // method.
    ResultRecord* record = nullptr;
    {
      auto record_ptr = std::make_unique<ResultRecord>();
      record = record_ptr.get();
      result_map_[&record->result] = std::move(record_ptr);
    }
    shaderc_include_result* const result = &record->result;
    *result = {};

    record->file_path = requested_source;
    record->file_contents = filesystem_watcher_->ReadFile(record->file_path);

    if (record->file_contents.empty()) {
      record->error_msg = "ShaderModuleTemplate: file not found.";
      *result = {"", 0, record->error_msg.data(), record->error_msg.length(), nullptr};
    } else {
      *result = {record->file_path.data(), record->file_path.length(), record->file_contents.data(),
                 record->file_contents.length(), nullptr};
    }
    return result;
  }

  // |shaderc::CompileOptions::IncluderInterface|.
  void ReleaseInclude(shaderc_include_result* data) override { result_map_.erase(data); }

 private:
  HackFilesystemWatcher* const filesystem_watcher_;
  std::unordered_map<shaderc_include_result*, std::unique_ptr<ResultRecord>> result_map_;
};

#endif  // ESCHER_USE_RUNTIME_GLSL

}  // anonymous namespace

#if ESCHER_USE_RUNTIME_GLSL
ShaderModuleTemplate::ShaderModuleTemplate(vk::Device device, shaderc::Compiler* compiler,
                                           ShaderStage shader_stage, HackFilePath path,
                                           HackFilesystemPtr filesystem)
    : device_(device),
      compiler_(compiler),
      shader_stage_(shader_stage),
      path_(std::move(path)),
      filesystem_(std::move(filesystem)) {}
#else
ShaderModuleTemplate::ShaderModuleTemplate(vk::Device device, ShaderStage shader_stage,
                                           HackFilePath path, HackFilesystemPtr filesystem)
    : device_(device),
      shader_stage_(shader_stage),
      path_(std::move(path)),
      filesystem_(std::move(filesystem)) {}
#endif  // ESCHER_USE_RUNTIME_GLSL

ShaderModuleTemplate::~ShaderModuleTemplate() { FX_DCHECK(variants_.empty()); }

ShaderModulePtr ShaderModuleTemplate::GetShaderModuleVariant(const ShaderVariantArgs& args) {
  if (Variant* variant = variants_[args]) {
    return ShaderModulePtr(variant);
  }

  auto variant = new Variant(this, args);

  auto module_ptr = fxl::AdoptRef<ShaderModule>(variant);
  RegisterVariant(variant);
  variant->ScheduleCompilation();
  return module_ptr;
}

void ShaderModuleTemplate::RegisterVariant(Variant* variant) {
  FX_DCHECK(variants_.find(variant->args()) != variants_.end()) << "Variant already registered.";
  variants_[variant->args()] = variant;
}

void ShaderModuleTemplate::UnregisterVariant(Variant* variant) {
  auto it = variants_.find(variant->args());
  FX_DCHECK(it != variants_.end());
  FX_DCHECK(it->second == variant);
  variants_.erase(it);
}

void ShaderModuleTemplate::ScheduleVariantCompilation(fxl::WeakPtr<Variant> variant) {
  // TODO(https://fxbug.dev/42098032): Recompile immediately.  Eventually we might want to
  // momentarily defer this, so that we don't recompile multiple times if
  // several files are changing at once (as when all changed files are pushed to
  // the device in rapid succession).
  if (variant) {
    variant->UpdateModule();
  }
}

#if ESCHER_USE_RUNTIME_GLSL
bool ShaderModuleTemplate::CompileVariantToSpirv(const ShaderVariantArgs& args,
                                                 std::vector<uint32_t>* output) {
  FX_CHECK(output);
  Variant* variant = nullptr;
  if (variants_[args]) {
    variant = variants_[args];
  } else {
    variant = new Variant(this, args);
    RegisterVariant(variant);
  }

  // Variant only has the method |GenerateSpirV| when ESCHER_USE_RUNTIME_GLSL is true.
  return variant->GenerateSpirV(output);
}
#endif

ShaderModuleTemplate::Variant::Variant(ShaderModuleTemplate* tmplate, ShaderVariantArgs args)
    : ShaderModule(tmplate->device_, tmplate->shader_stage_),
      template_(tmplate),
      args_(std::move(args)),
      weak_factory_(this) {
  // Cannot do this as an initializer, because weak_factory_ must have already
  // been initialized, and weak_factory_ must be initialized last (at least if
  // we don't want to invite trouble).
  auto& fs = template_->filesystem_;
  filesystem_watcher_ =
      fs->RegisterWatcher([weak = weak_factory_.GetWeakPtr()](HackFilePath changed_path) {
        if (weak) {
          weak->template_->ScheduleVariantCompilation(weak);
        }
      });
}

ShaderModuleTemplate::Variant::~Variant() { template_->UnregisterVariant(this); }

void ShaderModuleTemplate::Variant::ScheduleCompilation() {
  template_->ScheduleVariantCompilation(weak_factory_.GetWeakPtr());
}

#if ESCHER_USE_RUNTIME_GLSL

// Generates the spirv for a compiled shader and returns it via the |output| parameter.
// Returns true if the compilation was successful and false otherwise.
bool ShaderModuleTemplate::Variant::GenerateSpirV(std::vector<uint32_t>* output) {
  TRACE_DURATION("gfx", "ShaderModuleTemplate::GenerateSpirV");

  FX_DCHECK(output);

  // Clear watcher paths; we'll gather new ones during compilation.
  filesystem_watcher_->ClearPaths();

  // Initialize compilation options.
  shaderc::CompileOptions options;
  for (auto& define : args_.definitions()) {
    options.AddMacroDefinition(define.first, define.second);
  }
  options.SetOptimizationLevel(shaderc_optimization_level_performance);
  options.SetIncluder(std::make_unique<Includer>(filesystem_watcher_.get()));
  // TODO(https://fxbug.dev/42098025): update this once we can rely upon Vulkan 1.1.
  options.SetTargetEnvironment(shaderc_target_env_vulkan, shaderc_env_version_vulkan_1_0);
  options.SetWarningsAsErrors();

  // Compile GLSL to SPIR-V, keeping track of paths as we go.
  auto main_file = filesystem_watcher_->ReadFile(template_->path_);

  auto result = template_->compiler_->CompileGlslToSpv(main_file.data(), main_file.size(),
                                                       ShaderStageToKind(shader_stage()),
                                                       template_->path_.c_str(), "main", options);

  auto status = result.GetCompilationStatus();
  if (status != shaderc_compilation_status_success) {
    FX_LOGS(ERROR) << "Shader compilation failed with status: " << status << ". "
                   << " Error message: " << result.GetErrorMessage();
    return false;
  }

  *output = {result.cbegin(), result.cend()};
  return true;
}

// Generates the spirv  for the shader and recreates the vk shader module with it.
void ShaderModuleTemplate::Variant::UpdateModule() {
  std::vector<uint32_t> spirv;
  bool result = GenerateSpirV(&spirv);
  FX_CHECK(result) << "Shader compilation failed!";
  RecreateModuleFromSpirvAndNotifyListeners(spirv);
}
#else

void ShaderModuleTemplate::Variant::UpdateModule() {
  std::vector<uint32_t> spirv;
  const std::optional<std::string>& base_path = template_->filesystem_->base_path();
  if (!base_path.has_value()) {
    // Mimic ReadSpirvFromDisk
    auto path = template_->path_ + std::to_string(args_.hash().val);
    std::replace(path.begin(), path.end(), '.', '_');
    std::replace(path.begin(), path.end(), '/', '_');
    path = "/data/shaders/" + path + ".spirv";
    auto contents = template_->filesystem_->ReadFile(path);
    FX_CHECK(!contents.empty()) << "module " << path << " is empty or non-existent.\n"
                                << "Update //src/ui/lib/escher/{BUILD.gn,test/gtest_escher.cc}";
    const size_t num = (contents.size() + 3) / sizeof(uint32_t);
    spirv.resize(num);
    memcpy(spirv.data(), contents.data(), num * sizeof(uint32_t));
  } else {
    bool result =
        shader_util::ReadSpirvFromDisk(args_, *base_path + "/shaders/", template_->path_, &spirv);
    FX_CHECK(result) << "Read SPIR-V file failed!";
  }
  RecreateModuleFromSpirvAndNotifyListeners(spirv);
}

#endif  // ESCHER_USE_RUNTIME_GLSL
}  // namespace escher
