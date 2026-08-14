/*
 * Copyright (c) 2019 The Fuchsia Authors
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR ANY
 * SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN ACTION
 * OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF OR IN
 * CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 */

#include <fidl/fuchsia.wlan.ieee80211/cpp/wire.h>

#include <gtest/gtest.h>

#include "src/connectivity/wlan/drivers/third_party/broadcom/brcmfmac/brcmu_d11.h"
#include "third_party/bcmdhd/crossdriver/bcmwifi_channels.h"

namespace {

static void verify_channel_to_chanspec(const fuchsia_wlan_ieee80211::wire::ChannelNumber& in_ch,
                                       fuchsia_wlan_ieee80211::wire::ChannelBandwidth cbw,
                                       const brcmu_chan& expected) {
  brcmu_d11inf d11_inf = {.io_type = BRCMU_D11AC_IOTYPE};
  brcmu_d11_attach(&d11_inf);

  uint16_t chanspec = channel_to_chanspec(&d11_inf, in_ch, cbw);
  brcmu_chan actual = {.chspec = chanspec};
  d11_inf.decchspec(&actual);

  EXPECT_EQ(actual.chnum, expected.chnum);
  EXPECT_EQ(actual.band, expected.band);
  EXPECT_EQ(actual.bw, expected.bw);
  EXPECT_EQ(actual.sb, expected.sb);
}

TEST(ChannelConversion, ChannelToChanspec) {
  brcmu_chan out_ch;
  using fuchsia_wlan_ieee80211::wire::ChannelBandwidth;

  {
    // Try a simple 20 MHz channel in the 2.4 GHz band
    fuchsia_wlan_ieee80211::wire::ChannelNumber in_ch = {
        .band = fuchsia_wlan_ieee80211::wire::WlanBand::kTwoGhz, .number = 11};
    out_ch = {
        .chnum = 11, .band = BRCMU_CHAN_BAND_2G, .bw = BRCMU_CHAN_BW_20, .sb = BRCMU_CHAN_SB_NONE};
    verify_channel_to_chanspec(in_ch, ChannelBandwidth::kCbw20, out_ch);
  }

  {
    // Try a 40+ MHz channel in the 5 GHz band
    fuchsia_wlan_ieee80211::wire::ChannelNumber in_ch = {
        .band = fuchsia_wlan_ieee80211::wire::WlanBand::kFiveGhz, .number = 44};
    out_ch = {
        .chnum = 44, .band = BRCMU_CHAN_BAND_5G, .bw = BRCMU_CHAN_BW_40, .sb = BRCMU_CHAN_SB_L};
    verify_channel_to_chanspec(in_ch, ChannelBandwidth::kCbw40, out_ch);
  }

  {
    // Try a 40- MHz channel in the 5 GHz band
    fuchsia_wlan_ieee80211::wire::ChannelNumber in_ch = {
        .band = fuchsia_wlan_ieee80211::wire::WlanBand::kFiveGhz, .number = 112};
    out_ch = {
        .chnum = 112, .band = BRCMU_CHAN_BAND_5G, .bw = BRCMU_CHAN_BW_40, .sb = BRCMU_CHAN_SB_U};
    verify_channel_to_chanspec(in_ch, ChannelBandwidth::kCbw40Below, out_ch);
  }
}

static void verify_chanspec_to_operating_channel(
    const brcmu_chan& in_ch, const fuchsia_wlan_ieee80211::wire::ChannelNumber& expected_channel,
    fuchsia_wlan_ieee80211::wire::ChannelBandwidth expected_cbw,
    const fuchsia_wlan_ieee80211::wire::ChannelNumber& expected_secondary80) {
  brcmu_d11inf d11_inf = {.io_type = BRCMU_D11AC_IOTYPE};
  brcmu_d11_attach(&d11_inf);

  brcmu_chan in_ch_temp = in_ch;
  d11_inf.encchspec(&in_ch_temp);

  auto actual_channel = chanspec_to_operating_channel_number(&d11_inf, in_ch_temp.chspec);
  auto actual_cbw = chanspec_to_channel_bandwidth(&d11_inf, in_ch_temp.chspec);
  auto actual_secondary80 = chanspec_to_secondary80(&d11_inf, in_ch_temp.chspec);

  EXPECT_EQ(actual_channel.number, expected_channel.number);
  EXPECT_EQ(actual_channel.band, expected_channel.band);
  EXPECT_EQ(actual_cbw, expected_cbw);
  EXPECT_EQ(actual_secondary80.number, expected_secondary80.number);
  EXPECT_EQ(actual_secondary80.band, expected_secondary80.band);
}

static void verify_chanspec_to_primary_channel(
    const brcmu_chan& in_ch, const fuchsia_wlan_ieee80211::wire::ChannelNumber& expected_channel) {
  brcmu_d11inf d11_inf = {.io_type = BRCMU_D11AC_IOTYPE};
  brcmu_d11_attach(&d11_inf);

  brcmu_chan in_ch_temp = in_ch;
  d11_inf.encchspec(&in_ch_temp);

  auto actual_channel = chanspec_to_primary_channel_number(&d11_inf, in_ch_temp.chspec);

  EXPECT_EQ(actual_channel.number, expected_channel.number);
  EXPECT_EQ(actual_channel.band, expected_channel.band);
}

TEST(ChannelConversion, ChanspecToOperatingChannel) {
  brcmu_chan in_ch;
  using fuchsia_wlan_ieee80211::wire::ChannelBandwidth;
  using fuchsia_wlan_ieee80211::wire::WlanBand;

  {
    // Try a simple 20 MHz channel in the 2.4 GHz band
    in_ch = {
        .chnum = 11, .band = BRCMU_CHAN_BAND_2G, .bw = BRCMU_CHAN_BW_20, .sb = BRCMU_CHAN_SB_NONE};
    fuchsia_wlan_ieee80211::wire::ChannelNumber out_ch = {.band = WlanBand::kTwoGhz, .number = 11};
    verify_chanspec_to_operating_channel(in_ch, out_ch, ChannelBandwidth::kCbw20,
                                         {.band = out_ch.band, .number = 0});
  }

  {
    // Try a 40+ MHz channel in the 2.4 GHz band (SB_L => secondary above)
    in_ch = {.chnum = 3, .band = BRCMU_CHAN_BAND_2G, .bw = BRCMU_CHAN_BW_40, .sb = BRCMU_CHAN_SB_L};
    fuchsia_wlan_ieee80211::wire::ChannelNumber out_ch = {.band = WlanBand::kTwoGhz, .number = 3};
    verify_chanspec_to_operating_channel(in_ch, out_ch, ChannelBandwidth::kCbw40,
                                         {.band = out_ch.band, .number = 0});
  }

  {
    // Try a 40- MHz channel in the 2.4 GHz band (SB_U => secondary below)
    in_ch = {.chnum = 9, .band = BRCMU_CHAN_BAND_2G, .bw = BRCMU_CHAN_BW_40, .sb = BRCMU_CHAN_SB_U};
    fuchsia_wlan_ieee80211::wire::ChannelNumber out_ch = {.band = WlanBand::kTwoGhz, .number = 9};
    verify_chanspec_to_operating_channel(in_ch, out_ch, ChannelBandwidth::kCbw40Below,
                                         {.band = out_ch.band, .number = 0});
  }

  {
    // Try a 40+ MHz channel in the 5 GHz band (SB_L => secondary above)
    in_ch = {
        .chnum = 46, .band = BRCMU_CHAN_BAND_5G, .bw = BRCMU_CHAN_BW_40, .sb = BRCMU_CHAN_SB_L};
    fuchsia_wlan_ieee80211::wire::ChannelNumber out_ch = {.band = WlanBand::kFiveGhz, .number = 46};
    verify_chanspec_to_operating_channel(in_ch, out_ch, ChannelBandwidth::kCbw40,
                                         {.band = out_ch.band, .number = 0});
  }

  {
    // Try a 40- MHz channel in the 5 GHz band (SB_U => secondary below)
    in_ch = {
        .chnum = 112, .band = BRCMU_CHAN_BAND_5G, .bw = BRCMU_CHAN_BW_40, .sb = BRCMU_CHAN_SB_U};
    fuchsia_wlan_ieee80211::wire::ChannelNumber out_ch = {.band = WlanBand::kFiveGhz,
                                                          .number = 112};
    verify_chanspec_to_operating_channel(in_ch, out_ch, ChannelBandwidth::kCbw40Below,
                                         {.band = out_ch.band, .number = 0});
  }
}

TEST(ChannelConversion, ChanspecToPrimaryChannel) {
  brcmu_chan in_ch;
  using fuchsia_wlan_ieee80211::wire::WlanBand;

  {
    // Try a simple 20 MHz channel in the 2.4 GHz band
    in_ch = {
        .chnum = 11, .band = BRCMU_CHAN_BAND_2G, .bw = BRCMU_CHAN_BW_20, .sb = BRCMU_CHAN_SB_NONE};
    fuchsia_wlan_ieee80211::wire::ChannelNumber out_ch = {.band = WlanBand::kTwoGhz, .number = 11};
    verify_chanspec_to_primary_channel(in_ch, out_ch);
  }

  {
    // Try a 40+ MHz channel in the 5 GHz band (center 46, SB Upper => control 48)
    in_ch = {
        .chnum = 46, .band = BRCMU_CHAN_BAND_5G, .bw = BRCMU_CHAN_BW_40, .sb = BRCMU_CHAN_SB_U};
    fuchsia_wlan_ieee80211::wire::ChannelNumber out_ch = {.band = WlanBand::kFiveGhz, .number = 48};
    verify_chanspec_to_primary_channel(in_ch, out_ch);
  }

  {
    // Try a 40- MHz channel in the 5 GHz band (center 110, SB Lower => control 108)
    in_ch = {
        .chnum = 110, .band = BRCMU_CHAN_BAND_5G, .bw = BRCMU_CHAN_BW_40, .sb = BRCMU_CHAN_SB_L};
    fuchsia_wlan_ieee80211::wire::ChannelNumber out_ch = {.band = WlanBand::kFiveGhz,
                                                          .number = 108};
    verify_chanspec_to_primary_channel(in_ch, out_ch);
  }
}

TEST(ChannelConversion, Override80P80) {
  const fuchsia_wlan_ieee80211::wire::ChannelNumber expected_primary = {
      .band = fuchsia_wlan_ieee80211::wire::WlanBand::kFiveGhz, .number = 36};
  using fuchsia_wlan_ieee80211::wire::ChannelBandwidth;

  const auto out_cbw = enforce_bandwidth_limitations(expected_primary, ChannelBandwidth::kCbw80P80);
  // Override should only change the bandwidth.
  EXPECT_EQ(out_cbw, ChannelBandwidth::kCbw20);
}

TEST(ChannelConversion, Override80P80IgnoresOtherBandwidths) {
  using fuchsia_wlan_ieee80211::wire::ChannelBandwidth;
  const std::array<ChannelBandwidth, 4> bandwidths{
      ChannelBandwidth::kCbw20, ChannelBandwidth::kCbw40, ChannelBandwidth::kCbw80,
      ChannelBandwidth::kCbw160};
  for (const auto& bandwidth : bandwidths) {
    const auto out_cbw = enforce_bandwidth_limitations(
        fuchsia_wlan_ieee80211::wire::ChannelNumber{
            .band = fuchsia_wlan_ieee80211::wire::WlanBand::kFiveGhz, .number = 36},
        bandwidth);
    EXPECT_EQ(out_cbw, bandwidth);
  }
}

TEST(ChannelConversion, OverrideWideBandwidthForChannel165) {
  using fuchsia_wlan_ieee80211::wire::ChannelBandwidth;
  const std::array<ChannelBandwidth, 2> bandwidths{ChannelBandwidth::kCbw40,
                                                   ChannelBandwidth::kCbw80};

  for (const auto& bandwidth : bandwidths) {
    const auto out_cbw = enforce_bandwidth_limitations(
        fuchsia_wlan_ieee80211::wire::ChannelNumber{
            .band = fuchsia_wlan_ieee80211::wire::WlanBand::kFiveGhz, .number = 165},
        bandwidth);
    EXPECT_EQ(out_cbw, ChannelBandwidth::kCbw20);
  }
}

TEST(ChannelConversion, OverrideWideBandwidthForChannel173) {
  using fuchsia_wlan_ieee80211::wire::ChannelBandwidth;
  const auto out_cbw = enforce_bandwidth_limitations(
      fuchsia_wlan_ieee80211::wire::ChannelNumber{
          .band = fuchsia_wlan_ieee80211::wire::WlanBand::kFiveGhz, .number = 173},
      ChannelBandwidth::kCbw40);
  EXPECT_EQ(out_cbw, ChannelBandwidth::kCbw20);
}

static void verify_round_trip(const fuchsia_wlan_ieee80211::wire::ChannelNumber& in_channel,
                              fuchsia_wlan_ieee80211::wire::ChannelBandwidth in_cbw) {
  brcmu_d11inf d11_inf = {.io_type = BRCMU_D11AC_IOTYPE};
  brcmu_d11_attach(&d11_inf);

  uint16_t chanspec = channel_to_chanspec(&d11_inf, in_channel, in_cbw);
  auto actual_channel = chanspec_to_operating_channel_number(&d11_inf, chanspec);
  auto actual_cbw = chanspec_to_channel_bandwidth(&d11_inf, chanspec);

  EXPECT_EQ(actual_channel.number, in_channel.number)
      << "Channel number mismatch for channel " << static_cast<int>(in_channel.number);
  EXPECT_EQ(actual_channel.band, in_channel.band)
      << "Band mismatch for channel " << static_cast<int>(in_channel.number);
  EXPECT_EQ(actual_cbw, in_cbw) << "Bandwidth mismatch for channel "
                                << static_cast<int>(in_channel.number);
}

TEST(ChannelConversion, RoundTrip40MHz) {
  using fuchsia_wlan_ieee80211::wire::ChannelBandwidth;
  using fuchsia_wlan_ieee80211::wire::WlanBand;

  // 2.4 GHz 40+ MHz (Cbw40)
  // IEEE Std 802.11-2024 Table E-4 Operating Class 83
  for (uint8_t ch = 1; ch <= 9; ++ch) {
    verify_round_trip({.band = WlanBand::kTwoGhz, .number = ch}, ChannelBandwidth::kCbw40);
  }

  // 2.4 GHz 40- MHz (Cbw40Below)
  // IEEE Std 802.11-2024 Table E-4 Operating Class 84
  for (uint8_t ch = 5; ch <= 13; ++ch) {
    verify_round_trip({.band = WlanBand::kTwoGhz, .number = ch}, ChannelBandwidth::kCbw40Below);
  }

  // 5 GHz 40+ MHz (Cbw40)
  // IEEE Std 802.11-2024 Table E-4 Operating Class 116, 119, 122, 126
  const std::array<uint8_t, 14> five_ghz_40m_plus_channels = {36,  44,  52,  60,  100, 108, 116,
                                                              124, 132, 140, 149, 157, 165, 173};
  for (uint8_t ch : five_ghz_40m_plus_channels) {
    verify_round_trip({.band = WlanBand::kFiveGhz, .number = ch}, ChannelBandwidth::kCbw40);
  }

  // 5 GHz 40- MHz (Cbw40Below)
  // IEEE Std 802.11-2024 Table E-4 Operating Class 117, 120, 123, 127
  const std::array<uint8_t, 14> five_ghz_40m_minus_channels = {40,  48,  56,  64,  104, 112, 120,
                                                               128, 136, 144, 153, 161, 169, 177};
  for (uint8_t ch : five_ghz_40m_minus_channels) {
    verify_round_trip({.band = WlanBand::kFiveGhz, .number = ch}, ChannelBandwidth::kCbw40Below);
  }
}
}  // namespace
