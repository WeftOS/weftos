//! Release discovery, download and verification for `weaver update`.
//!
//! Trust chain, all of it fail-closed:
//! 1. `dist-manifest.json` (cargo-dist) names the release tag and, per target
//!    triple, the archives and the binaries inside. It is the only source of
//!    the release set; nothing is guessed from asset names.
//! 2. Every archive is hashed and compared with its published `.sha256`
//!    (and with `sha256.sum` when the manifest declares one). A missing,
//!    malformed or disagreeing checksum aborts the update.
//! 3. Archives are extracted by [`extract`], never by `tar`: only regular
//!    files and directories are written; symlinks, hardlinks, devices,
//!    absolute and `..` paths are refused, and total size and entry count are
//!    capped. Each binary is then run once (`--version`) so a wrong-arch or
//!    wrong-version payload is caught before anything is installed.
//!
//! 4. Authenticity: `weftos-release.json` lists the sha256 of every release
//!    asset and is Ed25519-signed by the compiled-in WeaveLogic release key
//!    (see [`super::update_signature`]). The signature is checked before the
//!    manifest is trusted, the manifest's own sha256 and tag must match the
//!    signed list, and every archive must hash to its signed entry. The
//!    `.sha256` files alone only prove integrity: they come from the same
//!    release as the archives, so whoever can replace an archive can rehash it.
//!
//! Downloads use `curl -q` (no `.curlrc`), https only, at most 5 redirects.
//! CA-bundle overrides (`CURL_CA_BUNDLE`, `SSL_CERT_FILE`, `SSL_CERT_DIR`) are
//! dropped; standard proxy variables (`HTTPS_PROXY`, `ALL_PROXY`, `NO_PROXY`)
//! still apply, so a proxy you configured is used.
//!
//! Nothing here writes outside the staging directory it is handed.

use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use clawft_rpc::doctor::probe::{parse_semver, probe_version, sha256_file};
use serde_json::Value;

use super::update_signature::{self as signature, SignedRelease, Trust};

/// Binaries `weaver update` is willing to install. A manifest naming anything
/// else is ignored for that entry: a release never gets to pick new file names
/// in the user's bin directory.
pub const KNOWN_BINARIES: [&str; 3] = ["weft", "weaver", "weftos"];

const MAX_TEXT_BYTES: u64 = 4 * 1024 * 1024;
/// Most bytes one archive may extract to (the three binaries are ~100 MB together).
pub const MAX_EXTRACT_BYTES: u64 = 512 * 1024 * 1024;
const MAX_ENTRIES: usize = 10_000;

/// Where releases are fetched from.
#[derive(Debug, Clone)]
pub struct Source {
    /// `https://github.com/<owner>/<repo>/releases`.
    pub base: String,
    /// Allow plain `http://` (loopback mock servers in tests only).
    pub allow_http: bool,
    /// Extraction size cap per archive.
    pub max_extract_bytes: u64,
    /// Extra environment for `curl` (tests: a temp `HOME` with a hostile `.curlrc`).
    pub curl_env: Vec<(String, String)>,
}

impl Source {
    /// The real GitHub Releases of this project.
    pub fn github() -> Self {
        Self {
            base: "https://github.com/WeftOS/weftos/releases".into(),
            allow_http: false,
            max_extract_bytes: MAX_EXTRACT_BYTES,
            curl_env: Vec::new(),
        }
    }

    fn manifest_url(&self) -> String {
        format!("{}/latest/download/dist-manifest.json", self.base)
    }

    fn asset_url(&self, tag: &str, name: &str) -> String {
        format!("{}/download/{tag}/{name}", self.base)
    }
}

/// One archive of the release set for this platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseArtifact {
    pub name: String,
    pub checksum_name: String,
    pub binaries: Vec<String>,
}

/// The release set for one target triple.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub tag: String,
    pub version: String,
    pub artifacts: Vec<ReleaseArtifact>,
    pub unified_checksum: Option<String>,
    /// The verified signed hash list; `None` only under [`Trust::Skip`].
    pub signed: Option<SignedRelease>,
}

