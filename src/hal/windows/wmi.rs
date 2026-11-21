use std::collections::HashMap;
use tracing::{debug, info};

use windows::core::{w, BSTR};
use windows::Win32::System::Com::{
    CoCreateInstance,
    CoInitializeEx,
    CoInitializeSecurity,
    CoSetProxyBlanket,
    CLSCTX_INPROC_SERVER,
    COINIT_MULTITHREADED,
    RPC_C_AUTHN_LEVEL_CALL,
    RPC_C_AUTHN_LEVEL_CONNECT,
    RPC_C_IMP_LEVEL_IMPERSONATE,
    EOAC_NONE,
};
use windows::Win32::System::Ole::{SafeArrayGetElement, SafeArrayGetLBound, SafeArrayGetUBound};
use windows::Win32::System::Rpc::{RPC_C_AUTHN_WINNT, RPC_C_AUTHZ_NONE};
use windows::Win32::System::Variant::{
    VariantChangeType,
    VariantClear,
    VARIANT,
    VAR_CHANGE_FLAGS,
    VT_ARRAY,
    VT_I4,
};
use windows::Win32::System::Wmi::{
    IWbemClassObject,
    IWbemLocator,
    IWbemServices,
    WbemLocator,
    WBEM_FLAG_FORWARD_ONLY,
    WBEM_FLAG_RETURN_IMMEDIATELY,
};

use crate::hal::descriptor::{DisplayCapabilities, DisplayDescriptor, DriverType};
use crate::hal::error::DriverError;
use super::display_config::{get_display_config_targets, query_gdi_display_mode};

pub struct WmiDisplayEntry {
    pub descriptor: DisplayDescriptor,
    pub instance_name: String,
    pub wmi_object_path: Option<String>,
}

pub struct WindowsWmiDriver {
    monitors: HashMap<String, WmiDisplayEntry>,
}

fn initialize_com_security() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let _ = CoInitializeSecurity(
            None,
            -1,
            None,
            None,
            RPC_C_AUTHN_LEVEL_CONNECT,
            RPC_C_IMP_LEVEL_IMPERSONATE,
            None,
            EOAC_NONE,
            None,
        );
    }
}

fn connect_wmi() -> windows::core::Result<IWbemServices> {
    initialize_com_security();

    unsafe {
        let locator: IWbemLocator = CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER)?;
        let service = locator.ConnectServer(
            &BSTR::from("ROOT\\WMI"),
            &BSTR::default(),
            &BSTR::default(),
            &BSTR::default(),
            0,
            &BSTR::default(),
            None,
        )?;

        let _ = CoSetProxyBlanket(
            &service,
            RPC_C_AUTHN_WINNT,
            RPC_C_AUTHZ_NONE,
            None,
            RPC_C_AUTHN_LEVEL_CALL,
            RPC_C_IMP_LEVEL_IMPERSONATE,
            None,
            EOAC_NONE,
        );

        Ok(service)
    }
}

fn variant_to_i32(v: &VARIANT) -> Option<i32> {
    unsafe {
        let mut dest = VARIANT::default();
        if VariantChangeType(&mut dest, v, VAR_CHANGE_FLAGS(0), VT_I4).is_ok() {
            let val = dest.Anonymous.Anonymous.Anonymous.lVal;
            let _ = VariantClear(&mut dest);
            Some(val)
        } else {
            None
        }
    }
}

fn variant_to_string(v: &VARIANT) -> Option<String> {
    unsafe {
        let bstr = &v.Anonymous.Anonymous.Anonymous.bstrVal;
        if !bstr.is_empty() {
            Some(bstr.to_string())
        } else {
            None
        }
    }
}

fn extract_wmi_uint16_string(v: &VARIANT) -> String {
    unsafe {
        let vt = v.Anonymous.Anonymous.vt;
        if (vt.0 & VT_ARRAY.0) != 0 {
            let parray = v.Anonymous.Anonymous.Anonymous.parray;
            if !parray.is_null() {
                if let (Ok(lower), Ok(upper)) = (SafeArrayGetLBound(parray, 1), SafeArrayGetUBound(parray, 1)) {
                    let mut chars = Vec::new();
                    for i in lower..=upper {
                        let mut elem = 0u16;
                        if SafeArrayGetElement(parray, &i, &mut elem as *mut _ as *mut _).is_ok() {
                            if elem == 0 {
                                break;
                            }
                            chars.push(elem);
                        }
                    }
                    return String::from_utf16_lossy(&chars);
                }
            }
        }
    }
    String::new()
}

