// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/zircon-internal/debug.h>
#include <stddef.h>
#include <stdint.h>
#include <string.h>
#include <zircon/assert.h>
#include <zircon/errors.h>
#include <zircon/status.h>
#include <zircon/syscalls.h>
#include <zircon/types.h>

#include <memory>
#include <utility>

#include <explicit-memory/bytes.h>
#include <fbl/algorithm.h>
#include <fbl/alloc_checker.h>
#include <fbl/macros.h>

// See note in //zircon/kernel/lib/crypto/boringssl/BUILD.gn
#define BORINGSSL_NO_CXX
#include <openssl/mem.h>

#include "src/security/lib/fcrypto/bytes.h"
#include "src/security/lib/fcrypto/secret.h"

#define ZXDEBUG 0

namespace crypto {

// Public methods

Bytes::Bytes() : buf_(nullptr), len_(0) {}
Bytes::~Bytes() { Clear(); }

Bytes::Bytes(Bytes&& o) noexcept : buf_(std::move(o.buf_)), len_(o.len_) {
  o.buf_ = nullptr;
  o.len_ = 0;
}

Bytes& Bytes::operator=(Bytes&& o) noexcept {
  buf_ = std::move(o.buf_);
  len_ = o.len_;
  o.buf_ = nullptr;
  o.len_ = 0;
  return *this;
}

zx_status_t Bytes::Randomize(size_t len) {
  zx_status_t status = Resize(len);
  if (status != ZX_OK) {
    return status;
  }
  zx_cprng_draw(buf_.get(), len);
  return ZX_OK;
}

zx_status_t Bytes::Resize(size_t size, uint8_t fill) {
  // Early exit if truncating to zero or if size is unchanged
  if (size == 0) {
    buf_.reset();
    len_ = 0;
    return ZX_OK;
  }
  if (size == len_) {
    return ZX_OK;
  }

  // Allocate new memory
  fbl::AllocChecker ac;
  std::unique_ptr<uint8_t[]> tmp(new (&ac) uint8_t[size]);
  if (!ac.check()) {
    xprintf("allocation failed: %zu bytes\n", size);
    return ZX_ERR_NO_MEMORY;
  }

  // Fill it with old data and pad as needed
  if (len_ == 0) {
    memset(tmp.get(), fill, size);
  } else if (len_ < size) {
    memcpy(tmp.get(), buf_.get(), len_);
    memset(tmp.get() + len_, fill, size - len_);
  } else {
    memcpy(tmp.get(), buf_.get(), size);
  }

  len_ = size;
  buf_ = std::move(tmp);
  return ZX_OK;
}

zx_status_t Bytes::Copy(const void* buf, size_t len, zx_off_t off) {
  zx_status_t rc;

  if (len == 0) {
    return ZX_OK;
  }
  if (!buf) {
    xprintf("null buffer\n");
    return ZX_ERR_INVALID_ARGS;
  }
  size_t size;
  if (add_overflow(off, len, &size)) {
    xprintf("overflow\n");
    return ZX_ERR_INVALID_ARGS;
  }

  if (len_ < size && (rc = Resize(size)) != ZX_OK) {
    return rc;
  }
  memcpy(buf_.get() + off, buf, len);

  return ZX_OK;
}

void Bytes::Clear() {
  if (buf_) {
    mandatory_memset(buf_.get(), 0, len_);
  }
  buf_.reset();
  len_ = 0;
}

const uint8_t& Bytes::operator[](zx_off_t off) const {
  ZX_ASSERT(off < len_);
  return buf_[off];
}

uint8_t& Bytes::operator[](zx_off_t off) {
  ZX_ASSERT(off < len_);
  return buf_[off];
}

bool Bytes::operator==(const Bytes& other) const {
  if (len_ != other.len_) {
    return false;
  } else if (len_ == 0) {
    return true;
  } else {
    ZX_DEBUG_ASSERT(buf_.get());
    ZX_DEBUG_ASSERT(other.buf_.get());
    return CRYPTO_memcmp(buf_.get(), other.buf_.get(), len_) == 0;
  }
}

}  // namespace crypto
