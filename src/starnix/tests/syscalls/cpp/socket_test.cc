// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <arpa/inet.h>
#include <fcntl.h>
#include <net/ethernet.h>
#include <netinet/icmp6.h>
#include <netinet/in.h>
#include <netinet/ip_icmp.h>
#include <poll.h>
#include <stdio.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/socket.h>
#include <sys/types.h>
#include <sys/uio.h>
#include <sys/un.h>
#include <unistd.h>

#include <fstream>
#include <thread>

#include <asm-generic/socket.h>
#include <fbl/unaligned.h>
#include <fbl/unique_fd.h>
#include <gtest/gtest.h>
#include <linux/bpf.h>
#include <linux/capability.h>
#include <linux/filter.h>
#include <linux/if_ether.h>
#include <linux/if_packet.h>
#include <linux/ipv6.h>
#include <linux/netfilter/x_tables.h>
#include <linux/rtnetlink.h>

#include "fault_test.h"
#include "src/lib/fxl/strings/string_printf.h"
#include "src/starnix/tests/syscalls/cpp/capabilities_helper.h"
#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"
#include "test_helper.h"

#if !defined(__NR_memfd_create)
#if defined(__x86_64__)
#define __NR_memfd_create 319
#elif defined(__i386__)
#define __NR_memfd_create 356
#elif defined(__aarch64__)
#define __NR_memfd_create 279
#elif defined(__arm__)
#define __NR_memfd_create 385
#endif
#endif  // !defined(__NR_memfd_create)

// `IPT_SO_GET_REVISION_TARGET` is defined in `linux/netfilter_ipv4/ip_tables.h`,
// but that header is not usable with `clang` due to broken pointer math in
// `ipt_get_target()`. Bionic's version of that header is usable.
#if defined(__BIONIC__)
#include <linux/netfilter_ipv4/ip_tables.h>
#else
#define IPT_BASE_CTL 64
#define IPT_SO_GET_REVISION_TARGET (IPT_BASE_CTL + 3)
#endif

TEST(UnixSocket, ReadAfterClose) {
  int fds[2];

  ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_STREAM, 0, fds));
  ASSERT_EQ(1, write(fds[0], "0", 1));
  ASSERT_EQ(0, close(fds[0]));
  char buf[1];
  ASSERT_EQ(1, read(fds[1], buf, 1));
  ASSERT_EQ('0', buf[0]);
  ASSERT_EQ(0, read(fds[1], buf, 1));
}

TEST(UnixSocket, ReadAfterReadShutdown) {
  int fds[2];

  ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_STREAM, 0, fds));
  ASSERT_EQ(1, write(fds[0], "0", 1));
  ASSERT_EQ(0, shutdown(fds[1], SHUT_RD));
  char buf[1];
  ASSERT_EQ(1, read(fds[1], buf, 1));
  ASSERT_EQ('0', buf[0]);
  ASSERT_EQ(0, read(fds[1], buf, 1));
}

TEST(UnixSocket, HupEvent) {
  int fds[2];

  ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_STREAM, 0, fds));

  int epfd = epoll_create1(0);
  ASSERT_LT(-1, epfd);
  epoll_event ev = {EPOLLIN, {.u64 = 42}};
  ASSERT_EQ(0, epoll_ctl(epfd, EPOLL_CTL_ADD, fds[0], &ev));

  epoll_event outev = {0, {.u64 = 0}};

  int no_ready = epoll_wait(epfd, &outev, 1, 0);
  ASSERT_EQ(0, no_ready);

  close(fds[1]);

  no_ready = epoll_wait(epfd, &outev, 1, 0);
  ASSERT_EQ(1, no_ready);
  ASSERT_EQ(EPOLLIN | EPOLLHUP, outev.events);
  ASSERT_EQ(42ul, fbl::UnalignedLoad<uint64_t>(&outev.data.u64));

  close(fds[0]);
  close(epfd);
}

TEST(UnixSocket, ReadConnReset) {
  int fds[2];
  ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_STREAM, 0, fds));

  ASSERT_EQ(1, write(fds[1], "A", 1));
  ASSERT_EQ(1, write(fds[0], "B", 1));
  ASSERT_EQ(0, close(fds[0]));
  char buf[1];
  ASSERT_EQ(1, read(fds[1], buf, 1));
  ASSERT_EQ('B', buf[0]);
  ASSERT_EQ(-1, read(fds[1], buf, 1));
  EXPECT_EQ(errno, ECONNRESET);
}

TEST(UnixSocket, ReadConnResetEmptyBuffer) {
  int fds[2];
  ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_STREAM, 0, fds));

  ASSERT_EQ(1, write(fds[1], "A", 1));
  ASSERT_EQ(0, close(fds[0]));
  char buf[1];
  ASSERT_EQ(0, read(fds[1], buf, 0));
}

TEST(UnixSocket, ReadConnResetAndShutdown) {
  int fds[2];
  ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_STREAM, 0, fds));

  ASSERT_EQ(1, write(fds[1], "A", 1));
  ASSERT_EQ(1, write(fds[0], "B", 1));
  ASSERT_EQ(0, close(fds[0]));
  ASSERT_EQ(0, shutdown(fds[1], SHUT_RD));

  char buf[1];
  ASSERT_EQ(1, read(fds[1], buf, 1));
  ASSERT_EQ('B', buf[0]);
  ASSERT_EQ(-1, read(fds[1], buf, 1));
  EXPECT_EQ(errno, ECONNRESET);
}

struct read_info_spec {
  unsigned char* mem;
  size_t length;
  size_t bytes_read;
  int fd;
};

void* reader(void* arg) {
  read_info_spec* read_info = reinterpret_cast<read_info_spec*>(arg);
  while (read_info->bytes_read < read_info->length) {
    size_t to_read = read_info->length - read_info->bytes_read;
    fflush(stdout);
    ssize_t bytes_read = read(read_info->fd, read_info->mem + read_info->bytes_read, to_read);
    EXPECT_LT(-1, bytes_read) << strerror(errno);
    if (bytes_read < 0) {
      return nullptr;
    }
    read_info->bytes_read += bytes_read;
  }
  return nullptr;
}

