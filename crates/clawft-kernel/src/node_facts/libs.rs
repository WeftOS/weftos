//! Library and toolchain facts next to the hardware they serve (ADR-120 X1).
//!
//! Each is an experimental `x.lib.<name>` capability with versions as
//! attributes, emitted only when a tool, file or Python distribution shows
//! it (`probed`). Facts describe; they never grant access. Python is asked
//! for distribution metadata only (no heavy import) except for the ONNX
//! Runtime execution-provider list, and only when onnxruntime is installed.
//!
//! - `x.lib.cuda`: driver and API version (`nvidia-smi`), toolkit (`nvcc`),
//!   cuDNN and TensorRT majors (`ldconfig -p`).
//! - `x.lib.rocm`: `/opt/rocm/.info/version`.
//! - `x.lib.coreml`: CoreML.framework (macOS), plus coremltools if installed.
//! - `x.lib.mlx`, `x.lib.torch` (with its build), `x.lib.onnxruntime` (with
//!   its execution providers).

use clawft_types::placement::Provenance;

use super::host::ProbeHost;
use super::probe::{Collected, cap, str_list};

const COREML: &str = "/System/Library/Frameworks/CoreML.framework";
const ROCM_VERSION: &str = "/opt/rocm/.info/version";

/// One `python3 -c` call printing `<dist>=<version>` for each installed one.
pub(crate) const PY_VERSIONS: &str = "import importlib.metadata as m\n\
for d in ('mlx','torch','onnxruntime','onnxruntime-gpu','coremltools'):\n\
\ttry: print(d+'='+m.version(d))\n\
\texcept Exception: pass";

/// Execution providers the installed ONNX Runtime reports.
pub(crate) const PY_ORT_EPS: &str =
    "import onnxruntime as o;print(','.join(o.get_available_providers()))";

/// Run the library probes (after the vendor accelerators).
pub fn probe(host: &dyn ProbeHost, c: &mut Collected) {
    cuda(host, c);
    rocm(host, c);
    let dists = python_dists(host);
    coreml(host, &dists, c);
    python_libs(host, &dists, c);
}

/// Text after `label` up to whitespace.
fn after<'a>(text: &'a str, label: &str) -> Option<&'a str> {
    let rest = text.split(label).nth(1)?.trim_start();
    let v = rest.split_whitespace().next()?;
    (!v.is_empty()).then_some(v)
}

/// Major number of a shared library from `ldconfig -p` (`libcudnn.so.9 (...)`).
fn ldconfig_major(listing: &str, lib: &str) -> Option<String> {
    let needle = format!("{lib}.so.");
    listing
        .lines()
        .filter_map(|l| l.trim().strip_prefix(&needle))
        .filter_map(|r| r.split(|ch: char| !ch.is_ascii_digit()).next())
        .filter(|m| !m.is_empty())
        .max_by_key(|m| m.parse::<u64>().unwrap_or(0))
        .map(str::to_owned)
}

fn cuda(host: &dyn ProbeHost, c: &mut Collected) {
    if !c.has("accel.gpu.cuda") {
        return;
    }
    let Some(mut l) = cap("x.lib.cuda", Provenance::Probed) else { return };
    let smi = host.run("nvidia-smi", &[]).unwrap_or_default();
    if let Some(v) = after(&smi, "Driver Version:") {
        l = l.with_attr("driver", v);
    }
    if let Some(v) = after(&smi, "CUDA Version:") {
        l = l.with_attr("cuda_api", v);
    }
    if let Some(nvcc) = host.run("nvcc", &["--version"])
        && let Some(v) = nvcc.split(", V").nth(1).and_then(|r| r.split_whitespace().next())
    {
        l = l.with_attr("toolkit", v);
    }
    let ld = host.run("ldconfig", &["-p"]).unwrap_or_default();
    if let Some(v) = ldconfig_major(&ld, "libcudnn") {
        l = l.with_attr("cudnn", v.as_str());
    }
    if let Some(v) = ldconfig_major(&ld, "libnvinfer") {
        l = l.with_attr("tensorrt", v.as_str());
    }
    c.push(l);
    c.note("x.lib.cuda", "nvidia-smi (driver, API), nvcc (toolkit), ldconfig -p (cuDNN, TensorRT majors)");
}

fn rocm(host: &dyn ProbeHost, c: &mut Collected) {
    let Some(v) = host.read_file(ROCM_VERSION) else { return };
    let v = v.trim();
    if let (false, Some(l)) = (v.is_empty(), cap("x.lib.rocm", Provenance::Probed)) {
        c.push(l.with_attr("version", v));
    }
}

/// `(distribution, version)` pairs Python reports as installed.
fn python_dists(host: &dyn ProbeHost) -> Vec<(String, String)> {
    host.run("python3", &["-c", PY_VERSIONS])
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(d, v)| (d.trim().to_owned(), v.trim().to_owned()))
        .filter(|(d, v)| !d.is_empty() && !v.is_empty())
        .collect()
}

fn dist<'a>(dists: &'a [(String, String)], name: &str) -> Option<&'a str> {
    dists.iter().find(|(d, _)| d == name).map(|(_, v)| v.as_str())
}

fn coreml(host: &dyn ProbeHost, dists: &[(String, String)], c: &mut Collected) {
    if !host.exists(COREML) {
        return;
    }
    if let Some(mut l) = cap("x.lib.coreml", Provenance::Probed) {
        if let Some(v) = dist(dists, "coremltools") {
            l = l.with_attr("coremltools", v);
        }
        c.push(l);
    }
}

/// Build of a PyTorch version string: the `+cu121` / `+rocm6.0` / `+cpu`
/// label, else MPS on Apple silicon macOS, else the default (CPU) wheel.
pub(crate) fn torch_build(version: &str, os: &str, arch: &str) -> String {
    match version.split_once('+').map(|(_, b)| b) {
        Some(b) if !b.is_empty() => b.to_owned(),
        _ if os == "macos" && arch == "aarch64" => "mps".into(),
        _ => "default".into(),
    }
}

fn python_libs(host: &dyn ProbeHost, dists: &[(String, String)], c: &mut Collected) {
    if let (Some(v), Some(l)) = (dist(dists, "mlx"), cap("x.lib.mlx", Provenance::Probed)) {
        c.push(l.with_attr("version", v));
    }
    if let (Some(v), Some(l)) = (dist(dists, "torch"), cap("x.lib.torch", Provenance::Probed)) {
        let build = torch_build(v, &host.os(), &host.arch());
        c.push(l.with_attr("version", v).with_attr("build", build.as_str()));
    }
    let ort = dist(dists, "onnxruntime-gpu").map(|v| (v, true)).or_else(|| dist(dists, "onnxruntime").map(|v| (v, false)));
    if let (Some((v, gpu)), Some(l)) = (ort, cap("x.lib.onnxruntime", Provenance::Probed)) {
        let mut l = l.with_attr("version", v).with_attr("gpu_dist", gpu);
        let eps: Vec<String> = host
            .run("python3", &["-c", PY_ORT_EPS])
            .unwrap_or_default()
            .trim()
            .split(',')
            .filter(|e| e.ends_with("ExecutionProvider"))
            .map(str::to_owned)
            .collect();
        if !eps.is_empty() {
            l = l.with_attr("providers", str_list(&eps));
        }
        c.push(l);
    }
}
