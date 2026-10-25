//! Media, device, Qualcomm NPU and GenieX probes against a scripted Arduino
//! Ventuno Q (values taken from the board on 2026-10-25), plus a USB camera,
//! a Pluto SDR and a SocketCAN interface added for the plug-in cases.

use clawft_types::placement::{AttrValue, Capability, CapabilityId, Requirement, match_all};

use super::fake_host::FakeHost;
use super::media::{card_names, pcm_counts};
use super::probe::{ProbeConfig, build_facts, probe_capabilities};
use super::runtimes::openai_model_ids;

const V4L: &str = "/sys/class/video4linux";
const GENIEX: &str = "curl -sf -m 2 http://127.0.0.1:18181/v1/models";

fn ventuno() -> FakeHost {
    FakeHost::new("linux", "aarch64")
        .file("/proc/cpuinfo", "processor\t: 0\nprocessor\t: 1\n")
        .file("/proc/device-tree/model", "Arduino VENTUNO Q\0")
        .file("/proc/device-tree/compatible", "arduino,monza\0qcom,monaco-monza\0qcom,qcs8300\0")
        .file("/proc/meminfo", "MemTotal: 14000000 kB\nMemAvailable: 13000000 kB\n")
        .dir(
            "/dev",
            &[
                "null", "fastrpc-adsp-secure", "fastrpc-cdsp", "fastrpc-cdsp-secure", "fastrpc-gdsp0",
                "gpiochip0", "gpiochip1", "gpiochip2", "gpiochip3", "i2c-12", "i2c-13", "i2c-14",
                "ttyS0", "ttyS1", "ttySTM0", "ttyGS0", "ttyMSM0", "ttyUSB0",
            ],
        )
        .file("/sys/class/tty/console/active", "ttyMSM0\n")
        .file("/proc/cmdline", "root=/dev/mmcblk0p71 console=ttyMSM0,115200n8 quiet\n")
        .dir(V4L, &["video32", "video33"])
        .file(&format!("{V4L}/video32/device/uevent"), "DRIVER=msm_vidc_v4l2\nOF_NAME=video-codec\n")
        .file(&format!("{V4L}/video33/device/uevent"), "DRIVER=msm_vidc_v4l2\n")
        .out("gst-inspect-1.0 v4l2h264enc", "Factory Details:\n")
        .out("gst-inspect-1.0 v4l2h265enc", "Factory Details:\n")
        .out("gst-inspect-1.0 v4l2h264dec", "Factory Details:\n")
        .out("gst-inspect-1.0 v4l2av1dec", "Factory Details:\n")
        .file("/proc/asound/cards", " 0 [monacogertrude ]: qcs8300 - monaco-gertrude\n                      monaco-gertrude\n")
        .file(
            "/proc/asound/pcm",
            "00-00: MultiMedia1 Playback (*) :  : playback 1\n00-01: MultiMedia2 Capture (*) :  : capture 1\n00-02: MultiMedia3 Playback (*) :  : playback 1\n",
        )
        .dir("/sys/class/net", &["end0", "lo", "wlp3s0"])
        .file("/sys/class/net/end0/type", "1\n")
        .file("/sys/class/net/lo/type", "772\n")
        .file("/sys/class/net/wlp3s0/type", "1\n")
        .path("/run/arduino-router.sock")
        .dir("/sys/class/hwmon", &["hwmon0", "hwmon7", "hwmon8"])
        .file("/sys/class/hwmon/hwmon0/name", "emc2305\n")
        .file("/sys/class/hwmon/hwmon7/name", "ina232\n")
        .file("/sys/class/hwmon/hwmon8/name", "nsp_0_0_0_thermal\n")
        .out("lsusb", "Bus 001 Device 001: ID 1d6b:0002 Linux Foundation 2.0 root hub\n")
        .out(
            GENIEX,
            r#"{"data":[{"id":"google/gemma-4-E2B-it-qat-q4_0-gguf:Q4_0","object":"model"},{"id":"qualcomm/qwen3_vl_4b_instruct:W4A16","object":"model"}]}"#,
        )
}

/// The same board with a USB webcam, a Pluto SDR and a CAN interface.
fn ventuno_plugged() -> FakeHost {
    ventuno()
        .dir(V4L, &["video0", "video1", "video32", "video33"])
        .file(&format!("{V4L}/video0/name"), "HD Pro Webcam C920\n")
        .file(&format!("{V4L}/video0/index"), "0\n")
        .file(&format!("{V4L}/video0/device/uevent"), "DEVTYPE=usb_interface\nDRIVER=uvcvideo\n")
        .file(&format!("{V4L}/video1/name"), "HD Pro Webcam C920\n")
        .file(&format!("{V4L}/video1/index"), "1\n")
        .file(&format!("{V4L}/video1/device/uevent"), "DRIVER=uvcvideo\n")
        .out(
            "lsusb",
            "Bus 002 Device 003: ID 0456:b673 Analog Devices, Inc. LibIIO based AD9363 Software Defined Radio [PlutoSDR]\nBus 001 Device 002: ID 046d:082d Logitech, Inc. HD Pro Webcam C920\n",
        )
        .dir("/sys/class/net", &["can0", "end0"])
        .file("/sys/class/net/can0/type", "280\n")
        .file("/sys/class/net/end0/type", "1\n")
}

fn get<'a>(caps: &'a [Capability], id: &str) -> Option<&'a Capability> {
    caps.iter().find(|c| c.id.as_str() == id)
}

fn strs(c: &Capability, attr: &str) -> Vec<String> {
    match c.attrs.get(attr) {
        Some(AttrValue::List(l)) => l
            .iter()
            .filter_map(|x| match x {
                AttrValue::Str(s) => Some(s.clone()),
                _ => None,
            })
            .collect(),
        _ => vec![],
    }
}

