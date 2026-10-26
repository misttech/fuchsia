// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_LIB_FILES_FILE_H_
#define SRC_LIB_FILES_FILE_H_

#include <stdint.h>

#include <string>
#include <string_view>
#include <vector>

namespace files {

// Writes the given data to the file at the given path. Returns true if the data
// was successfully written, otherwise returns false.
bool WriteFile(const std::string& path, std::string_view data);
bool WriteFile(const std::string& path, const char* data, ssize_t size);
bool WriteFileAt(int dirfd, const std::string& path, const char* data, ssize_t size);

// Atomically creates |filename| under |dirfd| with |data|.
//
// This is done by writing |data| to a temporary file named "<filename>.tmp" in
// |dirfd|, syncing the temporary file, renaming it to |filename|, and syncing
// |dirfd|. |dirfd| must be a valid open directory file descriptor (not
// AT_FDCWD), and |filename| must be a valid single-component filename (no '/').
// Returns true if the file was successfully written, otherwise returns false.
//
// Note that power failure after the temporary file is created, but before it is
// renamed, will result in "<filename>.tmp" existing in |dirfd|. A subsequent
// call for the same |filename| will overwrite and clean up the temporary file,
// but if this function is never called again for |filename|, the temporary file
// will remain. Callers must not invoke this concurrently with the same |dirfd|
// and |filename|.
bool WriteFileInTwoPhases(int dirfd, const std::string& filename, std::string_view data);

// Reads the contents of the file at the given path or file descriptor and
// stores the data in result. Returns true if the file was read successfully,
// otherwise returns false. If this function returns false, |result| will be
// the empty string.
bool ReadFileToString(const std::string& path, std::string* result);
bool ReadFileDescriptorToString(int fd, std::string* result);
bool ReadFileToStringAt(int dirfd, const std::string& path, std::string* result);

// Reads the contents of the file at the given path or file descriptor and stores the data in
// result. Returns true if the file was read successfully, otherwise returns
// false. If this function returns false, |result| will be the empty string.
bool ReadFileToVector(const std::string& path, std::vector<uint8_t>* result);
bool ReadFileDescriptorToVector(int fd, std::vector<uint8_t>* result);
bool ReadFileToVectorAt(int dirfd, const std::string& path, std::vector<uint8_t>* result);

// Returns whether the given path is a file.
bool IsFile(const std::string& path);
bool IsFileAt(int dirfd, const std::string& path);

// If the given path is a file, set size to the size of the file.
bool GetFileSize(const std::string& path, uint64_t* size);
bool GetFileSizeAt(int dirfd, const std::string& path, uint64_t* size);

}  // namespace files

#endif  // SRC_LIB_FILES_FILE_H_
