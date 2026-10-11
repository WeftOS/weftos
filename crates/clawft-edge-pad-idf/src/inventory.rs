//! Boot-time hardware inventory for the CrowPanel DIS08070H (2026-10-10).
//!
//! Everything here is read-only and logged: an I²C scan of the one wired
//! bus (GPIO19 SDA / GPIO20 SCL — the V3.0 schematic routes no other I²C
//! bus), ID-register reads for the parts that answer, a PCA9557 register
//! dump against the schematic's pin names, the chip temperature sensor,
//! the Wi-Fi RSSI once associated, a 5 s BLE advertise + scan (no
//! pairing), and a level watcher on IO38_D, the one free GPIO on the J7
//! "GPIO D" header. Schematic: `CrowPanel ESP32 Display-7.0-inch-V3.0-20240420.pdf`.

use std::sync::atomic::{AtomicU32, Ordering};

use esp_idf_hal::i2c::I2cDriver;
use esp_idf_hal::temp_sensor::{TempSensor, TempSensorConfig, TempSensorDriver};
use log::{info, warn};

/// I²C transaction timeout in FreeRTOS ticks (1 kHz tick → ms).
const T: u32 = 50;

/// Probe every 7-bit address with a one-byte read (a NACK is "absent").
pub fn i2c_scan(i2c: &mut I2cDriver<'_>) -> Vec<u8> {
    let mut found = Vec::new();
    for addr in 0x03u8..=0x77 {
        let mut b = [0u8; 1];
        if i2c.read(addr, &mut b, T).is_ok() {
            found.push(addr);
        }
    }
    let list: Vec<String> = found.iter().map(|a| format!("0x{a:02x}")).collect();
    info!("[inventory] I2C0 (SDA=GPIO19, SCL=GPIO20, 400 kHz) responders: [{}]", list.join(", "));
    found
}

/// Expander pins per the V3.0 schematic (IC1, address 0x18).
const PCA9557_PINS: [&str; 8] = [
    "IO0 TP-RESET (GT911 reset)",
    "IO1 INTE (GT911 interrupt, input)",
    "IO2 LCD-RESET (panel reset via R46)",
    "IO3 LCD-POWER-C (panel TFT-3V3 gate, Q6/Q4)",
    "IO4 not connected",
    "IO5 not connected",
    "IO6 not connected",
    "IO7 not connected",
];

