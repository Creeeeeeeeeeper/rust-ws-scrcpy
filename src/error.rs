use thiserror::Error;

#[derive(Error, Debug)]
pub enum ScrcpyError {
    #[error("ADB error: {0}")]
    Adb(String),

    #[error("Device not found")]
    DeviceNotFound,

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Network error: {0}")]
    Network(String),

    #[error("Video stream error: {0}")]
    VideoStream(String),

    #[error("Parse error: {0}")]
    Parse(String),

    #[error("No available port found in range {0}-{1}")]
    NoAvailablePort(u16, u16),
}

pub type Result<T> = std::result::Result<T, ScrcpyError>;
