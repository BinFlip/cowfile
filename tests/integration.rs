//! Integration tests for the cowfile crate.

use std::{
    io::{Read, Seek, SeekFrom, Write},
    sync::Arc,
    thread,
};

use cowfile::{CowFile, ReadFrom, WriteTo};

#[test]
fn test_end_to_end_vec() {
    // Create a 1KB buffer with recognizable pattern.
    let mut data = vec![0u8; 1024];
    for (i, byte) in data.iter_mut().enumerate() {
        *byte = (i % 256) as u8;
    }

    let pf = CowFile::from_vec(data.clone());

    // Pass 1: Patch the first 4 bytes (MZ header simulation).
    pf.write(0, &[0x4D, 0x5A, 0x90, 0x00]).unwrap();
    pf.commit().unwrap();

    // Pass 2: Patch a region in the middle.
    pf.write(512, &[0xDE, 0xAD, 0xBE, 0xEF]).unwrap();
    pf.commit().unwrap();

    // Pass 3: Patch near the end.
    pf.write(1020, &[0xCA, 0xFE, 0xBA, 0xBE]).unwrap();

    // Verify reads.
    assert_eq!(pf.read(0, 4).unwrap(), vec![0x4D, 0x5A, 0x90, 0x00]);
    assert_eq!(pf.read(512, 4).unwrap(), vec![0xDE, 0xAD, 0xBE, 0xEF]);
    assert_eq!(pf.read(1020, 4).unwrap(), vec![0xCA, 0xFE, 0xBA, 0xBE]);

    // Unmodified regions should match original.
    assert_eq!(pf.read(4, 4).unwrap(), data[4..8]);
    assert_eq!(pf.read(100, 10).unwrap(), data[100..110]);

    // Produce output.
    let output = pf.to_vec().unwrap();
    assert_eq!(output.len(), 1024);
    assert_eq!(&output[0..4], &[0x4D, 0x5A, 0x90, 0x00]);
    assert_eq!(&output[512..516], &[0xDE, 0xAD, 0xBE, 0xEF]);
    assert_eq!(&output[1020..1024], &[0xCA, 0xFE, 0xBA, 0xBE]);

    // Verify unmodified regions preserved in output.
    assert_eq!(&output[4..512], &data[4..512]);
    assert_eq!(&output[516..1020], &data[516..1020]);

    // Base data should be completely unchanged.
    assert_eq!(pf.base_data(), &data[..]);
}

#[test]
fn test_end_to_end_mmap() {
    // Create a temporary file with known content.
    let mut tmpfile = tempfile::NamedTempFile::new().unwrap();
    let original: Vec<u8> = (0..256).map(|i| i as u8).collect();
    tmpfile.write_all(&original).unwrap();
    tmpfile.flush().unwrap();

    let pf = CowFile::from_path(tmpfile.path()).unwrap();
    assert_eq!(pf.len(), 256);

    // Modify and verify.
    pf.write(0, &[0xFF]).unwrap();
    pf.write(255, &[0x00]).unwrap();
    pf.commit().unwrap();

    let output = pf.to_vec().unwrap();
    assert_eq!(output[0], 0xFF);
    assert_eq!(output[1], 1); // Original
    assert_eq!(output[255], 0x00);

    // Base data unchanged.
    assert_eq!(pf.base_data(), &original[..]);
}

