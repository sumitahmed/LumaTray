use std::sync::Arc;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::hal::descriptor::DisplayDescriptor;
use super::config::ConfigManager;
use super::sync_engine::{DisplaySyncEngine, PiecewiseCurve};
use super::worker::HardwareWorkerHandle;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplayState {
    pub descriptor: DisplayDescriptor,
    pub user_friendly_name: Option<String>,
    pub current_brightness: u32,
    pub current_contrast: Option<u32>,
    pub is_linked: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppState {
    pub displays: Vec<DisplayState>,
    pub linked_mode: bool,
    pub master_brightness: u32,
    pub sync_engine: DisplaySyncEngine,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            displays: Vec::new(),
            linked_mode: false,
            master_brightness: 50,
            sync_engine: DisplaySyncEngine::new(),
        }
    }
}

pub struct StateStore {
    state: Arc<RwLock<AppState>>,
    worker: HardwareWorkerHandle,
    config_manager: ConfigManager,
}

#[allow(dead_code)]
impl StateStore {
    pub fn new(worker: HardwareWorkerHandle) -> Self {
        let config_mgr = ConfigManager::new();
        let saved_cfg = config_mgr.load();

        let mut app_state = AppState::default();
        app_state.linked_mode = saved_cfg.sync_enabled;
        app_state.master_brightness = saved_cfg.master_brightness;

        Self {
            state: Arc::new(RwLock::new(app_state)),
            worker,
            config_manager: config_mgr,
        }
    }

    pub fn snapshot(&self) -> AppState {
        self.state.read().clone()
    }

    /// Updates the display inventory from a fresh enumeration.
    pub fn update_inventory(&self, descriptors: Vec<DisplayDescriptor>) {
        let saved_cfg = self.config_manager.load();
        let mut state = self.state.write();

        // Preserve previous user settings / curves where matching
        let mut new_displays = Vec::new();

        for desc in descriptors {
            let existing = state.displays.iter().find(|d| d.descriptor.id == desc.id);
            let saved_disp = saved_cfg.displays.get(&desc.id);

            let user_name = existing
                .and_then(|d| d.user_friendly_name.clone())
                .or_else(|| saved_disp.and_then(|d| d.friendly_name.clone()));

            let current_bright = desc.capabilities.current_brightness;
            let current_contrast = desc.capabilities.current_contrast;

            let is_linked = existing
                .map(|d| d.is_linked)
                .or_else(|| saved_disp.map(|d| d.is_linked))
                .unwrap_or(true);

            if let Some(disp_cfg) = saved_disp {
                if let Some(ref curve) = disp_cfg.custom_curve {
                    state.sync_engine.set_curve(desc.id.clone(), curve.clone());
                }
            }

            new_displays.push(DisplayState {
                descriptor: desc,
                user_friendly_name: user_name,
                current_brightness: current_bright,
                current_contrast,
                is_linked,
            });
        }

        state.displays = new_displays;
        info!("StateStore inventory updated: {} displays", state.displays.len());
    }

    /// Sets the master brightness slider and synchronizes all linked displays.
    pub fn set_master_brightness(&self, level: u32) {
        let clamped = level.clamp(0, 100);
        let mut state = self.state.write();
        state.master_brightness = clamped;

        if !state.linked_mode {
            return;
        }

        let ids: Vec<String> = state
            .displays
            .iter()
            .filter(|d| d.is_linked)
            .map(|d| d.descriptor.id.clone())
            .collect();

        let synced_levels = state.sync_engine.calculate_synced_levels(&ids, clamped);

        for display in &mut state.displays {
            if let Some(&new_level) = synced_levels.get(&display.descriptor.id) {
                display.current_brightness = new_level;
                // Dispatch asynchronous throttled hardware write
                self.worker.set_brightness(display.descriptor.id.clone(), new_level);
            }
        }
    }

    /// Adjusts an individual display's brightness slider.
    pub fn set_display_brightness(&self, id: &str, level: u32) {
        let clamped = level.clamp(0, 100);
        let mut state = self.state.write();

        let is_linked_mode = state.linked_mode;
        let is_display_linked = state
            .displays
            .iter()
            .find(|d| d.descriptor.id == id)
            .map(|d| d.is_linked)
            .unwrap_or(false);

        if is_linked_mode && is_display_linked {
            // Compute the visual master level from this display's curve
            let curve = state.sync_engine.get_curve(id);
            let visual_master = curve.map_reverse(clamped as f64).round() as u32;
            state.master_brightness = visual_master;

            let ids: Vec<String> = state
                .displays
                .iter()
                .filter(|d| d.is_linked)
                .map(|d| d.descriptor.id.clone())
                .collect();

            let synced_levels = state.sync_engine.calculate_synced_levels(&ids, visual_master);

            for display in &mut state.displays {
                if let Some(&new_level) = synced_levels.get(&display.descriptor.id) {
                    display.current_brightness = new_level;
                    self.worker.set_brightness(display.descriptor.id.clone(), new_level);
                }
            }
        } else {
            // Unlinked / independent display adjustment
            if let Some(display) = state.displays.iter_mut().find(|d| d.descriptor.id == id) {
                display.current_brightness = clamped;
                self.worker.set_brightness(id.to_string(), clamped);
            }
        }
    }

    /// Adjusts an individual display's contrast slider.
    pub fn set_display_contrast(&self, id: &str, level: u32) {
        let clamped = level.clamp(0, 100);
        let mut state = self.state.write();

        if let Some(display) = state.displays.iter_mut().find(|d| d.descriptor.id == id) {
            display.current_contrast = Some(clamped);
            self.worker.set_contrast(id.to_string(), clamped);
        }
    }

    /// Toggles the master linked mode.
    pub fn set_linked_mode(&self, enabled: bool) {
        let mut state = self.state.write();
        state.linked_mode = enabled;

        if enabled {
            // Re-sync all linked displays to current master brightness
            let master_level = state.master_brightness;
            let ids: Vec<String> = state
                .displays
                .iter()
                .filter(|d| d.is_linked)
                .map(|d| d.descriptor.id.clone())
                .collect();

            let synced_levels = state.sync_engine.calculate_synced_levels(&ids, master_level);

            for display in &mut state.displays {
                if let Some(&new_level) = synced_levels.get(&display.descriptor.id) {
                    display.current_brightness = new_level;
                    self.worker.set_brightness(display.descriptor.id.clone(), new_level);
                }
            }
        }
    }

    pub fn set_display_link_state(&self, id: &str, is_linked: bool) {
        let mut state = self.state.write();
        if let Some(display) = state.displays.iter_mut().find(|d| d.descriptor.id == id) {
            display.is_linked = is_linked;
        }
    }

    pub fn set_calibration_curve(&self, id: String, curve: PiecewiseCurve) {
        let mut state = self.state.write();
        state.sync_engine.set_curve(id, curve);
    }
}
