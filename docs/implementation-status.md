# Implementation status

What is done and how it was verified, by development phase. ✅ done and
verified by the named test or measurement · 🟡 partially done · ⬜ not done ·
🔬 needs a real Sober session.

Test counts are from the 2026-11-01 run of `cargo test --workspace` with an
isolated Xvfb (`PULSE_TEST_DISPLAY`) and `PULSE_TEST_UINPUT=1`: 137 passed,
0 failed, 0 skipped.

## Phase 0 — Discovery

| Item | Status | Evidence |
|---|---|---|
| Native X11 confirmed | ✅ | XDG_SESSION_TYPE=x11; X server queried: no XWAYLAND extension, XTEST 2.2, XInput 2.x, XKB |
| Kernel / GPU / driver recorded | ✅ | [benchmarks.md](benchmarks.md#environment-2026-11-01) |
| Sober installed | ✅ | org.vinegarhq.Sober 1.8.0; controlled test experience 🔬 |
| `/dev/uinput` permission model | ✅ | user ACL present (`getfacl`); narrow udev rule example provided |
| Slint license/renderer decision | 🟡 | Pulse is MIT; Slint is used under its royalty-free desktop license, whose `AboutSlint` credit is in the About box; renderer comparison on a real GPU ⬜ |
| Baseline FPS capture procedure | 🟡 | documented in [sober-test-plan.md](sober-test-plan.md); not executed 🔬 |

## Phase 1 — Pure engine foundation

| Item | Status | Evidence |
|---|---|---|
| Normalized input model (physical keys, mouse buttons) | ✅ | `pulse-core/src/input.rs` + tests |
| Held-input ledger (fixed size, no allocation) | ✅ | `held_inputs.rs` tests |
| Clicker validation (interval/hold/limits, hotkey conflicts, mouse or key target) | ✅ | `clicker.rs`, `validation.rs`, `pulse-config` plan tests |
| Cancellation generation and safe stop | ✅ | executor cancel tests; engine `cancellation_tests` |
| Absolute-deadline scheduler, skip/rebase without bursts | ✅ | 10 executor unit tests (`scheduler.rs`) |
| Instant clicks (hold 0) up to 1000 CPS, no bursts | ✅ | `instant_clicks_reach_1000_per_second` (executor and engine); property tests cover hold 0 and 1 ms; XTest end to end on Xvfb: 2002 of 2002 clicks received ([benchmarks.md §3](benchmarks.md#3-submission-cost-and-latency)) |
| Fake clock + property tests | ✅ | 5000 random clicker runs (mouse and keyboard targets) with stalls/cancels + 1 h drift simulation |

## Phase 2 — XTest backend

| Item | Status | Evidence |
|---|---|---|
| Persistent X11 connection | ✅ | `connection_is_reused_across_many_events` (3000 events, one connection) |
| XTEST negotiation, missing-extension and Xwayland errors | ✅ | `open()`; `cannot_connect_error_is_actionable` |
| Button/key mapping (verified evdev+8, keysym fallback) | ✅ | keymap unit tests (incl. AZERTY-style layout); Xvfb delivery test |
| Flush per batch, no per-event round trip; bounded x11rb bookkeeping | ✅ | encoding test vs x11rb serializer; sync every 256 requests; memory soak |
| Native test receiver | ✅ | `pulse-test-receiver` |
| XInput2 raw hotkeys, self-trigger suppression | ✅ | `x11_engine_tests` (7 tests) |
| Mouse side buttons (Mouse4/Mouse5) as hotkeys; raw button events only from physical pointers, only while one is set | ✅ | `mouse_side_buttons_work_as_hotkeys`, `button_selection_follows_hotkeys_and_hotplug`, `side_buttons_work_like_keys` |
| XGrabKey evaluation | ⬜ | trade-off documented in [architecture.md](architecture.md#hotkeys-and-self-trigger-suppression) |
| Fault handling and close; release on drop | ✅ | `drop_releases_held_inputs`, `sigterm_while_held_releases_and_exits_cleanly` |
| Releases confirmed by the server before disconnecting | ✅ | `release_survives_immediate_disconnect` (was 2/20 stuck without the round trip, now 0/60) |
| Keyboard-key clicker | ✅ | `keyboard_key_clicker_presses_the_key`; XTest key delivery on Xvfb |

## Phase 3 — uinput backend

| Item | Status | Evidence |
|---|---|---|
| Explicit availability probe (no device creation) | ✅ | `probe_uinput`; `pulse-cli backends` |
| Keyboard + mouse virtual devices, minimal capabilities, BUS_VIRTUAL | ✅ | `devices_have_expected_identity_and_capabilities` |
| Code mapping and SYN framing (chords share a frame) | ✅ | `events_have_correct_codes_values_and_syn_framing` |
| Persistent handles, reuse | ✅ | `devices_are_created_once_and_reused` |
| Permission guide / udev example | ✅ | [security.md](security.md#uinput), `packaging/udev/` |
| Teardown, no device after exit | ✅ | `release_all_and_drop_cleanup`; live check: 0 devices left after switching back |
| Registration on the real X server before Ready | ✅ | live `:0` check (no injection): switch to uinput 246 ms incl. X registration |
| Events reach an X client through libinput | ⬜ | Xvfb has no input hotplug; requires a manual live test with `pulse-test-receiver` |

## Phase 4 — Runtime backend selection

| Item | Status | Evidence |
|---|---|---|
| Choice stored in config, persists across restart | ✅ | `xtest_to_uinput_while_idle_switches_and_persists` |
| Initialization and rollback | ✅ | `failed_switch_restores_healthy_previous_backend`, `failed_switch_back_restores_uinput` |
| Reject switch mid-run | ✅ | `switch_is_rejected_while_running` |
| Requested vs active reported separately | ✅ | `status.desired_backend` / `active_backend`; `failed_startup_backend_never_reports_ready` |
| Inactive backend does no work | ✅ | `inactive_backend_receives_no_events`; inactive backend holds no resources |
| Dynamic dispatch vs specialized loop | ✅ | measured ~0.6 ns/call difference ([benchmarks.md §4](benchmarks.md#4-hot-path-allocations-and-dispatch)): no specialization needed |

## Phase 5 — IPC, settings, CLI

| Item | Status | Evidence |
|---|---|---|
| Unix socket, 0700 dir / 0600 socket, SO_PEERCRED | ✅ | `socket_and_directory_permissions_are_private`; other-UID rejection is a manual check ([security.md](security.md#ipc-socket)) |
| Versioned commands, errors, bounded frames | ✅ | protocol tests; `version_mismatch_and_malformed_requests_are_rejected_safely`, `oversized_frame_is_rejected_and_disconnected` |
| Atomic persistence, strict schemas (other versions refused) | ✅ | config tests (12), `unknown_fields_and_versions_are_rejected` |
| CLI status/start/stop/estop/set/backend/target/diagnostics | ✅ | `pulse-cli` unit tests for `set`; live smoke run on Xvfb |
| Settings apply immediately, never mid-run; invalid settings rejected with field paths | ✅ | `settings_apply_immediately_but_not_while_clicking`, `invalid_settings_are_rejected_with_field_paths_and_not_saved` |
| Single-instance lock, stale-socket handling | ✅ | `second_instance_is_refused`, `stale_socket_is_replaced_and_foreign_file_is_refused` |
| Engine independent of UI | ✅ | `ui_disconnect_does_not_stop_a_running_macro` |
| Plain-language start blockers (no input method, invalid settings, no game window picked) | ✅ | `failed_startup_backend_never_reports_ready`, `hotkey_without_a_picked_window_explains_instead_of_clicking` |

## Phase 6 — Sober validation 🔬

All items are **not tested**; see [sober-test-plan.md](sober-test-plan.md). The
target picker and focus rules are implemented and tested on Xvfb
(`focus_guard_blocks_start_and_cancels_on_focus_loss`).

## Phase 7 — Slint UI

| Item | Status | Evidence |
|---|---|---|
| Separate process, IPC only; no `pulse-input`/engine dependency | ✅ | `cargo tree -p pulse-ui` (no pulse-input/evdev); engine tree has no Slint |
| One compact window (400×680): status, Start/Stop, Clicking, Game window, More options, About, Quit | ✅ | screenshots on Xvfb and on the live desktop |
| Click = left/right/middle **or keyboard key** (capture or list), speed with clicks/sec, hold, Toggle/Hold, start/stop key, emergency key shown | ✅ | Xvfb walkthrough; settings saved and reflected by the engine |
| Settings apply automatically; invalid values shown inline and never sent; locked while clicking | ✅ | 12 UI unit tests (form↔config, issue paths) + Xvfb walkthrough |
| Plain-language states and blockers ("Pick the game window first", "Not in front", live click counter, last run) | ✅ | screenshots of ready / clicking / after-run / blocked states |
| Pick window (3 s countdown), "Only click in this window" with warning when off | ✅ | success path with a fake window manager on Xvfb; picking Pulse itself refused |
| Input method choice with availability and fix text (More options) | 🟡 | shown and working for XTest; switching to uinput from the window not run live (it creates host devices) |
| Starts the engine automatically; detects an older engine and offers "Restart Pulse" | ✅ | auto-start verified on Xvfb and live; restart path verified against a protocol-1 stand-in |
| Closing the window or pressing Quit Pulse asks: keep running in the background or quit (Escape cancels; no question when Pulse isn't running) | ✅ | verified on Xvfb with openbox: dialog, Escape, Keep running (engine stays), Quit Pulse (both exit), engine already stopped (window just closes) |
| Quitting from the dialog stops engine and window | ✅ | verified on Xvfb (title-bar close and the Quit Pulse button) |
| Start key from a mouse side button (press it over the key box, or pick Mouse4/Mouse5 from the list) | ✅ | Xvfb walkthrough (XTest button 8 over the key box); the "Key to press" list stays keyboard-only |
| Escape closes the About box | ✅ | verified with window focus |
| Black-and-white theme (soft red only for errors); number fields drawn from the theme so no widget-style accent shows | ✅ | screenshots on Xvfb: ready, clicking, switch on, dialog, inline error, text selection; number field +/−, typing, Enter, Up/Down, wheel and out-of-range typing checked against the saved config |
| Software vs FemtoVG comparison | 🟡 | both measured idle on Xvfb ([benchmarks.md §6](benchmarks.md#6-ui-process-optional-separate)); real GPU not measured |
| Typed key capture on non-US layouts | 🟡 | maps to US physical positions; the ▾ key list is exact |

## Phase 8 — Performance, packaging, release

| Item | Status | Evidence |
|---|---|---|
| Idle/active CPU, wakeups, RSS | ✅ | [benchmarks.md §1](benchmarks.md#1-engine-cpu-wakeups-and-memory-window-closed) |
| Timer jitter, trigger latency | ✅ | [benchmarks.md §2–3](benchmarks.md#2-timer-accuracy) |
| XTest vs uinput comparison | 🟡 | submission cost measured; game-side comparison 🔬 |
| Sober FPS / frame time A/B | ⬜ 🔬 | — |
| Security audit / license check | 🟡 | License MIT; all dependencies permissive except Slint (royalty-free option, credit in the About box); `cargo audit` / `cargo deny` not installed |
| `install.sh`: per-user build and install, apps-menu entry, update, uninstall | ✅ | run in a throwaway prefix: install, installed window starts the installed engine, restart prompt (yes and no), uninstall stops Pulse and removes every file |
| systemd user unit, PKGBUILD, desktop entry | ✅ | `systemd-analyze --user verify` clean; `desktop-file-validate` clean; PKGBUILD not yet run through `makepkg` |
| Docs, troubleshooting, uninstall | ✅ | README, docs/ |

## Release gates

| Gate | Status |
|---|---|
| A — correctness foundation | ✅ (engine without GUI, start/stop, no stuck inputs on cancellation, bounded backlog, validated atomic config, UI never emits clicks) |
| B — XTest | ✅ on Xvfb + live connection checks; in-game behaviour 🔬 |
| C — uinput | 🟡 virtual devices verified via grabbed nodes; delivery through the desktop input stack ⬜ |
| D — backend selection | ✅ engine/CLI and window; window → uinput switch not exercised live |
| E — Sober | 🟡 XTest clicks confirmed working in-game by the user (informal, 2026-11-01); controlled matrix, uinput and frame times ⬜ |
| F — stability/performance | 🟡 start/stop ×1000, cancellation, 5-minute memory soak; 8 h idle / 1 h active soaks ⬜; frame times ⬜ |
| G — release usability | 🟡 compact window verified on Xvfb and running on the live desktop; MIT license; not yet published |

## Known limitations

- SIGKILL with XTest leaves synthetic state pressed (verified by
  `sigkill_while_held_leaves_xtest_state_documented_limitation`).
- Suspend is detected on the first wakeup after resume, through the
  BOOTTIME/MONOTONIC offset (not from logind); not tested with a real suspend.
- No desktop-lock detection (focus loss covers lock screens that take focus).
- Single-key hotkeys only; no modifier combinations.
- UI hotkey capture maps typed characters to US physical positions; on other
  layouts use the key dropdown (F-keys are unaffected).
- An engine started from the UI discards its log output (use the systemd
  unit or a terminal to keep logs).
