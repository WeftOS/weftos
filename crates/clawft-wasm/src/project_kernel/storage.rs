//! Guest-owned persistent state within the runner's single WASI preopen.
use crate::{Result, chain::ChainManager};
use ed25519_dalek::SigningKey;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
};

pub const CHAIN: &str = "/project/chain/chain.rvf";

pub fn atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    // The runner holds the OS chain lock; one guest may write this state.
    let next = path.with_extension("wasm-next");
    let mut f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&next)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    drop(f);
    fs::rename(next, path)?;
    Ok(())
}

pub fn key() -> Result<SigningKey> {
    let path = Path::new("/project/project.key");
    match fs::read(path) {
        Ok(seed) => Ok(SigningKey::from_bytes(
            &seed
                .try_into()
                .map_err(|_| "project key must be 32 bytes")?,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // WASI random_get: host entropy only, key generation/signing in guest.
            let key = SigningKey::generate(&mut rand::rngs::OsRng);
            let mut f = OpenOptions::new().create_new(true).write(true).open(path)?;
            f.write_all(&key.to_bytes())?;
            f.sync_all()?;
            Ok(key)
        }
        Err(e) => Err(e.into()),
    }
}

pub fn load_chain(key: &SigningKey) -> Result<ChainManager> {
    match fs::metadata(CHAIN) {
        Ok(_) => {
            let bytes = fs::read(CHAIN)?;
            let chain = ChainManager::load_from_rvf(Path::new(CHAIN), 100)?;
            verify_restored(&bytes, key, &chain)?;
            Ok(chain.with_signing_key(key.clone()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Ok(ChainManager::new(0, 100).with_signing_key(key.clone()))
        }
        Err(e) => Err(e.into()),
    }
}

pub fn save_chain(chain: &ChainManager) -> Result<()> {
    let next = Path::new("/project/chain/chain.wasm-next");
    chain.save_to_rvf(next)?;
    OpenOptions::new().read(true).open(next)?.sync_all()?;
    fs::rename(next, CHAIN)?;
    Ok(())
}

/// Persistence failure is fatal: never serve from an uncommitted new policy
/// or acknowledge an in-memory-only audit event.
pub fn durable(chain: &ChainManager) -> Result<()> {
    if save_chain(chain).is_err() {
        std::process::exit(2);
    }
    Ok(())
}

/// Authenticate the exact checkpoint being compared, not a separate reread.
/// Loading may race a file replacement: the restored chain must still match
/// this authenticated commitment, including each event's chain ID/sequence.
fn verify_restored(bytes: &[u8], key: &SigningKey, chain: &ChainManager) -> Result<()> {
    use rvf_crypto::{decode_signature_footer, verify_segment};
    use rvf_wire::{read_segment, validate_segment, writer::calculate_padded_size};
    let mut offset = 0usize;
    let mut last = None;
    while let Ok((header, payload)) = read_segment(&bytes[offset..]) {
        validate_segment(&header, payload).map_err(|e| format!("segment: {e}"))?;
        let size = calculate_padded_size(
            rvf_types::SEGMENT_HEADER_SIZE,
            header.payload_length as usize,
        );
        let next = offset
            .checked_add(size)
            .filter(|n| *n <= bytes.len())
            .ok_or("truncated segment")?;
        last = Some((header, payload));
        offset = next;
    }
    let (header, payload) = last.ok_or("missing checkpoint")?;
    let footer = decode_signature_footer(&bytes[offset..]).map_err(|e| format!("footer: {e}"))?;
    if !verify_segment(&header, payload, &footer, &key.verifying_key()) {
        return Err("project chain signature failed".into());
    }
    // ExoChainHeader v1 is 64 bytes; see the shared chain writer.
    if payload.len() < 64
        || payload[..4] != 0x4558_4f43u32.to_le_bytes()
        || payload[4] != 1
        || payload[5] != 0x41
    {
        return Err("signed segment is not a checkpoint".into());
    }
    let id = u32::from_le_bytes(payload[8..12].try_into()?);
    let sequence = u64::from_le_bytes(payload[16..24].try_into()?);
    let cp: serde_json::Value = ciborium::from_reader(&payload[64..])?;
    let status = chain.status();
    let events = chain.tail(0);
    if !chain.verify_integrity().valid
        || id != status.chain_id
        || sequence != status.sequence.saturating_sub(1)
        || payload[32..64] != status.last_hash
        || cp["last_hash"].as_str()
            != Some(&clawft_types::project::canon::hex_encode(&status.last_hash))
        || cp["event_count"].as_u64() != Some(status.event_count as u64)
        || events
            .iter()
            .enumerate()
            .any(|(i, e)| e.chain_id != id || e.sequence != i as u64)
    {
        return Err("signed checkpoint does not commit to restored chain".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retained_signed_footer_cannot_authenticate_rewritten_events() {
        use rvf_wire::{read_segment, writer::calculate_padded_size};
        // Real chain serialization: both event histories are internally valid.
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!(
            ".wasm-chain-test-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        fs::create_dir(&dir).unwrap();
        let key = SigningKey::from_bytes(&[17; 32]);
        let original = ChainManager::new(0, 100).with_signing_key(key.clone());
        original.append("test", "original", Some(serde_json::json!({"value":1})));
        let path = dir.join("original.rvf");
        original.save_to_rvf(&path).unwrap();
        let good = fs::read(&path).unwrap();
        verify_restored(&good, &key, &original).unwrap();
        let forged = ChainManager::new(0, 100); // attacker has no signing key
        forged.append("test", "forged", Some(serde_json::json!({"value":2})));
        let forged_path = dir.join("forged.rvf");
        forged.save_to_rvf(&forged_path).unwrap();
        let bad = fs::read(&forged_path).unwrap();
        fn checkpoint(bytes: &[u8]) -> usize {
            let mut at = 0;
            loop {
                let (h, p) = read_segment(&bytes[at..]).unwrap();
                if p[5] == 0x41 {
                    return at;
                }
                at += calculate_padded_size(
                    rvf_types::SEGMENT_HEADER_SIZE,
                    h.payload_length as usize,
                );
            }
        }
        let mut attack = bad[..checkpoint(&bad)].to_vec();
        attack.extend_from_slice(&good[checkpoint(&good)..]);
        fs::write(&forged_path, &attack).unwrap();
        assert!(ChainManager::verify_rvf_signature(&forged_path, &key.verifying_key()).unwrap());
        let loaded = ChainManager::load_from_rvf(&forged_path, 100).unwrap();
        assert!(loaded.verify_integrity().valid);
        assert!(verify_restored(&attack, &key, &loaded).is_err());
        fs::remove_file(path).unwrap();
        fs::remove_file(forged_path).unwrap();
        fs::remove_dir(dir).unwrap();
    }
}
