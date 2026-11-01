# Pulse benchmarks

Measured results only. Anything not listed here has **not been measured**;
in particular there are **no Sober FPS / frame-time measurements yet** (see
[sober-test-plan.md](sober-test-plan.md)).

Raw data is in [`benchmarks/raw/2026-11-01/`](../benchmarks/raw/2026-11-01/);
numbers taken from test output name the test.

Reproduce with:

```bash
scripts/collect_benchmarks.sh --display :99 --seconds 30   # needs an isolated Xvfb on :99
scripts/soak_memory.sh --display :99 --minutes 5            # memory soak at 500 CPS
```

## Environment (2026-11-01)

| Item | Value |
|---|---|
| CPU | Intel Core i5-11400F, 6C/12T, `intel_pstate`, governor `performance`, cpuidle `intel_idle` + `menu` (C1 1 µs, C2 253 µs, C3 1048 µs exit latency) |
| Kernel | 7.2.11-zen1-1-zen (CachyOS), default timer slack 50 µs |
| GPU / driver | NVIDIA GeForce RTX 3060, 615.78.08 |
| Session | native X11, LXQt (the live desktop is **not** used for injection benchmarks) |
| X server under test | Xvfb 21.1.25 (`-noreset`), CachyOS `xorg-server-xvfb` package |
| Build | `cargo build --release` (thin LTO, codegen-units = 1, panic = unwind), rustc 1.99.1 |

XTest numbers come from Xvfb, an otherwise idle software X server; a real Xorg
session with a compositor and a game adds its own latency. uinput numbers are
the cost of the `write(2)` calls into the kernel; the benchmark holds an
exclusive `EVIOCGRAB` on its virtual devices, so no desktop processing is
included.

## 1. Engine CPU, wakeups and memory (window closed)

`pulse-bench process` over 30 s windows, XTest input method on Xvfb, release
build. Wakeups are voluntary context switches summed over both engine
threads. CPU time has 10 ms tick resolution, so "0.00 s" means less than one
tick in 30 s (< 0.033 % of one core).

| Configuration | CPU time / 30 s | CPU % (one core) | Wakeups / s | RSS |
|---|---|---|---|---|
| Idle (ready, not clicking) | 0.00 s | < 0.033 | **0.0** | 3.8 MiB |
| Clicking, 200 ms interval (5 CPS) | 0.00 s | < 0.033 | 10.0 | 3.8 MiB |
| Clicking, 100 ms (10 CPS) | 0.00 s | < 0.033 | 20.0 | 3.8 MiB |
| Clicking, 50 ms (20 CPS) | 0.01 s | 0.033 | 40.0 | 3.8 MiB |
| Clicking, 20 ms (50 CPS) | 0.02 s | 0.067 | 100.0 | 3.8 MiB |
| Clicking, 1 ms, hold 0 (1000 CPS) | 0.13 s | 0.43 | 1000.0 | 3.7 MiB |

- The idle engine did not wake once in 30 s: no polling, no timers.
- While clicking, wakeups are exactly two per click (press deadline, release
  deadline), the theoretical minimum for explicit press/release. With hold 0
  the release is sent in the same wakeup as the press: one wakeup per click.

## 2. Timer accuracy

`pulse-bench timer`: lateness = wake time − absolute deadline (raw samples,
microseconds).

| Primitive | Interval | n | p50 | p95 | p99 | max |
|---|---|---|---|---|---|---|
| `timerfd` + `TFD_TIMER_ABSTIME` (used) | 1 ms | 3000 | 1.8 | 4.1 | 8.5 | 1270.8 |
| `timerfd` + `TFD_TIMER_ABSTIME` (used) | 10 ms | 1000 | 42.8 | 137.5 | 140.6 | 143.1 |
| `clock_nanosleep(TIMER_ABSTIME)` | 1 ms | 3000 | 51.7 | 53.6 | 56.2 | 101.8 |
| `clock_nanosleep(TIMER_ABSTIME)` | 10 ms | 1000 | 147.2 | 188.6 | 192.1 | 295.3 |