impl Release {
    /// Every binary the release installs, in manifest order.
    pub fn binaries(&self) -> Vec<String> {
        self.artifacts.iter().flat_map(|a| a.binaries.clone()).collect()
    }

    /// Archive that carries `bin`.
    pub fn artifact_of(&self, bin: &str) -> Option<&ReleaseArtifact> {
        self.artifacts.iter().find(|a| a.binaries.iter().any(|b| b == bin))
    }
}

/// A verified, extracted binary waiting to be installed.
#[derive(Debug, Clone)]
pub struct Staged {
    pub name: String,
    pub path: PathBuf,
}

fn safe_name(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'))
}

/// Parse a cargo-dist `dist-manifest.json` for `triple`.
pub fn parse_manifest(text: &str, triple: &str) -> anyhow::Result<Release> {
    let v: Value = serde_json::from_str(text).context("dist-manifest.json is not valid JSON")?;
    let tag = v["announcement_tag"].as_str().ok_or_else(|| anyhow!("manifest has no announcement_tag"))?;
    if !safe_name(tag) {
        bail!("manifest tag {tag:?} is not a plain tag name");
    }
    let version = tag.strip_prefix('v').unwrap_or(tag).to_string();
    if parse_semver(&version).is_none() {
        bail!("manifest tag {tag:?} is not a version");
    }
    for r in v["releases"].as_array().into_iter().flatten() {
        if let Some(av) = r["app_version"].as_str()
            && av.strip_prefix('v').unwrap_or(av) != version
        {
            bail!("manifest tag {tag} does not match release version {av}");
        }
    }
    let arts = v["artifacts"].as_object().ok_or_else(|| anyhow!("manifest has no artifacts"))?;
    let mut artifacts: Vec<ReleaseArtifact> = Vec::new();
    let mut unified_checksum = None;
    for (key, a) in arts {
        match a["kind"].as_str() {
            Some("unified-checksum") => {
                let name = a["name"].as_str().unwrap_or(key);
                if !safe_name(name) {
                    bail!("manifest checksum file name {name:?} is not a plain file name");
                }
                unified_checksum = Some(name.to_string());
            }
            Some("executable-zip") => {
                let on_triple = a["target_triples"]
                    .as_array()
                    .is_some_and(|t| t.iter().any(|x| x.as_str() == Some(triple)));
                if !on_triple {
                    continue;
                }
                let name = a["name"].as_str().unwrap_or(key);
                if !safe_name(name) {
                    bail!("manifest archive name {name:?} is not a plain file name");
                }
                let checksum_name =
                    a["checksum"].as_str().map(str::to_owned).unwrap_or_else(|| format!("{name}.sha256"));
                if !safe_name(&checksum_name) {
                    bail!("manifest checksum name {checksum_name:?} is not a plain file name");
                }
                let binaries: Vec<String> = a["assets"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|x| x["kind"].as_str() == Some("executable"))
                    .filter_map(|x| x["name"].as_str().or_else(|| x["path"].as_str()))
                    .map(|n| n.trim_end_matches(".exe").to_string())
                    .filter(|n| KNOWN_BINARIES.contains(&n.as_str()))
                    .collect();
                if !binaries.is_empty() {
                    artifacts.push(ReleaseArtifact { name: name.into(), checksum_name, binaries });
                }
            }
            _ => {}
        }
    }
    if artifacts.is_empty() {
        bail!("release {tag} has no archive for {triple}");
    }
    let mut seen: Vec<String> = Vec::new();
    for b in artifacts.iter().flat_map(|a| &a.binaries) {
        if seen.contains(b) {
            bail!("release {tag} ships {b} in more than one archive for {triple}");
        }
        seen.push(b.clone());
    }
    if !seen.iter().any(|b| b == "weaver") {
        bail!("release {tag} has no weaver binary for {triple}");
    }
    Ok(Release { tag: tag.into(), version, artifacts, unified_checksum, signed: None })
}

