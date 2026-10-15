// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/bringup/bin/netsvc/netboot.h"

#include <assert.h>
#include <errno.h>
#include <fidl/fuchsia.boot/cpp/wire.h>
#include <fidl/fuchsia.hardware.power.statecontrol/cpp/wire.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <lib/netboot/netboot.h>
#include <stdio.h>
#include <string.h>
#include <threads.h>
#include <zircon/processargs.h>
#include <zircon/syscalls.h>

#include "src/bringup/bin/netsvc/inet6.h"
#include "src/bringup/bin/netsvc/netcp.h"
#include "src/bringup/bin/netsvc/netsvc.h"

bool xfer_active = false;

namespace {

uint32_t last_cookie = 0;
uint32_t last_cmd = 0;
uint32_t last_arg = 0;
uint32_t last_ack_cmd = 0;
uint32_t last_ack_arg = 0;

#define MAX_ADVERTISE_DATA_LEN 256

namespace statecontrol = fuchsia_hardware_power_statecontrol;

}  // namespace

void netboot_advertise(const char* nodename) {
  // Don't advertise if a transfer is active.
  if (xfer_active)
    return;

  struct {
    netboot_message_header_t hdr;
    char data[MAX_ADVERTISE_DATA_LEN];
  } packet_data;
  netboot_message_header_t* hdr = &packet_data.hdr;
  hdr->magic = NETBOOT_MAGIC;
  hdr->cookie = 0;
  hdr->cmd = NETBOOT_COMMAND_ADVERTISE;
  hdr->arg = NETBOOT_VERSION_CURRENT;

  snprintf(packet_data.data, MAX_ADVERTISE_DATA_LEN, "version=%s;nodename=%s",
           NETBOOT_BOOTLOADER_VERSION, nodename);
  const size_t data_len = strlen(packet_data.data) + 1;
  udp6_send(&packet_data, sizeof(netboot_message_header_t) + data_len, &ip6_ll_all_nodes,
            NETBOOT_PORT_ADVERT, NETBOOT_PORT_SERVER, false);
}

