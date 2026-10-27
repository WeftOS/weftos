//! `weaver cog ...` and `weaver workload catalog --kind cog` (ADR-105).
//!
//! Project cog sources, licences, search, info and install, plus the cog
//! catalog across sources. The logic lives in the Cogs release's `cog-sources`; this file
//! is argument parsing and output. Everything here runs locally: no daemon is
//! needed, and nothing is sent to a daemon or a chain yet (ADR-105 section 9).
//!
//! Scope: edits go to the project (`<root>/.weftos/cog-sources.toml`, found by
//! walking up from the current directory) unless `--user` selects the user
//! default list (`~/.weftos/cog-sources.toml`). Reads use the merged view.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context};
use chrono::Utc;
use clap::{Args, Subcommand, ValueEnum};
use cog_sources::catalog::{build_rows, render_table, Expectations};
use cog_sources::config::{
    load_effective, project_sources_path, user_sources_path, CogLicence, CogSource, EffectiveSources, LicensedCogs,
    SourceKind, SourcesFile, COGNITUM_DEFAULT_URL,
};
use cog_sources::fetch::HttpReader;
use cog_sources::RevokedKeys;
use cog_sources::{fetch_verified, install_into_host, load_all, parse_ref, resolve, FetchCtx};

/// `weaver cog` arguments.
#[derive(Args)]
pub struct CogArgs {
    /// Subcommand.
    #[command(subcommand)]
    pub command: CogCommand,
}

