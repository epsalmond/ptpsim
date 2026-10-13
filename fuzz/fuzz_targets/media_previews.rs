#![no_main]

use libfuzzer_sys::fuzz_target;

const MAX_THUMBNAIL_BYTES: usize = 4096;

fn jpeg_with_thumbnail(data: &[u8]) -> Vec<u8> {
    let mut thumbnail = vec![0xff, 0xd8];
    thumbnail.extend_from_slice(&data[..data.len().min(MAX_THUMBNAIL_BYTES)]);
    thumbnail.extend_from_slice(&[0xff, 0xd9]);

    let thumbnail_offset = 44u32;
    let mut tiff = Vec::new();
    tiff.extend_from_slice(b"II");
    tiff.extend_from_slice(&42u16.to_le_bytes());
    tiff.extend_from_slice(&8u32.to_le_bytes());
    tiff.extend_from_slice(&0u16.to_le_bytes());
    tiff.extend_from_slice(&14u32.to_le_bytes());
    tiff.extend_from_slice(&2u16.to_le_bytes());
    for (tag, value) in [
        (0x0201u16, thumbnail_offset),
        (0x0202u16, thumbnail.len() as u32),
    ] {
        tiff.extend_from_slice(&tag.to_le_bytes());
        tiff.extend_from_slice(&4u16.to_le_bytes());
        tiff.extend_from_slice(&1u32.to_le_bytes());
        tiff.extend_from_slice(&value.to_le_bytes());
    }
    tiff.extend_from_slice(&0u32.to_le_bytes());
    tiff.extend_from_slice(&thumbnail);

    let mut app1 = b"Exif\0\0".to_vec();
    app1.extend_from_slice(&tiff);
    let mut jpeg = vec![0xff, 0xd8, 0xff, 0xe1];
    jpeg.extend_from_slice(&((app1.len() + 2) as u16).to_be_bytes());
    jpeg.extend_from_slice(&app1);
    jpeg.extend_from_slice(&[0xff, 0xd9]);
    jpeg
}

fn raf_with_preview(preview: &[u8]) -> Vec<u8> {
    let offset = 0x94usize;
    let mut raf = vec![0u8; offset];
    raf[..b"FUJIFILMCCD-RAW".len()].copy_from_slice(b"FUJIFILMCCD-RAW");
    raf[0x54..0x58].copy_from_slice(&(offset as u32).to_be_bytes());
    raf[0x58..0x5c].copy_from_slice(&(preview.len() as u32).to_be_bytes());
    raf.extend_from_slice(preview);
    raf
}

fuzz_target!(|data: &[u8]| {
    camera_media_store::fuzz_preview_parsers(data);
    let jpeg = jpeg_with_thumbnail(data);
    camera_media_store::fuzz_preview_parsers(&jpeg);
    camera_media_store::fuzz_preview_parsers(&raf_with_preview(&jpeg));
});
