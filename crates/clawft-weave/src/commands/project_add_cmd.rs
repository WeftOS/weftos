//! `weaver project add | up | down | status | remove`: the developer's one-step
//! flow for a project on their own machine. The work is in
//! `clawft_weave::project_add*`; this file fetches, calls it and prints.



use anyhow::{Context, bail};
use clap::Args;

use crate::project_add::{AddReport, Finish, finish, register};
use crate::project_add_receipt::receipts_dir;
use crate::project_add_route::RoutePlan;
use crate::project_add_run::{self as run, UpOutcome};
use crate::project_add_spec::{FetchPlan, SourceSpec, plan_add};
use crate::project_fetch_mesh::fetch_via_daemon;
use crate::project_install::{FetchReport, ProjectFetcher};
use crate::project_install_git::GitRemoteFetcher;
use crate::project_install_handlers::InstallEnv;
use crate::project_install_layout::{Rollback, check_free};

/// `weaver project add`.
#[derive(Args)]
pub struct AddOpts {
    /// What to add: `weftos://<mesh>/projects/<ULID>` (fetched over the mesh from
    /// the paired primary), or a git URL (`https://...`, `git@host:org/repo`).
    pub source: String,
    /// Where to put it (default `~/Projects/<slug>`). Must be inside your home directory.
    #[arg(long)]
    pub path: Option<String>,
    /// Name for the project: its directory, its route (`/<slug>`) and the handle
    /// for `up`/`down`/`status`/`remove`. Defaults to the git repository's name.
    #[arg(long)]
    pub slug: Option<String>,
    /// Branch to check out.
    #[arg(long)]
    pub branch: Option<String>,
    /// For a git URL: register the checkout as a working copy of this existing
    /// project ULID instead of as a project of its own.
    #[arg(long, value_name = "ULID")]
    pub project: Option<String>,
    /// Print the report as JSON.
    #[arg(long)]
    pub json: bool,
}

fn env() -> anyhow::Result<InstallEnv> {
    InstallEnv::from_process().context("cannot determine the home directory")
}

async fn reload_router() {
    if let Some(mut c) = clawft_rpc::DaemonClient::connect().await {
        let _ = c.call(clawft_rpc::Request::with_params(crate::router_rpc::RELOAD, serde_json::json!({}))).await;
    }
}

/// `weaver project add`.
pub async fn add(o: AddOpts) -> anyhow::Result<()> {
    let env = env()?;
    let mut plan = plan_add(&o.source, o.slug.as_deref(), o.path.as_deref(), o.branch.as_deref(), &env.home)?;
    if let Some(p) = &o.project {
        if !matches!(plan.spec, SourceSpec::Git(_)) {
            bail!("--project only applies to a git URL; a weftos:// name already carries its project");
        }
        clawft_types::project::validate_id(p).map_err(|_| anyhow::anyhow!("--project is not a ULID"))?;
        plan.request.project_ulid = p.clone();
    }
    check_free(&[], &plan.target).map_err(anyhow::Error::msg)?;
    let weft = env.weft_bin().map_err(anyhow::Error::msg)?;
    let rollback = Rollback::snapshot(&[], &plan.target);
    eprintln!("fetching {} into {} ({})", short(&o.source), plan.target.display(), match plan.fetch {
        FetchPlan::Mesh => "over the mesh, from the paired primary",
        FetchPlan::Git => "from its git remote, with your own git credentials",
    });
    let fetched: Result<FetchReport, String> = match (&plan.spec, plan.fetch) {
        (SourceSpec::Mesh { uri, mesh, ulid }, FetchPlan::Mesh) => fetch_via_daemon(uri, *mesh, ulid, &plan.request, &plan.target).await,
        (_, FetchPlan::Git) => GitRemoteFetcher::new().fetch(&plan.request, &plan.target).await,
        _ => Err("this source cannot be fetched".into()),
    };
    let report = match fetched {
        Ok(r) => r,
        Err(e) => {
            rollback.undo();
            bail!("{e}");
        }
    };
    let registered = async {
        std::fs::create_dir_all(&plan.target).map_err(|e| format!("cannot create {}: {e}", plan.target.display()))?;
        let ulid = Some(plan.request.project_ulid.as_str()).filter(|u| !u.is_empty());
        register(&env, &weft, &plan.target, ulid, Some(&plan.slug), &[]).await
    }
    .await;
    let (ulid, registered_as) = match registered {
        Ok(x) => x,
        Err(e) => {
            rollback.undo();
            bail!("{e}");
        }
    };
    let added = finish(&env, Finish { slug: &plan.slug, root: &plan.target, siblings: Vec::new(), ulid, registered_as, fetcher: report.fetcher, repos: report.repos });
    reload_router().await;
    if o.json {
        println!("{}", serde_json::to_string_pretty(&added)?);
    } else {
        print!("{}", render_add(&added, &report.warnings));
    }
    Ok(())
}