#[allow(dead_code)]
impl WindowsWmiDriver {
    pub fn new() -> Self {
        Self {
            monitors: HashMap::new(),
        }
    }

    pub fn enumerate(&mut self) -> Result<Vec<DisplayDescriptor>, DriverError> {
        self.monitors.clear();

        let service = match connect_wmi() {
            Ok(s) => s,
            Err(e) => {
                debug!("WMI ConnectServer(ROOT\\WMI) not available: {:?}", e);
                return Ok(Vec::new());
            }
        };

        // 1. Collect friendly names from WmiMonitorID
        let mut friendly_names = HashMap::new();
        unsafe {
            let query = BSTR::from("SELECT InstanceName, UserFriendlyName FROM WmiMonitorID");
            if let Ok(enumerator) = service.ExecQuery(
                &BSTR::from("WQL"),
                &query,
                WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY,
                None,
            ) {
                loop {
                    let mut class_obj = [None];
                    let mut returned = 0u32;
                    if enumerator.Next(500, &mut class_obj, &mut returned).is_err() || returned == 0 {
                        break;
                    }
                    if let Some(obj) = &class_obj[0] {
                        let mut inst_v = VARIANT::default();
                        let mut name_v = VARIANT::default();
                        let _ = obj.Get(w!("InstanceName"), 0, &mut inst_v, None, None);
                        let _ = obj.Get(w!("UserFriendlyName"), 0, &mut name_v, None, None);

                        if let Some(inst_name) = variant_to_string(&inst_v) {
                            let name = extract_wmi_uint16_string(&name_v);
                            if !name.is_empty() {
                                friendly_names.insert(inst_name, name);
                            }
                        }
                        let _ = VariantClear(&mut inst_v);
                        let _ = VariantClear(&mut name_v);
                    }
                }
            }
        }

        // 2. Collect exact __PATH object paths from WmiMonitorBrightnessMethods
        let mut method_paths = HashMap::new();
        unsafe {
            let query = BSTR::from("SELECT __PATH, InstanceName FROM WmiMonitorBrightnessMethods");
            if let Ok(enumerator) = service.ExecQuery(
                &BSTR::from("WQL"),
                &query,
                WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY,
                None,
            ) {
                loop {
                    let mut class_obj = [None];
                    let mut returned = 0u32;
                    if enumerator.Next(500, &mut class_obj, &mut returned).is_err() || returned == 0 {
                        break;
                    }
                    if let Some(obj) = &class_obj[0] {
                        let mut inst_v = VARIANT::default();
                        let mut path_v = VARIANT::default();
                        let _ = obj.Get(w!("InstanceName"), 0, &mut inst_v, None, None);
                        let _ = obj.Get(w!("__PATH"), 0, &mut path_v, None, None);

                        if let (Some(inst_name), Some(wmi_path)) = (variant_to_string(&inst_v), variant_to_string(&path_v)) {
                            method_paths.insert(inst_name, wmi_path);
                        }
                        let _ = VariantClear(&mut inst_v);
                        let _ = VariantClear(&mut path_v);
                    }
                }
            }
        }

        // 3. Query active internal brightness from WmiMonitorBrightness
        let mut results = Vec::new();
        unsafe {
            let query = BSTR::from("SELECT InstanceName, CurrentBrightness FROM WmiMonitorBrightness");
            let enumerator = match service.ExecQuery(
                &BSTR::from("WQL"),
                &query,
                WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY,
                None,
            ) {
                Ok(e) => e,
                Err(_) => {
                    debug!("WmiMonitorBrightness query not supported on this device.");
                    return Ok(results);
                }
            };

            loop {
                let mut class_obj = [None];
                let mut returned = 0u32;
                if enumerator.Next(500, &mut class_obj, &mut returned).is_err() || returned == 0 {
                    break;
                }
                if let Some(obj) = &class_obj[0] {
                    let mut inst_v = VARIANT::default();
                    let mut bright_v = VARIANT::default();
                    let _ = obj.Get(w!("InstanceName"), 0, &mut inst_v, None, None);
                    let _ = obj.Get(w!("CurrentBrightness"), 0, &mut bright_v, None, None);

                    let instance = variant_to_string(&inst_v).unwrap_or_default();
                    let brightness = variant_to_i32(&bright_v).unwrap_or(50).clamp(0, 100) as u32;

                    let _ = VariantClear(&mut inst_v);
                    let _ = VariantClear(&mut bright_v);

                    if instance.is_empty() {
                        continue;
                    }

                    let friendly = friendly_names
                        .get(&instance)
                        .cloned()
                        .unwrap_or_else(|| "Integrated Display".to_string());

                    let wmi_path = method_paths.get(&instance).cloned();

                    // Match WMI instance hardware ID against DisplayConfig targets to get real GDI name
                    let hw_code = instance.split('\\').nth(1).unwrap_or("").to_uppercase();
                    let targets = get_display_config_targets();
                    let matched_gdi = targets
                        .iter()
                        .find(|t| !hw_code.is_empty() && t.device_path.to_uppercase().contains(&hw_code))
                        .map(|t| t.gdi_device_name.clone())
                        .unwrap_or_else(|| "InternalPanel".to_string());

                    let (resolution, refresh_rate_hz) = query_gdi_display_mode(&matched_gdi);

                    let id = format!("WMI\\{}", instance);
                    let descriptor = DisplayDescriptor {
                        id: id.clone(),
                        name: friendly,
                        gdi_name: matched_gdi,
                        resolution,
                        refresh_rate_hz,
                        driver_type: DriverType::WmiInternal,
                        is_internal: true,
                        capabilities: DisplayCapabilities {
                            min_brightness: 0,
                            max_brightness: 100,
                            current_brightness: brightness,
                            supports_contrast: false,
                            min_contrast: None,
                            max_contrast: None,
                            current_contrast: None,
                            vcp_brightness_code: 0x10,
                            supported_vcp_codes: vec![0x10],
                        },
                    };

                    info!(
                        "Registered WMI Internal Display: '{}' [ID: {}] (Brightness: {})",
                        descriptor.name, descriptor.id, brightness
                    );

                    self.monitors.insert(
                        id.clone(),
                        WmiDisplayEntry {
                            descriptor: descriptor.clone(),
                            instance_name: instance,
                            wmi_object_path: wmi_path,
                        },
                    );

                    results.push(descriptor);
                }
            }
        }

        Ok(results)
    }

