//! `weaver workload pack | verify | keygen` (mesh-placement-07).
//!
//! Builds and verifies signed cog packages (`cogpkg.json`, ADR-100 sec. 1)
//! with the trust rules of ADR-099 sec. 8. The rest of the `weaver workload`
//! group (list / inspect / daemon RPCs) lives in `workload_cmd`, and
//! [`WorkloadPackCmd`] is designed to be flattened into it with
//! `#[command(flatten)]`.
//!
//! `verify` exits with a distinct status per failure class so scripts and
//! the conformance harness can tell the cases apart.

use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};
use clawft_kernel::artifact_store::ArtifactStore;
use clawft_kernel::workload_pkg::{
    CogPackInput, PackageSource, TrustAnchors, VerifiedPackage, VerifyError, VerifyPolicy,
    key_id_for, pack_cog, sign_envelope, signing_key_from_hex, store_package, verify_dir,
    verify_stored, write_manifest,
};

/// Package subcommands of `weaver workload`.
#[derive(Subcommand, Debug)]
pub enum WorkloadPackCmd {
    /// Build (and sign) a cog package from a cog source dir and per-arch binaries.
    Pack(PackArgs),
    /// Verify a package's signatures and file hashes against pinned keys.
    Verify(VerifyArgs),
    /// Generate an Ed25519 package-signing key (hex seed, mode 0600).
    Keygen(KeygenArgs),
}

/// `weaver workload pack`.
#[derive(Args, Debug)]
pub struct PackArgs {
    /// Cog source directory holding the unmodified `cog.toml`
    /// (e.g. `vendor/cogs/src/cogs/anomaly-detect`).
    #[arg(long)]
    pub cog_dir: PathBuf,
    /// Per-arch binary, `<arch>=<path>` (repeat; arch: aarch64, armv7, x86_64, wasm, or aarch64-macos for a thin arm64 Mach-O run on macOS).
    #[arg(long = "bin", value_name = "ARCH=PATH", required = true)]
    pub bins: Vec<String>,
    /// Output package directory (must not exist or be empty).
    #[arg(long)]
    pub out: PathBuf,
    /// Signing key file (64 hex chars). Omit to write an unsigned package.
    #[arg(long)]
    pub key: Option<PathBuf>,
    /// Key id recorded with the signature (default: derived from the public key).
    #[arg(long)]
    pub key_id: Option<String>,
    /// Source commit (default: `git rev-parse HEAD` in --cog-dir).
    #[arg(long)]
    pub source_commit: Option<String>,
    /// Source repository label.
    #[arg(long)]
    pub source_repo: Option<String>,
    /// Upstream release URL, when packaging released binaries (provenance only).
    #[arg(long)]
    pub release_url: Option<String>,
    /// Cognitum ADR-154/155 release record to carry as an attestation.
    #[arg(long)]
    pub cognitum_record: Option<PathBuf>,
    /// Sign the package as redistributable: other nodes may be seeded and
    /// served its files over the swarm. Off by default; a package that
    /// carries a Cognitum attestation is never redistributed regardless.
    ///
    /// Requires evidence that the binary is not a licensed Cognitum one: a
    /// `provenance.json` (from `--provenance`, the cog dir, or next to a
    /// `--bin`, where `weaver cog install` writes it) with a trust other than
    /// `cognitum-sha256`, and whose sha256 matches a packed binary. Without
    /// one, `--redistributable` is refused unless `--no-provenance-ok` is given.
    #[arg(long)]
    pub redistributable: bool,
    /// Explicit `provenance.json` for the binaries (default: the cog dir's,
    /// then one next to each `--bin`).
    #[arg(long)]
    pub provenance: Option<PathBuf>,
    /// Allow `--redistributable` with no `provenance.json` anywhere (your own
    /// build from source). You are asserting the binary is not a Cognitum one.
    #[arg(long)]
    pub no_provenance_ok: bool,
    /// Also verify and store the package in this file-backed ArtifactStore.
    #[arg(long, requires = "trust")]
    pub store: Option<PathBuf>,
    /// Trust file (weftos.workload-trust.v1), required with --store.
    #[arg(long)]
    pub trust: Option<PathBuf>,
}

