//! [fork] Brings parked UAC prompts to the front.
//!
//! When the program asking for elevation is not in the foreground (an installer started by winget from a
//! terminal, an app launched in the background, ...), Windows doesn't show the prompt: it parks it as a
//! flashing taskbar button owned by `consent.exe` and waits for the user to click it. Seelen UI hides the
//! native taskbar, and its dock skips windows of processes it can't open (`consent.exe` runs as SYSTEM),
//! so the prompt was unreachable.
//!
//! A prompt shown the normal way lives on the secure desktop, where our WinEvent hook can't see it, so any
//! `consent.exe` window that shows up on the user's desktop is a parked one.
//!
//! ## Surfacing a parked prompt
//!
//! Restoring the window does not make it usable: the prompt is answered on the secure desktop, so the
//! window only has to become the foreground one. That activation is the whole job, and Windows heavily
//! restricts it:
//!
//! - `ShowWindow(SW_RESTORE)` from the elevated service is refused (`E_ACCESSDENIED`): a High integrity
//!   caller still may not touch a SYSTEM window. It is only a non-fatal restore hint here.
//! - `SetForegroundWindow` (what [`slu_utils::windows::set_foreground`] does) needs foreground rights that
//!   neither Seelen nor its service has for this window.
//! - `SwitchToThisWindow` is what the native taskbar button itself calls. It crosses integrity levels and
//!   needs no foreground permission, so it is the first step of the ladder below.
//!
//! Every step is judged by re-reading the real foreground window, never by a call's return value, and a
//! prompt that can't be activated is retried a bounded number of times before being left alone (the
//! native path was already unavailable to the user, so giving up is better than fighting them).

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        OnceLock,
        mpsc::{Receiver, RecvTimeoutError, Sender, channel},
    },
    time::{Duration, Instant},
};

use slu_ipc::messages::SvcAction;
use windows::Win32::{
    Foundation::HWND,
    UI::WindowsAndMessaging::{SW_RESTORE, SwitchToThisWindow},
};

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
/// Executables outside `%SystemRoot%\System32` never match, so this list can not be used to hijack the
/// prompt of another program. To support another prompt host, add its file name here.
const PROMPT_HOSTS: &[&str] = &["consent.exe"];
/// How long each activation step gets to make the prompt the foreground window
/// (the window may not accept it right after being shown).
const STEP_TIMEOUT: Duration = Duration::from_millis(120);
/// Attempts per prompt before leaving it alone until it is shown again.
const MAX_ATTEMPTS: u32 = 3;
/// Delay before retrying a prompt that could not be activated.
const RETRY_DELAY: Duration = Duration::from_millis(900);
/// Idle wait of the surfacer thread. Only bounds how long it sleeps, every message wakes it up.
const IDLE_WAIT: Duration = Duration::from_secs(3600);

static QUEUE: OnceLock<Sender<PromptMsg>> = OnceLock::new();

pub fn start() {
    let (sender, receiver) = channel::<PromptMsg>();
    if QUEUE.set(sender.clone()).is_err() {
        return;
    }

    // activation blocks (service round trip, timeouts), so it runs on its own thread
    spawn_named_thread("Fork UAC Prompt Surfacer", move || {
        Surfacer::new(receiver).run();
    });

    HookManager::subscribe(|(event, window)| {
        // Only the rare lifecycle events are subscribed: recognizing a prompt host costs a process
        // query per window, so this must not run for every window event of the session.
        let msg = match event {
            WindowEvent::Shown | WindowEvent::Uncloaked => PromptMsg::Seen(window.address()),
            WindowEvent::Destroyed => PromptMsg::Gone(window.address()),
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
                let _ = sender.send(PromptMsg::Seen(window.address()));
            }
        }
        Err(err) => log::error!("Could not enumerate windows: {err:?}"),
    }
}

/// Activation attempts of one prompt that has not been surfaced yet.
struct PendingAttempt {
    attempts: u32,
    /// When the next attempt is due, so a prompt that is still loading is retried later instead of
    /// being polled.
    due: Instant,
}

/// Owns the activation state of the prompts seen during this session.
///
/// `pending` holds the prompts still being retried, `settled` the ones that are done with: either
/// activated, or given up on. A settled prompt is never activated again, so the user can switch away
/// from a prompt (or dismiss one that keeps failing) without it coming back.
struct Surfacer {
    receiver: Receiver<PromptMsg>,
    pending: HashMap<isize, PendingAttempt>,
    settled: HashSet<isize>,
}

impl Surfacer {
    fn new(receiver: Receiver<PromptMsg>) -> Self {
        Self {
            receiver,
            pending: HashMap::new(),
            settled: HashSet::new(),
        }
    }

