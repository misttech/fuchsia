// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_ui_composition::{ScreenshotFormat, ScreenshotMarker, ScreenshotTakeRequest};
use fuchsia_component::client::connect_to_protocol;

use std::convert::TryInto;
use std::fs;
use std::io::{BufWriter, Write};

const SCREENSHOT_FILE: &'static str = "/custom_artifacts/screenshot.png";

/// Captures screenshot and saves it as PNG file to |SCREENSHOT_FILE|
/// panics if screenshot is blank.
pub async fn take_screenshot() {
    let screenshot =
        connect_to_protocol::<ScreenshotMarker>().expect("failed to connect to Screenshot");
    let data = screenshot
        .take(ScreenshotTakeRequest { format: Some(ScreenshotFormat::Png), ..Default::default() })
        .await
        .expect("cannot take screenshot using scenic");
    let image_vmo = data.vmo.expect("failed to obtain vmo handle for screenshot");
    let vmo_size: usize = match image_vmo.get_content_size() {
        Ok(size) if size > 0 => size.try_into().unwrap(),
        _ => image_vmo.get_size().expect("failed to obtain vmo size").try_into().unwrap(),
    };
    let screenshot_file = fs::File::create(SCREENSHOT_FILE)
        .unwrap_or_else(|e| panic!("cannot create file {}: {:?}", SCREENSHOT_FILE, e));
    let ref mut w = BufWriter::new(screenshot_file);

    let mut image_data = vec![0u8; vmo_size];
    let vmo_is_readable = image_vmo.read(&mut image_data, 0).is_ok();

    if !vmo_is_readable {
        let image_vmo_mapping = mapped_vmo::Mapping::create_from_vmo(
            &image_vmo,
            vmo_size,
            zx::VmarFlags::PERM_READ | zx::VmarFlags::ALLOW_FAULTS,
        )
        .expect("failed to map VMO");
        image_vmo_mapping.read_at(0, &mut image_data);
    }
    if is_image_blank(&image_data) {
        panic!("Captured screenshot is blank.");
    }
    w.write_all(&image_data).expect("failed to write image data as PNG");
    w.flush().unwrap();
}

fn is_image_blank(img_data: &[u8]) -> bool {
    let decoder = png::Decoder::new(std::io::Cursor::new(img_data));
    let mut reader = decoder.read_info().expect("failed to decode PNG header");
    let mut buf = vec![0; reader.output_buffer_size().expect("failed to get PNG buffer size")];
    let info = reader.next_frame(&mut buf).expect("failed to decode PNG frame");
    let pixels = &buf[..info.buffer_size()];
    pixels.iter().all(|&b| b == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_rgba_png(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        let mut encoder = png::Encoder::new(&mut buf, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(rgba).unwrap();
        drop(writer);
        buf
    }

    #[test]
    fn test_is_image_blank() {
        let blank_png = encode_rgba_png(2, 2, &[0u8; 2 * 2 * 4]);
        assert!(is_image_blank(&blank_png));

        let mut non_blank = vec![0u8; 2 * 2 * 4];
        non_blank[3] = 255;
        let non_blank_png = encode_rgba_png(2, 2, &non_blank);
        assert!(!is_image_blank(&non_blank_png));
    }
}