TEST(UnixSocket, BigWrite) {
  const size_t write_size = 300000;
  unsigned char* send_mem = new unsigned char[write_size];
  ASSERT_TRUE(send_mem != nullptr);

  for (size_t i = 0; i < write_size; i++) {
    send_mem[i] = 0xff & random();
  }

  int fds[2];
  ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_STREAM, 0, fds)) << strerror(errno);

  read_info_spec read_info;
  read_info.mem = new unsigned char[write_size];
  bzero(read_info.mem, sizeof(unsigned char) * write_size);
  ASSERT_TRUE(read_info.mem != nullptr);
  read_info.length = write_size;
  read_info.fd = fds[1];
  read_info.bytes_read = 0;

  pthread_t read_thread;
  ASSERT_EQ(0, pthread_create(&read_thread, nullptr, reader, &read_info));
  size_t write_count = 0;
  while (write_count < write_size) {
    size_t to_send = write_size - write_count;
    ssize_t bytes_read = write(fds[0], send_mem + write_count, to_send);
    ASSERT_LT(-1, bytes_read) << strerror(errno);
    write_count += bytes_read;
  }

  ASSERT_EQ(0, pthread_join(read_thread, nullptr)) << strerror(errno);

  close(fds[0]);
  close(fds[1]);

  ASSERT_EQ(write_count, read_info.bytes_read);
  ASSERT_EQ(0, memcmp(send_mem, read_info.mem, sizeof(unsigned char) * write_size));

  delete[] send_mem;
  delete[] read_info.mem;
}

TEST(UnixSocket, SeqPacket) {
  char* tmp = getenv("TEST_TMPDIR");
  auto socket_path =
      tmp == nullptr ? "/tmp/socktest_seqpacket" : std::string(tmp) + "/socktest_seqpacket";
  struct sockaddr_un sun;
  sun.sun_family = AF_UNIX;
  strcpy(sun.sun_path, socket_path.c_str());
  struct sockaddr* addr = reinterpret_cast<struct sockaddr*>(&sun);

  auto server = socket(AF_UNIX, SOCK_SEQPACKET, 0);
  ASSERT_GE(server, 0);
  ASSERT_EQ(bind(server, addr, sizeof(sun)), 0);
  ASSERT_EQ(listen(server, 1), 0);
  int pipes[2];
  ASSERT_EQ(pipe(pipes), 0);
  pid_t pid = fork();
  if (pid == 0) {
    // Child
    auto client = socket(AF_UNIX, SOCK_SEQPACKET, 0);
    if (client < 0) {
      exit(1);
    }
    if (connect(client, addr, sizeof(sun)) < 0) {
      exit(1);
    }
    const char* message = "hello";
    if (send(client, message, strlen(message), 0) < 0) {
      exit(1);
    }
    char data;
    read(pipes[0], &data, 1);
    close(pipes[0]);
    close(client);
    exit(0);
  }

  // Parent
  auto accepted_fd = accept(server, nullptr, nullptr);
  ASSERT_GE(accepted_fd, 0);
  char byte = 0;
  write(accepted_fd, &byte, 1);

  char buf[1024];
  ssize_t bytes_read = recv(accepted_fd, buf, sizeof(buf), 0);
  ASSERT_EQ(bytes_read, 5);
  ASSERT_EQ(memcmp(buf, "hello", 5), 0);

  char buffer[256];
  struct iovec iov[1];
  iov[0].iov_base = buffer;
  iov[0].iov_len = sizeof(buffer);

  struct msghdr msg;
  memset(&msg, 0, sizeof(msg));
  msg.msg_iov = iov;
  msg.msg_iovlen = 1;
  ASSERT_EQ(write(pipes[1], buffer, 1), 1);
  int status;
  ASSERT_EQ(waitpid(pid, &status, 0), pid);
  ASSERT_TRUE(WIFEXITED(status) && WEXITSTATUS(status) == 0);
  // recvmsg should return ECONNRESET when the peer closes the connection
  // followed by 0 on the subsequent read.
  ssize_t n = recvmsg(accepted_fd, &msg, MSG_CMSG_CLOEXEC);
  ASSERT_EQ(errno, ECONNRESET);
  ASSERT_EQ(n, -1);
  n = recvmsg(accepted_fd, &msg, MSG_CMSG_CLOEXEC);
  ASSERT_EQ(n, 0);

  ASSERT_EQ(unlink(socket_path.c_str()), 0);
  ASSERT_EQ(close(accepted_fd), 0);
  ASSERT_EQ(close(server), 0);
  close(pipes[1]);
}

TEST(UnixSocket, ConnectZeroBacklog) {
  char* tmp = getenv("TEST_TMPDIR");
  auto socket_path =
      tmp == nullptr ? "/tmp/socktest_connect" : std::string(tmp) + "/socktest_connect";
  struct sockaddr_un sun;
  sun.sun_family = AF_UNIX;
  strcpy(sun.sun_path, socket_path.c_str());
  struct sockaddr* addr = reinterpret_cast<struct sockaddr*>(&sun);

  auto server = socket(AF_UNIX, SOCK_STREAM, 0);
  ASSERT_EQ(bind(server, addr, sizeof(sun)), 0);
  ASSERT_EQ(listen(server, 0), 0);

  auto client = socket(AF_UNIX, SOCK_STREAM, 0);
  ASSERT_GT(client, -1);
  ASSERT_EQ(connect(client, addr, sizeof(sun)), 0);

  ASSERT_EQ(unlink(socket_path.c_str()), 0);
  ASSERT_EQ(close(client), 0);
  ASSERT_EQ(close(server), 0);
}

TEST(UnixSocket, ConnectLargeSize) {
  struct sockaddr_un sun;
  sun.sun_family = AF_UNIX;
  strcpy(sun.sun_path, "/bogus/path/value");
  struct sockaddr* addr = reinterpret_cast<struct sockaddr*>(&sun);

  auto client = socket(AF_UNIX, SOCK_STREAM, 0);
  ASSERT_GT(client, -1);
  ASSERT_EQ(connect(client, addr, sizeof(struct sockaddr_un) + 1), -1);
  EXPECT_EQ(errno, EINVAL);
}

TEST(InetSocket, ConnectLargeSize) {
  struct sockaddr_in in;
  in.sin_family = AF_INET;
  struct sockaddr* addr = reinterpret_cast<struct sockaddr*>(&in);

  auto client = socket(AF_INET, SOCK_STREAM, 0);
  ASSERT_GT(client, -1);
  ASSERT_EQ(connect(client, addr, sizeof(struct sockaddr_storage) + 1), -1);
  EXPECT_EQ(errno, EINVAL);
}