#[test]
fn test_deobfuscation_simulation() {
    // Simulate a multi-pass deobfuscation pipeline.
    let size = 4096;
    let mut base = vec![0u8; size];

    // Fill with "encrypted" pattern.
    for (i, byte) in base.iter_mut().enumerate() {
        *byte = (i ^ 0xAA) as u8;
    }

    let pf = CowFile::from_vec(base);

    // Pass 1: "Decrypt" the first section (bytes 0..1024).
    for i in 0..1024u64 {
        let original = pf.read_byte(i).unwrap();
        let decrypted = original ^ 0xAA;
        pf.write_byte(i, decrypted).unwrap();
    }
    pf.commit().unwrap();

    // Pass 2: "Decrypt" the second section (bytes 1024..2048).
    for i in 1024..2048u64 {
        let original = pf.read_byte(i).unwrap();
        let decrypted = original ^ 0xAA;
        pf.write_byte(i, decrypted).unwrap();
    }
    pf.commit().unwrap();

    // Pass 3: "Decrypt" the rest.
    for i in 2048..4096u64 {
        let original = pf.read_byte(i).unwrap();
        let decrypted = original ^ 0xAA;
        pf.write_byte(i, decrypted).unwrap();
    }
    pf.commit().unwrap();

    // Verify all bytes are "decrypted" (should be sequential indices).
    let output = pf.to_vec().unwrap();
    for (i, &byte) in output.iter().enumerate() {
        assert_eq!(byte, (i % 256) as u8, "mismatch at offset {i}");
    }
}

#[test]
fn test_to_file_matches_to_vec() {
    let pf = CowFile::from_vec(vec![0u8; 500]);

    // Apply various modifications.
    pf.write(0, &[0xAA, 0xBB, 0xCC]).unwrap();
    pf.commit().unwrap();
    pf.write(100, &[0xDD, 0xEE]).unwrap();
    pf.write(200, &[0xFF]).unwrap();

    // Get both outputs.
    let vec_output = pf.to_vec().unwrap();

    let tmpfile = tempfile::NamedTempFile::new().unwrap();
    pf.to_file(tmpfile.path()).unwrap();
    let file_output = std::fs::read(tmpfile.path()).unwrap();

    // They must be byte-identical.
    assert_eq!(vec_output, file_output);
}

#[test]
fn test_mmap_to_file_roundtrip() {
    // Create initial file.
    let mut tmpfile = tempfile::NamedTempFile::new().unwrap();
    tmpfile.write_all(&[0u8; 256]).unwrap();
    tmpfile.flush().unwrap();

    // Open with cowfile, modify, write to new file.
    let pf = CowFile::from_path(tmpfile.path()).unwrap();
    pf.write(0, &[0x4D, 0x5A]).unwrap();
    pf.write(128, &[0xFF; 16]).unwrap();
    pf.commit().unwrap();

    let outfile = tempfile::NamedTempFile::new().unwrap();
    pf.to_file(outfile.path()).unwrap();

    // Open the output file with cowfile and verify.
    let pf2 = CowFile::from_path(outfile.path()).unwrap();
    assert_eq!(pf2.len(), 256);
    assert_eq!(pf2.read(0, 2).unwrap(), vec![0x4D, 0x5A]);
    assert_eq!(pf2.read(128, 16).unwrap(), vec![0xFF; 16]);
    assert_eq!(pf2.read_byte(2).unwrap(), 0x00);
}

#[test]
fn test_concurrent_read_write_isolation() {
    // Verify that reads from one thread see a consistent snapshot
    // even when another thread is writing.
    let pf = Arc::new(CowFile::from_vec(vec![0u8; 1000]));

    // Fill with initial pattern and commit.
    for i in 0..1000u64 {
        pf.write_byte(i, 0xAA).unwrap();
    }
    pf.commit().unwrap();

    let pf_reader = Arc::clone(&pf);
    let pf_writer = Arc::clone(&pf);

    let reader = thread::spawn(move || {
        for _ in 0..500 {
            let data = pf_reader.read(0, 1000).unwrap();
            // Every byte should be either 0xAA (committed) or 0xBB (pending write).
            for &byte in &data {
                assert!(byte == 0xAA || byte == 0xBB, "unexpected byte: {byte:#04X}");
            }
        }
    });

    let writer = thread::spawn(move || {
        for i in 0..1000u64 {
            pf_writer.write_byte(i, 0xBB).unwrap();
        }
    });

    writer.join().unwrap();
    reader.join().unwrap();
}