fn short(s: &str) -> String {
    s.chars().take(100).collect()
}

/// The add report as text.
pub fn render_add(a: &AddReport, warnings: &[String]) -> String {
    let mut o = format!("added {} at {}\n  fetched by {}; registered as a {} ({})\n", a.slug, a.root.display(), a.fetcher, match a.registered_as {
        crate::project_add::RegisteredAs::Workspace => "workspace",
        crate::project_add::RegisteredAs::Project => "project of its own",
    }, a.ulid);
    o += &match &a.run {
        crate::project_add_detect::RunKind::ProcessCompose { file } => format!("  runs:  process-compose ({}){}\n", file.display(), a.process_compose_port.map(|p| format!(", HTTP :{p}")).unwrap_or_default()),
        crate::project_add_detect::RunKind::PackageScripts { scripts } => format!("  runs:  not detected as process-compose; package.json scripts: {}\n", scripts.join(", ")),
        crate::project_add_detect::RunKind::Unknown => "  runs:  not detected\n".into(),
    };
    o += &match &a.route {
        RoutePlan::Overlay { prefix, port } => format!("  route: {prefix} -> 127.0.0.1:{port} (written to your route overlay; the project declares none)\n"),
        RoutePlan::Repo { prefixes } => format!("  route: {} (declared by the project's compose/ports.yaml)\n", prefixes.join(", ")),
        RoutePlan::Conflict { reason } => format!("  route: NOT installed: {reason}\n"),
        RoutePlan::None { reason } => format!("  route: none: {reason}\n"),
    };
    if !a.env_vars.is_empty() {
        let list: Vec<String> = a.env_vars.iter().map(|e| format!("{}{}", e.name, if e.set_here { "" } else { " (not set)" })).collect();
        o += &format!("  env:   {}\n", list.join(", "));
    }
    for n in a.notes.iter().chain(warnings) {
        o += &format!("  note:  {n}\n");
    }
    o += "next:\n";
    for n in &a.next {
        o += &format!("  - {n}\n");
    }
    o
}

fn target(env: &InstallEnv, slug: &str) -> anyhow::Result<run::RunTarget> {
    run::target(env, slug).map_err(anyhow::Error::msg)
}

/// `weaver project up <slug>`.
pub async fn up(slug: &str, bin: &str) -> anyhow::Result<()> {
    let env = env()?;
    let t = target(&env, slug)?;
    match run::up(&t, bin).await.map_err(anyhow::Error::msg)? {
        UpOutcome::Started => println!("{slug} is up (process-compose on :{})", t.port),
        UpOutcome::AlreadyRunning => println!("{slug} is already up (process-compose on :{})", t.port),
    }
    print!("{}", render_status(&run::status(&env, slug, "tailscale").await.map_err(anyhow::Error::msg)?));
    Ok(())
}

/// `weaver project down <slug>`.
pub async fn down(slug: &str, bin: &str) -> anyhow::Result<()> {
    let env = env()?;
    let t = target(&env, slug)?;
    let was = run::down(&t, bin).await.map_err(anyhow::Error::msg)?;
    println!("{slug} {}", if was { "stopped" } else { "was not running" });
    Ok(())
}

/// Text for a status report.
pub fn render_status(s: &run::StatusReport) -> String {
    let pc = &s.process_compose;
    let mut o = format!("{} ({})\n  root:     {}\n", s.slug, s.registered_as, s.root.display());
    o += &match pc["state"].as_str() {
        Some("ok") => format!("  running:  {}/{} processes (process-compose :{})\n", pc["running"], pc["total"], pc["port"]),
        _ => format!("  running:  no ({})\n", pc["detail"].as_str().unwrap_or("not reachable")),
    };
    if let Some(procs) = pc["processes"].as_array() {
        for p in procs {
            o += &format!("            {:<28} {:<10} restarts {}\n", p["name"].as_str().unwrap_or("-"), p["status"].as_str().unwrap_or("-"), p["restarts"]);
        }
    }
    if let (Some(prefix), Some(src)) = (&s.route_prefix, &s.route_source) {
        o += &format!("  route:    {prefix} ({src})\n");
    }
    if let Some(u) = &s.local_url {
        o += &format!("  local:    {u}\n");
    }
    if let Some(u) = &s.tailnet_url {
        o += &format!("  tailnet:  {u}\n");
    }
    for n in &s.notes {
        o += &format!("  note:     {n}\n");
    }
    o
}

