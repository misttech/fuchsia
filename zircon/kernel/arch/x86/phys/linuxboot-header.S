// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/arch/asm.h>
#include <lib/arch/x86/cpuid-asm.h>
#include <lib/arch/x86/descriptor-asm.h>
#include <lib/arch/x86/msr.h>
#include <lib/arch/x86/system-asm.h>

#include "linuxboot-asm.h"

// This heads a position-independent 64-bit phys executable to make it a
// Linux/x86 bzImage.  All three Linux/x86 boot protocols land in start.S: the
// 64-bit protocol directly, and the 16-bit and 32-bit protocols by way of the
// 32-bit entry point here, which enters 64-bit mode itself.

// Selectors into gLinuxbootGdt, defined below.
#define LINUXBOOT_CODE32_SEL (1 << 3)
#define LINUXBOOT_DATA32_SEL (2 << 3)
#define LINUXBOOT_CODE64_SEL (3 << 3)
#define LINUXBOOT_GDT_SIZE (4 * 8)

// The phys.ld linker script places this first in the image.  The linuxboot.ld
// linker script arranges that this means it is "before" the actual load
// address of the bzImage 32-bit code (which includes the 32-bit entry point).
.section .boot.header, "awx", %progbits

// This is the putative boot_params object at the start of the image.
// See linuxboot.h for full details about the protocol.  In fact, this
// just corresponds to the boot_params layout for purposes of locating
// the boot_params::setup_header, below.
.org 0
.label zero_page

// This is not used by the Linux/x86 boot protocol.  Historically,
// this format could be used directly as a Master Boot Record image.
// This ancient protocol consists of simply loading the first sector
// into a well-known location in memory and jumping to it in 16-bit
// real mode.  This is not supported by Linux kernels any more, but a
// legacy stub is provided to panic nicely if loaded in the original
// x86 PC way by firmware supporting the old 16-bit x86 BIOS ABI.
.code16
.function MbrEntry, nosection=nosection
  // This is the traditional physical load address.  Make sure
  // it's the exact start of the %cs segment.
  ljmp $0x07c0, $(0f - MbrEntry)
0:

  // Reset segment registers to be safe and don't trust any stack pointer.
  mov %cs, %ax
  mov %ax, %ds
  mov %ax, %es
  mov %ax, %ss
  mov %ax, %ss
  xor %sp, %sp

  // Normalize flags that might affect the BIOS or instructions used here.
  sti
  cld

  // cf https://en.wikipedia.org/wiki/BIOS_interrupt_call
  mov $(MbrEntryMessage - MbrEntry), %si
  mov $MbrEntryMessageSize, %cx
.Lbios_write_char:
  lodsb         // %ax = *%si++
  mov $0xe, %ah // Write character in TTY mode.
  mov $0xd, %bx // Color (light magenta).
  int $0x10     // https://en.wikipedia.org/wiki/INT_10H
  loop .Lbios_write_char

.Lbios_wait_for_key:
  xor %ax, %ax
  int $0x16

.Lbios_reboot:
  int $0x19             // Ask the BIOS to reboot.  Should not return.
  ljmp $0xf000, $0xfff0 // If it does, jump directly to the BIOS entry point.
.end_function

.object MbrEntryMessage, nosection=nosection
  .ascii "Legacy x86 MBR booting is not supported.\r\n"
  .ascii "Install a boot loader that supports Linux/x86 bzImage format.\r\n"
  .ascii "Hit any key to reboot...\r\n"
  MbrEntryMessageSize = . - MbrEntryMessage
.end_object

// The space before boot_params::hdr is not examined by the boot loader.  A
// boot loader using the 16-bit entry protocol reads hdr.setup_sects and then
// loads this whole file's contents into memory at zero_page before entering
// at setup_header::jump, below.
.org BOOT_PARAMS_HDR

