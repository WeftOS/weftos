//! Loopback mock release server and release publisher for the update tests.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clawft_rpc::doctor::probe::sha256_file;
use ed25519_dalek::{Signer, SigningKey};

use super::update_release::Source;
use super::update_signature::{DOMAIN, SIGNATURE, SIGNED_DOC, Trust};
use cog_repo::RevokedKeys;

pub const TRIPLE: &str = "test-triple";
pub const BINS: [(&str, &str); 3] = [("weft", "clawft-cli"), ("weaver", "clawft-weave"), ("weftos", "weftos")];

/// Throwaway release key the mock signs with; never the real one.
pub fn test_key() -> SigningKey {
    SigningKey::from_bytes(&[0x5a; 32])
}

pub fn test_trust() -> Trust {
    Trust::Pinned { key: test_key().verifying_key(), revoked: RevokedKeys::none() }
}

pub type Routes = Arc<Mutex<HashMap<String, Vec<u8>>>>;

pub struct Mock {
    pub port: u16,
    pub routes: Routes,
    log: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
}

impl Mock {
    pub fn start() -> Self {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let routes: Routes = Arc::default();
        let log: Arc<Mutex<Vec<String>>> = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let (r, lg, st) = (routes.clone(), log.clone(), stop.clone());
        std::thread::spawn(move || {
            for conn in l.incoming() {
                if st.load(Ordering::SeqCst) {
                    break;
                }
                if let Ok(c) = conn {
                    serve(c, &r, &lg);
                }
            }
        });
        Mock { port, routes, log, stop }
    }

    pub fn source(&self) -> Source {
        Source {
            base: format!("http://127.0.0.1:{}/r", self.port),
            allow_http: true,
            max_extract_bytes: super::update_release::MAX_EXTRACT_BYTES,
            curl_env: Vec::new(),
        }
    }

    pub fn asset_requests(&self) -> usize {
        let sig_file = |p: &str| p.ends_with(SIGNED_DOC) || p.ends_with(SIGNATURE);
        self.log.lock().unwrap().iter().filter(|p| p.contains("/download/v") && !sig_file(p)).count()
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
    }
}

