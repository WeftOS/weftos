//! FU-11: concurrent durable saves and checkpoint saves of one chain file.
//!
//! The contract under test (owner rule: each write is confirmed on disk):
//! once a save returns `Ok`, the file on disk holds at least every event
//! that existed before that call, and no later save by another writer can
//! put an older snapshot back. No sleeps: eight writers start together at a
//! barrier and each checks the file right after its own save returns.

#![cfg(all(feature = "native", feature = "exochain"))]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};

use clawft_kernel::chain::ChainManager;

const DURABLE_WRITERS: usize = 4;
const CHECKPOINT_WRITERS: usize = 4;
const ROUNDS: usize = 40;

#[test]
fn concurrent_durable_and_checkpoint_saves_never_lose_an_acknowledged_event() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("chain.rvf");
    let cm = Arc::new(ChainManager::new(0, 1000));
    cm.save_to_rvf(&path).expect("initial save");

    // Highest event count whose save has returned Ok.
    let acked = Arc::new(AtomicUsize::new(0));
    let failures: Arc<Mutex<Vec<String>>> = Arc::default();
    let barrier = Arc::new(Barrier::new(DURABLE_WRITERS + CHECKPOINT_WRITERS));

    let mut handles = Vec::new();
    for w in 0..DURABLE_WRITERS + CHECKPOINT_WRITERS {
        let durable = w < DURABLE_WRITERS;
        let (cm, path, acked, failures, barrier) = (
            Arc::clone(&cm),
            path.clone(),
            Arc::clone(&acked),
            Arc::clone(&failures),
            Arc::clone(&barrier),
        );
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            for round in 0..ROUNDS {
                cm.append("race", "event", Some(serde_json::json!({ "w": w, "r": round })));
                let floor = cm.len();
                let saved = if durable {
                    cm.save_to_rvf_durable(&path).map(|_| ())
                } else {
                    cm.save_to_rvf(&path)
                };
                if let Err(e) = saved {
                    failures.lock().unwrap().push(format!("w{w} r{round}: save failed: {e}"));
                    continue;
                }
                acked.fetch_max(floor, Ordering::SeqCst);
                let must_hold = acked.load(Ordering::SeqCst);
                match ChainManager::load_from_rvf(&path, 1000) {
                    Ok(on_disk) if on_disk.len() >= must_hold => {}
                    Ok(on_disk) => failures.lock().unwrap().push(format!(
                        "w{w} r{round}: disk has {} events, {must_hold} were acknowledged",
                        on_disk.len()
                    )),
                    Err(e) => failures.lock().unwrap().push(format!("w{w} r{round}: unreadable file: {e}")),
                }
            }
        }));
    }
    for h in handles {
        h.join().expect("writer thread panicked");
    }

    let failures = failures.lock().unwrap();
    assert!(
        failures.is_empty(),
        "{} violations, first 5: {:?}",
        failures.len(),
        &failures[..failures.len().min(5)]
    );

    // No temp file is left behind by any writer.
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n != "chain.rvf")
        .collect();
    assert!(leftovers.is_empty(), "stray files: {leftovers:?}");
}

/// The durable save used to stage through one fixed sibling name
/// (`<file>.rvf-durable-next`). Anything already at that name, such as a
/// second writer's staging file or an operator's file, was overwritten and
/// then renamed away. Every save now stages under a name nothing else uses.
#[test]
fn durable_save_does_not_touch_the_old_fixed_staging_name() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("chain.rvf");
    let bystander = dir.path().join("chain.rvf-durable-next");
    std::fs::write(&bystander, b"not the chain").unwrap();

    let cm = ChainManager::new(0, 1000);
    cm.append("race", "event", None);
    cm.save_to_rvf_durable(&path).expect("durable save");

    assert_eq!(
        std::fs::read(&bystander).ok().as_deref(),
        Some(&b"not the chain"[..]),
        "the durable save clobbered a file it does not own"
    );
    assert_eq!(ChainManager::load_from_rvf(&path, 1000).unwrap().len(), cm.len());
}
