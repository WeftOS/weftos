//! Library/toolchain facts (ADR-120 X1) against scripted DGX-like and Mac hosts.

use clawft_types::placement::{AttrValue, Capability};

use super::fake_host::FakeHost;
use super::libs::{PY_ORT_EPS, PY_VERSIONS, torch_build};
use super::probe::{ProbeConfig, build_facts, probe_capabilities, refresh_live};

const SMI: &str = "| NVIDIA-SMI 550.54.15   Driver Version: 550.54.15   CUDA Version: 12.4 |\n";
const NVCC: &str = "nvcc: NVIDIA (R) Cuda compiler driver\nCuda compilation tools, release 12.4, V12.4.131\n";
const LDCONFIG: &str = "\tlibcudnn.so.9 (libc6,x86-64) => /usr/lib/x86_64-linux-gnu/libcudnn.so.9\n\
\tlibcudnn.so.8 (libc6,x86-64) => /usr/lib/x86_64-linux-gnu/libcudnn.so.8\n\
\tlibnvinfer.so.10 (libc6,x86-64) => /usr/lib/x86_64-linux-gnu/libnvinfer.so.10\n";
const GPU_Q: &str = "nvidia-smi --query-gpu=name,memory.total,memory.free --format=csv,noheader,nounits";

fn dgx() -> FakeHost {
    FakeHost::new("linux", "x86_64")
        .out(GPU_Q, "NVIDIA GB10, 131072, 100000\n")
        .out("nvidia-smi", SMI)
        .out("nvcc --version", NVCC)
        .out("ldconfig -p", LDCONFIG)
        .file("/opt/rocm/.info/version", "")
        .out(
            &format!("python3 -c {PY_VERSIONS}"),
            "torch=2.5.1+cu124\nonnxruntime-gpu=1.20.1\n",
        )
        .out(
            &format!("python3 -c {PY_ORT_EPS}"),
            "TensorrtExecutionProvider,CUDAExecutionProvider,CPUExecutionProvider\n",
        )
}

fn get(c: &[Capability], id: &str) -> Option<Capability> {
    c.iter().find(|x| x.id.as_str() == id).cloned()
}

fn s(c: &Capability, k: &str) -> Option<String> {
    match c.attrs.get(k) {
        Some(AttrValue::Str(v)) => Some(v.clone()),
        _ => None,
    }
}

#[test]
fn cuda_stack_versions_and_vram_free_vs_total() {
    let caps = probe_capabilities(&dgx(), &ProbeConfig::default()).caps;
    let l = get(&caps, "x.lib.cuda").expect("x.lib.cuda");
    assert_eq!(s(&l, "driver").as_deref(), Some("550.54.15"));
    assert_eq!(s(&l, "cuda_api").as_deref(), Some("12.4"));
    assert_eq!(s(&l, "toolkit").as_deref(), Some("12.4.131"));
    assert_eq!(s(&l, "cudnn").as_deref(), Some("9"), "highest cuDNN major");
    assert_eq!(s(&l, "tensorrt").as_deref(), Some("10"));
    let v = get(&caps, "mem.vram").unwrap();
    assert!(matches!(v.attrs.get("total"), Some(AttrValue::Int(t)) if *t == 131072 * 1024 * 1024));
    assert!(matches!(v.attrs.get("free"), Some(AttrValue::Int(f)) if *f == 100000 * 1024 * 1024));
    let t = get(&caps, "x.lib.torch").unwrap();
    assert_eq!(s(&t, "build").as_deref(), Some("cu124"));
    let o = get(&caps, "x.lib.onnxruntime").unwrap();
    assert_eq!(o.attrs.get("gpu_dist"), Some(&AttrValue::Bool(true)));
    match o.attrs.get("providers") {
        Some(AttrValue::List(p)) => assert_eq!(p.len(), 3),
        other => panic!("providers {other:?}"),
    }
    assert!(get(&caps, "x.lib.rocm").is_none(), "empty rocm version file is not a fact");
    build_facts("n-dgx", 1, 600, 1, probe_capabilities(&dgx(), &ProbeConfig::default()))
        .validate()
        .unwrap();
}

#[test]
fn no_gpu_no_cuda_lib_and_no_python_no_python_libs() {
    let h = FakeHost::new("linux", "aarch64");
    let caps = probe_capabilities(&h, &ProbeConfig::default()).caps;
    assert!(caps.iter().all(|c| !c.id.as_str().starts_with("x.lib.")));
}

#[test]
fn nvidia_smi_failing_still_reports_what_other_tools_show() {
    let h = FakeHost::new("linux", "x86_64")
        .out(GPU_Q, "NVIDIA L4, 24576, 20000\n")
        .out("ldconfig -p", LDCONFIG);
    let caps = probe_capabilities(&h, &ProbeConfig::default()).caps;
    let l = get(&caps, "x.lib.cuda").unwrap();
    assert_eq!(s(&l, "driver"), None);
    assert_eq!(s(&l, "cudnn").as_deref(), Some("9"));
}

#[test]
fn rocm_version_from_the_info_file() {
    let h = FakeHost::new("linux", "x86_64").file("/opt/rocm/.info/version", "6.1.2-119\n");
    let caps = probe_capabilities(&h, &ProbeConfig::default()).caps;
    assert_eq!(s(&get(&caps, "x.lib.rocm").unwrap(), "version").as_deref(), Some("6.1.2-119"));
}

#[test]
fn mac_reports_coreml_mlx_and_mps_torch() {
    let h = FakeHost::new("macos", "aarch64")
        .path("/System/Library/Frameworks/CoreML.framework")
        .out(
            &format!("python3 -c {PY_VERSIONS}"),
            "mlx=0.22.0\ntorch=2.5.1\ncoremltools=8.1\n",
        );
    let caps = probe_capabilities(&h, &ProbeConfig::default()).caps;
    assert_eq!(s(&get(&caps, "x.lib.coreml").unwrap(), "coremltools").as_deref(), Some("8.1"));
    assert_eq!(s(&get(&caps, "x.lib.mlx").unwrap(), "version").as_deref(), Some("0.22.0"));
    assert_eq!(s(&get(&caps, "x.lib.torch").unwrap(), "build").as_deref(), Some("mps"));
}

#[test]
fn torch_build_labels() {
    assert_eq!(torch_build("2.4.0+cu121", "linux", "x86_64"), "cu121");
    assert_eq!(torch_build("2.4.0+rocm6.0", "linux", "x86_64"), "rocm6.0");
    assert_eq!(torch_build("2.4.0", "macos", "aarch64"), "mps");
    assert_eq!(torch_build("2.4.0", "linux", "x86_64"), "default");
}

#[test]
fn live_refresh_updates_vram_free_only() {
    let base = build_facts("n-dgx", 1, 600, 1, probe_capabilities(&dgx(), &ProbeConfig::default()));
    let h = dgx().out(
        "nvidia-smi --query-gpu=memory.free --format=csv,noheader,nounits",
        "5000\n",
    );
    let f = refresh_live(&h, &base, 2);
    let v = f.capabilities.iter().find(|c| c.id.as_str() == "mem.vram").unwrap();
    assert!(matches!(v.attrs.get("free"), Some(AttrValue::Int(x)) if *x == 5000 * 1024 * 1024));
    assert!(matches!(v.attrs.get("total"), Some(AttrValue::Int(x)) if *x == 131072 * 1024 * 1024));
}
