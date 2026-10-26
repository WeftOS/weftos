//! Best-effort USB inventory for the console's "Identify hardware" modal (`GET /hw/usb`).
//!
//! Linux reads `/sys/bus/usb/devices`; macOS parses `system_profiler SPUSBHostDataType -json` (falling
//! back to the older `SPUSBDataType`). Each device gets a stable key, is diffed against an operator
//! baseline (`<root>/hw/usb-baseline.json`) and labelled from the bundled id table. Serial numbers
//! never leave this module in full: keys carry a short hash and the report shows the last 4 chars.

use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use weftos_cog_market::usb::UsbIdTable;
use weftos_cog_repo::sha256_hex;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct UsbDevice {
    pub key: String,
    pub vid: u16,
    pub pid: u16,
    pub manufacturer: String,
    pub product: String,
    pub serial: Option<String>,
    pub class: String,
    pub speed: String,
    pub bus_path: String,
    pub ports: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Scan {
    pub devices: Vec<UsbDevice>,
    /// Serial ports we saw but could not attach to a device (macOS matching is best-effort).
    pub unmatched_ports: Vec<String>,
}

/// Stable device key: `vid:pid:s-<16 hex of sha256(host salt | serial)>`, or `vid:pid:p-<bus path>`
/// without a serial. The salt is per host (`<root>/hw/salt`, random, 0600), so neither the key nor a
/// hash of it lets anyone recompute or correlate a serial number across hosts.
pub fn device_key(vid: u16, pid: u16, serial: Option<&str>, bus_path: &str, salt: &str) -> String {
    match serial.filter(|s| !s.is_empty()) {
        Some(s) => format!("{vid:04x}:{pid:04x}:s-{}", &sha256_hex(format!("{salt}|{s}").as_bytes())[..16]),
        None => format!("{vid:04x}:{pid:04x}:p-{bus_path}"),
    }
}

/// The per-host salt, created on first use (random, 0600, atomic).
pub fn load_salt(root: &Path) -> String {
    let p = root.join("hw").join("salt");
    if let Some(s) = std::fs::read_to_string(&p).ok().map(|s| s.trim().to_string()).filter(|s| s.len() >= 32) {
        return s;
    }
    let s = crate::auth::random_hex(32).unwrap_or_else(|_| sha256_hex(format!("{}{:?}", std::process::id(), Instant::now()).as_bytes()));
    let _ = crate::auth::write_private(&p, s.as_bytes());
    // another thread may have won the race; read back what is on disk
    std::fs::read_to_string(&p).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).unwrap_or(s)
}

/// macOS serial-port names embed the USB serial (`cu.usbmodemA1B2C3D4501`); anything leaving the
/// host shows only `…` plus the last 4 characters of that suffix. Short suffixes (location ids such
/// as `wchusbserial1410`) and Linux `ttyUSB0`/`ttyACM0` names carry no serial and pass through.
pub fn redact_port(p: &str) -> String {
    let (dir, name) = p.rsplit_once('/').map(|(d, n)| (format!("{d}/"), n)).unwrap_or((String::new(), p));
    for prefix in ["cu.usbmodem", "cu.usbserial-", "cu.usbserial", "cu.wchusbserial", "cu.SLAB_USBtoUART", "tty.usbmodem", "tty.usbserial-", "tty.usbserial", "tty.wchusbserial", "tty.SLAB_USBtoUART"] {
        if let Some(tail) = name.strip_prefix(prefix) {
            let n = tail.chars().count();
            if n > 4 {
                return format!("{dir}{prefix}…{}", tail.chars().skip(n - 4).collect::<String>());
            }
            return p.to_string();
        }
    }
    p.to_string()
}

/// Show only the last 4 characters (`…1234`); short serials are fully masked.
pub fn redact_serial(s: &str) -> String {
    let n = s.chars().count();
    if n <= 4 {
        return "****".into();
    }
    format!("…{}", s.chars().skip(n - 4).collect::<String>())
}

fn class_name(code: u8) -> &'static str {
    match code {
        0x00 => "per-interface",
        0x01 => "audio",
        0x02 => "communications",
        0x03 => "hid",
        0x08 => "storage",
        0x09 => "hub",
        0x0e => "video",
        0xef => "miscellaneous",
        0xfe => "application-specific",
        0xff => "vendor-specific",
        _ => "other",
    }
}

// ---- scanning ---------------------------------------------------------------------------------

