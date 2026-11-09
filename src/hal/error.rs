use thiserror::Error;

#[derive(Error, Debug)]
#[allow(dead_code)]
pub enum DriverError {
    #[error("Win32 error (code {code:#010X}): {message}")]
    Win32 { code: u32, message: String },

    #[error("WMI COM error (HRESULT {hresult:#010X}): {message}")]
    Wmi { hresult: i32, message: String },

    #[error("Monitor not found: {0}")]
    MonitorNotFound(String),

    #[error("Unsupported monitor feature: {0}")]
    Unsupported(String),

    #[error("I2C/DDC communication failure after retries: {0}")]
    DdcTimeout(String),

    #[error("Internal driver error: {0}")]
    Internal(String),
}
