# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.


# Data Rates
class SupportedRates:
    OFDM = [6000, 9000, 12000, 18000, 24000, 36000, 48000, 54000]
    CCK = [1000, 2000, 5500, 11000]
    CCK_AND_OFDM = CCK + OFDM


class BasicRates:
    OFDM_ONLY = [6000, 12000, 24000]
    CCK_AND_OFDM = [1000, 2000, 5500, 11000]


class VendorElements:
    CORRECT_LENGTH = "dd0411223301"
    ZERO_LENGTH_WITHOUT_DATA = "dd00"
