//! Stream rings over a real Unix socket (ADR-122 B2): in-order delivery,
//! visible gaps, the three reader policies, writer and broker restarts,
//! and the manifest rules.
#![cfg(unix)]

mod common;

use std::time::{Duration, Instant};

use clawft_types::cog_topics::{InputPolicy, TopicClass};
use common::*;
use weftos_cogbus::stream::StreamOptions;
use weftos_cogbus::{ErrorCode, OutputSpec, StreamConfig};

fn small(slots: u64, max_record: u64) -> StreamOptions {
    let slot = (32 + max_record.div_ceil(8) * 8).div_ceil(64) * 64;
    StreamOptions {
        ring_bytes: Some(128 + slots * slot),
        ..StreamOptions::new(max_record)
    }
}

#[tokio::test]
async fn records_arrive_in_order_and_a_reader_may_subscribe_first() {
    let rig = Rig::new(StreamConfig::default()).await;
    rig.cog("mic", 'a', vec![pcm_out()], vec![]);
    rig.cog("asr", 'b', vec![], vec![pcm_in(InputPolicy::DropOldest)]);
    let (a, b) = (rig.connect('a').await, rig.connect('b').await);

    // The reader subscribes before any writer: it starts when one opens.
    let mut r = b.subscribe_stream(TOPIC, PCM).await.unwrap();
    assert!(r.try_next().is_none());
    let mut w = a
        .stream_writer(KIND, SOURCE, PCM, StreamOptions::new(640))
        .await
        .unwrap();
    assert_eq!(w.geometry().slot_count, ((4 << 20) - 128) / 704);
    // The ring notice reaches the reader through its connection.
    tokio::time::sleep(Duration::from_millis(50)).await;
    for i in 1..=1000u64 {
        assert_eq!(w.write(i * 1_000, &[(i % 256) as u8; 640]).unwrap(), i);
    }
    for i in 1..=1000u64 {
        let rec = next(&mut r).await;
        assert_eq!(
            (rec.seq, rec.t_ns, rec.data[0]),
            (i, i * 1_000, (i % 256) as u8)
        );
        assert_eq!(rec.data.len(), 640);
        assert!(rec.w_ns > 0);
    }
    assert_eq!(r.skipped(), 0);

    let rows = rig.broker.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (rows[0].class, rows[0].messages, rows[0].subscribers),
        (TopicClass::Stream, 1000, 1)
    );
    assert_eq!(rows[0].writer.as_deref(), Some("mic"));
    assert_eq!(rig.broker.stream_bytes(), w.geometry().bytes() as u64);
    assert_eq!(b.list().await.unwrap()["topics"][0]["topic"], TOPIC);
}

#[tokio::test]
async fn undeclared_wrong_schema_and_foreign_streams_are_refused() {
    let rig = Rig::new(StreamConfig::default()).await;
    let telemetry = OutputSpec {
        topic: "telemetry".into(),
        schema: "telemetry.v1".into(),
        class: TopicClass::Reading,
    };
    rig.cog("mic", 'a', vec![pcm_out(), telemetry], vec![]);
    rig.cog("asr", 'b', vec![], vec![pcm_in(InputPolicy::DropOldest)]);
    rig.cog("other", 'c', vec![pcm_out()], vec![]);
    let (a, b, c) = (
        rig.connect('a').await,
        rig.connect('b').await,
        rig.connect('c').await,
    );
    let o = StreamOptions::new(640);

    assert_eq!(
        refused(a.stream_writer("iq.raw", SOURCE, "iq.cs16.v1", o).await),
        ErrorCode::Undeclared
    );
    assert_eq!(
        refused(a.stream_writer(KIND, SOURCE, "iq.cs16.v1", o).await),
        ErrorCode::SchemaMismatch
    );
    assert_eq!(
        refused(a.stream_writer("telemetry", "b", "telemetry.v1", o).await),
        ErrorCode::Unsupported
    );
    assert_eq!(
        refused(
            a.stream_writer(KIND, SOURCE, PCM, StreamOptions::new(0))
                .await
        ),
        ErrorCode::TooLarge
    );
    assert_eq!(
        refused(
            a.stream_writer(
                KIND,
                SOURCE,
                PCM,
                StreamOptions {
                    ring_bytes: Some(1 << 30),
                    ..o
                }
            )
            .await
        ),
        ErrorCode::TooLarge
    );
    assert_eq!(
        refused(b.subscribe_stream(TOPIC, "iq.cs16.v1").await),
        ErrorCode::SchemaMismatch
    );
    assert_eq!(
        refused(b.subscribe_stream("video.raw/cam", PCM).await),
        ErrorCode::Undeclared
    );
    assert_eq!(
        refused(b.subscribe_stream("audio.pcm/*", PCM).await),
        ErrorCode::Unsupported
    );
    assert_eq!(
        refused(a.subscribe_stream(TOPIC, PCM).await),
        ErrorCode::Undeclared
    );

    let mut w = a.stream_writer(KIND, SOURCE, PCM, o).await.unwrap();
    // One instance writes a topic; a second connection of it waits too.
    assert_eq!(
        refused(c.stream_writer(KIND, SOURCE, PCM, o).await),
        ErrorCode::WriterConflict
    );
    let a2 = rig.connect('a').await;
    assert_eq!(
        refused(a2.stream_writer(KIND, SOURCE, PCM, o).await),
        ErrorCode::WriterConflict
    );
    assert_eq!(refused(w.write(0, &[0; 641])), ErrorCode::TooLarge);
    // Readings stay on publish: a stream output cannot be published.
    assert_eq!(
        refused(a.publish(KIND, SOURCE, PCM, 0, serde_json::json!({})).await),
        ErrorCode::Unsupported
    );
}