namespace {

void nb_open(const char* filename, uint32_t cookie, uint32_t arg, const ip6_addr_t* saddr,
             uint16_t sport, uint16_t dport) {
  netboot_message_header_t m;
  m.magic = NETBOOT_MAGIC;
  m.cookie = cookie;
  m.cmd = NETBOOT_COMMAND_ACK;
  m.arg = netcp_open(filename, arg, nullptr);
  udp6_send(&m, sizeof(m), saddr, sport, dport, false);
}

struct netfilemsg {
  netboot_message_header_t hdr;
  uint8_t data[1024];
};

void nb_read(uint32_t cookie, uint32_t arg, const ip6_addr_t* saddr, uint16_t sport,
             uint16_t dport) {
  static netfilemsg m = {
      .hdr =
          {
              .magic = NETBOOT_MAGIC,
              .cookie = 0,
              .cmd = NETBOOT_COMMAND_ACK,
              .arg = 0,
          },
      .data = {},
  };
  static size_t msg_size = 0;
  static uint32_t blocknum = static_cast<uint32_t>(-1);
  if (arg == blocknum) {
    // Request to resend last message, verify that the cookie is unchanged
    if (cookie != m.hdr.cookie) {
      m.hdr.arg = -EIO;
      m.hdr.cookie = cookie;
      msg_size = sizeof(m.hdr);
    }
  } else if (arg == 0 || arg == blocknum + 1) {
    ssize_t result = netcp_read(&m.data, sizeof(m.data));
    if (result < 0) {
      m.hdr.arg = static_cast<uint32_t>(result);
      msg_size = sizeof(m.hdr);
    } else {
      // Note that the response does not use actual size as the argument,
      // it matches the *requested* size. Actual size can be determined by
      // the packet size.
      m.hdr.arg = arg;
      msg_size = sizeof(m.hdr) + result;
    }
    m.hdr.cookie = cookie;
    blocknum = arg;
  } else {
    // Ignore bogus read requests -- host will timeout if they're confused
    return;
  }
  udp6_send(&m, msg_size, saddr, sport, dport, false);
}

void nb_write(const char* data, size_t len, uint32_t cookie, uint32_t arg, const ip6_addr_t* saddr,
              uint16_t sport, uint16_t dport) {
  static netboot_message_header_t m = {
      .magic = NETBOOT_MAGIC,
      .cookie = 0,
      .cmd = NETBOOT_COMMAND_ACK,
      .arg = 0,
  };
  static uint32_t blocknum = static_cast<uint32_t>(-1);
  if (arg == blocknum) {
    // Request to repeat last write, verify that cookie is unchanged
    if (cookie != m.cookie) {
      m.arg = -EIO;
    }
  } else if (arg == 0 || arg == blocknum + 1) {
    ssize_t result = netcp_write(data, len);
    m.arg = static_cast<uint32_t>(result > 0 ? 0 : result);
    blocknum = arg;
  }
  m.cookie = cookie;
  udp6_send(&m, sizeof(m), saddr, sport, dport, false);
}

void nb_close(uint32_t cookie, const ip6_addr_t* saddr, uint16_t sport, uint16_t dport) {
  netboot_message_header_t m;
  m.magic = NETBOOT_MAGIC;
  m.cookie = cookie;
  m.cmd = NETBOOT_COMMAND_ACK;
  m.arg = netcp_close();
  udp6_send(&m, sizeof(m), saddr, sport, dport, false);
}

zx_status_t reboot() {
  zx::result client_end = component::Connect<statecontrol::Admin>();
  if (client_end.is_error()) {
    return client_end.status_value();
  }

  fidl::Arena arena;
  auto builder = fuchsia_hardware_power_statecontrol::wire::ShutdownOptions::Builder(arena);
  fuchsia_hardware_power_statecontrol::ShutdownReason reasons[1] = {
      fuchsia_hardware_power_statecontrol::ShutdownReason::kDeveloperRequest};
  auto vector_view =
      fidl::VectorView<fuchsia_hardware_power_statecontrol::ShutdownReason>::FromExternal(reasons);
  builder.reasons(vector_view);
  builder.action(fuchsia_hardware_power_statecontrol::ShutdownAction::kReboot);
  fidl::WireResult response = fidl::WireCall(client_end.value())->Shutdown(builder.Build());
  if (response.status() != ZX_OK) {
    return response.status();
  }
  if (response.value().is_error()) {
    return response.value().error_value();
  }
  // Wait for the world to end.
  zx_nanosleep(ZX_TIME_INFINITE);
  return ZX_ERR_INTERNAL;
}

void bootloader_recv(void* data, size_t len, const ip6_addr_t* daddr, uint16_t dport,
                     const ip6_addr_t* saddr, uint16_t sport) {
  netboot_message_header_t msg_header;
  netboot_message_header_t ack;

  bool do_transmit = true;
  bool do_reboot = false;

  if (dport != NETBOOT_PORT_SERVER)
    return;

  // Not enough bytes to be a message
  if (len < sizeof(netboot_message_header_t))
    return;

  // Must be copied to stack structure rather than interpreted in place for
  // UBSan, see https://fxbug.dev/42122166.
  memcpy(&msg_header, data, sizeof(msg_header));
  char* msg_data = reinterpret_cast<char*>(data) + sizeof(msg_header);

  len -= sizeof(netboot_message_header_t);

  if ((last_cookie == msg_header.cookie) && (last_cmd == msg_header.cmd) &&
      (last_arg == msg_header.arg)) {
    // host must have missed the ack. resend
    ack.magic = NETBOOT_MAGIC;
    ack.cookie = last_cookie;
    ack.cmd = last_ack_cmd;
    ack.arg = last_ack_arg;
    goto transmit;
  }

  ack.cmd = NETBOOT_COMMAND_ACK;
  ack.arg = 0;

  switch (msg_header.cmd) {
    case NETBOOT_COMMAND_EXECUTE:
      if (len == 0)
        return;
      msg_data[len - 1] = 0;
      break;
    case NETBOOT_COMMAND_REBOOT:
      do_reboot = true;
      printf("netboot: Reboot ...\n");
      break;
    default:
      // We don't have a handler for this command, let netsvc handle it.
      do_transmit = false;
  }

  last_cookie = msg_header.cookie;
  last_cmd = msg_header.cmd;
  last_arg = msg_header.arg;
  last_ack_cmd = ack.cmd;
  last_ack_arg = ack.arg;

  ack.cookie = msg_header.cookie;
  ack.magic = NETBOOT_MAGIC;
transmit:
  if (do_transmit) {
    udp6_send(&ack, sizeof(ack), saddr, sport, NETBOOT_PORT_SERVER, false);
  }

  if (do_reboot) {
    zx_status_t status = reboot();
    if (status != ZX_OK) {
      printf("netboot: Reboot failed. status = %d\n", status);
    }
  }
}

}  // namespace