/// Hash from a `.sha256` file (`<hex>  <file>` or `<hex> *<file>`); when the
/// file names a name it must be `artifact`.
pub fn parse_sha256_file(text: &str, artifact: &str) -> anyhow::Result<String> {
    let line = text.lines().find(|l| !l.trim().is_empty()).ok_or_else(|| anyhow!("empty checksum file"))?;
    let mut parts = line.split_whitespace();
    let hash = parts.next().unwrap_or("").to_ascii_lowercase();
    if hash.len() != 64 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("malformed checksum for {artifact}");
    }
    if let Some(file) = parts.next() {
        let file = file.trim_start_matches('*');
        let file = file.rsplit('/').next().unwrap_or(file);
        if file != artifact {
            bail!("checksum file is for {file}, not {artifact}");
        }
    }
    Ok(hash)
}

/// Hash for `artifact` out of a unified `sha256.sum`, when it lists it.
pub fn find_in_unified(text: &str, artifact: &str) -> Option<String> {
    text.lines().find_map(|l| {
        let mut p = l.split_whitespace();
        let (h, f) = (p.next()?, p.next()?);
        let f = f.trim_start_matches('*');
        (f.rsplit('/').next() == Some(artifact)).then(|| h.to_ascii_lowercase())
    })
}

fn curl(src: &Source, url: &str, dest: &Path) -> anyhow::Result<()> {
    if !src.allow_http && !url.starts_with("https://") {
        bail!("refusing non-https URL {url}");
    }
    // `-q` must be the first argument: it makes curl ignore any `.curlrc`.
    let mut c = Command::new("curl");
    c.args(["-q", "-fsSL", "--max-redirs", "5", "--max-time", "300", "--max-filesize", "1073741824"]);
    if !src.allow_http {
        c.args(["--proto", "=https", "--proto-redir", "=https"]);
    }
    for k in ["CURL_CA_BUNDLE", "SSL_CERT_FILE", "SSL_CERT_DIR"] {
        c.env_remove(k);
    }
    c.envs(src.curl_env.iter().map(|(k, v)| (k, v)));
    let status = c.arg("-o").arg(dest).arg(url).status().context("cannot run curl")?;
    if !status.success() {
        bail!("download failed ({status}): {url}");
    }
    Ok(())
}

fn fetch_text(src: &Source, url: &str, scratch: &Path) -> anyhow::Result<String> {
    let f = scratch.join("fetch.txt");
    curl(src, url, &f)?;
    if std::fs::metadata(&f)?.len() > MAX_TEXT_BYTES {
        bail!("{url} is larger than {MAX_TEXT_BYTES} bytes");
    }
    std::fs::read_to_string(&f).with_context(|| format!("{url} is not text"))
}

/// Fetch the latest release's manifest for `triple` and, unless `trust` is
/// [`Trust::Skip`], verify it against the signed hash list for its tag. A
/// missing or bad signature fails here, before anything else is believed.
pub fn fetch_latest(src: &Source, triple: &str, scratch: &Path, trust: &Trust) -> anyhow::Result<Release> {
    if let Trust::Pinned { key, revoked } = trust {
        signature::check_not_revoked(key, revoked)?;
    }
    let text = fetch_text(src, &src.manifest_url(), scratch)?;
    let mut rel = parse_manifest(&text, triple)?;
    let Trust::Pinned { key, .. } = trust else {
        return Ok(rel);
    };
    let missing = |what: &str| format!("release {} has no {what}; refusing an unsigned release", rel.tag);
    let doc = fetch_text(src, &src.asset_url(&rel.tag, signature::SIGNED_DOC), scratch)
        .with_context(|| missing(signature::SIGNED_DOC))?;
    let sig = fetch_text(src, &src.asset_url(&rel.tag, signature::SIGNATURE), scratch)
        .with_context(|| missing(signature::SIGNATURE))?;
    let signed = signature::verify(doc.as_bytes(), &sig, key)?;
    if signed.tag != rel.tag {
        bail!("signed release is for {}, but the manifest says {}", signed.tag, rel.tag);
    }
    signed.check(signature::MANIFEST, &signature::sha256_hex(text.as_bytes()))?;
    rel.signed = Some(signed);
    Ok(rel)
}