#[tokio::test]
async fn a_stalled_drop_oldest_reader_does_not_slow_the_writer() {
    let rig = Rig::new(StreamConfig::default()).await;
    rig.cog("mic", 'a', vec![pcm_out()], vec![]);
    rig.cog("asr", 'b', vec![], vec![pcm_in(InputPolicy::DropOldest)]);
    let (a, b) = (rig.connect('a').await, rig.connect('b').await);
    let mut w = a
        .stream_writer(KIND, SOURCE, PCM, StreamOptions::new(640))
        .await
        .unwrap();
    let n = w.geometry().slot_count * 4;
    let chunk = [7u8; 640];
    let rate = |w: &mut weftos_cogbus::StreamWriter| {
        let t = Instant::now();
        for i in 0..n {
            w.write(i, &chunk).unwrap();
        }
        n as f64 / t.elapsed().as_secs_f64()
    };
    let alone = rate(&mut w);
    // Subscribed, never reading.
    let mut r = b.subscribe_stream(TOPIC, PCM).await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(r.try_next().is_none(), "resumes at the head");
    let stalled = rate(&mut w);
    eprintln!("writer: {alone:.0} records/s alone, {stalled:.0} with a stalled drop-oldest reader");
    // Throughput is only reported: on a loaded machine it halves with or
    // without a reader. The writer's own wait is the measure: four laps
    // past a reader that never reads cost it nothing.
    assert_eq!(w.waited(), Duration::ZERO, "the writer waited for a drop-oldest reader");
    assert_eq!(w.demotions(), 0);
    // The reader is now a ring behind: it jumps, and the gap is visible.
    w.write(0, &chunk).unwrap();
    let head = w.last_seq();
    let rec = r.try_next().unwrap();
    assert!(
        rec.seq > head - w.geometry().slot_count,
        "jumped to the oldest kept record"
    );
    assert_eq!(r.try_next().unwrap().seq, rec.seq + 1);
}

#[tokio::test]
async fn a_lapped_reader_sees_the_gap_in_seq() {
    let rig = Rig::new(StreamConfig::default()).await;
    rig.cog("mic", 'a', vec![pcm_out()], vec![]);
    rig.cog("asr", 'b', vec![], vec![pcm_in(InputPolicy::DropOldest)]);
    let (a, b) = (rig.connect('a').await, rig.connect('b').await);
    let mut w = a
        .stream_writer(KIND, SOURCE, PCM, small(16, 64))
        .await
        .unwrap();
    let mut r = b.subscribe_stream(TOPIC, PCM).await.unwrap();
    for i in 1..=5 {
        w.write(i, b"x").unwrap();
    }
    assert_eq!(next(&mut r).await.seq, 1);
    for i in 6..=100 {
        w.write(i, b"x").unwrap();
    }
    let rec = next(&mut r).await;
    assert_eq!(rec.seq, 100 + 2 - 16, "oldest safe record after a lap");
    assert_eq!(r.skipped(), rec.seq - 2);
}

