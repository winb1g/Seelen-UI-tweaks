//! [fork] Keeps the charging state (and the rest of the power/battery data) in sync.
//!
//! Upstream only refreshes the power status when the hidden background window gets
//! `WM_POWERBROADCAST`/`PBT_APMPOWERSTATUSCHANGE`, reads the batteries once at that moment, and otherwise only
//! when the battery percentage changes. That misses the charger being plugged in on some laptops:
//!
//! - if the battery read fails while the power source is switching, upstream bails out before sending the new
//!   AC status, so the toolbar keeps showing "on battery" (no charging bolt);
//! - the battery only reports `charging` a moment after the charger is connected, and with a charge limit or a
//!   full battery the percentage never changes again, so that state is never picked up.
//!
//! This service re-reads the power status and the batteries, and feeds any difference into upstream's
//! `PowerManager` (which updates its cache and notifies the widgets as usual):
//!
//! - right after the power source changes (`GUID_ACDC_POWER_SOURCE` notification, which doesn't depend on window
//!   broadcasts, plus the background window's power broadcasts), and again a few seconds later while the
//!   battery settles;
//! - every [`POLL_INTERVAL`] as a safety net, while the session is interactive.

use std::{
    ffi::c_void,
    sync::{
        OnceLock,
        atomic::Ordering,
        mpsc::{Receiver, RecvTimeoutError, Sender, channel},
    },
    time::{Duration, Instant},
};

use seelen_core::system_state::{Battery, PowerStatus};
use windows::Win32::{
    Foundation::HANDLE,
    System::{
        Power::{DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS, PowerSettingRegisterNotification},
        SystemServices::GUID_ACDC_POWER_SOURCE,
    },
    UI::WindowsAndMessaging::{
        DEVICE_NOTIFY_CALLBACK, PBT_APMPOWERSTATUSCHANGE, PBT_APMRESUMEAUTOMATIC,
        PBT_APMRESUMESUSPEND, WM_POWERBROADCAST,
    },
};

use crate::{
    error::{Result, ResultLogExt},
    modules::power::application::{PowerManager, PowerManagerEvent},
    utils::spawn_named_thread,
    windows_api::event_window::{IS_INTERACTIVE_SESSION, subscribe_to_background_window},
};

/// Safety-net refresh interval, in case a power notification is missed.
const POLL_INTERVAL: Duration = Duration::from_secs(30);
/// Extra refreshes after a power change: the battery takes a moment to report it is (dis)charging.
const SETTLE_DELAYS: [Duration; 3] = [
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(15),
];

/// Wakes the sync thread when the power source changes.
static KICK: OnceLock<Sender<()>> = OnceLock::new();

/// State of the sync thread.
#[derive(Default)]
struct PowerSync {
    /// The last read came back with no batteries while upstream still had some.
    batteries_missing: bool,
}

impl PowerSync {
    fn run(mut self, kicks: Receiver<()>) {
        let mut settle_at: Vec<Instant> = Vec::new();
        loop {
            let wait = settle_at
                .iter()
                .min()
                .map(|at| at.saturating_duration_since(Instant::now()))
                .unwrap_or(POLL_INTERVAL);

            match kicks.recv_timeout(wait) {
                Ok(()) => {
                    // coalesce bursts (both notification sources fire for the same change)
                    while kicks.try_recv().is_ok() {}
                    let now = Instant::now();
                    settle_at = SETTLE_DELAYS.iter().map(|delay| now + *delay).collect();
                }
                Err(RecvTimeoutError::Timeout) => {
                    let now = Instant::now();
                    let settling = !settle_at.is_empty();
                    settle_at.retain(|at| *at > now);
                    if !settling && !IS_INTERACTIVE_SESSION.load(Ordering::Acquire) {
                        continue;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }

            self.reconcile();
        }
    }

    /// Reads the current power state and sends whatever differs from upstream's cache.
    fn reconcile(&mut self) {
        // copy what we need and release the lock before touching the OS
        let (cached_status, cached_batteries) = {
            let manager = PowerManager::instance().lock();
            (
                status_key(&manager.power_status),
                batteries_key(&manager.batteries),
            )
        };

        match PowerManager::get_power_status() {
            Ok(status) => {
                if status_key(&status) != cached_status {
                    log::debug!("[fork] power status out of sync, updating: {status:?}");
                    PowerManager::send(PowerManagerEvent::PowerStatusChanged(status));
                }
            }
            Err(err) => log::debug!("[fork] failed to read the power status: {err}"),
        }

        match PowerManager::get_batteries() {
            Ok(batteries) => {
                // reads can come back empty for a moment while the power source switches:
                // only trust it once it is confirmed by the next read
                if batteries.is_empty() && !cached_batteries.is_empty() && !self.batteries_missing {
                    self.batteries_missing = true;
                    return;
                }
                self.batteries_missing = false;

                if batteries_key(&batteries) != cached_batteries {
                    log::debug!("[fork] batteries out of sync, updating");
                    PowerManager::send(PowerManagerEvent::BatteriesChanged(batteries));
                }
            }
            Err(err) => log::debug!("[fork] failed to read the batteries: {err}"),
        }
    }
}

pub fn start() {
    let (sender, receiver) = channel::<()>();
    if KICK.set(sender).is_err() {
        return;
    }

    spawn_named_thread("Fork Power Sync", move || {
        PowerSync::default().run(receiver)
    });

    subscribe_to_background_window(|msg, w_param, _| {
        if msg == WM_POWERBROADCAST
            && matches!(
                w_param as u32,
                PBT_APMPOWERSTATUSCHANGE | PBT_APMRESUMESUSPEND | PBT_APMRESUMEAUTOMATIC
            )
        {
            kick();
        }
        Ok(())
    });

    register_power_source_notification().log_error();
}

/// Registers for AC/DC power source changes through a callback, so it doesn't depend on window broadcasts.
/// Windows invokes the callback once right after registering, which also runs the first sync.
fn register_power_source_notification() -> Result<()> {
    // must outlive the registration, which lasts for the whole process
    let params = Box::leak(Box::new(DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS {
        Callback: Some(on_power_source_change),
        Context: std::ptr::null_mut(),
    }));

    let mut registration = std::ptr::null_mut();
    unsafe {
        PowerSettingRegisterNotification(
            &GUID_ACDC_POWER_SOURCE,
            DEVICE_NOTIFY_CALLBACK,
            HANDLE(params as *mut DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS as *mut c_void),
            &mut registration,
        )
        .ok()?;
    }
    Ok(())
}

unsafe extern "system" fn on_power_source_change(
    _context: *const c_void,
    _type: u32,
    _setting: *const c_void,
) -> u32 {
    kick();
    0
}

fn kick() {
    if let Some(sender) = KICK.get() {
        let _ = sender.send(());
    }
}

/// The parts of the power status that the widgets render.
fn status_key(status: &PowerStatus) -> (u8, u8, u8) {
    (
        status.ac_line_status,
        status.battery_flag,
        status.battery_life_percent,
    )
}

/// The parts of each battery that the widgets render (state drives the charging bolt).
fn batteries_key(batteries: &[Battery]) -> Vec<(String, i32, bool)> {
    batteries
        .iter()
        .map(|b| (b.state.clone(), b.percentage.round() as i32, b.smart_charging))
        .collect()
}