.object setup_header, nosection=nosection
  // This tells the boot loader how many 512-byte sectors after this first
  // one to load.  LINUXBOOT_SETUP_SIZE is defined at the end of the file.
  .org BOOT_PARAMS_HDR + SETUP_HEADER_SETUP_SECTS
  .byte ((LINUXBOOT_SETUP_SIZE >> 9) - 1)

  // This tells the boot loader how many 16-byte units (Intelspeak paragraphs)
  // to load at the load address for the 32-bit kernel image.  The linuxboot.ld
  // linker script calculates the value of LINUXBOOT_SYSSIZE.
  .org BOOT_PARAMS_HDR + SETUP_HEADER_SYSSIZE

  // LLD would allow `.int LINUXBOOT_SYSSIZE` directly even in this
  // position-independent link, but GNU (BFD) ld does not.  It will only allow a
  // PC-relative relocation.  So linuxboot.ld also computes
  // LINUXBOOT_SYSSIZE_RELATIVE as the difference between the LINUXBOOT_SYSSIZE
  // value and the link-time load address (PHYS_LOAD_ADDRESS, aka the zero_page
  // label above); it's not a meaningful address since it's scaled by 16, but
  // it's the right addend for a PC-relative relocation to resolve to exactly
  // LINUXBOOT_SYSSIZE.
  .int LINUXBOOT_SYSSIZE_RELATIVE - zero_page

  // This is a required magic number the boot loader checks for.
  .org BOOT_PARAMS_HDR + SETUP_HEADER_BOOT_FLAG
  .short LINUXBOOT_BOOT_FLAG

  // This is offset 512, where the 16-bit entry point is.  Since there's a
  // required header field right after, this must be a two-byte instruction.
  // So we make it a jump to past the end of the fixed header layout, where
  // we define the real-mode 16-bit entry path.
  .org BOOT_PARAMS_HDR + SETUP_HEADER_JUMP
  // This must be a 1-byte displacement to make a 2-byte instruction that
  // fits before the next field.  That makes a maximum jump distance of 127,
  // to 0x202 + 127 = 0x281.
  jmp Phys16Entry

  // This is a required magic number the boot loader checks for.
  .org BOOT_PARAMS_HDR + SETUP_HEADER_HEADER
  .ascii "HdrS"

  // This tells the boot loader which precise version of the protocol for
  // encoding bits in these headers this kernel image is compatible with.
  // Version 2.12 is not the newest, but none of the Zircon shim code needs
  // to take advantange of any of the newer features.
  .org BOOT_PARAMS_HDR + SETUP_HEADER_VERSION
  .short 0x020c

  // This indicates "bzImage" format: load the 32-bit code at 1MiB.
  .org BOOT_PARAMS_HDR + SETUP_HEADER_LOADFLAGS
  .byte LOADFLAGS_LOADED_HIGH

  // This is the address of the 32-bit entry point, which is the start of the
  // load image when loaded at the default "bzImage" address.  A boot loader
  // that relocates the kernel is expected to update this field before using
  // the 16-bit entry path, which jumps to the address found here.
  .org BOOT_PARAMS_HDR + SETUP_HEADER_CODE32_START
  .int LINUXBOOT_LOADED_HIGH_ADDRESS

  // This gives the boot loader license to place the initrd RAMDISK image
  // anywhere in the 4G address space.  (This will probably be ignored by a
  // boot loader using the 64-bit boot protocol, where it can be above 4GiB.)
  .org BOOT_PARAMS_HDR + SETUP_HEADER_INITRD_ADDR_MAX
  .int 0xffffffff

  // Preferred alignment for the load address is just 4KiB.
  .org BOOT_PARAMS_HDR + SETUP_HEADER_KERNEL_ALIGNMENT
  .int 0x1000

  // The link is position-independent, so any (aligned) load address is fine.
  // The 64-bit protocol stipulates relocatability regardless, but this lets
  // the 16-bit and 32-bit protocols relocate the kernel too.
  .org BOOT_PARAMS_HDR + SETUP_HEADER_RELOCATABLE_KERNEL
  .byte 1

  // Load addresses must be aligned to 4KiB (1 << 12).
  .org BOOT_PARAMS_HDR + SETUP_HEADER_MIN_ALIGNMENT
  .byte 12

  // This indicates a 64-bit kernel that can both use the 64-bit protocol and
  // accept all addresses of interest being past the 4GiB barrier.  The 16-bit
  // and 32-bit protocols remain available regardless.
  .org BOOT_PARAMS_HDR + SETUP_HEADER_XLOADFLAGS
  .short XLF_KERNEL_64 | XLF_CAN_BE_LOADED_ABOVE_4G

   // This constrains the maximum size of kernel command line data passed.
  .org BOOT_PARAMS_HDR + SETUP_HEADER_CMDLINE_SIZE
  .int 0xffffffff

  // This tells the boot loader how much space to leave from the start of the
  // kernel's load image.  This ensures it leaves space for the .bss and not
  // just the image it copies in.
  .org BOOT_PARAMS_HDR + SETUP_HEADER_INIT_SIZE
  .int LINUXBOOT_INIT_SIZE

   // The rest of setup_header contains fields a boot loader might examine
   // or modify though they're not meaningful to the version of the protocol
   // we support if they have zero-initialized values.
  .org BOOT_PARAMS_HDR + SIZEOF_SETUP_HEADER
