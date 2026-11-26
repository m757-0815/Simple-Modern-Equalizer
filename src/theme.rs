use serde::{Deserialize, Deserializer, Serialize};

// #######################################################
// set the plot colors and default accent
// #######################################################

// These neutrals match the Slint surfaces and dividers.
pub const PLOT_BACKGROUND: [u8; 3] = [21, 24, 29];
pub const PLOT_GRID: [u8; 3] = [45, 49, 57];
pub const PLOT_ZERO: [u8; 3] = [75, 80, 89];
pub const DEFAULT_ACCENT: [u8; 3] = [57, 201, 138];
const CORAL_DEFAULT: [u8; 3] = [255, 111, 105];
const MINT_DEFAULT: [u8; 3] = [119, 217, 189];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct ThemeColors {
    #[serde(rename = "accent_rgb", skip_serializing_if = "is_default_accent")]
    pub accent: [u8; 3],
}

fn is_default_accent(rgb: &[u8; 3]) -> bool {
    *rgb == DEFAULT_ACCENT
}

impl Default for ThemeColors {
    fn default() -> Self {
        Self { accent: DEFAULT_ACCENT }
    }
}

impl<'de> Deserialize<'de> for ThemeColors {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct SavedColors {
            accent_rgb: Option<[u8; 3]>,
            accent: Option<[u8; 3]>,
            graph: Option<[u8; 3]>,
            text: Option<[u8; 3]>,
        }
        let saved = SavedColors::deserialize(deserializer)?;
        let custom = |color: Option<[u8; 3]>| color.filter(|rgb| *rgb != MINT_DEFAULT);
        let accent = saved.accent_rgb
            .or_else(|| saved.accent.filter(|rgb| *rgb != CORAL_DEFAULT))
            .or_else(|| custom(saved.graph)).or_else(|| custom(saved.text))
            .unwrap_or(DEFAULT_ACCENT);
        Ok(Self { accent })
    }
}

// #######################################################
// derive widget and graph colors from the accent
// #######################################################

impl ThemeColors {
    pub fn parse(value: &str) -> Result<Self, String> {
        let value = value.trim();
        let value = value.strip_prefix('#').unwrap_or(value);
        if value.len() != 6 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(format!("Enter a six-digit color such as {}.", Self::hex(DEFAULT_ACCENT)));
        }
        let rgb = u32::from_str_radix(value, 16).unwrap().to_be_bytes();
        Ok(Self { accent: [rgb[1], rgb[2], rgb[3]] })
    }

    pub fn hex([red, green, blue]: [u8; 3]) -> String {
        format!("#{red:02X}{green:02X}{blue:02X}")
    }

    pub fn graph_tint(self, strength: f32) -> [u8; 3] {
        std::array::from_fn(|channel| {
            let background = f32::from(PLOT_BACKGROUND[channel]);
            (background + (f32::from(self.accent[channel]) - background) * strength).round() as u8
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_colors_validate_and_normalize_hex() {
        let colors = ThemeColors::parse(" #aabbcc ").unwrap();
        assert_eq!(ThemeColors::hex(colors.accent), "#AABBCC");
        for invalid in ["##AABBCC", "#12345", "#oops00"] {
            assert!(ThemeColors::parse(invalid).is_err());
        }
    }

    #[test]
    fn migrates_previous_defaults_and_keeps_custom_colors() {
        for (saved, expected) in [
            (r#"{"graph":[119,217,189],"text":[119,217,189]}"#, DEFAULT_ACCENT),
            (r#"{"graph":[10,20,30],"text":[30,40,50]}"#, [10, 20, 30]),
            (r#"{"graph":[119,217,189],"text":[30,40,50]}"#, [30, 40, 50]),
            (r#"{"accent":[119,217,189]}"#, MINT_DEFAULT),
            (r#"{"accent":[255,111,105]}"#, DEFAULT_ACCENT),
            (r#"{"accent_rgb":[255,111,105]}"#, CORAL_DEFAULT),
        ] {
            assert_eq!(serde_json::from_str::<ThemeColors>(saved).unwrap().accent, expected);
        }
        assert_eq!(serde_json::to_string(&ThemeColors::default()).unwrap(), "{}");
        let coral = ThemeColors { accent: CORAL_DEFAULT };
        assert_eq!(serde_json::from_str::<ThemeColors>(&serde_json::to_string(&coral).unwrap()).unwrap(), coral);
    }
}