class UnixSocketTest : public testing::Test {
  // SetUp() - make socket
 protected:
  void SetUp() override {
    char* tmp = getenv("TEST_TMPDIR");
    socket_path_ = tmp == nullptr ? "/tmp/socktest" : std::string(tmp) + "/socktest";
    struct sockaddr_un sun;
    sun.sun_family = AF_UNIX;
    strcpy(sun.sun_path, socket_path_.c_str());
    struct sockaddr* addr = reinterpret_cast<struct sockaddr*>(&sun);

    server_ = socket(AF_UNIX, SOCK_STREAM, 0);
    ASSERT_GT(server_, -1);
    ASSERT_EQ(bind(server_, addr, sizeof(sun)), 0);
    ASSERT_EQ(listen(server_, 1), 0);

    client_ = socket(AF_UNIX, SOCK_STREAM, 0);
    ASSERT_GT(client_, -1);
    ASSERT_EQ(connect(client_, addr, sizeof(sun)), 0);
  }

  void TearDown() override {
    ASSERT_EQ(unlink(socket_path_.c_str()), 0);
    ASSERT_EQ(close(client_), 0);
    ASSERT_EQ(close(server_), 0);
  }

  int client() const { return client_; }

 private:
  int client_ = 0;
  int server_ = 0;
  std::string socket_path_;
};

TEST_F(UnixSocketTest, ImmediatePeercredCheck) {
  struct ucred cred;
  socklen_t cred_size = sizeof(cred);
  ASSERT_EQ(getsockopt(client(), SOL_SOCKET, SO_PEERCRED, &cred, &cred_size), 0);
  ASSERT_NE(cred.pid, 0);
  ASSERT_EQ(cred.uid, getuid());
  ASSERT_EQ(cred.gid, getgid());
}

TEST(UnixSocket, SendZeroFds) {
  int fds[2];
  ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_STREAM, 0, fds));

  char data[] = "a";
  struct iovec iov[] = {{
      .iov_base = data,
      .iov_len = 1,
  }};
  char buf[CMSG_SPACE(0)];
  struct msghdr msg = {
      .msg_iov = iov,
      .msg_iovlen = 1,
      .msg_control = buf,
      .msg_controllen = sizeof(buf),
  };
  *CMSG_FIRSTHDR(&msg) = (struct cmsghdr){
      .cmsg_len = CMSG_LEN(0),
      .cmsg_level = SOL_SOCKET,
      .cmsg_type = SCM_RIGHTS,
  };
  ASSERT_EQ(sendmsg(fds[0], &msg, 0), 1);

  memset(data, 0, sizeof(data));
  memset(buf, 0, sizeof(buf));
  ASSERT_EQ(recvmsg(fds[1], &msg, 0), 1);
  EXPECT_EQ(data[0], 'a');
  EXPECT_EQ(msg.msg_controllen, 0u);
  EXPECT_EQ(msg.msg_flags, 0);
}

#if defined(__NR_memfd_create)
TEST(UnixSocket, SendMemFd) {
  int fds[2];
  ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_STREAM, 0, fds));

  int memfd = static_cast<int>(syscall(__NR_memfd_create, "test_memfd", 0));

  char data[] = "";
  struct iovec iov[] = {{
      .iov_base = data,
      .iov_len = 1,
  }};
  char buf[CMSG_SPACE(sizeof(int))];
  struct msghdr msg = {
      .msg_iov = iov,
      .msg_iovlen = 1,
      .msg_control = buf,
      .msg_controllen = sizeof(buf),
  };
  struct cmsghdr* cmsg = CMSG_FIRSTHDR(&msg);
  *cmsg = (struct cmsghdr){
      .cmsg_len = CMSG_LEN(sizeof(int)),
      .cmsg_level = SOL_SOCKET,
      .cmsg_type = SCM_RIGHTS,
  };
  memmove(CMSG_DATA(cmsg), &memfd, sizeof(int));
  msg.msg_controllen = cmsg->cmsg_len;

  ASSERT_EQ(sendmsg(fds[0], &msg, 0), 1);

  memset(data, 0, sizeof(data));
  memset(buf, 0, sizeof(buf));
  ASSERT_EQ(recvmsg(fds[1], &msg, 0), 1);
  EXPECT_EQ(data[0], '\0');
  EXPECT_GT(msg.msg_controllen, 0u);
  EXPECT_EQ(msg.msg_flags, 0);
}
#endif  // defined(__NR_memfd_create)

// This test verifies that we can concurrently attempt to create the same type of socket from
// multiple threads.
TEST(Socket, ConcurrentCreate) {
  std::atomic_int barrier{0};
  std::atomic_int child_ready{0};
  auto child = std::thread([&] {
    child_ready.store(1);
    while (barrier.load() == 0) {
    }
    fbl::unique_fd fd;
    EXPECT_TRUE(fd = fbl::unique_fd(socket(AF_INET, SOCK_STREAM, 0))) << strerror(errno);
  });
  while (child_ready.load() == 0) {
  }
  barrier.store(1);

  fbl::unique_fd fd;
  EXPECT_TRUE(fd = fbl::unique_fd(socket(AF_INET, SOCK_STREAM, 0))) << strerror(errno);
  child.join();
}

TEST(Socket, SocketCookie) {
  fbl::unique_fd fd;

  // Create a socket and get its cookie.
  EXPECT_TRUE(fd = fbl::unique_fd(socket(AF_INET, SOCK_STREAM, 0))) << strerror(errno);
  uint64_t cookie1 = 0;
  socklen_t optlen = sizeof(cookie1);
  ASSERT_EQ(getsockopt(fd.get(), SOL_SOCKET, SO_COOKIE, &cookie1, &optlen), 0) << strerror(errno);
  EXPECT_EQ(optlen, sizeof(cookie1));

  // Create another socket verify that it has a different cookie value.
  EXPECT_TRUE(fd = fbl::unique_fd(socket(AF_INET, SOCK_DGRAM, 0))) << strerror(errno);
  uint64_t cookie2 = 0;
  ASSERT_EQ(getsockopt(fd.get(), SOL_SOCKET, SO_COOKIE, &cookie2, &optlen), 0) << strerror(errno);
  EXPECT_EQ(optlen, sizeof(cookie2));

  EXPECT_NE(cookie1, cookie2);
}