.end_object

// This is close to the farthest the two-byte jump above can reach.
// The boot loader enters at offset 512, which is that short jump here.
.function Phys16Entry, nosection=nosection
  // Normalize flags that might affect the BIOS or instructions used here.
  sti
  cld

  // To be safe, only trust %ds to have been set by the boot loader.
  mov %ds, %ax
  mov %ax, %es

  // Copy the setup_header, as modified by the boot loader, into the
  // boot_params struct to be provided to the 32-bit entry path.
  mov $(boot_params + BOOT_PARAMS_HDR - zero_page), %di
  mov $(setup_header - zero_page), %si
  mov $SIZEOF_SETUP_HEADER, %cx
  rep movsb

  // boot_params::e820_table needs to be populated using the legacy BIOS ABI.
  xor %ebx, %ebx    // Cleared for first BIOS call.
  movb %bl, (boot_params + BOOT_PARAMS_E820_ENTRIES - zero_page)
.Le820_loop:
  // If you've been wondering where the name came from...
  mov $0xe820, %eax

  // %ebx is zero for the first iteration and is updated by the BIOS
  // to new, unspecified, nonzero values with each iteration.

  // This gives the buffer size: we read only one entry at a time.
  mov $SIZEOF_E820ENTRY, %ecx

  // This is a second magic number that must be set exactly.
  mov $E820_MAGIC, %edx

  // Use a single temporary buffer so as not to trust the BIOS to handle
  // an address advancing through the table properly.
  mov $(gE820Buffer - zero_page), %di

  // Call the BIOS.  The carry flag indicates the call failed.
  // Otherwise a few registers have results.
  int $0x15
  jc .Le820_end

  // Ignore the whole table if the BIOS is not following the protocol.
  cmp $E820_MAGIC, %eax
  je 0f
  movb $0, (boot_params + BOOT_PARAMS_E820_ENTRIES - zero_page)
  jmp .Le820_end
0:

  // Copy the entry.
  mov $SIZEOF_E820ENTRY, %cx
  mov $(gE820Buffer - zero_page), %si
  mov $(boot_params + BOOT_PARAMS_E820_TABLE - zero_page), %di
  movb (boot_params + BOOT_PARAMS_E820_ENTRIES - zero_page), %al
  mov %al, %dl
  mul %cl
  add %ax, %di
  rep movsb

  // Update the boot_params::e820_entries counter.
  inc %dl
  movb %dl, (boot_params + BOOT_PARAMS_E820_ENTRIES - zero_page)

  // Bail out if the table is full.
  cmpb $MAX_E820_TABLE_ENTRIES, %dl
  jle .Le820_end

  // The BIOS returns zero in %ebx when there are no more entries.
  test %ebx, %ebx
  jnz .Le820_loop
