# Pulse architecture

Pulse is an auto clicker: it repeats one input, a mouse button or one
keyboard key, on a fixed time grid, started and stopped by a hotkey.

## Processes

```text
pulse-ui (Slint, optional) ──┐
pulse-cli ───────────────────┼── Unix socket $XDG_RUNTIME_DIR/pulse-macro/engine.sock
                             │   (control only: never per-click traffic)
                             ▼
┌──────────────────────────── pulse-engine ────────────────────────────┐
│ control thread (epoll)                     runner thread (epoll)     │
│  ├ X11 watcher: XI2 raw keys,              ├ timerfd (ABSTIME,       │
│  │  _NET_ACTIVE_WINDOW focus                │  CLOCK_MONOTONIC)       │
│  ├ IPC server + clients        commands ──►├ eventfd (commands)       │
│  ├ signal self-pipe            ◄── events  ├ Executor (clicker grid)  │
│  └ state machine, policy,                  └ Box<dyn InputBackend>    │
│    persistence                  atomics:      (XTest or uinput)      │
│                                 cancel gen, focus_ok, metrics        │
└──────────────────────────────────────────────────────────────────────┘
```

- **pulse-engine** owns timing, hotkeys, focus, backends and safety state.
  It links no GUI code (`cargo tree -p pulse-engine` contains no Slint).
- **pulse-ui** only presents and edits configuration and sends commands. It
  can exit at any time; the engine keeps its state and a new UI reconnects.
- **pulse-cli** is the terminal/emergency client.
- **pulse-bench** and **pulse-test-receiver** measure and verify.

## Crates

| Crate | Role | Depends on |
|---|---|---|
| `pulse-core` | Pure logic: input model, held-input ledger, clicker validation, executor (scheduler), trigger controller, focus rules, state machine, limits. No I/O, no clocks. | serde, thiserror |
| `pulse-input` | `InputBackend` trait, XTest backend (x11rb), uinput backend (evdev), keymap translation, availability probes, mock backend | core, x11rb, evdev |
| `pulse-config` | XDG paths, `config.toml` (one set of settings), strict schema, atomic save, validation into a click plan | core, toml |
| `pulse-protocol` | Versioned length-delimited JSON messages and a blocking client | core, config, serde_json |
| `pulse-engine` | The daemon: control loop, runner, X11 watcher, IPC server, metrics | all of the above, rustix, signal-hook |
| `pulse-cli` | Control utility | protocol, config, clap |
| `pulse-ui` | Slint UI | protocol, config, core, slint |
| `pulse-bench` | Benchmarks and the X11 test receiver | core, input |

## Threads and the hot path

**Control thread** blocks in `epoll_wait` with no timeout. Its event sources
are the X11 socket (raw key events, focus changes, device hierarchy), the
listening socket and clients, the runner's notification `eventfd`, and a
self-pipe written by the SIGTERM/SIGINT/SIGHUP handlers. Before each wait it
drains events x11rb may already have buffered.

**Runner thread** blocks in `epoll_wait` on a `timerfd` and a command
`eventfd`. Per wakeup it:

1. reads `CLOCK_MONOTONIC` and records wake jitter;
2. checks the cancellation generation, focus flag and suspend offset
   (atomics and two vDSO clock reads);
3. calls `Executor::poll(now)`, pure logic over a small `Copy` program that
   returns at most one event (press or release) and the next deadline, with
   no allocation;
4. submits that event with one backend call and updates relaxed counters;
5. arms the `timerfd` for the next absolute deadline. A deadline that is
   already due (the release of an instant click, hold 0) is handled in the
   same wakeup by repeating steps 2–4, a bounded number of times.

No logging, formatting, locking, allocation or IPC happens per click.
Measured hot-path allocations: 0 (see [benchmarks.md](benchmarks.md)).

Commands to the runner go over an `mpsc` channel plus an `eventfd` wake;
results come back the same way. A stop first publishes the run id into an
atomic cancellation generation, which the runner checks before every event,
and then sends the command, so cancellation takes effect even if the
command is still queued.

## Scheduling policy (pure, in `pulse-core`)

- Absolute deadlines: presses at `t0 + n·T`. Small lateness never
  accumulates into drift.
- **Overload:** a press later than its hold time (half a period for an
  instant click, hold 0) is skipped and the run moves to the next grid slot
  after `now`. After a late release, the next
  press waits for a slot that leaves at least `(T − hold)/2` of up-time. A
  late-but-acceptable press keeps at least half its hold. Missed slots are
  counted, never burst. Releases are never skipped.
- Every finish path (click limit, time limit, cancel) releases the input
  before reporting done.
- Validated by unit tests and property tests: 5000 random runs (mouse and
  keyboard targets, intervals from 1 ms, holds from 0) with injected stalls
  and cancels, plus a simulated 1-hour run checked for drift.