class SocketFault : public FaultTest, public testing::WithParamInterface<std::pair<int, int>> {
 protected:
  void SetUp() override {
    const auto [type, protocol] = GetParam();

    // TODO(https://fxbug.dev/317285180) don't skip on baseline
    if (type == SOCK_DGRAM && protocol == IPPROTO_ICMP && getuid() != 0) {
      GTEST_SKIP() << "Ping sockets require root.";
    }

    sockaddr_in addr = {
        .sin_family = AF_INET,
        .sin_addr = {htonl(INADDR_LOOPBACK)},
    };
    socklen_t addrlen = sizeof(addr);
    ASSERT_TRUE(recv_fd_ = fbl::unique_fd(socket(AF_INET, type, protocol))) << strerror(errno);
    ASSERT_EQ(bind(recv_fd_.get(), reinterpret_cast<sockaddr*>(&addr), addrlen), 0)
        << strerror(errno);
    ASSERT_EQ(getsockname(recv_fd_.get(), reinterpret_cast<sockaddr*>(&addr), &addrlen), 0)
        << strerror(errno);
    ASSERT_EQ(addrlen, sizeof(addr));
    if (type == SOCK_STREAM) {
      ASSERT_EQ(listen(recv_fd_.get(), 0), 0) << strerror(errno);
      listen_fd_ = std::move(recv_fd_);
    }

    ASSERT_TRUE(send_fd_ = fbl::unique_fd(socket(AF_INET, type, protocol))) << strerror(errno);
    ASSERT_EQ(connect(send_fd_.get(), reinterpret_cast<const sockaddr*>(&addr), sizeof(addr)), 0)
        << strerror(errno);

    if (type == SOCK_STREAM) {
      ASSERT_TRUE(recv_fd_ = fbl::unique_fd(accept(listen_fd_.get(), nullptr, nullptr)))
          << strerror(errno);
    } else if (protocol == IPPROTO_ICMP) {
      // ICMP sockets only get the packet on the sending socket since sockets do not
      // receive ICMP requests, only replies. Note that the netstack internally
      // responds to ICMP requests without any user-application needing to handle
      // requests.
      ASSERT_TRUE(recv_fd_ = fbl::unique_fd(dup(send_fd_.get()))) << strerror(errno);
    }
  }

  void TearDown() override {
    send_fd_.reset();
    recv_fd_.reset();
    listen_fd_.reset();
  }

  void SetRecvFdNonBlocking() {
    int flags = fcntl(recv_fd_.get(), F_GETFL, 0);
    ASSERT_GE(flags, 0) << strerror(errno);
    ASSERT_EQ(fcntl(recv_fd_.get(), F_SETFL, flags | O_NONBLOCK), 0) << strerror(errno);
  }

  fbl::unique_fd recv_fd_;
  fbl::unique_fd listen_fd_;
  fbl::unique_fd send_fd_;
};

// Test sending a packet from invalid memory.
TEST_P(SocketFault, Write) {
  EXPECT_EQ(write(send_fd_.get(), faulting_ptr_, kFaultingSize_), -1);
  EXPECT_EQ(errno, EFAULT);
}

// Test receiving a packet to invalid memory.
TEST_P(SocketFault, Read) {
  // First send a valid message that we can read.
  //
  // We send an ICMP message since this test is generic over UDP/TCP/ICMP.
  // UDP/TCP do not care about the shape of the payload but ICMP does so we just
  // use an ICMP compatible payload for simplicity.
  constexpr icmphdr kSendIcmp = {
      .type = ICMP_ECHO,
  };
  ASSERT_EQ(write(send_fd_.get(), &kSendIcmp, sizeof(kSendIcmp)),
            static_cast<ssize_t>(sizeof(kSendIcmp)));

  pollfd p = {
      .fd = recv_fd_.get(),
      .events = POLLIN,
  };
  ASSERT_EQ(poll(&p, 1, -1), 1);
  ASSERT_EQ(p.revents, POLLIN);

  static_assert(kFaultingSize_ >= sizeof(kSendIcmp));
  EXPECT_EQ(read(recv_fd_.get(), faulting_ptr_, sizeof(kSendIcmp)), -1);
  EXPECT_EQ(errno, EFAULT);
}

TEST_P(SocketFault, ReadV) {
  // First send a valid message that we can read.
  //
  // We send an ICMP message since this test is generic over UDP/TCP/ICMP.
  // UDP/TCP do not care about the shape of the payload but ICMP does so we just
  // use an ICMP compatible payload for simplicity.
  constexpr icmphdr kSendIcmp = {
      .type = ICMP_ECHO,
  };
  ASSERT_EQ(write(send_fd_.get(), &kSendIcmp, sizeof(kSendIcmp)),
            static_cast<ssize_t>(sizeof(kSendIcmp)));

  pollfd p = {
      .fd = recv_fd_.get(),
      .events = POLLIN,
  };
  ASSERT_EQ(poll(&p, 1, -1), 1);
  ASSERT_EQ(p.revents, POLLIN);

  char base0[1];
  char base2[sizeof(kSendIcmp) - 1];
  iovec iov[] = {
      {
          .iov_base = base0,
          .iov_len = sizeof(base0),
      },
      {
          .iov_base = faulting_ptr_,
          .iov_len = sizeof(kFaultingSize_),
      },
      {
          .iov_base = base2,
          .iov_len = sizeof(base2),
      },
  };

  // Read once with iov holding the invalid pointer.
  ASSERT_EQ(readv(recv_fd_.get(), iov, std::size(iov)), -1);
  EXPECT_EQ(errno, EFAULT);

  // Read again after clearing the invalid buffer. This read will fail on UDP/ICMP
  // sockets since they deque the message before checking the validity of buffers
  // but TCP sockets will not remove bytes from the unread bytes held by the kernel
  // if any buffer faults. Note that what UDP/ICMP does is ~acceptable since they are
  // not meant to be a reliable protocol and the behaviour for TCP also makes sense
  // because when the socket returns EFAULT, there is no way to know how many
  // bytes the kernel write into our buffers. Since the kernel has no way to tell us
  // how many bytes were read when a fault occurred, it has no other option than to
  // keep the bytes before the fault to prevent userspace from dropping part of a
  // byte stream.
  ASSERT_NO_FATAL_FAILURE(SetRecvFdNonBlocking());
  const auto [type, protocol] = GetParam();
  iov[1] = iovec{};
  if (type == SOCK_STREAM) {
    ASSERT_EQ(readv(recv_fd_.get(), iov, std::size(iov)), static_cast<ssize_t>(sizeof(kSendIcmp)));
  } else {
    ASSERT_EQ(readv(recv_fd_.get(), iov, std::size(iov)), -1);
    EXPECT_EQ(errno, EAGAIN);
  }
}

