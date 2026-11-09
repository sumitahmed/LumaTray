pub mod descriptor;
pub mod error;
pub mod windows;

pub use descriptor::DisplayDescriptor;
pub use error::DriverError;

/// Core Hardware Abstraction Layer interface for display backends.
/// Implementations must be thread-safe (`Send + Sync`).
#[allow(dead_code)]
pub trait DisplayDriver: Send + Sync {
    /// Discovers all currently active monitors and their hardware capabilities.
    fn enumerate(&mut self) -> Result<Vec<DisplayDescriptor>, DriverError>;

    /// Reads the current brightness level (0..=100) for a specific display.
    fn get_brightness(&mut self, id: &str) -> Result<u32, DriverError>;

    /// Sets the brightness level (0..=100) for a specific display.
    fn set_brightness(&mut self, id: &str, level: u32) -> Result<(), DriverError>;

    /// Reads the current contrast level (0..=100) if supported.
    fn get_contrast(&mut self, id: &str) -> Result<u32, DriverError>;

    /// Sets the contrast level (0..=100) if supported.
    fn set_contrast(&mut self, id: &str, level: u32) -> Result<(), DriverError>;

    /// Reads an arbitrary VCP feature code (returns current value and maximum value).
    fn get_vcp(&mut self, id: &str, vcp_code: u8) -> Result<(u32, u32), DriverError>;

    /// Writes an arbitrary VCP feature code value.
    fn set_vcp(&mut self, id: &str, vcp_code: u8, value: u32) -> Result<(), DriverError>;
}
