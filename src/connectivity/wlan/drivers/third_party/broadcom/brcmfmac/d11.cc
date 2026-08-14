/*
 * Copyright (c) 2013 Broadcom Corporation
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
/*********************channel spec common functions*********************/

#include <fidl/fuchsia.wlan.common/cpp/wire.h>
#include <fidl/fuchsia.wlan.ieee80211/cpp/fidl.h>
#include <fidl/fuchsia.wlan.ieee80211/cpp/wire.h>
#include <zircon/assert.h>

#include <third_party/bcmdhd/crossdriver/bcmwifi_channels.h>

#include "src/connectivity/wlan/drivers/third_party/broadcom/brcmfmac/brcmu_d11.h"
#include "src/connectivity/wlan/drivers/third_party/broadcom/brcmfmac/brcmu_utils.h"
#include "src/connectivity/wlan/drivers/third_party/broadcom/brcmfmac/brcmu_wifi.h"
#include "src/connectivity/wlan/drivers/third_party/broadcom/brcmfmac/debug.h"
#include "src/connectivity/wlan/drivers/third_party/broadcom/brcmfmac/linuxisms.h"

static uint16_t d11n_sb(enum brcmu_chan_sb sb) {
  switch (sb) {
    case BRCMU_CHAN_SB_NONE:
      return BRCMU_CHSPEC_D11N_SB_N;
    case BRCMU_CHAN_SB_L:
      return BRCMU_CHSPEC_D11N_SB_L;
    case BRCMU_CHAN_SB_U:
      return BRCMU_CHSPEC_D11N_SB_U;
    default:
      WARN_ON(1);
  }
  return 0;
}

static uint16_t d11n_bw(enum brcmu_chan_bw bw) {
  switch (bw) {
    case BRCMU_CHAN_BW_20:
      return BRCMU_CHSPEC_D11N_BW_20;
    case BRCMU_CHAN_BW_40:
      return BRCMU_CHSPEC_D11N_BW_40;
    default:
      WARN_ON(1);
  }
  return 0;
}

static void brcmu_d11n_encchspec(struct brcmu_chan* ch) {
  if (ch->bw == BRCMU_CHAN_BW_20) {
    ch->sb = BRCMU_CHAN_SB_NONE;
  }

  ch->chspec = 0;
  brcmu_maskset16(&ch->chspec, BRCMU_CHSPEC_CH_MASK, BRCMU_CHSPEC_CH_SHIFT, ch->chnum);
  brcmu_maskset16(&ch->chspec, BRCMU_CHSPEC_D11N_SB_MASK, 0, d11n_sb(ch->sb));
  brcmu_maskset16(&ch->chspec, BRCMU_CHSPEC_D11N_BW_MASK, 0, d11n_bw(ch->bw));

  if (ch->chnum <= CH_MAX_2G_CHANNEL) {
    ch->chspec |= BRCMU_CHSPEC_D11N_BND_2G;
  } else {
    ch->chspec |= BRCMU_CHSPEC_D11N_BND_5G;
  }
}

static uint16_t d11ac_bw(enum brcmu_chan_bw bw) {
  switch (bw) {
    case BRCMU_CHAN_BW_20:
      return BRCMU_CHSPEC_D11AC_BW_20;
    case BRCMU_CHAN_BW_40:
      return BRCMU_CHSPEC_D11AC_BW_40;
    case BRCMU_CHAN_BW_80:
      return BRCMU_CHSPEC_D11AC_BW_80;
    default:
      WARN_ON(1);
  }
  return 0;
}

static void brcmu_d11ac_encchspec(struct brcmu_chan* ch) {
  if (ch->bw == BRCMU_CHAN_BW_20 || ch->sb == BRCMU_CHAN_SB_NONE) {
    ch->sb = BRCMU_CHAN_SB_L;
  }

  brcmu_maskset16(&ch->chspec, BRCMU_CHSPEC_CH_MASK, BRCMU_CHSPEC_CH_SHIFT, ch->chnum);
  brcmu_maskset16(&ch->chspec, BRCMU_CHSPEC_D11AC_SB_MASK, BRCMU_CHSPEC_D11AC_SB_SHIFT, ch->sb);
  brcmu_maskset16(&ch->chspec, BRCMU_CHSPEC_D11AC_BW_MASK, 0, d11ac_bw(ch->bw));

  ch->chspec &= ~BRCMU_CHSPEC_D11AC_BND_MASK;
  if (ch->chnum <= CH_MAX_2G_CHANNEL) {
    ch->chspec |= BRCMU_CHSPEC_D11AC_BND_2G;
  } else {
    ch->chspec |= BRCMU_CHSPEC_D11AC_BND_5G;
  }
}

