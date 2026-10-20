// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Result, format_err};
use display_utils::{Image, ImageId};
use fuchsia_image_format::{
    image_format_minimum_row_bytes_2, image_format_stride_bytes_per_width_pixel_2,
};

use mapped_vmo::Mapping;
use std::cell::Cell;
#[cfg(test)]
use std::cell::RefCell;
use std::cmp::{max, min};
use std::iter::Sum;
use std::num::NonZero;
use std::ops::{Add, Range};

// TODO(armansito): Extend this to support different patterns and tiled image formats.

pub struct MappedImage {
    image: Image,
    surface: Surface,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Frame {
    pub pos_x: u32,
    pub pos_y: u32,
    pub width: u32,
    pub height: u32,
}

impl Frame {
    /// Returns the smallest frame that contains both `self` and `other`.
    pub fn bounding_box(&self, other: &Frame) -> Frame {
        let left = min(self.pos_x, other.pos_x);
        let top = min(self.pos_y, other.pos_y);
        let right = max(self.right(), other.right());
        let bottom = max(self.bottom(), other.bottom());
        Frame { pos_x: left, pos_y: top, width: right - left, height: bottom - top }
    }

    /// Returns the pixels that are in both `self` and `other`, or None if there are none.
    pub fn intersection(&self, other: &Frame) -> Option<Frame> {
        let left = max(self.pos_x, other.pos_x);
        let top = max(self.pos_y, other.pos_y);
        let right = min(self.right(), other.right());
        let bottom = min(self.bottom(), other.bottom());
        if left < right && top < bottom {
            Some(Frame { pos_x: left, pos_y: top, width: right - left, height: bottom - top })
        } else {
            None
        }
    }

    fn right(&self) -> u32 {
        self.pos_x.saturating_add(self.width)
    }

    fn bottom(&self) -> u32 {
        self.pos_y.saturating_add(self.height)
    }
}

/// Where the pixels of a linear image are in its memory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SurfaceLayout {
    /// Width of the image, in pixels.
    pub width: NonZero<u32>,
    /// Height of the image, in pixels.
    pub height: NonZero<u32>,
    /// Size of one pixel, in bytes.
    pub pixel_width: NonZero<u32>,
    /// Distance between the starts of two consecutive rows, in bytes.
    pub row_bytes: NonZero<u32>,
}

impl SurfaceLayout {
    /// Distance between the starts of two consecutive pixels in a row, in bytes.
    fn pixel_stride(&self) -> u32 {
        self.row_bytes.get() / self.width
    }

    /// Clips `frame` to the image bounds, the way [`Surface::fill_region`] does: an origin past
    /// the right or bottom edge moves to the last column or row.
    pub fn clip(&self, frame: &Frame) -> Frame {
        let width = self.width.get();
        let height = self.height.get();
        let pos_x = min(width - 1, frame.pos_x);
        let pos_y = min(height - 1, frame.pos_y);
        Frame {
            pos_x,
            pos_y,
            width: min(frame.width, width - pos_x),
            height: min(frame.height, height - pos_y),
        }
    }

    /// Returns the bytes that [`Surface::fill_region`] writes for any of `frames`, as offsets
    /// into the image's memory.
    ///
    /// Each row of each clipped frame yields the range from the row's first written byte to its
    /// last. The result is sorted, and ranges that overlap or touch are merged, so no byte is in
    /// two ranges.
    fn row_byte_ranges(&self, frames: &[Frame]) -> Vec<Range<usize>> {
        let pixel_stride = self.pixel_stride() as usize;
        let row_bytes = self.row_bytes.get() as usize;
        let pixel_width = self.pixel_width.get() as usize;
        let mut ranges: Vec<Range<usize>> = Vec::new();
        for frame in frames {
            let frame = self.clip(frame);
            let (Some(frame_width), Some(frame_height)) =
                (NonZero::new(frame.width), NonZero::new(frame.height))
            else {
                continue;
            };
            // `frame` is clipped to `self`, so `pos_x + frame_width <= width` and
            // `pos_y + frame_height <= height`, and `pixel_stride * width <= row_bytes`.
            // All values are `u32`s promoted to 64-bit `usize`, so none of the products or sums
            // below can overflow.
            let first_start =
                row_bytes * frame.pos_y as usize + pixel_stride * frame.pos_x as usize;
            let len = (frame_width.get() - 1) as usize * pixel_stride + pixel_width;
            ranges.extend((0..frame_height.get() as usize).map(|row| {
                let start = first_start + row_bytes * row;
                start..start + len
            }));
        }
        // Each frame's rows are already sorted, and the stable sort merges such runs in linear
        // time.
        ranges.sort_by_key(|range| range.start);
        let mut merged: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
        for range in ranges {
            match merged.last_mut() {
                Some(last) if range.start <= last.end => last.end = max(last.end, range.end),
                _ => merged.push(range),
            }
        }
        merged
    }
}

/// A CPU access to a [`Surface`]'s bytes, recorded by tests.
#[cfg(test)]
#[derive(Clone, Debug, PartialEq)]
pub enum SurfaceOp {
    Write(Range<usize>),
    Clean(Range<usize>),
}

/// The bytes that drawing on one or more [`Surface`]s wrote and cleaned from the caches.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ByteCounts {
    /// Bytes written by drawing, including zeroing.
    pub written: u64,
    /// Bytes cleaned (written back) from the data caches.
    pub cleaned: u64,
}

impl Add for ByteCounts {
    type Output = ByteCounts;

    fn add(self, other: ByteCounts) -> ByteCounts {
        ByteCounts {
            written: self.written.saturating_add(other.written),
            cleaned: self.cleaned.saturating_add(other.cleaned),
        }
    }
}

impl Sum for ByteCounts {
    fn sum<I: Iterator<Item = ByteCounts>>(iter: I) -> ByteCounts {
        iter.fold(ByteCounts::default(), Add::add)
    }
}