/// Run `bin --version` and require it to report `version`.
fn smoke_check(path: &Path, version: &str) -> anyhow::Result<()> {
    let got = probe_version(path, Duration::from_secs(10))
        .ok_or_else(|| anyhow!("{} did not run on this host (wrong platform?)", path.display()))?;
    if parse_semver(&got.version) != parse_semver(version) {
        bail!("{} reports {} but the release is {version}", path.display(), got.version);
    }
    Ok(())
}

fn unsafe_path(p: &Path) -> bool {
    p.as_os_str().is_empty() || p.components().any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
}

/// Extract `tarball` into `dest`: regular files and directories only, bounded
/// in entries and bytes. Nothing is ever created as a link, so no entry can
/// redirect a later write.
pub fn extract(tarball: &Path, dest: &Path, max_bytes: u64) -> anyhow::Result<()> {
    let gz = flate2::read::GzDecoder::new(std::fs::File::open(tarball)?);
    let mut ar = tar::Archive::new(gz);
    let mut total = 0u64;
    for (n, entry) in ar.entries()?.enumerate() {
        let mut entry = entry.context("corrupt archive")?;
        if n >= MAX_ENTRIES {
            bail!("archive has more than {MAX_ENTRIES} entries");
        }
        let path = entry.path()?.into_owned();
        if unsafe_path(&path) {
            bail!("archive entry {} escapes the extraction directory", path.display());
        }
        let target = dest.join(&path);
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            std::fs::create_dir_all(&target)?;
        } else if kind.is_file() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&target)?;
            let room = max_bytes.saturating_sub(total);
            let copied = std::io::copy(&mut (&mut entry).take(room + 1), &mut f)?;
            total += copied;
            if copied > room {
                bail!("archive extracts to more than {max_bytes} bytes");
            }
        } else if kind.is_pax_global_extensions() || kind.is_pax_local_extensions() || kind.is_gnu_longname() {
            continue;
        } else {
            bail!("archive entry {} is a {kind:?}; only files and directories are allowed", path.display());
        }
    }
    Ok(())
}

fn find_binary(dir: &Path, name: &str) -> Option<PathBuf> {
    let plain = |p: &Path| std::fs::symlink_metadata(p).is_ok_and(|m| m.is_file() && m.len() > 0);
    let top = dir.join(name);
    if plain(&top) {
        return Some(top);
    }
    let real_dir = |p: &Path| std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir());
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|d| real_dir(d))
        .map(|d| d.join(name))
        .find(|p| plain(p))
}