In-engine wake lateness while clicking (histogram, bucket-upper-bound
resolution ≤ 12.5 %), microseconds:

| Interval | wakeups | p50 | p99 | max |
|---|---|---|---|---|
| 200 ms | 310 | 20 | 146 | 146 |
| 100 ms | 620 | 144 | 144 | 145 |
| 50 ms | 1240 | 144 | 160 | 165 |
| 20 ms | 3100 | 144 | 144 | 148 |
| 1 ms, hold 0 | 31002 | 2 | 10 | 1318 |

No deadline was missed in the 200–20 ms runs. At 1 ms (hold 0) a separate
30 s run skipped 3 of 30 002 grid slots (wakeups at least half a period,
0.5 ms, late), without bursting.

- **`timerfd` vs `clock_nanosleep`:** `clock_nanosleep` is consistently ~50 µs
  later because the default 50 µs timer slack applies to it but not to
  `timerfd`. This is the measured reason Pulse waits on a `timerfd` with
  `epoll`.
- **Drift:** a naive loop of relative `sleep(10 ms)` accumulated **123.7 ms**
  of drift over 1000 iterations (197.4 ms over 3000 × 1 ms). With absolute
  deadlines the final wake was 134 µs (10 ms run) and 1.7 µs (1 ms run) after its
  own deadline. That lateness does not accumulate.
- **Why ~140 µs at 10 ms and above:** for sleeps of several milliseconds the
  kernel's idle governor parks the core in a deep C-state (C2/C3 above: 253
  and 1048 µs declared exit latency). Waking pays that cost; 1 ms sleeps stay
  in C1 and wake in ~2 µs. The raw `timerfd` benchmark, which contains no
  Pulse code, shows the same effect, so it is the platform, not Pulse. It is
  a nearly **constant offset** (p50 ≈ p99), so clicks stay evenly spaced.
  An earlier run under background compile load (shallower idle states)
  measured 7–40 µs. Pulse does not force CPU power settings
  (`/dev/cpu_dma_latency`, governors): that would cost power and heat
  system-wide for an invisible 0.14 ms.

## 3. Submission cost and latency

| Measurement | n | p50 | p99 | p99.9 | max |
|---|---|---|---|---|---|
| XTest `emit()` of one event incl. flush (Xvfb) | 10000 | 0.83 µs | 1.34 µs | 9.1 µs | 17.0 µs |
| uinput `emit()` of one event incl. `SYN_REPORT` | 4000 | 0.50 µs | 1.09 µs | 5.4 µs | 16.7 µs |
| XTest submit → test client receives `ButtonPress` (Xvfb) | 2000 | 5.8 µs | 18.0 µs | 23.5 µs | 139.8 µs |

End to end at 1000 CPS (1 ms, hold 0): in 2 s Pulse submitted 2002 clicks and
`pulse-test-receiver` on Xvfb received all 2002 presses and 2002 releases,
each press 1 ms after the previous one by X server timestamps (1 ms
resolution; [raw](../benchmarks/raw/2026-11-01/e2e_1000cps_hold0_receiver.json)).

Engine reaction times (a few runs each, so only the range is meaningful):

| Measurement | Runs | Range |
|---|---|---|
| Hotkey received → first event submitted (debug build, mock backend; `toggle_hotkey_starts_and_stops_and_records_trigger_latency`) | 5 | 29–80 µs |
| Stop requested → release submitted (release build, XTest on Xvfb; `stop_latency` in the `*_diagnostics.json` files) | 4 | 13–52 µs |

Input-method initialization: XTest 0.17 ms (connect + extension + keymap);
uinput 30 ms (two devices plus waiting for udev to create the nodes; the
engine additionally waits up to 2 s for the X server to register them, 246 ms
measured on the live Xorg session).

The two input methods differ by well under a microsecond per event, far below
scheduling jitter: neither is "faster" in any way that matters. Choose by
compatibility.

## 4. Hot-path allocations and dispatch

