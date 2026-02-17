use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Swap {
    Replace,
    Merge,
}

impl Default for Swap {
    fn default() -> Self {
        Self::Replace
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Patch {
    pub target: String,
    #[serde(default)]
    pub swap: Swap,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub html: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiUpdate {
    pub event: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
    pub patches: Vec<Patch>,
}

impl UiUpdate {
    pub fn new(event: impl Into<String>, patches: Vec<Patch>) -> Self {
        Self {
            event: event.into(),
            payload: None,
            patches,
        }
    }
}

pub mod targets {
    pub const PANEL_BOTTOM_BAR: &str = "panel.bottom.bar";
    pub const PANEL_RIGHT: &str = "panel.right";
    pub const PANEL_LEFT: &str = "panel.left";
}