## State machine

`Booting → Idle | Ready → Running → Stopping → Ready`, plus `Error` and
`ShuttingDown`. The state is *derived* from facts (shutting down, active run
and stop request, sticky fault, active backend) after every change and
broadcast to subscribers on transitions only.

- In `Ready` the hotkey starts clicking whenever `can_start` holds: valid settings, a ready input method, no fault and, with
  the window restriction on, a picked game window. `status.start_blocker`
  explains in plain language what is missing.
- Emergency stop (hotkey, UI, CLI) stops immediately and releases.
- Backend faults and X11 loss enter `Error` and block the hotkey until the
  user switches the input method or reconnects. Suspend/resume stops the run.
- A failed release enters `Error` with an explicit "input may be stuck"
  message.

## Backend selection

One `Box<dyn InputBackend>` lives in the runner. A switch (`select_backend`)
runs as a transaction on the control thread:

1. Refuse if a run is active ("Stop clicking to switch the input method").
2. The runner releases everything and hands the old backend back.
3. Construct the new backend. For uinput, wait up to 2 s for the X server to
   register the virtual devices.
4. On success: install it, drop the old one, persist `engine.backend`.
5. On failure: reinstall the old backend only if it passes a live health
   check (`check()`, a real round trip); otherwise end with no backend in
   `Error`. The persisted preference changes only on success.

The inactive backend has no resources: no X connection and no virtual
devices.

## Hotkeys and self-trigger suppression

XInput2 raw key events (`XI_RawKeyPress/Release`, XI ≥ 2.1) are observed on
the root window without grabbing, so the hotkey still reaches the focused
application. Events whose source device is an XTEST slave or a
`Pulse Virtual Keyboard/Mouse` are dropped, so Pulse cannot trigger itself
with either backend. Auto-repeat is ignored by tracking pressed state per key.
Keycodes are mapped through the verified X11 keymap: evdev+8 when the
server's layout-independent keys confirm it, otherwise a keysym lookup.

A hotkey can also be a mouse side button, `Mouse4` (X11 button 8, back) or
`Mouse5` (button 9, forward). Raw button events
(`XI_RawButtonPress/Release`) are selected only while a side button is a
hotkey, and only on the physical pointer devices (re-selected when devices
are plugged in or removed), never on the XTEST or Pulse devices. Pulse's
own clicks therefore never wake the control thread. Left, right and middle
are never hotkeys.

*Trade-off:* raw events wake the engine for every physical key transition
(not mouse motion), plus every physical button press while a side-button
hotkey is set. `XGrabKey`/`XGrabButton` would wake only for the hotkeys but
steal them from the game, so Pulse observes raw events.

## Focus guard

The engine watches `_NET_ACTIVE_WINDOW` on the root window and the title
properties of the active window. The target rule (WM_CLASS
instance/class, optional title substring) comes from user selection and is
never hardcoded. A cached `focus_ok` atomic is checked before every event;
a focus change also sends an explicit stop. Without `_NET_ACTIVE_WINDOW`
(no EWMH window manager), focus is treated as *not* on target, which is
safe.

## Design decisions and limits

| Topic | Decision | Why |
|---|---|---|
| Scope | One clicker (a mouse button **or** one key), one set of settings, no sequences or profiles | Pulse is an auto clicker; config schema and protocol are version 2 and other versions are refused |
| Window | One compact window with basic settings and a collapsed "More options"; diagnostics via `pulse-cli diagnostics` | Keeps the window small and simple |
| Directory names | `pulse-macro` under each XDG base directory | `pulse` is PulseAudio/PipeWire's directory (`~/.config/pulse/cookie`, `$XDG_RUNTIME_DIR/pulse/native`); sharing it would mix files with the audio server |
| Backend initialization | Construction is initialization (`open()`) | A half-initialized backend cannot exist, so it can never be reported `Ready` |
| Pointer motion / scroll | Not supported | Pulse only clicks; the uinput mouse still advertises REL_X/REL_Y because libinput needs them to treat it as a mouse |
| Suspend detection | `CLOCK_BOOTTIME − CLOCK_MONOTONIC` offset checked per wake, not logind | No D-Bus dependency; detects suspend on resume |
| Desktop-lock detection | Not implemented | Needs D-Bus; lock screens take focus, which the focus guard already handles |
| Automatic input-method choice | Not implemented: the user picks XTest (default) or uinput | Whether a game accepts a method can only be seen in that game |
| Modifier hotkeys (Ctrl+F6) | Single keys only | Raw events carry no modifier state |
| Mouse-button hotkeys | Side buttons only (`Mouse4`, `Mouse5`) | Left, right and middle can conflict with the clicks Pulse sends |