/// Scan the bus. Results are reused for 2 s (the macOS `system_profiler` call is slow), so
/// hammering the endpoints cannot hammer the system.
pub fn scan(salt: &str) -> Scan {
    static CACHE: Mutex<Option<(Instant, String, Scan)>> = Mutex::new(None);
    let mut c = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, s, scan)) = c.as_ref()
        && s == salt
        && at.elapsed() < Duration::from_secs(2)
    {
        return scan.clone();
    }
    let fresh = scan_uncached(salt);
    *c = Some((Instant::now(), salt.to_string(), fresh.clone()));
    fresh
}

fn scan_uncached(salt: &str) -> Scan {
    #[cfg(target_os = "linux")]
    {
        return scan_sysfs(Path::new("/sys/bus/usb/devices"), Path::new("/dev"), salt);
    }
    #[cfg(target_os = "macos")]
    {
        return scan_macos(salt);
    }
    #[allow(unreachable_code)]
    {
        let _ = salt;
        Scan::default()
    }
}

fn read_trim(p: &Path) -> String {
    std::fs::read_to_string(p).map(|s| s.trim().to_string()).unwrap_or_default()
}

fn hex_attr(p: &Path) -> Option<u16> {
    u16::from_str_radix(&read_trim(p), 16).ok()
}

/// Linux: walk `<sysfs>/<bus-port>` device dirs; tty names come from their interface dirs
/// (`<dev>:1.0/ttyUSB0` or `<dev>:1.0/tty/ttyACM0`) and map to `<dev_root>/<name>`.
pub fn scan_sysfs(sysfs: &Path, dev_root: &Path, salt: &str) -> Scan {
    let mut devices = Vec::new();
    let Ok(rd) = std::fs::read_dir(sysfs) else { return Scan::default() };
    let mut names: Vec<String> = rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    for name in &names {
        // `usbN` are root hubs; `N-P:C.I` are interfaces, handled with their parent device.
        if name.starts_with("usb") || name.contains(':') {
            continue;
        }
        let d = sysfs.join(name);
        let (Some(vid), Some(pid)) = (hex_attr(&d.join("idVendor")), hex_attr(&d.join("idProduct"))) else { continue };
        let serial = Some(read_trim(&d.join("serial"))).filter(|s| !s.is_empty());
        let class = u8::from_str_radix(&read_trim(&d.join("bDeviceClass")), 16).map(class_name).unwrap_or("").to_string();
        let speed = Some(read_trim(&d.join("speed"))).filter(|s| !s.is_empty()).map(|s| format!("{s} Mb/s")).unwrap_or_default();
        let mut ports = Vec::new();
        let prefix = format!("{name}:");
        for iface in names.iter().filter(|n| n.starts_with(&prefix)) {
            for base in [sysfs.join(iface), sysfs.join(iface).join("tty")] {
                let Ok(rd) = std::fs::read_dir(&base) else { continue };
                for e in rd.flatten() {
                    let n = e.file_name().to_string_lossy().into_owned();
                    if n.starts_with("ttyUSB") || n.starts_with("ttyACM") {
                        ports.push(dev_root.join(&n).display().to_string());
                    }
                }
            }
        }
        ports.sort();
        ports.dedup();
        devices.push(UsbDevice {
            key: device_key(vid, pid, serial.as_deref(), name, salt),
            vid,
            pid,
            manufacturer: read_trim(&d.join("manufacturer")),
            product: read_trim(&d.join("product")),
            serial,
            class,
            speed,
            bus_path: name.clone(),
            ports,
        });
    }
    Scan { devices, unmatched_ports: Vec::new() }
}

#[cfg(target_os = "macos")]
fn scan_macos(salt: &str) -> Scan {
    use std::process::Command;
    let ports = mac_serial_ports(Path::new("/dev"));
    for dt in ["SPUSBHostDataType", "SPUSBDataType"] {
        let Ok(out) = Command::new("system_profiler").args([dt, "-json"]).output() else { continue };
        let Ok(v) = serde_json::from_slice::<Value>(&out.stdout) else { continue };
        let scan = parse_system_profiler(&v, &ports, salt);
        if !scan.devices.is_empty() {
            return scan;
        }
    }
    Scan { devices: Vec::new(), unmatched_ports: ports }
}

/// `/dev/cu.usb*`, `cu.wchusbserial*`, `cu.SLAB_USBtoUART*`.
pub fn mac_serial_ports(dev_root: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dev_root)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("cu.usb") || n.starts_with("cu.wchusbserial") || n.starts_with("cu.SLAB_USBtoUART"))
        .map(|n| dev_root.join(n).display().to_string())
        .collect();
    v.sort();
    v
}

