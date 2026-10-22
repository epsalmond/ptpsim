use protocol_primitives::chunk_frame::{
    encode_window, source_sum16, ChunkChecksum, ChunkError, Direction, SourceSum16Decoder,
    MAX_CHUNK_WINDOWS, SENTINEL,
};
use serde_json::Value;

fn bytes(hex: &str) -> Vec<u8> {
    hex.as_bytes()
        .chunks_exact(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

fn header(index: u64, length: u64) -> Option<Vec<u8>> {
    Some(
        [
            u16::try_from(index).ok()?.to_le_bytes().as_slice(),
            u32::try_from(length).ok()?.to_le_bytes().as_slice(),
        ]
        .concat(),
    )
}

#[test]
fn public_synthetic_vectors() {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/chunk-frames.json")).unwrap();
    assert_eq!(fixture["vectors"].as_array().unwrap().len(), 21);
    for vector in fixture["vectors"].as_array().unwrap() {
        let direction = if vector["direction"] == "restore" {
            Direction::Restore
        } else {
            Direction::Backup
        };
        let size = vector["source_size"]
            .as_u64()
            .or_else(|| vector["expected_source_size"].as_u64())
            .unwrap() as usize;
        let mut decoder = SourceSum16Decoder::new(direction, 120, 4096, Some(size)).unwrap();
        let frames: Vec<_> = vector["frames_hex"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| bytes(v.as_str().unwrap()))
            .collect();
        let result = frames
            .iter()
            .try_for_each(|frame| decoder.push(frame))
            .and_then(|()| decoder.finish());
        if let Some(source) = vector["source_hex"].as_str() {
            let source = bytes(source);
            assert_eq!(result.unwrap(), source, "{}", vector["id"]);
            assert_eq!(
                u64::from(source_sum16(&source)),
                vector["checksum_u16"].as_u64().unwrap()
            );
            for (ordinal, expected) in frames.iter().enumerate() {
                let index = if ordinal + 1 == frames.len() {
                    SENTINEL
                } else {
                    ordinal as u64
                };
                let mut actual = encode_window(
                    &source,
                    120,
                    index,
                    SENTINEL,
                    Some(ChunkChecksum::SourceSum16Le),
                    header,
                )
                .unwrap();
                if direction == Direction::Backup {
                    actual.truncate(actual.len() - 2);
                }
                assert_eq!(&actual, expected, "{}", vector["id"]);
            }
            if direction == Direction::Backup {
                let mut decoder = SourceSum16Decoder::new(direction, 120, 4096, None).unwrap();
                for mut frame in frames {
                    frame.extend([0x7b; 19]);
                    decoder.push(&frame).unwrap();
                }
                assert_eq!(decoder.finish().unwrap(), source);
            }
        } else {
            let expected = match vector["proposed_validation_error"].as_str().unwrap() {
                "checksum-mismatch" => ChunkError::Checksum,
                "truncated-body" => ChunkError::TruncatedBody,
                "restore-trailer-policy" => ChunkError::Trailer,
                "short-frame" => ChunkError::ShortFrame,
                "short-body" => ChunkError::ShortBody,
                "missing-sentinel-policy" => ChunkError::MissingSentinel,
                "size-mismatch-policy" => ChunkError::Size,
                "after-sentinel-policy" => ChunkError::AfterSentinel,
                "ordinal-policy" => ChunkError::Sequence,
                error => panic!("unhandled fixture error {error}"),
            };
            assert_eq!(result, Err(expected), "{}", vector["id"]);
        }
    }
    assert_eq!(
        encode_window(
            &[0x80, 0xff],
            120,
            SENTINEL,
            SENTINEL,
            Some(ChunkChecksum::SourceSum16Le),
            header
        )
        .unwrap(),
        bytes("ffff0400000080ff7f017f01")
    );
}

#[test]
fn bounds_and_error_state_are_explicit() {
    let encode = |source: &[u8], size, index| {
        encode_window(
            source,
            size,
            index,
            SENTINEL,
            Some(ChunkChecksum::SourceSum16Le),
            header,
        )
    };
    assert_eq!(encode(&[], 120, SENTINEL), Err(ChunkError::EmptySource));
    assert_eq!(encode(&[1, 2], 1, SENTINEL), Err(ChunkError::Layout));
    assert_eq!(encode(&[1, 2], 120, u64::MAX), Err(ChunkError::Index));
    assert_eq!(
        encode(&vec![1; (MAX_CHUNK_WINDOWS + 1) * 2], 2, SENTINEL),
        Err(ChunkError::Limit)
    );
    assert_eq!(
        encode_window(
            &[1, 2],
            120,
            SENTINEL,
            SENTINEL,
            Some(ChunkChecksum::SourceSum16Le),
            |_, _| Some(vec![])
        ),
        Err(ChunkError::Layout)
    );
    let final_frame = encode(&[0x80, 0xff], 120, SENTINEL).unwrap();
    let mut decoder = SourceSum16Decoder::new(Direction::Restore, 120, 1, None).unwrap();
    assert_eq!(decoder.push(&final_frame), Err(ChunkError::Limit));
    let mut decoder = SourceSum16Decoder::new(Direction::Restore, 120, 2, Some(2)).unwrap();
    assert_eq!(
        decoder.push(&[0, 0, 255, 255, 255, 255, 0, 0]),
        Err(ChunkError::TruncatedBody)
    );
    assert_eq!(
        decoder.push(&bytes("00000200000080ff0000")),
        Err(ChunkError::WindowLength)
    );
    let mut corrupt = final_frame.clone();
    corrupt[8] ^= 1;
    assert_eq!(decoder.push(&corrupt), Err(ChunkError::Checksum));
    decoder.push(&final_frame).unwrap();
    assert_eq!(decoder.finish().unwrap(), [0x80, 0xff]);
    let mut decoder = SourceSum16Decoder::new(Direction::Backup, 2, 3, None).unwrap();
    assert_eq!(
        decoder.push(&bytes("000003000000010203")),
        Err(ChunkError::WindowLength)
    );
    let mut decoder = SourceSum16Decoder::new(Direction::Restore, 120, 2, Some(2)).unwrap();
    let mut extra = final_frame.clone();
    extra.push(0);
    assert_eq!(decoder.push(&extra), Err(ChunkError::Trailer));
    decoder.push(&final_frame).unwrap();
    assert_eq!(decoder.finish().unwrap(), [0x80, 0xff]);
    let mut decoder =
        SourceSum16Decoder::new(Direction::Backup, 2, MAX_CHUNK_WINDOWS * 2 + 2, None).unwrap();
    for index in 0..MAX_CHUNK_WINDOWS {
        let mut frame = header(index as u64, 2).unwrap();
        frame.extend([1, 1]);
        decoder.push(&frame).unwrap();
    }
    assert_eq!(decoder.push(&final_frame), Err(ChunkError::Limit));
    let mut decoder = SourceSum16Decoder::new(Direction::Backup, 120, 2, None).unwrap();
    assert_eq!(
        decoder.push(&bytes("ffff020000000000")),
        Err(ChunkError::EmptySource)
    );
    // Without independent size, this is a valid shorter source.
    decoder.push(&final_frame).unwrap();
    assert_eq!(decoder.finish().unwrap(), [0x80, 0xff]);
}
