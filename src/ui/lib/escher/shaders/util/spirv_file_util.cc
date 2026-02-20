// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/fit/defer.h>
#include <string.h>

#ifdef __Fuchsia__
#include <fidl/fuchsia.io/cpp/fidl.h>
#endif

#include "src/ui/lib/escher/shaders/util/spirv_file_util.h"

namespace escher {
namespace {

// Given a path name for a variant shader and its args, generate a new hashed name for that
// shader's spirv code to be saved on disk.
// For example if the shader name was "main.vert" and the hash is "9731555" then the final
// hashed name will be "main_vert9731555.spirv".
std::string GenerateHashedSpirvName(const std::string& path, const ShaderVariantArgs& args) {
  uint64_t hash_value = args.hash().val;
  std::string result = path + std::to_string(hash_value);
  std::replace(result.begin(), result.end(), '.', '_');
  std::replace(result.begin(), result.end(), '/', '_');
  return result + ".spirv";
}
}  // namespace

namespace shader_util {
bool WriteSpirvToDisk(const std::vector<uint32_t>& spirv, const ShaderVariantArgs& args,
                      const std::string& base_path, const std::string& shader_name) {
  auto hash_name = GenerateHashedSpirvName(shader_name, args);
  auto full_path = base_path + hash_name;
  FILE* fp = fopen(full_path.c_str(), "wb");
  if (fp) {
    fwrite(spirv.data(), sizeof(uint32_t), spirv.size(), fp);
    fclose(fp);
    return true;
  } else {
    FX_LOGS(ERROR) << "Could not write file: " << full_path;
  }

  return false;
}

bool ReadSpirvFromDisk(const ShaderVariantArgs& args, const std::string& base_path,
                       const std::string& shader_name, std::vector<uint32_t>* out_spirv) {
  FX_DCHECK(out_spirv);
  auto hash_name = GenerateHashedSpirvName(shader_name, args);
  auto full_path = base_path + hash_name;
  FILE* fp = fopen(full_path.c_str(), "rb");
  if (fp) {
    auto close_file = fit::defer([fp] { fclose(fp); });
    std::size_t binary_size;
    fseek(fp, 0, SEEK_END);
    binary_size = ftell(fp);
    rewind(fp);

    // File was empty.
    if (binary_size == 0) {
      FX_LOGS(WARNING) << "Empty SPIR-V file: " << hash_name << " (base: " << base_path
                       << ", args: " << args << ")";
      return false;
    }

    size_t num_elements = binary_size / sizeof(uint32_t);
    out_spirv->resize(num_elements);
    size_t num_read = fread(out_spirv->data(), sizeof(uint32_t), num_elements, fp);

    if (num_read != num_elements) {
      FX_LOGS(WARNING) << "Read unexpected number of bytes from SPIR-V file: " << hash_name
                       << " (expected: " << num_elements << ", read: " << num_read
                       << ", args: " << args << ")";
      return false;
    }
    return true;
  }

  FX_LOGS(WARNING) << "Could not open SPIR-V file: " << hash_name << " (base: " << base_path
                   << ", args: " << args << ")";
  return false;
}

#ifdef __Fuchsia__

namespace {
// Implementation of fread(3) with a fuchsia.io/File.
size_t FRead(void* destv, size_t element_size, size_t num_elements,
             const fidl::SyncClient<fuchsia_io::File>& f, size_t file_size) {
  unsigned char* dest = static_cast<unsigned char*>(destv);
  const size_t len = element_size * num_elements;
  size_t nleft = len;
  size_t nlast_read = 0;

  for (; nleft > 0; nleft -= nlast_read, dest += nlast_read) {
    fuchsia_io::ReadableReadRequest read_request;
    read_request.count(nleft);
    const auto read_r = f->Read(read_request);
    if (read_r.is_error()) {
      return -1;
    }
    nlast_read = read_r->data().size();
    if (nlast_read == 0) {
      return (len - nleft) / element_size;
    }
    memcpy(dest, read_r->data().data(), read_r->data().size());
  }

  return num_elements;
}
}  // namespace

bool ReadSpirvFromDiskAtDir(const ShaderVariantArgs& args,
                            const fidl::SyncClient<fuchsia_io::Directory>& base_dir,
                            const std::string& shader_name, std::vector<uint32_t>* out_spirv) {
  FX_DCHECK(out_spirv);
  const auto hash_name = GenerateHashedSpirvName(shader_name, args);

  zx::channel client, server;
  zx::channel::create(0, &client, &server);
  {
    fuchsia_io::DirectoryOpenRequest open_request;
    open_request.path(hash_name);
    open_request.flags(fuchsia_io::Flags::kProtocolFile | fuchsia_io::kPermReadable);
    open_request.options(fuchsia_io::Options{});
    open_request.object(std::move(server));
    if (auto r = base_dir->Open(std::move(open_request)); r.is_error()) {
      FX_LOGS(WARNING) << "Could not open SPIR-V file: " << hash_name << ", args: " << args
                       << "): " << r.error_value();
      return false;
    }
  }

  size_t file_size = -1;
  fidl::SyncClient<fuchsia_io::File> file(fidl::ClientEnd<fuchsia_io::File>(std::move(client)));
  {
    fuchsia_io::FileSeekRequest seek_request;
    seek_request.offset(0);
    seek_request.origin(fuchsia_io::SeekOrigin::kEnd);
    const auto seek_r = file->Seek(seek_request);
    if (seek_r.is_error()) {
      FX_LOGS(WARNING) << "Could not seek to end of SPIR-V file: (name: " << hash_name
                       << ", args: " << args << "): " << seek_r.error_value();
      return false;
    }
    file_size = seek_r->offset_from_start();
  }
  {
    fuchsia_io::FileSeekRequest seek_request;
    seek_request.offset(0);
    seek_request.origin(fuchsia_io::SeekOrigin::kStart);
    const auto seek_r = file->Seek(seek_request);
    if (seek_r.is_error()) {
      FX_LOGS(WARNING) << "Could not seek to start of SPIR-V file: (name: " << hash_name
                       << ", args: " << args << "): " << seek_r.error_value();
      return false;
    }
  }

  // File was empty.
  if (file_size == 0) {
    FX_LOGS(WARNING) << "Empty SPIR-V file: " << hash_name << " (name: " << hash_name
                     << ", args: " << args << ")";
    return false;
  }

  const size_t num_elements = file_size / sizeof(uint32_t);
  out_spirv->resize(num_elements);
  const size_t num_read = FRead(out_spirv->data(), sizeof(uint32_t), num_elements, file, file_size);
  if (num_read != num_elements) {
    FX_LOGS(WARNING) << "Read unexpected number of bytes from SPIR-V file: " << hash_name
                     << " (expected: " << num_elements << ", read: " << num_read
                     << ", args: " << args << ")";
    return false;
  }
  return true;
}
#endif

bool SpirvExistsOnDisk(const ShaderVariantArgs& args, const std::string& abs_root,
                       const std::string& shader_name, const std::vector<uint32_t>& spirv) {
  bool should_write_spirv = true;
  std::vector<uint32_t> existing_spirv;
  if (shader_util::ReadSpirvFromDisk(args, abs_root, shader_name, &existing_spirv)) {
    if (existing_spirv == spirv) {
      should_write_spirv = false;
    }
  }
  return should_write_spirv;
}

}  // namespace shader_util
}  // namespace escher