.Le820_end:

  // Make sure interrupts are disabled just in case.
  cli

  // Materialize the linear address of the code below, which will be the
  // new PC in the flat 32-bit code segment.
  mov %ds, %eax
  shl $4, %eax
  add $(Protected32Entry - zero_page), %eax

  // Push the long return address that will be popped below.
  pushl $LINUXBOOT_CODE32_SEL
  .cfi_adjust_cfa_offset 4
  pushl %eax
  .cfi_adjust_cfa_offset 4

  // Fetch the 32-bit entry point.  This is where the boot loader actually
  // placed the kernel's load image, which it records here if it relocated
  // the kernel from the default address set in the header above.
  movl (boot_params + BOOT_PARAMS_HDR + SETUP_HEADER_CODE32_START - zero_page), %ebx

  // Now point %esi at the boot_params being filled out here.
  // This is the argument register for the 32-bit entry path.
  mov %ds, %esi
  shl $4, %esi
  add $(boot_params - zero_page), %esi

  // Build a temporary descriptor pointing at the GDT to load it.  The GDT is
  // in the load image, whose runtime address is found relative to the 32-bit
  // entry point since the setup area here and the load image are placed
  // independently.
  lea (gLinuxbootGdt - LINUXBOOT_LOAD_ADDRESS)(%ebx), %eax
  movl %eax, -4(%esp)
  movw $(LINUXBOOT_GDT_SIZE - 1), -6(%esp)
  lgdtl -6(%esp)

  // Enable protected mode.  The Intel manual recommends doing a far jump
  // into a new %cs segment immediately after setting the PE bit.
  mov %cr0, %eax
  orb $X86_CR0_PE, %al
  mov %eax, %cr0

  // Immediately return into the new 32-bit code segment.
  lretl
.end_function

// Execution resumes here, now in 32-bit protected mode.
// This code still has to be purely position-independent.
.code32
.function Protected32Entry, align=16, nosection=nosection
  // Reload all the segment registers (except %cs).  The Intel manual
  // recommends doing this immediately after jumping into the new %cs segment.
  mov $LINUXBOOT_DATA32_SEL, %ax // Flat data segment for %ds, %es, and %ss.
  mov %ax, %ds
  mov %ax, %es
  mov %ax, %ss
  xor %eax, %eax
  mov %ax, %fs // Null segment selector for %fs and %gs.
  mov %ax, %gs

  // Move the 32-bit entry point into %eax for the jump below.
  mov %ebx, %eax

  // Linux booting protocol requires these all be cleared.  It doesn't really
  // matter to our code, but it makes it consistent with what a boot loader
  // doing direct 32-bit entry will do.
  xor %ebx, %ebx
  xor %ecx, %ecx
  xor %edx, %edx
  xor %ebp, %ebp
  xor %edi, %edi

  // Now jump to the 32-bit entry point.
  jmp *%eax
.end_function

// When the 16-bit entry path is used, it passes this to the 32-bit path after
// copying setup_header, as previously modified by the boot loader, into it.
// The 16-bit setup code here is the only source of information other than what
// the boot loader wrote into setup_header.  When the boot loader uses the
// 32-bit entry path, it passes its own pointer to a boot_params struct it has
// allocated elsewhere and filled with appropriate data, including copying our
// setup_header into that struct.  In that case this space is not used at all.
.balign 16, 0
.object boot_params, nosection=nosection
  .space SIZEOF_BOOT_PARAMS, 0
.end_object

.object gE820Buffer, align=4, nosection=nosection
  .space SIZEOF_E820ENTRY, 0
.end_object

