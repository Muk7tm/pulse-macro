# Sober compatibility test plan

**Status: first informal result only.** Except for the result below, everything
is `not tested` until someone runs it in a real Sober session. Do not infer
Sober behaviour from the Xvfb or uinput integration tests: those prove that
events are *submitted*, not that Roblox *acts* on them.

## Results log

### 2026-11-01: first in-game run (user-reported, informal)

| Item | Value |
|---|---|
| Sober | 1.8.0 (commit 7b6430c7…), Flatpak |
| System | CachyOS, kernel 7.2.11-zen1, native X11 (LXQt), NVIDIA RTX 3060 |
| Backend | XTest |
| Settings | Quick Clicker: left button, 100 ms interval (10 CPS), 20 ms hold, toggle |
| Target | `window_class = "org.vinegarhq.Sober"`, picked with the 3 s window picker; restriction on |
| Engine counters | 15 runs, 2298 clicks submitted, 0 missed deadlines, 0 backend errors, every stop `release_ok=true` |
| Outcome | **User confirmed the clicks worked in the game.** |

This partly covers test 1 (basic clicks, XTest) below. It was not a
controlled run: no experience name, no submitted-vs-acted count, and no
chat, menu, first-person, focus-loss or frame-time checks were recorded. The
activation hotkey during this run was the letter `F` rather than the
default `F6`. Note that a letter key also toggles the clicker while you type
in chat. uinput was not tried in-game.

Use only an experience you own or have explicit permission to automate.
Stop immediately if chat focus, input lag or frame times deteriorate.

## Preparation

```bash
flatpak info org.vinegarhq.Sober | grep -E 'Version|Commit'
uname -a; Xorg -version 2>&1 | head -3; glxinfo -B | grep -E 'renderer|version'
pulse-engine &                     # or: systemctl --user start pulse-engine
pulse-cli backends                 # both should be Ready (uinput may need setup)
```

1. Launch Sober and join the test experience.
2. Select the target **explicitly**: run `pulse-cli target capture --delay 3`
   and focus the Sober window within 3 s, or use Settings → Target in the UI.
   Record the class and title Pulse reports; never hardcode them.
3. Confirm with `pulse-cli status` that `focused=true` while Sober is active
   and `false` after alt-tab.
4. Warm-up: play normally for 2 minutes with physical input.

## Procedure (repeat for `xtest`, then `uinput`)

| # | Test | Steps | Pass criteria | XTest | uinput |
|---|---|---|---|---|---|
| 1 | Basic clicks | Quick Clicker 100 ms / 20 ms hold, toggle F6, 30 s | Every click acts in-world; F6 stops at once | works (user-reported, informal, 2026-11-01) | not tested |
| 2 | Submitted vs acted | Count in-game actions vs `submitted_clicks` | Equal within observation error | not tested | not tested |
| 3 | Menus/inventory | Click UI buttons and inventory slots | Each click registers once | not tested | not tested |
| 4 | Chat focus | Run 30 s, stop, press `/` and type | Chat opens and receives text normally | not tested | not tested |
| 5 | Text field focus | Click in a text box while running | Behaviour documented | not tested | not tested |
| 6 | First-person / mouse lock | Clicks in first person | Camera unaffected; clicks register | not tested | not tested |
| 7 | Shift lock | Clicks with shift lock on | Documented, even if unsupported | not tested | not tested |
| 8 | 100 start/stop cycles | F6 ×200 | No stuck button (receiver/game check) | not tested | not tested |
| 9 | Focus loss | Alt-tab while running | Run stops (`reason: target window lost focus`) | not tested | not tested |
| 10 | Minimize / maximize / resolution change | While running | Stops on focus loss, no clicks elsewhere | not tested | not tested |
| 11 | Sober crash / relaunch | Kill Sober while Pulse is ready | Target rediscovered by class after relaunch | not tested | not tested |
| 12 | Emergency stop | F8 while running | Stops and releases at once | not tested | not tested |
| 13 | Hold mode | Hold F6 | Runs only while held | not tested | not tested |
| 14 | Shorter intervals | 50 ms, then 20 ms, only if 1–13 pass | Stop at the first chat/focus/lag problem | not tested | not tested |

The 2026-08-13 report (vinegarhq/sober#2429) of chat-focus problems below
~100 ms with XClicker is a reproduction lead: repeat test 4 at every interval
tried in test 14 and record the smallest stable interval per backend.

## Frame-time measurement

Run each configuration ≥ 3 times in A/B/A order, same scene and camera, at
least 60 s after a warm-up. Use MangoHud if it works with Sober's renderer
(`MANGOHUD=1 flatpak run org.vinegarhq.Sober`, log to CSV); otherwise record
the method used.

| Configuration | Avg FPS | 1 % low | p95 frame time | p99 frame time |
|---|---|---|---|---|
| Baseline: engine not running | not measured | | | |
| Engine idle, UI closed | not measured | | | |
| Engine idle, UI open | not measured | | | |
| XTest 100 ms, UI closed | not measured | | | |
| uinput 100 ms, UI closed | not measured | | | |

Also capture `pulse-bench process --pid $(pgrep -x pulse-engine) --seconds 60`
for every active configuration.

## Record

Sober version and commit, Roblox version (if shown), kernel, Xorg version,
window manager/compositor, GPU and driver, CPU governor, experience name,
intervals tested, and any unsupported cases. Put results and raw logs in
`benchmarks/raw/<date>-sober/` and update this file with the measured values
(never estimates).
