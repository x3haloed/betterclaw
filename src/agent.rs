use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub id: String,
    pub display_name: String,
    pub workspace_id: String,
}

impl Agent {
    pub fn new(
        id: impl Into<String>,
        display_name: impl Into<String>,
        workspace_id: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            display_name: display_name.into(),
            workspace_id: workspace_id.into(),
        }
    }
}