/// Read ID registers for whatever answered. Only reads; nothing is written
/// but the register pointer.
pub fn i2c_identify(i2c: &mut I2cDriver<'_>, found: &[u8]) {
    for &a in found {
        match a {
            0x5D | 0x14 => {
                let mut id = [0u8; 4];
                let mut cfg = [0u8; 1];
                let ok = i2c.write_read(a, &[0x81, 0x40], &mut id, T).is_ok()
                    && i2c.write_read(a, &[0x80, 0x47], &mut cfg, T).is_ok();
                if ok {
                    info!(
                        "[inventory] 0x{a:02x}: GT911 touch controller, product id {:?}, config version 0x{:02x}",
                        core::str::from_utf8(&id).unwrap_or("?").trim_end_matches('\0'),
                        cfg[0]
                    );
                } else {
                    warn!("[inventory] 0x{a:02x}: GT911-range address but PRODUCT_ID read failed");
                }
            }
            0x18 => {
                let mut regs = [0u8; 4];
                let mut ok = true;
                for r in 0..4u8 {
                    ok &= i2c.write_read(0x18, &[r], &mut regs[r as usize..r as usize + 1], T).is_ok();
                }
                if !ok {
                    warn!("[inventory] 0x18: register read failed");
                    continue;
                }
                let [input, output, polarity, config] = regs;
                info!(
                    "[inventory] 0x18: PCA9557 I/O expander — input=0x{input:02x} output=0x{output:02x} polarity=0x{polarity:02x} config=0x{config:02x} (config bit 1 = input)"
                );
                for (bit, name) in PCA9557_PINS.iter().enumerate() {
                    let m = 1u8 << bit;
                    info!(
                        "[inventory]   {name}: {} level={}",
                        if config & m != 0 { "input " } else { "output" },
                        if config & m != 0 { (input & m != 0) as u8 } else { (output & m != 0) as u8 }
                    );
                }
            }
            0x68 | 0x69 => {
                // MPU6050/ICM-class WHO_AM_I at 0x75; DS3231 RTC status at 0x0F.
                let mut who = [0u8; 1];
                let mut st = [0u8; 1];
                let w = i2c.write_read(a, &[0x75], &mut who, T).is_ok();
                let s = i2c.write_read(a, &[0x0F], &mut st, T).is_ok();
                info!("[inventory] 0x{a:02x}: unexpected device; WHO_AM_I(0x75)={} status(0x0F)={}",
                    if w { format!("0x{:02x}", who[0]) } else { "nack".into() },
                    if s { format!("0x{:02x}", st[0]) } else { "nack".into() });
            }
            0x6A | 0x6B | 0x19 | 0x1E | 0x1C => {
                let mut who = [0u8; 1];
                let w = i2c.write_read(a, &[0x0F], &mut who, T).is_ok();
                info!("[inventory] 0x{a:02x}: unexpected device; WHO_AM_I(0x0F)={}",
                    if w { format!("0x{:02x}", who[0]) } else { "nack".into() });
            }
            0x36 => {
                let mut v = [0u8; 2];
                let w = i2c.write_read(a, &[0x08], &mut v, T).is_ok();
                info!("[inventory] 0x36: unexpected device; MAX1704x VERSION(0x08)={}",
                    if w { format!("0x{:02x}{:02x}", v[0], v[1]) } else { "nack".into() });
            }
            0x51 => {
                let mut s = [0u8; 1];
                let w = i2c.write_read(a, &[0x02], &mut s, T).is_ok();
                info!("[inventory] 0x51: unexpected device; PCF8563 seconds(0x02)={}",
                    if w { format!("0x{:02x}", s[0]) } else { "nack".into() });
            }
            _ => info!("[inventory] 0x{a:02x}: unexpected device; no safe ID probe attempted"),
        }
    }
}

/// Die temperature from the built-in sensor, one reading.
pub fn chip_temperature(sensor: TempSensor<'static>) {
    match TempSensorDriver::new(&TempSensorConfig::default(), sensor) {
        Ok(mut d) => match d.enable().and_then(|_| d.get_celsius()) {
            Ok(c) => info!("[inventory] chip temperature sensor: {c:.1} °C"),
            Err(e) => warn!("[inventory] chip temperature read failed: {e}"),
        },
        Err(e) => warn!("[inventory] chip temperature sensor unavailable: {e}"),
    }
}

static BLE_SEEN: AtomicU32 = AtomicU32::new(0);

/// Advertise as `clawft-edge-pad` and run an active scan for `secs`,
/// logging every advertiser (address, RSSI, name). No connections, no
/// pairing. Runs on the caller's thread via `block_on`.
pub fn ble_probe(secs: u32) {
    use esp32_nimble::{BLEAdvertisementData, BLEDevice, BLEScan};
    esp_idf_hal::task::block_on(async {
        let dev = BLEDevice::take();
        let adv = dev.get_advertising();
        let mut data = BLEAdvertisementData::new();
        data.name("clawft-edge-pad");
        match adv.lock().set_data(&mut data).and_then(|_| adv.lock().start()) {
            Ok(()) => info!("[inventory] BLE advertising as 'clawft-edge-pad'"),
            Err(e) => warn!("[inventory] BLE advertising failed: {e:?}"),
        }
        let mut scan = BLEScan::new();
        scan.active_scan(true).interval(100).window(99);
        BLE_SEEN.store(0, Ordering::Relaxed);
        let r = scan
            .start(dev, (secs * 1000) as i32, |device, data| {
                BLE_SEEN.fetch_add(1, Ordering::Relaxed);
                info!("[inventory] BLE adv: addr={} rssi={} name={:?}", device.addr(), device.rssi(), data.name());
                None::<()>
            })
            .await;
        match r {
            Ok(_) => info!("[inventory] BLE scan done: {} advertisements in {secs} s", BLE_SEEN.load(Ordering::Relaxed)),
            Err(e) => warn!("[inventory] BLE scan failed: {e:?}"),
        }
        let _ = adv.lock().stop();
    });
}

