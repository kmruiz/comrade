//! Stable per-agent colors.
//!
//! Every model that takes part in a session - the main agent and each configured
//! delegate - is assigned one palette hue when the app starts. That hue colors
//! the agent's name everywhere it appears (chat author tags, the model panel,
//! the model-pick overlay) and, heavily dimmed, the background band of the
//! agent's sub-chat in the chat window.

use ratatui::style::Color;

/// The chat's background base; keep in sync with `tui::DIFF_BASE_BG`.
const BASE: (u8, u8, u8) = (0x13, 0x14, 0x18);

/// Per-agent name colors, handed out in order on first sight. Bright enough to
/// read as a foreground on the dark chat base.
pub const AGENT_PALETTE: [Color; 8] = [
    Color::Rgb(0x6d, 0x9c, 0xff), // blue
    Color::Rgb(0xff, 0xb0, 0x5c), // amber
    Color::Rgb(0x7d, 0xd8, 0x8a), // green
    Color::Rgb(0xd9, 0x8c, 0xff), // violet
    Color::Rgb(0x62, 0xd6, 0xd0), // teal
    Color::Rgb(0xff, 0x8f, 0xa8), // rose
    Color::Rgb(0xd8, 0xd0, 0x6a), // olive
    Color::Rgb(0x9c, 0xb0, 0xc8), // slate
];

/// How strongly an agent color tints its sub-chat band. Low, so the band reads
/// as "really dimmed".
const BAND_ALPHA: f32 = 0.14;

/// Color used for a model that was never registered.
const FALLBACK: Color = Color::Rgb(0x9c, 0xb0, 0xc8);

/// Assigns a stable color to each model name, in first-seen order.
#[derive(Default)]
pub struct ModelColors {
    entries: Vec<(String, Color)>,
}

impl ModelColors {
    pub fn new() -> Self {
        Self::default()
    }

    /// Assign a palette color to every name not seen yet, in order. Call once
    /// at start with the main model and the configured delegates.
    pub fn assign(&mut self, names: &[String]) {
        for name in names {
            self.slot(name);
        }
    }

    /// Assign (once) and return the color for `name`.
    fn slot(&mut self, name: &str) -> Color {
        if let Some((_, color)) = self.entries.iter().find(|(n, _)| n == name) {
            return *color;
        }
        let color = AGENT_PALETTE[self.entries.len() % AGENT_PALETTE.len()];
        self.entries.push((name.to_string(), color));
        color
    }

    /// The name color for `name` (`FALLBACK` when unknown; never mutates).
    pub fn name_color(&self, name: &str) -> Color {
        self.entries
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, color)| *color)
            .unwrap_or(FALLBACK)
    }

    /// The dimmed background band for `name`'s sub-chat.
    pub fn band_color(&self, name: &str) -> Color {
        blend(BASE, rgb_of(self.name_color(name)), BAND_ALPHA)
    }
}

/// The `(r, g, b)` of an RGB color, or `BASE` for anything else.
fn rgb_of(color: Color) -> (u8, u8, u8) {
    match color {
        Color::Rgb(r, g, b) => (r, g, b),
        _ => BASE,
    }
}

/// Blend `tint` over `base` at `alpha` (0.0 = base, 1.0 = tint).
pub fn blend(base: (u8, u8, u8), tint: (u8, u8, u8), alpha: f32) -> Color {
    let mix = |b: u8, t: u8| (b as f32 * (1.0 - alpha) + t as f32 * alpha).round() as u8;
    Color::Rgb(
        mix(base.0, tint.0),
        mix(base.1, tint.1),
        mix(base.2, tint.2),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn same_name_keeps_the_same_color() {
        let mut c = ModelColors::new();
        c.assign(&names(&["a", "b"]));
        let first = c.name_color("a");
        c.assign(&names(&["a"]));
        assert_eq!(c.name_color("a"), first);
    }

    #[test]
    fn distinct_names_get_distinct_palette_colors() {
        let mut c = ModelColors::new();
        c.assign(&names(&["a", "b"]));
        assert_eq!(c.name_color("a"), AGENT_PALETTE[0]);
        assert_eq!(c.name_color("b"), AGENT_PALETTE[1]);
    }

    #[test]
    fn unknown_name_falls_back() {
        let c = ModelColors::new();
        assert_eq!(c.name_color("nobody"), FALLBACK);
    }

    #[test]
    fn band_is_dimmed_between_base_and_name() {
        let mut c = ModelColors::new();
        c.assign(&names(&["a"]));
        assert_ne!(c.band_color("a"), c.name_color("a"));
        let Color::Rgb(r, g, b) = c.band_color("a") else {
            panic!("band must be rgb");
        };
        let (nr, ng, nb) = rgb_of(c.name_color("a"));
        // Each channel stays between the base and the name color (i.e. dimmer).
        assert!(r <= nr.max(BASE.0) && r >= BASE.0.min(nr));
        assert!(g <= ng.max(BASE.1) && g >= BASE.1.min(ng));
        assert!(b <= nb.max(BASE.2) && b >= BASE.2.min(nb));
    }
}