/// `weaver workload verify`.
#[derive(Args, Debug)]
pub struct VerifyArgs {
    /// Package directory (holding `cogpkg.json`).
    #[arg(required_unless_present = "manifest")]
    pub dir: Option<PathBuf>,
    /// Trust file (weftos.workload-trust.v1). Without it only the compiled-in
    /// WeftOS signer set is trusted.
    #[arg(long)]
    pub trust: Option<PathBuf>,
    /// Accept a valid Cognitum release record (pinned key) as a signature.
    #[arg(long)]
    pub cognitum_release: bool,
    /// BLAKE3 of a reviewed `cog.toml` to pin for record-only trust (repeat).
    /// A Cognitum record does not cover `cog.toml`, so a package trusted by a
    /// record alone is refused unless its `cog.toml` hash is pinned here.
    #[arg(long = "cog-toml-pin", value_name = "BLAKE3")]
    pub cog_toml_pins: Vec<String>,
    /// Verify from a file-backed ArtifactStore instead of a directory.
    #[arg(long, requires = "manifest")]
    pub store: Option<PathBuf>,
    /// Manifest artifact hash (printed by `pack --store`).
    #[arg(long, requires = "store", conflicts_with = "dir")]
    pub manifest: Option<String>,
    /// Emit a JSON result on stdout.
    #[arg(long)]
    pub json: bool,
}

/// `weaver workload keygen`.
#[derive(Args, Debug)]
pub struct KeygenArgs {
    /// Output key file (created new, mode 0600).
    #[arg(long)]
    pub out: PathBuf,
    /// Key id to print in the trust-file snippet (default: derived).
    #[arg(long)]
    pub key_id: Option<String>,
}

/// Dispatch.
pub fn run(cmd: WorkloadPackCmd) -> anyhow::Result<()> {
    match cmd {
        WorkloadPackCmd::Pack(a) => pack(a),
        WorkloadPackCmd::Verify(a) => verify(a),
        WorkloadPackCmd::Keygen(a) => keygen(a),
    }
}

/// Process exit status for a verification failure.
pub fn exit_code(e: &VerifyError) -> i32 {
    match e {
        VerifyError::Manifest(_) => 10,
        VerifyError::MissingSignature => 11,
        VerifyError::UntrustedSigner { .. } => 12,
        VerifyError::BadSignature { .. } => 13,
        VerifyError::FileMissing { .. } => 14,
        VerifyError::HashMismatch { .. } => 15,
        VerifyError::CognitumRecord(_) => 16,
        VerifyError::Io { .. } => 17,
    }
}

fn parse_bin(s: &str) -> anyhow::Result<(String, PathBuf)> {
    let (arch, path) = s
        .split_once('=')
        .ok_or_else(|| anyhow::anyhow!("--bin expects <arch>=<path>, got {s:?}"))?;
    Ok((arch.to_string(), PathBuf::from(path)))
}

/// Reject a `--bin` whose bytes do not match its arch key, using the
/// kernel's own payload classifier so pack and admission agree.
#[cfg(unix)]
fn check_bin_matches_key(arch: &str, bytes: &[u8]) -> anyhow::Result<()> {
    use clawft_kernel::workload_runtime::native::{
        CPU_TYPE_ARM64, PayloadKind, classify, elf_machine,
    };
    let kind = classify(bytes);
    match arch {
        "aarch64-macos" => match kind {
            PayloadKind::MachO(CPU_TYPE_ARM64) => Ok(()),
            _ => anyhow::bail!(
                "--bin aarch64-macos must be a thin arm64 Mach-O executable (got {kind:?}; fat, dylib, 32-bit and big-endian files are refused)"
            ),
        },
        "aarch64" | "armv7" | "x86_64" => match (kind, elf_machine(arch)) {
            (PayloadKind::Elf(m), Some(want)) if m == want => Ok(()),
            _ => anyhow::bail!("--bin {arch} must be an ELF for {arch} (got {kind:?})"),
        },
        _ => Ok(()),
    }
}

/// The payload classifier is part of the unix-only workload runtime; a
/// Windows `pack` leaves the check to admission on the node.
#[cfg(not(unix))]
fn check_bin_matches_key(_arch: &str, _bytes: &[u8]) -> anyhow::Result<()> {
    Ok(())
}

