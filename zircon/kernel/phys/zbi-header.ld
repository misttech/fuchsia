/* Copyright 2020 The Fuchsia Authors
 *
 * Use of this source code is governed by a MIT-style
 * license that can be found in the LICENSE file or at
 * https://opensource.org/licenses/MIT
 */

/*
 * This provides the zbi_kernel_t (see <lib/zbi-format/zbi.h>) payload header
 * for ZBI executables, inserted immediately after the ZBI container and item
 * headers zbi-header.S generates.  For BFD ld, this file must be listed in a
 * -T switch *before* the -T switch for phys.ld so that the `INSERT` clause
 * can be "pending" when the main SECTIONS clause is read.
 */

SECTIONS {
  .boot.header.zbi_kernel_t : ALIGN(8) {
    /*
     * Two words: the entry point offset, and the bss size.
     * PHYS_LOAD_ADDRESS is zero for ZBI executables, so the entry point
     * is an offset from the actual load address.
     */
    QUAD(_start - __executable_start + PHYS_LOAD_ADDRESS);
    QUAD(PHYS_RESERVE_MEMORY_SIZE);
  }
} INSERT AFTER .boot.header
