use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub model: Option<String>,
    pub android_version: Option<String>,
    pub screen_size: Option<(u32, u32)>,
}

impl Device {
    pub fn new(id: String) -> Self {
        Self {
            id,
            model: None,
            android_version: None,
            screen_size: None,
        }
    }

    pub fn with_info(
        id: String,
        model: String,
        android_version: String,
        screen_size: (u32, u32),
    ) -> Self {
        Self {
            id,
            model: Some(model),
            android_version: Some(android_version),
            screen_size: Some(screen_size),
        }
    }
}
