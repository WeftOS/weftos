# ESP32-P4 running Linux: m5stamp-p4-c6-linux

Reference review of [tsuyoshi-otake/m5stamp-p4-c6-linux](https://github.com/tsuyoshi-otake/m5stamp-p4-c6-linux),
read on 2026-10-17 at commit `c7ec6af` (release v0.2.5). GPL-2.0-only. Created 2026-09-04, one
author, very active (most of its 2026-10-17 history is hardware-test fixes). Nothing here was
built or flashed by us.

## What it is

A reproducible RISC-V **NOMMU** Linux 6.18 distribution for the **M5Stack Stamp-P4** (ESP32-P4,
dual-core RV32IMAC at 360 MHz, 32 MiB PSRAM, 16 MiB flash) with a **Stamp-AddOn C6** as the Wi-Fi
radio over 4-bit SDIO. It boots into a Wi-Fi SoftAP with DHCP, Dropbear SSH, BusyBox and
MicroPython, with a read-only SquashFS root and RAM-backed OverlayFS on `/home`, `/root`, `/var/lib`.
Built with Buildroot from pinned submodules (linux, buildroot, esp-idf, esp-hosted, and
mrbreaker/why2025-linux as a reference-only earlier P4 Linux proof); 97 patches.

Facts from its README (not verified by us):
- **Single core in practice.** The kernel is SMP, but the shipped image boots `maxcpus=1`. With two
  cores about one boot in five stalls (CPU1 stops taking interrupts, its issue #5); `isolcpus=1`
  avoided it in 17/17 boots but leaves CPU1 idle.
- **NOMMU limits shape everything.** Programs are bFLT, loaded into a 10 MB reserved pool; each
  BusyBox process takes about 500 KB because bFLT programs do not share text. No `sudo`, `curl`,
  `nano`; 5-6 concurrent SSH sessions.
- **Wi-Fi via ESP-Hosted NG 1.0.6 on the C6.** SDIO at 5 MHz by default (40 MHz passed boot checks,
  not soaked). Measured ~300-440 KB/s raw TCP, ~210-250 KB/s over SSH (Dropbear crypto on one
  360 MHz core is the limit). WPA2-PSK only; Secure Boot and flash encryption are off.
- **C6 updates go over SDIO** from a temporary P4 updater image; the C6 module has no USB port.

## Is it useful for our ESP32 builds?

**As a runtime for WeftOS components: no, not today.** It is NOMMU riscv32 Linux with bFLT
binaries and a ~500 KB per-process budget. Our Rust components (cog host, leaf agents) target
`std` Linux with an MMU (aarch64, armv7, x86-64) or `no_std`/ESP-IDF firmware. We are not aware of a
Rust `std` target for NOMMU riscv32 Linux (unverified; check before planning around it). Our P4
plans should stay on ESP-IDF/esp-hal, where Espressif supports the chip.

**As engineering practice: yes, several patterns are worth borrowing** (ideas only; the code is
GPL-2.0 and must not be copied into our MIT crates):

| Pattern | What they do | Where it maps for us |
|---|---|---|
| Board contract | `board-contract.json`: part, flash/PSRAM sizes, ROM and chip revision observed on the real unit, boot/reset pins, and the SHA-256 of a verified full stock flash readback (two matching readbacks) | Leaf and cog bring-up: record the observed board, not the datasheet, and keep a hashed stock backup before the first write (our CrowPanel chunked-backup step) |
| Release assets gated by hardware validation | CI refuses to publish any asset whose hash is not listed in a `*hardware-validation-*.md` ledger written after flashing that exact build | Our signed releases and cog repository: only publish firmware hashes that a recorded device run validated |
| Pinned recovery tooling | `versions.lock.json` pins esptool **4.8.1** for P4 readback (5.3.1 lost the USB-Serial/JTAG response around `0x001AB000`) | Note for any P4 work: pin esptool for readback/recovery separately from the build toolchain |
| Config drive with A/B slots | USB mass-storage FAT `/boot` drive: save a file, the board validates and applies it, then writes it to redundant A/B flash slots; no eject or reboot | Field configuration of leaf/sensor nodes without a companion app |
| Per-device credentials | Random SSID/PSK/SSH password generated on first boot, written to the config drive; no shared default password | Matches our "secrets never shared or in chat" rule for provisioning |
| ESP-Hosted NG over SDIO | A C6 as the Wi-Fi co-processor for a radio-less host SoC | Relevant if a P4-class host ever needs Wi-Fi; see our C5/C6 notes |

## Related

- [esp32-c5-vs-c6-ruview.md](esp32-c5-vs-c6-ruview.md) (C6 radio limits: 2.4 GHz only).
- [microcontrollers.md](microcontrollers.md) (the RISC-V ESP32 family, including the P4).
- [sbcs.md](sbcs.md) for MMU-capable Linux boards that can run the cog host today.