/// `weaver cog` subcommands.
#[derive(Subcommand)]
pub enum CogCommand {
    /// Manage where this project gets cogs (WeftOS, Cognitum, private).
    Source {
        /// Source subcommand.
        #[command(subcommand)]
        command: SourceCommand,
    },
    /// Declare a Cognitum licence entitlement for this project.
    Licence {
        /// Licence subcommand.
        #[command(subcommand)]
        command: LicenceCommand,
    },
    /// Search the cogs of every enabled source.
    Search {
        /// Substring of the id, name, category or description.
        query: Option<String>,
        /// Only this source.
        #[arg(long)]
        source: Option<String>,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show one cog (`cog-id` or `source:cog-id`): source, trust, policy.
    Info {
        /// `cog-id` or `source:cog-id`.
        reference: String,
        /// Print JSON.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        inputs: CatalogInputs,
    },
    /// Verify and install a cog into the local cog-host root.
    Install {
        /// `cog-id` or `source:cog-id`.
        reference: String,
        /// `arm` or `arm64`.
        #[arg(long, default_value = "arm")]
        arch: String,
        /// Start it (the cog-host supervisor keeps it running).
        #[arg(long)]
        enable: bool,
        /// CLI arg for the cog (repeat), e.g. `--arg=--interval --arg=1`.
        #[arg(long = "arg", allow_hyphen_values = true)]
        args: Vec<String>,
        /// cog-host root (default `$WEFTOS_COG_ROOT` or `~/.weftos/cogs`).
        #[arg(long)]
        root: Option<PathBuf>,
        /// Allow `--enable` for a bare id that resolved to a source defined only in this
        /// project's cog-sources.toml (a cloned repo can define its own sources).
        #[arg(long)]
        confirm_project_source: bool,
    },
    /// Check out a Cognitum cog through the mesh's Seed steward, approve its
    /// hashes, or list grants and approvals (ADR-106; needs the daemon).
    #[cfg(all(feature = "placement", unix))]
    Checkout(super::cog_checkout_cmd::CheckoutArgs),
}

/// `weaver cog source` subcommands.
#[derive(Subcommand)]
pub enum SourceCommand {
    /// Add a source.
    Add {
        /// Name, used as the `name:cog-id` namespace.
        name: String,
        /// `weftos`, `cognitum` or `private`.
        #[arg(long, value_enum)]
        kind: KindArg,
        /// registry.json / app-registry.json URL, file path or repo directory
        /// (a `cognitum` source defaults to Cognitum's public registry).
        #[arg(long)]
        url: Option<String>,
        /// Ed25519 public key (64 hex) allowed to sign this source (repeat).
        /// Required for `private`.
        #[arg(long = "key")]
        keys: Vec<String>,
        /// Higher wins when a bare id is in several sources.
        #[arg(long, default_value_t = 0)]
        priority: i32,
        /// Edit the user default list instead of the project.
        #[arg(long)]
        user: bool,
        /// Development only: let a cognitum source use http:// or a local path.
        #[arg(long)]
        allow_insecure: bool,
    },
    /// List the effective sources (project overlaid on user defaults).
    List {
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Remove a source.
    Remove {
        /// Source name.
        name: String,
        /// Edit the user default list.
        #[arg(long)]
        user: bool,
    },
    /// Enable a source.
    Enable {
        /// Source name.
        name: String,
        /// Edit the user default list.
        #[arg(long)]
        user: bool,
    },
    /// Disable a source (kept in the file, skipped by search and install).
    Disable {
        /// Source name.
        name: String,
        /// Edit the user default list.
        #[arg(long)]
        user: bool,
    },
}

/// `weaver cog licence` subcommands.
#[derive(Subcommand)]
pub enum LicenceCommand {
    /// Record a licence. A declaration, not a payment check.
    Add {
        /// Source the licence is for.
        #[arg(long, default_value = "cognitum")]
        source: String,
        /// Licensed cog id (repeat), or use `--all`.
        #[arg(long = "cog", conflicts_with = "all")]
        cogs: Vec<String>,
        /// Covers every cog of the source.
        #[arg(long)]
        all: bool,
        /// Licensee account label.
        #[arg(long, default_value = "")]
        account: String,
        /// `YYYY-MM-DD` or RFC 3339.
        #[arg(long)]
        expires: Option<String>,
        /// Edit the user default list.
        #[arg(long)]
        user: bool,
    },
    /// List the effective licences.
    List,
    /// Remove every licence for a source.
    Remove {
        /// Source name.
        source: String,
        /// Edit the user default list.
        #[arg(long)]
        user: bool,
    },
}

/// Source kind on the command line.
#[derive(Clone, Copy, ValueEnum)]
pub enum KindArg {
    /// WeftOS / WeaveLogic signed registry.
    Weftos,
    /// Cognitum app registry (licence required to install).
    Cognitum,
    /// The project's own signed registry.
    Private,
}

impl From<KindArg> for SourceKind {
    fn from(k: KindArg) -> Self {
        match k {
            KindArg::Weftos => SourceKind::Weftos,
            KindArg::Cognitum => SourceKind::Cognitum,
            KindArg::Private => SourceKind::Private,
        }
    }
}

/// Optional local inputs that enrich the catalog.
#[derive(Args, Clone, Default)]
pub struct CatalogInputs {
    /// Conformance baseline (default `scripts/cogs/expectations.json` if it exists here).
    #[arg(long)]
    pub baseline: Option<PathBuf>,
    /// Directory of cog source dirs holding `<id>/cog.toml` (resources, secrets, hardware).
    #[arg(long)]
    pub cog_toml_dir: Option<PathBuf>,
}

/// `weaver workload catalog`.
#[derive(Args)]
pub struct CatalogArgs {
    /// Workload kind to list (only `cog` today).
    #[arg(long, default_value = "cog")]
    pub kind: String,
    /// Only this source.
    #[arg(long)]
    pub source: Option<String>,
    /// Print JSON.
    #[arg(long)]
    pub json: bool,
    #[command(flatten)]
    pub inputs: CatalogInputs,
}

struct Roots {
    user_file: Option<PathBuf>,
    project_file: Option<PathBuf>,
}

fn roots() -> Roots {
    let home = clawft_types::runtime_paths::home_dir();
    let cwd = std::env::current_dir().ok();
    let project = cwd
        .as_deref()
        .and_then(|c| clawft_types::project::find_project_toml(c, home.as_deref()));
    Roots {
        user_file: home.as_deref().map(user_sources_path),
        project_file: project.as_deref().map(project_sources_path),
    }
}

impl Roots {
    fn effective(&self) -> anyhow::Result<EffectiveSources> {
        let e = load_effective(self.user_file.as_deref(), self.project_file.as_deref())?;
        for w in &e.warnings {
            eprintln!("warning: {w}");
        }
        Ok(e)
    }

