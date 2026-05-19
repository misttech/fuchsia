// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use mmio::{register, register_block};

register! {
    CtrlR0, u32, 0x00, RW, {
        pub spi_frf, set_spi_frf: 22, 21;
        pub dfs_32, set_dfs_32: 20, 16;
        pub cfs, set_cfs: 15, 12;
        pub srl, set_srl: 11;
        pub slv_oe, set_slv_oe: 10;
        pub tmod, set_tmod: 9, 8;
        pub scpol, set_scpol: 7;
        pub scph, set_scph: 6;
        pub frf, set_frf: 5, 4;
        pub dfs, set_dfs: 3, 0;
    }
}

register! {
    CtrlR1, u32, 0x04, RW, {
        pub ndf, set_ndf: 15, 0;
    }
}

register! {
    SsiEnr, u32, 0x08, RW, {
        pub ssi_en, set_ssi_en: 0;
    }
}

register! {
    Ser, u32, 0x10, RW, {
        // Bits are target select lines, up to 16.
        pub ser, set_ser: 15, 0;
    }
}

register! {
    Baudr, u32, 0x14, RW, {
        pub sckdv, set_sckdv: 15, 0;
    }
}

register! {
    Sr, u32, 0x28, RO, {
        pub dcol, _: 6;
        pub txe, _: 5;
        pub rff, _: 4;
        pub rfne, _: 3;
        pub tfe, _: 2;
        pub tfnf, _: 1;
        pub busy, _: 0;
    }
}

register! {
    Imr, u32, 0x2c, RW, {
        pub mstim, set_mstim: 5;
        pub rxfim, set_rxfim: 4;
        pub rxoim, set_rxoim: 3;
        pub rxuim, set_rxuim: 2;
        pub txoim, set_txoim: 1;
        pub txeim, set_txeim: 0;
    }
}

register_block! {
    pub struct DwSpiRegsBlock<M> {
        pub ctrlr0: CtrlR0,
        pub ctrlr1: CtrlR1,
        pub ssi_enr: SsiEnr,
        pub ser: Ser,
        pub baudr: Baudr,
        pub sr: Sr,
        pub imr: Imr,
    }
}
