//! Provided MCP servers: a catalog file a publisher (WeaveLogic) ships, and
//! the step that turns the entries a user enabled into [`McpServerConfig`]s.
//!
//! ```json
//! {"schema": 1, "publisher": "weavelogic", "signature": "<hex, optional>",
//!  "servers": [{"id": "travel", "name": "Travel search", "publisher": "weavelogic",
//!               "authorized": true,
//!               "transport": {"type": "http", "url": "https://<host>/mcp"},
//!               "auth": {"kind": "bearer_env", "env": "TRAVEL_MCP_TOKEN"},
//!               "allow": ["search_*"], "gate": "open", "tool_gates": {"book_*": "confirm"},
//!               "use_for": "flights and trip plans"}]}
//! ```
//!
//! Signing mirrors signed-only installs in `weftos-cog-repo`: Ed25519, a
//! pinned public key, no bypass once a signature is present. The signed
//! payload is [`SIGN_PREFIX`] plus the canonical JSON (sorted keys, compact)
//! of the document without its `signature`, so a cog-release signature can
//! never validate a catalog. A signature that does not verify refuses the
//! whole catalog. An unsigned catalog loads unless `require_signature` is set.

use std::collections::BTreeMap;
use std::path::Path;

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::CosError;
use crate::config::{CosConfig, McpAuth, McpGate, McpOrigin, McpServerConfig, McpTransportConfig};

/// Domain separator for catalog signatures.
pub const SIGN_PREFIX: &[u8] = b"weftos-mcp-catalog-v1\n";

/// The catalog key built into the host: none until the publisher has issued
/// one. Until then a signed catalog needs `mcp_catalog.pinned_key`.
pub const BUILTIN_CATALOG_KEY_HEX: Option<&str> = None;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CatalogAuth {
    #[default]
    None,
    /// Bearer read from this environment variable (the user may point it
    /// elsewhere in `mcp_enable`).
    BearerEnv { env: String },
    /// Not supported yet; an entry that needs it cannot be enabled.
    Oauth {},
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogEntry {
    pub id: String,
    pub name: String,
    pub publisher: String,
    #[serde(default)]
    pub authorized: bool,
    pub transport: McpTransportConfig,
    #[serde(default)]
    pub auth: CatalogAuth,
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub gate: McpGate,
    #[serde(default)]
    pub tool_gates: BTreeMap<String, McpGate>,
    #[serde(default)]
    pub use_for: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Catalog {
    pub publisher: String,
    pub servers: Vec<CatalogEntry>,
    /// A signature was present and verified against the pinned key.
    pub signed: bool,
}

/// Sorted keys, compact: the same bytes however the file was formatted.
pub fn canonical(v: &Value) -> String {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<_> = m.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys.into_iter().map(|k| format!("{}:{}", Value::String(k.clone()), canonical(&m[k]))).collect();
            format!("{{{}}}", parts.join(","))
        }
        Value::Array(a) => format!("[{}]", a.iter().map(canonical).collect::<Vec<_>>().join(",")),
        other => other.to_string(),
    }
}

fn payload(doc: &Value) -> Vec<u8> {
    let mut body = doc.clone();
    if let Some(o) = body.as_object_mut() {
        o.remove("signature");
    }
    [SIGN_PREFIX, canonical(&body).as_bytes()].concat()
}

/// Hex Ed25519 signature for `doc` (what the publisher's tooling writes to
/// its `signature` field).
pub fn sign_catalog(doc: &Value, key: &SigningKey) -> String {
    hex::encode(key.sign(&payload(doc)).to_bytes())
}

fn parse_key(hex_key: &str) -> Result<VerifyingKey, CosError> {
    let raw: [u8; 32] = hex::decode(hex_key)
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| CosError::Config("catalog key is not 32 bytes of hex".into()))?;
    VerifyingKey::from_bytes(&raw).map_err(|_| CosError::Config("catalog key is not a valid Ed25519 public key".into()))
}

