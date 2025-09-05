// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/sshd-host/service.h"

#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <fidl/fuchsia.component/cpp/fidl.h>
#include <fidl/fuchsia.process/cpp/fidl.h>
#include <lib/async/cpp/task.h>
#include <lib/async/dispatcher.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <lib/fdio/fd.h>
#include <lib/fit/defer.h>
#include <lib/fit/function.h>
#include <lib/syslog/cpp/macros.h>
#include <netdb.h>
#include <netinet/in.h>
#include <poll.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <zircon/errors.h>
#include <zircon/processargs.h>
#include <zircon/types.h>

#include <array>
#include <vector>

#include <fbl/unique_fd.h>

#include "src/lib/fxl/strings/string_printf.h"

namespace sshd_host {

Service::Service(async_dispatcher_t* dispatcher, uint16_t port)
    : dispatcher_(dispatcher),
      sock_(fbl::unique_fd(socket(AF_INET6, SOCK_STREAM, IPPROTO_TCP))),
      waiter_(dispatcher) {
  if (!sock_.is_valid()) {
    FX_LOGS(FATAL) << "Failed to create socket: " << strerror(errno);
  }
  sockaddr_storage addr;
  *reinterpret_cast<struct sockaddr_in6*>(&addr) = sockaddr_in6{
      .sin6_family = AF_INET6,
      .sin6_port = htons(port),
      .sin6_addr = in6addr_any,
  };
  if (bind(sock_.get(), reinterpret_cast<const sockaddr*>(&addr), sizeof addr) < 0) {
    FX_LOGS(FATAL) << "Failed to bind to " << port << ": " << strerror(errno);
  }

  FX_LOG_KV(INFO, "listen() for inbound SSH connections", FX_KV("port", (int)port));
  if (listen(sock_.get(), 10) < 0) {
    FX_LOGS(FATAL) << "Failed to listen: " << strerror(errno);
  }

  Wait();
}

Service::~Service() = default;

void Service::Wait() {
  FX_LOG_KV(DEBUG, "Waiting for next connection");

  waiter_.Wait(
      [this](zx_status_t status, uint32_t /*events*/) {
        if (status != ZX_OK) {
          FX_PLOGS(FATAL, status) << "Failed to wait on socket";
        }

        struct sockaddr_storage peer_addr{};
        socklen_t peer_addr_len = sizeof(peer_addr);
        fbl::unique_fd conn(
            accept(sock_.get(), reinterpret_cast<struct sockaddr*>(&peer_addr), &peer_addr_len));
        if (!conn.is_valid()) {
          if (errno == EPIPE) {
            FX_LOGS(ERROR) << "The netstack died. Terminating.";
            // Avoid a crash here because the netstack terminating already
            // causes the system to reboot. This prevents cascading crash
            // reports.
            exit(1);
          } else {
            FX_LOGS(ERROR) << "Failed to accept: " << strerror(errno);
            // Wait for another connection.
            Wait();
          }
          return;
        }

        std::string peer_name = "unknown";
        char host[NI_MAXHOST];
        char port[NI_MAXSERV];
        if (int res =
                getnameinfo(reinterpret_cast<struct sockaddr*>(&peer_addr), peer_addr_len, host,
                            sizeof(host), port, sizeof(port), NI_NUMERICHOST | NI_NUMERICSERV);
            res == 0) {
          peer_name = fxl::StringPrintf("[%s]:%s", host, port);
        } else {
          FX_LOGS(WARNING)
              << "Error from getnameinfo(.., NI_NUMERICHOST | NI_NUMERICSERV) for peer address: "
              << gai_strerror(res);
        }
        FX_LOG_KV(DEBUG, "Accepted connection", FX_KV("remote", peer_name.c_str()));

        Launch(std::move(conn));
        Wait();
      },
      sock_.get(), POLLIN);
}

void Service::Launch(fbl::unique_fd conn) {
  uint64_t child_num = next_child_num_++;
  std::string child_name = fxl::StringPrintf("sshd-%lu", child_num);

  auto realm_client_end = component::Connect<fuchsia_component::Realm>();
  if (realm_client_end.is_error()) {
    FX_PLOGS(ERROR, realm_client_end.status_value()) << "Failed to connect to realm service";
    return;
  }

  fidl::SyncClient<fuchsia_component::Realm> realm{std::move(*realm_client_end)};

  auto controller_endpoints = fidl::CreateEndpoints<fuchsia_component::Controller>();
  if (controller_endpoints.is_error()) {
    FX_PLOGS(ERROR, controller_endpoints.status_value())
        << "Failed to connect to create controller endpoints";
    return;
  }

  fidl::SyncClient<fuchsia_component::Controller> controller{
      std::move(controller_endpoints->client)};
  {
    fuchsia_component_decl::CollectionRef collection{{
        .name = std::string(kShellCollection),
    }};
    fuchsia_component_decl::Child decl{{.name = child_name,
                                        .url = "#meta/sshd.cm",
                                        .startup = fuchsia_component_decl::StartupMode::kLazy}};

    fuchsia_component::CreateChildArgs args{
        {.controller = std::move(controller_endpoints->server)}};

    auto result = realm->CreateChild(
        {{.collection = collection, .decl = std::move(decl), .args = std::move(args)}});
    if (result.is_error()) {
      FX_LOGS(ERROR) << "Failed to create sshd child: " << result.error_value().FormatDescription();
      return;
    }
  }

  auto execution_controller_endpoints =
      fidl::CreateEndpoints<fuchsia_component::ExecutionController>();
  if (execution_controller_endpoints.is_error()) {
    FX_LOGS(ERROR) << "Failed to create execution controller endpoints: "
                   << execution_controller_endpoints.status_string();
    return;
  }

  // Create a socket and pass it to the child as stderr. We read the stderr output and
  // print it to the logs for debugging purposes.
  zx::socket stderr_socket, child_stderr;
  if (zx_status_t status = zx::socket::create(0, &stderr_socket, &child_stderr); status != ZX_OK) {
    FX_PLOGS(ERROR, status) << "Failed to create stderr socket";
    return;
  }

  controllers_.emplace(
      std::piecewise_construct, std::forward_as_tuple(child_num),
      std::forward_as_tuple(this, child_num, std::move(child_name),
                            std::move(execution_controller_endpoints->client), dispatcher_,
                            std::move(realm), std::move(stderr_socket)));
  auto remove_controller_on_error =
      fit::defer([this, child_num]() { controllers_.erase(child_num); });

  // Pass the connection fd as stdin and stdout handles to the sshd component.
  std::vector<fuchsia_process::HandleInfo> numbered_handles;
  for (int fd : {STDIN_FILENO, STDOUT_FILENO}) {
    zx::handle conn_handle;
    if (zx_status_t status = fdio_fd_clone(conn.get(), conn_handle.reset_and_get_address());
        status != ZX_OK) {
      FX_PLOGS(ERROR, status) << "Failed to clone connection file descriptor " << conn.get();
      return;
    }
    numbered_handles.push_back(
        fuchsia_process::HandleInfo{{.handle = std::move(conn_handle), .id = PA_HND(PA_FD, fd)}});
  }
  numbered_handles.push_back(fuchsia_process::HandleInfo{
      {.handle = std::move(child_stderr), .id = PA_HND(PA_FD, STDERR_FILENO)}});

  auto result = controller->Start(
      {{.args = {{
            .numbered_handles = std::move(numbered_handles),
            .namespace_entries = {},
        }},
        .execution_controller = std::move(execution_controller_endpoints->server)}});

  if (result.is_error()) {
    FX_LOGS(ERROR) << "Failed to start sshd child: " << result.error_value().FormatDescription();
    return;
  }

  remove_controller_on_error.cancel();
}

Service::Controller::Controller(Service* service, uint64_t child_num, std::string child_name,
                                fidl::ClientEnd<fuchsia_component::ExecutionController> client_end,
                                async_dispatcher_t* dispatcher,
                                fidl::SyncClient<fuchsia_component::Realm> realm,
                                zx::socket stderr_socket)
    : service_(service),
      child_num_(child_num),
      child_name_(std::move(child_name)),
      client_(std::move(client_end), dispatcher, this),
      realm_(std::move(realm)),
      stderr_socket_(std::move(stderr_socket)),
      stderr_waiter_(this, stderr_socket_.get(), ZX_SOCKET_READABLE | ZX_SOCKET_PEER_CLOSED) {
  Wait();
}

Service::Controller::~Controller() { stderr_waiter_.Cancel(); }

void Service::Controller::Wait() {
  zx_status_t status = stderr_waiter_.Begin(service_->dispatcher_);
  if (status != ZX_OK) {
    FX_PLOGS(ERROR, status) << "Failed to wait on stderr socket for " << child_name_;
  }
}

void Service::Controller::OnStderr(async_dispatcher_t* dispatcher, async::WaitBase* wait,
                                   zx_status_t status, const zx_packet_signal_t* signal) {
  if (status != ZX_OK) {
    FX_PLOGS(ERROR, status) << "Wait on stderr failed for " << child_name_;
    return;
  }

  // It's possible for the socket to be both readable and closed in the same signal.
  if (signal->observed & ZX_SOCKET_READABLE) {
    constexpr size_t kStderrBufSize = 1024;
    std::array<char, kStderrBufSize> buf;
    size_t actual;
    if (zx_status_t status = stderr_socket_.read(0, buf.data(), buf.size(), &actual);
        status != ZX_OK) {
      if (status != ZX_ERR_PEER_CLOSED) {
        FX_PLOGS(ERROR, status) << "Failed to read from stderr socket for " << child_name_;
      }
      return;
    }

    stderr_buf_.append(buf.data(), actual);

    constexpr size_t kMaxStderrBufSize = 16 * 1024;  // 16 KiB
    if (stderr_buf_.length() > kMaxStderrBufSize) {
      FX_LOGS(WARNING) << "sshd stderr buffer for " << child_name_
                       << " is full, flushing without newline.";
      FX_LOGS(DEBUG) << "sshd stderr: " << stderr_buf_;
      stderr_buf_.clear();
    }

    std::string_view msg_stream(stderr_buf_);
    while (!msg_stream.empty()) {
      size_t msg_end = msg_stream.find('\n');
      // no msg in stream (e.g. line break not found).
      if (msg_end == std::string_view::npos) {
        break;
      }
      // include '\n'
      std::string_view msg = msg_stream.substr(0, msg_end + 1);
      msg_stream.remove_prefix(msg.size());
      // remove '\n'
      msg.remove_suffix(1);
      // remove '\r' if present, '\r' may only be inserted
      // in certain systems preceding `\n`.
      if (msg.ends_with('\r')) {
        msg.remove_suffix(1);
      }
      // output msg even if empty
      FX_LOGS(DEBUG) << "ssh stderr: " << msg;
    }

    // If the entire buffer was processed, the view will be empty
    if (msg_stream.empty()) {
      stderr_buf_.clear();
    } else if (msg_stream.data() != stderr_buf_.data()) {
      stderr_buf_ = std::string(msg_stream);
    }
  }
  if (signal->observed & ZX_SOCKET_PEER_CLOSED) {
    if (!stderr_buf_.empty()) {
      FX_LOGS(DEBUG) << "sshd stderr: " << stderr_buf_;
    }
    // Do not re-arm the wait, the socket is closed.
    return;
  }
  Wait();
}

void Service::OnStop(zx_status_t status, Controller* ptr) {
  if (status != ZX_OK) {
    FX_PLOGS(INFO, status) << "sshd component stopped with status";
  }

  // The controller is currently executing on the dispatcher thread. We can't
  // destroy it here, because that would be a use-after-free. Instead, we
  // schedule its destruction for the next turn of the event loop.
  async::PostTask(dispatcher_, [this, child_num = ptr->child_num_]() {
    auto it = controllers_.find(child_num);
    if (it == controllers_.end()) {
      return;
    }
    auto& controller = it->second;

    // Take ownership of the realm client to ensure it outlives the DestroyChild call.
    auto realm = std::move(controller.realm_);

    // Destroy the component.
    auto result = realm->DestroyChild({{.child = {{
                                            .name = controller.child_name_,
                                            .collection = std::string(kShellCollection),

                                        }}}});
    if (result.is_error()) {
      FX_LOGS(ERROR) << "Failed to destroy sshd child: "
                     << result.error_value().FormatDescription();
    }

    // Remove the controller.
    controllers_.erase(it);
  });
}

}  // namespace sshd_host