#[test]
fn test_overlapping_write_regions() {
    let pf = CowFile::from_vec(vec![0u8; 20]);

    // Write overlapping regions in the same pass.
    pf.write(0, &[0xAA; 10]).unwrap(); // [0..10)
    pf.write(5, &[0xBB; 10]).unwrap(); // [5..15) - overlaps

    let data = pf.read(0, 20).unwrap();
    // [0..5) should be 0xAA, [5..15) should be 0xBB, [15..20) should be 0x00.
    assert!(data[..5].iter().all(|&b| b == 0xAA));
    assert!(data[5..15].iter().all(|&b| b == 0xBB));
    assert!(data[15..20].iter().all(|&b| b == 0x00));

    pf.commit().unwrap();

    // After commit, result should be the same.
    let committed = pf.to_vec().unwrap();
    assert_eq!(&committed[..], &data[..]);
}

#[test]
fn test_overwrite_committed_with_pending() {
    let pf = CowFile::from_vec(vec![0u8; 10]);

    // Commit some data.
    pf.write(0, &[0xAA; 5]).unwrap();
    pf.commit().unwrap();

    // Overwrite part of it with pending.
    pf.write(2, &[0xBB; 3]).unwrap();

    let data = pf.read(0, 5).unwrap();
    assert_eq!(data, vec![0xAA, 0xAA, 0xBB, 0xBB, 0xBB]);

    // Commit again and verify.
    pf.commit().unwrap();
    let output = pf.to_vec().unwrap();
    assert_eq!(&output[0..5], &[0xAA, 0xAA, 0xBB, 0xBB, 0xBB]);
    assert_eq!(&output[5..10], &[0x00; 5]);
}

#[test]
fn test_error_types() {
    let pf = CowFile::from_vec(vec![0u8; 10]);

    // OutOfBounds on read.
    let err = pf.read(5, 10).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("offset 5"));
    assert!(msg.contains("length 10"));
    assert!(msg.contains("file size 10"));

    // OutOfBounds on write.
    let err = pf.write(10, &[0xFF]).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("offset 10"));

    // Nonexistent file.
    let err = CowFile::from_path("/this/does/not/exist.bin").unwrap_err();
    assert!(matches!(err, cowfile::Error::Io(_)));
}

#[test]
fn test_typed_io_multipass() {
    let pf = CowFile::from_vec(vec![0u8; 64]);

    // Pass 1: Write header fields.
    pf.write_le::<u32>(0, 0x4D5A9000).unwrap();
    pf.write_le::<u16>(4, 3).unwrap();
    pf.commit().unwrap();

    // Pass 2: Write data fields.
    pf.write_le::<u64>(8, 0x0123456789ABCDEF).unwrap();
    pf.write_be::<u32>(16, 0xCAFEBABE).unwrap();
    pf.commit().unwrap();

    // Read back and verify all values.
    assert_eq!(pf.read_le::<u32>(0).unwrap(), 0x4D5A9000);
    assert_eq!(pf.read_le::<u16>(4).unwrap(), 3);
    assert_eq!(pf.read_le::<u64>(8).unwrap(), 0x0123456789ABCDEF);
    assert_eq!(pf.read_be::<u32>(16).unwrap(), 0xCAFEBABE);
}

#[test]
fn test_typed_io_with_mmap() {
    let mut tmpfile = tempfile::NamedTempFile::new().unwrap();
    tmpfile.write_all(&[0u8; 64]).unwrap();
    tmpfile.flush().unwrap();

    let pf = CowFile::from_path(tmpfile.path()).unwrap();
    pf.write_le::<u32>(0, 0xDEADBEEF).unwrap();
    pf.write_le::<u16>(4, 42).unwrap();
    pf.commit().unwrap();

    assert_eq!(pf.read_le::<u32>(0).unwrap(), 0xDEADBEEF);
    assert_eq!(pf.read_le::<u16>(4).unwrap(), 42);
}

