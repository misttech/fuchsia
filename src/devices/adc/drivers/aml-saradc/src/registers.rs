// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use mmio::{register, register_block};

register! {
    Reg0, u32, 0x00 << 2, RW, {
        pub val, set_val: 31, 0;
        pub bool, sampling_stop, set_sampling_stop: 14;
        pub fifo_cnt_irq, set_fifo_cnt_irq: 8, 4;
        pub bool, fifo_irq_en, set_fifo_irq_en: 3;
        pub bool, sampling_start, set_sampling_start: 2;
        pub bool, sampling_enable, set_sampling_enable: 0;
    }
}

register! {
    ChanList, u32, 0x01 << 2, RW, {
        pub val, set_val: 31, 0;
    }
}

register! {
    AvgCntl, u32, 0x02 << 2, RW, {
        pub val, set_val: 31, 0;
    }
}

register! {
    Reg3, u32, 0x03 << 2, RW, {
        pub val, set_val: 31, 0;
        pub bool, adc_en, set_adc_en: 21;
    }
}

register! {
    Delay, u32, 0x04 << 2, RW, {
        pub val, set_val: 31, 0;
    }
}

register! {
    FifoRd, u32, 0x06 << 2, RO, {
        pub val, _: 31, 0;
    }
}

register! {
    AuxSw, u32, 0x07 << 2, RW, {
        pub val, set_val: 31, 0;
    }
}

register! {
    Chan10Sw, u32, 0x08 << 2, RW, {
        pub val, set_val: 31, 0;
    }
}

register! {
    DetectIdleSw, u32, 0x09 << 2, RW, {
        pub val, set_val: 31, 0;
    }
}

register! {
    Reg11, u32, 0x0b << 2, RW, {
        pub val, set_val: 31, 0;
        pub bool, ts_vbg_en, set_ts_vbg_en: 13;
        pub bool, rsv6, set_rsv6: 6;
        pub bool, rsv5, set_rsv5: 5;
        pub bool, rsv1, set_rsv1: 1;
    }
}

register! {
    Reg13, u32, 0x0d << 2, RW, {
        pub val, set_val: 31, 0;
    }
}

register_block! {
    pub struct AdcRegsBlock<M> {
        pub reg0: Reg0,
        pub chan_list: ChanList,
        pub avg_cntl: AvgCntl,
        pub reg3: Reg3,
        pub delay: Delay,
        pub fifo_rd: FifoRd,
        pub aux_sw: AuxSw,
        pub chan_10_sw: Chan10Sw,
        pub detect_idle_sw: DetectIdleSw,
        pub reg11: Reg11,
        pub reg13: Reg13,
    }
}

register! {
    AoSarClk, u32, 0x24 << 2, RW, {
        pub val, set_val: 31, 0;
        pub bool, clk_ena, set_clk_ena: 8;
        pub clk_src, set_clk_src: 10, 9;
        pub clk_div, set_clk_div: 7, 0;
    }
}

register_block! {
    pub struct AoRegsBlock<M> {
        pub ao_sar_clk: AoSarClk,
    }
}
