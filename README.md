# Pulse

A fast, low-overhead **auto clicker** for **native X11** on Linux (Arch/CachyOS
first), built with Sober (Roblox on Linux) in mind.

Pulse repeats **one thing**: a mouse button (left, right or middle) **or** one
keyboard key. There are no macro sequences, presets or profiles: one set of
settings, a start/stop hotkey and an emergency stop.

- **Simple:** pick what to click, set the speed, press **F6** in your game.
  **F8** stops everything, always.
- **Safe:** by default it only clicks while the game window you picked is in
  front, and stops the moment you switch away. It releases whatever it pressed
  on every kind of stop, and never "catches up" missed clicks in a burst.
- **Light:** a headless Rust engine (~4 MiB of memory) that does **no work at
  all while idle** and wakes exactly twice per click while running
  ([measurements](docs/benchmarks.md)). The window is optional: close it and
  the hotkeys keep working.
- **Two input methods:** *Standard (X11/XTest)*, the default, or
  *Virtual device (uinput)* for games that ignore the standard method.

> Pulse makes no promise of zero CPU, zero latency or zero FPS impact, or that
> a game accepts its clicks. Roblox prohibits cheating and disruptive
> automation: use Pulse only where automation is allowed. Pulse contains no
> anti-cheat evasion, process injection or memory access.

## Using Pulse

1. Open the Pulse window (`pulse-ui`). It starts the background engine by
   itself if it isn't running.
2. Choose **Click** (left/right/middle or a keyboard key), the **Speed**
   (milliseconds between clicks) and the **Mode**: *Toggle* (press F6 to start
   and again to stop) or *Hold* (clicks while you hold F6). Changes apply
   immediately.
3. Under **Game window**, press **Pick window** and click your game within
   3 seconds.
4. In the game, press **F6**. Press **F8** to stop everything at once.

Closing the window keeps F6/F8 working; **Quit Pulse** stops everything.
Settings live in `~/.config/pulse-macro/config.toml`
([all options](configs/example.toml)).

### From a terminal

```bash
pulse-cli status                      # what Pulse is doing and whether it can start
pulse-cli set --cps 20 --button left  # 20 clicks per second, left button
pulse-cli set --key E --mode hold     # spam the E key while F6 is held
pulse-cli set --hotkey F7 --count 100 # F7 to start, stop after 100 clicks
pulse-cli target capture              # pick the game window (switch to it within 3 s)
pulse-cli backend uinput              # use the virtual-device input method
pulse-cli estop                       # emergency stop
pulse-cli shutdown                    # quit Pulse
```

`pulse-cli set --help` lists every option. Intervals below 10 ms need
`--fast on` and are experimental: Sober has a reported chat-focus problem at
high click rates.

### The virtual-device input method (uinput)

It needs write access to `/dev/uinput`. Many desktops already grant it; check
with `pulse-cli backends`. If it reports *Device Permission Denied*, follow
[docs/security.md](docs/security.md#uinput) (a narrow, opt-in udev rule).
Never run Pulse as root.

## Build

Requirements: Rust ≥ 1.85 (`pacman -S rust`), an X11 session. The window also
needs `fontconfig`, `freetype2` and `libxkbcommon` (present on most desktops).

```bash
cargo build --release                  # engine, CLI and benchmark tools
cargo build --release -p pulse-ui      # the window
target/release/pulse-ui &              # starts the engine automatically
```

An Arch package can be built from [`packaging/arch/PKGBUILD`](packaging/arch/PKGBUILD)
(`cd packaging/arch && makepkg -si`). An optional `systemd --user` unit is in
[`packaging/systemd/`](packaging/systemd/pulse-engine.service).

## Tests

```bash
cargo test --workspace                             # unit, property and engine tests (no X needed)
scripts/run_xvfb_integration.sh --uinput           # X11 e2e on an isolated Xvfb (pacman -S xorg-server-xvfb);
                                                   # uinput tests use exclusively grabbed devices
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

The X11 tests never touch your desktop: they only use the display named in
`PULSE_TEST_DISPLAY`. Benchmarks: `scripts/collect_benchmarks.sh --display :99`
and `pulse-bench --help`. `pulse-test-receiver` opens a test window that counts
the clicks it actually receives.

## Documentation

- [Architecture](docs/architecture.md): processes, threads, hot path, state machine, design decisions and limits
- [Protocol](docs/protocol.md): IPC commands, framing, errors, events
- [Security](docs/security.md): permissions, uinput setup, stuck-input limits
- [Benchmarks](docs/benchmarks.md): measured results and methodology
- [Sober test plan](docs/sober-test-plan.md): in-game procedure and results
- [Implementation status](docs/implementation-status.md): checklist

## License

Pulse is released under the [MIT License](LICENSE).

The window (`pulse-ui`) is built with [Slint](https://slint.dev), used under
the Slint Royalty-free Desktop, Mobile, and Web Applications License 2.0.
That license requires Slint's "Made with Slint" credit, which Pulse shows
in its About box; keep it there when distributing builds of the window. The
engine and `pulse-cli` do not use Slint. All other dependencies are
available under permissive licenses (MIT, Apache-2.0, BSD, Zlib and similar).