/// The CPU-writable pixels of a linear image, mapped into memory.
///
/// Holds no display resources, so drawing can be tested over any VMO.
pub struct Surface {
    mapping: Mapping,
    /// The address where `mapping` starts. Only used to clean caches with `zx_cache_flush`;
    /// pixels are read and written through `mapping`.
    base_addr: usize,
    /// The VMO behind `mapping`.
    vmo: zx::Vmo,
    layout: SurfaceLayout,

    /// Time spent cleaning caches since the last [`Surface::take_clean_time`] call.
    clean_time: Cell<zx::MonotonicDuration>,
    /// Bytes written by drawing (including zeroing) and cleaned from the caches since the last
    /// [`Surface::take_byte_counts`] call.
    byte_counts: Cell<ByteCounts>,

    /// Every write and clean, in order, since the last [`Surface::take_log`] call.
    #[cfg(test)]
    log: RefCell<Vec<SurfaceOp>>,
}

impl Surface {
    /// `mapping` must map all of `vmo`, and `layout` must fit in it.
    pub fn new(mut mapping: Mapping, vmo: zx::Vmo, layout: SurfaceLayout) -> Self {
        assert!(
            u64::from(layout.width.get()) * u64::from(layout.pixel_width.get())
                <= u64::from(layout.row_bytes.get()),
            "row of {} pixels of {} bytes exceeds row_bytes {}",
            layout.width,
            layout.pixel_width,
            layout.row_bytes
        );
        assert!(
            u64::from(layout.row_bytes.get()) * u64::from(layout.height.get())
                <= mapping.len() as u64,
            "surface of {} rows of {} bytes exceeds {} byte mapping",
            layout.height,
            layout.row_bytes,
            mapping.len()
        );
        // `as_ptr_len()` documents that its pointer may only be used to free the memory, because
        // reading or writing the memory through it would bypass `SharedBuffer`. The address is
        // only ever passed to `zx_cache_flush`, which does not access the memory's contents (see
        // `Surface::clean_rows`), and `mapping` stays usable.
        let (base, _) = mapping.as_ptr_len();
        Surface {
            mapping,
            base_addr: base as usize,
            vmo,
            layout,
            clean_time: Cell::new(zx::MonotonicDuration::ZERO),
            byte_counts: Cell::new(ByteCounts::default()),
            #[cfg(test)]
            log: RefCell::new(Vec::new()),
        }
    }

    /// Clips `frame` to the surface bounds, the way [`Surface::fill_region`] does: an origin past
    /// the right or bottom edge moves to the last column or row.
    pub fn clip(&self, frame: &Frame) -> Frame {
        self.layout.clip(frame)
    }

    fn record_write(&self, range: Range<usize>) {
        self.byte_counts.update(|counts| ByteCounts {
            written: counts.written.saturating_add(range.len() as u64),
            ..counts
        });
        #[cfg(test)]
        self.log.borrow_mut().push(SurfaceOp::Write(range));
    }

    fn record_clean(&self, range: Range<usize>) {
        self.byte_counts.update(|counts| ByteCounts {
            cleaned: counts.cleaned.saturating_add(range.len() as u64),
            ..counts
        });
        #[cfg(test)]
        self.log.borrow_mut().push(SurfaceOp::Clean(range));
    }

    /// Sets every byte of the surface to zero.
    pub fn zero(&self) -> Result<()> {
        let size = self.vmo.get_size()?;
        self.vmo.op_range(zx::VmoOp::ZERO, 0, size)?;
        self.record_write(0..usize::try_from(size)?);
        Ok(())
    }

    /// Fill the specified region of this surface with the specified color. The size and
    /// interpretation of the given color must match the underlying image format.
    pub fn fill_region(&self, color: &[u8], frame: &Frame) -> Result<()> {
        let layout = &self.layout;
        if layout.pixel_width.get() != u32::try_from(color.len())? {
            return Err(format_err!(
                "provided color width ({}) does not match expected pixel format width ({})",
                color.len(),
                layout.pixel_width
            ));
        }

        // First clip the frame to the image bounds.
        let frame = self.clip(frame);

        // Color the pixels within the frame.
        let pixel_stride = layout.pixel_stride();
        let row_bytes = usize::try_from(layout.row_bytes.get())?;
        let pixel_stride_usize = usize::try_from(pixel_stride)?;
        let pos_x = usize::try_from(frame.pos_x)?;
        let pos_y = usize::try_from(frame.pos_y)?;
        let frame_width = usize::try_from(frame.width)?;
        let frame_height = usize::try_from(frame.height)?;
        if pixel_stride == layout.pixel_width.get() {
            // The pixels of each row of the frame are contiguous, so each row is written with a
            // single copy. This is much faster than one `write_at()` call per pixel.
            let row = color.repeat(frame_width);
            let first_row_offset = row_bytes * pos_y + pixel_stride_usize * pos_x;
            for row_index in 0..frame_height {
                let offset = first_row_offset + row_bytes * row_index;
                self.mapping.write_at(offset, &row);
                self.record_write(offset..offset + row.len());
            }
        } else {
            for row in 0..frame_height {
                for col in 0..frame_width {
                    let idx = row_bytes * (row + pos_y) + pixel_stride_usize * (col + pos_x);
                    self.mapping.write_at(idx, color);
                    self.record_write(idx..idx + color.len());
                }
            }
        }

        Ok(())
    }

    /// Clean (write back) data caches for the whole surface.
    pub fn cache_clean(&self) -> Result<()> {
        let start = zx::MonotonicInstant::get();
        let size = self.vmo.get_size()?;
        self.vmo.op_range(zx::VmoOp::CACHE_CLEAN, 0, size)?;
        self.record_clean(0..usize::try_from(size)?);
        self.clean_time.set(self.clean_time.get() + (zx::MonotonicInstant::get() - start));
        Ok(())
    }

    /// Clean (write back) data caches for every pixel that [`Surface::fill_region`] writes for
    /// any of `frames`.
    ///
    /// Cleans each row of each frame from its first to its last pixel (see
    /// [`SurfaceLayout::row_byte_ranges`]), in user space, so the bytes between the rows are not
    /// cleaned. Rows of different frames that overlap are merged, so no byte is cleaned twice.
    pub fn cache_clean_regions(&self, frames: &[Frame]) -> Result<()> {
        let start = zx::MonotonicInstant::get();
        let ranges = self.layout.row_byte_ranges(frames);
        let result = self.clean_rows(&ranges);
        self.clean_time.set(self.clean_time.get() + (zx::MonotonicInstant::get() - start));
        result
    }

