//! Media probes (Linux): cameras, the hardware video codec and ALSA audio
//! (Ventuno cog plan, section 4.6).
//!
//! Sources: `/sys/class/video4linux/*/{name,index,device/uevent}`,
//! `gst-inspect-1.0` for the V4L2 codec elements, `/proc/asound/{cards,pcm}`.
//! A capture device is advertised because it exists, not because anything is
//! plugged into it: an ALSA capture PCM with no microphone fitted still shows,
//! and the note says so.

use clawft_types::placement::Provenance;

use super::host::ProbeHost;
use super::probe::{Collected, cap, str_list};

const V4L: &str = "/sys/class/video4linux";

/// Kernel drivers of memory-to-memory video codecs (not cameras).
const CODEC_DRIVERS: &[&str] = &[
    "msm_vidc_v4l2",
    "qcom-venus",
    "qcom-iris",
    "bcm2835-codec",
    "rpivid",
    "hantro-vpu",
    "rkvdec",
];

/// GStreamer V4L2 codec elements and the format each one names.
const ENCODERS: &[(&str, &str)] = &[("v4l2h264enc", "h264"), ("v4l2h265enc", "h265")];
const DECODERS: &[(&str, &str)] = &[
    ("v4l2h264dec", "h264"),
    ("v4l2h265dec", "h265"),
    ("v4l2vp9dec", "vp9"),
    ("v4l2av1dec", "av1"),
];

/// Run the media probes.
pub fn probe(host: &dyn ProbeHost, c: &mut Collected) {
    video(host, c);
    audio(host, c);
}

/// One V4L2 node: `(node, name, driver, index)`.
fn v4l_nodes(host: &dyn ProbeHost) -> Vec<(String, String, String, Option<u32>)> {
    host.list_dir(V4L)
        .unwrap_or_default()
        .into_iter()
        .filter(|n| n.starts_with("video") || n.starts_with("v4l-subdev"))
        .map(|n| {
            let read = |f: &str| {
                host.read_file(&format!("{V4L}/{n}/{f}"))
                    .unwrap_or_default()
                    .trim()
                    .to_string()
            };
            let driver = read("device/uevent")
                .lines()
                .find_map(|l| l.strip_prefix("DRIVER=").map(str::to_string))
                .unwrap_or_default();
            let index = read("index").parse().ok();
            (n.clone(), read("name"), driver, index)
        })
        .collect()
}

fn video(host: &dyn ProbeHost, c: &mut Collected) {
    let nodes = v4l_nodes(host);
    // UVC registers a capture node (index 0) and a metadata node per camera.
    let usb: Vec<&(String, String, String, Option<u32>)> = nodes
        .iter()
        .filter(|(n, _, d, i)| n.starts_with("video") && d == "uvcvideo" && i.unwrap_or(0) == 0)
        .collect();
    if !usb.is_empty()
        && let Some(cam) = cap("media.camera.usb", Provenance::Probed)
    {
        let devices: Vec<&str> = usb.iter().map(|(n, ..)| n.as_str()).collect();
        let names: Vec<&str> = usb.iter().map(|(_, m, ..)| m.as_str()).collect();
        c.push(
            cam.with_attr("count", usb.len() as i64)
                .with_attr("devices", str_list(&devices))
                .with_attr("names", str_list(&names)),
        );
    }

    // A CSI camera is a camss capture path plus a bound sensor sub-device.
    let camss = nodes.iter().any(|(_, _, d, _)| d.contains("camss"));
    let sensors: Vec<&str> = nodes
        .iter()
        .filter(|(n, m, ..)| n.starts_with("v4l-subdev") && !m.is_empty() && !m.starts_with("msm_"))
        .map(|(_, m, ..)| m.as_str())
        .collect();
    if camss && !sensors.is_empty() {
        if let Some(cam) = cap("media.camera.csi", Provenance::Probed) {
            c.push(cam.with_attr("count", sensors.len() as i64).with_attr("sensors", str_list(&sensors)));
        }
    } else if camss || host.exists("/sys/bus/platform/drivers/qcom-camss") {
        c.note("media.camera.csi", "CSI receiver present; no camera sensor bound");
    }

    let codec = nodes.iter().find(|(_, _, d, _)| CODEC_DRIVERS.contains(&d.as_str()));
    if let Some((_, _, driver, _)) = codec
        && let Some(mut v) = cap("media.video.codec", Provenance::Probed)
    {
        v = v.with_attr("driver", driver.as_str());
        if host.which("gst-inspect-1.0").is_some() {
            let has = |list: &[(&str, &'static str)]| -> Vec<&'static str> {
                list.iter()
                    .filter(|(e, _)| host.run("gst-inspect-1.0", &[e]).is_some())
                    .map(|(_, f)| *f)
                    .collect()
            };
            v = v
                .with_attr("encode", str_list(&has(ENCODERS)))
                .with_attr("decode", str_list(&has(DECODERS)))
                .with_attr("via", "gstreamer-v4l2");
        } else {
            c.note("media.video.codec", "codec driver bound; gst-inspect-1.0 absent, formats not probed");
        }
        c.push(v);
    }
}

/// Count `capture N` / `playback N` streams in `/proc/asound/pcm`.
pub fn pcm_counts(pcm: &str) -> (i64, i64) {
    let mut cap_n = 0;
    let mut play_n = 0;
    for line in pcm.lines() {
        for field in line.split(':').skip(1) {
            let f = field.trim();
            if let Some(n) = f.strip_prefix("capture ") {
                cap_n += n.trim().parse::<i64>().unwrap_or(1);
            } else if let Some(n) = f.strip_prefix("playback ") {
                play_n += n.trim().parse::<i64>().unwrap_or(1);
            }
        }
    }
    (cap_n, play_n)
}

/// Card names from `/proc/asound/cards` (`" 0 [id ]: driver - name"`).
pub fn card_names(cards: &str) -> Vec<String> {
    cards
        .lines()
        .filter(|l| l.trim_start().chars().next().is_some_and(|ch| ch.is_ascii_digit()))
        .filter_map(|l| l.split(" - ").nth(1).map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty())
        .collect()
}

fn audio(host: &dyn ProbeHost, c: &mut Collected) {
    let Some(pcm) = host.read_file("/proc/asound/pcm") else {
        return;
    };
    let cards = card_names(&host.read_file("/proc/asound/cards").unwrap_or_default());
    let (capture, playback) = pcm_counts(&pcm);
    for (id, n) in [("media.audio.capture", capture), ("media.audio.playback", playback)] {
        if n > 0
            && let Some(a) = cap(id, Provenance::Probed)
        {
            c.push(a.with_attr("streams", n).with_attr("cards", str_list(&cards)));
        }
    }
    if capture > 0 {
        c.note("media.audio.capture", "ALSA capture streams exist; whether a microphone is fitted is not visible");
    }
}