TEST_P(SocketFault, WriteV) {
  icmphdr kSendIcmp = {
      .type = ICMP_ECHO,
  };
  constexpr size_t kBase0Size = 1;
  iovec iov[] = {
      {
          .iov_base = &kSendIcmp,
          .iov_len = kBase0Size,
      },
      {
          .iov_base = faulting_ptr_,
          .iov_len = sizeof(kFaultingSize_),
      },
      {
          .iov_base = reinterpret_cast<char*>(&kSendIcmp) + kBase0Size,
          .iov_len = sizeof(kSendIcmp) - kBase0Size,
      },
  };
  ASSERT_EQ(writev(send_fd_.get(), iov, std::size(iov)), -1);
  EXPECT_EQ(errno, EFAULT);

  // Reading should fail since nothing should have been written.
  ASSERT_NO_FATAL_FAILURE(SetRecvFdNonBlocking());
  char recv_buf[sizeof(kSendIcmp)];
  ASSERT_EQ(read(recv_fd_.get(), &recv_buf, sizeof(recv_buf)), -1);
  EXPECT_EQ(errno, EAGAIN);
}

TEST_P(SocketFault, SendmsgENOBUFS) {
  int sockfd;
  sockfd = socket(AF_INET, SOCK_DGRAM, 0);
  ASSERT_GT(sockfd, 0) << strerror(errno);

  char data[] = "a";
  struct iovec iov[] = {{
      .iov_base = data,
      .iov_len = 1,
  }};
  char buf[CMSG_SPACE(0)];
  struct msghdr msg = {
      .msg_iov = iov,
      .msg_iovlen = 1,
      .msg_control = buf,
      .msg_controllen = UINT_MAX,
  };
  *CMSG_FIRSTHDR(&msg) = (struct cmsghdr){
      .cmsg_len = UINT_MAX,
      .cmsg_level = SOL_SOCKET,
      .cmsg_type = SCM_RIGHTS,
  };

  ASSERT_EQ(sendmsg(sockfd, &msg, 0), -1);
  ASSERT_EQ(errno, ENOBUFS);
}

INSTANTIATE_TEST_SUITE_P(SocketFault, SocketFault,
                         testing::Values(std::make_pair(SOCK_DGRAM, 0),
                                         std::make_pair(SOCK_DGRAM, IPPROTO_ICMP),
                                         std::make_pair(SOCK_STREAM, 0)));
class SndRcvBufSockOpt : public testing::TestWithParam<int> {};

// This test asserts that the value of SO_RCVBUF and SO_SNDBUF are doubled on
// set, and this doubled value is returned on get, as described in the Linux
// socket(7) man page.
TEST_P(SndRcvBufSockOpt, DoubledOnGet) {
  fbl::unique_fd fd;
  EXPECT_TRUE(fd = fbl::unique_fd(socket(AF_INET, SOCK_STREAM, 0))) << strerror(errno);

  int buf_size;
  socklen_t optlen = sizeof(buf_size);
  ASSERT_EQ(getsockopt(fd.get(), SOL_SOCKET, GetParam(), &buf_size, &optlen), 0) << strerror(errno);

  ASSERT_EQ(setsockopt(fd.get(), SOL_SOCKET, GetParam(), &buf_size, optlen), 0) << strerror(errno);

  int new_buf_size;
  ASSERT_EQ(getsockopt(fd.get(), SOL_SOCKET, GetParam(), &new_buf_size, &optlen), 0)
      << strerror(errno);
  ASSERT_EQ(new_buf_size, 2 * buf_size);
}

INSTANTIATE_TEST_SUITE_P(SndRcvBufSockOpt, SndRcvBufSockOpt, testing::Values(SO_SNDBUF, SO_RCVBUF),
                         [](const testing::TestParamInfo<int>& info) {
                           switch (info.param) {
                             case SO_SNDBUF:
                               return std::string("SO_SNDBUF");
                             case SO_RCVBUF:
                               return std::string("SO_RCVBUF");
                           }
                           return std::string("UNKNOWN(") + std::to_string(info.param) + ")";
                         });

class SocketMarkSockOpt : public testing::TestWithParam<std::tuple<int, int>> {
 protected:
  void SetUp() override {
    if (!test_helper::HasCapability(CAP_NET_ADMIN)) {
      GTEST_SKIP() << "Need CAP_NET_ADMIN to run SO_MARK tests";
    }

    auto [domain, type] = GetParam();
    EXPECT_TRUE(fd_ = fbl::unique_fd(socket(domain, type, 0))) << strerror(errno);
  }

  fbl::unique_fd fd_;
};

TEST_P(SocketMarkSockOpt, SetAndGet) {
  int initial_mark = -1;
  socklen_t optlen = sizeof(initial_mark);
  ASSERT_EQ(getsockopt(fd_.get(), SOL_SOCKET, SO_MARK, &initial_mark, &optlen), 0)
      << strerror(errno);
  ASSERT_EQ(initial_mark, 0);

  int mark = 100;
  ASSERT_EQ(setsockopt(fd_.get(), SOL_SOCKET, SO_MARK, &mark, sizeof(mark)), 0) << strerror(errno);
  int retrieved_mark = 0;
  optlen = sizeof(retrieved_mark);
  ASSERT_EQ(getsockopt(fd_.get(), SOL_SOCKET, SO_MARK, &retrieved_mark, &optlen), 0)
      << strerror(errno);
  ASSERT_EQ(optlen, sizeof(mark));
  ASSERT_EQ(mark, retrieved_mark);
}

TEST_P(SocketMarkSockOpt, NoCapabilities) {
  if (!test_helper::HasCapability(CAP_NET_RAW)) {
    GTEST_SKIP() << "Test expects CAP_NET_RAW";
  }

  test_helper::UnsetCapabilityEffective(CAP_NET_ADMIN);
  test_helper::UnsetCapabilityEffective(CAP_NET_RAW);

  // `setsockopt(SO_MARK)` must fail without the capability.
  int mark = 100;
  EXPECT_EQ(setsockopt(fd_.get(), SOL_SOCKET, SO_MARK, &mark, sizeof(mark)), -1);
  EXPECT_EQ(errno, EPERM);

  // The mark should not be set.
  int value = 1;
  socklen_t optlen = sizeof(value);
  EXPECT_EQ(getsockopt(fd_.get(), SOL_SOCKET, SO_MARK, &value, &optlen), 0) << strerror(errno);
  EXPECT_EQ(value, 0);

  // Restore capabilities.
  test_helper::SetCapabilityEffective(CAP_NET_ADMIN);
  test_helper::SetCapabilityEffective(CAP_NET_RAW);
}

