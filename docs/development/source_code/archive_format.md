# Fuchsia archive format (FAR)

## Overview

The Fuchsia archive format is a format for storing a directory tree in a
file. Like a `.tar` or `.zip` file, a Fuchsia archive file stores a mapping
from path names to file contents.

Fuchsia archive files are sometimes referred to as FARs or FAR archives,
and are given the filename extension `.far`.

For a reference how to create and build Fuchsia packages, inspect package
content etc, see [Developing with Fuchsia packages][pkg-dev].

## Format

An archive is a sequence of bytes, divided into chunks:

 * The first chunk is the index chunk, which describes where other chunks are
   located in the archive.
 * All the chunks listed in the index must appear in the archive in the order
   listed in the index (which is sorted by their type).
 * The archive may contain additional chunks that are not referenced in the
   index, but these chunks must appear in the archive after all the chunks
   listed in the index. For example, content chunks are not listed in the
   index. Instead, the content chunks are reachable from the directory chunk.
 * The chunks must not overlap.
 * All chunks are aligned on 64 bit boundaries.
 * All chunks must be packed as tightly as possible subject to their alignment
   constraints.
 * Any gaps between chunks must be filled with zeros.

All offsets and lengths are encoded as unsigned integers in little endian.

## Index chunk

The index chunk is required and must start at the beginning of the archive.

 * 8 bytes of magic.
    - Must be 0xc8 0xbf 0x0b 0x48 0xad 0xab 0xc5 0x11.
 * 64 bit length of concatenated index entries, in bytes.
 * Concatenated index entries.

No two index entries can have the same type and the entries must be sorted by
type in increasing lexicographical octet order (e.g., as compared by memcmp).
The chunks listed in the index must be stored in the archive in the order listed
in the index.

### Index entry

 * 64 bit chunk type.
 * 64 bit offset from start of the archive to the start of the referenced
   chunk, in bytes.
 * 64 bit length of referenced chunk, in bytes.

## Directory chunk (Type "DIR-----")

The directory chunk is required.  Entries in the directory chunk must have
unique names and the entries must be sorted by name in increasing
lexicographical octet order (e.g., as compared by memcmp).

 * Concatenated directory table entries.

These entries represent the files contained in the archive. Directories
themselves are not represented explicitly, which means archives cannot represent
empty directories.

### Directory table entry

 * Name.
    - 32 bit offset from the start of the directory names chunk to the path
      data, in bytes.
    - 16 bit length of name, in bytes.
 * 16 bits of zeros, reserved for future use.
 * Data.
    - 64 bit offset from start of archive to the start of the content chunk, in
      bytes.
    - 64 bit length of the data, in bytes.
 * 64 bits of zeros, reserved for future use.

## Directory names chunk (Type "DIRNAMES")

The directory names chunk is required and is used by the directory chunk to name
the content chunks. Path data must be sorted in increasing lexicographical
octet order (e.g., as compared by memcmp).

 * Concatenated path data (no encoding specified).
 * Zero padding to next 8 byte boundary.

Note: The offsets used to index into the path data are 32 bits long, which means
there is no reason to create a directory name chunk that is larger than 4 GB.

Although no encoding is specified, clients that wish to display path data using
unicode may attempt to decode the data as UTF-8. The path data might or might
not be UTF-8, which means that decoding might fail.

### Path data

 * Octets of path.
    - Must not be empty.
    - Must not contain a 0x00 octet.
    - The leading octet must not be 0x2F ('/').
    - The trailing octet must not be 0x2F ('/').
    - Let *segments* be the result of splitting the path on 0x2F ('/'). Each
      segment must meet the following requirements:
       - Must not be empty.
       - Must not be exactly 0x2E ('.')
       - Must not be exactly 0x2E 0x2E ('..')

## Content chunk

Content chunks must be after all the chunks listed in the index chunk. The
content chunks must appear in the archive in the order they are listed in the
directory.

 * data

The data must be aligned on a 4096 byte boundary from the start of the archive
and the data must be padded with zeros until the next 4096 byte boundary.

<!-- Reference links -->

[pkg-dev]: /docs/development/build/package_update.md