    /// The file an edit goes to.
    fn target(&self, user: bool) -> anyhow::Result<&Path> {
        let p = if user { self.user_file.as_deref() } else { self.project_file.as_deref() };
        p.ok_or_else(|| {
            if user {
                anyhow!("cannot determine the home directory")
            } else {
                anyhow!("not inside a WeftOS project (no .weftos/project.toml above here); run `weft project init`, or pass --user to edit the user defaults")
            }
        })
    }

    fn edit(&self, user: bool, f: impl FnOnce(&mut SourcesFile) -> cog_sources::Result<()>) -> anyhow::Result<PathBuf> {
        let path = self.target(user)?.to_path_buf();
        let mut file = SourcesFile::load(&path)?;
        f(&mut file)?;
        file.save(&path)?;
        Ok(path)
    }
}

/// The compiled-in WeftOS package signer keys (extra trusted keys of `weftos` sources).
/// `workload_pkg` is built only with `ecc` + `exochain`; without them no signer is compiled in.
/// That narrows what a `weftos` source is trusted with, so say so (once per process) instead of
/// silently trusting less. This is a warning, not a compile error: a weaver without those
/// features is a supported build, and a source's own pinned keys still apply.
#[cfg(not(all(feature = "ecc", feature = "exochain")))]
fn weftos_signer_keys() -> Vec<String> {
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        eprintln!(
            "warning: this weaver was built without the ecc and exochain features, so no WeftOS \
             signer keys are compiled in; `weftos` sources are trusted only with the keys pinned \
             in their own configuration. Use a default-feature build for the full WeftOS trust set."
        );
    });
    Vec::new()
}

/// The compiled-in WeftOS package signer keys (extra trusted keys of `weftos` sources).
#[cfg(all(feature = "ecc", feature = "exochain"))]
fn weftos_signer_keys() -> Vec<String> {
    clawft_kernel::workload_pkg::trust::WEFTOS_PINNED_SIGNERS
        .iter()
        .map(|(_, k)| k.to_string())
        .collect()
}

/// Signer keys the operator revoked, from the kernel `RevocationList` that sits in the runtime dir
/// (`revoked_hosts.json` and its sibling `revoked_subjects.json`). Fail-closed: an unreadable
/// subjects file refuses the install rather than reading as "nothing revoked".
fn revoked_signer_keys(host_ban_file: &Path) -> anyhow::Result<RevokedKeys> {
    use clawft_kernel::revocation::{RevocationKind, RevocationList};
    let list = RevocationList::load(host_ban_file.to_path_buf());
    if let Some(e) = list.subjects_error() {
        bail!("cannot read the signer revocation list, refusing to install: {e}");
    }
    Ok(RevokedKeys::from_keys(
        list.list_subjects(Some(RevocationKind::SignerKey)).into_iter().map(|s| s.id),
    ))
}

fn default_baseline(inputs: &CatalogInputs) -> anyhow::Result<Option<Expectations>> {
    let p = inputs.baseline.clone().or_else(|| {
        let d = PathBuf::from("scripts/cogs/expectations.json");
        d.is_file().then_some(d)
    });
    let Some(p) = p else { return Ok(None) };
    let bytes = std::fs::read(&p).with_context(|| format!("read baseline {}", p.display()))?;
    Ok(Some(Expectations::parse(&bytes)?))
}

