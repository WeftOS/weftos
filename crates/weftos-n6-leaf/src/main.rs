//! WeftOS leaf smoke on the STM32N6 (Cortex-M55), loaded into AXISRAM by
//! `scripts/n6.sh leaf`.
//!
//! 1. Builds an RVF `VEC` segment with `rvf-types` (no_std): 64-byte header,
//!    XXH3-128 content hash, payload `dim u16 | count u32 | count × (id u64 |
//!    f32 × dim)`, padded to 64 bytes. It lands in `N6_SEGMENT`, where the host
//!    dumps it and validates it independently with `rvf-wire`.
//! 2. Encodes a `weftos-leaf-types::LeafServices` announce to CBOR and
//!    decodes it back.
//! 3. Publishes the outcome in `N6_RESULT` and keeps a heartbeat counting.
#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use core::ptr::{addr_of, addr_of_mut, read_volatile, write_volatile};

use cortex_m as _; // links the single-core critical-section impl the allocator needs
use cortex_m_rt::entry;
use embedded_alloc::LlffHeap as Heap;
use rvf_types::{SegmentHeader, SegmentType, SEGMENT_ALIGNMENT, SEGMENT_HEADER_SIZE};
use weftos_leaf_types::{decode, encode, ComputeCap, LeafServices};

#[global_allocator]
static HEAP: Heap = Heap::empty();

const DIM: usize = 8;
const COUNT: usize = 4;
const PAYLOAD_LEN: usize = 2 + 4 + COUNT * (8 + 4 * DIM);
const SEG_LEN: usize = (SEGMENT_HEADER_SIZE + PAYLOAD_LEN).div_ceil(SEGMENT_ALIGNMENT) * SEGMENT_ALIGNMENT;

const RESULT_MAGIC: u32 = 0x5745_4654; // "WEFT"
const OK_RVF: u32 = 1 << 0;
const OK_LEAF: u32 = 1 << 1;

/// The RVF segment, read back by the host over SWD.
#[no_mangle]
#[used]
pub static mut N6_SEGMENT: [u8; SEG_LEN] = [0; SEG_LEN];

#[repr(C)]
pub struct N6Result {
    pub magic: u32,
    pub status: u32,
    pub seg_len: u32,
    pub cbor_len: u32,
    pub heartbeat: u32,
}

/// Outcome of the run, read back by the host over SWD.
#[no_mangle]
#[used]
pub static mut N6_RESULT: N6Result = N6Result { magic: 0, status: 0, seg_len: 0, cbor_len: 0, heartbeat: 0 };

fn put(buf: &mut [u8], at: usize, bytes: &[u8]) {
    buf[at..at + bytes.len()].copy_from_slice(bytes);
}

/// Serialize a header field by field (little endian), matching the RVF wire layout.
fn write_header(buf: &mut [u8], h: &SegmentHeader) {
    put(buf, 0x00, &h.magic.to_le_bytes());
    buf[0x04] = h.version;
    buf[0x05] = h.seg_type;
    put(buf, 0x06, &h.flags.to_le_bytes());
    put(buf, 0x08, &h.segment_id.to_le_bytes());
    put(buf, 0x10, &h.payload_length.to_le_bytes());
    put(buf, 0x18, &h.timestamp_ns.to_le_bytes());
    buf[0x20] = h.checksum_algo;
    buf[0x21] = h.compression;
    put(buf, 0x22, &h.reserved_0.to_le_bytes());
    put(buf, 0x24, &h.reserved_1.to_le_bytes());
    put(buf, 0x28, &h.content_hash);
    put(buf, 0x38, &h.uncompressed_len.to_le_bytes());
    put(buf, 0x3C, &h.alignment_pad.to_le_bytes());
}

fn build_segment(seg: &mut [u8]) -> bool {
    let payload = &mut seg[SEGMENT_HEADER_SIZE..SEGMENT_HEADER_SIZE + PAYLOAD_LEN];
    put(payload, 0, &(DIM as u16).to_le_bytes());
    put(payload, 2, &(COUNT as u32).to_le_bytes());
    let mut at = 6;
    for i in 0..COUNT {
        put(payload, at, &(1000 + i as u64).to_le_bytes());
        at += 8;
        for j in 0..DIM {
            put(payload, at, &(((i * DIM + j) as f32) * 0.125).to_le_bytes());
            at += 4;
        }
    }
    let hash = xxhash_rust::xxh3::xxh3_128(payload).to_le_bytes();

    let mut h = SegmentHeader::new(SegmentType::Vec as u8, 1);
    h.payload_length = PAYLOAD_LEN as u64;
    h.checksum_algo = 1; // XXH3-128
    h.content_hash = hash;
    h.alignment_pad = (SEG_LEN - SEGMENT_HEADER_SIZE - PAYLOAD_LEN) as u32;
    write_header(seg, &h);

    // Read it back the way a reader would: magic, length, hash.
    let magic = u32::from_le_bytes(seg[0..4].try_into().unwrap());
    let len = u64::from_le_bytes(seg[0x10..0x18].try_into().unwrap()) as usize;
    let stored: [u8; 16] = seg[0x28..0x38].try_into().unwrap();
    let again = xxhash_rust::xxh3::xxh3_128(&seg[SEGMENT_HEADER_SIZE..SEGMENT_HEADER_SIZE + len]).to_le_bytes();
    magic == rvf_types::SEGMENT_MAGIC && len == PAYLOAD_LEN && stored == again
}

fn leaf_roundtrip() -> Option<usize> {
    let announce = LeafServices {
        node_pubkey: [0x6e; 32],
        hostname: String::from("n6-leaf"),
        firmware_version: String::from(env!("CARGO_PKG_VERSION")),
        audio_sink: None,
        display_sink: None,
        compute: Some(ComputeCap { cpu_mhz: 64, free_heap_bytes: HEAP.free() as u32, eml_core: false }),
    };
    let bytes = encode(&announce).ok()?;
    let back: LeafServices = decode(&bytes).ok()?;
    (back == announce).then_some(bytes.len())
}

#[entry]
fn main() -> ! {
    {
        const HEAP_SIZE: usize = 32 * 1024;
        static mut HEAP_MEM: [u8; HEAP_SIZE] = [0; HEAP_SIZE];
        unsafe { HEAP.init(addr_of_mut!(HEAP_MEM) as usize, HEAP_SIZE) }
    }

    let mut status = 0;
    let seg = unsafe { &mut *addr_of_mut!(N6_SEGMENT) };
    if build_segment(seg) {
        status |= OK_RVF;
    }
    let cbor_len = match leaf_roundtrip() {
        Some(n) => {
            status |= OK_LEAF;
            n as u32
        }
        None => 0,
    };

    let r = addr_of_mut!(N6_RESULT);
    unsafe {
        write_volatile(addr_of_mut!((*r).seg_len), SEG_LEN as u32);
        write_volatile(addr_of_mut!((*r).cbor_len), cbor_len);
        write_volatile(addr_of_mut!((*r).status), status);
        write_volatile(addr_of_mut!((*r).magic), RESULT_MAGIC);
    }
    loop {
        unsafe {
            let hb = read_volatile(addr_of!((*r).heartbeat));
            write_volatile(addr_of_mut!((*r).heartbeat), hb.wrapping_add(1));
        }
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    unsafe { write_volatile(addr_of_mut!(N6_RESULT.status), 0xDEAD_0000) }
    loop {}
}
