//! Runtime probes: native, Docker-compatible engines (OrbStack, Docker
//! Desktop, plain engine), Podman, Apple `container`, and the inference
//! servers `llama-server`, `mlx_lm` and `ollama`.
//!
//! Emulated arches are only advertised when a probe sees an emulator:
//! the host's binfmt_misc on Linux, Rosetta on macOS, and for a VM-backed
//! engine the binfmt handlers inside the engine VM (listed from a local
//! image, never pulled) unioned with the BuildKit platform list.

use clawft_types::placement::{CapabilityState, Provenance};

use super::host::ProbeHost;
use super::linux::{emulated_from_listing, host_binfmt_emulated};
use super::probe::{Collected, ProbeConfig, cap, normalize_arch, str_list};

const ROSETTA: &str = "/Library/Apple/usr/share/rosetta/rosetta";

/// Shell snippet run inside the engine VM to list binfmt handlers.
pub const BINFMT_LIST: &str = "mount -t binfmt_misc binfmt_misc /proc/sys/fs/binfmt_misc 2>/dev/null; \
cd /proc/sys/fs/binfmt_misc && for f in *; do [ \"$f\" = register ] || [ \"$f\" = status ] || \
echo \"$f $(head -n1 \"$f\")\"; done";

fn host_emulated(host: &dyn ProbeHost, arch: &str) -> Vec<String> {
    match host.os().as_str() {
        "linux" => host_binfmt_emulated(host, arch),
        "macos" if host.exists(ROSETTA) && arch != "x86_64" => vec!["x86_64".into()],
        _ => Vec::new(),
    }
}

/// Run the runtime probes.
pub fn probe(host: &dyn ProbeHost, arch: &str, cfg: &ProbeConfig, c: &mut Collected) {
    let host_emu = host_emulated(host, arch);
    if let Some(n) = cap("runtime.native", Provenance::Probed) {
        c.push(
            n.with_attr("arches_native", str_list(&[arch]))
                .with_attr("arches_emulated", str_list(&host_emu)),
        );
    }
    docker(host, arch, &host_emu, cfg, c);
    podman(host, arch, &host_emu, c);
    if host.os() == "macos" {
        apple_container(host, arch, &host_emu, c);
    }
    inference(host, arch, c);
}

/// Docker engine variant from `docker info` `OperatingSystem`.
pub fn docker_variant(operating_system: &str) -> &'static str {
    if operating_system.contains("OrbStack") {
        "orbstack"
    } else if operating_system.contains("Docker Desktop") {
        "desktop"
    } else {
        "engine"
    }
}

/// Arches from a `docker buildx inspect` `Platforms:` line.
pub fn buildx_arches(out: &str) -> Vec<String> {
    let Some(line) = out
        .lines()
        .find(|l| l.trim_start().starts_with("Platforms:"))
    else {
        return Vec::new();
    };
    let mut v: Vec<String> = line
        .split_once(':')
        .map(|(_, r)| r)
        .unwrap_or_default()
        .split(',')
        .filter_map(|p| p.trim().strip_prefix("linux/"))
        .map(|p| match p {
            "arm/v7" | "arm/v6" => normalize_arch(p),
            other => normalize_arch(other.split('/').next().unwrap_or(other)),
        })
        .filter(|a| !a.is_empty())
        .collect();
    v.sort();
    v.dedup();
    v
}

