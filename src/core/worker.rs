use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;
use crossbeam_channel::{unbounded, Receiver, Sender};
use tracing::{debug, error, info};

use crate::hal::descriptor::DisplayDescriptor;
use crate::hal::error::DriverError;
use crate::hal::DisplayDriver;
use super::coalescer::{CoalesceAction, FeatureType, ThrottleKey, WriteCoalescer};

#[derive(Debug)]
#[allow(dead_code)]
pub enum HardwareCommand {
    Enumerate {
        reply: Sender<Result<Vec<DisplayDescriptor>, DriverError>>,
    },
    SetBrightness {
        id: String,
        level: u32,
        immediate: bool,
    },
    SetContrast {
        id: String,
        level: u32,
        immediate: bool,
    },
    SetVcp {
        id: String,
        vcp_code: u8,
        value: u32,
    },
    ReadBrightness {
        id: String,
        reply: Sender<Result<u32, DriverError>>,
    },
    SetThrottleInterval(Duration),
    FlushPending,
    Shutdown,
}

pub struct HardwareWorker {
    driver: Box<dyn DisplayDriver>,
    coalescer: WriteCoalescer,
    cmd_rx: Receiver<HardwareCommand>,
}

impl HardwareWorker {
    fn new(
        driver: Box<dyn DisplayDriver>,
        cmd_rx: Receiver<HardwareCommand>,
        throttle_interval: Duration,
    ) -> Self {
        Self {
            driver,
            coalescer: WriteCoalescer::new(throttle_interval),
            cmd_rx,
        }
    }

    fn run(&mut self) {
        info!("HardwareWorker actor loop started.");

        loop {
            // Determine if there are pending deferred writes to flush
            let pending = self.coalescer.pending_keys();
            let timeout = if pending.is_empty() {
                Duration::from_millis(500)
            } else {
                Duration::from_millis(20) // Fast check to flush deferred writes
            };

            match self.cmd_rx.recv_timeout(timeout) {
                Ok(HardwareCommand::Enumerate { reply }) => {
                    self.flush_all_pending();
                    let res = self.driver.enumerate();
                    let _ = reply.send(res);
                }
                Ok(HardwareCommand::SetBrightness { id, level, immediate }) => {
                    let key = ThrottleKey {
                        display_id: id.clone(),
                        feature: FeatureType::Brightness,
                    };

                    if immediate {
                        if let Err(e) = self.driver.set_brightness(&id, level) {
                            error!("Immediate SetBrightness error for '{}': {:?}", id, e);
                        }
                    } else {
                        match self.coalescer.submit(key.clone(), level) {
                            CoalesceAction::WriteNow(val) => {
                                if let Err(e) = self.driver.set_brightness(&id, val) {
                                    error!("WriteNow SetBrightness error for '{}': {:?}", id, e);
                                }
                            }
                            CoalesceAction::Defer { delay, latest_value } => {
                                debug!(
                                    "Deferred SetBrightness for '{}' to {} (delay: {:?})",
                                    id, latest_value, delay
                                );
                            }
                        }
                    }
                }
                Ok(HardwareCommand::SetContrast { id, level, immediate }) => {
                    let key = ThrottleKey {
                        display_id: id.clone(),
                        feature: FeatureType::Contrast,
                    };

                    if immediate {
                        if let Err(e) = self.driver.set_contrast(&id, level) {
                            error!("Immediate SetContrast error for '{}': {:?}", id, e);
                        }
                    } else {
                        match self.coalescer.submit(key, level) {
                            CoalesceAction::WriteNow(val) => {
                                if let Err(e) = self.driver.set_contrast(&id, val) {
                                    error!("WriteNow SetContrast error for '{}': {:?}", id, e);
                                }
                            }
                            CoalesceAction::Defer { delay, latest_value } => {
                                debug!(
                                    "Deferred SetContrast for '{}' to {} (delay: {:?})",
                                    id, latest_value, delay
                                );
                            }
                        }
                    }
                }
                Ok(HardwareCommand::SetVcp { id, vcp_code, value }) => {
                    if let Err(e) = self.driver.set_vcp(&id, vcp_code, value) {
                        error!("SetVcp error for '{}' [{:#04X}]: {:?}", id, vcp_code, e);
                    }
                }
                Ok(HardwareCommand::ReadBrightness { id, reply }) => {
                    self.flush_pending_for_id(&id);
                    let res = self.driver.get_brightness(&id);
                    let _ = reply.send(res);
                }
                Ok(HardwareCommand::SetThrottleInterval(interval)) => {
                    info!("Updated HardwareWorker throttle interval to {:?}", interval);
                    self.coalescer.set_interval(interval);
                }
                Ok(HardwareCommand::FlushPending) => {
                    self.flush_all_pending();
                }
                Ok(HardwareCommand::Shutdown) => {
                    self.flush_all_pending();
                    info!("HardwareWorker received Shutdown command. Exiting.");
                    break;
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    // Flush any pending deferred writes whose time window has passed
                    self.flush_all_pending();
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    info!("HardwareWorker channel disconnected. Shutting down.");
                    break;
                }
            }
        }
    }

