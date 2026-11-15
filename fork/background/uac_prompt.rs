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
//! - `SwitchToThisWindow` is documented as "not intended for general use" but it does switch focus and
//!   bring the window to the foreground, and in practice it is the only one of the three that succeeds
//!   here. It is tried first for that reason, not because its rules are known to differ: treat that as an
//!   observation, not a guarantee.
//!
//! Every step is judged by re-reading the real state of the window, never by a call's return value. Two
//! conditions must both hold for the prompt to count as surfaced: it must be the foreground window AND not
//! minimized. Windows lets a minimized window hold the foreground while still showing nothing, so checking
//! only the foreground would report success for a prompt the user still can't see.
//!
//! A prompt that can't be surfaced is retried a bounded number of times, then left alone until its window
//! is destroyed or it asks for attention again (see [`spawn_resurfacing_on_flash`]). The native path was
//! already unavailable to the user, so backing off is better than fighting them.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{OnceLock, atomic::Ordering},
    time::{Duration, Instant},
};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, unbounded};

use slu_ipc::messages::SvcAction;
use windows::Win32::{
    Foundation::HWND,
    UI::WindowsAndMessaging::{HSHELL_HIGHBIT, HSHELL_REDRAW, SW_RESTORE, SwitchToThisWindow},
};

use crate::{
    cli::ServicePipe,
    error::{Result, ResultLogExt},
    platform::{
        hook::{HookManager, WindowEvent},
        window::Window,
    },
    utils::spawn_named_thread,
    windows_api::{
        WindowsApi,
        event_window::{WM_SHELLHOOKMESSAGE, subscribe_to_background_window},
    },
};

/// `HSHELL_FLASH`: a taskbar button turned highlighted (a window is asking for attention). Not exported by
/// the `windows` crate, same definition as the apps module uses.
const HSHELL_FLASH: u32 = HSHELL_REDRAW | HSHELL_HIGHBIT;
/// System32 executables whose top-level windows on the user's desktop are prompts waiting for the user.
/// Executables outside `%SystemRoot%\System32` never match, so this list can not be used to hijack the
/// prompt of another program. To support another prompt host, add its file name here.
const PROMPT_HOSTS: &[&str] = &["consent.exe"];
/// How long each activation step gets to make the prompt the foreground window
/// (the window may not accept it right after being shown).
const STEP_TIMEOUT: Duration = Duration::from_millis(120);
/// Attempts per burst before the prompt is left alone until it asks for attention again.
const MAX_ATTEMPTS: u32 = 3;
/// Hard cap per prompt: a prompt that keeps asking for attention is retried in later bursts, but never
/// more than this, so a prompt that can never be surfaced doesn't turn into a permanent focus fight.
const MAX_TOTAL_ATTEMPTS: u32 = 12;
/// Delay before retrying a prompt that could not be activated.
const RETRY_DELAY: Duration = Duration::from_millis(900);
/// Idle wait of the surfacer thread. Only bounds how long it sleeps, every message wakes it up.
const IDLE_WAIT: Duration = Duration::from_secs(3600);

static QUEUE: OnceLock<Sender<PromptMsg>> = OnceLock::new();

pub fn start() {
    let (sender, receiver) = unbounded::<PromptMsg>();
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

    spawn_resurfacing_on_flash(sender.clone());
}

/// Activation attempts of one prompt, within the current burst of retries.
struct PendingAttempt {
    attempts: u32,
    /// When the next attempt is due, so a prompt that is still loading is retried later instead of
    /// being polled.
    due: Instant,
}

/// Owns the activation state of the prompts seen during this session.
///
/// A prompt is either `pending` (still being retried) or settled (activated, or given up on for now).
/// `attempts` keeps the total per prompt across bursts, so a prompt that keeps asking for attention is
/// retried again, but only up to [`MAX_TOTAL_ATTEMPTS`]: a prompt that never becomes visible (a broken
/// one, or one the user keeps ignoring) must not turn into a permanent focus fight.
struct Surfacer {
    receiver: Receiver<PromptMsg>,
    pending: HashMap<isize, PendingAttempt>,
    /// Total attempts made per prompt that is not pending anymore.
    attempts: HashMap<isize, u32>,
}

