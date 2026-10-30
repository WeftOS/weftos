//! Validate an RVF segment dumped from the N6 with the host's own RVF reader.
//!
//! Usage: weftos-n6-leaf-hostcheck <segment.bin>
use std::process::ExitCode;

use rvf_types::SegmentType;
use rvf_wire::{read_segment, validate_segment};

fn main() -> ExitCode {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: weftos-n6-leaf-hostcheck <segment.bin>");
        return ExitCode::from(2);
    };
    let data = match std::fs::read(&path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("read {path}: {e}");
            return ExitCode::from(2);
        }
    };

    let (header, payload) = match read_segment(&data) {
        Ok(v) => v,
        Err(e) => {
            println!("FAIL  read_segment: {e:?}");
            return ExitCode::FAILURE;
        }
    };
    let seg_type = SegmentType::try_from(header.seg_type);
    println!(
        "header: magic=0x{:08X} v{} type={:?} id={} payload={} B checksum_algo={} pad={}",
        header.magic, header.version, seg_type, header.segment_id, header.payload_length,
        header.checksum_algo, header.alignment_pad
    );
    if let Err(e) = validate_segment(&header, payload) {
        println!("FAIL  validate_segment (content hash): {e:?}");
        return ExitCode::FAILURE;
    }
    println!("PASS  rvf-wire read_segment + validate_segment (XXH3-128 content hash)");

    // VEC payload: dim u16 | count u32 | count × (id u64 | f32 × dim)
    let dim = u16::from_le_bytes([payload[0], payload[1]]) as usize;
    let count = u32::from_le_bytes(payload[2..6].try_into().unwrap()) as usize;
    let mut at = 6;
    for _ in 0..count {
        let id = u64::from_le_bytes(payload[at..at + 8].try_into().unwrap());
        at += 8;
        let v: Vec<f32> = (0..dim)
            .map(|j| f32::from_le_bytes(payload[at + 4 * j..at + 4 * j + 4].try_into().unwrap()))
            .collect();
        at += 4 * dim;
        println!("  vec id={id} {v:?}");
    }
    ExitCode::SUCCESS
}