fn print_failures(failures: &[(String, cog_sources::SourceError)]) {
    for (name, e) in failures {
        eprintln!("warning: source '{name}' not loaded: {e}");
    }
}

/// Run `weaver cog ...`.
pub async fn run(args: CogArgs) -> anyhow::Result<()> {
    #[cfg(all(feature = "placement", unix))]
    if let CogCommand::Checkout(a) = args.command {
        return super::cog_checkout_cmd::run(a).await;
    }
    // The HTTP reader is blocking; keep it off the async runtime threads.
    tokio::task::spawn_blocking(move || run_blocking(args)).await?.map_err(coded)
}

/// Prefix a source error with its stable code: `[cog_unlicensed] ...`.
pub fn coded(e: anyhow::Error) -> anyhow::Error {
    match e.downcast_ref::<cog_sources::SourceError>() {
        Some(s) => anyhow!("[{}] {}", s.code(), s),
        None => e,
    }
}

/// Run `weaver workload catalog`.
pub fn run_catalog(a: CatalogArgs) -> anyhow::Result<()> {
    run_catalog_inner(a).map_err(coded)
}

fn run_catalog_inner(a: CatalogArgs) -> anyhow::Result<()> {
    if a.kind != "cog" {
        bail!("only `--kind cog` has a catalog today (got {:?})", a.kind);
    }
    let eff = roots().effective()?;
    let loaded = load_all(&eff, &HttpReader::new());
    print_failures(&loaded.failures);
    let base = default_baseline(&a.inputs)?;
    let mut rows = build_rows(&loaded.loaded, a.inputs.cog_toml_dir.as_deref(), base.as_ref(), &eff.licences, Utc::now());
    if let Some(s) = &a.source {
        rows.retain(|r| &r.source == s);
    }
    if a.json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else {
        print!("{}", render_table(&rows));
        if let Some(b) = base {
            let s = b.summary();
            println!(
                "\nconformance baseline: {} clean / {} need --interval / {} need seed or extra setup / {} no build",
                s.clean, s.needs_interval, s.needs_extra_cli, s.no_build
            );
        }
    }
    Ok(())
}

