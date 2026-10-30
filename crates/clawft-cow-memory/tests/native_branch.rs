//! rvf-runtime 0.5 native `branch()` read-through (ADR-128 T0).
//!
//! After `promote`, a lineage is the single-parent shape (`working` branched
//! from `base`, no checkpoints): queries are answered by one native query
//! with no chain walk, so everything below passes only if upstream's
//! parent read-through and membership filter do their job.

mod common;

use clawft_cow_memory::VectorItem;

#[test]
fn branched_child_sees_parent_vectors_by_native_read_through() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut mem = common::new_memory(dir.path());
    let dim = common::dim() as usize;
    let mut rng = rand::thread_rng();

    let v = common::random_vector(&mut rng, dim);
    let id = mem.ingest(&[VectorItem::new(v.clone())]).unwrap()[0];
    mem.promote().expect("promote lands v in base");

    assert_eq!(mem.status().base.total_vectors, 1);
    assert_eq!(
        mem.status().working.total_vectors,
        0,
        "the fresh working child owns no vectors of its own"
    );
    assert_eq!(mem.status().own_checkpoint_depth, 0);

    let hits = mem.query(&v, 3).unwrap();
    assert_eq!(hits.first().map(|r| r.id), Some(id), "read through to base");
}

#[test]
fn child_side_delete_hides_a_parent_vector() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut mem = common::new_memory(dir.path());
    let dim = common::dim() as usize;
    let mut rng = rand::thread_rng();

    let keep = common::random_vector(&mut rng, dim);
    let gone = common::random_vector(&mut rng, dim);
    let ids = mem
        .ingest(&[VectorItem::new(keep.clone()), VectorItem::new(gone.clone())])
        .unwrap();
    mem.promote().unwrap();

    mem.delete(&[ids[1]]).unwrap();
    let hits = mem.query(&gone, 5).unwrap();
    assert!(!hits.iter().any(|r| r.id == ids[1]), "deleted parent id hidden");
    assert!(hits.iter().any(|r| r.id == ids[0]), "sibling still visible");
}

#[test]
fn branched_children_survive_reopen() {
    let dir = tempfile::tempdir().expect("tempdir");
    let dim = common::dim() as usize;
    let mut rng = rand::thread_rng();
    let v = common::random_vector(&mut rng, dim);
    let id;
    {
        let mut mem = common::new_memory(dir.path());
        id = mem.ingest(&[VectorItem::new(v.clone())]).unwrap()[0];
        mem.checkpoint("c1").unwrap();
    }
    let mem = clawft_cow_memory::BranchableMemory::open(dir.path()).expect("reopen");
    assert!(mem.query(&v, 3).unwrap().iter().any(|r| r.id == id));
}

#[test]
fn sparse_vector_ids_fall_back_to_derive_and_still_query() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut mem = common::new_memory(dir.path());
    let dim = common::dim() as usize;
    let mut rng = rand::thread_rng();
    // Digest-style id: far past upstream's dense membership bitmap bound.
    let v = common::random_vector(&mut rng, dim);
    let id = mem
        .ingest(&[VectorItem::new(v.clone()).with_id(1 << 40)])
        .unwrap()[0];
    mem.promote().expect("promote must fall back, not fail");
    assert!(mem.query(&v, 3).unwrap().iter().any(|r| r.id == id));
    mem.delete(&[id]).unwrap();
    assert!(!mem.query(&v, 3).unwrap().iter().any(|r| r.id == id));
}
