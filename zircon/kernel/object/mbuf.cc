// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "object/mbuf.h"

#include <lib/counters.h>
#include <lib/fit/defer.h>
#include <lib/user_copy/user_ptr.h>
#include <zircon/compiler.h>

#include <fbl/algorithm.h>
#include <fbl/alloc_checker.h>
#include <ktl/algorithm.h>
#include <ktl/type_traits.h>
#include <vm/physmap.h>
#include <vm/pmm.h>

#include <ktl/enforce.h>

#define LOCAL_TRACE 0

// Total amount of memory occupied by MBuf objects.
KCOUNTER(mbuf_total_bytes_count, "mbuf.total_bytes")

MBufChain::~MBufChain() { FreeMBufs(ktl::move(buffers_)); }

zx_status_t MBufChain::Read(user_out_ptr<char> dst, size_t len, bool datagram, size_t* actual) {
  return ReadHelper(this, dst, len, datagram, actual);
}

zx_status_t MBufChain::Peek(user_out_ptr<char> dst, size_t len, bool datagram,
                            size_t* actual) const {
  return ReadHelper(this, dst, len, datagram, actual);
}

template <class T>
zx_status_t MBufChain::ReadHelper(T* chain, user_out_ptr<char> dst, size_t len, bool datagram,
                                  size_t* actual) {
  if (chain->size_ == 0) {
    *actual = 0;
    return ZX_OK;
  }

  if (datagram && len > chain->buffers_.front().pkt_len_)
    len = chain->buffers_.front().pkt_len_;

  size_t pos = 0;
  auto iter = chain->buffers_.begin();
  // To handle peeking and non-peeking, cache our read cursor offset and set it when we're done.
  uint32_t read_off = chain->read_cursor_off_;
  auto update_cursor = fit::defer([&] {
    if constexpr (!ktl::is_const<T>::value) {
      chain->read_cursor_off_ = read_off;
    }
  });
  MBufList free_list;

  zx_status_t status = ZX_OK;
  while (pos < len && iter != chain->buffers_.end() && status == ZX_OK) {
    const char* src = iter->data_ + read_off;
    size_t copy_len = ktl::min(static_cast<size_t>(iter->len_ - read_off), len - pos);
    status = dst.byte_offset(pos).copy_array_to_user(src, copy_len);
    bool copy_succeeded = status == ZX_OK;
    if (likely(copy_succeeded)) {
      pos += copy_len;
    }

    if constexpr (ktl::is_const<T>::value) {
      read_off = 0;
      ++iter;
    } else {
      if (likely(copy_succeeded)) {
        read_off += static_cast<uint32_t>(copy_len);
        chain->size_ -= copy_len;
      }

      if (read_off == iter->len_ || datagram) {
        if (datagram) {
          chain->size_ -= (iter->len_ - read_off);
        }
        free_list.push_front(chain->buffers_.pop_front());
        iter = chain->buffers_.begin();
        // Start the next buffer at the beginning.
        read_off = 0;
      }
    }
  }

  // Drain any leftover mbufs in the datagram packet if we're consuming data, even
  // if we fail to read bytes.
  if constexpr (!ktl::is_const<T>::value) {
    if (datagram) {
      while (!chain->buffers_.is_empty() && chain->buffers_.front().pkt_len_ == 0) {
        MBuf* cur = chain->buffers_.pop_front();
        chain->size_ -= (cur->len_ - read_off);
        free_list.push_front(cur);
        read_off = 0;
      }
    }
  }
  if constexpr (!ktl::is_const_v<T>) {
    if (!free_list.is_empty()) {
      chain->FreeMBufs(ktl::move(free_list));
    }
  }

  // Record the fact that some data might have been read, even if the overall operation is
  // considered a failure.
  *actual = pos;
  return status;
}