#[test]
fn test_read_write_type_roundtrip() {
    struct ImageHeader {
        width: u32,
        height: u32,
        bpp: u16,
        flags: u16,
    }

    impl ReadFrom for ImageHeader {
        fn read_from(pf: &CowFile, offset: u64) -> cowfile::Result<Self> {
            Ok(ImageHeader {
                width: pf.read_le::<u32>(offset)?,
                height: pf.read_le::<u32>(offset + 4)?,
                bpp: pf.read_le::<u16>(offset + 8)?,
                flags: pf.read_le::<u16>(offset + 10)?,
            })
        }
    }

    impl WriteTo for ImageHeader {
        fn write_to(&self, pf: &CowFile, offset: u64) -> cowfile::Result<()> {
            pf.write_le::<u32>(offset, self.width)?;
            pf.write_le::<u32>(offset + 4, self.height)?;
            pf.write_le::<u16>(offset + 8, self.bpp)?;
            pf.write_le::<u16>(offset + 10, self.flags)?;
            Ok(())
        }
    }

    let pf = CowFile::from_vec(vec![0u8; 64]);

    let header = ImageHeader {
        width: 1920,
        height: 1080,
        bpp: 32,
        flags: 0x0F,
    };

    pf.write_type(0, &header).unwrap();
    pf.commit().unwrap();

    let read_back: ImageHeader = pf.read_type(0).unwrap();
    assert_eq!(read_back.width, 1920);
    assert_eq!(read_back.height, 1080);
    assert_eq!(read_back.bpp, 32);
    assert_eq!(read_back.flags, 0x0F);
}

#[test]
fn test_cursor_read_write_seek_roundtrip() {
    let pf = CowFile::from_vec(vec![0u8; 128]);

    {
        let mut cursor = pf.cursor();

        // Write a header via cursor.
        cursor.write_all(&[0x4D, 0x5A, 0x90, 0x00]).unwrap();

        // Seek to offset 64 and write more data.
        cursor.seek(SeekFrom::Start(64)).unwrap();
        cursor.write_all(&[0xDE, 0xAD, 0xBE, 0xEF]).unwrap();

        // Seek back and verify.
        cursor.seek(SeekFrom::Start(0)).unwrap();
        let mut header = [0u8; 4];
        cursor.read_exact(&mut header).unwrap();
        assert_eq!(header, [0x4D, 0x5A, 0x90, 0x00]);

        cursor.seek(SeekFrom::Start(64)).unwrap();
        let mut payload = [0u8; 4];
        cursor.read_exact(&mut payload).unwrap();
        assert_eq!(payload, [0xDE, 0xAD, 0xBE, 0xEF]);
    }

    // Verify via raw cowfile API.
    assert_eq!(pf.read(0, 4).unwrap(), vec![0x4D, 0x5A, 0x90, 0x00]);
    assert_eq!(pf.read(64, 4).unwrap(), vec![0xDE, 0xAD, 0xBE, 0xEF]);
}

#[test]
fn test_cursor_std_io_compatibility() {
    fn read_u32_from<R: Read + Seek>(reader: &mut R, offset: u64) -> u32 {
        reader.seek(SeekFrom::Start(offset)).unwrap();
        let mut buf = [0u8; 4];
        reader.read_exact(&mut buf).unwrap();
        u32::from_le_bytes(buf)
    }

    let pf = CowFile::from_vec(vec![0u8; 32]);
    pf.write_le::<u32>(0, 0xDEADBEEF).unwrap();
    pf.write_le::<u32>(16, 0xCAFEBABE).unwrap();

    let mut cursor = pf.cursor();
    assert_eq!(read_u32_from(&mut cursor, 0), 0xDEADBEEF);
    assert_eq!(read_u32_from(&mut cursor, 16), 0xCAFEBABE);
}

