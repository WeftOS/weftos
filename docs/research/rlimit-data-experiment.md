# RLIMIT_DATA backstop experiment

Date: 2026-11-15. Host: a 16-core x86-64 Linux machine, glibc 2.43, rustc 1.95.

## Question

The native cog supervisor sets `RLIMIT_DATA` as a kernel backstop under the RSS watchdog (`apply_in_child` in `crates/clawft-kernel/src/workload_runtime/supervise.rs`). The first version used 2x `ram_mb`. We measured instead of guessing: which value gives no false kills on realistic workloads and still bounds runaway growth?

A false kill is a process stopped by the rlimit although its peak resident memory stayed under `ram_mb`. The stop shows up as SIGABRT (a failed allocation), SIGTRAP (V8), SIGSEGV, or `ENOMEM` from `mmap`. Overuse of resident memory is the watchdog's job, so that is a true kill and is not counted.

## Method

A throwaway harness (not part of the repo) forked each workload with a cleared environment and applied `setrlimit(RLIMIT_DATA, mult * ram_mb)` before exec, the same call as `apply_in_child`. It sampled `/proc/<pid>/status` every 5 ms for `VmRSS`, `VmHWM`, `VmData` and `VmPeak`, and recorded the exit status and signal.

Grid: `ram_mb` in {64, 256, 1024} x multiplier in {0 (no limit, natural peak), 1, 1.5, 2, 3, 4, 6, 8}. Each cell ran 3 times: 2088 runs in total.

Workloads ("touch" means every page is written):

| id | what |
|---|---|
| a:touch-half / -90pct | well-behaved, glibc malloc, touches 50% / 90% of `ram_mb` |
| map:none-4GiB / 64GiB-commit50 | `PROT_NONE` reservation, commit 50 MiB (Go, V8 cage, wasmtime style) |
| map:rw-1GiB / 4GiB-untouched50 | `PROT_READ\|PROT_WRITE` private mapping, touch 50 MiB (JVM, naive arena style) |
| b:glibc-thrK | K threads churning small allocations, 1 MiB held each; default glibc arenas (64 MiB reserved per thread) |
| b:glibc-thr64-ARENA_MAX=2 | same with `MALLOC_ARENA_MAX=2` |
| c:jemalloc-preload / -crate | jemalloc via `LD_PRELOAD`, and in-process `tikv-jemallocator` |
| d:mimalloc | in-process `mimalloc` |
| e:tokio-wK-tT | tokio multi-thread runtime, K workers, T tasks holding 1 MiB each, plus the blocking pool |
| f:python3, f:node | real interpreters (Python 3 bytearray; Node 24 Buffer) |
| r:runaway-* | grow until stopped: 64 MiB steps, 4 MiB steps, RW `mmap` in 32 MiB steps (self-cap 8 GiB) |

Not covered:

- A real cog binary. Node and Python stand in for foreign runtimes.
- Wasmtime. The `wasmtime-project-v1` adapter launches through its own `ChildLauncher`, not the native `ProcLimits` path, so the native rlimit never applies to it.
- Go. No Go binary was available. The `map:none` rows stand in for its `PROT_NONE` reservations.

## Natural peaks (no limit)

`VmData` is what `RLIMIT_DATA` counts. Fixed costs dominate for small cogs.

| workload | peak RSS MiB | peak VmData MiB | note |
|---|---|---|---|
| a:touch-half (ram 256) | 131 | 128 | data = rss |
| map:none-4GiB / 64GiB | 53 | 50 | `PROT_NONE` is not counted |
| map:rw-1GiB / 4GiB | 53 | 1024 / 4096 | counted in full |
| b:glibc-thr64 | 117 | 243 | 2.1x rss |
| c:jemalloc-thr64 (preload / crate) | 162 / 160 | 560 / 576 | 3.5x rss |
| d:mimalloc-thr4 | 33 | 1035 | 31x rss, about 1 GiB fixed |
| d:mimalloc-thr64 | 430-450 | 1164 | |
| d:mimalloc-touch-half (ram 256) | 135 | 1027 | |
| e:tokio-w16-t128 | 132 | 180 | tokio is cheap |
| f:node-buffer-10 | 54 | 570 | about 0.55 GiB fixed |
| f:node-buffer-half (ram 256) | 172 | 689 | |
| f:python3-bytearray-half | 138 | 133 | data = rss |

