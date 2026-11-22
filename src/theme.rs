use serde::{Deserialize, Serialize};

// Raster graph colors shared by the EQ response and Live View.
pub const PLOT_BACKGROUND: [u8; 3] = [35, 35, 37];
pub const PLOT_GRID: [u8; 3] = [53, 53, 57];
pub const PLOT_ZERO: [u8; 3] = [77, 77, 83];
pub const MINT: [u8; 3] = [119, 217, 189];
pub const METER_SCALE: [u8; 3] = [164, 164, 172];
pub const METER_HOT: [u8; 3] = [224, 145, 97];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemeColors {
    pub graph: [u8; 3],
    pub text: [u8; 3],
}

impl Default for ThemeColors {
    fn default() -> Self {
        Self { graph: MINT, text: MINT }
    }
}

impl ThemeColors {
    pub fn parse(graph: &str, text: &str) -> Result<Self, String> {
        let parse_color = |name: &str, value: &str| {
            let value = value.trim();
            let value = value.strip_prefix('#').unwrap_or(value);
            if value.len() != 6 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(format!("{name} needs a six-digit color such as #77D9BD."));
            }
            let rgb = u32::from_str_radix(value, 16).unwrap().to_be_bytes();
            Ok([rgb[1], rgb[2], rgb[3]])
        };
        Ok(Self {
            graph: parse_color("Graphs", graph)?,
            text: parse_color("Accent text", text)?,
        })
    }

    pub fn hex([red, green, blue]: [u8; 3]) -> String {
        format!("#{red:02X}{green:02X}{blue:02X}")
    }

    pub fn graph_tint(self, strength: f32) -> [u8; 3] {
        std::array::from_fn(|channel| {
            let background = f32::from(PLOT_BACKGROUND[channel]);
            (background + (f32::from(self.graph[channel]) - background) * strength).round() as u8
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_colors_validate_and_normalize_hex() {
        let colors = ThemeColors::parse(" #aabbcc ", "dDeEfF").unwrap();
        assert_eq!(ThemeColors::hex(colors.graph), "#AABBCC");
        assert_eq!(ThemeColors::hex(colors.text), "#DDEEFF");
        assert!(ThemeColors::parse("##AABBCC", "#DDEEFF").is_err());
        assert!(ThemeColors::parse("#AABBCC", "#12345").is_err());
        assert!(ThemeColors::parse("#AABBCC", "#oops00").is_err());
    }
}
