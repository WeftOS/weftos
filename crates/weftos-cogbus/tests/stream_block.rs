//! `block` readers over a real Unix socket (ADR-122 §6): a reader that keeps
//! up loses nothing, and a stalled one holds the writer for the deadline,
//! then falls back to `drop-oldest` and is told.
#![cfg(unix)]

mod common;

use std::time::{Duration, Instant};

use clawft_types::cog_topics::InputPolicy;
use common::*;
use weftos_cogbus::StreamConfig;
use weftos_cogbus::stream::StreamOptions;

fn small(slots: u64, max_record: u64) -> StreamOptions {
    let slot = (32 + max_record.div_ceil(8) * 8).div_ceil(64) * 64;
    StreamOptions {
        ring_bytes: Some(128 + slots * slot),
        ..StreamOptions::new(max_record)
    }
}

async fn block_rig(policy_b: InputPolicy) -> (Rig, weftos_cogbus::Client, weftos_cogbus::Client) {
    let rig = Rig::new(StreamConfig::default()).await;
    rig.cog("mic", 'a', vec![pcm_out()], vec![]);
    rig.cog("rec", 'b', vec![], vec![pcm_in(policy_b)]);
    let (a, b) = (rig.connect('a').await, rig.connect('b').await);
    (rig, a, b)
}

async fn wait_block_readers(w: &weftos_cogbus::StreamWriter, n: usize) {
    let t = Instant::now();
    while w.block_readers() != n {
        assert!(
            t.elapsed() < Duration::from_secs(5),
            "writer never saw {n} block readers"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_block_reader_that_keeps_up_loses_nothing() {
    let (_rig, a, b) = block_rig(InputPolicy::Block).await;
    let mut w = a
        .stream_writer(KIND, SOURCE, PCM, small(8, 64))
        .await
        .unwrap();
    let mut r = b.subscribe_stream(TOPIC, PCM).await.unwrap();
    wait_block_readers(&w, 1).await;
    let total = 8 * 50u64;
    let reader = tokio::spawn(async move {
        r.set_poll_interval(Duration::from_micros(100));
        let mut seqs = Vec::new();
        while seqs.len() < total as usize {
            seqs.push(next(&mut r).await.seq);
            if seqs.len() % 16 == 0 {
                // A slow consumer, well inside the deadline.
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }
        (seqs, r.skipped())
    });
    for i in 1..=total {
        w.write_async(i, &i.to_le_bytes()).await.unwrap();
    }
    let (seqs, skipped) = reader.await.unwrap();
    assert_eq!(seqs, (1..=total).collect::<Vec<_>>());
    assert_eq!((skipped, w.demotions()), (0, 0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stalled_block_reader_falls_back_at_the_deadline_and_is_told() {
    let (rig, a, b) = block_rig(InputPolicy::Block).await;
    let mut w = a
        .stream_writer(KIND, SOURCE, PCM, small(8, 64))
        .await
        .unwrap();
    let mut r = b.subscribe_stream(TOPIC, PCM).await.unwrap();
    assert!(r.try_next().is_none(), "attach: cursor at the head");
    wait_block_readers(&w, 1).await;
    // Eight slots, one kept clear of the writer: seven fit.
    for i in 1..=7 {
        let t = Instant::now();
        w.write(i, b"b").unwrap();
        assert!(t.elapsed() < Duration::from_millis(20), "the ring had room");
    }
    // Record 8 puts record 1, unread, out of the reader's reach.
    let t = Instant::now();
    assert_eq!(w.write(8, b"b").unwrap(), 8);
    let waited = t.elapsed();
    eprintln!("block writer waited {waited:?} for the stalled reader");
    assert!(
        waited >= Duration::from_millis(45) && waited < Duration::from_secs(2),
        "{waited:?}"
    );
    assert_eq!((w.demotions(), w.block_readers()), (1, 0));
    // No more waiting.
    let t = Instant::now();
    for i in 9..=100 {
        w.write(i, b"b").unwrap();
    }
    assert!(t.elapsed() < Duration::from_millis(45));

    let t = Instant::now();
    let events = loop {
        let e = r.take_events();
        if !e.is_empty() {
            break e;
        }
        assert!(t.elapsed() < Duration::from_secs(5), "reader never told");
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    assert_eq!(events[0].policy, InputPolicy::DropOldest);
    assert!(events[0].reason.contains("50 ms"), "{}", events[0].reason);
    assert_eq!(r.policy(), InputPolicy::DropOldest);
    assert_eq!(rig.broker.snapshot()[0].drops, 1);
    // It reads on as drop-oldest, with the gap visible.
    assert!(r.try_next().unwrap().seq > 2);
}
