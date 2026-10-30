//! build-kb — Generate an RVF knowledge-base file from WeftOS docs.
//!
//! Walks one or more docs trees (Fumadocs MDX plus repo markdown), chunks
//! each file by heading, generates deterministic hash-based embeddings
//! (SHA-256 -> 384 floats), and writes a binary `.rvf` segment file for
//! the browser playground RAG pipeline.
//!
//! The output contains:
//! - One Meta segment (0x07) with CBOR-encoded corpus manifest
//! - One Vec segment (0x01) per document chunk with CBOR-encoded payload

mod corpus;

use std::path::PathBuf;

use chrono::Utc;
use clap::Parser;
use rvf_types::{SegmentFlags, SegmentType};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::corpus::collect_from_dirs;

#[derive(Parser)]
#[command(name = "build-kb", about = "Build an RVF knowledge base from MDX/MD docs")]
struct Cli {
    /// Path to a docs tree. Repeat for Fumadocs + ADR/guides/research/etc.
    #[arg(long = "docs-dir", required = true)]
    docs_dirs: Vec<PathBuf>,

    /// Output path for the .rvf file.
    #[arg(long)]
    output: PathBuf,

    /// Repository root used for tags and GitHub blob URLs. Defaults to cwd.
    #[arg(long)]
    repo_root: Option<PathBuf>,
}

#[derive(Serialize)]
struct ManifestPayload {
    agent_id: String,
    namespace: String,
    segment_count: usize,
    dimension: u32,
    embedder_name: String,
    created_at: String,
    version: u32,
}

#[derive(Serialize)]
struct VecPayload {
    id: String,
    text: String,
    embedding: Vec<f32>,
    metadata: serde_json::Value,
    tags: Vec<String>,
    namespace: String,
    dimension: u32,
    embedder_name: String,
}

const EMBEDDING_DIM: usize = 384;
const EMBEDDER_NAME: &str = "hash-sha256";

/// Deterministic hash-based embedding (placeholder for real model).
///
/// SHA-256 of the input text is expanded to 384 floats in [-1, 1],
/// then L2-normalised. The shape matches all-MiniLM-L6-v2 so downstream
/// consumers need no changes when we swap in ONNX inference later.
fn hash_embed(text: &str) -> Vec<f32> {
    let hash = Sha256::digest(text.as_bytes());
    let mut embedding = Vec::with_capacity(EMBEDDING_DIM);
    for i in 0..EMBEDDING_DIM {
        let byte = hash[i % 32];
        let val = (byte as f32 / 127.5) - 1.0;
        let pos_factor = ((i as f32) * 0.01).sin();
        embedding.push(val * 0.5 + pos_factor * 0.5);
    }
    let norm: f32 = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for v in &mut embedding {
            *v /= norm;
        }
    }
    embedding
}

fn cbor_encode<T: Serialize>(value: &T) -> Vec<u8> {
    let mut buf = Vec::new();
    ciborium::into_writer(value, &mut buf).expect("CBOR serialization should not fail");
    buf
}

fn main() {
    let cli = Cli::parse();
    let repo_root = cli
        .repo_root
        .unwrap_or_else(|| std::env::current_dir().expect("cwd"));

    for dir in &cli.docs_dirs {
        if !dir.is_dir() {
            eprintln!("Error: docs directory does not exist: {}", dir.display());
            std::process::exit(1);
        }
    }

    let now = Utc::now();
    let (file_count, chunks) = collect_from_dirs(&cli.docs_dirs, &repo_root);

    let mut vec_segments: Vec<Vec<u8>> = Vec::new();
    let mut total_text_bytes: usize = 0;
    let mut chunk_count: usize = 0;

    for chunk in &chunks {
        let id = format!("{}-{}", chunk.slug, chunk.chunk_index);
        let metadata = serde_json::json!({
            "source": chunk.source,
            "title": chunk.title,
            "section": chunk.section,
            "doc_url": chunk.doc_url,
            "category": chunk.category,
            "chunk_index": chunk.chunk_index,
            "total_chunks": chunk.total_chunks,
            "has_code": chunk.has_code,
        });

        let embedding = hash_embed(&chunk.text);
        total_text_bytes += chunk.text.len();

        let payload = VecPayload {
            id,
            text: chunk.text.clone(),
            embedding,
            metadata,
            tags: chunk.tags.clone(),
            namespace: "docs".to_string(),
            dimension: EMBEDDING_DIM as u32,
            embedder_name: EMBEDDER_NAME.to_string(),
        };

        let cbor_buf = cbor_encode(&payload);
        chunk_count += 1;
        let seg_bytes = rvf_wire::write_segment(
            SegmentType::Vec as u8,
            &cbor_buf,
            SegmentFlags::empty(),
            chunk_count as u64,
        );
        vec_segments.push(seg_bytes);
    }

    let manifest = ManifestPayload {
        agent_id: "tour-guide".to_string(),
        namespace: "docs".to_string(),
        segment_count: chunk_count,
        dimension: EMBEDDING_DIM as u32,
        embedder_name: EMBEDDER_NAME.to_string(),
        created_at: now.to_rfc3339(),
        version: 1,
    };
    let manifest_cbor = cbor_encode(&manifest);
    let manifest_seg = rvf_wire::write_segment(
        SegmentType::Meta as u8,
        &manifest_cbor,
        SegmentFlags::empty(),
        0,
    );

    let mut output = Vec::with_capacity(
        manifest_seg.len() + vec_segments.iter().map(|s| s.len()).sum::<usize>(),
    );
    output.extend_from_slice(&manifest_seg);
    for seg in &vec_segments {
        output.extend_from_slice(seg);
    }

    if let Some(parent) = cli.output.parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent).unwrap_or_else(|e| {
                eprintln!("Error: could not create output directory: {e}");
                std::process::exit(1);
            });
        }
    }

    std::fs::write(&cli.output, &output).unwrap_or_else(|e| {
        eprintln!("Error: could not write output file: {e}");
        std::process::exit(1);
    });

    let output_size = output.len();
    println!("Knowledge base built successfully (binary RVF).");
    println!("  Files processed:  {file_count}");
    println!("  Segments created: {chunk_count}");
    println!(
        "  Total text:       {:.1} KB",
        total_text_bytes as f64 / 1024.0
    );
    println!(
        "  Output file:      {} ({:.1} KB)",
        cli.output.display(),
        output_size as f64 / 1024.0
    );
}
