//! Idle art for the e-ink glance panel: pick a weaving scene, ask the image
//! service for it, dither the PNG to the panel's 250x122 1-bit bitmap and
//! pack it for the `img` serial message. Also the pure scheduler that decides
//! when art may be generated and when activity takes it down again.

use std::path::Path;
use std::time::Duration as StdDuration;

use base64::Engine as _;
use chrono::{DateTime, Duration, Utc};
use serde_json::{Value, json};

use crate::config::ArtConfig;
use crate::glance::ascii_clip;

pub const W: usize = 250;
pub const H: usize = 122;
/// Bytes per packed row: `ceil(250 / 8)`.
pub const ROW_BYTES: usize = W.div_ceil(8);
pub const IMG_BYTES: usize = ROW_BYTES * H;
pub const MAX_CAPTION: usize = 24;
/// No activity for this long before the first art.
pub const IDLE_AFTER: Duration = Duration::minutes(15);
/// At most one art change per this long.
pub const MIN_GAP: Duration = Duration::minutes(60);
/// Wait this long after a failed attempt.
pub const RETRY_AFTER: Duration = Duration::minutes(10);
const STYLE: &str = ", woodcut, black ink on white paper, high contrast";
const NEGATIVE: &str = "color, gray, shading, photo, blurry";
const STEPS: u32 = 12;

// ---- scenes ----------------------------------------------------------------

