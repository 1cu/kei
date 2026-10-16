//! Sparse synthetic inputs exercise the real probe, prepare, validation and
//! publication graph under actual Rust allocation high-water measurement.

use super::heif_writer::prepare_metadata_heif;
use super::probe::{HeifNativeCaptureTime, probe_exif};
use super::test_support::embed_temp_entries;
use super::values::MetadataWrite;
use super::xmp_fields::ensure_initialized;
use crate::download::file::ExistingFileFingerprint;
use crate::download::heif;
use crate::test_helpers::allocation::{HeapHighWater, measure_heap_high_water};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

const BODY_MARKER: &[u8] = b"kei sparse synthetic media";
// Longer than the copy buffer, so it crosses a boundary regardless of the
// preceding metadata patch offsets.
const BOUNDARY_MARKER: &[u8] = &[b'z'; 64 * 1024 + 37];
const TAIL_MARKER: &[u8] = b"unchanged final media bytes";
const BOUNDARY_OFFSET: u64 = 64 * 1024 - 9;

fn capture_tiff() -> Vec<u8> {
    // IFD0 points at the two-entry Exif IFD. ASCII storage is deliberately
    // separate, so a native repair can change only its intended fields.
    let mut bytes = b"II\x2a\0\x08\0\0\0".to_vec();
    let entry = |tag: u16, kind: u16, count: u32, value: u32| {
        [
            tag.to_le_bytes().as_slice(),
            kind.to_le_bytes().as_slice(),
            count.to_le_bytes().as_slice(),
            value.to_le_bytes().as_slice(),
        ]
        .concat()
    };
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&entry(0x8769, 4, 1, 26));
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes.extend_from_slice(&2_u16.to_le_bytes());
    bytes.extend_from_slice(&entry(0x9003, 2, 20, 56));
    bytes.extend_from_slice(&entry(0x9011, 2, 7, 76));
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes.extend_from_slice(b"2023:09:03 09:28:14\0+03:00\0");
    bytes
}

fn sparse_heif(path: &Path, size: u64) -> u64 {
    let mut prefix = heif::apple_multi_exif_heic(&capture_tiff());
    let body_start = prefix.len() as u64 + 16;
    // The primary grid's first hvc1 tile now owns the entire sparse body, so
    // production post-rewrite validation must compare every large media byte.
    heif::set_test_file_item_extent(
        &mut prefix,
        2,
        u32::try_from(body_start).unwrap(),
        u32::try_from(size - body_start).unwrap(),
    );
    let mut file = File::create(path).unwrap();
    file.write_all(&prefix).unwrap();
    let mdat_len = size - prefix.len() as u64;
    file.write_all(&1_u32.to_be_bytes()).unwrap();
    file.write_all(b"mdat").unwrap();
    file.write_all(&mdat_len.to_be_bytes()).unwrap();
    file.write_all(BODY_MARKER).unwrap();
    file.seek(SeekFrom::Start(body_start + BOUNDARY_OFFSET))
        .unwrap();
    file.write_all(BOUNDARY_MARKER).unwrap();
    file.set_len(size).unwrap();
    file.seek(SeekFrom::End(-(TAIL_MARKER.len() as i64)))
        .unwrap();
    file.write_all(TAIL_MARKER).unwrap();
    size - body_start
}

fn fingerprint(path: &Path) -> ExistingFileFingerprint {
    let mut file = File::open(path).unwrap();
    let size = file.metadata().unwrap().len();
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer).unwrap();
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    ExistingFileFingerprint {
        size,
        sha256: hash.finalize().into(),
    }
}

fn assert_sparse_media_preserved(path: &Path, expected_body_len: u64) {
    let mut file = File::open(path).unwrap();
    let end = file.metadata().unwrap().len();
    let mut start = 0_u64;
    while start < end {
        file.seek(SeekFrom::Start(start)).unwrap();
        let mut head = [0_u8; 16];
        file.read_exact(&mut head[..8]).unwrap();
        let size32 = u32::from_be_bytes(head[..4].try_into().unwrap());
        let (size, header_len) = match size32 {
            0 => (end - start, 8),
            1 => {
                file.read_exact(&mut head[8..]).unwrap();
                (u64::from_be_bytes(head[8..].try_into().unwrap()), 16)
            }
            size => (u64::from(size), 8),
        };
        assert!(size >= header_len && start + size <= end);
        if &head[4..8] == b"mdat" && size - header_len == expected_body_len {
            let body = start + header_len;
            for (offset, expected) in [
                (0, BODY_MARKER),
                (BOUNDARY_OFFSET, BOUNDARY_MARKER),
                (expected_body_len - TAIL_MARKER.len() as u64, TAIL_MARKER),
            ] {
                file.seek(SeekFrom::Start(body + offset)).unwrap();
                let mut actual = vec![0_u8; expected.len()];
                file.read_exact(&mut actual).unwrap();
                assert_eq!(actual, expected, "unchanged media at {offset}");
            }
            file.seek(SeekFrom::Start(
                body + BOUNDARY_OFFSET + BOUNDARY_MARKER.len() as u64 + 4096,
            ))
            .unwrap();
            let mut hole = [1_u8; 32];
            file.read_exact(&mut hole).unwrap();
            assert_eq!(hole, [0_u8; 32]);
            return;
        }
        start += size;
    }
    panic!("rewritten container lost its large synthetic mdat");
}

