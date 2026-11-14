//! [fork] Brings parked UAC prompts to the front.
//!
//! When the program asking for elevation is not in the foreground (an installer started by winget from a
//! terminal, an app launched in the background, ...), Windows doesn't show the prompt: it parks it as a
//! flashing taskbar button owned by `consent.exe` and waits for the user to click it. Seelen UI hides the
//! native taskbar, and its dock skips windows of processes it can't open (`consent.exe` runs as SYSTEM),
//! so the prompt was unreachable.
//!
//! A prompt shown the normal way lives on the secure desktop, where our WinEvent hook can't see it, so any
//! `consent.exe` window that shows up on the user's desktop is a parked one. It is activated once, which
//! makes `consent.exe` continue and show the prompt as usual (on the secure desktop when enabled).
//! Activation goes through the elevated SLU service when Seelen itself is not allowed to do it.

use std::{
    collections::HashSet,
    path::Path,
    sync::{
        OnceLock,
        mpsc::{Sender, channel},
    },
    time::Duration,
};

use slu_ipc::messages::SvcAction;

use crate::{
    cli::ServicePipe,
    error::{Result, ResultLogExt},
    platform::{
        hook::{HookManager, WindowEvent},
        window::Window,
    },
    utils::spawn_named_thread,
    windows_api::WindowsApi,
};

/// System32 executables whose top-level windows on the user's desktop are prompts waiting for the user.
const PROMPT_HOSTS: &[&str] = &["consent.exe"];
/// Activation attempts per prompt, the window may not accept activation right after being shown.
const ACTIVATION_ATTEMPTS: u32 = 5;
const ACTIVATION_RETRY_DELAY: Duration = Duration::from_millis(150);

static QUEUE: OnceLock<Sender<PromptMsg>> = OnceLock::new();

pub fn start() {
    let (sender, receiver) = channel::<PromptMsg>();
    if QUEUE.set(sender.clone()).is_err() {
        return;
    }

    // activation can block (service round trip, retries), so it runs on its own thread
    spawn_named_thread("Fork UAC Prompt Surfacer", move || {
        // windows already activated, so the user can switch away from a prompt without it coming back
        let mut surfaced = HashSet::<isize>::new();
        for msg in receiver {
            match msg {
                PromptMsg::Shown(hwnd) => {
                    let window = Window::from(hwnd);
                    if is_parked_prompt(&window) && surfaced.insert(hwnd) {
                        log::info!("Surfacing parked UAC prompt: {window}");
                        surface(&window).log_error();
                    }
                }
                PromptMsg::Destroyed(hwnd) => {
                    surfaced.remove(&hwnd);
                }
            }
        }
    });

    HookManager::subscribe(|(event, window)| {
        let msg = match event {
            WindowEvent::Shown | WindowEvent::Uncloaked => PromptMsg::Shown(window.address()),
            WindowEvent::Destroyed => PromptMsg::Destroyed(window.address()),
            _ => return,
        };
        if let Some(queue) = QUEUE.get() {
            let _ = queue.send(msg);
        }
    });

    // a prompt parked before Seelen UI started
    match Window::enumerate() {
        Ok(windows) => {
            for window in windows {
                let _ = sender.send(PromptMsg::Shown(window.address()));
            }
        }
        Err(err) => log::error!("Could not enumerate windows: {err:?}"),
    }
}

enum PromptMsg {
    Shown(isize),
    Destroyed(isize),
}

fn is_parked_prompt(window: &Window) -> bool {
    window.is_window()
        && window.is_visible()
        && !window.is_cloaked()
        && window.parent().is_none()
        && is_prompt_host(window.process_id())
}

/// Matched by image path, read without opening the process (we are not allowed to open SYSTEM processes).
fn is_prompt_host(process_id: u32) -> bool {
    let Ok(path) = WindowsApi::get_process_path_by_pid(process_id) else {
        return false;
    };
    let path = Path::new(&path);
    let Some(file_name) = path.file_name().map(|name| name.to_string_lossy()) else {
        return false;
    };
    if !PROMPT_HOSTS
        .iter()
        .any(|host| host.eq_ignore_ascii_case(&file_name))
    {
        return false;
    }
    let system_root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    let system32 = Path::new(&system_root).join("System32");
    path.parent()
        .is_some_and(|dir| dir.as_os_str().eq_ignore_ascii_case(system32.as_os_str()))
}

fn surface(window: &Window) -> Result<()> {
    // routed through the elevated service, we can't open the prompt's process
    window.unminimize()?;

    for _ in 0..ACTIVATION_ATTEMPTS {
        if !window.is_window() || window.is_focused() {
            return Ok(());
        }
        // `focus` already falls back to the service when the foreground window is elevated,
        // the service is also tried directly in case Windows refused the activation to us
        if window.focus().is_err() {
            ServicePipe::request_sync(SvcAction::SetForeground(window.address())).log_error();
        }
        std::thread::sleep(ACTIVATION_RETRY_DELAY);
    }

    if window.is_window() && !window.is_focused() {
        return Err(format!("Could not activate the parked UAC prompt {window}").into());
    }
    Ok(())
}