/// (prompt subject, caption base). Captions stay short: base + " " + when <= 24.
const SUBJECTS: &[(&str, &str)] = &[
    ("a wooden loom strung with warp threads", "the loom"),
    ("a spider's web strung with dew between branches", "a spider's web"),
    ("a hand-woven reed basket on a table", "a basket"),
    ("a large tapestry hanging on a stone wall", "a tapestry"),
    ("an old spinning wheel with a skein of wool", "spinning wheel"),
    ("fishermen mending a fishing net on a quay", "mending nets"),
    ("a bird's nest woven from twigs and grass", "a bird's nest"),
    ("threads of light weaving through a dark forest", "threads of light"),
    ("a weaver's shuttle passing through the warp", "the shuttle"),
    ("a spindle and a ball of yarn beside a window", "spindle and yarn"),
    ("a cat asleep beside a basket of yarn", "yarn and a cat"),
    ("a rope bridge woven across a gorge", "a rope bridge"),
];
/// (prompt detail, caption tail).
const WHENS: &[(&str, &str)] = &[
    ("at dusk, long shadows", "at dusk"),
    ("at dawn, soft mist", "at dawn"),
    ("by lamplight", "by lamp"),
    ("in winter, falling snow", "in snow"),
    ("at noon, strong sun", "at noon"),
    ("in the rain", "in rain"),
    ("under a full moon", "by moon"),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scene {
    pub prompt: String,
    pub caption: String,
}

fn mix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// A scene chosen deterministically from `seed`.
pub fn scene(seed: u64) -> Scene {
    let a = mix(seed);
    let (subject, base) = SUBJECTS[(a % SUBJECTS.len() as u64) as usize];
    let (detail, when) = WHENS[(mix(a) % WHENS.len() as u64) as usize];
    Scene { prompt: format!("{subject} {detail}{STYLE}"), caption: ascii_clip(&format!("{base} {when}"), MAX_CAPTION) }
}

// ---- image -----------------------------------------------------------------

/// Decode a PNG into 8-bit gray (alpha composited on white).
pub fn decode_gray(png_bytes: &[u8]) -> Result<(Vec<u8>, usize, usize), String> {
    let mut dec = png::Decoder::new(std::io::Cursor::new(png_bytes));
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = dec.read_info().map_err(|e| format!("png: {e}"))?;
    let mut buf = vec![0u8; reader.output_buffer_size().ok_or("png: too large")?];
    let info = reader.next_frame(&mut buf).map_err(|e| format!("png: {e}"))?;
    let (w, h) = (info.width as usize, info.height as usize);
    let ch = info.color_type.samples();
    let px = |i: usize| -> u8 {
        let p = &buf[i * ch..(i + 1) * ch];
        let (g, a) = match info.color_type {
            png::ColorType::Grayscale => (p[0] as u32, 255u32),
            png::ColorType::GrayscaleAlpha => (p[0] as u32, p[1] as u32),
            png::ColorType::Rgb | png::ColorType::Indexed => ((299 * p[0] as u32 + 587 * p[1] as u32 + 114 * p[2] as u32) / 1000, 255),
            png::ColorType::Rgba => ((299 * p[0] as u32 + 587 * p[1] as u32 + 114 * p[2] as u32) / 1000, p[3] as u32),
        };
        ((g * a + 255 * (255 - a)) / 255) as u8
    };
    if w == 0 || h == 0 {
        return Err("png: empty".into());
    }
    Ok(((0..w * h).map(px).collect(), w, h))
}

/// Centre-crop to the panel's aspect ratio and box-average down to 250x122.
pub fn fit_gray(src: &[u8], sw: usize, sh: usize) -> Vec<u8> {
    // Crop window in source pixels, (cw x ch), centred.
    let (cw, ch) = if sw * H >= sh * W { ((sh * W / H).max(1), sh) } else { (sw, (sw * H / W).max(1)) };
    let (x0, y0) = ((sw - cw) / 2, (sh - ch) / 2);
    let mut out = vec![0u8; W * H];
    for y in 0..H {
        let (ys, ye) = (y0 + y * ch / H, (y0 + (y + 1) * ch / H).max(y0 + y * ch / H + 1));
        for x in 0..W {
            let (xs, xe) = (x0 + x * cw / W, (x0 + (x + 1) * cw / W).max(x0 + x * cw / W + 1));
            let (mut sum, mut n) = (0u32, 0u32);
            for yy in ys..ye.min(sh) {
                for xx in xs..xe.min(sw) {
                    sum += src[yy * sw + xx] as u32;
                    n += 1;
                }
            }
            out[y * W + x] = (sum / n.max(1)) as u8;
        }
    }
    out
}

/// Atkinson dither of a 250x122 gray image: `true` is ink (black).
pub fn atkinson(gray: &[u8]) -> Vec<bool> {
    assert_eq!(gray.len(), W * H);
    let mut buf: Vec<i16> = gray.iter().map(|&g| g as i16).collect();
    let mut out = vec![false; W * H];
    for y in 0..H {
        for x in 0..W {
            let old = buf[y * W + x];
            let ink = old < 128;
            out[y * W + x] = ink;
            let err = (old - if ink { 0 } else { 255 }) / 8;
            for (dx, dy) in [(1i32, 0i32), (2, 0), (-1, 1), (0, 1), (1, 1), (0, 2)] {
                let (nx, ny) = (x as i32 + dx, y as i32 + dy);
                if (0..W as i32).contains(&nx) && (0..H as i32).contains(&ny) {
                    buf[ny as usize * W + nx as usize] += err;
                }
            }
        }
    }
    out
}

/// Pack MSB-first, 32 bytes per row, 1 = ink; the 6 pad bits per row are 0.
pub fn pack(bits: &[bool]) -> Vec<u8> {
    let mut out = vec![0u8; IMG_BYTES];
    for y in 0..H {
        for x in 0..W {
            if bits[y * W + x] {
                out[y * ROW_BYTES + x / 8] |= 0x80 >> (x % 8);
            }
        }
    }
    out
}

/// PNG bytes to the packed panel bitmap.
pub fn png_to_bitmap(png_bytes: &[u8]) -> Result<Vec<u8>, String> {
    let (gray, w, h) = decode_gray(png_bytes)?;
    Ok(pack(&atkinson(&fit_gray(&gray, w, h))))
}

// ---- the serial message ----------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtImage {
    pub bitmap: Vec<u8>,
    pub caption: String,
    /// The service's PNG, kept for the phone (`GET /v1/cos/glance/art.png`).
    pub png: Vec<u8>,
}

impl ArtImage {
    /// `{"v":1,"img":"<base64>","w":250,"h":122,"caption":"...","ts":"HH:MM"}\n`
    pub fn to_line(&self, ts: &str) -> String {
        let v = json!({"v": 1, "img": base64::engine::general_purpose::STANDARD.encode(&self.bitmap), "w": W, "h": H,
            "caption": ascii_clip(&self.caption, MAX_CAPTION), "ts": ascii_clip(ts, 8)});
        format!("{v}\n")
    }
}

/// The checks the firmware makes on an image line, for tests and tooling.
pub fn validate_line(line: &str) -> Result<(), String> {
    let v: Value = serde_json::from_str(line.trim_end()).map_err(|e| format!("not json: {e}"))?;
    if v["v"] != 1 {
        return Err("v must be 1".into());
    }
    if v["w"] != W || v["h"] != H {
        return Err(format!("image must be {W}x{H}"));
    }
    let raw = base64::engine::general_purpose::STANDARD.decode(v["img"].as_str().ok_or("img missing")?).map_err(|e| format!("img: {e}"))?;
    if raw.len() != IMG_BYTES {
        return Err(format!("img is {} bytes, want {IMG_BYTES}", raw.len()));
    }
    let cap = v["caption"].as_str().unwrap_or("");
    if !cap.is_ascii() || cap.len() > MAX_CAPTION {
        return Err("caption must be ASCII, 24 chars at most".into());
    }
    if line.trim_end().contains('\n') {
        return Err("one line only".into());
    }
    Ok(())
}

