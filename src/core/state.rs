use std::sync::Arc;
use std::time::Duration;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::hal::descriptor::DisplayDescriptor;
use super::config::{AppConfig, ConfigManager, DisplayConfig};
use super::sync_engine::{DisplaySyncEngine, PiecewiseCurve};
use super::worker::HardwareWorkerHandle;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplayState {
    pub descriptor: DisplayDescriptor,
    pub user_friendly_name: Option<String>,
    pub current_brightness: u32,
    pub current_contrast: Option<u32>,
    pub min_limit: u32,
    pub max_limit: u32,
    pub show_contrast: bool,
    pub is_linked: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppState {
    pub displays: Vec<DisplayState>,
    pub linked_mode: bool,
    pub master_brightness: u32,
    pub sync_engine: DisplaySyncEngine,
    pub config: AppConfig,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            displays: Vec::new(),
            linked_mode: false,
            master_brightness: 75,
            sync_engine: DisplaySyncEngine::new(),
            config: AppConfig::default(),
        }
    }
}

/// Maps a 0..=100 UI percentage to the monitor's configured [min_limit..=max_limit] hardware range.
fn map_to_hardware_range(ui_percent: u32, min_limit: u32, max_limit: u32) -> u32 {
    let min_l = min_limit.min(95);
    let max_l = max_limit.max(min_l + 5).min(100);
    let ratio = ui_percent.clamp(0, 100) as f64 / 100.0;
    (min_l as f64 + ratio * (max_l - min_l) as f64).round() as u32
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

        worker.set_throttle_interval(Duration::from_millis(saved_cfg.throttle_ms.clamp(15, 250)));

        let mut app_state = AppState::default();
        app_state.linked_mode = saved_cfg.sync_enabled;
        app_state.master_brightness = saved_cfg.master_brightness;
        app_state.config = saved_cfg;

        Self {
            state: Arc::new(RwLock::new(app_state)),
            worker,
            config_manager: config_mgr,
        }
    }

    pub fn snapshot(&self) -> AppState {
        self.state.read().clone()
    }

    fn persist_state_locked(&self, state: &mut AppState) {
        state.config.sync_enabled = state.linked_mode;
        state.config.master_brightness = state.master_brightness;

        for disp in &state.displays {
            let entry = state
                .config
                .displays
                .entry(disp.descriptor.id.clone())
                .or_insert_with(DisplayConfig::default);
            entry.friendly_name = disp.user_friendly_name.clone();
            entry.is_linked = disp.is_linked;
            entry.min_brightness = disp.min_limit;
            entry.max_brightness = disp.max_limit;
            entry.saved_brightness = Some(disp.current_brightness);
            entry.saved_contrast = disp.current_contrast;
            entry.show_contrast = disp.show_contrast;
        }

        self.config_manager.save(&state.config);
    }

    /// Updates the display inventory from a fresh enumeration.
    pub fn update_inventory(&self, descriptors: Vec<DisplayDescriptor>) {
        let mut state = self.state.write();
        let restore_on_start = state.config.restore_brightness_on_startup && state.displays.is_empty();

        let mut new_displays = Vec::new();

        for desc in descriptors {
            let existing = state.displays.iter().find(|d| d.descriptor.id == desc.id);
            let saved_disp = state.config.displays.get(&desc.id).cloned();

            let user_name = existing
                .and_then(|d| d.user_friendly_name.clone())
                .or_else(|| saved_disp.as_ref().and_then(|d| d.friendly_name.clone()));

            let min_limit = existing
                .map(|d| d.min_limit)
                .or_else(|| saved_disp.as_ref().map(|d| d.min_brightness))
                .unwrap_or(0)
                .min(90);

            let max_limit = existing
                .map(|d| d.max_limit)
                .or_else(|| saved_disp.as_ref().map(|d| d.max_brightness))
                .unwrap_or(100)
                .clamp(min_limit + 10, 100);

            let show_contrast = existing
                .map(|d| d.show_contrast)
                .or_else(|| saved_disp.as_ref().map(|d| d.show_contrast))
                .unwrap_or(false);

            let mut current_bright = desc.capabilities.current_brightness;
            if restore_on_start {
                if let Some(saved_b) = saved_disp.as_ref().and_then(|d| d.saved_brightness) {
                    current_bright = saved_b.clamp(0, 100);
                    let hw_val = map_to_hardware_range(current_bright, min_limit, max_limit);
                    self.worker.set_brightness(desc.id.clone(), hw_val);
                }
            }

            let current_contrast = desc
                .capabilities
                .current_contrast
                .or_else(|| saved_disp.as_ref().and_then(|d| d.saved_contrast))
                .or(Some(50));

            let is_linked = existing
                .map(|d| d.is_linked)
                .or_else(|| saved_disp.as_ref().map(|d| d.is_linked))
                .unwrap_or(true);

            if let Some(disp_cfg) = saved_disp {
                if let Some(curve) = disp_cfg.custom_curve {
                    state.sync_engine.set_curve(desc.id.clone(), curve);
                }
            }

            new_displays.push(DisplayState {
                descriptor: desc,
                user_friendly_name: user_name,
                current_brightness: current_bright,
                current_contrast,
                min_limit,
                max_limit,
                show_contrast,
                is_linked,
            });
        }

        state.displays = new_displays;
        self.persist_state_locked(&mut state);
        info!("StateStore inventory updated: {} displays", state.displays.len());
    }

    /// Sets the master brightness slider and synchronizes all linked displays.
    pub fn set_master_brightness(&self, level: u32) {
        let clamped = level.clamp(0, 100);
        let mut state = self.state.write();
        state.master_brightness = clamped;

        if !state.linked_mode {
            self.persist_state_locked(&mut state);
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
                let hw_val = map_to_hardware_range(new_level, display.min_limit, display.max_limit);
                self.worker.set_brightness(display.descriptor.id.clone(), hw_val);
            }
        }
        self.persist_state_locked(&mut state);
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
                    let hw_val = map_to_hardware_range(new_level, display.min_limit, display.max_limit);
                    self.worker.set_brightness(display.descriptor.id.clone(), hw_val);
                }
            }
        } else if let Some(display) = state.displays.iter_mut().find(|d| d.descriptor.id == id) {
            display.current_brightness = clamped;
            let hw_val = map_to_hardware_range(clamped, display.min_limit, display.max_limit);
            self.worker.set_brightness(id.to_string(), hw_val);
        }
        self.persist_state_locked(&mut state);
    }

    /// Adjusts an individual display's contrast slider.
    pub fn set_display_contrast(&self, id: &str, level: u32) {
        let clamped = level.clamp(0, 100);
        let mut state = self.state.write();

        if let Some(display) = state.displays.iter_mut().find(|d| d.descriptor.id == id) {
            display.current_contrast = Some(clamped);
            self.worker.set_contrast(id.to_string(), clamped);
        }
        self.persist_state_locked(&mut state);
    }

    /// Updates a display's min/max brightness limits or contrast visibility.
    pub fn update_display_limits(
        &self,
        id: &str,
        min_limit: u32,
        max_limit: u32,
        show_contrast: bool,
    ) {
        let mut state = self.state.write();
        let min_clamped = min_limit.min(90);
        let max_clamped = max_limit.clamp(min_clamped + 10, 100);

        if let Some(display) = state.displays.iter_mut().find(|d| d.descriptor.id == id) {
            display.min_limit = min_clamped;
            display.max_limit = max_clamped;
            display.show_contrast = show_contrast;
            let hw_val = map_to_hardware_range(display.current_brightness, min_clamped, max_clamped);
            self.worker.set_brightness(id.to_string(), hw_val);
        }
        self.persist_state_locked(&mut state);
    }

    /// Toggles the master linked mode.
    pub fn set_linked_mode(&self, enabled: bool) {
        let mut state = self.state.write();
        state.linked_mode = enabled;

        if enabled {
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
                    let hw_val = map_to_hardware_range(new_level, display.min_limit, display.max_limit);
                    self.worker.set_brightness(display.descriptor.id.clone(), hw_val);
                }
            }
        }
        self.persist_state_locked(&mut state);
    }

    pub fn set_display_link_state(&self, id: &str, is_linked: bool) {
        let mut state = self.state.write();
        if let Some(display) = state.displays.iter_mut().find(|d| d.descriptor.id == id) {
            display.is_linked = is_linked;
        }
        self.persist_state_locked(&mut state);
    }

    pub fn set_calibration_curve(&self, id: String, curve: PiecewiseCurve) {
        let mut state = self.state.write();
        state.sync_engine.set_curve(id, curve);
    }

    pub fn update_preferences<F>(&self, updater: F)
    where
        F: FnOnce(&mut AppConfig),
    {
        let mut state = self.state.write();
        updater(&mut state.config);
        let throttle = state.config.throttle_ms.clamp(15, 250);
        self.worker.set_throttle_interval(Duration::from_millis(throttle));
        self.persist_state_locked(&mut state);
    }

    /// Resets all theme colors, monitor limits, contrast toggles, and preferences to factory defaults.
    pub fn reset_to_defaults(&self) {
        let mut state = self.state.write();
        let first_run_done = state.config.first_run_completed;
        state.config = AppConfig::default();
        state.config.first_run_completed = first_run_done;

        for disp in &mut state.displays {
            disp.user_friendly_name = None;
            disp.min_limit = 0;
            disp.max_limit = 100;
            disp.show_contrast = false;
            disp.is_linked = true;
            let hw_val = disp.current_brightness.clamp(0, 100);
            self.worker.set_brightness(disp.descriptor.id.clone(), hw_val);
        }

        self.worker.set_throttle_interval(Duration::from_millis(50));
        self.persist_state_locked(&mut state);
        info!("All LumaTray settings, colors, and monitor limits reset to factory defaults.");
    }
}