#[test]
fn test_cursor_multiple_independent() {
    let pf = CowFile::from_vec(vec![0u8; 32]);

    // Two cursors writing to different regions simultaneously.
    let mut c1 = pf.cursor();
    let mut c2 = pf.cursor();

    c1.seek(SeekFrom::Start(0)).unwrap();
    c2.seek(SeekFrom::Start(16)).unwrap();

    c1.write_all(&[0xAA; 8]).unwrap();
    c2.write_all(&[0xBB; 8]).unwrap();

    let output = pf.to_vec().unwrap();
    assert!(output[..8].iter().all(|&b| b == 0xAA));
    assert!(output[8..16].iter().all(|&b| b == 0x00));
    assert!(output[16..24].iter().all(|&b| b == 0xBB));
    assert!(output[24..32].iter().all(|&b| b == 0x00));
}

#[test]
fn test_error_into_io_error() {
    let pf = CowFile::from_vec(vec![0u8; 4]);
    let mut cursor = pf.cursor();

    // Seek past the end and try to write — should get an io::Error.
    cursor.seek(SeekFrom::Start(3)).unwrap();
    let result = cursor.write_all(&[0xFF; 4]);
    assert!(result.is_err());
}

#[test]
fn test_three_layer_composition() {
    // Base has sequential pattern, committed overwrites middle, pending overwrites
    // a range that partially overlaps both committed and base.
    let base: Vec<u8> = (0..32).collect();
    let pf = CowFile::from_vec(base);

    // Committed: overwrite [8..16) with 0xAA.
    pf.write(8, &[0xAA; 8]).unwrap();
    pf.commit().unwrap();

    // Pending: overwrite [12..20) with 0xBB — overlaps committed at [12..16).
    pf.write(12, &[0xBB; 8]).unwrap();

    // Read [4..24) — spans all three layers.
    let data = pf.read(4, 20).unwrap();

    // [4..8) = base (4,5,6,7)
    assert_eq!(&data[0..4], &[4, 5, 6, 7]);
    // [8..12) = committed 0xAA
    assert!(data[4..8].iter().all(|&b| b == 0xAA));
    // [12..20) = pending 0xBB (overwrites committed in [12..16) and base in [16..20))
    assert!(data[8..16].iter().all(|&b| b == 0xBB));
    // [20..24) = base (20,21,22,23)
    assert_eq!(&data[16..20], &[20, 21, 22, 23]);
}

#[test]
fn test_commit_merges_overlapping_committed_entries() {
    // Two passes where committed entries from different passes overlap.
    let pf = CowFile::from_vec(vec![0u8; 30]);

    // Pass 1: Two separate regions.
    pf.write(0, &[0xAA; 5]).unwrap();
    pf.write(10, &[0xBB; 5]).unwrap();
    pf.commit().unwrap();

    // Pass 2: Bridge the gap between them.
    pf.write(3, &[0xCC; 9]).unwrap();
    pf.commit().unwrap();

    // Verify final output.
    let output = pf.to_vec().unwrap();
    assert!(output[..3].iter().all(|&b| b == 0xAA));
    assert!(output[3..12].iter().all(|&b| b == 0xCC));
    assert!(output[12..15].iter().all(|&b| b == 0xBB));
    assert!(output[15..30].iter().all(|&b| b == 0x00));
}

#[test]
fn test_pending_fully_covering_committed() {
    let pf = CowFile::from_vec(vec![0u8; 20]);

    // Commit a small region.
    pf.write(5, &[0xAA; 5]).unwrap();
    pf.commit().unwrap();

    // Pending write that entirely covers it.
    pf.write(0, &[0xBB; 20]).unwrap();

    let output = pf.to_vec().unwrap();
    assert!(output.iter().all(|&b| b == 0xBB));
}

#[test]
fn test_single_byte_operations_overlap() {
    let pf = CowFile::from_vec(vec![0u8; 10]);

    // Write single bytes to the same offset across commit boundaries.
    pf.write_byte(5, 0xAA).unwrap();
    pf.commit().unwrap();
    pf.write_byte(5, 0xBB).unwrap();
    pf.commit().unwrap();
    pf.write_byte(5, 0xCC).unwrap();

    assert_eq!(pf.read_byte(5).unwrap(), 0xCC);

    pf.commit().unwrap();
    assert_eq!(pf.read_byte(5).unwrap(), 0xCC);
}