| Measurement | Result |
|---|---|
| Executor `poll()` allocations | **0** in 100 000 polls |
| XTest via x11rb's generated `fake_input` | 1.01 allocations / request |
| XTest via Pulse's pre-serialized request | 0.012 allocations / request (amortized periodic sync reply) |
| uinput `emit()` | 0.00025 allocations / event |
| `dyn InputBackend` call vs concrete call | 3.17 ns vs 2.58 ns |

Dynamic dispatch costs ~0.6 ns per call against a submission cost of
~500–850 ns, so the single `Box<dyn InputBackend>` design is kept; no
backend-specialized runner is justified.

## 5. Memory soak

`scripts/soak_memory.sh --display :99 --minutes 5 --interval-ms 2`: XTest at
500 CPS (1000 requests/s) for 5 minutes on Xvfb, RSS sampled every 30 s
([raw](../benchmarks/raw/2026-11-01/soak_memory_500cps_5min.txt)). Nothing
else ran on the machine during the soak.

| t (s) | 0 | 30 | 60 | 120 | 180 | 240 | 300 |
|---|---|---|---|---|---|---|---|
| RSS (KiB) | 3804 | 3808 | 3812 | 3812 | 3812 | 3812 | 3812 |
| submitted events | 3 | 30 012 | 60 026 | 120 062 | 180 098 | 240 134 | 300 170 |

RSS is flat after warm-up across ~300 000 XTest requests: the periodic sync
bounds x11rb's per-request bookkeeping. CPU: 0.37 s user + 0.36 s system over
300 s (0.24 % of one core at 500 CPS). In-engine wake lateness over 300 188
wakeups: p50 2 µs, p99 6 µs, max 1.6 ms. The final stop released cleanly
after 150 093 clicks.

## 6. UI process (optional, separate)

The compact window, release build, visible and idle, 20 s on Xvfb
([software](../benchmarks/raw/2026-11-01/ui_idle_software.json),
[FemtoVG](../benchmarks/raw/2026-11-01/ui_idle_femtovg.json)):

| Renderer | CPU / 20 s | Wakeups / s | RSS |
|---|---|---|---|
| `winit-software` | 0.00 s | 2.0 | 24.7 MiB |
| `winit-femtovg` | 0.00 s | 2.0 | 130.2 MiB |

Xvfb provides only Mesa software OpenGL, so the FemtoVG memory figure is not
representative of a GPU driver. Closing the window removes this cost
entirely; the engine is unaffected.

## 7. Findings that changed the implementation

1. **x11rb request bookkeeping grows without reads.** x11rb records every
   sent request and prunes the record only when it reads a packet from the
   server. XTest `FakeInput` has no reply, so a write-only injection
   connection grows without bound. The XTest backend queues a `GetInputFocus`
   every 256 requests and drains replies non-blockingly.
2. **x11rb's `fake_input` wrapper allocates per request**; Pulse writes the
   36-byte request from a stack buffer (byte-for-byte equal to x11rb's
   serializer, verified by a unit test).
3. **The X server can drop a disconnecting client's last requests.** Closing
   the XTest connection right after a release lost the release in 2 of 20
   tries, leaving a button stuck. Pulse now waits for a server round trip
   before closing (0 of 60 since, regression test
   `release_survives_immediate_disconnect`).
4. **`clock_nanosleep` is subject to timer slack, `timerfd` is not** (§2).
5. **Xvfb must run with `-noreset`** for integration tests, otherwise the
   server regenerates between tests and connections are reset.

## Not yet measured

- Sober average FPS, 1 % lows and p95/p99 frame time (baseline vs engine idle
  vs clicking with each input method).
- XTest latency on the real Xorg server (only Xvfb was used, to avoid
  injecting into the live desktop).
- uinput delivery latency through libinput/Xorg to an X client.
- 8-hour idle soak and 1-hour active soak (only the 5-minute memory soak ran).
- The window's CPU/GPU use on the real desktop and GPU (only Xvfb was measured).