impl Surfacer {
    fn new(receiver: Receiver<PromptMsg>) -> Self {
        Self {
            receiver,
            pending: HashMap::new(),
            attempts: HashMap::new(),
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
                self.attempts.remove(&hwnd);
            }
        }
    }

    fn schedule(&mut self, hwnd: isize) {
        if self.pending.contains_key(&hwnd) {
            return;
        }
        if self
            .attempts
            .get(&hwnd)
            .is_some_and(|n| *n >= MAX_TOTAL_ATTEMPTS)
        {
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
        let total = self.attempts.entry(hwnd).or_default();
        *total += 1;
        let total = *total;

        let window = Window::from(hwnd);
        if surface(&window).is_ok() {
            // settled: a surfaced prompt is never activated again on its own, so the user can switch
            // away from it without it coming back
            self.pending.remove(&hwnd);
            self.attempts.remove(&hwnd);
            return;
        }

        if attempts >= MAX_ATTEMPTS || total >= MAX_TOTAL_ATTEMPTS {
            log::warn!("Giving up on the parked UAC prompt {window} after {total} attempts");
            self.pending.remove(&hwnd);
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

/// Re-arms a prompt that was given up on when it asks for attention again.
///
/// Windows parks a prompt as a *flashing* taskbar button, and the flashing stops once the window is
/// activated. So a new flash means the prompt is still waiting and still unreadable, which makes it the
/// natural signal to retry: it can't fire for a prompt the user already answered, and it can't keep
/// firing forever (a flash ends when the window is activated). This keeps a prompt that was given up on
/// from being abandoned for the rest of the session.
fn spawn_resurfacing_on_flash(queue: Sender<PromptMsg>) {
    subscribe_to_background_window(move |msg, w_param, l_param| {
        if msg != WM_SHELLHOOKMESSAGE.load(Ordering::Relaxed) {
            return Ok(());
        }
        // HSHELL_FLASH: a taskbar button turned highlighted, i.e. a window is asking for attention.
        if w_param as u32 == HSHELL_FLASH {
            queue.send(PromptMsg::Seen(l_param))?;
        }
        Ok(())
    });
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

/// Activation ladder, most reliable first. Every step is verified by re-reading the real state of the
/// window, never by the call's own return value (see `slu_utils::windows::set_foreground`'s docs for why).
fn activate(window: &Window) -> bool {
    let hwnd = window.address();

    // 1. What works in practice for this window, see the module docs.
    unsafe { SwitchToThisWindow(HWND(hwnd as _), true) };
    if wait_until_surfaced(window, STEP_TIMEOUT) {
        return true;
    }

    // 2. The elevated service, for when Seelen itself is not allowed to activate the prompt.
    ServicePipe::request_sync(SvcAction::SetForeground(hwnd)).log_error();
    wait_until_surfaced(window, STEP_TIMEOUT)
}

/// Polls until the window is surfaced, is gone (the prompt was answered or dismissed) or the timeout
/// expires. A destroyed window counts as surfaced: there is nothing left to surface.
///
/// A minimized window can still be the foreground one while showing nothing at all, so being focused is
/// not enough on its own; it has to be un-minimized as well (see the module docs).
fn wait_until_surfaced(window: &Window, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if !window.is_window() || (!window.is_minimized() && window.is_focused()) {
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

    /// `schedule` needs a real window, so the state transitions are exercised through the pieces that
    /// don't: the per-burst and total caps, and the re-arm gate they feed.
    #[test]
    fn a_prompt_is_not_scheduled_while_already_pending() {
        let mut surfacer = Surfacer::new(unbounded().1);
        surfacer.pending.insert(
            1,
            PendingAttempt {
                attempts: 0,
                due: Instant::now(),
            },
        );
        surfacer.schedule(1);
        assert_eq!(
            surfacer.pending.len(),
            1,
            "a pending prompt must not be duplicated"
        );
    }

    #[test]
    fn a_prompt_that_reached_the_total_cap_is_not_retried_again() {
        let mut surfacer = Surfacer::new(unbounded().1);
        surfacer.attempts.insert(1, MAX_TOTAL_ATTEMPTS);
        surfacer.schedule(1);
        assert!(
            surfacer.pending.is_empty(),
            "a prompt at the total cap must be left alone"
        );
    }

    /// One attempt below the cap must still be allowed, otherwise the cap is off by one and the last
    /// useful retry never happens.
    #[test]
    fn a_prompt_below_the_total_cap_is_retried_again() {
        let mut surfacer = Surfacer::new(unbounded().1);
        surfacer.attempts.insert(1, MAX_TOTAL_ATTEMPTS - 1);
        // `schedule` still rejects it, but only because `1` is not a real prompt window.
        surfacer.schedule(1);
        assert!(surfacer.pending.is_empty());
        assert_eq!(
            surfacer.attempts.get(&1),
            Some(&(MAX_TOTAL_ATTEMPTS - 1)),
            "scheduling must not consume an attempt"
        );
    }

    #[test]
    fn a_destroyed_prompt_forgets_both_its_state_maps() {
        let mut surfacer = Surfacer::new(unbounded().1);
        surfacer.pending.insert(
            1,
            PendingAttempt {
                attempts: 1,
                due: Instant::now(),
            },
        );
        surfacer.attempts.insert(1, 5);
        surfacer.handle(PromptMsg::Gone(1));
        assert!(surfacer.pending.is_empty());
        assert!(surfacer.attempts.is_empty());
    }

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