The overhead is roughly constant (0.25 to 1.1 GiB depending on runtime and thread count). It is not a fraction of `ram_mb`. At `ram_mb = 64`, 2x is 128 MiB. Node needs 570 MiB just to start with 10 MiB of data.

## Results with a pure multiplier

Survivors out of 3 runs at `ram_mb = 256`. A cell counts runs that finished without an rlimit stop.

| workload | 1x | 1.5x | 2x | 3x | 4x | 6x | 8x |
|---|---|---|---|---|---|---|---|
| touch-half, touch-90pct, python3 | 3 | 3 | 3 | 3 | 3 | 3 | 3 |
| PROT_NONE 4 GiB / 64 GiB | 3 | 3 | 3 | 3 | 3 | 3 | 3 |
| glibc thr4/16/64, ARENA_MAX=2 | 3 | 3 | 3 | 3 | 3 | 3 | 3 |
| jemalloc thr4 / thr16 | 3 | 3 | 3 | 3 | 3 | 3 | 3 |
| jemalloc thr64 (preload, crate) | 0 | 0 | 0 | 3 | 3 | 3 | 3 |
| mimalloc thr4 / thr16 / touch-half | 3 / 3 / 0 | 3 | 3 | 3 | 3 | 3 | 3 |
| mimalloc thr64 | 0 | 0 | 0 | 3 | 3 | 3 | 3 |
| tokio w2 / w8 / w16 | 3 | 3 | 3 | 3 | 3 | 3 | 3 |
| node buffer 10 MiB / 128 MiB | 0 | 0 | 0 | 3 | 3 | 3 | 3 |
| RW map 1 GiB untouched | 0 | 0 | 0 | 0 | 0 | 3 | 3 |
| RW map 4 GiB untouched | 0 | 0 | 0 | 0 | 0 | 0 | 0 |

False kills among realistic workloads whose peak RSS stayed under `ram_mb` (3 runs each):

| ram_mb | 1x | 1.5x | 2x | 3x | 4x | 6x | 8x |
|---|---|---|---|---|---|---|---|
| 64 | 18/48 | 15/48 | 15/48 | 9/48 | 9/48 | 9/48 | 9/48 |
| 256 | 21/75 | 18/75 | 18/75 | 6/75 | 6/75 | 3/75 | 3/75 |
| 1024 | 9/78 | 3/78 | 3/78 | 3/78 | 3/78 | 0/78 | 0/78 |

No pure multiplier up to 6x is clean at `ram_mb` 64 or 256. At 64, even 8x still kills Node and the RW maps. The old 2x default false-killed Node, mimalloc, jemalloc with many threads, and any RW mapping over 1 GiB at 256, and Node at 64.

Failure shape at the limit: `mmap` returns `ENOMEM` (exit 71), Rust allocators abort (SIGABRT, 240 runs), V8 traps (SIGTRAP, 60 runs), and occasionally SIGSEGV (17 runs). There were no hangs and no kernel OOM kills.

## Runaway bound

A process stopped by the rlimit alone dies at a `VmData` just under the limit. The 4 MiB, 64 MiB and RW-`mmap` step sizes all stop within one step of it. Peak RSS matched the limit in every cell (for example 451 MiB at a 512 MiB limit, and 2047 MiB at 2048 MiB).

