// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_BRINGUP_BIN_NETSVC_INET6_H_
#define SRC_BRINGUP_BIN_NETSVC_INET6_H_

#include <endian.h>
#include <lib/async/dispatcher.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <zircon/compiler.h>
#include <zircon/types.h>

#include <iterator>

using mac_addr_t = struct mac_addr;
using ip6_addr_t = struct ip6_addr;
using ip6_hdr_t = struct ip6_hdr;
using udp_hdr_t = struct udp_hdr;
using icmp6_hdr_t = struct icmp6_hdr;
using ndp_n_hdr_t = struct ndp_n_hdr;

#define ETH_ADDR_LEN 6
#define ETH_HDR_LEN 14
#define ETH_MTU 1514

#define IP6_ADDR_LEN 16

#define IP6_HDR_LEN 40

#define IP6_MIN_MTU 1280

#define UDP_HDR_LEN 8

struct mac_addr {
  uint8_t x[ETH_ADDR_LEN];
} __attribute__((packed));

struct ip6_addr {
  uint8_t u8[IP6_ADDR_LEN];

  inline bool operator==(const ip6_addr& rhs) const { return memcmp(u8, rhs.u8, sizeof(u8)) == 0; }
  inline bool operator!=(const ip6_addr& rhs) const { return !(*this == rhs); }
} __attribute__((packed));

extern const ip6_addr_t ip6_ll_all_nodes;

#define ETH_IP4 0x0800
#define ETH_ARP 0x0806
#define ETH_IP6 0x86DD

#define HDR_HNH_OPT 0
#define HDR_TCP 6
#define HDR_UDP 17
#define HDR_ROUTING 43
#define HDR_FRAGMENT 44
#define HDR_ICMP6 58
#define HDR_NONE 59
#define HDR_DST_OPT 60

struct ip6_hdr {
  uint32_t ver_tc_flow;
  uint16_t length;
  uint8_t next_header;
  uint8_t hop_limit;
  ip6_addr_t src;
  ip6_addr_t dst;
} __attribute__((packed));

struct udp_hdr {
  uint16_t src_port;
  uint16_t dst_port;
  uint16_t length;
  uint16_t checksum;
} __attribute__((packed));

#define ICMP6_DEST_UNREACHABLE 1
#define ICMP6_PACKET_TOO_BIG 2
#define ICMP6_TIME_EXCEEDED 3
#define ICMP6_PARAMETER_PROBLEM 4

#define ICMP6_ECHO_REQUEST 128
#define ICMP6_ECHO_REPLY 129

#define ICMP6_NDP_R_ADVERTISE 134

#define ICMP6_NDP_N_SOLICIT 135
#define ICMP6_NDP_N_ADVERTISE 136

struct icmp6_hdr {
  uint8_t type;
  uint8_t code;
  uint16_t checksum;
} __attribute__((packed));

struct ndp_n_hdr {
  uint8_t type;
  uint8_t code;
  uint16_t checksum;
  uint32_t flags;
  uint8_t target[IP6_ADDR_LEN];
  uint8_t options[0];
} __attribute__((packed));

#define NDP_N_SRC_LL_ADDR 1
#define NDP_N_TGT_LL_ADDR 2
#define NDP_N_PREFIX_INFO 3
#define NDP_N_REDIRECTED_HDR 4
#define NDP_N_MTU 5

#ifndef ntohs
#define ntohs(n) be16toh(n)
#define htons(n) htobe16(n)
#endif

#ifndef ntohl
#define ntohl(n) be32toh(n)
#define htonl(n) htobe32(n)
#endif

// provided by inet6.c
void ip6_init(mac_addr_t macaddr, bool quiet);
void eth_recv(async_dispatcher_t* dispatcher, void* data, size_t len);

int eth_add_mcast_filter(const mac_addr_t* addr);

// call to transmit a UDP packet
zx_status_t udp6_send(const void* data, size_t len, const ip6_addr_t* daddr, uint16_t dport,
                      uint16_t sport, bool block);

// implement to receive UDP packets
void udp6_recv(async_dispatcher_t* dispatcher, void* data, size_t len, const ip6_addr_t* daddr,
               uint16_t dport, const ip6_addr_t* saddr, uint16_t sport);

uint16_t ip6_checksum(const ip6_hdr_t& ip, uint8_t type);

uint16_t ip6_header_checksum(const ip6_hdr_t& ip, uint8_t type);

uint16_t ip6_finalize_checksum(uint16_t header_checksum, const void* payload, size_t len);

void send_router_advertisement();

// NOTES
//
// This is an extremely minimal IPv6 stack, supporting just enough
// functionality to talk to link local hosts over UDP.
//
// It responds to ICMPv6 Neighbor Solicitations for its link local
// address, which is computed from the mac address provided by the
// ethernet interface driver.
//
// It responds to PINGs.
//
// It can only transmit to multicast addresses or to the address it
// last received a packet from (general usecase is to reply to a UDP
// packet from the UDP callback, which this supports)
//
// It does not currently do duplicate address detection, which is
// probably the most severe bug.
//
// It does not support any IPv6 options and will drop packets with
// options.
//
// It expects the network stack to provide transmit buffer allocation
// and free functionality.  It will allocate a single transmit buffer
// from udp6_send() or icmp6_send() to fill out and either pass to the
// network stack via eth_send() or, in the event of an error, release
// via eth_put_buffer().
//

#endif  // SRC_BRINGUP_BIN_NETSVC_INET6_H_
