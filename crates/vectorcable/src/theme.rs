//! The Vector design system in the terminal. Every colour comes from the
//! module `build.rs` generates out of `design/vector-tokens.json`; nothing
//! here names a colour by value.
//!
//! Depth: 24-bit when `COLORTERM` says so, xterm-256 otherwise, and no colour
//! at all (bold and reverse video only) under `NO_COLOR` or a terminal with
//! 16 colours or fewer.

use ratatui::style::{Color, Modifier, Style};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

/// One colour token, in both themes and both depths.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColorToken {
    pub name: &'static str,
    pub standard: Rgb,
    pub night_ops: Rgb,
    pub standard_256: u8,
    pub night_ops_256: u8,
}

/// One constant per colour token in `design/vector-tokens.json`; the UI uses
/// the ones a terminal can show.
#[allow(dead_code)]
mod tokens {
    use super::{ColorToken, Rgb};
    include!(concat!(env!("OUT_DIR"), "/theme_tokens.rs"));
}
pub use tokens::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThemeName {
    Standard,
    NightOps,
}

impl ThemeName {
    pub fn word(self) -> &'static str {
        match self {
            ThemeName::Standard => "Standard",
            ThemeName::NightOps => "Night Ops",
        }
    }

    pub fn parse(s: &str) -> Option<ThemeName> {
        match s.trim().to_ascii_lowercase().as_str() {
            "standard" => Some(ThemeName::Standard),
            "night-ops" | "night_ops" | "nightops" | "night ops" => Some(ThemeName::NightOps),
            _ => None,
        }
    }

    pub fn toggled(self) -> ThemeName {
        match self {
            ThemeName::Standard => ThemeName::NightOps,
            ThemeName::NightOps => ThemeName::Standard,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Depth {
    TrueColor,
    Indexed256,
    /// `NO_COLOR`, or 16 colours or fewer: bold and reverse video only.
    None,
}

impl Depth {
    /// From the environment: `NO_COLOR` wins, then `COLORTERM`, then `TERM`.
    pub fn detect(env: &dyn Fn(&str) -> Option<String>) -> Depth {
        if env("NO_COLOR").is_some_and(|v| !v.is_empty()) {
            return Depth::None;
        }
        let colorterm = env("COLORTERM").unwrap_or_default().to_ascii_lowercase();
        if colorterm == "truecolor" || colorterm == "24bit" {
            return Depth::TrueColor;
        }
        let term = env("TERM").unwrap_or_default().to_ascii_lowercase();
        if term.contains("256color") || term.contains("direct") {
            Depth::Indexed256
        } else {
            Depth::None
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Theme {
    pub name: ThemeName,
    pub depth: Depth,
    /// False where the terminal misreads the underline-colour sequence.
    pub underline_colour: bool,
}

/// Whether the terminal can colour underlines (SGR 58). macOS Terminal.app
/// cannot: it reads `58;2;r;g;b` as separate codes, so `2` turns the text
/// faint and a `b` such as 46 paints an ANSI background (test 11, 2026-10-07).
pub fn underline_colour_supported(env: &dyn Fn(&str) -> Option<String>) -> bool {
    env("TERM_PROGRAM").as_deref() != Some("Apple_Terminal")
}

impl Theme {
    pub fn new(name: ThemeName, depth: Depth) -> Theme {
        Theme {
            name,
            depth,
            underline_colour: true,
        }
    }

    pub fn color(&self, token: &ColorToken) -> Option<Color> {
        match (self.depth, self.name) {
            (Depth::None, _) => None,
            (Depth::TrueColor, ThemeName::Standard) => {
                let c = token.standard;
                Some(Color::Rgb(c.0, c.1, c.2))
            }
            (Depth::TrueColor, ThemeName::NightOps) => {
                let c = token.night_ops;
                Some(Color::Rgb(c.0, c.1, c.2))
            }
            (Depth::Indexed256, ThemeName::Standard) => Some(Color::Indexed(token.standard_256)),
            (Depth::Indexed256, ThemeName::NightOps) => Some(Color::Indexed(token.night_ops_256)),
        }
    }

    /// Text in `fg` on `bg`. Without colour, `emphasis` carries the meaning.
    pub fn on(&self, fg: &ColorToken, bg: &ColorToken, emphasis: Modifier) -> Style {
        match (self.color(fg), self.color(bg)) {
            (Some(f), Some(b)) => Style::default().fg(f).bg(b).add_modifier(emphasis),
            _ => Style::default().add_modifier(emphasis),
        }
    }

    /// Plain text on panel-fill.
    pub fn text(&self) -> Style {
        self.on(&TEXT_PRIMARY, &PANEL_FILL, Modifier::empty())
    }

    pub fn secondary(&self) -> Style {
        self.on(&TEXT_SECONDARY, &PANEL_FILL, Modifier::empty())
    }

    pub fn panel(&self) -> Style {
        self.on(&TEXT_PRIMARY, &PANEL_FILL, Modifier::empty())
    }

    pub fn border(&self) -> Style {
        self.on(&PANEL_BORDER, &PANEL_FILL, Modifier::empty())
    }

    pub fn background(&self) -> Style {
        self.on(&TEXT_PRIMARY, &BACKGROUND, Modifier::empty())
    }

    pub fn good(&self) -> Style {
        self.on(&STATUS_OK, &PANEL_FILL, Modifier::BOLD)
    }

    pub fn warning(&self) -> Style {
        self.on(&WARNING, &PANEL_FILL, Modifier::BOLD)
    }

    pub fn fault(&self) -> Style {
        self.on(&TX_TEXT, &PANEL_FILL, Modifier::BOLD)
    }

    pub fn locked(&self) -> Style {
        self.on(&STATUS_LOCKED, &PANEL_FILL, Modifier::BOLD)
    }

    /// White bold on tx-fill: the TX pill and the Transmitting verdict.
    /// Without colour: bold reverse video (brief section 8).
    pub fn tx_pill(&self) -> Style {
        if self.depth == Depth::None {
            Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else {
            self.on(&TEXT_PRIMARY, &TX_FILL, Modifier::BOLD)
        }
    }

    /// The selected tab: Standard accent on accent-soft with an accent
    /// underline; Night Ops bold text-primary with an accent underline and no
    /// fill. A terminal that cannot colour underlines shows the underline in
    /// the text colour, the accepted fallback; one that misreads the sequence
    /// gets no underline colour at all. Without colour: bold reverse video,
    /// no underline.
    pub fn selected_tab(&self) -> Style {
        if self.depth == Depth::None {
            return Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED);
        }
        let base = match self.name {
            ThemeName::Standard => {
                self.on(&ACCENT, &ACCENT_SOFT, Modifier::UNDERLINED | Modifier::BOLD)
            }
            ThemeName::NightOps => self.on(
                &TEXT_PRIMARY,
                &PANEL_FILL,
                Modifier::UNDERLINED | Modifier::BOLD,
            ),
        };
        match self.color(&ACCENT) {
            Some(c) if self.underline_colour => base.underline_color(c),
            _ => base,
        }
    }

    pub fn meter_fill(&self) -> Style {
        self.on(&STATUS_OK, &TILE_FILL, Modifier::empty())
    }

    pub fn meter_track(&self) -> Style {
        self.on(&TEXT_SECONDARY, &TILE_FILL, Modifier::empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(dead_code)]
    mod source {
        include!(concat!(env!("CARGO_MANIFEST_DIR"), "/build/tokens.rs"));
    }

    fn tokens_json() -> serde_json::Value {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../design/vector-tokens.json");
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn rgb(c: Rgb) -> source::Rgb8 {
        source::Rgb8(c.0, c.1, c.2)
    }

    /// The generated module matches `design/vector-tokens.json`, token for token.
    #[test]
    fn generated_theme_matches_the_vendored_tokens() {
        let json = tokens_json();
        let expected = source::resolve_all(&json);
        assert!(!expected.is_empty());
        assert_eq!(TOKENS_VERSION, json["version"].as_u64().unwrap());
        assert_eq!(TOKENS_SYNCED, json["meta"]["synced"].as_str().unwrap());
        assert_eq!(ALL.len(), expected.len());
        for (gen, want) in ALL.iter().zip(expected.iter()) {
            assert_eq!(gen.name, want.name);
            assert_eq!(rgb(gen.standard), want.standard, "{}", gen.name);
            assert_eq!(rgb(gen.night_ops), want.night_ops, "{}", gen.name);
            assert_eq!(gen.standard_256, want.standard_256, "{}", gen.name);
            assert_eq!(gen.night_ops_256, want.night_ops_256, "{}", gen.name);
        }
        // Spot checks straight from the JSON text.
        let panel = json["color"]["tokens"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "panel-fill")
            .unwrap();
        assert_eq!(
            Some(rgb(PANEL_FILL.standard)),
            source::parse_hex(panel["value"]["standard"].as_str().unwrap())
        );
        assert_eq!(STATUS_OK.standard, RX.standard, "status-ok is {{rx}}");
        assert_eq!(STATUS_LOCKED.night_ops, TEXT_SECONDARY.night_ops);
    }

    /// Every text pair passes 4.5:1 in 24-bit and in the 256-colour fallback.
    #[test]
    fn every_text_pair_passes_4_5_to_1_at_both_depths() {
        let by_name = |n: &str| ALL.iter().find(|t| t.name == n).copied().unwrap();
        for (fg, bg, std, night) in source::TEXT_PAIRS {
            let (f, b) = (by_name(fg), by_name(bg));
            if *std {
                let c = source::contrast(rgb(f.standard), rgb(b.standard));
                assert!(c >= 4.5, "{fg} on {bg} Standard 24-bit: {c:.2}");
                let c =
                    source::contrast(source::xterm(f.standard_256), source::xterm(b.standard_256));
                assert!(c >= 4.5, "{fg} on {bg} Standard 256: {c:.2}");
            }
            if *night {
                let c = source::contrast(rgb(f.night_ops), rgb(b.night_ops));
                assert!(c >= 4.5, "{fg} on {bg} Night Ops 24-bit: {c:.2}");
                let c = source::contrast(
                    source::xterm(f.night_ops_256),
                    source::xterm(b.night_ops_256),
                );
                assert!(c >= 4.5, "{fg} on {bg} Night Ops 256: {c:.2}");
            }
        }
    }

    #[test]
    fn depth_follows_no_color_colorterm_and_term() {
        let env = |vars: &[(&str, &str)]| {
            let vars: Vec<(String, String)> = vars
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            move |k: &str| vars.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
        };
        assert_eq!(
            Depth::detect(&env(&[("COLORTERM", "truecolor")])),
            Depth::TrueColor
        );
        assert_eq!(
            Depth::detect(&env(&[("COLORTERM", "24bit"), ("NO_COLOR", "1")])),
            Depth::None
        );
        assert_eq!(
            Depth::detect(&env(&[("TERM", "xterm-256color")])),
            Depth::Indexed256
        );
        assert_eq!(Depth::detect(&env(&[("TERM", "xterm")])), Depth::None);
        assert_eq!(Depth::detect(&env(&[("TERM", "dumb")])), Depth::None);
        let none = Theme::new(ThemeName::Standard, Depth::None);
        assert_eq!(none.color(&ACCENT), None);
        assert_eq!(none.good().fg, None);
        assert!(none.good().add_modifier.contains(Modifier::BOLD));
        assert!(none.tx_pill().add_modifier.contains(Modifier::REVERSED));
        assert_eq!(none.tx_pill().bg, None);
        assert!(none
            .selected_tab()
            .add_modifier
            .contains(Modifier::REVERSED));
        assert!(!none
            .selected_tab()
            .add_modifier
            .contains(Modifier::UNDERLINED));
        let night = Theme::new(ThemeName::NightOps, Depth::TrueColor);
        assert_eq!(night.selected_tab().underline_color, night.color(&ACCENT));
        assert_eq!(night.selected_tab().fg, night.color(&TEXT_PRIMARY));
        assert_eq!(night.selected_tab().bg, night.color(&PANEL_FILL));
        assert!(night
            .selected_tab()
            .add_modifier
            .contains(Modifier::UNDERLINED));
        let std = Theme::new(ThemeName::Standard, Depth::Indexed256);
        assert_eq!(std.selected_tab().bg, std.color(&ACCENT_SOFT));
        assert_eq!(std.selected_tab().underline_color, std.color(&ACCENT));
        assert!(!std.tx_pill().add_modifier.contains(Modifier::REVERSED));
    }

    /// Terminal.app gets the underline with no colour sequence; others keep it.
    #[test]
    fn terminal_app_selected_tab_has_no_underline_colour() {
        let program = |p: Option<&'static str>| {
            move |k: &str| {
                (k == "TERM_PROGRAM")
                    .then_some(p)
                    .flatten()
                    .map(str::to_string)
            }
        };
        assert!(!underline_colour_supported(&program(Some(
            "Apple_Terminal"
        ))));
        assert!(underline_colour_supported(&program(Some("iTerm.app"))));
        assert!(underline_colour_supported(&program(None)));
        for name in [ThemeName::Standard, ThemeName::NightOps] {
            for depth in [Depth::TrueColor, Depth::Indexed256] {
                let theme = Theme {
                    underline_colour: false,
                    ..Theme::new(name, depth)
                };
                let tab = theme.selected_tab();
                assert_eq!(tab.underline_color, None, "{name:?} {depth:?}");
                assert!(tab.add_modifier.contains(Modifier::UNDERLINED));
                assert!(Theme::new(name, depth)
                    .selected_tab()
                    .underline_color
                    .is_some());
            }
        }
    }

    /// At 256 colours, Night Ops' accent and the TX colour are a true red
    /// (index 160), not the orange 166 that is nearest by distance.
    #[test]
    fn accent_and_tx_are_red_at_256_colours() {
        assert_eq!(ACCENT.night_ops_256, 160);
        assert_eq!(TX.standard_256, 160);
        assert_eq!(TX.night_ops_256, 160);
        let source::Rgb8(r, g, b) = source::xterm(160);
        assert!(r > 150 && g == 0 && b == 0);
        // Standard's accent is green and keeps its contrast-checked index.
        assert_ne!(ACCENT.standard_256, 160);
    }
}