fn parse_hex_prefix(s: &str) -> Option<u16> {
    let t = s.split_whitespace().next()?;
    u16::from_str_radix(t.trim_start_matches("0x"), 16).ok()
}

fn str_of<'a>(o: &'a Value, keys: &[&str]) -> &'a str {
    keys.iter().find_map(|k| o.get(*k).and_then(|v| v.as_str())).unwrap_or("")
}

/// Does serial port `port` plausibly belong to this device? Serial substring first, then the
/// location-id digits (how `wchusbserial*` ports are named).
fn port_matches(port: &str, serial: Option<&str>, location: &str) -> bool {
    let base = port.rsplit('/').next().unwrap_or(port).to_lowercase();
    if let Some(s) = serial.filter(|s| s.len() >= 4)
        && base.contains(&s.to_lowercase())
    {
        return true;
    }
    let loc = location.split_whitespace().next().unwrap_or("").trim_start_matches("0x").to_lowercase();
    let loc = loc.trim_end_matches('0');
    loc.len() >= 3 && base.ends_with(loc)
}

fn collect_mac(v: &Value, out: &mut Vec<(UsbDevice, String)>, salt: &str) {
    let Some(items) = v.as_array() else { return };
    for it in items {
        let vid_s = str_of(it, &["USBDeviceKeyVendorID", "vendor_id"]);
        if let (Some(vid), Some(pid)) = (parse_hex_prefix(vid_s), parse_hex_prefix(str_of(it, &["USBDeviceKeyProductID", "product_id"]))) {
            let serial = Some(str_of(it, &["USBDeviceKeySerialNumber", "serial_num"]).to_string())
                .filter(|s| !s.is_empty() && s != "Not Provided");
            let location = str_of(it, &["USBKeyLocationID", "location_id"]).to_string();
            let bus_path = location.split_whitespace().next().unwrap_or("").to_string();
            out.push((
                UsbDevice {
                    key: device_key(vid, pid, serial.as_deref(), &bus_path, salt),
                    vid,
                    pid,
                    manufacturer: str_of(it, &["USBDeviceKeyVendorName", "manufacturer"]).to_string(),
                    product: str_of(it, &["_name"]).to_string(),
                    serial,
                    class: String::new(),
                    speed: str_of(it, &["USBDeviceKeyLinkSpeed", "device_speed"]).to_string(),
                    bus_path,
                    ports: Vec::new(),
                },
                location,
            ));
        }
        if let Some(children) = it.get("_items") {
            collect_mac(children, out, salt);
        }
    }
}

/// Parse `system_profiler SPUSBHostDataType|SPUSBDataType -json` and attach serial ports.
pub fn parse_system_profiler(v: &Value, serial_ports: &[String], salt: &str) -> Scan {
    let mut found = Vec::new();
    if let Some(obj) = v.as_object() {
        for tree in obj.values() {
            collect_mac(tree, &mut found, salt);
        }
    }
    let mut left: Vec<String> = serial_ports.to_vec();
    let mut devices = Vec::new();
    for (mut d, loc) in found {
        let (mine, rest): (Vec<String>, Vec<String>) = left.drain(..).partition(|p| port_matches(p, d.serial.as_deref(), &loc));
        left = rest;
        d.ports = mine;
        devices.push(d);
    }
    Scan { devices, unmatched_ports: left }
}

// ---- baseline + diff --------------------------------------------------------------------------

pub fn baseline_path(root: &Path) -> PathBuf {
    root.join("hw").join("usb-baseline.json")
}

fn baseline_entry(d: &UsbDevice) -> Value {
    json!({
        "key": d.key,
        "vid": format!("{:04x}", d.vid),
        "pid": format!("{:04x}", d.pid),
        "manufacturer": d.manufacturer,
        "product": d.product,
    })
}

/// Entries of the saved baseline, or `None` when no baseline exists yet.
pub fn load_baseline(root: &Path) -> Option<Vec<Value>> {
    let bytes = std::fs::read(baseline_path(root)).ok()?;
    let v: Value = serde_json::from_slice(&bytes).ok()?;
    Some(v.get("devices")?.as_array()?.clone())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    crate::auth::write_private(path, bytes)
}

static BASELINE_LOCK: Mutex<()> = Mutex::new(());