static void brcmu_d11n_decchspec(struct brcmu_chan* ch) {
  uint16_t val;

  ch->chnum = (uint8_t)(ch->chspec & BRCMU_CHSPEC_CH_MASK);
  ch->control_ch_num = ch->chnum;

  switch (ch->chspec & BRCMU_CHSPEC_D11N_BW_MASK) {
    case BRCMU_CHSPEC_D11N_BW_20:
      ch->bw = BRCMU_CHAN_BW_20;
      ch->sb = BRCMU_CHAN_SB_NONE;
      break;
    case BRCMU_CHSPEC_D11N_BW_40:
      ch->bw = BRCMU_CHAN_BW_40;
      val = ch->chspec & BRCMU_CHSPEC_D11N_SB_MASK;
      if (val == BRCMU_CHSPEC_D11N_SB_L) {
        ch->sb = BRCMU_CHAN_SB_L;
        ch->control_ch_num -= CH_10MHZ_APART;
      } else {
        ch->sb = BRCMU_CHAN_SB_U;
        ch->control_ch_num += CH_10MHZ_APART;
      }
      break;
    default:
      WARN_ON_ONCE(1);
      break;
  }

  switch (ch->chspec & BRCMU_CHSPEC_D11N_BND_MASK) {
    case BRCMU_CHSPEC_D11N_BND_5G:
      ch->band = BRCMU_CHAN_BAND_5G;
      break;
    case BRCMU_CHSPEC_D11N_BND_2G:
      ch->band = BRCMU_CHAN_BAND_2G;
      break;
    default:
      WARN_ON_ONCE(1);
      break;
  }
}

static void brcmu_d11ac_decchspec(struct brcmu_chan* ch) {
  uint16_t val;

  ch->chnum = (uint8_t)(ch->chspec & BRCMU_CHSPEC_CH_MASK);
  ch->control_ch_num = ch->chnum;

  switch (ch->chspec & BRCMU_CHSPEC_D11AC_BW_MASK) {
    case BRCMU_CHSPEC_D11AC_BW_20:
      ch->bw = BRCMU_CHAN_BW_20;
      ch->sb = BRCMU_CHAN_SB_NONE;
      break;
    case BRCMU_CHSPEC_D11AC_BW_40:
      ch->bw = BRCMU_CHAN_BW_40;
      val = ch->chspec & BRCMU_CHSPEC_D11AC_SB_MASK;
      if (val == BRCMU_CHSPEC_D11AC_SB_L) {
        ch->sb = BRCMU_CHAN_SB_L;
        ch->control_ch_num -= CH_10MHZ_APART;
      } else if (val == BRCMU_CHSPEC_D11AC_SB_U) {
        ch->sb = BRCMU_CHAN_SB_U;
        ch->control_ch_num += CH_10MHZ_APART;
      } else {
        WARN_ON_ONCE(1);
      }
      break;
    case BRCMU_CHSPEC_D11AC_BW_80:
      ch->bw = BRCMU_CHAN_BW_80;
      ch->sb = static_cast<brcmu_chan_sb>(
          brcmu_maskget16(ch->chspec, BRCMU_CHSPEC_D11AC_SB_MASK, BRCMU_CHSPEC_D11AC_SB_SHIFT));
      switch (ch->sb) {
        case BRCMU_CHAN_SB_LL:
          ch->control_ch_num -= CH_30MHZ_APART;
          break;
        case BRCMU_CHAN_SB_LU:
          ch->control_ch_num -= CH_10MHZ_APART;
          break;
        case BRCMU_CHAN_SB_UL:
          ch->control_ch_num += CH_10MHZ_APART;
          break;
        case BRCMU_CHAN_SB_UU:
          ch->control_ch_num += CH_30MHZ_APART;
          break;
        default:
          WARN_ON_ONCE(1);
          break;
      }
      break;
    case BRCMU_CHSPEC_D11AC_BW_8080:
    case BRCMU_CHSPEC_D11AC_BW_160:
    default:
      WARN_ON_ONCE(1);
      break;
  }

  switch (ch->chspec & BRCMU_CHSPEC_D11AC_BND_MASK) {
    case BRCMU_CHSPEC_D11AC_BND_5G:
      ch->band = BRCMU_CHAN_BAND_5G;
      break;
    case BRCMU_CHSPEC_D11AC_BND_2G:
      ch->band = BRCMU_CHAN_BAND_2G;
      break;
    default:
      WARN_ON_ONCE(1);
      break;
  }
}