// ---- the service -----------------------------------------------------------

/// Read the bearer token; refuse a file other users can read.
pub fn read_token(path: &Path) -> Result<String, String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path).map_err(|e| format!("token file {}: {e}", path.display()))?.permissions().mode();
        if mode & 0o077 != 0 {
            return Err(format!("token file {} must be mode 600 (is {:o})", path.display(), mode & 0o777));
        }
    }
    let t = std::fs::read_to_string(path).map_err(|e| format!("token file {}: {e}", path.display()))?;
    let t = t.trim().to_string();
    if t.is_empty() { Err(format!("token file {} is empty", path.display())) } else { Ok(t) }
}

/// Ask the service for one scene and return the dithered bitmap. Errors never
/// carry the token.
pub async fn generate(cfg: &ArtConfig, scene: &Scene, seed: u64) -> Result<ArtImage, String> {
    let url = cfg.url.as_deref().ok_or("art.url is not set")?;
    let token = read_token(&cfg.token_file)?;
    let http = reqwest::Client::builder().timeout(StdDuration::from_secs(30)).build().map_err(|e| e.to_string())?;
    let resp = http
        .post(format!("{}/v1/generate", url.trim_end_matches('/')))
        .bearer_auth(token)
        .json(&json!({"prompt": scene.prompt, "negative_prompt": NEGATIVE, "seed": seed % (1 << 31), "steps": STEPS}))
        .send()
        .await
        .map_err(|e| format!("art request: {}", e.without_url()))?;
    if !resp.status().is_success() {
        return Err(format!("art service answered {}", resp.status()));
    }
    let bytes = resp.bytes().await.map_err(|e| format!("art body: {}", e.without_url()))?;
    if bytes.len() > 8 << 20 {
        return Err("art image too large".into());
    }
    Ok(ArtImage { bitmap: png_to_bitmap(&bytes)?, caption: scene.caption.clone(), png: bytes.to_vec() })
}

// ---- scheduling ------------------------------------------------------------

/// When art may be made, and when activity takes it down. Pure: callers pass
/// the clock. Persist `last_art` with [`load_last_art`] / [`save_last_art`] so
/// a restart does not start a new hour.
#[derive(Debug, Clone)]
pub struct ArtSchedule {
    last_activity: DateTime<Utc>,
    last_art: Option<DateTime<Utc>>,
    retry_at: Option<DateTime<Utc>>,
    pub showing: bool,
    pub in_flight: bool,
    epoch: u64,
}

impl ArtSchedule {
    pub fn new(now: DateTime<Utc>, last_art: Option<DateTime<Utc>>) -> Self {
        Self { last_activity: now, last_art, retry_at: None, showing: false, in_flight: false, epoch: 0 }
    }

    pub fn last_art(&self) -> Option<DateTime<Utc>> {
        self.last_art
    }

    /// Anything happened (an event, a card). Returns true when art was on
    /// the panel and the status must be redrawn. A generation in flight is
    /// now stale.
    pub fn activity(&mut self, now: DateTime<Utc>) -> bool {
        self.last_activity = now;
        self.epoch += 1;
        std::mem::take(&mut self.showing)
    }

    /// Whether to start generating. `quiet` is the caller's view: art is
    /// configured, the agent is idle, no card is up, no link problem.
    pub fn due(&self, now: DateTime<Utc>, quiet: bool) -> bool {
        quiet
            && !self.in_flight
            && now - self.last_activity >= IDLE_AFTER
            && self.last_art.is_none_or(|t| now - t >= MIN_GAP)
            && self.retry_at.is_none_or(|t| now >= t)
    }

    /// Start a generation; the token comes back with its result.
    pub fn begin(&mut self) -> u64 {
        self.in_flight = true;
        self.epoch
    }

    /// Whether a finished generation is still wanted (nothing happened since).
    pub fn accept(&mut self, token: u64) -> bool {
        self.in_flight = false;
        token == self.epoch
    }

    pub fn failed(&mut self, now: DateTime<Utc>) {
        self.in_flight = false;
        self.retry_at = Some(now + RETRY_AFTER);
    }

    /// The image went out to the panel.
    pub fn shown(&mut self, now: DateTime<Utc>) {
        self.showing = true;
        self.last_art = Some(now);
    }
}

