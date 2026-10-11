//! Push-to-talk on the CrowPanel DIS08070H (2026-10-11 wiring decision).
//!
//! Button: IO44 (UART0 RXD0 on header J10), active-low with the internal
//! pull-up, to GND when pressed. The serial console becomes TX-only at
//! runtime; flashing still works as long as the button is not held. A
//! 20 ms debounce turns the level into `ptt.down` / `ptt.up` events —
//! today a log line and the [`PTT_DOWN`] flag, later a sensor event on
//! the leaf channel (see "Design" below).
//!
//! Microphones (feature `ptt-mic`): two INMP441 on one I²S standard-mode
//! bus, stereo 32-bit slots at 16 kHz. Both share SD → IO38 (an INMP441
//! tri-states SD outside its slot; L/R → GND is the left slot, L/R → 3V3
//! the right), SCK ← IO42 and WS ← IO18 tapped at the amp's 0R links
//! R19 / R12 (the NS4168 is the I2S0 TX side; a later full-duplex driver
//! shares BCLK/WS with it). The INMP441 puts 24 bits left-justified in
//! the 32-bit slot; samples are `i32 >> 8`. Capture runs only while the
//! button is held and logs per-channel RMS every 500 ms so each mic can
//! be verified after wiring. 3.3 V only: VDD from J7/J6 3V3.
//!
//! Design for streaming (not built): while held, 16 kHz × 2 ch × 16-bit
//! = 64 kB/s; the leaf channel's 32 KiB signed-frame bound means ~250 ms
//! chunks, sequence-numbered, published under `mesh.leaf.<id>.audio`
//! with `ptt.down`/`ptt.up` as the start/stop markers. Opus or ADPCM
//! before the mesh is the obvious next step.

use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use esp_idf_hal::gpio::{Input, PinDriver};
use log::info;

/// True while the push-to-talk button is held (debounced).
pub static PTT_DOWN: AtomicBool = AtomicBool::new(false);

/// Debounce: sample every 5 ms, require 4 identical samples (20 ms).
pub fn spawn_button(pin: PinDriver<'static, Input>) {
    let _ = thread::Builder::new().name("ptt-btn".into()).stack_size(3072).spawn(move || {
        let mut stable = pin.is_low();
        let mut candidate = stable;
        let mut run = 0u8;
        info!("[ptt] IO44 (J10 RXD0) button watcher up; initial {}", if stable { "LOW (held?)" } else { "HIGH" });
        loop {
            let now = pin.is_low();
            if now == candidate {
                run = run.saturating_add(1);
            } else {
                candidate = now;
                run = 1;
            }
            if run >= 4 && candidate != stable {
                stable = candidate;
                PTT_DOWN.store(stable, Ordering::Relaxed);
                info!("[ptt] {}", if stable { "ptt.down" } else { "ptt.up" });
            }
            thread::sleep(Duration::from_millis(5));
        }
    });
}

#[cfg(feature = "ptt-mic")]
pub mod mic {
    use super::PTT_DOWN;
    use esp_idf_hal::gpio::{AnyIOPin, Gpio18, Gpio38, Gpio42};
    use esp_idf_hal::i2s::config::{DataBitWidth, StdConfig};
    use esp_idf_hal::i2s::{I2sDriver, I2S0};
    use log::{info, warn};
    use std::sync::atomic::Ordering;
    use std::thread;
    use std::time::Duration;

    pub const SAMPLE_RATE_HZ: u32 = 16_000;
    /// 100 ms of stereo 32-bit frames per read.
    const FRAMES_PER_READ: usize = 1600;
    const BYTES_PER_READ: usize = FRAMES_PER_READ * 2 * 4;

    pub fn spawn(i2s: I2S0<'static>, bclk: Gpio42<'static>, ws: Gpio18<'static>, din: Gpio38<'static>) {
        let _ = thread::Builder::new().name("ptt-mic".into()).stack_size(8192).spawn(move || {
            let cfg = StdConfig::philips(SAMPLE_RATE_HZ, DataBitWidth::Bits32);
            let mut drv = match I2sDriver::new_std_rx(i2s, &cfg, bclk, din, None::<AnyIOPin>, ws) {
                Ok(d) => d,
                Err(e) => {
                    warn!("[ptt-mic] I2S0 RX init failed: {e}");
                    return;
                }
            };
            info!("[ptt-mic] I2S0 RX ready: SCK=IO42 WS=IO18 SD=IO38, 16 kHz stereo 32-bit slots; capturing while ptt.down");
            let mut buf = vec![0u8; BYTES_PER_READ];
            let mut enabled = false;
            let (mut acc_l, mut acc_r, mut n, mut blocks) = (0f64, 0f64, 0usize, 0u32);
            loop {
                if !PTT_DOWN.load(Ordering::Relaxed) {
                    if enabled {
                        let _ = drv.rx_disable();
                        enabled = false;
                        info!("[ptt-mic] capture stopped");
                    }
                    thread::sleep(Duration::from_millis(20));
                    continue;
                }
                if !enabled {
                    if let Err(e) = drv.rx_enable() {
                        warn!("[ptt-mic] rx_enable failed: {e}");
                        thread::sleep(Duration::from_millis(500));
                        continue;
                    }
                    enabled = true;
                    (acc_l, acc_r, n, blocks) = (0.0, 0.0, 0, 0);
                    info!("[ptt-mic] capture started");
                }
                let got = match drv.read(&mut buf, 200) {
                    Ok(g) => g,
                    Err(e) => {
                        warn!("[ptt-mic] read failed: {e}");
                        continue;
                    }
                };
                for frame in buf[..got].chunks_exact(8) {
                    // 24-bit left-justified in the 32-bit slot.
                    let l = i32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]) >> 8;
                    let r = i32::from_le_bytes([frame[4], frame[5], frame[6], frame[7]]) >> 8;
                    acc_l += (l as f64) * (l as f64);
                    acc_r += (r as f64) * (r as f64);
                    n += 1;
                }
                blocks += 1;
                if blocks >= 5 && n > 0 {
                    let full = 8_388_607f64; // 2^23 - 1
                    let rms_l = (acc_l / n as f64).sqrt();
                    let rms_r = (acc_r / n as f64).sqrt();
                    let db = |v: f64| if v > 0.0 { 20.0 * (v / full).log10() } else { -144.0 };
                    info!("[ptt-mic] rms L={:.0} ({:.1} dBFS)  R={:.0} ({:.1} dBFS)  over {} frames", rms_l, db(rms_l), rms_r, db(rms_r), n);
                    (acc_l, acc_r, n, blocks) = (0.0, 0.0, 0, 0);
                }
            }
        });
    }
}