fn assert_heap_bound(measured: &HeapHighWater, stage: &str) {
    assert!(
        measured.peak_growth < 4 * 1024 * 1024,
        "{stage} retained whole-file bytes: {measured:?}"
    );
    assert!(
        measured.largest_allocation < 1024 * 1024,
        "{stage} allocated a whole-file buffer: {measured:?}"
    );
}

#[test]
fn heif_sparse_metadata_heap_is_independent_of_media_size() {
    ensure_initialized();
    let dir = tempfile::tempdir().unwrap();
    let mut peaks = Vec::new();
    for size in [1024 * 1024, 128 * 1024 * 1024] {
        let path = dir.path().join(format!("sparse-{size}.heic"));
        let media_len = sparse_heif(&path, size);
        let expected_input = fingerprint(&path);
        let (probe, measured_probe) = measure_heap_high_water(|| probe_exif(&path).unwrap());
        assert_eq!(
            probe.datetime_original.as_deref(),
            Some("2023:09:03 09:28:14")
        );
        assert_heap_bound(&measured_probe, "probe");

        let write = MetadataWrite {
            datetime: Some("2024:06:15 10:00:00".into()),
            offset_time_original: Some("+11:00".into()),
            require_native_heif_capture_time: true,
            rating: Some(5),
            ..MetadataWrite::default()
        };
        let (expected_output, measured_prepare) = measure_heap_high_water(|| {
            let prepared =
                prepare_metadata_heif(&path, &write, ".meta-tmp", Some(expected_input)).unwrap();
            let output = prepared.output_fingerprint();
            assert_eq!(prepared.publish(&path).unwrap(), output);
            output
        });
        assert_heap_bound(
            &measured_prepare,
            "native repair, XMP insertion, validation and publication",
        );
        assert_eq!(fingerprint(&path), expected_output);
        assert_sparse_media_preserved(&path, media_len);
        let repaired = probe_exif(&path).unwrap();
        assert!(matches!(repaired.native_heif_capture_time,
            HeifNativeCaptureTime::Present { ref datetime_original, ref offset_time_original, .. }
                if datetime_original.as_deref() == Some("2024:06:15 10:00:00")
                    && offset_time_original.as_deref() == Some("+11:00")));

        let replacement = MetadataWrite {
            rating: Some(2),
            ..MetadataWrite::default()
        };
        let (_, measured_replace) = measure_heap_high_water(|| {
            prepare_metadata_heif(&path, &replacement, ".meta-tmp", Some(expected_output))
                .unwrap()
                .publish(&path)
                .unwrap();
        });
        assert_heap_bound(&measured_replace, "existing XMP replacement and validation");
        assert_sparse_media_preserved(&path, media_len);
        assert!(embed_temp_entries(dir.path(), ".meta-tmp").is_empty());
        peaks.push((
            measured_probe.peak_growth,
            measured_prepare.peak_growth,
            measured_replace.peak_growth,
        ));
    }
    for (small, large) in [
        (peaks[0].0, peaks[1].0),
        (peaks[0].1, peaks[1].1),
        (peaks[0].2, peaks[1].2),
    ] {
        assert!(
            large <= small + 64 * 1024,
            "heap high-water grew with unchanged media: 1 MiB={small}, 128 MiB={large}"
        );
    }
    eprintln!(
        "HEIF real Rust heap peaks (probe, native prepare/publish, XMP replace/publish): {peaks:?}"
    );
}

#[test]
fn heif_sparse_stale_input_preserves_media_and_cleans_temporary_files() {
    ensure_initialized();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stale.heic");
    sparse_heif(&path, 1024 * 1024);
    let approved = fingerprint(&path);
    let mut source = fs::OpenOptions::new().write(true).open(&path).unwrap();
    source.seek(SeekFrom::End(-1)).unwrap();
    source.write_all(b"!").unwrap();
    drop(source);
    let external = fingerprint(&path);
    let error = prepare_metadata_heif(
        &path,
        &MetadataWrite {
            rating: Some(5),
            ..MetadataWrite::default()
        },
        ".meta-tmp",
        Some(approved),
    )
    .err()
    .expect("stale planned source must refuse");
    assert!(crate::download::file::classify_conditional_publish_error(&error).target_changed);
    assert_eq!(fingerprint(&path), external);
    assert!(embed_temp_entries(dir.path(), ".meta-tmp").is_empty());
}