fn run_blocking(args: CogArgs) -> anyhow::Result<()> {
    let roots = roots();
    match args.command {
        CogCommand::Source { command } => source(&roots, command),
        CogCommand::Licence { command } => licence(&roots, command),
        #[cfg(all(feature = "placement", unix))]
        CogCommand::Checkout(_) => unreachable!("checkout is served by the daemon, before this"),
        CogCommand::Search { query, source, json } => {
            let eff = roots.effective()?;
            let loaded = load_all(&eff, &HttpReader::new());
            print_failures(&loaded.failures);
            let mut rows = build_rows(&loaded.loaded, None, None, &eff.licences, Utc::now());
            if let Some(s) = source {
                rows.retain(|r| r.source == s);
            }
            if let Some(q) = query.map(|q| q.to_lowercase()) {
                rows.retain(|r| {
                    [&r.id, &r.category, &r.description].iter().any(|f| f.to_lowercase().contains(&q))
                });
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                print!("{}", render_table(&rows));
            }
            Ok(())
        }
        CogCommand::Info { reference, json, inputs } => {
            let eff = roots.effective()?;
            let loaded = load_all(&eff, &HttpReader::new());
            print_failures(&loaded.failures);
            let r = resolve(&eff, &loaded.loaded, &parse_ref(&reference)?)?;
            let base = default_baseline(&inputs)?;
            let rows = build_rows(std::slice::from_ref(r.loaded), inputs.cog_toml_dir.as_deref(), base.as_ref(), &eff.licences, Utc::now());
            let row = rows.into_iter().find(|x| x.id == r.cog.id).ok_or_else(|| anyhow!("cog vanished"))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&row)?);
            } else {
                println!("{}  v{}  [{} source, {:?}]", row.reference, row.version, row.kind, row.access);
                println!("  {}", row.description);
                println!("  arches: {}   hardware: {}", row.arches.join(","), row.hardware_requirement.join(","));
                println!("  run: {:?}{}", row.run.mode, row.run.interval.map(|n| format!(" (--interval {n})")).unwrap_or_default());
                println!("  placement: needs {}", row.policy.requires.join(", "));
                println!(
                    "  placement: min trust {}, signed package required: {}, emulation: {}",
                    row.policy.min_trust_tier,
                    row.policy.signed_package_required,
                    if row.policy.allow_emulated { "allowed" } else { "operator opt-in only" }
                );
            }
            Ok(())
        }
        CogCommand::Install { reference, arch, enable, args, root, confirm_project_source } => {
            let eff = roots.effective()?;
            let reader = HttpReader::new();
            let loaded = load_all(&eff, &reader);
            print_failures(&loaded.failures);
            let cref = parse_ref(&reference)?;
            let r = resolve(&eff, &loaded.loaded, &cref)?;
            let keys = weftos_signer_keys();
            // Say what will be fetched, and from where, before anything is downloaded.
            let origin = if eff.from_project(&r.loaded.source.name) { "this project's cog-sources.toml" } else { "your user sources" };
            eprintln!(
                "resolved {} -> {} [{} source from {origin}]; {} key(s) can sign it",
                reference,
                r.namespaced(),
                r.loaded.source.kind.label(),
                r.loaded.source.effective_keys(&keys).len()
            );
            cog_sources::resolve::enable_guard(&eff, &cref, &r, enable, confirm_project_source)?;
            let revoked = revoked_signer_keys(&clawft_types::runtime_paths::RuntimePaths::resolve().revoked_hosts())?;
            let ctx = FetchCtx { reader: &reader, licences: &eff.licences, now: Utc::now(), extra_weftos_keys: &keys, revoked: &revoked };
            let fetched = fetch_verified(r.loaded, &r.cog.id, &arch, &ctx)?;
            let root = root.unwrap_or_else(cog_sources::default_host_root);
            let rec = install_into_host(&root, &fetched, enable, &args)?;
            let p = &fetched.provenance;
            println!("installed {} v{} from {} ({}) into {}", rec.id, rec.version, p.source, p.trust, root.display());
            if let Some(k) = &p.signer_key_id {
                println!("  signed by {k}");
            }
            if !p.placement_eligible {
                println!("  not eligible for governed placement until an operator hashes and signs it (weaver workload pack)");
            }
            Ok(())
        }
    }
}

fn source(roots: &Roots, cmd: SourceCommand) -> anyhow::Result<()> {
    match cmd {
        SourceCommand::Add { name, kind, url, keys, priority, user, allow_insecure } => {
            if allow_insecure && !user {
                bail!("--allow-insecure is honoured only from the user file; add it with --user (a project file's allow_insecure is ignored)");
            }
            let kind: SourceKind = kind.into();
            let url = match (url, kind) {
                (Some(u), _) => u,
                (None, SourceKind::Cognitum) => COGNITUM_DEFAULT_URL.to_string(),
                (None, _) => bail!("--url is required for a {} source", kind.label()),
            };
            let p = roots.edit(user, |f| {
                f.add_source(CogSource { name: name.clone(), kind, url, pinned_keys: keys, priority, enabled: true, allow_insecure })
            })?;
            println!("added source '{name}' to {}", p.display());
            if kind == SourceKind::Cognitum {
                println!("note: Cognitum cogs are listable now; installing one needs `weaver cog licence add`");
            }
        }
        SourceCommand::List { json } => {
            let eff = roots.effective()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&eff.sources)?);
                return Ok(());
            }
            if eff.sources.is_empty() {
                println!("No cog sources. Add one: weaver cog source add weftos --kind weftos --url <registry url or dir>");
            }
            for s in &eff.sources {
                println!(
                    "{:<16} {:<9} prio {:<4} {:<9} keys {}  {}",
                    s.name,
                    s.kind.label(),
                    s.priority,
                    if s.enabled { "enabled" } else { "disabled" },
                    s.effective_keys(&weftos_signer_keys()).len(),
                    s.url
                );
            }
        }
        SourceCommand::Remove { name, user } => {
            let p = roots.edit(user, |f| f.remove_source(&name))?;
            println!("removed source '{name}' from {}", p.display());
        }
        SourceCommand::Enable { name, user } => {
            roots.edit(user, |f| f.set_enabled(&name, true))?;
            println!("enabled '{name}'");
        }
        SourceCommand::Disable { name, user } => {
            roots.edit(user, |f| f.set_enabled(&name, false))?;
            println!("disabled '{name}'");
        }
    }
    Ok(())
}