fn docker(
    host: &dyn ProbeHost,
    arch: &str,
    host_emu: &[String],
    cfg: &ProbeConfig,
    c: &mut Collected,
) {
    if host.which("docker").is_none() {
        return;
    }
    let info = host
        .run("docker", &["info", "--format", "{{json .}}"])
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s.trim()).ok());
    let Some(info) = info.filter(|v| v["ServerVersion"].is_string()) else {
        if let Some(d) = cap("runtime.container.docker", Provenance::Probed) {
            c.push(d.with_state(CapabilityState::Degraded));
        }
        c.note(
            "docker",
            "docker CLI present but the engine did not answer `docker info`; degraded",
        );
        return;
    };
    let os = info["OperatingSystem"].as_str().unwrap_or_default();
    let variant = docker_variant(os);
    let native = info["Architecture"]
        .as_str()
        .map(normalize_arch)
        .unwrap_or_else(|| arch.into());
    let local_linux_engine = host.os() == "linux" && variant == "engine";
    let (mut emulated, source) = if local_linux_engine {
        (
            host_emu.to_vec(),
            "host binfmt_misc (shared kernel)".to_string(),
        )
    } else {
        vm_emulation(host, &native, cfg)
    };
    emulated.retain(|a| *a != native);
    let Some(mut d) = cap("runtime.container.docker", Provenance::Probed) else {
        return;
    };
    d = d
        .with_attr("variant", variant)
        .with_attr("arches_native", str_list(&[native.as_str()]))
        .with_attr("arches_emulated", str_list(&emulated));
    if let Some(v) = info["ServerVersion"].as_str() {
        d = d.with_attr("version", v);
    }
    c.push(d);
    // The CLI answers for whichever engine the daemon's own HOME and docker
    // context select, which may not be the one an interactive shell uses.
    let context = host
        .run("docker", &["context", "show"])
        .map(|s| public_context(s.trim()))
        .unwrap_or_else(|| "unknown".into());
    c.note(
        "docker",
        format!(
            "docker info (variant {variant}); emulation from {source}; \
             probed as the daemon's user with docker context {context}"
        ),
    );
}

/// Docker context names that are the same on every install. A context a user
/// named themselves (a host, a project, a person) is reported as `custom`.
const KNOWN_CONTEXTS: &[&str] = &["default", "desktop-linux", "orbstack", "colima", "rancher-desktop"];

fn public_context(name: &str) -> String {
    if KNOWN_CONTEXTS.contains(&name) {
        name.to_string()
    } else if name.is_empty() {
        "unknown".into()
    } else {
        "custom".into()
    }
}

fn vm_emulation(host: &dyn ProbeHost, native: &str, cfg: &ProbeConfig) -> (Vec<String>, String) {
    let mut arches = host
        .run("docker", &["buildx", "inspect"])
        .map(|o| buildx_arches(&o))
        .unwrap_or_default();
    let mut sources = vec!["buildx platforms"];
    let image = cfg.docker_probe_image.as_deref().filter(|img| {
        super::probe::valid_image_ref(img)
            && host
                .run("docker", &["image", "inspect", "--format", "{{.Id}}", img])
                .is_some()
    });
    match image {
        Some(img) => {
            let cached = cfg.emulation_cache.as_ref().and_then(|c| c.get(img));
            let listing = cached.or_else(|| {
                let l = host.run(
                    "docker",
                    &[
                        "run",
                        "--rm",
                        "--privileged",
                        "--pull=never",
                        "--network=none",
                        "--entrypoint",
                        "/bin/sh",
                        img,
                        "-c",
                        BINFMT_LIST,
                    ],
                );
                if let (Some(l), Some(c)) = (&l, &cfg.emulation_cache) {
                    c.put(img, l);
                }
                l
            });
            if let Some(l) = listing {
                arches.extend(emulated_from_listing(&l, native));
                sources.push("engine VM binfmt_misc");
            }
        }
        None => sources.push("(engine VM binfmt not listed: probe image not present locally)"),
    }
    arches.retain(|a| a != native);
    arches.sort();
    arches.dedup();
    (arches, sources.join(" + "))
}

fn podman(host: &dyn ProbeHost, arch: &str, host_emu: &[String], c: &mut Collected) {
    if host.which("podman").is_none() {
        return;
    }
    let info = host
        .run("podman", &["info", "--format", "json"])
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
    let Some(mut p) = cap("runtime.container.podman", Provenance::Probed) else {
        return;
    };
    let Some(info) = info else {
        c.push(p.with_state(CapabilityState::Degraded));
        c.note(
            "podman",
            "podman present but `podman info` failed; degraded",
        );
        return;
    };
    let native = info["host"]["arch"]
        .as_str()
        .map(normalize_arch)
        .unwrap_or_else(|| arch.into());
    let emulated: Vec<String> = if host.os() == "linux" {
        host_emu.to_vec()
    } else {
        Vec::new()
    };
    p = p
        .with_attr("arches_native", str_list(&[native.as_str()]))
        .with_attr("arches_emulated", str_list(&emulated));
    if let Some(v) = info["version"]["Version"].as_str() {
        p = p.with_attr("version", v);
    }
    c.push(p);
}