TEST_P(SocketMarkSockOpt, RawCapability) {
  if (!test_helper::HasCapability(CAP_NET_RAW)) {
    GTEST_SKIP() << "Test expects CAP_NET_RAW";
  }

  // Drop the `NET_ADMIN` capability, but keep `NET_RAW`.
  test_helper::UnsetCapabilityEffective(CAP_NET_ADMIN);

  // `setsockopt(SO_MARK)` should not fail.
  int mark = 100;
  ASSERT_EQ(setsockopt(fd_.get(), SOL_SOCKET, SO_MARK, &mark, sizeof(mark)), 0) << strerror(errno);

  // The mark should be set.
  int value = 0;
  socklen_t optlen = sizeof(value);
  EXPECT_EQ(getsockopt(fd_.get(), SOL_SOCKET, SO_MARK, &value, &optlen), 0) << strerror(errno);
  EXPECT_EQ(value, mark);

  // Restore capabilities.
  test_helper::SetCapabilityEffective(CAP_NET_ADMIN);
}

TEST_P(SocketMarkSockOpt, AdminCapability) {
  if (!test_helper::HasCapability(CAP_NET_RAW)) {
    GTEST_SKIP() << "Test expects CAP_NET_RAW";
  }

  // Drop the `NET_RAW` capability, but keep `NET_ADMIN`.
  test_helper::UnsetCapabilityEffective(CAP_NET_RAW);

  // `setsockopt(SO_MARK)` should not fail.
  int mark = 100;
  ASSERT_EQ(setsockopt(fd_.get(), SOL_SOCKET, SO_MARK, &mark, sizeof(mark)), 0) << strerror(errno);

  // The mark should be set.
  int value = 0;
  socklen_t optlen = sizeof(value);
  EXPECT_EQ(getsockopt(fd_.get(), SOL_SOCKET, SO_MARK, &value, &optlen), 0) << strerror(errno);
  EXPECT_EQ(value, mark);

  // Restore capabilities.
  test_helper::SetCapabilityEffective(CAP_NET_RAW);
}

INSTANTIATE_TEST_SUITE_P(SocketMarkSockOpt, SocketMarkSockOpt,
                         testing::Combine(testing::Values(AF_INET, AF_INET6),
                                          testing::Values(SOCK_STREAM, SOCK_DGRAM)));

class BpfTest : public testing::Test {
 protected:
  void SetUp() override {
    if (!test_helper::HasCapability(CAP_NET_RAW)) {
      GTEST_SKIP() << "Need CAP_NET_RAW to run BpfTest";
    }

    packet_socket_fd_ = fbl::unique_fd(socket(AF_PACKET, SOCK_RAW, 0));
    ASSERT_TRUE(packet_socket_fd_) << strerror(errno);
    sockaddr_ll addr_ll = {
        .sll_family = AF_PACKET,
        .sll_protocol = htons(ETH_P_ALL),
    };
    ASSERT_EQ(bind(packet_socket_fd_.get(), reinterpret_cast<sockaddr*>(&addr_ll), sizeof(addr_ll)),
              0);
  }

  void SendPacketAndCheckReceived(int domain, uint16_t dst_port, bool expected);

  fbl::unique_fd packet_socket_fd_;
};

void BpfTest::SendPacketAndCheckReceived(int domain, uint16_t dst_port, bool expected) {
  sockaddr_in addr4 = {
      .sin_family = AF_INET,
      .sin_port = htons(dst_port),
      .sin_addr =
          {
              .s_addr = htonl(INADDR_LOOPBACK),
          },
  };
  sockaddr_in6 addr6 = {
      .sin6_family = AF_INET6,
      .sin6_port = htons(dst_port),
      .sin6_addr = IN6ADDR_LOOPBACK_INIT,
  };
  sockaddr* addr = domain == AF_INET6 ? reinterpret_cast<sockaddr*>(&addr6)
                                      : reinterpret_cast<sockaddr*>(&addr4);
  socklen_t addrlen = domain == AF_INET6 ? sizeof(addr6) : sizeof(addr4);

  const char data[] = "test message";
  fbl::unique_fd sendfd;
  ASSERT_TRUE(sendfd = fbl::unique_fd(socket(domain, SOCK_DGRAM, 0))) << strerror(errno);
  ASSERT_EQ(sendto(sendfd.get(), data, sizeof(data), 0, addr, addrlen),
            static_cast<int>(sizeof(data)))
      << strerror(errno);

  pollfd pfd = {
      .fd = packet_socket_fd_.get(),
      .events = POLLIN,
  };

  const int kPositiveCheckTimeoutMs = 10000;
  const int kNegativeCheckTimeoutMs = 1000;
  int timeout = expected ? kPositiveCheckTimeoutMs : kNegativeCheckTimeoutMs;
  int n = poll(&pfd, 1, timeout);
  ASSERT_GE(n, 0) << strerror(errno);
  if (expected) {
    ASSERT_EQ(n, 1);
    char buf[4096];
    ASSERT_GT(recv(packet_socket_fd_.get(), buf, sizeof(buf), 0), 0);

    // The packet was sent to loopback, so we expect to receive it twice.
    ASSERT_EQ(poll(&pfd, 1, 1000), 1);
    ASSERT_GT(recv(packet_socket_fd_.get(), buf, sizeof(buf), 0), 0);
  } else {
    ASSERT_EQ(n, 0);
  }
}