    /// Clean (write back) data caches for each of `ranges`, offsets into the surface's memory,
    /// with one `zx_cache_flush` call per range.
    ///
    /// Panics if a range does not lie inside the mapping.
    fn clean_rows(&self, ranges: &[Range<usize>]) -> Result<()> {
        let mapping_len = self.mapping.len();
        for range in ranges {
            assert!(
                range.start <= range.end && range.end <= mapping_len,
                "clean range {:?} outside the {} byte mapping",
                range,
                mapping_len
            );
            // SAFETY: `zx_cache_flush` with `ZX_CACHE_FLUSH_DATA` is a vDSO routine that only
            // performs cache maintenance (on arm64, `dc cvac` per line and a `dsb`) on the lines
            // of the given address range. It does not change the memory's contents, so it cannot
            // conflict with reads and writes through `self.mapping`. The range lies inside the
            // mapping (asserted above), so `base_addr + range.end` does not overflow, and the
            // range stays mapped while `self.mapping` exists, which outlives this call; so the
            // cache operations cannot fault.
            let status = unsafe {
                zx::sys::zx_cache_flush(
                    (self.base_addr + range.start) as *const u8,
                    range.len(),
                    zx::sys::ZX_CACHE_FLUSH_DATA,
                )
            };
            zx::Status::ok(status).context("zx_cache_flush failed")?;
            self.record_clean(range.clone());
        }
        Ok(())
    }

    /// Returns the time spent cleaning caches since the previous call, and resets the count.
    pub fn take_clean_time(&self) -> zx::MonotonicDuration {
        self.clean_time.replace(zx::MonotonicDuration::ZERO)
    }

    /// Returns the bytes written and the bytes cleaned since the previous call, and resets the
    /// counts.
    pub fn take_byte_counts(&self) -> ByteCounts {
        self.byte_counts.take()
    }

    /// Returns the writes and cleans since the previous call, and clears the log.
    #[cfg(test)]
    pub fn take_log(&self) -> Vec<SurfaceOp> {
        self.log.take()
    }

    /// Returns a copy of all the surface's bytes.
    #[cfg(test)]
    pub fn bytes(&self) -> Vec<u8> {
        let mut bytes = vec![0; self.mapping.len()];
        self.mapping.read_at(0, &mut bytes);
        bytes
    }
}

impl MappedImage {
    pub fn create(image: Image) -> Result<MappedImage> {
        let size = usize::try_from(*image.buffer_settings.size_bytes.as_ref().unwrap())?;
        let vmo = image
            .vmo
            .duplicate_handle(zx::Rights::SAME_RIGHTS)
            .context("failed to duplicate VMO handle")?;
        let mapping = Mapping::create_from_vmo(
            &vmo,
            size,
            zx::VmarFlags::PERM_READ | zx::VmarFlags::PERM_WRITE,
        )
        .context("failed to map VMO")?;

        let width = NonZero::new(image.parameters.width)
            .ok_or_else(|| format_err!("image width must be non-zero"))?;
        let height = NonZero::new(image.parameters.height)
            .ok_or_else(|| format_err!("image height must be non-zero"))?;
        let constraints = &image.format_constraints;
        let pixel_width = image_format_stride_bytes_per_width_pixel_2(
            *constraints.pixel_format.as_ref().unwrap(),
        )
        .unwrap();
        let pixel_width =
            NonZero::new(pixel_width).ok_or_else(|| format_err!("pixel width must be non-zero"))?;
        let row_bytes = image_format_minimum_row_bytes_2(constraints, image.parameters.width)?;
        let row_bytes =
            NonZero::new(row_bytes).ok_or_else(|| format_err!("row bytes must be non-zero"))?;
        let surface =
            Surface::new(mapping, vmo, SurfaceLayout { width, height, pixel_width, row_bytes });
        Ok(MappedImage { image, surface })
    }

    pub fn id(&self) -> ImageId {
        self.image.id
    }

    /// The image's pixels.
    pub fn surface(&self) -> &Surface {
        &self.surface
    }

    pub fn zero(&self) -> Result<()> {
        self.surface.zero()
    }

    /// Fill the image with the specified color. The size and interpretation of the given color
    /// must match the underlying image format.
    pub fn fill(&self, color: &[u8]) -> Result<()> {
        self.fill_region(
            color,
            &Frame { pos_x: 0, pos_y: 0, width: self.width(), height: self.height() },
        )
    }

    /// Fill the specified region of this image with the specified color. The size and
    /// interpretation of the given color must match the underlying image format.
    pub fn fill_region(&self, color: &[u8], frame: &Frame) -> Result<()> {
        self.surface.fill_region(color, frame)
    }

    /// Clean (write back) data caches, so previous writes are visible in main memory. This
    /// operation must be called at once before this image buffer gets presented to avoid artifacts
    /// during scanout in height refresh rates if the image is changing frequently. This operation
    /// can be expensive on large images and should be performed sparingly.
    pub fn cache_clean(&self) -> Result<()> {
        self.surface.cache_clean()
    }

    /// Returns the time spent cleaning caches since the previous call, and resets the count.
    pub fn take_clean_time(&self) -> zx::MonotonicDuration {
        self.surface.take_clean_time()
    }

    /// Returns the bytes written and the bytes cleaned since the previous call, and resets the
    /// counts.
    pub fn take_byte_counts(&self) -> ByteCounts {
        self.surface.take_byte_counts()
    }

    fn width(&self) -> u32 {
        self.image.parameters.width
    }

    fn height(&self) -> u32 {
        self.image.parameters.height
    }
}

/// Helpers for tests of code that draws on a [`Surface`].
#[cfg(test)]
pub mod testing {
    use super::*;