fn load_trust(path: Option<&Path>) -> anyhow::Result<TrustAnchors> {
    match path {
        Some(p) => {
            TrustAnchors::from_trust_json(&std::fs::read(p)?).map_err(|e| anyhow::anyhow!(e))
        }
        None => TrustAnchors::weftos_default().map_err(|e| anyhow::anyhow!(e)),
    }
}

fn git_head(dir: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn pack(a: PackArgs) -> anyhow::Result<()> {
    let binaries = a
        .bins
        .iter()
        .map(|b| parse_bin(b))
        .collect::<anyhow::Result<Vec<_>>>()?;
    for (arch, path) in &binaries {
        let bytes = std::fs::read(path)
            .map_err(|e| anyhow::anyhow!("--bin {arch}: cannot read {}: {e}", path.display()))?;
        check_bin_matches_key(arch, &bytes)?;
    }
    let commit = a.source_commit.clone().or_else(|| {
        if a.release_url.is_none() {
            git_head(&a.cog_dir)
        } else {
            None
        }
    });
    let input = CogPackInput {
        cog_dir: a.cog_dir.clone(),
        binaries,
        source: PackageSource {
            repo: a.source_repo.clone(),
            commit,
            release_url: a.release_url.clone(),
        },
        cognitum_record: a.cognitum_record.clone(),
        redistributable: a.redistributable,
        provenance: a.provenance.clone(),
        allow_no_provenance: a.no_provenance_ok,
    };
    let mut env = pack_cog(&input, &a.out)?;
    if let Some(key_path) = &a.key {
        let key = signing_key_from_hex(&std::fs::read_to_string(key_path)?)
            .map_err(|e| anyhow::anyhow!(e))?;
        let key_id = a
            .key_id
            .clone()
            .unwrap_or_else(|| key_id_for(key.verifying_key().as_bytes()));
        sign_envelope(&mut env, &key, &key_id)?;
        println!("signed by {key_id}");
    } else {
        eprintln!("warning: no --key given; package is unsigned and will not verify");
    }
    let manifest = write_manifest(&a.out, &env)?;
    let body = env.cog_body()?;
    println!(
        "packed {}@{} -> {}",
        body.id,
        body.version,
        manifest.display()
    );
    println!("package id {}", env.package_id()?);
    for (arch, f) in &body.binaries {
        println!(
            "  {arch:<13} {} {} bytes blake3 {}",
            f.path, f.size, f.blake3
        );
    }
    if let Some(store_dir) = &a.store {
        let anchors = load_trust(a.trust.as_deref())?;
        let store = ArtifactStore::open_file(store_dir.clone())?;
        let stored = store_package(&store, &a.out, &anchors, &VerifyPolicy::default())
            .map_err(|e| anyhow::anyhow!("not stored: [{}] {e}", e.code()))?;
        println!(
            "stored in {} manifest {}",
            store_dir.display(),
            stored.manifest_hash
        );
    }
    Ok(())
}

fn report_ok(v: &VerifiedPackage, json: bool) {
    let signers: Vec<String> = v
        .signers
        .iter()
        .map(|s| format!("{} ({:?})", s.key_id, s.origin))
        .collect();
    if json {
        let out = serde_json::json!({
            "ok": true, "package_id": v.package_id, "id": v.body.id, "version": v.body.version,
            "arches": v.body.binaries.keys().collect::<Vec<_>>(), "signers": signers,
        });
        println!("{out}");
    } else {
        println!(
            "OK {}@{} package {}",
            v.body.id, v.body.version, v.package_id
        );
        println!(
            "  arches: {}",
            v.body
                .binaries
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        );
        println!("  signers: {}", signers.join(", "));
    }
}

fn verify(a: VerifyArgs) -> anyhow::Result<()> {
    let anchors = load_trust(a.trust.as_deref())?;
    let policy = VerifyPolicy {
        accept_cognitum_release: a.cognitum_release,
        record_cog_toml_pins: a.cog_toml_pins.clone(),
    };
    let result = match (&a.store, &a.manifest, &a.dir) {
        (Some(store_dir), Some(hash), _) => {
            let store = ArtifactStore::open_file(store_dir.clone())?;
            verify_stored(&store, hash, &anchors, &policy)
        }
        (_, _, Some(dir)) => verify_dir(dir, &anchors, &policy),
        _ => anyhow::bail!("give a package directory, or --store with --manifest"),
    };
    match result {
        Ok(v) => {
            report_ok(&v, a.json);
            Ok(())
        }
        Err(e) => {
            if a.json {
                println!(
                    "{}",
                    serde_json::json!({"ok": false, "code": e.code(), "error": e.to_string()})
                );
            }
            eprintln!("FAIL [{}] {e}", e.code());
            std::process::exit(exit_code(&e));
        }
    }
}

fn keygen(a: KeygenArgs) -> anyhow::Result<()> {
    use rand::RngCore;
    use std::io::Write;
    let mut seed = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut seed);
    let key = ed25519_dalek::SigningKey::from_bytes(&seed);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&a.out)?;
    writeln!(f, "{}", hex::encode(seed))?;
    let pk = key.verifying_key().to_bytes();
    let key_id = a.key_id.unwrap_or_else(|| key_id_for(&pk));
    println!("wrote {}", a.out.display());
    println!("key id     {key_id}");
    println!("public key {}", hex::encode(pk));
    println!(
        "trust-file entry: {{\"key_id\":\"{key_id}\",\"public_key\":\"{}\"}}",
        hex::encode(pk)
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_are_distinct_per_failure_class() {
        let errs = [
            VerifyError::Manifest(String::new()),
            VerifyError::MissingSignature,
            VerifyError::UntrustedSigner {
                key_ids: String::new(),
            },
            VerifyError::BadSignature {
                key_id: String::new(),
            },
            VerifyError::FileMissing {
                path: String::new(),
            },
            VerifyError::HashMismatch {
                path: String::new(),
                expected: String::new(),
                actual: String::new(),
            },
            VerifyError::CognitumRecord(String::new()),
            VerifyError::Io {
                path: String::new(),
                msg: String::new(),
            },
        ];
        let mut codes: Vec<i32> = errs.iter().map(exit_code).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), errs.len());
        assert!(
            codes.iter().all(|c| *c > 1),
            "must not collide with anyhow's exit status 1"
        );
    }

    #[test]
    fn parse_bin_requires_arch_and_path() {
        assert_eq!(
            parse_bin("armv7=/x/y").unwrap(),
            ("armv7".into(), PathBuf::from("/x/y"))
        );
        assert!(parse_bin("armv7").is_err());
    }

    fn macho(cputype: u32, filetype: u32) -> Vec<u8> {
        let mut b = vec![0u8; 32];
        b[..4].copy_from_slice(&[0xcf, 0xfa, 0xed, 0xfe]);
        b[4..8].copy_from_slice(&cputype.to_le_bytes());
        b[12..16].copy_from_slice(&filetype.to_le_bytes());
        b
    }

    fn elf(machine: u16) -> Vec<u8> {
        let mut b = vec![0u8; 64];
        b[..4].copy_from_slice(b"\x7fELF");
        b[4] = 2;
        b[5] = 1;
        b[18..20].copy_from_slice(&machine.to_le_bytes());
        b
    }

    #[cfg(unix)]
    #[test]
    fn pack_rejects_a_bin_that_does_not_match_its_arch_key() {
        let arm = 0x0100_000c;
        assert!(check_bin_matches_key("aarch64-macos", &macho(arm, 2)).is_ok());
        assert!(check_bin_matches_key("aarch64-macos", &macho(0x0100_0007, 2)).is_err());
        assert!(check_bin_matches_key("aarch64-macos", &macho(arm, 6)).is_err());
        assert!(check_bin_matches_key("aarch64-macos", &elf(183)).is_err());
        assert!(check_bin_matches_key("aarch64-macos", b"#!/bin/sh\n").is_err());
        assert!(check_bin_matches_key("aarch64", &elf(183)).is_ok());
        assert!(check_bin_matches_key("aarch64", &elf(62)).is_err());
        assert!(check_bin_matches_key("x86_64", &elf(62)).is_ok());
        assert!(check_bin_matches_key("armv7", &elf(40)).is_ok());
        assert!(check_bin_matches_key("armv7", &macho(arm, 2)).is_err());
        assert!(check_bin_matches_key("x86_64", b"junk").is_err());
        assert!(check_bin_matches_key("wasm", b"\0asm").is_ok());
    }
}