#[tokio::test]
async fn a_sample_reader_gets_the_newest_record() {
    let rig = Rig::new(StreamConfig::default()).await;
    rig.cog("mic", 'a', vec![pcm_out()], vec![]);
    rig.cog("vu", 'b', vec![], vec![pcm_in(InputPolicy::Sample)]);
    let (a, b) = (rig.connect('a').await, rig.connect('b').await);
    let mut w = a
        .stream_writer(KIND, SOURCE, PCM, StreamOptions::new(64))
        .await
        .unwrap();
    let mut r = b.subscribe_stream(TOPIC, PCM).await.unwrap();
    assert_eq!(r.policy(), InputPolicy::Sample);
    for i in 1..=100 {
        w.write(i, b"s").unwrap();
    }
    assert_eq!(r.try_next().unwrap().seq, 100);
    assert!(r.try_next().is_none());
    for i in 101..=105 {
        w.write(i, b"s").unwrap();
    }
    assert_eq!(r.try_next().unwrap().seq, 105);
}

#[tokio::test]
async fn the_ring_outlives_its_writer_and_the_next_writer_continues_it() {
    let rig = Rig::new(StreamConfig::default()).await;
    rig.cog("mic", 'a', vec![pcm_out()], vec![]);
    rig.cog("asr", 'b', vec![], vec![pcm_in(InputPolicy::DropOldest)]);
    let b = rig.connect('b').await;
    let mut r = b.subscribe_stream(TOPIC, PCM).await.unwrap();
    {
        let a = rig.connect('a').await;
        let mut w = a
            .stream_writer(KIND, SOURCE, PCM, StreamOptions::new(64))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        for i in 1..=10 {
            w.write(i, b"one").unwrap();
        }
    } // writer and its connection gone
    tokio::time::sleep(Duration::from_millis(50)).await;
    for want in 1..=10 {
        assert_eq!(
            next(&mut r).await.seq,
            want,
            "readable after the writer died"
        );
    }
    assert_eq!(rig.broker.snapshot()[0].writer.as_deref(), Some("mic"));
    let a = rig.connect('a').await;
    let mut w = a
        .stream_writer(KIND, SOURCE, PCM, StreamOptions::new(64))
        .await
        .unwrap();
    assert_eq!(w.last_seq(), 10, "same ring");
    for i in 11..=20 {
        assert_eq!(w.write(i, b"two").unwrap(), i);
    }
    for want in 11..=20 {
        let rec = next(&mut r).await;
        assert_eq!((rec.seq, rec.data.as_slice()), (want, &b"two"[..]));
    }
    // A writer asking for another shape gets a new ring; the reader is
    // moved to it and resumes at its head (seq keeps increasing).
    drop(w);
    drop(a);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let a = rig.connect('a').await;
    let mut w = a
        .stream_writer(KIND, SOURCE, PCM, StreamOptions::new(640))
        .await
        .unwrap();
    assert_eq!(w.last_seq(), 20);
    tokio::time::sleep(Duration::from_millis(50)).await;
    w.write(21, &[3; 640]).unwrap();
    let rec = next(&mut r).await;
    assert_eq!((rec.seq, rec.data.len()), (21, 640));
}