/// Write the baseline. `keys = None` accepts the whole current scan; `Some(keys)` adds only those
/// currently-present devices to the existing baseline. Returns how many devices it now holds.
pub fn save_baseline(root: &Path, scan: &Scan, keys: Option<&[String]>, now: u64) -> std::io::Result<usize> {
    let _g = BASELINE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut entries: Vec<Value> = match keys {
        None => Vec::new(),
        Some(_) => load_baseline(root).unwrap_or_default(),
    };
    let have: BTreeSet<String> = entries.iter().filter_map(|e| e["key"].as_str().map(String::from)).collect();
    for d in &scan.devices {
        let wanted = keys.is_none_or(|k| k.contains(&d.key));
        if wanted && !have.contains(&d.key) {
            entries.push(baseline_entry(d));
        }
    }
    let doc = json!({ "saved_at": now, "devices": entries });
    atomic_write(&baseline_path(root), doc.to_string().as_bytes())?;
    Ok(entries.len())
}

/// Baseline entries whose device is no longer attached.
pub fn removed(baseline: &[Value], scan: &Scan) -> Vec<Value> {
    let now: BTreeSet<&str> = scan.devices.iter().map(|d| d.key.as_str()).collect();
    baseline.iter().filter(|e| e["key"].as_str().is_some_and(|k| !now.contains(k))).cloned().collect()
}

// ---- report -----------------------------------------------------------------------------------

pub fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn id_json(table: &UsbIdTable, d: &UsbDevice) -> Value {
    match table.lookup_device(d.vid, d.pid, &d.product) {
        // `claimed`: matched on a product string only (spoofable by any device on a shared VID)
        Some(i) => json!({ "name": i.name, "kind": i.kind, "chip": i.chip, "module": i.module, "notes": i.notes, "claimed": i.product.is_some() }),
        None => Value::Null,
    }
}