zx_status_t MBufChain::WriteDatagram(user_in_ptr<const char> src, size_t len, size_t* written) {
  if (len == 0) {
    *written = 0;
    return ZX_ERR_INVALID_ARGS;
  }
  if (len > kSizeMax) {
    *written = 0;
    return ZX_ERR_OUT_OF_RANGE;
  }
  if (len + size_ > kSizeMax) {
    *written = 0;
    return ZX_ERR_SHOULD_WAIT;
  }

  ktl::optional<MBufList> alloc_bufs = AllocMBufs(MBuf::NumBuffersForPayload(len));
  if (!alloc_bufs.has_value()) {
    *written = 0;
    return ZX_ERR_SHOULD_WAIT;
  }
  MBufList& bufs = *alloc_bufs;

  size_t pos = 0;
  for (auto& buf : bufs) {
    size_t copy_len = ktl::min(MBuf::kPayloadSize, len - pos);
    if (src.byte_offset(pos).copy_array_from_user(buf.data_, copy_len) != ZX_OK) {
      FreeMBufs(ktl::move(bufs));
      *written = 0;
      return ZX_ERR_INVALID_ARGS;  // Bad user buffer.
    }
    pos += copy_len;
    buf.len_ += static_cast<uint32_t>(copy_len);
  }

  bufs.front().pkt_len_ = static_cast<uint32_t>(len);

  // Successfully built the packet mbufs. Put it on the socket.
  buffers_.splice(buffers_.end(), bufs);

  *written = len;
  size_ += len;
  return ZX_OK;
}

zx_status_t MBufChain::WriteStream(user_in_ptr<const char> src, size_t len, size_t* written) {
  // Cap len by the max we are allowed to write.
  len = ktl::min(kSizeMax - size_, len);

  size_t pos = 0;

  auto write_buffer = [&](MBuf& buf) -> zx_status_t {
    char* dst = buf.data_ + buf.len_;
    size_t copy_len = ktl::min(buf.rem(), len - pos);

    zx_status_t status = src.byte_offset(pos).copy_array_from_user(dst, copy_len);
    if (status != ZX_OK) {
      // TODO(https://fxbug.dev/42109418): Note that although we set |written| for the benefit of
      // the socket dispatcher updating signals, ultimately we're not indicating to the caller that
      // data added so far in previous copies was written successfully. This means the caller may
      // try to re-send the same data again, leading to duplicate data. Consider changing the socket
      // dispatcher to forward this partial write information to the caller, or consider not
      // committing any of the new data until we can ensure success, or consider putting the socket
      // in a state where it can't succeed a subsequent write.
      *written = pos;
      return status;
    }

    pos += copy_len;
    buf.len_ += static_cast<uint32_t>(copy_len);
    size_ += copy_len;
    return ZX_OK;
  };

  // If there's space available in the write buffer, go there first.
  if (!buffers_.is_empty() && buffers_.back().rem() > 0) {
    zx_status_t status = write_buffer(buffers_.back());
    if (status != ZX_OK) {
      return status;
    }
  }

  // See if we need to allocate additional buffers.
  if (pos != len) {
    if (ktl::optional<MBufList> bufs = AllocMBufs(MBuf::NumBuffersForPayload(len - pos))) {
      while (!bufs->is_empty()) {
        zx_status_t status = write_buffer(bufs->front());
        if (status != ZX_OK) {
          FreeMBufs(ktl::move(*bufs));
          return status;
        }
        buffers_.push_back(bufs->pop_front());
      }
    }
  }

  *written = pos;
  if (pos == 0) {
    return ZX_ERR_SHOULD_WAIT;
  }
  return ZX_OK;
}

ktl::optional<fbl::DoublyLinkedList<MBufChain::MBuf*>> MBufChain::AllocMBufs(size_t num) {
  list_node_t pages = LIST_INITIAL_VALUE(pages);
  zx_status_t status = Pmm::Node().AllocPages(num, 0, &pages);
  if (status != ZX_OK) {
    return ktl::nullopt;
  }
  MBufList ret;
  while (!list_is_empty(&pages)) {
    vm_page_t* page = list_remove_head_type(&pages, vm_page_t, queue_node);
    MBuf* buf = reinterpret_cast<MBuf*>(paddr_to_physmap(page->paddr()));
    new (buf) MBuf(page);
    ret.push_front(buf);
  }
  return ret;
}

void MBufChain::FreeMBufs(MBufList&& bufs) {
  list_node_t pages = LIST_INITIAL_VALUE(pages);

  while (!bufs.is_empty()) {
    MBuf* buf = bufs.pop_front();
    vm_page_t* page = buf->page_;
    buf->~MBuf();
    list_add_head(&pages, &page->queue_node);
  }
  Pmm::Node().FreeList(&pages);
}

MBufChain::MBuf::MBuf(vm_page_t* page) : page_(page) {
  page->set_state(vm_page_state::IPC);
  kcounter_add(mbuf_total_bytes_count, sizeof(MBufChain::MBuf));
}

MBufChain::MBuf::~MBuf() {
  kcounter_add(mbuf_total_bytes_count, -static_cast<int64_t>(sizeof(MBufChain::MBuf)));
}
