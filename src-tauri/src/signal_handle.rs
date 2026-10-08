use crate::TranscriptionCoordinator;
#[cfg(unix)]
use log::debug;
use log::warn;
use tauri::{AppHandle, Manager};

#[cfg(unix)]
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM, SIGUSR1, SIGUSR2};
#[cfg(unix)]
use signal_hook::iterator::Signals;
#[cfg(unix)]
use std::thread;

/// Send a transcription input to the coordinator.
/// Used by signal handlers, CLI flags, and any other external trigger.
pub fn send_transcription_input(app: &AppHandle, binding_id: &str, source: &str) {
    if let Some(c) = app.try_state::<TranscriptionCoordinator>() {
        c.send_input(binding_id, source, true, false);
    } else {
        warn!("TranscriptionCoordinator not initialized");
    }
}

#[cfg(unix)]
pub fn setup_signal_handler(app_handle: AppHandle, mut signals: Signals) {
    debug!("Signal handlers registered (SIGUSR1, SIGUSR2, SIGTERM, SIGINT, SIGHUP)");
    thread::spawn(move || {
        for sig in signals.forever() {
            let (binding_id, signal_name) = match sig {
                SIGUSR1 => ("transcribe_with_post_process", "SIGUSR1"),
                SIGUSR2 => ("transcribe", "SIGUSR2"),
                SIGTERM | SIGINT | SIGHUP => {
                    exit_on_signal(&app_handle, sig);
                    return;
                }
                _ => continue,
            };
            debug!("Received {signal_name}");
            send_transcription_input(&app_handle, binding_id, signal_name);
        }
    });
}

/// A termination signal is a deliberate end — `kill`, Ctrl-C in a dev
/// terminal, a closing terminal — so any meeting recording is finalised on
/// disk and marked as ended before the process goes. A watchdog thread
/// forces the exit if the finalise itself hangs: the signal must always
/// end the process, recording or not.
#[cfg(unix)]
fn exit_on_signal(app_handle: &AppHandle, sig: i32) {
    let name = match sig {
        SIGTERM => "SIGTERM",
        SIGINT => "SIGINT",
        SIGHUP => "SIGHUP",
        _ => "signal",
    };
    warn!("Received {name} — finalising any recording and exiting");
    thread::spawn(|| {
        thread::sleep(std::time::Duration::from_secs(5));
        std::process::exit(1);
    });
    #[cfg(target_os = "macos")]
    if let Some(meetings) =
        app_handle.try_state::<std::sync::Arc<crate::managers::meeting::MeetingManager>>()
    {
        meetings.finalize_for_exit(name);
    }
    log::info!("Lark exiting");
    std::process::exit(0);
}