#[tokio::test]
async fn after_a_broker_restart_seq_continues_and_the_reader_sees_the_gap() {
    let setup = |rig: &Rig| {
        rig.cog("mic", 'a', vec![pcm_out()], vec![]);
        rig.cog("asr", 'b', vec![], vec![pcm_in(InputPolicy::DropOldest)]);
    };
    let rig = Rig::new(StreamConfig::default()).await;
    setup(&rig);
    let (a, b) = (rig.connect('a').await, rig.connect('b').await);
    let mut w = a
        .stream_writer(KIND, SOURCE, PCM, StreamOptions::new(64))
        .await
        .unwrap();
    let mut r = b.subscribe_stream(TOPIC, PCM).await.unwrap();
    for i in 1..=10 {
        w.write(i, b"x").unwrap();
    }
    for _ in 0..7 {
        next(&mut r).await;
    }
    let (last_written, last_read) = (w.last_seq(), r.last_seq());
    drop((w, r, a, b, rig));

    let rig = Rig::new(StreamConfig::default()).await;
    setup(&rig);
    let (a, b) = (rig.connect('a').await, rig.connect('b').await);
    let opts = StreamOptions {
        seq_base: Some(last_written),
        ..StreamOptions::new(64)
    };
    let mut w = a.stream_writer(KIND, SOURCE, PCM, opts).await.unwrap();
    let mut r = b.subscribe_stream(TOPIC, PCM).await.unwrap();
    assert_eq!(w.write(0, b"y").unwrap(), last_written + 1);
    let rec = next(&mut r).await;
    assert_eq!(rec.seq, 11);
    assert!(
        rec.seq > last_read + 1,
        "the reader's gap: {last_read} -> {}",
        rec.seq
    );
}

#[tokio::test]
async fn the_node_cap_bounds_all_rings() {
    let cfg = StreamConfig {
        node_cap_bytes: 6 << 20,
        ..StreamConfig::default()
    };
    let rig = Rig::new(cfg).await;
    let out2 = OutputSpec {
        topic: "audio.pcm2".into(),
        ..pcm_out()
    };
    rig.cog("mic", 'a', vec![pcm_out(), out2], vec![]);
    let a = rig.connect('a').await;
    let _w = a
        .stream_writer(KIND, SOURCE, PCM, StreamOptions::new(640))
        .await
        .unwrap();
    let second = a
        .stream_writer("audio.pcm2", SOURCE, PCM, StreamOptions::new(640))
        .await;
    assert_eq!(refused(second), ErrorCode::TooLarge);
    // A revoked writer with no readers frees its ring.
    assert!(rig.broker.revoke("mic"));
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(rig.broker.stream_bytes(), 0);
}

#[tokio::test]
async fn a_revoked_writer_frees_the_topic_for_another_instance() {
    let rig = Rig::new(StreamConfig::default()).await;
    rig.cog("mic", 'a', vec![pcm_out()], vec![]);
    rig.cog("mic2", 'c', vec![pcm_out()], vec![]);
    rig.cog("asr", 'b', vec![], vec![pcm_in(InputPolicy::DropOldest)]);
    let (a, b, c) = (
        rig.connect('a').await,
        rig.connect('b').await,
        rig.connect('c').await,
    );
    let mut r = b.subscribe_stream(TOPIC, PCM).await.unwrap();
    let mut w = a
        .stream_writer(KIND, SOURCE, PCM, StreamOptions::new(64))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    w.write(1, b"a").unwrap();
    assert_eq!(
        refused(
            c.stream_writer(KIND, SOURCE, PCM, StreamOptions::new(64))
                .await
        ),
        ErrorCode::WriterConflict
    );
    assert!(rig.broker.revoke("mic"));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(rig.broker.snapshot()[0].writer, None);
    let mut w2 = c
        .stream_writer(KIND, SOURCE, PCM, StreamOptions::new(64))
        .await
        .unwrap();
    assert_eq!(w2.write(2, b"c").unwrap(), 2, "the same ring, continued");
    assert_eq!(next(&mut r).await.data, b"a");
    assert_eq!(next(&mut r).await.data, b"c");
    drop((w, a));
}

#[tokio::test]
async fn a_reader_waiting_for_a_writer_misses_none_of_its_first_records() {
    let rig = Rig::new(StreamConfig::default()).await;
    rig.cog("mic", 'a', vec![pcm_out()], vec![]);
    rig.cog("asr", 'b', vec![], vec![pcm_in(InputPolicy::DropOldest)]);
    let (a, b) = (rig.connect('a').await, rig.connect('b').await);
    let mut r = b.subscribe_stream(TOPIC, PCM).await.unwrap();
    let mut w = a.stream_writer(KIND, SOURCE, PCM, StreamOptions::new(8)).await.unwrap();
    // No pause: the records land before the reader's ring notice is read.
    for i in 1..=3 {
        w.write(i, b"r").unwrap();
    }
    for want in 1..=3 {
        assert_eq!(next(&mut r).await.seq, want);
    }
}