Growth rate on the test host was about 1.07 GiB/s for malloc and RW `mmap` alike (4 GiB touched in 3.8 s). The watchdog polls every 500 ms, so a hog can add about 0.5 GiB between samples. The backstop therefore only has to hold a bound near `ram_mb` plus 0.5 GiB. It covers a hog faster than one poll, or a stalled supervisor.

## Choice: limit = 2 * ram_mb + 1280 MiB

We fitted candidates against the natural `VmData` of every workload whose RSS stays under `ram_mb`. A candidate survives a workload when its limit is at least the natural peak.

| a (x ram_mb) | C (MiB) | false kills at ram 64 / 256 / 1024 | limit / ram at 64 / 256 / 1024 | failing workloads |
|---|---|---|---|---|
| 2 | 0 (old) | 7/16, 9/25, 1/26 | 2 / 2 / 2 | jemalloc, mimalloc, node, RW maps |
| 4 | 0 | 5/16, 5/25, 1/26 | 4 / 4 / 4 | mimalloc, node, RW maps |
| 8 | 0 | 5/16, 1/25, 0/26 | 8 / 8 / 8 | mimalloc, node, RW maps (at ram 64) |
| 2 | 768 | 4/16, 1/25, 1/26 | 14 / 5 / 2.8 | mimalloc-thr4, touch-half, RW maps |
| 2 | 1024 | 1/16, 1/25, 1/26 | 18 / 6 / 3 | RW 4 GiB only |
| **2** | **1280** | **1/16, 1/25, 1/26** | **22 / 7 / 3.2** | **RW 4 GiB only** |
| 2 | 2048 | 1/16, 1/25, 1/26 | 34 / 10 / 4 | RW 4 GiB only |
| 0 | 1280 | 1/16, 1/25, 1/26 | 20 / 5 / 1.2 | RW 4 GiB only |

No single multiplier is free of false kills. The RSS watchdog is the real enforcement. `RLIMIT_DATA` is a loose backstop with a fixed floor.

The worst legitimate overhead above 2x `ram_mb` was 933 MiB (mimalloc with 16 threads at `ram_mb` 64: 1061 MiB data minus 128 MiB). 1280 MiB leaves a 37% margin. It is not a free allowance: it is about 1.2 s of growth at the measured rate, and the watchdog acts within 0.5 s. Keeping the 2x term leaves room for fragmentation on large cogs, where the data to RSS ratio was at most about 1.1 for touch-heavy runtimes.

Residual case: a cog that maps a read-write private region larger than the headroom and never touches it (4 GiB RW untouched) is still stopped. That is inherent to `RLIMIT_DATA`, which counts committed-style address space. Such a cog should reserve with `PROT_NONE` (as Go, V8 and most allocators do), declare a larger `ram_mb`, or wait for the cgroup v2 follow-up, which counts resident memory only.

## What WeftOS changed

- `crates/clawft-kernel/src/workload_runtime/supervise.rs`: `DATA_BACKSTOP_HEADROOM_MB = 1280`; `ProcLimits::data_limit_bytes()` returns `2 * ram_mb + 1280` MiB; `apply_in_child` uses it.
- Tests pin the value: `data_backstop_is_twice_ram_plus_fixed_headroom` (values for 64, 256 and 1024, and overflow) and `rlimit_data_backstop_is_set_and_address_space_stays_unlimited` (`ulimit -d` is 1835008 KiB at 256 MiB). A native test checks that `ram_mb` 64 gives 1441792 KiB.
- Docs: `docs/reference/config-sections.md` and `docs/adr/adr-100-cog-workload-kind.md`.

## Limits of the experiment

- One host, one glibc version, one run per cell group, on an otherwise idle machine.
- Thread counts above 16 are synthetic for a 256 MiB cog. They are realistic for hosts with many cores, because glibc arena counts scale with cores.
- Node 24 and Python 3.14 stand in for foreign runtimes.
- ARM hosts were not measured. Page size and allocator defaults may differ. Re-run the harness there before lowering the headroom.