fn serve(mut c: TcpStream, routes: &Routes, log: &Arc<Mutex<Vec<String>>>) {
    let _ = c.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buf = [0u8; 4096];
    let n = c.read(&mut buf).unwrap_or(0);
    let req = String::from_utf8_lossy(&buf[..n]).to_string();
    let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
    log.lock().unwrap().push(path.clone());
    let body = routes.lock().unwrap().get(&path).cloned();
    let _ = match body {
        Some(b) => {
            let _ = write!(c, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", b.len());
            c.write_all(&b)
        }
        None => c.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
    };
}

pub fn script(bin: &str, version: &str) -> String {
    format!("#!/bin/sh\necho \"{bin} {version}\"\n")
}

#[derive(Clone, Copy, PartialEq)]
pub enum Evil {
    Symlink,
    Hardlink,
    DotDot,
    Big,
}

fn evil_tar(path: &Path, kind: Evil) {
    let gz = flate2::write::GzEncoder::new(std::fs::File::create(path).unwrap(), flate2::Compression::fast());
    let mut b = tar::Builder::new(gz);
    let good = script("weftos", "0.9.0");
    let mut h = tar::Header::new_gnu();
    h.set_size(good.len() as u64);
    h.set_mode(0o755);
    h.set_path(format!("weftos-{TRIPLE}/weftos")).unwrap();
    h.set_cksum();
    b.append(&h, good.as_bytes()).unwrap();
    let mut h = tar::Header::new_gnu();
    h.set_mode(0o755);
    match kind {
        Evil::Symlink | Evil::Hardlink => {
            h.set_entry_type(if kind == Evil::Symlink { tar::EntryType::Symlink } else { tar::EntryType::Link });
            h.set_path(format!("weftos-{TRIPLE}/link")).unwrap();
            h.set_link_name(if kind == Evil::Symlink { "/etc/passwd" } else { "weftos-test-triple/weftos" }).unwrap();
            h.set_size(0);
            h.set_cksum();
            b.append(&h, std::io::empty()).unwrap();
        }
        Evil::DotDot => {
            h.as_old_mut().name[..7].copy_from_slice(b"../evil");
            h.set_size(1);
            h.set_cksum();
            b.append(&h, &b"x"[..]).unwrap();
        }
        Evil::Big => {
            h.set_path(format!("weftos-{TRIPLE}/big")).unwrap();
            h.set_size(8192);
            h.set_cksum();
            b.append(&h, std::io::repeat(0).take(8192)).unwrap();
        }
    }
    b.into_inner().unwrap().finish().unwrap();
}

/// How (or whether) the mock signs the release.
#[derive(Default, Clone, Copy, PartialEq)]
pub enum Sign {
    #[default]
    Good,
    /// Neither `weftos-release.json` nor its signature.
    Unsigned,
    /// The hash list without a signature.
    NoSig,
    /// Signed by some other key.
    WrongKey,
    /// The signature file is not hex.
    Garbage,
    /// Signed over the raw document, without the domain prefix.
    NoDomain,
    /// A correctly signed hash list, but for another tag (a replay).
    OtherTag,
    /// After signing, replace this archive and republish matching `.sha256`
    /// and `sha256.sum`: what someone holding the release (not the key) can do.
    TamperRehash(&'static str),
    /// After signing, edit `dist-manifest.json`.
    TamperManifest,
}

#[derive(Default, Clone)]
pub struct Rel {
    pub sign: Sign,
    /// The signed `published` time (default: now).
    pub published: Option<&'static str>,
    /// Make the `weftos` archive hostile (checksums are still published and valid).
    pub evil: Option<Evil>,
    /// Version the binaries inside the archives report (defaults to the release's).
    pub payload_version: Option<&'static str>,
    /// Replace this archive's bytes after its checksum was published.
    pub corrupt: Option<&'static str>,
    /// Publish a `sha256.sum` that disagrees.
    pub bad_unified: bool,
    /// Do not publish this archive's `.sha256`.
    pub no_sha: Option<&'static str>,
}

fn make_tar(work: &Path, stage: &str, stem: &str, bin: &str, content: &str) -> std::path::PathBuf {
    let dir = work.join(format!("{stage}-{stem}/{stem}-{TRIPLE}"));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(bin), content).unwrap();
    let tarball = work.join(format!("{stage}-{stem}-{TRIPLE}.tar.gz"));
    let st = Command::new("tar")
        .arg("czf")
        .arg(&tarball)
        .arg("-C")
        .arg(work.join(format!("{stage}-{stem}")))
        .arg(format!("{stem}-{TRIPLE}"))
        .status()
        .unwrap();
    assert!(st.success());
    tarball
}

fn sign_doc(key: &SigningKey, doc: &[u8], domain: bool) -> Vec<u8> {
    let mut m = if domain { DOMAIN.to_vec() } else { Vec::new() };
    m.extend_from_slice(doc);
    format!("{}\n", hex::encode(key.sign(&m).to_bytes())).into_bytes()
}

/// The signed hash list over every published file of `tag`, as CI writes it.
pub fn hash_doc(routes: &HashMap<String, Vec<u8>>, tag: &str, doc_tag: &str, published: &str) -> Vec<u8> {
    let prefix = format!("/r/download/{tag}/");
    let mut assets = std::collections::BTreeMap::new();
    for (path, body) in routes {
        if let Some(name) = path.strip_prefix(&prefix) {
            assets.insert(name.to_string(), super::update_signature::sha256_hex(body));
        }
    }
    serde_json::to_vec_pretty(&serde_json::json!({"schema": 1, "kind": "weftos-release", "tag": doc_tag, "published": published, "assets": assets}))
        .unwrap()
}

pub fn publish(mock: &Mock, work: &Path, version: &str, rel: &Rel) {
    let tag = format!("v{version}");
    let mut routes = mock.routes.lock().unwrap();
    let mut arts = serde_json::Map::new();
    let mut unified = String::new();
    for (bin, stem) in BINS {
        let name = format!("{stem}-{TRIPLE}.tar.gz");
        let tarball = make_tar(work, "stage", stem, bin, &script(bin, rel.payload_version.unwrap_or(version)));
        if stem == "weftos" && let Some(k) = rel.evil {
            evil_tar(&tarball, k);
        }
        let sha = sha256_file(&tarball).unwrap();
        let mut bytes = std::fs::read(&tarball).unwrap();
        if rel.corrupt == Some(stem) {
            bytes.extend_from_slice(b"tampered");
        }
        routes.insert(format!("/r/download/{tag}/{name}"), bytes);
        if rel.no_sha != Some(stem) {
            routes.insert(format!("/r/download/{tag}/{name}.sha256"), format!("{sha}  {name}\n").into_bytes());
        }
        unified.push_str(&format!("{}  {name}\n", if rel.bad_unified { "0".repeat(64) } else { sha.clone() }));
        arts.insert(
            name.clone(),
            serde_json::json!({"name": name, "kind": "executable-zip", "target_triples": [TRIPLE],
                "assets": [{"name": bin, "path": bin, "kind": "executable"}], "checksum": format!("{name}.sha256")}),
        );
    }
    arts.insert("sha256.sum".into(), serde_json::json!({"name": "sha256.sum", "kind": "unified-checksum"}));
    routes.insert(format!("/r/download/{tag}/sha256.sum"), unified.into_bytes());
    let manifest = serde_json::json!({"announcement_tag": tag, "artifacts": arts}).to_string().into_bytes();
    // CI uploads dist-manifest.json as a release asset; `latest/` redirects to it.
    routes.insert(format!("/r/download/{tag}/dist-manifest.json"), manifest.clone());
    routes.insert("/r/latest/download/dist-manifest.json".into(), manifest);
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    sign_release(&mut routes, work, version, rel.sign, rel.published.unwrap_or(&now));
}

fn sign_release(routes: &mut HashMap<String, Vec<u8>>, work: &Path, version: &str, mode: Sign, published: &str) {
    let tag = format!("v{version}");
    let at = |n: &str| format!("/r/download/{tag}/{n}");
    let doc_tag = if mode == Sign::OtherTag { "v0.8.5" } else { tag.as_str() };
    let doc = hash_doc(routes, &tag, doc_tag, published);
    let sig = match mode {
        Sign::WrongKey => sign_doc(&SigningKey::from_bytes(&[9; 32]), &doc, true),
        Sign::Garbage => b"not a signature\n".to_vec(),
        Sign::NoDomain => sign_doc(&test_key(), &doc, false),
        _ => sign_doc(&test_key(), &doc, true),
    };
    if mode != Sign::Unsigned {
        routes.insert(at(SIGNED_DOC), doc);
    }
    if !matches!(mode, Sign::Unsigned | Sign::NoSig) {
        routes.insert(at(SIGNATURE), sig);
    }
    match mode {
        Sign::TamperRehash(stem) => {
            let (bin, _) = BINS.iter().find(|(_, s)| *s == stem).copied().unwrap();
            let evil = format!("{}# backdoor\n", script(bin, version));
            let tarball = make_tar(work, "evil", stem, bin, &evil);
            let name = format!("{stem}-{TRIPLE}.tar.gz");
            let sha = sha256_file(&tarball).unwrap();
            let old = super::update_signature::sha256_hex(&routes[&at(&name)]);
            routes.insert(at(&name), std::fs::read(&tarball).unwrap());
            routes.insert(at(&format!("{name}.sha256")), format!("{sha}  {name}\n").into_bytes());
            let sum = String::from_utf8(routes[&at("sha256.sum")].clone()).unwrap().replace(&old, &sha);
            routes.insert(at("sha256.sum"), sum.into_bytes());
        }
        Sign::TamperManifest => {
            let m = routes.get_mut("/r/latest/download/dist-manifest.json").unwrap();
            m.splice(1..1, br#""note":"edited","#.iter().copied());
        }
        _ => {}
    }
}

