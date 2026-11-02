use serde::{Deserialize, Serialize};
use ts_rs::TS;

pub const VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(export)]
pub enum ControlEvent {
    Pointer {
        x: f64,
        y: f64,
        #[serde(default)]
        seq: u32,
    },
    Button {
        button: MouseButton,
        down: bool,
        #[serde(default)]
        seq: u32,
        #[serde(default)]
        x: Option<f64>,
        #[serde(default)]
        y: Option<f64>,
    },
    Wheel {
        dx: f64,
        dy: f64,
    },
    Key {
        code: String,
        down: bool,
    },
    Text {
        text: String,
    },
    ReleaseAll,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

impl ControlEvent {
    pub fn validate(&self) -> bool {
        match self {
            Self::Pointer { x, y, .. } => {
                x.is_finite() && y.is_finite() && (0.0..=1.0).contains(x) && (0.0..=1.0).contains(y)
            }
            Self::Button { x, y, .. } => match (x, y) {
                (Some(x), Some(y)) => {
                    x.is_finite()
                        && y.is_finite()
                        && (0.0..=1.0).contains(x)
                        && (0.0..=1.0).contains(y)
                }
                (None, None) => true,
                _ => false,
            },
            Self::Wheel { dx, dy } => {
                dx.is_finite() && dy.is_finite() && dx.abs() <= 2000.0 && dy.abs() <= 2000.0
            }
            Self::Key { code, .. } => !code.is_empty() && code.len() <= 40,
            Self::Text { text } => text.len() <= 16_384 && !text.contains('\0'),
            _ => true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct DisplayInfo {
    pub id: String,
    pub width: u32,
    pub height: u32,
    pub scale: f64,
}

/// The relay treats payloads as opaque JSON and never logs their contents.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct Envelope {
    pub version: u32,
    pub to: String,
    #[serde(default)]
    pub from: String,
    #[ts(type = "unknown")]
    pub payload: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_invalid_input() {
        assert!(!ControlEvent::Pointer {
            x: f64::NAN,
            y: 0.0,
            seq: 1
        }
        .validate());
        assert!(!ControlEvent::Pointer {
            x: 1.1,
            y: 0.0,
            seq: 1
        }
        .validate());
        assert!(!ControlEvent::Wheel {
            dx: 0.0,
            dy: f64::INFINITY
        }
        .validate());
        assert!(ControlEvent::Text {
            text: "繁體中文🦀".into()
        }
        .validate());
    }
}