void netboot_recv(void* data, size_t len, bool is_mcast, const ip6_addr_t* daddr, uint16_t dport,
                  const ip6_addr_t* saddr, uint16_t sport) {
  netboot_message_header_t msg_header;
  // Not enough bytes to be a message
  if (len < sizeof(msg_header)) {
    return;
  }
  // Must be copied to stack structure rather than interpreted in place for
  // UBSan, see https://fxbug.dev/42122166.
  memcpy(&msg_header, data, sizeof(msg_header));
  char* msg_data = reinterpret_cast<char*>(data) + sizeof(msg_header);

  if (msg_header.magic != NETBOOT_MAGIC) {
    return;
  }
  len -= sizeof(msg_header);

  if (len) {
    msg_data[len - 1] = '\0';
  }

  if (!all_features()) {
    assert(!netbootloader());  // This is checked in netsvc's main().
    if (msg_header.cmd != NETBOOT_COMMAND_QUERY) {
      // Only QUERY is allowed in minimal mode.
      return;
    }
  }

  switch (msg_header.cmd) {
    case NETBOOT_COMMAND_QUERY: {
      std::string_view msg_data_view{msg_data};
      if (msg_data_view != "*" && msg_data_view != nodename()) {
        break;
      }
      size_t dlen = strlen(nodename()) + 1;
      char buf[1024 + sizeof(netboot_message_header_t)];
      if ((dlen + sizeof(netboot_message_header_t)) > sizeof(buf)) {
        return;
      }
      msg_header.cmd = NETBOOT_COMMAND_ACK;
      memcpy(buf, &msg_header, sizeof(netboot_message_header_t));
      memcpy(buf + sizeof(netboot_message_header_t), nodename(), dlen);
      udp6_send(buf, sizeof(netboot_message_header_t) + dlen, saddr, sport, dport, false);
      break;
    }
    case NETBOOT_COMMAND_GET_ADVERT: {
      // Only respond when netbootloader is enabled, so that paving behavior is same as the
      // broadcasted advertisement.
      if (!netbootloader()) {
        break;
      }

      struct {
        netboot_message_header_t hdr;
        char data[MAX_ADVERTISE_DATA_LEN];
      } packet_data;
      netboot_message_header_t* hdr = &packet_data.hdr;
      hdr->magic = NETBOOT_MAGIC;
      hdr->cookie = 0;
      hdr->cmd = NETBOOT_COMMAND_ADVERTISE;
      hdr->arg = NETBOOT_VERSION_CURRENT;

      snprintf(packet_data.data, MAX_ADVERTISE_DATA_LEN, "version=%s;nodename=%s",
               NETBOOT_BOOTLOADER_VERSION, nodename());
      const size_t data_len = strlen(packet_data.data) + 1;
      udp6_send(&packet_data, sizeof(netboot_message_header_t) + data_len, saddr, sport, dport,
                false);
      break;
    }
    case NETBOOT_COMMAND_SHELL_CMD:
      if (!is_mcast) {
        netboot_run_cmd(msg_data);
        return;
      }
      break;
    case NETBOOT_COMMAND_OPEN:
      nb_open(msg_data, msg_header.cookie, msg_header.arg, saddr, sport, dport);
      break;
    case NETBOOT_COMMAND_READ:
      nb_read(msg_header.cookie, msg_header.arg, saddr, sport, dport);
      break;
    case NETBOOT_COMMAND_WRITE:
      len--;  // NB NUL-terminator is not part of the data
      nb_write(msg_data, len, msg_header.cookie, msg_header.arg, saddr, sport, dport);
      break;
    case NETBOOT_COMMAND_CLOSE:
      nb_close(msg_header.cookie, saddr, sport, dport);
      break;
    default:
      // If the bootloader is enabled, then let it have a crack at the
      // incoming packets as well.
      if (netbootloader()) {
        bootloader_recv(data, len + sizeof(netboot_message_header_t), daddr, dport, saddr, sport);
      }
  }
}
