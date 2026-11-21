use std::collections::HashMap;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[allow(dead_code)]
pub enum FeatureType {
    Brightness,
    Contrast,
    Vcp(u8),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ThrottleKey {
    pub display_id: String,
    pub feature: FeatureType,
}

#[derive(Debug, PartialEq, Eq)]
pub enum CoalesceAction {
    /// Write immediately to the hardware.
    WriteNow(u32),
    /// Defer writing. The caller should wait for `delay` before writing the pending value.
    Defer { delay: Duration, latest_value: u32 },
}

struct ThrottleSlot {
    last_write: Instant,
    pending: Option<u32>,
}

pub struct WriteCoalescer {
    interval: Duration,
    slots: HashMap<ThrottleKey, ThrottleSlot>,
}

impl WriteCoalescer {
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            slots: HashMap::new(),
        }
    }

    pub fn set_interval(&mut self, interval: Duration) {
        self.interval = interval;
    }

    /// Submits a new value. Returns whether to execute immediately or defer.
    pub fn submit(&mut self, key: ThrottleKey, value: u32) -> CoalesceAction {
        let now = Instant::now();

        let slot = self.slots.entry(key).or_insert_with(|| ThrottleSlot {
            // Initialize in the past so the very first write is immediate
            last_write: now.checked_sub(self.interval * 2).unwrap_or(now),
            pending: None,
        });

        let elapsed = now.duration_since(slot.last_write);

        if elapsed >= self.interval && slot.pending.is_none() {
            slot.last_write = now;
            slot.pending = None;
            CoalesceAction::WriteNow(value)
        } else {
            slot.pending = Some(value);
            let delay = if elapsed < self.interval {
                self.interval - elapsed
            } else {
                Duration::from_millis(5)
            };
            CoalesceAction::Defer {
                delay,
                latest_value: value,
            }
        }
    }

    /// Checks if a slot has a pending write and returns it, marking the write as executed now.
    pub fn pop_pending(&mut self, key: &ThrottleKey) -> Option<u32> {
        let now = Instant::now();
        if let Some(slot) = self.slots.get_mut(key) {
            if let Some(val) = slot.pending.take() {
                slot.last_write = now;
                return Some(val);
            }
        }
        None
    }

    /// Returns all keys that currently have pending writes waiting to be flushed.
    pub fn pending_keys(&self) -> Vec<ThrottleKey> {
        self.slots
            .iter()
            .filter_map(|(k, v)| if v.pending.is_some() { Some(k.clone()) } else { None })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_coalescer_immediate_first_write() {
        let mut coalescer = WriteCoalescer::new(Duration::from_millis(50));
        let key = ThrottleKey {
            display_id: "monitor_1".into(),
            feature: FeatureType::Brightness,
        };

        // First write must be immediate
        let action = coalescer.submit(key.clone(), 70);
        assert_eq!(action, CoalesceAction::WriteNow(70));
    }

    #[test]
    fn test_coalescer_throttles_rapid_writes() {
        let mut coalescer = WriteCoalescer::new(Duration::from_millis(50));
        let key = ThrottleKey {
            display_id: "monitor_1".into(),
            feature: FeatureType::Brightness,
        };

        // 1st write: immediate
        let act1 = coalescer.submit(key.clone(), 10);
        assert_eq!(act1, CoalesceAction::WriteNow(10));

        // 2nd write 5ms later: must be deferred
        let act2 = coalescer.submit(key.clone(), 20);
        match act2 {
            CoalesceAction::Defer { latest_value, .. } => assert_eq!(latest_value, 20),
            _ => panic!("Expected deferred write"),
        }

        // 3rd write 5ms later: overwrites pending with latest value
        let act3 = coalescer.submit(key.clone(), 30);
        match act3 {
            CoalesceAction::Defer { latest_value, .. } => assert_eq!(latest_value, 30),
            _ => panic!("Expected deferred write"),
        }

        // Pop pending should return the latest value (30)
        let popped = coalescer.pop_pending(&key);
        assert_eq!(popped, Some(30));

        // No more pending
        assert_eq!(coalescer.pop_pending(&key), None);
    }
}