    fn run(mut self) {
        loop {
            match self.receiver.recv_timeout(self.time_until_next_attempt()) {
                Ok(msg) => self.handle(msg),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            self.run_due_attempts();
        }
    }

    fn handle(&mut self, msg: PromptMsg) {
        match msg {
            PromptMsg::Seen(hwnd) => self.schedule(hwnd),
            PromptMsg::Gone(hwnd) => {
                self.pending.remove(&hwnd);
                self.settled.remove(&hwnd);
            }
        }
    }

    fn schedule(&mut self, hwnd: isize) {
        if self.settled.contains(&hwnd) || self.pending.contains_key(&hwnd) {
            return;
        }
        let window = Window::from(hwnd);
        if !is_parked_prompt(&window) {
            return;
        }
        log::info!("Surfacing parked UAC prompt: {window}");
        self.pending.insert(
            hwnd,
            PendingAttempt {
                attempts: 0,
                due: Instant::now(),
            },
        );
    }

    fn run_due_attempts(&mut self) {
        let now = Instant::now();
        let due: Vec<isize> = self
            .pending
            .iter()
            .filter(|(_, pending)| pending.due <= now)
            .map(|(hwnd, _)| *hwnd)
            .collect();
        for hwnd in due {
            self.attempt(hwnd);
        }
    }

    fn attempt(&mut self, hwnd: isize) {
        let Some(attempts) = self.pending.get_mut(&hwnd).map(|pending| {
            pending.attempts += 1;
            pending.attempts
        }) else {
            return;
        };

        let window = Window::from(hwnd);
        if surface(&window).is_ok() {
            self.pending.remove(&hwnd);
            self.settled.insert(hwnd);
            return;
        }

        if attempts >= MAX_ATTEMPTS {
            log::warn!("Giving up on the parked UAC prompt {window} after {attempts} attempts");
            self.pending.remove(&hwnd);
            self.settled.insert(hwnd);
            return;
        }
        if let Some(pending) = self.pending.get_mut(&hwnd) {
            pending.due = Instant::now() + RETRY_DELAY;
        }
    }

    /// How long the thread can sleep before the earliest pending attempt is due.
    fn time_until_next_attempt(&self) -> Duration {
        self.pending
            .values()
            .map(|pending| pending.due.saturating_duration_since(Instant::now()))
            .min()
            .unwrap_or(IDLE_WAIT)
    }
}

enum PromptMsg {
    Seen(isize),
    Gone(isize),
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
    prompt_image_matches(Path::new(&path), &system32_dir())
}

fn system32_dir() -> PathBuf {
    let system_root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    Path::new(&system_root).join("System32")
}

fn prompt_image_matches(path: &Path, system32: &Path) -> bool {
    let Some(file_name) = path.file_name().map(|name| name.to_string_lossy()) else {
        return false;
    };
    if !PROMPT_HOSTS
        .iter()
        .any(|host| host.eq_ignore_ascii_case(&file_name))
    {
        return false;
    }
    // Windows paths are case insensitive, and the user profile can contain a `consent.exe` too.
    path.parent()
        .is_some_and(|dir| dir.as_os_str().eq_ignore_ascii_case(system32.as_os_str()))
}

/// Makes the prompt the foreground window, which is what lets the user answer it.
fn surface(window: &Window) -> Result<()> {
    if window.is_minimized() {
        // Only a hint: the service can't touch a SYSTEM window (see the module docs), so this is
        // allowed to fail and the activation ladder below is what actually surfaces the prompt.
        ServicePipe::request(SvcAction::ShowWindowAsync {
            hwnd: window.address(),
            command: SW_RESTORE.0,
        })
        .log_error();
    }

    if activate(window) {
        log::info!("Parked UAC prompt surfaced: {window}");
        return Ok(());
    }
    Err(format!("Could not activate the parked UAC prompt {window}").into())
}

/// Activation ladder, most reliable first. Every step is verified with the real foreground window,
/// never by the call's own return value (see `slu_utils::windows::set_foreground`'s docs for why).
fn activate(window: &Window) -> bool {
    let hwnd = window.address();

    // 1. What the native taskbar button calls. It needs no foreground permission and crosses
    //    integrity levels, which `SetForegroundWindow` does not.
    unsafe { SwitchToThisWindow(HWND(hwnd as _), true) };
    if wait_until_activated(window, STEP_TIMEOUT) {
        return true;
    }

    // 2. The elevated service, for when Seelen itself is not allowed to activate the prompt.
    ServicePipe::request_sync(SvcAction::SetForeground(hwnd)).log_error();
    wait_until_activated(window, STEP_TIMEOUT)
}

/// Polls until the window is focused, is gone (the prompt was answered or dismissed) or the timeout
/// expires. A destroyed window counts as activated: there is nothing left to surface.
fn wait_until_activated(window: &Window, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if !window.is_window() || window.is_focused() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SYSTEM32: &str = r"C:\Windows\System32";

    #[test]
    fn matches_a_prompt_host_installed_in_system32() {
        assert!(prompt_image_matches(
            Path::new(&format!(r"{SYSTEM32}\consent.exe")),
            Path::new(SYSTEM32)
        ));
    }

    #[test]
    fn matches_regardless_of_case() {
        assert!(prompt_image_matches(
            Path::new(r"c:\windows\system32\CONSENT.EXE"),
            Path::new(SYSTEM32)
        ));
    }

    #[test]
    fn ignores_other_executables_in_system32() {
        assert!(!prompt_image_matches(
            Path::new(&format!(r"{SYSTEM32}\not-consent.exe")),
            Path::new(SYSTEM32)
        ));
    }

    /// The same file name outside System32 is not a prompt host, `%SystemRoot%` included.
    #[test]
    fn ignores_the_same_file_name_outside_system32() {
        assert!(!prompt_image_matches(
            Path::new(r"C:\Users\user\consent.exe"),
            Path::new(SYSTEM32)
        ));
        for dir in [
            r"C:\Windows",
            r"C:\Windows\SysWOW64",
            r"C:\Windows\System32\drivers",
        ] {
            assert!(
                !prompt_image_matches(
                    Path::new(&format!(r"{dir}\consent.exe")),
                    Path::new(SYSTEM32)
                ),
                "{dir} must not match"
            );
        }
    }
}