uint16_t channel_to_chanspec(const brcmu_d11inf* d11inf,
                             const fuchsia_wlan_ieee80211::ChannelNumber& channel,
                             fuchsia_wlan_ieee80211::ChannelBandwidth cbw) {
  struct brcmu_chan ch_inf = {.chnum = channel.number()};

  switch (cbw) {
    case fuchsia_wlan_ieee80211::ChannelBandwidth::kCbw20:
      ch_inf.bw = BRCMU_CHAN_BW_20;
      ch_inf.sb = BRCMU_CHAN_SB_NONE;
      break;
    case fuchsia_wlan_ieee80211::ChannelBandwidth::kCbw40:
      ch_inf.bw = BRCMU_CHAN_BW_40;
      ch_inf.sb = BRCMU_CHAN_SB_L;
      break;
    case fuchsia_wlan_ieee80211::ChannelBandwidth::kCbw40Below:
      ch_inf.bw = BRCMU_CHAN_BW_40;
      ch_inf.sb = BRCMU_CHAN_SB_U;
      break;
    case fuchsia_wlan_ieee80211::ChannelBandwidth::kCbw80:
    case fuchsia_wlan_ieee80211::ChannelBandwidth::kCbw160:
    case fuchsia_wlan_ieee80211::ChannelBandwidth::kCbw80P80:
    default:
      BRCMF_ERR("unsupported channel width: %u", static_cast<uint32_t>(cbw));
      break;
  }

  // ch_info.band is handled by encchspec
  d11inf->encchspec(&ch_inf);

  return ch_inf.chspec;
}

uint16_t channel_to_chanspec(const brcmu_d11inf* d11inf,
                             const fuchsia_wlan_ieee80211::wire::ChannelNumber& channel,
                             fuchsia_wlan_ieee80211::wire::ChannelBandwidth cbw) {
  struct brcmu_chan ch_inf = {.chnum = channel.number};

  switch (cbw) {
    case fuchsia_wlan_ieee80211::wire::ChannelBandwidth::kCbw20:
      ch_inf.bw = BRCMU_CHAN_BW_20;
      ch_inf.sb = BRCMU_CHAN_SB_NONE;
      break;
    case fuchsia_wlan_ieee80211::wire::ChannelBandwidth::kCbw40:
      ch_inf.bw = BRCMU_CHAN_BW_40;
      ch_inf.sb = BRCMU_CHAN_SB_L;
      break;
    case fuchsia_wlan_ieee80211::wire::ChannelBandwidth::kCbw40Below:
      ch_inf.bw = BRCMU_CHAN_BW_40;
      ch_inf.sb = BRCMU_CHAN_SB_U;
      break;
    case fuchsia_wlan_ieee80211::wire::ChannelBandwidth::kCbw80:
    case fuchsia_wlan_ieee80211::wire::ChannelBandwidth::kCbw160:
    case fuchsia_wlan_ieee80211::wire::ChannelBandwidth::kCbw80P80:
    default:
      BRCMF_ERR("unsupported channel width: %u", static_cast<uint32_t>(cbw));
      break;
  }

  // ch_info.band is handled by encchspec
  d11inf->encchspec(&ch_inf);

  return ch_inf.chspec;
}

fuchsia_wlan_ieee80211::wire::ChannelNumber chanspec_to_operating_channel_number(
    const brcmu_d11inf* d11_inf, uint16_t chanspec) {
  brcmu_chan ch_inf = {.chspec = chanspec};
  d11_inf->decchspec(&ch_inf);
  fuchsia_wlan_ieee80211::wire::WlanBand band =
      ch_inf.band == BRCMU_CHAN_BAND_2G ? fuchsia_wlan_ieee80211::wire::WlanBand::kTwoGhz
                                        : fuchsia_wlan_ieee80211::wire::WlanBand::kFiveGhz;
  return {.band = band, .number = ch_inf.chnum};
}

fuchsia_wlan_ieee80211::wire::ChannelNumber chanspec_to_primary_channel_number(
    const brcmu_d11inf* d11_inf, uint16_t chanspec) {
  brcmu_chan ch_inf = {.chspec = chanspec};
  d11_inf->decchspec(&ch_inf);
  fuchsia_wlan_ieee80211::wire::WlanBand band =
      ch_inf.band == BRCMU_CHAN_BAND_2G ? fuchsia_wlan_ieee80211::wire::WlanBand::kTwoGhz
                                        : fuchsia_wlan_ieee80211::wire::WlanBand::kFiveGhz;
  uint8_t ctl_chan = 0;
  zx_status_t status = chspec_ctlchan(chanspec, &ctl_chan);
  if (status != ZX_OK) {
    BRCMF_ERR("Failed to get control channel from chanspec: 0x%x status: %d", chanspec, status);
  }
  return {.band = band, .number = ctl_chan};
}