    pub fn get_brightness(&mut self, id: &str) -> Result<u32, DriverError> {
        let entry = self
            .monitors
            .get_mut(id)
            .ok_or_else(|| DriverError::MonitorNotFound(id.to_string()))?;

        let service = connect_wmi().map_err(|e| DriverError::Wmi {
            hresult: e.code().0,
            message: e.message().to_string(),
        })?;

        let query = format!(
            "SELECT CurrentBrightness FROM WmiMonitorBrightness WHERE InstanceName = '{}'",
            entry.instance_name.replace('\\', "\\\\")
        );

        unsafe {
            if let Ok(enumerator) = service.ExecQuery(
                &BSTR::from("WQL"),
                &BSTR::from(query),
                WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY,
                None,
            ) {
                let mut class_obj = [None];
                let mut returned = 0u32;
                if enumerator.Next(500, &mut class_obj, &mut returned).is_ok() && returned > 0 {
                    if let Some(obj) = &class_obj[0] {
                        let mut bright_v = VARIANT::default();
                        let _ = obj.Get(w!("CurrentBrightness"), 0, &mut bright_v, None, None);
                        let val = variant_to_i32(&bright_v).unwrap_or(50).clamp(0, 100) as u32;
                        let _ = VariantClear(&mut bright_v);
                        entry.descriptor.capabilities.current_brightness = val;
                        return Ok(val);
                    }
                }
            }
        }

        Ok(entry.descriptor.capabilities.current_brightness)
    }