/// Build the `GET /hw/usb` body.
pub fn report(root: &Path, scan: &Scan, table: &UsbIdTable, node: &str, now: u64) -> Value {
    let baseline = load_baseline(root);
    let known: BTreeSet<String> =
        baseline.iter().flatten().filter_map(|e| e["key"].as_str().map(String::from)).collect();
    let devices: Vec<Value> = scan
        .devices
        .iter()
        .map(|d| {
            json!({
                "key": d.key,
                "vid": format!("{:04x}", d.vid),
                "pid": format!("{:04x}", d.pid),
                "manufacturer": d.manufacturer,
                "product": d.product,
                "serial_redacted": d.serial.as_deref().map(redact_serial),
                "class": d.class,
                "speed": d.speed,
                "bus_path": d.bus_path,
                "ports": d.ports.iter().map(|p| redact_port(p)).collect::<Vec<_>>(),
                "state": if known.contains(&d.key) { "known" } else { "new" },
                "id": id_json(table, d),
            })
        })
        .collect();
    json!({
        "ok": true,
        "node": node,
        "scanned_at": now,
        "baseline_missing": baseline.is_none(),
        "devices": devices,
        "removed": baseline.as_deref().map(|b| removed(b, scan)).unwrap_or_default(),
        "unmatched_ports": scan.unmatched_ports.iter().map(|p| redact_port(p)).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn w(p: &Path, s: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, s).unwrap();
    }

    fn fake_sysfs(root: &Path) {
        // an ESP32 behind a CP210x (ttyUSB0), an ACM board without serial, a root hub
        let d = root.join("1-2");
        for (f, v) in [("idVendor", "10c4"), ("idProduct", "ea60"), ("manufacturer", "Silicon Labs"), ("product", "CP2102 USB to UART Bridge"), ("serial", "0001ABCD1234"), ("bDeviceClass", "00"), ("speed", "12")] {
            w(&d.join(f), &format!("{v}\n"));
        }
        fs::create_dir_all(root.join("1-2:1.0").join("ttyUSB0")).unwrap();
        let a = root.join("1-3.1");
        for (f, v) in [("idVendor", "303a"), ("idProduct", "1001"), ("product", "USB JTAG/serial debug unit"), ("bDeviceClass", "ef"), ("speed", "480")] {
            w(&a.join(f), &format!("{v}\n"));
        }
        fs::create_dir_all(root.join("1-3.1:1.0").join("tty").join("ttyACM0")).unwrap();
        w(&root.join("usb1").join("idVendor"), "1d6b\n");
        w(&root.join("usb1").join("idProduct"), "0002\n");
    }

    #[test]
    fn sysfs_scan_finds_devices_ports_and_skips_root_hubs() {
        let t = tempfile::tempdir().unwrap();
        fake_sysfs(t.path());
        let s = scan_sysfs(t.path(), Path::new("/dev"), "salt");
        assert_eq!(s.devices.len(), 2);
        let cp = s.devices.iter().find(|d| d.vid == 0x10c4).unwrap();
        assert_eq!(cp.ports, vec!["/dev/ttyUSB0"]);
        assert_eq!(cp.speed, "12 Mb/s");
        assert_eq!(cp.product, "CP2102 USB to UART Bridge");
        assert!(cp.key.starts_with("10c4:ea60:s-") && !cp.key.contains("ABCD1234"));
        assert_eq!(cp.key.rsplit("s-").next().unwrap().len(), 16);
        let esp = s.devices.iter().find(|d| d.vid == 0x303a).unwrap();
        assert_eq!(esp.ports, vec!["/dev/ttyACM0"]);
        assert_eq!(esp.key, "303a:1001:p-1-3.1");
        assert_eq!(esp.class, "miscellaneous");
    }

    const MAC_HOST: &str = r#"{"SPUSBHostDataType":[
      {"_name":"USB 4.0 Bus","USBKeyLocationID":"0x08000000"},
      {"_name":"USB 3.1 Bus","_items":[
        {"_name":"USB Single Serial","USBDeviceKeyLinkSpeed":"12 Mb/s","USBDeviceKeyProductID":"0x55d3",
         "USBDeviceKeySerialNumber":"A1B2C3D450","USBDeviceKeyVendorID":"0x1a86","USBKeyLocationID":"0x01100000"}]},
      {"_name":"USB 2 Bus","_items":[{"_name":"USB2.1 Hub","USBDeviceKeyProductID":"0x5411","USBDeviceKeySerialNumber":"Not Provided",
        "USBDeviceKeyVendorID":"0x0bda","USBDeviceKeyVendorName":"Generic","USBKeyLocationID":"0x00120000","_items":[
        {"_name":"STLINK-V3","USBDeviceKeyProductID":"0x3754","USBDeviceKeyVendorID":"0x0483","USBDeviceKeySerialNumber":"003A00","USBKeyLocationID":"0x00110000"}]}]}]}"#;

    #[test]
    fn macos_host_format_parses_nested_devices_and_matches_ports() {
        let v: Value = serde_json::from_str(MAC_HOST).unwrap();
        let ports = vec!["/dev/cu.usbmodemA1B2C3D4501".to_string(), "/dev/cu.usbserial-zzz".to_string()];
        let s = parse_system_profiler(&v, &ports, "salt");
        assert_eq!(s.devices.len(), 3);
        let ch = s.devices.iter().find(|d| d.vid == 0x1a86).unwrap();
        assert_eq!(ch.ports, vec!["/dev/cu.usbmodemA1B2C3D4501"]);
        assert_eq!(ch.speed, "12 Mb/s");
        let hub = s.devices.iter().find(|d| d.vid == 0x0bda).unwrap();
        assert!(hub.serial.is_none() && hub.key == "0bda:5411:p-0x00120000");
        assert_eq!(s.unmatched_ports, vec!["/dev/cu.usbserial-zzz"]);
    }

    #[test]
    fn macos_legacy_format_parses() {
        let v: Value = serde_json::from_str(
            r#"{"SPUSBDataType":[{"_name":"USB31Bus","_items":[{"_name":"FT232R USB UART","vendor_id":"0x0403","product_id":"0x6001",
            "serial_num":"A50285BI","manufacturer":"FTDI","location_id":"0x14100000 / 3","device_speed":"full_speed"}]}]}"#,
        )
        .unwrap();
        let s = parse_system_profiler(&v, &["/dev/cu.usbserial-A50285BI".into()], "salt");
        assert_eq!(s.devices.len(), 1);
        assert_eq!(s.devices[0].manufacturer, "FTDI");
        assert_eq!(s.devices[0].ports.len(), 1);
        assert_eq!(s.devices[0].bus_path, "0x14100000");
    }

    #[test]
    fn keys_are_salted_per_host() {
        let a = device_key(0x10c4, 0xea60, Some("SER123456"), "1-2", "salt-a");
        let b = device_key(0x10c4, 0xea60, Some("SER123456"), "1-2", "salt-b");
        assert_ne!(a, b);
        assert_eq!(a, device_key(0x10c4, 0xea60, Some("SER123456"), "9-9", "salt-a")); // bus path irrelevant with a serial
        assert!(!a.contains("SER123456") && a.rsplit("s-").next().unwrap().len() >= 16);
        let d = tempfile::tempdir().unwrap();
        let s1 = load_salt(d.path());
        assert!(s1.len() >= 32);
        assert_eq!(s1, load_salt(d.path()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(d.path().join("hw/salt")).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn macos_port_names_never_leak_the_serial() {
        assert_eq!(redact_port("/dev/cu.usbmodemA1B2C3D4501"), "/dev/cu.usbmodem…4501");
        assert_eq!(redact_port("/dev/cu.usbserial-A50285BI"), "/dev/cu.usbserial-…85BI");
        assert_eq!(redact_port("/dev/cu.wchusbserial1410"), "/dev/cu.wchusbserial1410"); // location id
        assert_eq!(redact_port("/dev/ttyUSB0"), "/dev/ttyUSB0");
        let v: Value = serde_json::from_str(MAC_HOST).unwrap();
        let sc = parse_system_profiler(&v, &["/dev/cu.usbmodemA1B2C3D4501".to_string(), "/dev/cu.usbserial-SECRET12345".to_string()], "salt");
        let rep = report(tempfile::tempdir().unwrap().path(), &sc, &UsbIdTable::bundled(), "n", 1);
        let text = rep.to_string();
        assert!(!text.contains("A1B2C3D4501") && !text.contains("SECRET12345"), "{text}");
        assert!(text.contains("cu.usbmodem…4501") && text.contains("cu.usbserial-…2345"));
        // matching still used the real names
        assert_eq!(sc.devices.iter().find(|d| d.vid == 0x1a86).unwrap().ports, vec!["/dev/cu.usbmodemA1B2C3D4501"]);
    }

    #[test]
    fn redaction_shows_only_last_four() {
        assert_eq!(redact_serial("0001ABCD1234"), "…1234");
        assert_eq!(redact_serial("abc"), "****");
        assert_eq!(redact_serial("12345"), "…2345");
    }

    #[test]
    fn baseline_diff_and_report_states() {
        let sysfs = tempfile::tempdir().unwrap();
        fake_sysfs(sysfs.path());
        let root = tempfile::tempdir().unwrap();
        let table = UsbIdTable::bundled();
        let scan = scan_sysfs(sysfs.path(), Path::new("/dev"), "salt");

        let r = report(root.path(), &scan, &table, "n", 1);
        assert_eq!(r["baseline_missing"], true);
        assert!(r["devices"].as_array().unwrap().iter().all(|d| d["state"] == "new"));
        assert_eq!(r["devices"][0]["serial_redacted"], "…1234");
        assert!(!r.to_string().contains("ABCD1234"));
        assert_eq!(r["devices"][0]["id"]["kind"], "usb-uart");

        assert_eq!(save_baseline(root.path(), &scan, None, 5).unwrap(), 2);
        let r = report(root.path(), &scan, &table, "n", 2);
        assert_eq!(r["baseline_missing"], false);
        assert!(r["devices"].as_array().unwrap().iter().all(|d| d["state"] == "known"));
        assert!(r["removed"].as_array().unwrap().is_empty());

        // unplug the ACM board, plug in an unknown device
        fs::remove_dir_all(sysfs.path().join("1-3.1")).unwrap();
        let nd = sysfs.path().join("1-4");
        w(&nd.join("idVendor"), "dead\n");
        w(&nd.join("idProduct"), "beef\n");
        let scan2 = scan_sysfs(sysfs.path(), Path::new("/dev"), "salt");
        let r = report(root.path(), &scan2, &table, "n", 3);
        let states: Vec<_> = r["devices"].as_array().unwrap().iter().map(|d| (d["vid"].as_str().unwrap().to_string(), d["state"].as_str().unwrap().to_string())).collect();
        assert!(states.contains(&("10c4".into(), "known".into())) && states.contains(&("dead".into(), "new".into())));
        assert_eq!(r["removed"][0]["vid"], "303a");
        assert!(r["devices"].as_array().unwrap().iter().find(|d| d["vid"] == "dead").unwrap()["id"].is_null());

        // accept only the new one: baseline now holds 3, ACM entry retained
        let key = scan2.devices.iter().find(|d| d.vid == 0xdead).unwrap().key.clone();
        assert_eq!(save_baseline(root.path(), &scan2, Some(&[key]), 6).unwrap(), 3);
    }
}