TEST_F(BpfTest, SoAttachFilter) {
  const uint16_t kTestDstPortIpv4 = 1234;
  const uint16_t kTestDstPortIpv6 = 1236;

  // This filter accepts IPv4 UDP packets on port kTestDstPortIpv4 and IPv6 UDP
  // packets on port kTestDstPortIpv6.
  static sock_filter filter_code[] = {
      // Load the protocol.
      BPF_STMT(BPF_LD | BPF_H | BPF_ABS, (__u32)SKF_AD_OFF + SKF_AD_PROTOCOL),

      // Check if this is IPv4, skip below otherwise.
      BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, ETHERTYPE_IP, 0, 8),

      // Check that the protocol is UDP.
      BPF_STMT(BPF_LD | BPF_B | BPF_ABS, (__u32)SKF_NET_OFF + 9),

      BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, IPPROTO_UDP, 1, 0),
      BPF_STMT(BPF_RET | BPF_K, 0),

      // Get the IP header length.
      BPF_STMT(BPF_LDX | BPF_B | BPF_MSH, (__u32)SKF_NET_OFF),

      // Check the destination port.
      BPF_STMT(BPF_LD | BPF_H | BPF_IND, (__u32)SKF_NET_OFF + 2),

      // Reject if not kTestDstPortIpv4.
      BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, kTestDstPortIpv4, 1, 0),
      BPF_STMT(BPF_RET | BPF_K, 0),

      // Accept.
      BPF_STMT(BPF_RET | BPF_K, 0xFFFFFFFF),

      // Check if this is IPv6.
      BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, ETHERTYPE_IPV6, 1, 0),
      BPF_STMT(BPF_RET | BPF_K, 0),

      // Check the protocol is UDP.
      BPF_STMT(BPF_LD | BPF_B | BPF_ABS, (__u32)SKF_NET_OFF + 6),
      BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, IPPROTO_UDP, 1, 0),
      BPF_STMT(BPF_RET | BPF_K, 0),

      // Load destination port, assuming standard, 40-byte IPv6 packet.
      BPF_STMT(BPF_LD | BPF_H | BPF_ABS, (__u32)SKF_NET_OFF + 42),

      // Check destination port.
      BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, kTestDstPortIpv6, 1, 0),
      BPF_STMT(BPF_RET | BPF_K, 0),

      // Accept.
      BPF_STMT(BPF_RET | BPF_K, 0xFFFFFFFF),
  };

  static const sock_fprog filter = {
      sizeof(filter_code) / sizeof(filter_code[0]),
      filter_code,
  };

  ASSERT_EQ(
      setsockopt(packet_socket_fd_.get(), SOL_SOCKET, SO_ATTACH_FILTER, &filter, sizeof(filter)),
      0);

  SendPacketAndCheckReceived(AF_INET, kTestDstPortIpv4, true);
  SendPacketAndCheckReceived(AF_INET6, kTestDstPortIpv6, true);
  SendPacketAndCheckReceived(AF_INET, kTestDstPortIpv6, false);
  SendPacketAndCheckReceived(AF_INET6, kTestDstPortIpv4, false);
}

TEST(IpTables, IpTablesAdminCap) {
  if (!test_helper::HasCapability(CAP_NET_ADMIN)) {
    GTEST_SKIP() << "Need CAP_NET_ADMIN to access iptables";
  }

  fbl::unique_fd sock;
  ASSERT_TRUE(sock = fbl::unique_fd(socket(AF_INET, SOCK_RAW, IPPROTO_ICMP))) << strerror(errno);

  struct xt_get_revision optval = {};
  strncpy(optval.name, "REDIRECT", sizeof(optval.name));
  optval.revision = 0;

  socklen_t optval_size = sizeof(optval);
  ASSERT_EQ(getsockopt(sock.get(), SOL_IP, IPT_SO_GET_REVISION_TARGET, &optval, &optval_size), 0);

  test_helper::UnsetCapabilityEffective(CAP_NET_ADMIN);

  ASSERT_EQ(getsockopt(sock.get(), SOL_IP, IPT_SO_GET_REVISION_TARGET, &optval, &optval_size), -1);
  EXPECT_EQ(errno, EPERM);

  test_helper::SetCapabilityEffective(CAP_NET_ADMIN);
}

// Helper to override `/proc/sys/net/ipv4/ping_group_range`.
class IcmpPingGidOverride {
 public:
  static constexpr char FILE_NAME[] = "/proc/sys/net/ipv4/ping_group_range";

  IcmpPingGidOverride() {
    {
      // First check that the file is writable.
      std::ofstream outfile(FILE_NAME);
      can_override_ = outfile.is_open();
    }

    if (can_override_) {
      std::tie(original_min_gid_, original_max_gid_) = Get();
    }
  }
  ~IcmpPingGidOverride() {
    if (can_override_) {
      Set(original_min_gid_, original_max_gid_);
    }
  }

  bool can_override() { return can_override_; }

  std::pair<gid_t, gid_t> Get() {
    std::ifstream ping_group_range_file(FILE_NAME);
    EXPECT_TRUE(ping_group_range_file.is_open()) << strerror(errno);
    gid_t min, max;
    ping_group_range_file >> min >> max;
    return std::make_pair(min, max);
  }

  void Set(gid_t min, gid_t max) {
    std::ofstream ping_group_range_file(FILE_NAME);
    ASSERT_TRUE(ping_group_range_file.is_open()) << strerror(errno);
    ping_group_range_file << min << " " << max;
    ping_group_range_file.close();
    EXPECT_FALSE(ping_group_range_file.fail());
  }

  void SetMin(gid_t min) {
    std::ofstream ping_group_range_file(FILE_NAME);
    ASSERT_TRUE(ping_group_range_file.is_open()) << strerror(errno);
    ping_group_range_file << min;
    ping_group_range_file.close();
    EXPECT_FALSE(ping_group_range_file.fail());
  }

 private:
  bool can_override_ = false;
  gid_t original_min_gid_ = 0;
  gid_t original_max_gid_ = 0;
};

class IcmpPingSocket : public testing::Test {
 protected:
  void SetUp() override {
    if (!test_helper::HasCapability(CAP_SETGID)) {
      GTEST_SKIP() << "Need CAP_SET_GID to access iptables";
    }

    if (!gid_range_.can_override()) {
      GTEST_SKIP() << "Need writable " << IcmpPingGidOverride::FILE_NAME;
    }
  }

  int TryCreateSocket(bool use_v6 = false) {
    fbl::unique_fd sock(
        socket(use_v6 ? AF_INET6 : AF_INET, SOCK_DGRAM,
               use_v6 ? static_cast<int>(IPPROTO_ICMPV6) : static_cast<int>(IPPROTO_ICMP)));
    if (sock) {
      return 0;
    } else {
      return -errno;
    }
  }

  void TestWriteRangeFail(const char contents[]) {
    std::ofstream ping_group_range_file(IcmpPingGidOverride::FILE_NAME);
    ASSERT_TRUE(ping_group_range_file.is_open()) << strerror(errno);
    ping_group_range_file << contents;
    ping_group_range_file.close();
    EXPECT_TRUE(ping_group_range_file.fail());
  }

