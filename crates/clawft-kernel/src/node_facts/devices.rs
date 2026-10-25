//! Device probes (Linux): the buses a cog's native half can open, an MCU
//! bridge, USB SDRs and board power monitors (Ventuno cog plan, section 4.6).
//!
//! A bus is advertised by its device nodes (`/dev/i2c-*`, `/dev/spidev*`,
//! `/dev/gpiochip*`, USB and SoC UARTs, SocketCAN interfaces). Pins wired to a
//! microcontroller are not Linux buses: they show only as `bus.mcu`, and which
//! MCU buses exist depends on the firmware the MCU runs.

use clawft_types::placement::Provenance;

use super::host::ProbeHost;
use super::probe::{Collected, cap, str_list};

/// UART name prefixes worth advertising. `ttyS*` is left out (the 8250 driver
/// registers dozens of placeholder ports) and so is `ttySTM*`, the MCU link.
/// Kernel console ports are removed by [`console_ttys`].
const UART_PREFIXES: &[&str] = &["ttyUSB", "ttyACM", "ttyAMA", "ttyHS", "ttyMSM"];

/// MCU bridge sockets: `(path, bridge name)`.
const MCU_BRIDGES: &[(&str, &str)] = &[
    ("/run/arduino-router.sock", "arduino-router"),
    ("/var/run/arduino-router.sock", "arduino-router"),
];

/// USB SDRs: `(vendor:product, capability id, model)`.
const SDR_USB: &[(&str, &str, &str)] = &[
    ("0456:b673", "sdr.iio", "pluto"),
    ("0456:b674", "sdr.iio", "pluto"),
    ("0bda:2838", "sdr.rtl", "rtl2838"),
    ("0bda:2832", "sdr.rtl", "rtl2832"),
    ("1d50:6089", "sdr.hackrf", "hackrf-one"),
    ("2cf0:5250", "sdr.bladerf", "bladerf2"),
    ("2cf0:5246", "sdr.bladerf", "bladerf1"),
];

/// hwmon names of INA2xx-family power monitors.
const POWER_MONITORS: &[&str] = &["ina219", "ina226", "ina230", "ina231", "ina232", "ina238", "ina2xx", "ina3221"];

/// SocketCAN interfaces report ARPHRD_CAN in `type`.
const ARPHRD_CAN: &str = "280";

/// Run the device probes.
pub fn probe(host: &dyn ProbeHost, c: &mut Collected) {
    buses(host, c);
    mcu(host, c);
    sdr(host, c);
    power(host, c);
}

fn push_bus(c: &mut Collected, id: &str, devices: &[String]) {
    if !devices.is_empty()
        && let Some(b) = cap(id, Provenance::Probed)
    {
        c.push(b.with_attr("devices", str_list(devices)));
    }
}

fn buses(host: &dyn ProbeHost, c: &mut Collected) {
    let dev = host.list_dir("/dev").unwrap_or_default();
    let pick = |f: &dyn Fn(&str) -> bool| -> Vec<String> {
        dev.iter().filter(|n| f(n)).cloned().collect()
    };
    push_bus(c, "bus.i2c", &pick(&|n| n.starts_with("i2c-")));
    push_bus(c, "bus.spi", &pick(&|n| n.starts_with("spidev")));
    push_bus(c, "bus.gpio", &pick(&|n| n.starts_with("gpiochip")));
    let consoles = console_ttys(host);
    push_bus(
        c,
        "bus.uart",
        &pick(&|n| UART_PREFIXES.iter().any(|p| n.starts_with(p)) && !consoles.iter().any(|t| t == n)),
    );
    if !consoles.is_empty() {
        c.note("bus.uart", format!("kernel console port(s) not advertised: {}", consoles.join(", ")));
    }
    let can: Vec<String> = host
        .list_dir("/sys/class/net")
        .unwrap_or_default()
        .into_iter()
        .filter(|i| {
            host.read_file(&format!("/sys/class/net/{i}/type"))
                .is_some_and(|t| t.trim() == ARPHRD_CAN)
        })
        .collect();
    push_bus(c, "bus.can", &can);
}

/// TTYs the kernel uses as its console (`/sys/class/tty/console/active` and
/// `console=` on the command line); a cog must not open them.
pub fn console_ttys(host: &dyn ProbeHost) -> Vec<String> {
    let mut out: Vec<String> = host
        .read_file("/sys/class/tty/console/active")
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_string)
        .collect();
    for arg in host.read_file("/proc/cmdline").unwrap_or_default().split_whitespace() {
        if let Some(v) = arg.strip_prefix("console=") {
            let tty = v.split(',').next().unwrap_or("").to_string();
            if !tty.is_empty() && !out.contains(&tty) {
                out.push(tty);
            }
        }
    }
    out
}

fn mcu(host: &dyn ProbeHost, c: &mut Collected) {
    let Some((_, bridge)) = MCU_BRIDGES.iter().find(|(p, _)| host.exists(p)) else {
        return;
    };
    if let Some(m) = cap("bus.mcu", Provenance::Probed) {
        c.push(m.with_attr("bridge", *bridge));
        c.note(
            "bus.mcu",
            format!("{bridge} socket present; MCU buses depend on the flashed sketch and are not probed"),
        );
    }
}

fn sdr(host: &dyn ProbeHost, c: &mut Collected) {
    if host.which("lsusb").is_none() {
        return;
    }
    let usb = host.run("lsusb", &[]).unwrap_or_default();
    for (usb_id, id, model) in SDR_USB {
        let n = usb.lines().filter(|l| l.contains(usb_id)).count();
        if n > 0
            && let Some(s) = cap(id, Provenance::Probed)
        {
            c.push(
                s.with_attr("model", *model)
                    .with_attr("transport", "usb")
                    .with_attr("count", n as i64)
                    .exclusive(),
            );
        }
    }
}

fn power(host: &dyn ProbeHost, c: &mut Collected) {
    let sensors: Vec<String> = host
        .list_dir("/sys/class/hwmon")
        .unwrap_or_default()
        .into_iter()
        .filter_map(|h| host.read_file(&format!("/sys/class/hwmon/{h}/name")))
        .map(|n| n.trim().to_string())
        .filter(|n| POWER_MONITORS.contains(&n.as_str()))
        .collect();
    if !sensors.is_empty()
        && let Some(p) = cap("power.monitor", Provenance::Probed)
    {
        c.push(p.with_attr("sensors", str_list(&sensors)).with_attr("via", "hwmon"));
    }
}
