use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use parking_lot::Mutex;
use tracing::info;

pub const WAKE_GRACE_PERIOD: Duration = Duration::from_secs(15);

#[derive(Clone)]
pub struct PowerSupervisor {
    is_sleeping: Arc<AtomicBool>,
    recently_woke_up: Arc<AtomicBool>,
    wake_instant: Arc<Mutex<Option<Instant>>>,
}

impl Default for PowerSupervisor {
    fn default() -> Self {
        Self {
            is_sleeping: Arc::new(AtomicBool::new(false)),
            recently_woke_up: Arc::new(AtomicBool::new(false)),
            wake_instant: Arc::new(Mutex::new(None)),
        }
    }
}

impl PowerSupervisor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Invoked when Windows notifies PBT_APMSUSPEND (system going to sleep).
    pub fn on_system_suspend(&self) {
        info!("System going to sleep (PBT_APMSUSPEND). Pausing DDC/CI queries.");
        self.is_sleeping.store(true, Ordering::SeqCst);
        self.recently_woke_up.store(false, Ordering::SeqCst);
        *self.wake_instant.lock() = None;
    }

    /// Invoked when Windows notifies PBT_APMRESUMEAUTOMATIC or PBT_APMRESUMESUSPEND.
    pub fn on_system_resume(&self) {
        info!("System resumed from sleep. Starting 15s DDC/CI wake grace window.");
        self.is_sleeping.store(false, Ordering::SeqCst);
        self.recently_woke_up.store(true, Ordering::SeqCst);
        *self.wake_instant.lock() = Some(Instant::now());
    }

    /// Checks if we are currently in the 15-second grace window after waking up.
    pub fn is_in_wake_grace_period(&self) -> bool {
        if !self.recently_woke_up.load(Ordering::SeqCst) {
            return false;
        }

        let guard = self.wake_instant.lock();
        if let Some(wake_time) = *guard {
            if wake_time.elapsed() < WAKE_GRACE_PERIOD {
                return true;
            }
        }

        self.recently_woke_up.store(false, Ordering::SeqCst);
        false
    }

    pub fn is_suspended(&self) -> bool {
        self.is_sleeping.load(Ordering::SeqCst)
    }
}