/// `weaver project status [slug]`: one project, or every project added here.
pub async fn status(slug: Option<&str>, json: bool) -> anyhow::Result<()> {
    let env = env()?;
    let slugs: Vec<String> = match slug {
        Some(s) => vec![s.to_owned()],
        None => crate::project_add_receipt::list(&receipts_dir(&env.home)).into_iter().map(|r| r.slug).collect(),
    };
    if slugs.is_empty() {
        println!("no projects were added on this machine; try `weaver project add <weftos://... | git URL>`");
        return Ok(());
    }
    let mut all = Vec::new();
    for s in &slugs {
        all.push(run::status(&env, s, "tailscale").await.map_err(anyhow::Error::msg)?);
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&all)?);
    } else {
        for s in &all {
            print!("{}", render_status(s));
        }
    }
    Ok(())
}

/// `weaver project remove <slug>`.
pub async fn remove(slug: &str, purge: bool, force: bool, bin: &str) -> anyhow::Result<()> {
    let env = env()?;
    if let Ok(t) = run::target(&env, slug)
        && run::down(&t, bin).await.map_err(anyhow::Error::msg)?
    {
        println!("stopped {slug}");
    }
    let r = crate::project_add_remove::remove(&env, slug, purge, force).map_err(anyhow::Error::msg)?;
    reload_router().await;
    for u in &r.undone {
        println!("  undone: {u}");
    }
    for k in &r.kept {
        println!("  kept:   {k}");
    }
    Ok(())
}

/// Default `process-compose` binary.
pub fn pc_bin() -> String {
    std::env::var("WEFTOS_PROCESS_COMPOSE").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "process-compose".into())
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_add::RegisteredAs;
    use crate::project_add_detect::{EnvVar, RunKind};

    fn report(route: RoutePlan) -> AddReport {
        AddReport {
            slug: "demo".into(),
            root: "/h/Projects/demo".into(),
            ulid: "01K6ZQ8N3T4V5W6X7Y8Z9A0B1C".into(),
            registered_as: RegisteredAs::Workspace,
            fetcher: "mesh".into(),
            repos: vec![],
            run: RunKind::ProcessCompose { file: "process-compose.yaml".into() },
            process_compose_port: Some(18110),
            route,
            env_vars: vec![EnvVar { name: "A_KEY".into(), file: ".env.example".into(), set_here: false }, EnvVar { name: "PORT".into(), file: ".env.example".into(), set_here: true }],
            notes: vec!["a note".into()],
            next: vec!["weaver project up demo".into()],
        }
    }

    #[test]
    fn add_report_says_what_was_found_and_never_a_value() {
        let t = render_add(&report(RoutePlan::Overlay { prefix: "/demo".into(), port: 18120 }), &["warn".into()]);
        for want in ["added demo at /h/Projects/demo", "registered as a workspace", "HTTP :18110", "/demo -> 127.0.0.1:18120", "A_KEY (not set), PORT", "note:  a note", "note:  warn", "- weaver project up demo"] {
            assert!(t.contains(want), "{want}\n{t}");
        }
    }

    #[test]
    fn a_route_conflict_is_loud_in_the_report() {
        let t = render_add(&report(RoutePlan::Conflict { reason: "prefix /demo is already routed by project other".into() }), &[]);
        assert!(t.contains("route: NOT installed: prefix /demo is already routed"), "{t}");
    }

    #[test]
    fn status_text_shows_processes_route_and_both_urls() {
        let s = run::StatusReport {
            slug: "demo".into(),
            root: "/h/p".into(),
            ulid: "U".into(),
            registered_as: "workspace".into(),
            process_compose: serde_json::json!({"state":"ok","port":18110,"running":1,"total":2,"processes":[{"name":"web","status":"Running","restarts":0}]}),
            route_prefix: Some("/demo".into()),
            route_source: Some("dashboard".into()),
            local_url: Some("http://127.0.0.1:18120/demo/".into()),
            tailnet_url: Some("https://m.t.ts.net/demo/".into()),
            notes: vec![],
        };
        let t = render_status(&s);
        for want in ["1/2 processes", "web", "/demo (dashboard)", "local:    http://127.0.0.1:18120/demo/", "tailnet:  https://m.t.ts.net/demo/"] {
            assert!(t.contains(want), "{want}\n{t}");
        }
    }
}