  void TryCreateSocketWithGid(gid_t gid, int expected_result, bool use_v6 = false) {
    SCOPED_TRACE(fxl::StringPrintf("GID: %d", gid));
    ASSERT_EQ(setegid(gid), 0) << strerror(errno);
    ASSERT_EQ(TryCreateSocket(use_v6), expected_result);
  }

  IcmpPingGidOverride gid_range_;
};

TEST_F(IcmpPingSocket, UpdateGidRange) {
  gid_range_.Set(10, 100);
  ASSERT_EQ(gid_range_.Get(), std::make_pair(10, 100));

  gid_range_.Set(10, 0);
  ASSERT_EQ(gid_range_.Get(), std::make_pair(1, 0));

  gid_range_.Set(10, 30);
  ASSERT_EQ(gid_range_.Get(), std::make_pair(10, 30));

  gid_range_.SetMin(20);
  ASSERT_EQ(gid_range_.Get(), std::make_pair(20, 30));

  gid_range_.SetMin(30);
  ASSERT_EQ(gid_range_.Get(), std::make_pair(30, 30));

  gid_range_.SetMin(31);
  ASSERT_EQ(gid_range_.Get(), std::make_pair(1, 0));
}

TEST_F(IcmpPingSocket, CreateSocket) {
  gid_t original_gid = getegid();

  gid_range_.Set(10, 100);

  TryCreateSocketWithGid(9, -EACCES);
  TryCreateSocketWithGid(10, 0);
  TryCreateSocketWithGid(50, 0);
  TryCreateSocketWithGid(100, 0);
  TryCreateSocketWithGid(101, -EACCES);

  ASSERT_EQ(setegid(original_gid), 0) << strerror(errno);
}

TEST_F(IcmpPingSocket, CreateSocketV6) {
  gid_t original_gid = getegid();

  gid_range_.Set(10, 100);

  TryCreateSocketWithGid(9, -EACCES, true);
  TryCreateSocketWithGid(10, 0, true);
  TryCreateSocketWithGid(50, 0, true);
  TryCreateSocketWithGid(100, 0, true);
  TryCreateSocketWithGid(101, -EACCES, true);

  ASSERT_EQ(setegid(original_gid), 0) << strerror(errno);
}

// Verifies that attempts to write an out-of-range value to the
// `ping_group_range` file are rejected. Starnix and current versions of Linux
// (e.g. 6.12) allow any values in range [0, 2^32-2] (i.e. any uint32_t, except
// UINT32_MAX). Older Linux versions (e.g. 5.15) allow only values in range
// [0, 2^31-1] (i.e. non-negative int32_t), which will cause this test to fail.
TEST_F(IcmpPingSocket, SetInvalid) {
  TestWriteRangeFail("---");
  // Min and max cannot be higher than 2^32-2.
  TestWriteRangeFail("0 4294967295");
  TestWriteRangeFail("0 4294967296");
  TestWriteRangeFail("4294967295 0");
  TestWriteRangeFail("4294967296 0");

  // We should be able to set both values to 2^32-2.
  gid_range_.Set(0, 4294967294);
  gid_range_.Set(4294967294, 0);
}

TEST(InetSocket, BindToDevice) {
  if (!test_helper::HasCapability(CAP_NET_RAW)) {
    GTEST_SKIP() << "Need CAP_NET_RAW to access SO_BINDTODEVICE";
  }
  if (!test_helper::IsStarnix()) {
    GTEST_SKIP() << "This test verifies behavior specific to Starnix";
  }

  test_helper::UnsetCapabilityEffective(CAP_NET_RAW);

  fbl::unique_fd sock;
  ASSERT_TRUE(sock = fbl::unique_fd(socket(AF_INET, SOCK_STREAM, 0))) << strerror(errno);

  char iface[] = "lo";
  EXPECT_EQ(setsockopt(sock.get(), SOL_SOCKET, SO_BINDTODEVICE, iface, sizeof(iface)), -1);
  EXPECT_EQ(errno, EPERM);

  test_helper::SetCapabilityEffective(CAP_NET_RAW);
}

class ReusePortSharingTest : public testing::Test {
 protected:
  void SetUp() override {
    if (!test_helper::HasCapability(CAP_SETUID)) {
      GTEST_SKIP() << "Need CAP_SETUID to access seteuid()";
    }
  }

  void SetReusePort(int fd) {
    int optval = 1;
    EXPECT_THAT(setsockopt(fd, SOL_SOCKET, SO_REUSEPORT, &optval, sizeof(optval)),
                SyscallSucceeds());
  }
};

TEST_F(ReusePortSharingTest, ReusePortSharing) {
  struct sockaddr_in addr;
  addr.sin_family = AF_INET;
  addr.sin_port = htons(12345);
  addr.sin_addr.s_addr = htonl(INADDR_ANY);

  // Bind first socket from UID=1.
  EXPECT_THAT(seteuid(1), SyscallSucceeds());
  fbl::unique_fd sock1;
  EXPECT_TRUE(sock1 = fbl::unique_fd(socket(AF_INET, SOCK_DGRAM, 0))) << strerror(errno);
  SetReusePort(sock1.get());
  EXPECT_THAT(bind(sock1.get(), (struct sockaddr*)(&addr), sizeof(addr)), SyscallSucceeds());

  {
    // Should be able to bind another socket from the same UID.
    fbl::unique_fd sock2;
    EXPECT_TRUE(sock2 = fbl::unique_fd(socket(AF_INET, SOCK_DGRAM, 0))) << strerror(errno);
    SetReusePort(sock2.get());
    EXPECT_THAT(bind(sock2.get(), (struct sockaddr*)(&addr), sizeof(addr)), SyscallSucceeds());
  }

  // Restore `CAP_SETUID`, otherwise `seteuid()` will fail.
  test_helper::SetCapabilityEffective(CAP_SETUID);

  EXPECT_THAT(seteuid(2), SyscallSucceeds());

  fbl::unique_fd sock3;
  EXPECT_TRUE(sock3 = fbl::unique_fd(socket(AF_INET, SOCK_DGRAM, 0))) << strerror(errno);
  SetReusePort(sock3.get());

  // Should not be able to bind another socket from a different UID.
  EXPECT_THAT(bind(sock3.get(), (struct sockaddr*)(&addr), sizeof(addr)),
              SyscallFailsWithErrno(EADDRINUSE));

  // Restore original uid.
  EXPECT_THAT(setresuid(0, 0, 0), SyscallSucceeds());
}