    fn flush_all_pending(&mut self) {
        let keys = self.coalescer.pending_keys();
        for key in keys {
            if let Some(val) = self.coalescer.pop_pending(&key) {
                match key.feature {
                    FeatureType::Brightness => {
                        debug!("Flushing deferred brightness write to '{}': {}", key.display_id, val);
                        if let Err(e) = self.driver.set_brightness(&key.display_id, val) {
                            error!("Flushed SetBrightness error for '{}': {:?}", key.display_id, e);
                        }
                    }
                    FeatureType::Contrast => {
                        debug!("Flushing deferred contrast write to '{}': {}", key.display_id, val);
                        if let Err(e) = self.driver.set_contrast(&key.display_id, val) {
                            error!("Flushed SetContrast error for '{}': {:?}", key.display_id, e);
                        }
                    }
                    FeatureType::Vcp(code) => {
                        let _ = self.driver.set_vcp(&key.display_id, code, val);
                    }
                }
            }
        }
    }

    fn flush_pending_for_id(&mut self, id: &str) {
        let key_bright = ThrottleKey {
            display_id: id.to_string(),
            feature: FeatureType::Brightness,
        };
        if let Some(val) = self.coalescer.pop_pending(&key_bright) {
            let _ = self.driver.set_brightness(id, val);
        }

        let key_contrast = ThrottleKey {
            display_id: id.to_string(),
            feature: FeatureType::Contrast,
        };
        if let Some(val) = self.coalescer.pop_pending(&key_contrast) {
            let _ = self.driver.set_contrast(id, val);
        }
    }
}

/// Thread-safe client handle to dispatch asynchronous commands to the Hardware Worker.
#[derive(Clone)]
pub struct HardwareWorkerHandle {
    sender: Sender<HardwareCommand>,
    _thread: Arc<MutexOptionThread>,
}

struct MutexOptionThread {
    handle: parking_lot::Mutex<Option<JoinHandle<()>>>,
}

#[allow(dead_code)]
impl HardwareWorkerHandle {
    pub fn spawn(driver: Box<dyn DisplayDriver>, throttle_interval: Duration) -> Self {
        let (tx, rx) = unbounded();

        let thread_handle = thread::Builder::new()
            .name("luma-hardware-worker".into())
            .spawn(move || {
                let mut worker = HardwareWorker::new(driver, rx, throttle_interval);
                worker.run();
            })
            .expect("Failed to spawn luma-hardware-worker thread");

        Self {
            sender: tx,
            _thread: Arc::new(MutexOptionThread {
                handle: parking_lot::Mutex::new(Some(thread_handle)),
            }),
        }
    }

    /// Synchronously requests display enumeration on the worker thread.
    pub fn enumerate(&self) -> Result<Vec<DisplayDescriptor>, DriverError> {
        let (reply_tx, reply_rx) = unbounded();
        self.sender
            .send(HardwareCommand::Enumerate { reply: reply_tx })
            .map_err(|e| DriverError::Internal(format!("Worker disconnected: {:?}", e)))?;

        reply_rx
            .recv()
            .map_err(|e| DriverError::Internal(format!("Worker reply failed: {:?}", e)))?
    }

    /// Asynchronously sets brightness. Non-blocking (returns in microseconds).
    pub fn set_brightness(&self, id: String, level: u32) {
        let _ = self.sender.send(HardwareCommand::SetBrightness {
            id,
            level,
            immediate: false,
        });
    }

    /// Immediately sets brightness, bypassing the coalescer.
    pub fn set_brightness_immediate(&self, id: String, level: u32) {
        let _ = self.sender.send(HardwareCommand::SetBrightness {
            id,
            level,
            immediate: true,
        });
    }

    /// Asynchronously sets contrast. Non-blocking.
    pub fn set_contrast(&self, id: String, level: u32) {
        let _ = self.sender.send(HardwareCommand::SetContrast {
            id,
            level,
            immediate: false,
        });
    }

    /// Reads brightness (synchronous round-trip to the worker).
    pub fn read_brightness(&self, id: String) -> Result<u32, DriverError> {
        let (reply_tx, reply_rx) = unbounded();
        self.sender
            .send(HardwareCommand::ReadBrightness {
                id,
                reply: reply_tx,
            })
            .map_err(|e| DriverError::Internal(format!("Worker disconnected: {:?}", e)))?;

        reply_rx
            .recv()
            .map_err(|e| DriverError::Internal(format!("Worker reply failed: {:?}", e)))?
    }

    /// Updates the write coalescer throttle interval dynamically.
    pub fn set_throttle_interval(&self, interval: Duration) {
        let _ = self.sender.send(HardwareCommand::SetThrottleInterval(interval));
    }

    /// Flushes all pending writes immediately.
    pub fn flush(&self) {
        let _ = self.sender.send(HardwareCommand::FlushPending);
    }

    /// Gracefully stops the worker thread.
    pub fn shutdown(&self) {
        let _ = self.sender.send(HardwareCommand::Shutdown);
        if let Some(handle) = self._thread.handle.lock().take() {
            let _ = handle.join();
        }
    }
}
