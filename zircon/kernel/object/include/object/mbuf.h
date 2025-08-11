// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_MBUF_H_
#define ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_MBUF_H_

#include <lib/user_copy/user_ptr.h>
#include <stdint.h>
#include <zircon/types.h>

#include <fbl/intrusive_double_list.h>
#include <vm/page.h>

// MBufChain is a container for storing a stream of bytes or a sequence of datagrams.
//
// It's designed to back sockets and channels.  Don't simultaneously store stream data and datagrams
// in a single instance.
class MBufChain {
 public:
  MBufChain() = default;
  ~MBufChain();

  // Writes |len| bytes of stream data from |src| and sets |written| to number of bytes written.
  //
  // Returns an error on failure, although some data may still have been written, in which case
  // |written| is set with the amount.
  zx_status_t WriteStream(user_in_ptr<const char> src, size_t len, size_t* written);

  // Writes a datagram of |len| bytes from |src| and sets |written| to number of bytes written.
  //
  // This operation is atomic in that either the entire datagram is written successfully or the
  // chain is unmodified.
  //
  // Writing a zero-length datagram is an error.
  //
  // Returns an error on failure, although some data may still have been written, in which case
  // |written| is set with the amount.
  zx_status_t WriteDatagram(user_in_ptr<const char> src, size_t len, size_t* written);

  // Reads up to |len| bytes from chain into |dst|.
  //
  // When |datagram| is false, the data in the chain is treated as a stream (no boundaries).
  //
  // When |datagram| is true, the data in the chain is treated as a sequence of datagrams and the
  // call will read at most one datagram. If |len| is too small to read a complete datagram, a
  // partial datagram is returned and its remaining bytes are discarded.
  //
  // The actual number of bytes read is returned in |actual|, and this can be non-zero even if the
  // read itself is an error.
  //
  // Returns an error on failure. If an error occurs while copying a datagram to |dst|, the
  // datagram is dropped.
  zx_status_t Read(user_out_ptr<char> dst, size_t len, bool datagram, size_t* actual);

  // Same as Read() but leaves the bytes in the chain instead of consuming them, even if
  // an error occurs.
  //
  // Returns an error on failure.
  zx_status_t Peek(user_out_ptr<char> dst, size_t len, bool datagram, size_t* actual) const;

  bool is_full() const { return size_ >= kSizeMax; }
  bool is_empty() const { return size_ == 0; }

  // Returns number of bytes stored in the chain.
  // When |datagram| is true, return only the number of bytes in the first
  // datagram, or 0 if in ZX_SOCKET_STREAM mode.
  size_t size(bool datagram = false) const {
    if (datagram && size_) {
      return buffers_.front().pkt_len_;
    }
    return size_;
  }

  // Returns the maximum number of bytes that can be stored in the chain.
  static size_t max_size() { return kSizeMax; }

  // Returns the number of bytes that will be placed in a given MBuf in the chain. Exposed only for
  // testing reasons.
  static size_t mbuf_payload_size() { return MBuf::kPayloadSize; }

 private:
  // An MBuf is a small fixed-size chainable memory buffer.
  struct MBuf : public fbl::DoublyLinkedListable<MBuf*> {
    explicit MBuf(vm_page_t* page);
    ~MBuf();

    // 16 for the linked list 16 for the explicit fields.
    static constexpr size_t kHeaderSize = (8 * 2) + (4 * 2) + 8;
    static constexpr size_t kPayloadSize = PAGE_SIZE - kHeaderSize;

    // Calculate the number of MBuf objects needed to store a payload of the given size.
    static constexpr size_t NumBuffersForPayload(size_t payload) {
      return 1 + ((payload - 1) / kPayloadSize);
    }

    // Returns number of bytes of free space in this MBuf.
    size_t rem() const { return kPayloadSize - len_; }

    // Length of the valid |data_| in this buffer. Writes can append more to |data_| and increment
    // this length.
    uint32_t len_ = 0u;

    // pkt_len_ is set to the total number of bytes in a packet
    // when a socket is in ZX_SOCKET_DATAGRAM mode. A pkt_len_ of
    // 0 means this mbuf is part of the body of a packet.
    //
    // Always 0 in ZX_SOCKET_STREAM mode.
    uint32_t pkt_len_ = 0u;

    // Back-pointer to the vm_page_t this MBuf was allocated from. Recording this is just an
    // optimization as it should always be the case that:
    // |Pmm::Node().PaddrToPage(physmap_to_paddr(this)) == page_|
    vm_page_t* const page_;

    // The data field is left uninitialized as the caller is going to immediately overwrite with the
    // payload, and is trusted to not access any uninitialized portions.
    char data_[kPayloadSize];
    // TODO: maybe union data_ with char* blocks for large messages
  };
  static_assert(sizeof(MBuf) == PAGE_SIZE);

  static constexpr size_t kSizeMax = 128 * MBuf::kPayloadSize;

  // The MBuf's are placed in a doubly linked list so that both the front and back of the list can
  // be manipulated and to allow for efficiently splicing lists into each other.
  using MBufList = fbl::DoublyLinkedList<MBuf*>;

  // Allocates exactly |num| buffers, or fails.
  static ktl::optional<MBufList> AllocMBufs(size_t num);

  // Takes ownership of and frees the provided buffers.
  static void FreeMBufs(MBufList&& bufs);

  // Helper method to provide common code for Read() and Peek().
  //
  // The static template function allows us to use the same code for both
  // const and non-const MBufChain objects. Const objects will peek,
  // non-const will read and consume.
  template <typename T>
  static zx_status_t ReadHelper(T* chain, user_out_ptr<char> dst, size_t len, bool datagram,
                                size_t* actual);

  // The active buffers that make up this chain. buffers_.front() + read_cursor_off_ is the read
  // cursor. buffers_.back() is the write cursor.
  MBufList buffers_;
  // The byte offset of the read cursor in next MBuf.
  uint32_t read_cursor_off_ = 0;
  size_t size_ = 0u;
};

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_MBUF_H_