/// Parse and, when it carries a signature, verify a catalog document.
pub fn parse(text: &str, pinned: Option<&str>, require_signature: bool) -> Result<Catalog, CosError> {
    let doc: Value = serde_json::from_str(text).map_err(|e| CosError::Config(format!("catalog: {e}")))?;
    let signature = doc.get("signature").and_then(Value::as_str).map(str::to_string);
    let signed = match signature {
        Some(sig_hex) => {
            let key_hex = pinned.or(BUILTIN_CATALOG_KEY_HEX).ok_or_else(|| {
                CosError::Config("catalog is signed but no key is pinned; set mcp_catalog.pinned_key".into())
            })?;
            let key = parse_key(key_hex)?;
            let sig: [u8; 64] = hex::decode(&sig_hex)
                .ok()
                .and_then(|b| b.try_into().ok())
                .ok_or_else(|| CosError::Config("catalog signature is not 64 bytes of hex".into()))?;
            key.verify(&payload(&doc), &Signature::from_bytes(&sig))
                .map_err(|_| CosError::Config("catalog signature rejected (not signed by the pinned key, or the file was altered)".into()))?;
            true
        }
        None if require_signature => {
            return Err(CosError::Config("catalog has no signature and mcp_catalog.require_signature is set".into()));
        }
        None => false,
    };
    if doc.get("schema").and_then(Value::as_u64) != Some(1) {
        return Err(CosError::Config("catalog schema must be 1".into()));
    }
    let publisher = doc.get("publisher").and_then(Value::as_str).unwrap_or("unknown").to_string();
    let servers: Vec<CatalogEntry> = serde_json::from_value(doc.get("servers").cloned().unwrap_or(Value::Null))
        .map_err(|e| CosError::Config(format!("catalog servers: {e}")))?;
    Ok(Catalog { publisher, servers, signed })
}

pub fn load(path: &Path, pinned: Option<&str>, require_signature: bool) -> Result<Catalog, CosError> {
    let text = std::fs::read_to_string(path).map_err(|e| CosError::Config(format!("catalog {}: {e}", path.display())))?;
    parse(&text, pinned, require_signature)
}

impl Catalog {
    /// The entry `id`, as a server config limited by what the user asked for.
    pub fn enable(&self, e: &crate::config::McpEnable) -> Result<McpServerConfig, CosError> {
        let entry = self
            .servers
            .iter()
            .find(|s| s.id == e.id)
            .ok_or_else(|| CosError::Config(format!("mcp_enable `{}`: not in the catalog", e.id)))?;
        let mut auth = match &entry.auth {
            CatalogAuth::None => McpAuth::default(),
            CatalogAuth::BearerEnv { env } => McpAuth { bearer_env: Some(env.clone()), bearer_file: None },
            CatalogAuth::Oauth {} => {
                return Err(CosError::Config(format!("mcp_enable `{}`: this server needs OAuth, which is not supported yet", e.id)));
            }
        };
        if (e.auth.bearer_env.is_some() || e.auth.bearer_file.is_some()) && matches!(entry.auth, CatalogAuth::BearerEnv { .. }) {
            auth = e.auth.clone();
        }
        let cfg = McpServerConfig {
            id: entry.id.clone(),
            transport: entry.transport.clone(),
            auth,
            allow: entry.allow.clone(),
            gate: entry.gate,
            tool_gates: entry.tool_gates.clone(),
            use_for: entry.use_for.clone(),
            connect_timeout_secs: None,
            call_timeout_secs: None,
            max_output_bytes: None,
            retry_secs: None,
            origin: McpOrigin::Catalog { publisher: entry.publisher.clone(), authorized: entry.authorized, signed: self.signed },
            restrict_to: e.allow.clone(),
            force_confirm: e.confirm.clone(),
        };
        cfg.validate()?;
        Ok(cfg)
    }
}

