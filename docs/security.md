# Security, permissions and limitations

## Trust model

The engine synthesizes keyboard and mouse input, which is powerful. Only the
local user who runs it may control it. There is no network listener,
telemetry, account or update channel. The UI, configuration files and IPC
messages are treated as untrusted input:

- every request is size-bounded, schema-checked (unknown commands and fields
  are rejected) and versioned;
- the clicker settings are a handful of bounded numbers (interval ≥ 10 ms,
  or ≥ 1 ms only with "fast speeds" explicitly allowed; hold shorter than the
  interval; click and time limits capped). There are no sequences, no
  scripting and no shell execution, and the clicker can never press the
  emergency-stop key;
- the config file is the only file Pulse reads; files larger than 1 MiB are
  refused, and unknown keys or any other schema version are rejected,
  not ignored.

## IPC socket

- `$XDG_RUNTIME_DIR/pulse-macro/` is created `0700` and must be owned by the
  engine's user (the engine tightens its mode or refuses to start).
- `engine.sock` is `0600`; peers are checked with `SO_PEERCRED` and other
  UIDs are disconnected unread.
- A `flock` on `engine.lock` prevents two engines. A leftover socket is
  removed only while that lock is held, and only if it is a socket owned by
  this user. Anything else at that path makes startup fail. Never use blind
  deletion.
- There is no fallback to `/tmp` when `XDG_RUNTIME_DIR` is missing.

*Not covered by automated tests:* rejecting a peer with a different UID needs
a second account. Manual check: `sudo -u nobody socat -
UNIX-CONNECT:/run/user/1000/pulse-macro/engine.sock` must be refused by the
directory permissions, and is also closed by the peer-credential check.

## XTest

XTest uses your X11 session credentials (`DISPLAY`, `XAUTHORITY`) and needs
no extra permission. The X11 security model lets any connected client inject
input; Pulse does not make it more or less private. On Xwayland the XTest
backend refuses to start: it would only reach Xwayland windows. (There is a
diagnostics-only `--allow-xwayland` override.)

## uinput

Any user with write access to `/dev/uinput` can synthesize system-wide
input. Pulse needs **write access to `/dev/uinput` only**: it never opens
physical `/dev/input/event*` devices, because hotkeys come from X11.

Check whether you already have access (many desktops grant an ACL, e.g.
through Steam's `uaccess` rule):

```bash
getfacl /dev/uinput      # look for user:<you>:rw-
pulse-cli backends       # "uinput ... Ready" when writable
```

Otherwise use the narrow, opt-in group rule in
[`packaging/udev/90-pulse-uinput.rules.example`](../packaging/udev/90-pulse-uinput.rules.example):

```bash
sudo groupadd --system pulse-uinput
sudo usermod -aG pulse-uinput "$USER"
sudo install -Dm644 packaging/udev/90-pulse-uinput.rules.example /etc/udev/rules.d/90-pulse-uinput.rules
echo uinput | sudo tee /etc/modules-load.d/pulse-uinput.conf
sudo udevadm control --reload && sudo udevadm trigger --name-match=uinput
# log out and back in
```

Do **not** `chmod 666 /dev/uinput`, do not add yourself to the broad `input`
group for this (it also grants reading physical keyboards), and do not run
Pulse or its UI as root.

The virtual devices are named `Pulse Virtual Keyboard` and
`Pulse Virtual Mouse`, use `BUS_VIRTUAL` with vendor 0, and advertise only the
keys/buttons/axes Pulse emits (no autorepeat). They do not impersonate
hardware.

## Stuck-input limits

- On normal stop, emergency stop, focus loss, duration end, backend switch,
  `SIGTERM`/`SIGINT`/`SIGHUP` and engine shutdown, Pulse releases everything
  its ledger says it pressed, and cross-checks the backend's own ledger.
- Before closing an XTest connection (shutdown, input-method switch), Pulse
  waits for the X server to confirm it processed the releases. The X server
  can discard requests from a client that disconnects; without this
  round trip a just-sent release was lost in 2 of 20 tests, leaving a button
  stuck (regression test `release_survives_immediate_disconnect`).
- A panic in the runner unwinds (`panic = "unwind"` is kept on purpose) and
  the backend's `Drop` releases held input.
- **`SIGKILL` cannot be handled.** With uinput, the kernel releases keys of
  a destroyed device when the process dies and its fd closes. With XTest,
  the X server keeps synthetic state: a killed engine can leave a button
  logically down until you press and release it physically. The default
  click (short press/release) keeps this window small.
- If a release fails, the engine enters `Error` and says that input may be
  stuck; press the affected key or button physically.
- Pulse releases only what it pressed. With XTest the server merges physical
  and synthetic state, so avoid having the clicker repeat a key you are physically
  holding.

## Game use

Pulse contains no anti-cheat evasion, no process injection, no memory
access and no stealth features. Roblox's community standards prohibit
cheating and disruptive automation; test only in experiences you own or have
permission to automate.