fn licence(roots: &Roots, cmd: LicenceCommand) -> anyhow::Result<()> {
    match cmd {
        LicenceCommand::Add { source, cogs, all, account, expires, user } => {
            let cogs = match (all, cogs.is_empty()) {
                (true, _) => LicensedCogs::All("all".into()),
                (false, false) => LicensedCogs::List(cogs),
                (false, true) => bail!("name the licensed cogs with --cog <id> (repeat), or pass --all"),
            };
            let p = roots.edit(user, |f| f.add_licence(CogLicence { source: source.clone(), cogs, account, expires }))?;
            println!("recorded licence for '{source}' in {}", p.display());
            println!("note: this is a declaration. It is checked for presence, coverage and expiry; it is not proof of payment.");
        }
        LicenceCommand::List => {
            let eff = roots.effective()?;
            if eff.licences.is_empty() {
                println!("No licences recorded.");
            }
            for l in &eff.licences {
                let cogs = match &l.cogs {
                    LicensedCogs::All(_) => "all".to_string(),
                    LicensedCogs::List(v) => v.join(","),
                };
                println!("{:<12} cogs {:<30} account {:<16} expires {}", l.source, cogs, l.account, l.expires.as_deref().unwrap_or("never"));
            }
        }
        LicenceCommand::Remove { source, user } => {
            roots.edit(user, |f| f.remove_licences(&source))?;
            println!("removed licences for '{source}'");
        }
    }
    Ok(())
}

#[cfg(test)]
mod revocation_tests {
    use super::*;
    use clawft_kernel::revocation::{RevocationKind, RevocationList};

    const KEY: &str = "0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f";

    #[test]
    fn a_kernel_signer_revocation_reaches_the_installer() {
        let dir = tempfile::tempdir().unwrap();
        let host_file = dir.path().join("revoked_hosts.json");
        assert!(revoked_signer_keys(&host_file).unwrap().is_empty());

        let list = RevocationList::load(host_file.clone());
        assert!(list.revoke_subject(RevocationKind::SignerKey, KEY, "leaked").unwrap());
        list.revoke_subject(RevocationKind::Package, "some-cog", "bad").unwrap();
        let keys = revoked_signer_keys(&host_file).unwrap();
        assert_eq!(keys.len(), 1);
        assert!(keys.contains(&KEY.to_uppercase()));
        // The standalone reader in cog-repo agrees with the kernel's file.
        let standalone = cog_repo_reads(&list.subjects_path());
        assert!(standalone);
    }

    fn cog_repo_reads(path: &Path) -> bool {
        RevokedKeys::load(path).unwrap().contains(KEY)
    }

    #[test]
    fn an_unreadable_revocation_list_refuses_the_install() {
        let dir = tempfile::tempdir().unwrap();
        let host_file = dir.path().join("revoked_hosts.json");
        std::fs::write(dir.path().join("revoked_subjects.json"), "{not json").unwrap();
        let err = revoked_signer_keys(&host_file).unwrap_err().to_string();
        assert!(err.contains("refusing to install"), "{err}");
    }
}