/// Every server the host should run: hand-written ones (unvetted) plus the
/// enabled catalog entries.
pub fn resolve_servers(cfg: &CosConfig) -> Result<Vec<McpServerConfig>, CosError> {
    let mut out = cfg.mcp_servers.clone();
    if !cfg.mcp_enable.is_empty() {
        let path = cfg.mcp_catalog.path.as_ref().ok_or_else(|| CosError::Config("mcp_enable needs mcp_catalog.path".into()))?;
        let cat = load(path, cfg.mcp_catalog.pinned_key.as_deref(), cfg.mcp_catalog.require_signature)?;
        for e in &cfg.mcp_enable {
            out.push(cat.enable(e)?);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::McpEnable;
    use serde_json::json;

    fn key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }
    fn pub_hex() -> String {
        hex::encode(key().verifying_key().to_bytes())
    }
    fn doc() -> Value {
        json!({"schema": 1, "publisher": "weavelogic", "servers": [
            {"id": "travel", "name": "Travel", "publisher": "weavelogic", "authorized": true,
             "transport": {"type": "http", "url": "https://x.example/mcp"},
             "auth": {"kind": "bearer_env", "env": "TRAVEL_MCP_TOKEN"},
             "allow": ["search_*", "book_*"], "gate": "open", "tool_gates": {"book_*": "confirm"}, "use_for": "flights"},
            {"id": "files", "name": "Files", "publisher": "weavelogic", "authorized": true,
             "transport": {"type": "http", "url": "https://y.example/mcp"}, "auth": {"kind": "oauth"}, "allow": ["*"]}]})
    }
    fn signed(mut d: Value) -> String {
        let sig = sign_catalog(&d, &key());
        d["signature"] = json!(sig);
        d.to_string()
    }
    fn enable(id: &str) -> McpEnable {
        McpEnable { id: id.into(), allow: vec![], confirm: vec![], auth: Default::default() }
    }

    #[test]
    fn a_valid_signature_verifies_whatever_the_formatting() {
        let text = signed(doc());
        let reformatted = serde_json::to_string_pretty(&serde_json::from_str::<Value>(&text).unwrap()).unwrap();
        for t in [text, reformatted] {
            assert!(parse(&t, Some(&pub_hex()), true).unwrap().signed);
        }
    }

    #[test]
    fn a_bad_signature_refuses_the_whole_catalog() {
        let mut d: Value = serde_json::from_str(&signed(doc())).unwrap();
        d["servers"][0]["allow"] = json!(["*"]); // altered after signing
        let err = parse(&d.to_string(), Some(&pub_hex()), false).unwrap_err().to_string();
        assert!(err.contains("signature rejected"), "{err}");
        let other = hex::encode(SigningKey::from_bytes(&[9u8; 32]).verifying_key().to_bytes());
        assert!(parse(&signed(doc()), Some(&other), false).is_err(), "wrong key");
        let mut d = doc();
        d["signature"] = json!("zz");
        assert!(parse(&d.to_string(), Some(&pub_hex()), false).unwrap_err().to_string().contains("64 bytes"));
    }

    #[test]
    fn a_signed_catalog_with_no_pinned_key_is_refused() {
        let err = parse(&signed(doc()), None, false).unwrap_err().to_string();
        assert!(err.contains("no key is pinned"), "{err}");
    }

    #[test]
    fn a_signature_over_other_bytes_does_not_validate_a_catalog() {
        // The same key signing the bare document (no domain prefix), as a cog release would.
        let d = doc();
        let mut with = d.clone();
        with["signature"] = json!(hex::encode(key().sign(canonical(&d).as_bytes()).to_bytes()));
        assert!(parse(&with.to_string(), Some(&pub_hex()), false).is_err());
    }

    #[test]
    fn unsigned_loads_unless_a_signature_is_required() {
        let c = parse(&doc().to_string(), None, false).unwrap();
        assert!(!c.signed);
        assert!(parse(&doc().to_string(), None, true).unwrap_err().to_string().contains("require_signature"));
    }

    #[test]
    fn enabling_takes_the_catalog_defaults_and_marks_the_origin() {
        let c = parse(&signed(doc()), Some(&pub_hex()), true).unwrap();
        let s = c.enable(&enable("travel")).unwrap();
        assert_eq!((s.allow.clone(), s.gate), (vec!["search_*".to_string(), "book_*".to_string()], McpGate::Open));
        assert_eq!(s.auth.bearer_env.as_deref(), Some("TRAVEL_MCP_TOKEN"));
        assert_eq!(s.origin, McpOrigin::Catalog { publisher: "weavelogic".into(), authorized: true, signed: true });
        assert!(s.origin.label().contains("authorized by weavelogic, signature verified"));
        assert_eq!(McpOrigin::default().label(), "unvetted");
    }

    #[test]
    fn enabling_can_narrow_and_tighten_but_not_widen_and_oauth_is_refused() {
        let c = parse(&doc().to_string(), None, false).unwrap();
        let s = c.enable(&McpEnable { allow: vec!["search_flights".into()], confirm: vec!["*".into()], ..enable("travel") }).unwrap();
        assert_eq!(s.restrict_to, ["search_flights"]);
        assert_eq!(s.allow, ["search_*", "book_*"], "the catalog allow-list is untouched");
        assert_eq!(s.force_confirm, ["*"]);
        let s = c.enable(&McpEnable { auth: McpAuth { bearer_env: Some("MY_TRAVEL_TOKEN".into()), bearer_file: None }, ..enable("travel") }).unwrap();
        assert_eq!(s.auth.bearer_env.as_deref(), Some("MY_TRAVEL_TOKEN"));
        assert!(c.enable(&enable("files")).unwrap_err().to_string().contains("OAuth"));
        assert!(c.enable(&enable("nope")).unwrap_err().to_string().contains("not in the catalog"));
        assert!(!c.signed);
    }

    #[test]
    fn unknown_entry_fields_are_refused() {
        let mut d = doc();
        d["servers"][0]["trusted"] = json!(true);
        assert!(parse(&d.to_string(), None, false).is_err());
    }
}