#[test]
fn ventuno_npu_codec_audio_buses_mcu_power() {
    let c = probe_capabilities(&ventuno(), &ProbeConfig::default());
    let f = build_facts("n-venq", 1_000, 600, 1, c.clone());
    f.validate().unwrap();
    let caps = &f.capabilities;

    let npu = get(caps, "accel.npu.qualcomm").expect("npu");
    assert_eq!(npu.attrs.get("soc"), Some(&AttrValue::Str("qcs8300".into())));
    assert!(strs(npu, "domains").contains(&"fastrpc-cdsp".to_string()));
    assert!(!npu.exclusive, "the NPU is shared through one service, not exclusive");

    let gx = get(caps, "runtime.infer.geniex").expect("geniex");
    assert_eq!(strs(gx, "models").len(), 2);
    assert!(get(caps, "format.qnn").is_some() && get(caps, "format.gguf").is_some());

    let codec = get(caps, "media.video.codec").expect("codec");
    assert_eq!(strs(codec, "encode"), ["h264", "h265"]);
    assert_eq!(strs(codec, "decode"), ["h264", "av1"]);
    assert!(get(caps, "media.camera.usb").is_none(), "codec nodes are not cameras");
    assert!(get(caps, "media.camera.csi").is_none());

    assert_eq!(get(caps, "media.audio.capture").unwrap().attrs.get("streams"), Some(&AttrValue::Int(1)));
    assert_eq!(get(caps, "media.audio.playback").unwrap().attrs.get("streams"), Some(&AttrValue::Int(2)));

    assert_eq!(strs(get(caps, "bus.i2c").unwrap(), "devices"), ["i2c-12", "i2c-13", "i2c-14"]);
    assert_eq!(strs(get(caps, "bus.gpio").unwrap(), "devices").len(), 4);
    assert_eq!(
        strs(get(caps, "bus.uart").unwrap(), "devices"),
        ["ttyUSB0"],
        "ttyS*, ttySTM0 (MCU link), ttyGS0 and the console ttyMSM0 are not advertised"
    );
    assert!(c.notes.iter().any(|n| n.probe == "bus.uart" && n.note.contains("ttyMSM0")));
    assert!(get(caps, "bus.spi").is_none() && get(caps, "bus.can").is_none());
    assert_eq!(get(caps, "bus.mcu").unwrap().attrs.get("bridge"), Some(&AttrValue::Str("arduino-router".into())));
    assert_eq!(strs(get(caps, "power.monitor").unwrap(), "sensors"), ["ina232"]);
    assert!(caps.iter().all(|c| !c.id.as_str().starts_with("sdr.")));
    assert!(c.notes.iter().any(|n| n.probe == "bus.mcu"));
}

#[test]
fn ventuno_with_usb_camera_pluto_and_can() {
    let c = probe_capabilities(&ventuno_plugged(), &ProbeConfig::default());
    let f = build_facts("n-venq", 1_000, 600, 1, c);
    f.validate().unwrap();
    let caps = &f.capabilities;

    let cam = get(caps, "media.camera.usb").expect("usb camera");
    assert_eq!(cam.attrs.get("count"), Some(&AttrValue::Int(1)), "metadata node not counted");
    assert_eq!(strs(cam, "devices"), ["video0"]);
    let sdr = get(caps, "sdr.iio").expect("pluto");
    assert_eq!(sdr.attrs.get("model"), Some(&AttrValue::Str("pluto".into())));
    assert!(sdr.exclusive);
    assert_eq!(strs(get(caps, "bus.can").unwrap(), "devices"), ["can0"]);

    // An interface requirement matches by prefix: any camera, any SDR.
    let req = |p: &str| Requirement::prefix(p).unwrap();
    assert!(match_all(&[req("media.camera"), req("sdr"), req("accel.npu")], caps).is_ok());
}

#[test]
fn plain_linux_box_gets_none_of_this() {
    let h = FakeHost::new("linux", "x86_64")
        .file("/proc/meminfo", "MemTotal: 1000 kB\n")
        .dir("/dev", &["null", "ttyS0", "ttyS1"]);
    let c = probe_capabilities(&h, &ProbeConfig::default());
    for p in ["accel.npu", "runtime.infer.geniex", "media.", "bus.", "sdr.", "power."] {
        assert!(c.caps.iter().all(|x| !x.id.as_str().starts_with(p)), "{p} advertised");
    }
    let _ = CapabilityId::new("bus.i2c").unwrap();
}

#[test]
fn geniex_absent_or_garbled_is_not_advertised() {
    let off = ventuno().out(GENIEX, "curl: (7) Failed to connect");
    let c = probe_capabilities(&off, &ProbeConfig::default());
    assert!(c.caps.iter().all(|x| x.id.as_str() != "runtime.infer.geniex"));
    assert!(c.caps.iter().any(|x| x.id.as_str() == "accel.npu.qualcomm"), "the NPU stays");
    assert_eq!(openai_model_ids(r#"{"data":[]}"#), Some(vec![]));
    assert_eq!(openai_model_ids("not json"), None);
}

#[test]
fn alsa_parsers() {
    assert_eq!(pcm_counts("00-00: a :  : playback 1 : capture 2\n00-01: b :  : capture 1\n"), (3, 1));
    assert_eq!(pcm_counts(""), (0, 0));
    assert_eq!(
        card_names(" 0 [monacogertrude ]: qcs8300 - monaco-gertrude\n                      monaco-gertrude\n 1 [U0x46d0x82d    ]: USB-Audio - HD Pro Webcam C920\n"),
        ["monaco-gertrude", "HD Pro Webcam C920"]
    );
}