// Pad out to a whole number of 512-byte sectors and record that total size (in
// bytes) as the value of the LINUXBOOT_SETUP_SIZE symbol (a SHN_ABS symbol
// that's a size, not an address).  The setup_header::setup_sects field
// initializer is calculated from this value above.  The linuxboot.ld linker
// script also uses this symbol to compute the LINUXBOOT_SYSSIZE symbol that's
// needed for the setup_header::syssize field initializer above.
//
// This is also where the load image begins, at the link-time load address
// linuxboot.ld uses.  At runtime the load image is page-aligned, so aligning
// here to 4KiB in the file too makes the usual assumptions about link-time vs
// runtime alignment hold true in the position-independent link, where the
// link-time base address is zero and the zero_page (including setup_header)
// is never actually part of the memory image, only part of the bzImage binary
// file format.  It might waste a few KiB in the image size, but it makes the
// address arithmetic easier to understand when debugging.
.p2align 12, 0
.label LINUXBOOT_SETUP_SIZE, global, notype, . - zero_page
.label LINUXBOOT_LOAD_ADDRESS, global

// This is the entrypoint for bootloaders using the 32-bit Linux boot protocol.
// The data earlier in the image is not actually part of what's loaded into
// memory.  Instead, the kernel's memory image starts here.  For the 32-bit
// non-relocatable protocol, that's exactly at 1MiB.  For the relocatable
// protocol, it's at some aligned address.  In the 64-bit boot protocol, it's
// anywhere in the address space, not necessarily in the low 4GiB, and the boot
// loader jumps to 0x200 past here in 64-bit mode.
//
// On entry, the CPU is in 32-bit protected mode with paging disabled and flat
// segments; %esi points to boot_params.  Nothing else can be relied on.
.function Linux32Entry, nosection=nosection, cfi=none
  // Interrupts should already be disabled, but make sure.
  cli

  // For the sake of convenience and simplicity, we rely on 1GiB pages to set
  // up an identity map to get us into long mode. We check for that first and
  // just enter a halt loop.
  mov $CPUID_EXT_LEAF0, %eax
  cpuid
  cmp $CPUID_AMD_FEATURE_LEAF, %eax
  jb .Lunsupported_cpu
  mov $CPUID_AMD_FEATURE_LEAF, %eax
  cpuid
  and $CPUID_AMD_FEATURE_PAGE1GB, %edx
  cmp $CPUID_AMD_FEATURE_PAGE1GB, %edx
  jne .Lunsupported_cpu

  // This code has to find its own load address, since the link is
  // position-independent and 32-bit code has no PC-relative addressing modes.
  // The only way is a call, but the 32-bit boot protocol provides no stack.
  // So borrow boot_params::scratch, which exists for just this purpose, for
  // that one call.
  lea (BOOT_PARAMS_SCRATCH + 4)(%esi), %esp
  call .Lpc
.Lpc:
  // With this, %ebp holds the runtime address of .Lpc, and we can compute the
  // runtime address of a symbols as `(sym - .Lpc)(%ebp)`.
  pop %ebp

  #define ADDR(sym) ((sym) - .Lpc)(%ebp)

  // Fill in the runtime addresses that the position-independent link can't
  // put into static data: the PML4's pointer to the PDPT, the GDT pointer's
  // base, and the far pointer to the 64-bit code to jump to below.

  lea ADDR(gLinuxbootPdpt), %eax
  or $X86_PAGING_PML4E_RWX, %eax
  mov %eax, ADDR(gLinuxbootPml4)

  lea ADDR(gLinuxbootGdt), %eax
  mov %eax, ADDR(gLinuxbootGdtr + 2)

  lea ADDR(Linux64EntryFrom32), %eax
  mov %eax, ADDR(gLinuxbootFarPtr)

  // Install the GDT that has the 64-bit code segment.  The data segments
  // currently in use stay valid until start.S resets them.
  lgdtl ADDR(gLinuxbootGdtr)

  // The 64-bit (4-level) page table format requires PAE.
  mov %cr4, %eax
  or $X86_CR4_PAE, %eax
  mov %eax, %cr4

  // Install the identity-mapping.
  lea ADDR(gLinuxbootPml4), %eax
  mov %eax, %cr3

  // Enable long mode, so that enabling paging activates it. While here, also
  // enable the No-Execute bit in page table entries, which the kernel expects.
  mov $MSR_IA32_EFER, %ecx
  rdmsr
  or $(X86_EFER_LME | X86_EFER_NXE), %eax
  wrmsr

  // Enable paging. This activates long mode, but the CPU is still executing
  // 32-bit code (now in compatibility mode).
  mov %cr0, %eax
  or $X86_CR0_PG, %eax
  mov %eax, %cr0

  // Jumping into the 64-bit code segment finishes the switch.
  ljmpl *ADDR(gLinuxbootFarPtr)
