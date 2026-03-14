use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelDefinition {
    pub name: String,
    pub description: String,
}

impl ChannelDefinition {
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
        }
    }
}

pub trait Channel: Send + Sync {
    fn definition(&self) -> ChannelDefinition;
}
