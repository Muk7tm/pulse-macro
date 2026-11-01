//! `pulse-test-receiver`: a controlled X11 window that records the input it
//! receives, to check Pulse against a controlled receiver before a game.
//!
//! Select it as Pulse's target (`pulse-cli target capture`), start clicking,
//! then compare "received" here with "submitted" in `pulse-cli status`.
//! Prints one JSON summary when the window is closed or after `--seconds`.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, PropMode, WindowClass};
use x11rb::wrapper::ConnectionExt as _;

const USAGE: &str = "usage: pulse-test-receiver [--display :N] [--seconds N] [--verbose]

Opens a test window on the X display (default: $DISPLAY) and counts the
clicks and key presses it receives. Prints a JSON summary when the window is
closed or after --seconds.";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    // Never open a window on the desktop for an argument this tool does not
    // understand (such as --help).
    let known = |a: &str| matches!(a, "--display" | "--seconds" | "--verbose");
    let mut it = args.iter().skip(1);
    while let Some(a) = it.next() {
        if a == "--help" || a == "-h" {
            println!("{USAGE}");
            return Ok(());
        }
        if !known(a) {
            anyhow::bail!("unknown argument {a}\n{USAGE}");
        }
        if a != "--verbose" {
            it.next();
        }
    }
    let display = args
        .iter()
        .position(|a| a == "--display")
        .and_then(|i| args.get(i + 1))
        .cloned();
    let seconds: Option<u64> = args
        .iter()
        .position(|a| a == "--seconds")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok());
    let verbose = args.iter().any(|a| a == "--verbose");

    let (conn, screen) = x11rb::connect(display.as_deref()).context("connect to X11")?;
    let s = conn.setup().roots[screen].clone();
    let win = conn.generate_id()?;
    conn.create_window(
        s.root_depth,
        win,
        s.root,
        100,
        100,
        480,
        320,
        0,
        WindowClass::INPUT_OUTPUT,
        s.root_visual,
        &CreateWindowAux::new().background_pixel(0x0011_1827).event_mask(
            EventMask::BUTTON_PRESS
                | EventMask::BUTTON_RELEASE
                | EventMask::KEY_PRESS
                | EventMask::KEY_RELEASE
                | EventMask::FOCUS_CHANGE,
        ),
    )?;
    let title = b"Pulse Test Receiver";
    conn.change_property8(PropMode::REPLACE, win, AtomEnum::WM_NAME, AtomEnum::STRING, title)?;
    conn.change_property8(
        PropMode::REPLACE,
        win,
        AtomEnum::WM_CLASS,
        AtomEnum::STRING,
        b"pulse-test-receiver\0PulseTestReceiver\0",
    )?;
    let protocols = conn.intern_atom(false, b"WM_PROTOCOLS")?.reply()?.atom;
    let delete = conn.intern_atom(false, b"WM_DELETE_WINDOW")?.reply()?.atom;
    conn.change_property32(PropMode::REPLACE, win, protocols, AtomEnum::ATOM, &[delete])?;
    conn.map_window(win)?;
    conn.flush()?;
    eprintln!("Pulse Test Receiver: window {win:#x}, WM_CLASS PulseTestReceiver. Close it to print the summary.");

    let start = Instant::now();
    let mut presses = [0u64; 6];
    let mut releases = [0u64; 6];
    let mut keys_down = 0u64;
    let mut keys_up = 0u64;
    let mut last_press: Option<u32> = None;
    let mut intervals_ms: Vec<u32> = Vec::new();
    let mut focus_changes = 0u64;
    loop {
        if let Some(limit) = seconds {
            if start.elapsed() >= Duration::from_secs(limit) {
                break;
            }
        }
        let Some(ev) = conn.poll_for_event()? else {
            std::thread::sleep(Duration::from_millis(1));
            continue;
        };
        match ev {
            Event::ButtonPress(e) => {
                presses[(e.detail as usize).min(5)] += 1;
                if e.detail == 1 {
                    if let Some(prev) = last_press {
                        intervals_ms.push(e.time.wrapping_sub(prev));
                    }
                    last_press = Some(e.time);
                }
                if verbose {
                    println!("{} press   button {}", e.time, e.detail);
                }
            }
            Event::ButtonRelease(e) => {
                releases[(e.detail as usize).min(5)] += 1;
                if verbose {
                    println!("{} release button {}", e.time, e.detail);
                }
            }
            Event::KeyPress(e) => {
                keys_down += 1;
                if verbose {
                    println!("{} key down {}", e.time, e.detail);
                }
            }
            Event::KeyRelease(e) => {
                keys_up += 1;
                if verbose {
                    println!("{} key up   {}", e.time, e.detail);
                }
            }
            Event::FocusIn(_) | Event::FocusOut(_) => focus_changes += 1,
            Event::ClientMessage(e) if e.data.as_data32()[0] == delete => break,
            _ => {}
        }
    }
    intervals_ms.sort_unstable();
    let pct = |p: f64| -> Option<u32> {
        (!intervals_ms.is_empty())
            .then(|| intervals_ms[((intervals_ms.len() as f64 * p).ceil() as usize).clamp(1, intervals_ms.len()) - 1])
    };
    let summary = serde_json::json!({
        "received_left_presses": presses[1],
        "received_left_releases": releases[1],
        "received_middle_presses": presses[2],
        "received_right_presses": presses[3],
        "received_key_presses": keys_down,
        "received_key_releases": keys_up,
        "focus_changes": focus_changes,
        "left_press_interval_ms": {
            "count": intervals_ms.len(),
            "p50": pct(0.5),
            "p99": pct(0.99),
            "min": intervals_ms.first(),
            "max": intervals_ms.last(),
            "resolution": "X server timestamps (1 ms)"
        },
        "stuck_left_button": presses[1] != releases[1],
        "elapsed_s": start.elapsed().as_secs_f64(),
    });
    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(())
}