fn apple_container(host: &dyn ProbeHost, arch: &str, host_emu: &[String], c: &mut Collected) {
    let Some(out) = host.run("container", &["--version"]) else {
        return;
    };
    let Some(mut a) = cap("runtime.container.apple", Provenance::Probed) else {
        return;
    };
    if let Some(v) = out
        .split("version")
        .nth(1)
        .and_then(|r| r.split_whitespace().next())
    {
        a = a.with_attr("version", v);
    }
    a = a
        .with_attr("arches_native", str_list(&[arch]))
        .with_attr("arches_emulated", str_list(host_emu));
    if host.run("container", &["system", "status"]).is_none() {
        a = a.with_state(CapabilityState::Degraded);
        c.note(
            "container",
            "Apple container CLI present but its system service is not running; degraded",
        );
    }
    c.push(a);
}

fn inference(host: &dyn ProbeHost, arch: &str, c: &mut Collected) {
    if host.which("llama-server").is_some()
        && let Some(mut l) = cap("runtime.infer.llamacpp", Provenance::Probed)
    {
        let ver = host.run_all("llama-server", &["--version"]).and_then(|o| {
            o.lines()
                .find_map(|l| l.trim().strip_prefix("version:"))
                .and_then(|v| v.split_whitespace().next().map(str::to_string))
        });
        if let Some(v) = ver {
            l = l.with_attr("version", v.as_str());
        }
        c.push(
            l.with_attr("formats", str_list(&["gguf"]))
                .with_attr("arches_native", str_list(&[arch])),
        );
    }
    if (host.which("mlx_lm.server").is_some() || host.which("mlx_lm.generate").is_some())
        && let Some(m) = cap("runtime.infer.mlx-lm", Provenance::Probed)
    {
        c.push(
            m.with_attr("formats", str_list(&["mlx"]))
                .with_attr("arches_native", str_list(&[arch])),
        );
        c.note(
            "runtime.infer.mlx-lm",
            "mlx_lm entry point on PATH; version not probed (python import is slow)",
        );
    }
    geniex(host, arch, c);
    if host.which("ollama").is_some()
        && let Some(mut o) = cap("runtime.infer.ollama", Provenance::Probed)
    {
        if let Some(v) = host
            .run("ollama", &["--version"])
            .and_then(|s| s.split("version is").nth(1).map(|v| v.trim().to_string()))
            .filter(|v| !v.is_empty())
        {
            o = o.with_attr("version", v.as_str());
        }
        c.push(
            o.with_attr("formats", str_list(&["gguf"]))
                .with_attr("arches_native", str_list(&[arch])),
        );
    }
}

/// Loopback address `geniex serve` is run on (Ventuno bench and plan).
pub const GENIEX_ADDR: &str = "127.0.0.1:18181";
/// Most model ids advertised from a GenieX server.
const MAX_GENIEX_MODELS: usize = 32;

/// Model ids from an OpenAI-style `/v1/models` body.
pub fn openai_model_ids(body: &str) -> Option<Vec<String>> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let ids = v
        .get("data")?
        .as_array()?
        .iter()
        .filter_map(|m| m.get("id")?.as_str().map(str::to_string))
        .take(MAX_GENIEX_MODELS)
        .collect();
    Some(ids)
}

/// Qualcomm GenieX: the binary lives outside `PATH`, so the evidence is a
/// server answering on its loopback address. Only the model ids are read.
fn geniex(host: &dyn ProbeHost, arch: &str, c: &mut Collected) {
    if host.which("curl").is_none() {
        return;
    }
    let url = format!("http://{GENIEX_ADDR}/v1/models");
    let Some(ids) = host
        .run("curl", &["-sf", "-m", "2", url.as_str()])
        .as_deref()
        .and_then(openai_model_ids)
    else {
        return;
    };
    if let Some(g) = cap("runtime.infer.geniex", Provenance::Probed) {
        c.push(
            g.with_attr("endpoint", GENIEX_ADDR)
                .with_attr("compute", "npu")
                .with_attr("formats", str_list(&["gguf", "qnn"]))
                .with_attr("models", str_list(&ids))
                .with_attr("arches_native", str_list(&[arch])),
        );
        c.note(
            "runtime.infer.geniex",
            "GenieX answered on loopback; models are those it can serve, not those resident",
        );
    }
}
