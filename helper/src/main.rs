//! vsblox-helper: keeps the Roblox Studio window pinned over a VS Code panel.
//!
//! The VS Code extension starts this process and talks to it over stdin/stdout,
//! one JSON object per line.
//!
//! stdin  (commands): {"cmd":"show"} {"cmd":"hide"} {"cmd":"recalibrate"} {"cmd":"detach"}
//!                    {"cmd":"size","width":1920,"height":1080}  (omit both to fit the panel)
//!                    {"cmd":"title_bar","hide":true}
//! stdout (events):   {"event":"status","state":"...","detail":"..."}
//!
//! The panel paints small marker squares in its corners. We find the panel by
//! looking for them in a capture of the VS Code window, then place Studio on top.

use std::io::BufRead;
use std::sync::mpsc;

#[cfg(windows)]
mod win;
#[cfg(windows)]
use win as platform;

#[derive(serde::Deserialize, Debug)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Cmd {
    Show,
    Hide,
    Recalibrate,
    Detach,
    Size {
        width: Option<i32>,
        height: Option<i32>,
    },
    TitleBar {
        hide: bool,
    },
}

pub fn emit(state: &str, detail: &str) {
    let line = serde_json::json!({ "event": "status", "state": state, "detail": detail });
    println!("{line}");
}

struct Args {
    host_exe: String,
    probe: bool,
}

fn parse_args() -> Args {
    let mut args = Args { host_exe: "Code.exe".into(), probe: false };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--host-exe" => args.host_exe = it.next().unwrap_or(args.host_exe),
            "--probe" => args.probe = true,
            _ => {}
        }
    }
    args
}

fn main() {
    let args = parse_args();
    platform::init();

    if args.probe {
        platform::probe(&args.host_exe);
        return;
    }

    // Read commands on a thread. `None` means stdin closed: the extension went away.
    let (tx, rx) = mpsc::channel::<Option<Cmd>>();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Cmd>(&line) {
                Ok(cmd) => {
                    if tx.send(Some(cmd)).is_err() {
                        return;
                    }
                }
                Err(e) => emit("error", &format!("bad command {line:?}: {e}")),
            }
        }
        let _ = tx.send(None);
    });

    platform::run(args.host_exe, rx);
    emit("detached", "");
}
