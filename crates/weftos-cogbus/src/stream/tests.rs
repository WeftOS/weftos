//! Ring and shared-memory unit tests (no socket).

use std::os::fd::AsFd;
use std::sync::Arc;

use clawft_types::cog_topics::InputPolicy;

use super::ring::{self, RingReader, RingWriter};
use super::shm::{self, Mapping};
use super::wire::{Geometry, StreamNotice};
use crate::proto::Event;

fn ring(max_record: u64, ring_bytes: u64) -> (RingWriter, RingReader, Geometry) {
    let g = Geometry::fit(max_record, ring_bytes).unwrap();
    let s = shm::create(g.bytes()).unwrap();
    let broker = Mapping::new(s.rw.as_fd(), g.bytes(), true).unwrap();
    ring::init(&broker, g, 7, 0);
    let w = RingWriter::attach(Mapping::new(s.rw.as_fd(), g.bytes(), true).unwrap(), g).unwrap();
    let r = RingReader::attach(Mapping::new(s.ro.as_fd(), g.bytes(), false).unwrap(), g).unwrap();
    (w, r, g)
}

#[test]
fn geometry_fits_records_and_refuses_tiny_rings() {
    let g = Geometry::fit(640, 4 << 20).unwrap();
    assert_eq!(g.slot_size, 704);
    assert_eq!(g.slot_count, ((4 << 20) - 128) / 704);
    assert!(g.bytes() <= 4 << 20);
    assert_eq!(Geometry::fit(640, 128 + 704), None);
    assert_eq!(Geometry::fit(0, 4096), None);
    assert_eq!(Geometry::fit(1, 128 + 128).unwrap().slot_count, 2);
}

#[test]
fn records_arrive_in_order_with_their_fields() {
    let (mut w, r, _) = ring(100, 64 << 10);
    let mut next = 1;
    assert!(r.read(&mut next).is_none());
    for i in 1..=50u64 {
        let data: Vec<u8> = (0..(i % 13) as u8).collect();
        assert_eq!(w.write(i * 10, 99, &data), i);
    }
    for i in 1..=50u64 {
        let rec = r.read(&mut next).unwrap();
        assert_eq!((rec.seq, rec.t_ns, rec.w_ns), (i, i * 10, 99));
        assert_eq!(rec.data, (0..(i % 13) as u8).collect::<Vec<_>>());
    }
    assert!(r.read(&mut next).is_none());
}

#[test]
fn a_reader_lapped_by_the_writer_jumps_and_sees_the_gap() {
    let (mut w, r, g) = ring(8, 128 + 8 * 64);
    assert_eq!(g.slot_count, 8);
    for i in 1..=30u64 {
        w.write(i, 0, &i.to_le_bytes());
    }
    let mut next = 1;
    let first = r.read(&mut next).unwrap();
    // 30 written, 8 slots, one kept clear for the writer: 24..=30 remain.
    assert_eq!(first.seq, 24);
    assert_eq!(first.data, 24u64.to_le_bytes());
    let rest: Vec<u64> = std::iter::from_fn(|| r.read(&mut next))
        .map(|x| x.seq)
        .collect();
    assert_eq!(rest, (25..=30).collect::<Vec<_>>());
}

#[test]
fn concurrent_reader_never_sees_a_torn_record() {
    let (mut w, r, _) = ring(256, 128 + 16 * 320);
    let r = Arc::new(r);
    let reader = {
        let r = r.clone();
        std::thread::spawn(move || {
            let (mut next, mut got, mut last) = (1u64, 0u64, 0u64);
            while last < 200_000 {
                if let Some(rec) = r.read(&mut next) {
                    assert!(rec.seq > last);
                    let fill = (rec.seq % 251) as u8;
                    assert_eq!(rec.data.len(), 200 + (rec.seq % 56) as usize);
                    assert!(
                        rec.data.iter().all(|b| *b == fill),
                        "torn record {}",
                        rec.seq
                    );
                    assert_eq!(rec.t_ns, rec.seq);
                    last = rec.seq;
                    got += 1;
                }
            }
            got
        })
    };
    for seq in 1..=200_000u64 {
        let data = vec![(seq % 251) as u8; 200 + (seq % 56) as usize];
        w.write(seq, 0, &data);
    }
    assert!(reader.join().unwrap() > 0);
}

#[test]
fn readers_cannot_map_the_ring_writable() {
    let s = shm::create(64 << 10).unwrap();
    let e = Mapping::new(s.ro.as_fd(), 4096, true).unwrap_err();
    // Linux says EACCES, macOS EPERM.
    assert!(
        matches!(e.raw_os_error(), Some(libc::EACCES | libc::EPERM)),
        "{e}"
    );
    assert!(Mapping::new(s.ro.as_fd(), 4096, false).is_ok());
    // Mapping past the object's end is refused, not a later fault.
    // (macOS rounds an object up to its page size; 64 KiB is a multiple.)
    assert!(Mapping::new(s.ro.as_fd(), 128 << 10, false).is_err());
}

#[test]
fn stream_notices_round_trip_inside_events() {
    let notices = [
        StreamNotice::Reader {
            topic: "audio.pcm/mic".into(),
            sub: 5,
            added: true,
            fds: 1,
        },
        StreamNotice::Ring {
            sub: 5,
            ring_id: 9,
            head: 3,
            geometry: Geometry::fit(640, 1 << 20).unwrap(),
            fds: 1,
        },
        StreamNotice::Policy {
            sub: 5,
            policy: InputPolicy::DropOldest,
            reason: "late".into(),
        },
    ];
    for n in notices {
        let ev = Event::Stream(n.clone());
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["type"], "stream");
        assert_eq!(serde_json::from_value::<Event>(v).unwrap(), ev);
    }
}