pub fn load_last_art(path: &Path) -> Option<DateTime<Utc>> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    DateTime::from_timestamp(v["last_art_at"].as_i64()?, 0)
}

/// Remember when art last went up (and its caption, for the phone). Owner-only.
pub fn save_last_art(path: &Path, at: DateTime<Utc>, caption: &str) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = crate::glance_store::write_private(path, json!({"last_art_at": at.timestamp(), "caption": caption}).to_string().as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-25T09:00:00Z").unwrap().with_timezone(&Utc)
    }

    fn fnv(b: &[u8]) -> u64 {
        b.iter().fold(0xcbf2_9ce4_8422_2325, |h, &x| (h ^ x as u64).wrapping_mul(0x100_0000_01b3))
    }

    #[test]
    fn every_scene_fits_the_caption_and_carries_the_style() {
        for (_, base) in SUBJECTS {
            for (_, when) in WHENS {
                let c = format!("{base} {when}");
                assert!(c.is_ascii() && c.len() <= MAX_CAPTION, "{c} ({})", c.len());
            }
        }
        let s = scene(7);
        assert!(s.prompt.ends_with(STYLE) && s.caption.len() <= MAX_CAPTION);
        assert_eq!(scene(7), s, "deterministic");
        let distinct: std::collections::BTreeSet<_> = (0..200).map(|i| scene(i).caption).collect();
        assert!(distinct.len() > 20, "{}", distinct.len());
    }

    #[test]
    fn pack_is_msb_first_with_zero_padding() {
        let mut bits = vec![false; W * H];
        bits[0] = true; // row 0, x 0 -> 0x80
        bits[7] = true; // row 0, x 7 -> 0x01
        bits[W - 1] = true; // row 0, x 249 -> byte 31, bit 1 -> 0x40
        bits[W + 8] = true; // row 1, x 8 -> byte 1 of row 1 -> 0x80
        let p = pack(&bits);
        assert_eq!(p.len(), 3904);
        assert_eq!((p[0], p[31], p[ROW_BYTES + 1]), (0x81, 0x40, 0x80));
        assert_eq!(p.iter().map(|b| b.count_ones()).sum::<u32>(), 4);
    }

    #[test]
    fn dither_golden() {
        // A horizontal ramp: black on the left to white on the right.
        let gray: Vec<u8> = (0..W * H).map(|i| ((i % W) * 255 / (W - 1)) as u8).collect();
        let bits = atkinson(&gray);
        let packed = pack(&bits);
        assert_eq!(packed.len(), IMG_BYTES);
        let ink = |x0: usize, x1: usize| (0..H).map(|y| (x0..x1).filter(|&x| bits[y * W + x]).count()).sum::<usize>();
        assert!(ink(0, 25) > 9 * ink(225, 250), "ramp should be dark left, light right");
        assert_eq!(ink(0, 1), H, "pure black stays black");
        assert_eq!(ink(W - 1, W), 0, "pure white stays white");
        for y in 0..H {
            assert_eq!(packed[y * ROW_BYTES + 31] & 0x3f, 0, "pad bits are zero");
        }
        assert_eq!(fnv(&packed), GOLDEN_RAMP, "dither output changed: {:#x}", fnv(&packed));
    }

    const GOLDEN_RAMP: u64 = 0x00ad_b9c0_6fd6_5640;

    #[test]
    fn centre_crop_takes_the_middle_band() {
        // 512x512 with the top and bottom thirds black and the middle white.
        let src: Vec<u8> = (0..512 * 512).map(|i| if (170..342).contains(&(i / 512)) { 255 } else { 0 }).collect();
        let fit = fit_gray(&src, 512, 512);
        assert_eq!(fit.len(), W * H);
        // The crop is 512 x 250 rows centred at 131..381: the white band covers it mostly.
        assert!(fit[(H / 2) * W + W / 2] == 255);
        let mean = fit.iter().map(|&b| b as u32).sum::<u32>() / (W * H) as u32;
        assert!(mean > 150, "{mean}");
    }

    #[test]
    fn png_round_trip_to_the_expected_size() {
        let mut png_bytes = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut png_bytes, 64, 64);
            enc.set_color(png::ColorType::Rgb);
            enc.set_depth(png::BitDepth::Eight);
            let mut w = enc.write_header().unwrap();
            let data: Vec<u8> = (0..64 * 64).flat_map(|i| if (i / 64) < 32 { [0, 0, 0] } else { [255, 255, 255] }).collect();
            w.write_image_data(&data).unwrap();
        }
        let bm = png_to_bitmap(&png_bytes).unwrap();
        assert_eq!(bm.len(), IMG_BYTES);
        assert_eq!(bm[0], 0xff, "black top");
        assert_eq!(bm[IMG_BYTES - 1], 0x00, "white bottom");
        assert!(png_to_bitmap(b"not a png").is_err());
    }

    #[test]
    fn image_line_validates_and_bad_ones_do_not() {
        let img = ArtImage { bitmap: vec![0xAA; IMG_BYTES], caption: "the loom at dusk".into(), png: vec![] };
        let line = img.to_line("09:15");
        assert!(line.is_ascii() && line.ends_with('\n') && line.matches('\n').count() == 1);
        validate_line(&line).unwrap();
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!((v["w"].as_u64(), v["h"].as_u64(), v["v"].as_u64()), (Some(250), Some(122), Some(1)));
        assert_eq!(v["img"].as_str().unwrap().len(), 5208);
        let mut short = v.clone();
        short["img"] = json!(base64::engine::general_purpose::STANDARD.encode(vec![0u8; 3000]));
        assert!(validate_line(&short.to_string()).unwrap_err().contains("3000"));
        let mut wrong = v.clone();
        wrong["w"] = json!(128);
        assert!(validate_line(&wrong.to_string()).is_err());
        let mut cap = v.clone();
        cap["caption"] = json!("x".repeat(25));
        assert!(validate_line(&cap.to_string()).is_err());
        let mut ver = v;
        ver["v"] = json!(2);
        assert!(validate_line(&ver.to_string()).is_err());
        assert!(validate_line("{nope").is_err());
        // The caption is clipped to ASCII <= 24 when the line is built.
        let long = ArtImage { bitmap: vec![0; IMG_BYTES], caption: format!("caf\u{e9} {}", "z".repeat(40)), png: vec![] };
        validate_line(&long.to_line("09:15")).unwrap();
    }

    #[test]
    fn schedule_waits_for_quiet_and_caps_at_one_an_hour() {
        let mut s = ArtSchedule::new(t0(), None);
        assert!(!s.due(t0() + Duration::minutes(14), true), "not idle long enough");
        assert!(!s.due(t0() + Duration::minutes(15), false), "caller says not quiet (card, busy, unconfigured)");
        assert!(s.due(t0() + Duration::minutes(15), true));
        let tok = s.begin();
        assert!(!s.due(t0() + Duration::minutes(16), true), "one request at a time");
        assert!(s.accept(tok));
        s.shown(t0() + Duration::minutes(16));
        assert!(s.showing);
        assert!(!s.due(t0() + Duration::minutes(75), true), "under an hour since the last art");
        assert!(s.due(t0() + Duration::minutes(76), true));
    }

    #[test]
    fn activity_preempts_and_stales_a_generation() {
        let mut s = ArtSchedule::new(t0(), None);
        let tok = s.begin();
        assert!(!s.activity(t0() + Duration::minutes(16)), "nothing on the panel yet");
        assert!(!s.accept(tok), "a result that arrives after activity is dropped");
        assert!(!s.due(t0() + Duration::minutes(20), true), "idle clock restarted");
        s.shown(t0() + Duration::minutes(40));
        assert!(s.activity(t0() + Duration::minutes(41)), "art was up: redraw status");
        assert!(!s.showing && !s.activity(t0() + Duration::minutes(42)));
    }

    #[test]
    fn failure_backs_off_and_last_art_survives_a_restart() {
        let mut s = ArtSchedule::new(t0(), None);
        s.begin();
        s.failed(t0() + Duration::minutes(15));
        assert!(!s.due(t0() + Duration::minutes(24), true));
        assert!(s.due(t0() + Duration::minutes(25), true));
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sub/art.json");
        assert_eq!(load_last_art(&p), None);
        save_last_art(&p, t0(), "the loom");
        let restarted = ArtSchedule::new(t0() + Duration::minutes(1), load_last_art(&p));
        assert!(!restarted.due(t0() + Duration::minutes(30), true), "restart does not reset the hour");
        assert!(restarted.due(t0() + Duration::minutes(61), true));
    }

    #[cfg(unix)]
    #[test]
    fn token_file_must_be_private_and_is_not_echoed() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("tok");
        std::fs::write(&p, "s3cret-token\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        let e = read_token(&p).unwrap_err();
        assert!(e.contains("mode 600") && !e.contains("s3cret"), "{e}");
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(read_token(&p).unwrap(), "s3cret-token");
        assert!(read_token(&dir.path().join("missing")).is_err());
    }
}