    /// Creates a surface of 4-byte pixels without row padding, all zero.
    pub fn new_surface(width: u32, height: u32) -> Surface {
        let width = NonZero::new(width).expect("test surface width must be non-zero");
        let height = NonZero::new(height).expect("test surface height must be non-zero");
        let pixel_width = NonZero::new(4).unwrap();
        let row_bytes = width.checked_mul(pixel_width).expect("row_bytes overflow");
        let page_size = zx::system_get_page_size() as usize;
        let size = (row_bytes.get() as usize * height.get() as usize).next_multiple_of(page_size);
        let (mapping, vmo) = Mapping::allocate(size).expect("allocate test VMO");
        Surface::new(mapping, vmo, SurfaceLayout { width, height, pixel_width, row_bytes })
    }

    /// For each byte of a surface, whether some ops wrote it and whether they cleaned it.
    pub struct TouchedBytes {
        pub written: Vec<bool>,
        pub cleaned: Vec<bool>,
    }

    /// Returns which of the first `size` bytes of a surface `ops` writes and cleans.
    pub fn touched_bytes(ops: &[SurfaceOp], size: usize) -> TouchedBytes {
        let mut touched = TouchedBytes { written: vec![false; size], cleaned: vec![false; size] };
        for op in ops {
            match op {
                SurfaceOp::Write(range) => touched.written[range.clone()].fill(true),
                SurfaceOp::Clean(range) => touched.cleaned[range.clone()].fill(true),
            }
        }
        touched
    }

