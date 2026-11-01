# Pulse control protocol, version 2

Implemented in `crates/pulse-protocol`. The engine rejects other versions
with `unsupported_version`.

## Transport and framing

- Unix domain stream socket `$XDG_RUNTIME_DIR/pulse-macro/engine.sock` (mode
  `0600`, in a `0700` directory owned by the user). No TCP.
- Each peer is authenticated with `SO_PEERCRED`; connections from other UIDs
  are closed without reading.
- Frame = 4-byte **big-endian** body length + UTF-8 JSON body. Maximum body
  size is 256 KiB. An oversized header gets a `bad_request` response, then
  the connection is closed.
- At most 16 concurrent clients; each client's pending output is capped at
  4 MiB, and a slower client is dropped.

## Request

```json
{
  "protocol_version": 2,
  "request_id": "a1b2c3",
  "command": "select_backend",
  "payload": {"backend": "uinput"}
}
```

`request_id` is at most 64 characters and is echoed in the response.
Commands without parameters omit `payload`.

| `command` | `payload` | Result `data.kind` |
|---|---|---|
| `hello` | — | `hello` (engine version, protocol version, pid) |
| `status` | — | `status` |
| `list_backends` | — | `backends` (probes without creating devices) |
| `select_backend` | `{"backend": "xtest"\|"uinput"}` | `backend_switched` |
| `get_config` | — | `config` |
| `set_config` | `{"config": Config}` | `config` (validated, saved atomically, applied at once; `engine.backend` is ignored, use `select_backend`) |
| `start` | — | `started` (`run_id`) |
| `stop` | — | `stopped` (waits ≤ 1 s for cleanup; `null` if nothing ran) |
| `emergency_stop` | — | `ack` (immediate) |
| `set_target` | `{"rule": {"window_class": "...", "window_title_pattern": "..."}}` | `ack` |
| `active_window` | — | `window` |
| `list_windows` | — | `windows` |
| `diagnostics` | — | `diagnostics` |
| `reset_diagnostics` | — | `ack` |
| `subscribe` | — | `ack`; this connection then receives events |
| `reconnect` | — | `ack` (retry X11 and the input method after a fault) |
| `shutdown` | — | `ack`, then the engine exits |

`Config` is the same structure as `config.toml` (see
[`configs/example.toml`](../configs/example.toml)).

## Response

```json
{
  "type": "response",
  "protocol_version": 2,
  "request_id": "a1b2c3",
  "ok": true,
  "state": "ready",
  "active_backend": "uinput",
  "data": {"kind": "backend_switched", "value": {"outcome": "switched", "active_backend": "uinput", "detail": "...", "warnings": []}}
}
```

On failure `ok` is `false` and `error` is set:

```json
{"code": "validation_failed", "message": "hold (150 ms) must be shorter than the interval (100 ms)", "issues": [{"severity": "error", "path": "clicker.hold_ms", "message": "..."}]}
```

Error codes: `bad_request`, `unsupported_version`, `unauthorized`,
`invalid_state`, `validation_failed`, `not_found`, `backend_unavailable`,
`x11_unavailable`, `io`, `internal`. Every response carries the current
`state` (`booting`, `idle`, `ready`, `running`, `stopping`, `error`,
`shutting_down`) and `active_backend`.

### Status

Besides state, input method, counters and session facts, `status` returns:

- `clicker`: the clicker settings in effect;
- `can_start` and `start_blocker`: whether the hotkey can start clicking, and
  if not, why, in plain language ("Pick the game window first", "No input
  method is ready (…)", "Fix the settings: …"). Which window is in front is
  not part of this, because it changes constantly. Starting while the game
  window is not in front fails with "The game window is not in front…";
- `warnings`: non-blocking notes about the settings (e.g. intervals below 100 ms);
- `require_focus`, `target`, `target_focused`, `active_window`.

## Events (after `subscribe`)

```json
{"type": "event", "protocol_version": 2, "event": {"type": "state_changed", "state": "running", "reason": "run 3 started", "run_id": 3}}
```

Event types: `state_changed`, `target_focus`, `fault` (including rejected
hotkey starts, with the reason), `config_changed`. Events are sent on
transitions only, never per click. Live counters come from `status`, which
the UI polls at 2 Hz only while clicking and visible.

## Semantics

- `stop` and `emergency_stop` are idempotent.
- Whenever `can_start` is true, the hotkey (or `start`) starts clicking.
- Settings, the game window and the input method cannot change while
  clicking (`invalid_state`).
- `submitted_clicks` counts presses submitted to the input method (mouse
  clicks or key presses), not presses the game registered.