/// Download, verify and extract every archive in `release` under `staging`.
/// Nothing outside `staging` is touched. Any failure aborts the whole set.
pub fn stage(
    src: &Source,
    release: &Release,
    staging: &Path,
    out: &mut dyn Write,
) -> anyhow::Result<Vec<Staged>> {
    let unified = match &release.unified_checksum {
        Some(n) => Some(fetch_text(src, &src.asset_url(&release.tag, n), staging)?),
        None => None,
    };
    let mut staged = Vec::new();
    for art in &release.artifacts {
        writeln!(out, "Downloading {}...", art.name)?;
        let tarball = staging.join(&art.name);
        curl(src, &src.asset_url(&release.tag, &art.name), &tarball)?;
        let want = parse_sha256_file(
            &fetch_text(src, &src.asset_url(&release.tag, &art.checksum_name), staging)?,
            &art.name,
        )?;
        let got = sha256_file(&tarball).ok_or_else(|| anyhow!("cannot hash {}", tarball.display()))?;
        if got != want {
            bail!("checksum mismatch for {}: published {want}, downloaded {got}", art.name);
        }
        if let Some(s) = &release.signed {
            s.check(&art.name, &got)?;
        }
        if let Some(text) = &unified {
            match find_in_unified(text, &art.name) {
                Some(u) if u == want => {}
                Some(u) => bail!("sha256.sum disagrees for {}: {u} vs {want}", art.name),
                None => bail!("sha256.sum does not list {}", art.name),
            }
        }
        let signed = if release.signed.is_some() { " (matches the signed release)" } else { "" };
        writeln!(out, "  sha256 verified {}{signed}", &got[..16])?;
        let dir = staging.join(format!("x-{}", art.name));
        std::fs::create_dir_all(&dir)?;
        extract(&tarball, &dir, src.max_extract_bytes)
            .with_context(|| format!("refusing {}", art.name))?;
        for bin in &art.binaries {
            let path = find_binary(&dir, bin).ok_or_else(|| anyhow!("{bin} is not in {}", art.name))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
            }
            smoke_check(&path, &release.version)?;
            staged.push(Staged { name: bin.clone(), path });
        }
    }
    Ok(staged)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_file_formats() {
        let h = "a".repeat(64);
        assert_eq!(parse_sha256_file(&format!("{h}  x.tar.gz\n"), "x.tar.gz").unwrap(), h);
        assert_eq!(parse_sha256_file(&format!("{} *x.tar.gz", h.to_uppercase()), "x.tar.gz").unwrap(), h);
        assert_eq!(parse_sha256_file(&h, "x.tar.gz").unwrap(), h);
        assert!(parse_sha256_file(&format!("{h}  y.tar.gz"), "x.tar.gz").is_err());
        assert!(parse_sha256_file("abc  x.tar.gz", "x.tar.gz").is_err());
        assert!(parse_sha256_file("", "x.tar.gz").is_err());
    }

    #[test]
    fn archive_paths_must_stay_inside() {
        for bad in ["/etc/passwd", "../x", "a/../../x", ""] {
            assert!(unsafe_path(Path::new(bad)), "{bad}");
        }
        for ok in ["weaver-x/weaver", "weaver-x/..hidden", "./weaver"] {
            assert!(!unsafe_path(Path::new(ok)), "{ok}");
        }
    }

    #[test]
    fn plain_http_is_refused_unless_allowed() {
        let d = tempfile::tempdir().unwrap();
        let src = Source::github();
        let e = curl(&src, "http://127.0.0.1:9/x", &d.path().join("o")).unwrap_err();
        assert!(e.to_string().contains("non-https"), "{e}");
        assert!(curl(&src, "ftp://example.invalid/x", &d.path().join("o")).is_err());
    }

    #[test]
    fn manifest_tag_and_version_must_agree() {
        let m = |app: &str| {
            format!(
                r#"{{"announcement_tag":"v1.2.3","releases":[{{"app_name":"a","app_version":"{app}"}}],"artifacts":{{"a.tar.gz":{{"kind":"executable-zip","name":"a.tar.gz","target_triples":["t"],"assets":[{{"kind":"executable","name":"weaver"}}]}}}}}}"#
            )
        };
        assert!(parse_manifest(&m("1.2.3"), "t").is_ok());
        let e = parse_manifest(&m("1.2.4"), "t").unwrap_err();
        assert!(e.to_string().contains("does not match"), "{e}");
    }

    #[test]
    fn manifest_rejects_unsafe_names_and_missing_weaver() {
        let m = |name: &str, bins: &str| {
            format!(
                r#"{{"announcement_tag":"v1.2.3","artifacts":{{"{name}":{{"kind":"executable-zip","name":"{name}","target_triples":["t"],"assets":[{bins}]}}}}}}"#
            )
        };
        let ok = r#"{"kind":"executable","name":"weaver"}"#;
        assert!(parse_manifest(&m("a.tar.gz", ok), "t").is_ok());
        assert!(parse_manifest(&m("../a.tar.gz", ok), "t").is_err());
        assert!(parse_manifest(&m("a.tar.gz", r#"{"kind":"executable","name":"weft"}"#), "t").is_err());
        // Unknown binary names are dropped, never installed.
        let r = parse_manifest(&m("a.tar.gz", &format!(r#"{ok},{{"kind":"executable","name":"evil"}}"#)), "t").unwrap();
        assert_eq!(r.binaries(), vec!["weaver".to_string()]);
        assert!(parse_manifest(&m("a.tar.gz", ok), "other-triple").is_err());
    }
}