fuchsia_wlan_ieee80211::wire::ChannelBandwidth chanspec_to_channel_bandwidth(
    const brcmu_d11inf* d11_inf, uint16_t chanspec) {
  brcmu_chan ch_inf = {.chspec = chanspec};
  d11_inf->decchspec(&ch_inf);

  switch (ch_inf.bw) {
    case BRCMU_CHAN_BW_20:
      return fuchsia_wlan_ieee80211::wire::ChannelBandwidth::kCbw20;
    case BRCMU_CHAN_BW_40:
      switch (ch_inf.sb) {
        // These macros describe whether the PRIMARY (or in brcmfmac parlance "control") channel is
        // above or below the side band.  If the primary channel is the upper, then the side band is
        // lower (eg: 40-).  And if the primary channel is the lower, then the side band is upper
        // (eg: 36+).
        case BRCMU_CHAN_SB_U:
          return fuchsia_wlan_ieee80211::wire::ChannelBandwidth::kCbw40Below;
        case BRCMU_CHAN_SB_L:
          return fuchsia_wlan_ieee80211::wire::ChannelBandwidth::kCbw40;
        default:
          BRCMF_ERR("unsupported channel side band: %hhu", static_cast<uint8_t>(ch_inf.sb));
          return fuchsia_wlan_ieee80211::wire::ChannelBandwidth::kCbw20;
      }
    case BRCMU_CHAN_BW_80:
      return fuchsia_wlan_ieee80211::wire::ChannelBandwidth::kCbw80;
    default:
      BRCMF_ERR("unsupported channel width: %u", ch_inf.bw);
      return fuchsia_wlan_ieee80211::wire::ChannelBandwidth::kCbw20;
  }
}

fuchsia_wlan_ieee80211::wire::ChannelNumber chanspec_to_secondary80(const brcmu_d11inf* d11_inf,
                                                                    uint16_t chanspec) {
  brcmu_chan ch_inf = {.chspec = chanspec};
  d11_inf->decchspec(&ch_inf);
  fuchsia_wlan_ieee80211::wire::WlanBand band =
      ch_inf.band == BRCMU_CHAN_BAND_2G ? fuchsia_wlan_ieee80211::wire::WlanBand::kTwoGhz
                                        : fuchsia_wlan_ieee80211::wire::WlanBand::kFiveGhz;
  return {.band = band, .number = 0};
}

void brcmu_d11_attach(struct brcmu_d11inf* d11inf) {
  if (d11inf->io_type == BRCMU_D11N_IOTYPE) {
    d11inf->encchspec = brcmu_d11n_encchspec;
    d11inf->decchspec = brcmu_d11n_decchspec;
  } else {
    d11inf->encchspec = brcmu_d11ac_encchspec;
    d11inf->decchspec = brcmu_d11ac_decchspec;
  }
}

fuchsia_wlan_ieee80211::wire::ChannelBandwidth enforce_bandwidth_limitations(
    const fuchsia_wlan_ieee80211::wire::ChannelNumber& primary,
    fuchsia_wlan_ieee80211::wire::ChannelBandwidth cbw) {
  using fuchsia_wlan_ieee80211::wire::ChannelBandwidth;
  if (cbw == ChannelBandwidth::kCbw80P80) {
    // Override the channel bandwidth with 20Mhz because `channel2chanspec` doesn't support
    // encoding 80+80 Mhz, and we have always overridden to 20Mhz in this case.
    // TODO(https://fxbug.dev/42144507) - Remove this override.
    return ChannelBandwidth::kCbw20;
  }

  // Connecting to channels >= 165 with bandwidths > 20MHz is not supported per fxrev.dev/1446009.
  if (primary.band == fuchsia_wlan_ieee80211::wire::WlanBand::kFiveGhz && primary.number >= 165 &&
      cbw != ChannelBandwidth::kCbw20) {
    return ChannelBandwidth::kCbw20;
  }

  return cbw;
}

fuchsia_wlan_ieee80211::ChannelBandwidth enforce_bandwidth_limitations(
    const fuchsia_wlan_ieee80211::ChannelNumber& primary,
    fuchsia_wlan_ieee80211::ChannelBandwidth cbw) {
  fidl::Arena arena;
  return enforce_bandwidth_limitations(fidl::ToWire(arena, primary), cbw);
}