    /// Checks that every byte written in `ops` is cleaned by a later op in `ops`.
    ///
    /// Returns the offset of the first byte that is not, if any.
    pub fn first_uncleaned_byte(ops: &[SurfaceOp]) -> Option<usize> {
        let size = ops
            .iter()
            .map(|op| match op {
                SurfaceOp::Write(range) | SurfaceOp::Clean(range) => range.end,
            })
            .max()
            .unwrap_or(0);
        let mut cleaned = vec![false; size];
        let mut first_uncleaned: Option<usize> = None;
        for op in ops.iter().rev() {
            match op {
                SurfaceOp::Clean(range) => cleaned[range.clone()].fill(true),
                SurfaceOp::Write(range) => {
                    if let Some(offset) = range.clone().find(|offset| !cleaned[*offset]) {
                        first_uncleaned =
                            Some(first_uncleaned.map_or(offset, |first| min(first, offset)));
                    }
                }
            }
        }
        first_uncleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use googletest::{expect_eq, expect_true, gtest};

    /// The per-pixel algorithm `fill_region` used before rows were copied at once, over a plain
    /// buffer. Results must match it byte for byte.
    fn reference_fill_region(
        buffer: &mut [u8],
        layout: &SurfaceLayout,
        color: &[u8],
        frame: &Frame,
    ) {
        let width = layout.width.get();
        let height = layout.height.get();
        let row_bytes = layout.row_bytes.get() as usize;
        let pos_x = min(width - 1, frame.pos_x);
        let pos_y = min(height - 1, frame.pos_y);
        let frame_width = min(frame.width, width - pos_x);
        let frame_height = min(frame.height, height - pos_y);
        let pixel_stride = layout.pixel_stride() as usize;
        for row in 0..frame_height {
            for col in 0..frame_width {
                let idx = row_bytes * (row as usize + pos_y as usize)
                    + pixel_stride * (col as usize + pos_x as usize);
                buffer[idx..idx + color.len()].copy_from_slice(color);
            }
        }
    }

    /// A [`Surface`] over a test VMO, plus the bytes it is expected to hold.
    struct Fixture {
        surface: Surface,
        expected: Vec<u8>,
    }

    impl Fixture {
        /// Creates a surface whose bytes start out as a non-zero pattern, so writes to the wrong
        /// place and missing writes both show up as differences.
        fn new(width: u32, height: u32, pixel_width: u32, row_bytes: u32) -> Fixture {
            let layout = SurfaceLayout {
                width: NonZero::new(width).unwrap(),
                height: NonZero::new(height).unwrap(),
                pixel_width: NonZero::new(pixel_width).unwrap(),
                row_bytes: NonZero::new(row_bytes).unwrap(),
            };
            let page_size = zx::system_get_page_size() as usize;
            let size = (row_bytes as usize * height as usize).next_multiple_of(page_size);
            let (mapping, vmo) = Mapping::allocate(size).expect("allocate test VMO");
            let expected: Vec<u8> = (0..size).map(|i| (i * 7 + 3) as u8).collect();
            mapping.write_at(0, &expected);
            Fixture { surface: Surface::new(mapping, vmo, layout), expected }
        }

        /// Fills `frame` in the surface and in the expected bytes.
        fn fill_region(&mut self, color: &[u8], frame: Frame) {
            self.surface.fill_region(color, &frame).expect("fill_region");
            reference_fill_region(&mut self.expected, &self.surface.layout, color, &frame);
        }

        fn actual(&self) -> Vec<u8> {
            let mut actual = vec![0; self.surface.mapping.len()];
            self.surface.mapping.read_at(0, &mut actual);
            actual
        }

        /// Returns the offset of the first byte that differs from the reference, if any.
        fn first_difference(&self) -> Option<usize> {
            self.actual().iter().zip(self.expected.iter()).position(|(a, e)| a != e)
        }
    }

    const COLOR: [u8; 4] = [0x10, 0x20, 0x30, 0xff];

    #[gtest]
    #[fuchsia::test]
    fn reference_fills_exactly_the_frame() {
        // Checks the reference itself, by hand, on a 4x3 surface with 2 bytes of padding per row.
        let mut buffer = vec![0u8; 18 * 3];
        reference_fill_region(
            &mut buffer,
            &SurfaceLayout {
                width: NonZero::new(4).unwrap(),
                height: NonZero::new(3).unwrap(),
                pixel_width: NonZero::new(4).unwrap(),
                row_bytes: NonZero::new(18).unwrap(),
            },
            &COLOR,
            &Frame { pos_x: 1, pos_y: 1, width: 2, height: 1 },
        );
        let mut expected = vec![0u8; 18 * 3];
        expected[22..26].copy_from_slice(&COLOR);
        expected[26..30].copy_from_slice(&COLOR);
        expect_eq!(buffer, expected);
    }

    #[gtest]
    #[fuchsia::test]
    fn fill_region_writes_only_the_frame() {
        let mut fixture = Fixture::new(4, 3, 4, 16);
        let before = fixture.actual();
        fixture.fill_region(&COLOR, Frame { pos_x: 1, pos_y: 1, width: 2, height: 2 });
        let actual = fixture.actual();
        for (offset, (byte, old)) in actual.iter().zip(before.iter()).enumerate() {
            let row = offset / 16;
            let col = (offset % 16) / 4;
            if (1..3).contains(&row) && (1..3).contains(&col) {
                expect_eq!(*byte, COLOR[offset % 4], "offset {}", offset);
            } else {
                expect_eq!(*byte, *old, "offset {}", offset);
            }
        }
    }

    #[gtest]
    #[fuchsia::test]
    fn fill_whole_surface() {
        let mut fixture = Fixture::new(64, 48, 4, 256);
        fixture.fill_region(&COLOR, Frame { pos_x: 0, pos_y: 0, width: 64, height: 48 });
        expect_eq!(fixture.first_difference(), None);
    }

    #[gtest]
    #[fuchsia::test]
    fn fill_interior_rect() {
        let mut fixture = Fixture::new(64, 48, 4, 256);
        fixture.fill_region(&COLOR, Frame { pos_x: 5, pos_y: 7, width: 20, height: 11 });
        expect_eq!(fixture.first_difference(), None);
    }

    #[gtest]
    #[fuchsia::test]
    fn fill_rect_clipped_at_right_edge() {
        let mut fixture = Fixture::new(64, 48, 4, 256);
        fixture.fill_region(&COLOR, Frame { pos_x: 50, pos_y: 3, width: 30, height: 10 });
        expect_eq!(fixture.first_difference(), None);
    }

    #[gtest]
    #[fuchsia::test]
    fn fill_rect_clipped_at_bottom_edge() {
        let mut fixture = Fixture::new(64, 48, 4, 256);
        fixture.fill_region(&COLOR, Frame { pos_x: 3, pos_y: 40, width: 10, height: 30 });
        expect_eq!(fixture.first_difference(), None);
    }

    #[gtest]
    #[fuchsia::test]
    fn fill_rect_clipped_at_bottom_right_corner() {
        let mut fixture = Fixture::new(64, 48, 4, 256);
        fixture
            .fill_region(&COLOR, Frame { pos_x: 60, pos_y: 45, width: u32::MAX, height: u32::MAX });
        expect_eq!(fixture.first_difference(), None);
    }

    #[gtest]
    #[fuchsia::test]
    fn fill_origin_past_edge_draws_last_column_and_row() {
        let mut fixture = Fixture::new(64, 48, 4, 256);
        fixture.fill_region(&COLOR, Frame { pos_x: 100, pos_y: 5, width: 10, height: 3 });
        fixture.fill_region(&[1, 2, 3, 4], Frame { pos_x: 5, pos_y: 100, width: 3, height: 10 });
        fixture.fill_region(&[5, 6, 7, 8], Frame { pos_x: 64, pos_y: 48, width: 2, height: 2 });
        expect_eq!(fixture.first_difference(), None);
    }

    #[gtest]
    #[fuchsia::test]
    fn fill_one_pixel() {
        let mut fixture = Fixture::new(64, 48, 4, 256);
        fixture.fill_region(&COLOR, Frame { pos_x: 17, pos_y: 9, width: 1, height: 1 });
        expect_eq!(fixture.first_difference(), None);
    }

    #[gtest]
    #[fuchsia::test]
    fn fill_one_pixel_surface() {
        let mut fixture = Fixture::new(1, 1, 4, 64);
        fixture.fill_region(&COLOR, Frame { pos_x: 0, pos_y: 0, width: 1, height: 1 });
        expect_eq!(fixture.first_difference(), None);
    }

    #[gtest]
    #[fuchsia::test]
    fn fill_empty_frame_writes_nothing() {
        let mut fixture = Fixture::new(64, 48, 4, 256);
        fixture.fill_region(&COLOR, Frame { pos_x: 5, pos_y: 5, width: 0, height: 10 });
        fixture.fill_region(&COLOR, Frame { pos_x: 5, pos_y: 5, width: 10, height: 0 });
        expect_eq!(fixture.first_difference(), None);
    }

    #[gtest]
    #[fuchsia::test]
    fn fill_three_byte_pixels() {
        // Rows without padding.
        let mut fixture = Fixture::new(10, 6, 3, 30);
        fixture.fill_region(&[1, 2, 3], Frame { pos_x: 2, pos_y: 1, width: 5, height: 3 });
        fixture.fill_region(&[4, 5, 6], Frame { pos_x: 8, pos_y: 4, width: 5, height: 5 });
        expect_eq!(fixture.first_difference(), None);

        // Rows padded by less than a pixel per pixel, so the pixel stride is still 3 bytes.
        let mut fixture = Fixture::new(10, 6, 3, 32);
        fixture.fill_region(&[1, 2, 3], Frame { pos_x: 2, pos_y: 1, width: 5, height: 3 });
        fixture.fill_region(&[4, 5, 6], Frame { pos_x: 8, pos_y: 4, width: 5, height: 5 });
        expect_eq!(fixture.first_difference(), None);
    }

    #[gtest]
    #[fuchsia::test]
    fn fill_padded_rows_with_pixel_stride_wider_than_pixel() {
        // 64-byte rows of 10 pixels give a pixel stride of 6 bytes for 4-byte pixels.
        let mut fixture = Fixture::new(10, 6, 4, 64);
        fixture.fill_region(&COLOR, Frame { pos_x: 0, pos_y: 0, width: 10, height: 6 });
        fixture.fill_region(&[1, 2, 3, 4], Frame { pos_x: 3, pos_y: 2, width: 4, height: 3 });
        fixture.fill_region(&[5, 6, 7, 8], Frame { pos_x: 8, pos_y: 5, width: 4, height: 4 });
        expect_eq!(fixture.first_difference(), None);
    }

    #[gtest]
    #[fuchsia::test]
    fn later_fills_paint_over_earlier_ones() {
        // A scaled-down multilayer-squares frame: clear, then overlapping squares.
        let mut fixture = Fixture::new(168, 374, 4, 168 * 4);
        fixture.fill_region(&[0, 0, 0, 0], Frame { pos_x: 0, pos_y: 0, width: 168, height: 374 });
        fixture.fill_region(&COLOR, Frame { pos_x: 10, pos_y: 20, width: 84, height: 84 });
        fixture
            .fill_region(&[59, 150, 0, 150], Frame { pos_x: 50, pos_y: 60, width: 21, height: 21 });
        fixture.fill_region(&[1, 2, 3, 4], Frame { pos_x: 120, pos_y: 300, width: 84, height: 84 });
        expect_eq!(fixture.first_difference(), None);
    }

    #[gtest]
    #[fuchsia::test]
    fn fill_with_wrong_color_size_fails_and_writes_nothing() {
        let fixture = Fixture::new(64, 48, 4, 256);
        let frame = Frame { pos_x: 0, pos_y: 0, width: 64, height: 48 };
        expect_true!(fixture.surface.fill_region(&[1, 2, 3], &frame).is_err());
        expect_true!(fixture.surface.fill_region(&[1, 2, 3, 4, 5], &frame).is_err());
        expect_eq!(fixture.first_difference(), None);
    }

    #[gtest]
    #[fuchsia::test]
    fn frame_intersection() {
        let a = Frame { pos_x: 10, pos_y: 20, width: 30, height: 40 };
        expect_eq!(
            a.intersection(&Frame { pos_x: 30, pos_y: 50, width: 100, height: 5 }),
            Some(Frame { pos_x: 30, pos_y: 50, width: 10, height: 5 })
        );
        expect_eq!(a.intersection(&a), Some(a));
        // Frames that only touch share no pixel.
        expect_eq!(a.intersection(&Frame { pos_x: 40, pos_y: 20, width: 5, height: 5 }), None);
        expect_eq!(a.intersection(&Frame { pos_x: 10, pos_y: 60, width: 5, height: 5 }), None);
        expect_eq!(a.intersection(&Frame { pos_x: 15, pos_y: 25, width: 0, height: 5 }), None);
    }

    #[gtest]
    #[fuchsia::test]
    fn frame_bounding_box() {
        let a = Frame { pos_x: 10, pos_y: 20, width: 30, height: 40 };
        let b = Frame { pos_x: 25, pos_y: 5, width: 100, height: 10 };
        let expected = Frame { pos_x: 10, pos_y: 5, width: 115, height: 55 };
        expect_eq!(a.bounding_box(&b), expected);
        expect_eq!(b.bounding_box(&a), expected);
        expect_eq!(a.bounding_box(&a), a);
    }

    /// Returns the total length of the cleans in `ops`.
    fn cleaned_len(ops: &[SurfaceOp]) -> usize {
        ops.iter()
            .map(|op| match op {
                SurfaceOp::Write(_) => 0,
                SurfaceOp::Clean(range) => range.len(),
            })
            .sum()
    }

    #[gtest]
    #[fuchsia::test]
    fn clean_region_covers_exactly_the_written_rows() {
        let frames = [
            Frame { pos_x: 5, pos_y: 7, width: 20, height: 11 },
            Frame { pos_x: 50, pos_y: 3, width: 30, height: 10 },
            Frame { pos_x: 3, pos_y: 40, width: 10, height: 30 },
            Frame { pos_x: 60, pos_y: 45, width: u32::MAX, height: u32::MAX },
            Frame { pos_x: 100, pos_y: 5, width: 10, height: 3 },
            Frame { pos_x: 64, pos_y: 48, width: 2, height: 2 },
            Frame { pos_x: 17, pos_y: 9, width: 1, height: 1 },
            Frame { pos_x: 0, pos_y: 0, width: 64, height: 48 },
        ];
        // Rows without padding, rows with padding, and pixel strides wider than the pixel (5 bytes
        // for 320-byte rows of 64 pixels, 6 bytes for 64-byte rows of 10 pixels).
        for (width, row_bytes) in [(64, 256), (64, 288), (64, 320), (10, 64)] {
            for frame in &frames {
                let fixture = Fixture::new(width, 48, 4, row_bytes);
                fixture.surface.fill_region(&COLOR, frame).expect("fill_region");
                fixture.surface.cache_clean_regions(&[*frame]).expect("cache_clean_regions");
                let ops = fixture.surface.take_log();
                expect_eq!(testing::first_uncleaned_byte(&ops), None);
                // In each row, exactly the bytes from the first to the last written byte are
                // cleaned, and none of them twice.
                let testing::TouchedBytes { written, cleaned } =
                    testing::touched_bytes(&ops, fixture.surface.mapping.len());
                let mut expected_cleaned = vec![false; written.len()];
                for row in
                    written.chunks(row_bytes as usize).enumerate().filter_map(|(row_index, row)| {
                        let first = row.iter().position(|w| *w)?;
                        let last = row.iter().rposition(|w| *w)?;
                        let row_start = row_index * row_bytes as usize;
                        Some(row_start + first..row_start + last + 1)
                    })
                {
                    expected_cleaned[row].fill(true);
                }
                expect_true!(
                    cleaned == expected_cleaned,
                    "width {} row_bytes {} {:?}",
                    width,
                    row_bytes,
                    frame
                );
                expect_eq!(
                    cleaned_len(&ops),
                    cleaned.iter().filter(|c| **c).count(),
                    "width {} row_bytes {} {:?}",
                    width,
                    row_bytes,
                    frame
                );
            }
        }
    }

    #[gtest]
    #[fuchsia::test]
    fn clean_regions_of_overlapping_frames_cleans_each_written_byte_once() {
        // Pairs of frames like an old and a new square position, plus disjoint and touching ones.
        let pairs = [
            (
                Frame { pos_x: 10, pos_y: 5, width: 20, height: 20 },
                Frame { pos_x: 14, pos_y: 9, width: 20, height: 20 },
            ),
            (
                Frame { pos_x: 30, pos_y: 20, width: 20, height: 20 },
                Frame { pos_x: 26, pos_y: 16, width: 20, height: 20 },
            ),
            (
                Frame { pos_x: 0, pos_y: 0, width: 5, height: 30 },
                Frame { pos_x: 50, pos_y: 10, width: 5, height: 30 },
            ),
            (
                Frame { pos_x: 0, pos_y: 0, width: 32, height: 10 },
                Frame { pos_x: 32, pos_y: 0, width: 32, height: 10 },
            ),
            (
                Frame { pos_x: 50, pos_y: 40, width: 30, height: 30 },
                Frame { pos_x: 0, pos_y: 47, width: 64, height: 1 },
            ),
        ];
        // Rows without and with padding; the pixels of a row are contiguous in both (320-byte
        // rows of 64 pixels would give a pixel stride of 5 bytes).
        for (width, row_bytes) in [(64, 256), (64, 288)] {
            for (a, b) in &pairs {
                let fixture = Fixture::new(width, 48, 4, row_bytes);
                fixture.surface.fill_region(&COLOR, a).expect("fill_region");
                fixture.surface.fill_region(&[1, 2, 3, 4], b).expect("fill_region");
                fixture.surface.cache_clean_regions(&[*a, *b]).expect("cache_clean_regions");
                let ops = fixture.surface.take_log();
                let testing::TouchedBytes { written, cleaned } =
                    testing::touched_bytes(&ops, fixture.surface.mapping.len());
                expect_true!(cleaned == written, "row_bytes {} frames {:?} {:?}", row_bytes, a, b);
                expect_eq!(
                    cleaned_len(&ops),
                    written.iter().filter(|w| **w).count(),
                    "row_bytes {} frames {:?} {:?}",
                    row_bytes,
                    a,
                    b
                );
            }
        }
    }

    #[gtest]
    #[fuchsia::test]
    fn clean_regions_skips_empty_frames() {
        let fixture = Fixture::new(64, 48, 4, 256);
        fixture
            .surface
            .cache_clean_regions(&[
                Frame { pos_x: 5, pos_y: 5, width: 0, height: 10 },
                Frame { pos_x: 5, pos_y: 5, width: 10, height: 0 },
            ])
            .expect("cache_clean_regions");
        expect_eq!(fixture.surface.take_log(), vec![]);
    }

    #[gtest]
    #[fuchsia::test]
    fn clean_regions_cleans_each_row_in_order() {
        let fixture = Fixture::new(64, 48, 4, 256);
        fixture
            .surface
            .cache_clean_regions(&[
                Frame { pos_x: 40, pos_y: 10, width: 4, height: 2 },
                Frame { pos_x: 0, pos_y: 0, width: 2, height: 1 },
                Frame { pos_x: 0, pos_y: 11, width: 2, height: 3 },
            ])
            .expect("cache_clean_regions");
        expect_eq!(
            fixture.surface.take_log(),
            vec![
                SurfaceOp::Clean(0..8),
                SurfaceOp::Clean(10 * 256 + 160..10 * 256 + 176),
                SurfaceOp::Clean(11 * 256..11 * 256 + 8),
                SurfaceOp::Clean(11 * 256 + 160..11 * 256 + 176),
                SurfaceOp::Clean(12 * 256..12 * 256 + 8),
                SurfaceOp::Clean(13 * 256..13 * 256 + 8),
            ]
        );
        expect_eq!(
            fixture.surface.take_byte_counts(),
            ByteCounts { written: 0, cleaned: 8 + 2 * 16 + 3 * 8 }
        );
    }

    /// 64 x 48 pixels of 4 bytes, without row padding.
    const LAYOUT: SurfaceLayout = SurfaceLayout {
        width: NonZero::new(64).unwrap(),
        height: NonZero::new(48).unwrap(),
        pixel_width: NonZero::new(4).unwrap(),
        row_bytes: NonZero::new(256).unwrap(),
    };

    /// [`LAYOUT`] with 32 bytes of padding after each row.
    const PADDED_LAYOUT: SurfaceLayout =
        SurfaceLayout { row_bytes: NonZero::new(288).unwrap(), ..LAYOUT };

    #[gtest]
    #[fuchsia::test]
    fn row_byte_ranges_of_interior_frame() {
        expect_eq!(
            LAYOUT.row_byte_ranges(&[Frame { pos_x: 5, pos_y: 7, width: 20, height: 3 }]),
            vec![
                7 * 256 + 20..7 * 256 + 100,
                8 * 256 + 20..8 * 256 + 100,
                9 * 256 + 20..9 * 256 + 100,
            ]
        );
    }

    #[gtest]
    #[fuchsia::test]
    fn row_byte_ranges_of_clipped_frames() {
        // Right edge: 14 of 30 columns remain.
        expect_eq!(
            LAYOUT.row_byte_ranges(&[Frame { pos_x: 50, pos_y: 3, width: 30, height: 2 }]),
            vec![3 * 256 + 200..4 * 256, 4 * 256 + 200..5 * 256]
        );
        // Bottom edge: 2 of 10 rows remain.
        expect_eq!(
            LAYOUT.row_byte_ranges(&[Frame { pos_x: 3, pos_y: 46, width: 2, height: 10 }]),
            vec![46 * 256 + 12..46 * 256 + 20, 47 * 256 + 12..47 * 256 + 20]
        );
        // Bottom-right corner, with padded rows: the padding is not cleaned.
        expect_eq!(
            PADDED_LAYOUT.row_byte_ranges(&[Frame {
                pos_x: 62,
                pos_y: 46,
                width: u32::MAX,
                height: u32::MAX
            }]),
            vec![46 * 288 + 248..46 * 288 + 256, 47 * 288 + 248..47 * 288 + 256]
        );
        // An origin past the edge moves to the last column or row.
        expect_eq!(
            LAYOUT.row_byte_ranges(&[Frame { pos_x: 100, pos_y: 5, width: 10, height: 1 }]),
            vec![5 * 256 + 252..6 * 256]
        );
        expect_eq!(
            LAYOUT.row_byte_ranges(&[Frame { pos_x: 5, pos_y: 100, width: 3, height: 10 }]),
            vec![47 * 256 + 20..47 * 256 + 32]
        );
    }

    #[gtest]
    #[fuchsia::test]
    fn row_byte_ranges_of_full_rows() {
        // Without padding, consecutive full rows touch and become one range.
        expect_eq!(
            LAYOUT.row_byte_ranges(&[Frame { pos_x: 0, pos_y: 2, width: 64, height: 3 }]),
            vec![2 * 256..5 * 256]
        );
        // With padding, they do not.
        expect_eq!(
            PADDED_LAYOUT.row_byte_ranges(&[Frame { pos_x: 0, pos_y: 2, width: 64, height: 2 }]),
            vec![2 * 288..2 * 288 + 256, 3 * 288..3 * 288 + 256]
        );
    }

    #[gtest]
    #[fuchsia::test]
    fn row_byte_ranges_with_pixel_stride_wider_than_pixel() {
        // 64-byte rows of 10 pixels give a pixel stride of 6 bytes. The range ends at the last
        // byte of the last pixel.
        expect_eq!(
            SurfaceLayout {
                width: NonZero::new(10).unwrap(),
                height: NonZero::new(6).unwrap(),
                pixel_width: NonZero::new(4).unwrap(),
                row_bytes: NonZero::new(64).unwrap(),
            }
            .row_byte_ranges(&[Frame { pos_x: 3, pos_y: 2, width: 4, height: 2 }]),
            vec![2 * 64 + 18..2 * 64 + 40, 3 * 64 + 18..3 * 64 + 40]
        );
    }

    #[gtest]
    #[fuchsia::test]
    fn row_byte_ranges_skips_empty_frames() {
        expect_eq!(LAYOUT.row_byte_ranges(&[]), Vec::<Range<usize>>::new());
        expect_eq!(
            LAYOUT.row_byte_ranges(&[
                Frame { pos_x: 5, pos_y: 5, width: 0, height: 10 },
                Frame { pos_x: 5, pos_y: 5, width: 10, height: 0 },
            ]),
            Vec::<Range<usize>>::new()
        );
    }

    #[gtest]
    #[fuchsia::test]
    fn row_byte_ranges_merges_overlapping_and_touching_rows() {
        // Overlapping frames: their shared row is one range.
        expect_eq!(
            LAYOUT.row_byte_ranges(&[
                Frame { pos_x: 12, pos_y: 1, width: 4, height: 2 },
                Frame { pos_x: 10, pos_y: 0, width: 4, height: 2 },
            ]),
            vec![40..56, 256 + 40..256 + 64, 512 + 48..512 + 64]
        );
        // Disjoint frames on the same row stay separate; touching ones merge.
        expect_eq!(
            LAYOUT.row_byte_ranges(&[
                Frame { pos_x: 10, pos_y: 0, width: 2, height: 1 },
                Frame { pos_x: 0, pos_y: 0, width: 2, height: 1 },
                Frame { pos_x: 2, pos_y: 0, width: 2, height: 1 },
            ]),
            vec![0..16, 40..48]
        );
        // A frame inside another adds nothing.
        expect_eq!(
            LAYOUT.row_byte_ranges(&[
                Frame { pos_x: 10, pos_y: 10, width: 10, height: 1 },
                Frame { pos_x: 12, pos_y: 10, width: 2, height: 1 },
            ]),
            vec![10 * 256 + 40..10 * 256 + 80]
        );
    }

    #[gtest]
    #[fuchsia::test]
    fn byte_counts_track_writes_and_cleans() {
        let fixture = Fixture::new(64, 48, 4, 256);
        fixture
            .surface
            .fill_region(&COLOR, &Frame { pos_x: 1, pos_y: 2, width: 3, height: 4 })
            .expect("fill_region");
        fixture.surface.cache_clean().expect("cache_clean");
        let size = fixture.surface.mapping.len() as u64;
        expect_eq!(
            fixture.surface.take_byte_counts(),
            ByteCounts { written: 3 * 4 * 4, cleaned: size }
        );
        expect_eq!(fixture.surface.take_byte_counts(), ByteCounts { written: 0, cleaned: 0 });
        fixture.surface.zero().expect("zero");
        expect_eq!(fixture.surface.take_byte_counts(), ByteCounts { written: size, cleaned: 0 });
    }

    #[gtest]
    #[fuchsia::test]
    fn byte_counts_sum_adds_written_and_cleaned_separately() {
        let counts = [
            ByteCounts { written: 1, cleaned: 20 },
            ByteCounts { written: 300, cleaned: 4000 },
            ByteCounts { written: 50000, cleaned: 0 },
        ];
        expect_eq!(
            counts.into_iter().sum::<ByteCounts>(),
            ByteCounts { written: 50301, cleaned: 4020 }
        );
        expect_eq!(
            std::iter::empty::<ByteCounts>().sum::<ByteCounts>(),
            ByteCounts { written: 0, cleaned: 0 }
        );
    }

    #[gtest]
    #[fuchsia::test]
    fn zero_clears_every_byte() {
        let fixture = Fixture::new(64, 48, 4, 256);
        fixture.surface.zero().expect("zero");
        expect_true!(fixture.surface.bytes().iter().all(|byte| *byte == 0));
    }

    #[gtest]
    #[fuchsia::test]
    fn first_uncleaned_byte_finds_writes_not_cleaned_later() {
        use SurfaceOp::{Clean, Write};
        expect_eq!(testing::first_uncleaned_byte(&[]), None);
        expect_eq!(testing::first_uncleaned_byte(&[Write(4..8), Clean(0..16)]), None);
        expect_eq!(testing::first_uncleaned_byte(&[Write(4..8)]), Some(4));
        // A clean before the write does not count.
        expect_eq!(testing::first_uncleaned_byte(&[Clean(0..16), Write(4..8)]), Some(4));
        // Partially cleaned writes.
        expect_eq!(
            testing::first_uncleaned_byte(&[Write(4..8), Write(20..24), Clean(0..22)]),
            Some(22)
        );
        expect_eq!(testing::first_uncleaned_byte(&[Write(4..8), Clean(5..8)]), Some(4));
    }
}