    pub fn set_brightness(&mut self, id: &str, level: u32) -> Result<(), DriverError> {
        let entry = self
            .monitors
            .get_mut(id)
            .ok_or_else(|| DriverError::MonitorNotFound(id.to_string()))?;

        let service = connect_wmi().map_err(|e| DriverError::Wmi {
            hresult: e.code().0,
            message: e.message().to_string(),
        })?;

        let clamped = level.clamp(0, 100);

        unsafe {
            // 1. Get method parameters template
            let mut method_class: Option<IWbemClassObject> = None;
            service
                .GetObject(
                    &BSTR::from("WmiMonitorBrightnessMethods"),
                    Default::default(),
                    None,
                    Some(&mut method_class),
                    None,
                )
                .map_err(|e| DriverError::Wmi {
                    hresult: e.code().0,
                    message: format!("Failed to get WmiMonitorBrightnessMethods: {}", e.message()),
                })?;

            let method_class = method_class.unwrap();
            let mut in_def: Option<IWbemClassObject> = None;
            let mut out_def: Option<IWbemClassObject> = None;
            method_class
                .GetMethod(
                    w!("WmiSetBrightness"),
                    0,
                    &mut in_def as *mut _,
                    &mut out_def as *mut _,
                )
                .map_err(|e| DriverError::Wmi {
                    hresult: e.code().0,
                    message: format!("Failed to get WmiSetBrightness signature: {}", e.message()),
                })?;

            let in_def = in_def.unwrap();
            let in_params = in_def.SpawnInstance(0).map_err(|e| DriverError::Wmi {
                hresult: e.code().0,
                message: format!("Failed to spawn WmiSetBrightness instance: {}", e.message()),
            })?;

            // Populate Timeout = 1
            let v_timeout: VARIANT = 1i32.into();
            let _ = in_params.Put(w!("Timeout"), 0, &v_timeout, 0);

            // Populate Brightness = clamped
            let v_brightness: VARIANT = (clamped as i32).into();
            let _ = in_params.Put(w!("Brightness"), 0, &v_brightness, 0);

            // 2. Determine target __PATH: use stored exact object path if known, otherwise query
            let target_path = if let Some(ref p) = entry.wmi_object_path {
                p.clone()
            } else {
                let query = format!(
                    "SELECT __PATH FROM WmiMonitorBrightnessMethods WHERE InstanceName = '{}'",
                    entry.instance_name.replace('\\', "\\\\")
                );
                let enumerator = service.ExecQuery(
                    &BSTR::from("WQL"),
                    &BSTR::from(query),
                    WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY,
                    None,
                ).map_err(|e| DriverError::Wmi {
                    hresult: e.code().0,
                    message: format!("Failed to query WmiMonitorBrightnessMethods path: {}", e.message()),
                })?;

                let mut class_obj = [None];
                let mut returned = 0u32;
                if enumerator.Next(500, &mut class_obj, &mut returned).is_ok() && returned > 0 {
                    if let Some(obj) = &class_obj[0] {
                        let mut path_v = VARIANT::default();
                        let _ = obj.Get(w!("__PATH"), 0, &mut path_v, None, None);
                        let p = variant_to_string(&path_v).unwrap_or_default();
                        let _ = VariantClear(&mut path_v);
                        p
                    } else {
                        String::new()
                    }
                } else {
                    String::new()
                }
            };

            if target_path.is_empty() {
                return Err(DriverError::Wmi {
                    hresult: -1,
                    message: format!("No valid WmiMonitorBrightnessMethods __PATH for '{}'", entry.instance_name),
                });
            }

            service
                .ExecMethod(
                    &BSTR::from(target_path),
                    &BSTR::from("WmiSetBrightness"),
                    Default::default(),
                    None,
                    &in_params,
                    None,
                    None,
                )
                .map_err(|e| DriverError::Wmi {
                    hresult: e.code().0,
                    message: format!("ExecMethod(WmiSetBrightness) failed: {}", e.message()),
                })?;

            entry.descriptor.capabilities.current_brightness = clamped;
        }

        Ok(())
    }
}