#undef ADDR

.Lunsupported_cpu:
  hlt
  jmp .Lunsupported_cpu
.end_function

# Add the in-kernel config file, which is gzipped.
# Some emulators look for this file using extract-ikconfig before booting.
.ascii "IKCFG_ST"
.incbin IKCONFIG_FILE
.ascii "IKCFG_ED"

// The 64-bit boot protocol always jumps here.  The CPU is in 64-bit mode with
// an identity-mapping page table and %rsi pointing to boot_params, which is
// just what start.S expects.
.org Linux32Entry + ENTRY64_OFFSET
.code64
.function Linux64Entry, nosection=nosection
  jmp _start
.end_function

// The 16-bit and 32-bit protocols get here instead, from the far jump at the
// end of Linux32Entry, having just entered 64-bit mode. The upper halves of
// the general registers are undefined after the switch, so %rsi must be
// zero-extended before start.S can use it. (That's not done in Linux64Entry
// because a 64-bit boot loader can pass a boot_params pointer above 4GiB.)
.function Linux64EntryFrom32, nosection=nosection
  mov %esi, %esi
  jmp _start
.end_function

// This is the GDT used by the entry code in this file: by the 16-bit entry
// path to enter 32-bit protected mode, and by the 32-bit entry path to enter
// 64-bit mode.  It's only for those transitions; the code entered afterwards
// installs its own GDT.  It lives here in the load image rather than in the
// setup area so that the 32-bit entry path, which is entered directly by the
// 32-bit boot protocol, can use it too.
.object gLinuxbootGdt, nosection=nosection, align=8
  .quad 0                       // Null descriptor.
  .quad X86_DESC32_FLAT_CODE32  // LINUXBOOT_CODE32_SEL
  .quad X86_DESC32_FLAT_DATA32  // LINUXBOOT_DATA32_SEL
  .quad X86_DESC32_CODE64       // LINUXBOOT_CODE64_SEL
  .if . - gLinuxbootGdt != LINUXBOOT_GDT_SIZE
    .error "LINUXBOOT_GDT_SIZE does not match gLinuxbootGdt"
  .endif
.end_object

// The descriptor for `lgdt` in Linux32Entry.  The base address is filled in
// at runtime.
.object gLinuxbootGdtr, nosection=nosection, align=4
  .short LINUXBOOT_GDT_SIZE - 1
  .long 0
.end_object

// The far pointer for the `ljmp` into 64-bit mode in Linux32Entry.  The
// offset is filled in at runtime.
.object gLinuxbootFarPtr, nosection=nosection, align=4
  .long 0
  .short LINUXBOOT_CODE64_SEL
.end_object

// This page table identity-maps the low 512GiB using 1GiB pages, which covers
// everything the 16-bit and 32-bit protocols could possibly hand us, since
// they are limited to 32-bit addresses (i.e., those below 4GiB). The 64-bit
// entry replaces it with page tables built from the memory map soon enough.
// Only the single PML4 entry needs an address that isn't known at link time;
// Linux32Entry fills that in. Everything else is static.f
.object gLinuxbootPml4, data, align=0x1000
  .skip 0x1000, 0
.end_object

.object gLinuxbootPdpt, data, align=0x1000
  .set .Lpdpte_address, 0
  .rept 512
    .quad .Lpdpte_address | X86_PAGING_PDPTE_1GIB_RWX
    .set .Lpdpte_address, .Lpdpte_address + (1 << 30)
  .endr
.end_object

