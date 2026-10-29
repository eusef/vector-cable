// Reads `design/vector-tokens.json` and resolves every colour token for the
// Standard and Night Ops themes, in 24-bit and as an xterm-256 index. Shared
// by `build.rs` (which writes the theme module) and by the theme test (which
// checks the generated module against the JSON). No colour is written here.

use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb8(pub u8, pub u8, pub u8);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThemeId {
    Standard,
    NightOps,
}

impl ThemeId {
    pub fn key(self) -> &'static str {
        match self {
            ThemeId::Standard => "standard",
            ThemeId::NightOps => "night-ops",
        }
    }
}

/// A resolved token: both themes, both depths.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub name: String,
    pub standard: Rgb8,
    pub night_ops: Rgb8,
    pub standard_256: u8,
    pub night_ops_256: u8,
}

/// Text pairs the terminal UI draws: (foreground, background, Standard?, Night Ops?).
/// Each must pass 4.5:1 at every depth that has colour.
pub const TEXT_PAIRS: &[(&str, &str, bool, bool)] = &[
    ("text-primary", "panel-fill", true, true),
    ("text-secondary", "panel-fill", true, true),
    ("text-primary", "background", true, true),
    ("text-secondary", "background", true, true),
    ("status-ok", "panel-fill", true, true),
    ("status-locked", "panel-fill", true, true),
    ("warning", "panel-fill", true, true),
    ("tx-text", "panel-fill", true, true),
    ("text-primary", "tx-fill", true, true),
    ("text-primary", "tile-fill", true, true),
    // The selected tab: accent text on accent-soft in Standard only; Night Ops
    // draws it as bold text-primary with an accent underline.
    ("accent", "accent-soft", true, false),
];

pub const MIN_CONTRAST: f64 = 4.5;

/// xterm-256 indexes picked by hand instead of the nearest: (token, theme, index).
/// The nearest to the Vector red is 166, an orange, so Night Ops' accent and
/// the TX colour use 160, a true red (the maintainer, 2026-10-07). Neither is a text pair.
pub const INDEX_OVERRIDES: &[(&str, ThemeId, u8)] = &[
    ("accent", ThemeId::NightOps, 160),
    ("tx", ThemeId::Standard, 160),
    ("tx", ThemeId::NightOps, 160),
];

pub fn parse_hex(s: &str) -> Option<Rgb8> {
    let h = s.strip_prefix('#')?;
    if h.len() != 6 {
        return None;
    }
    let v = u32::from_str_radix(h, 16).ok()?;
    Some(Rgb8((v >> 16) as u8, (v >> 8) as u8, v as u8))
}

/// `rgba(r, g, b, a)` composited over `under`.
fn parse_rgba_over(s: &str, under: Rgb8) -> Option<Rgb8> {
    let inner = s.strip_prefix("rgba(")?.strip_suffix(')')?;
    let parts: Vec<f64> = inner
        .split(',')
        .map(|p| p.trim().parse::<f64>().ok())
        .collect::<Option<Vec<f64>>>()?;
    if parts.len() != 4 {
        return None;
    }
    let a = parts[3].clamp(0.0, 1.0);
    let mix = |c: f64, u: u8| (c * a + u as f64 * (1.0 - a)).round().clamp(0.0, 255.0) as u8;
    Some(Rgb8(
        mix(parts[0], under.0),
        mix(parts[1], under.1),
        mix(parts[2], under.2),
    ))
}

fn token_value<'a>(tokens: &'a [Value], name: &str) -> Option<&'a Value> {
    tokens
        .iter()
        .find(|t| t["name"].as_str() == Some(name))
        .map(|t| &t["value"])
}

/// Resolves `name` for `theme`, following `{ref}` values and compositing
/// translucent ones over `panel-fill`.
pub fn resolve(tokens: &[Value], name: &str, theme: ThemeId, depth: usize) -> Option<Rgb8> {
    if depth > 8 {
        return None;
    }
    let value = token_value(tokens, name)?;
    let raw = match value {
        Value::String(s) => s.as_str(),
        Value::Object(m) => m.get(theme.key())?.as_str()?,
        _ => return None,
    };
    if let Some(r) = raw.strip_prefix('{').and_then(|r| r.strip_suffix('}')) {
        return resolve(tokens, r, theme, depth + 1);
    }
    if raw.starts_with('#') {
        return parse_hex(raw);
    }
    if raw.starts_with("rgba(") {
        let under = if name == "panel-fill" {
            Rgb8(0, 0, 0)
        } else {
            resolve(tokens, "panel-fill", theme, depth + 1)?
        };
        return parse_rgba_over(raw, under);
    }
    None
}

fn channel(c: u8) -> f64 {
    let s = c as f64 / 255.0;
    if s <= 0.03928 {
        s / 12.92
    } else {
        ((s + 0.055) / 1.055).powf(2.4)
    }
}

/// WCAG relative luminance.
pub fn luminance(c: Rgb8) -> f64 {
    0.2126 * channel(c.0) + 0.7152 * channel(c.1) + 0.0722 * channel(c.2)
}

/// WCAG contrast ratio, at least 1.0.
pub fn contrast(a: Rgb8, b: Rgb8) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// The xterm-256 colour of index 16 to 255 (the 16 system colours vary by
/// terminal, so they are never chosen).
pub fn xterm(index: u8) -> Rgb8 {
    if index < 16 {
        return Rgb8(0, 0, 0);
    }
    if index >= 232 {
        let g = 8 + 10 * (index - 232);
        return Rgb8(g, g, g);
    }
    let i = index - 16;
    let step = |n: u8| if n == 0 { 0 } else { 55 + 40 * n };
    Rgb8(step(i / 36), step((i / 6) % 6), step(i % 6))
}

fn distance(a: Rgb8, b: Rgb8) -> f64 {
    let dr = a.0 as f64 - b.0 as f64;
    let dg = a.1 as f64 - b.1 as f64;
    let db = a.2 as f64 - b.2 as f64;
    // Weighted for the eye's green bias.
    2.0 * dr * dr + 4.0 * dg * dg + 3.0 * db * db
}

/// Nearest index among 16 to 255 that passes [`MIN_CONTRAST`] against every
/// background in `against`; with none to pass, the plain nearest.
pub fn nearest_index(c: Rgb8, against: &[Rgb8]) -> u8 {
    let mut best: Option<(u8, f64)> = None;
    let mut fallback: Option<(u8, f64)> = None;
    for i in 16..=255u8 {
        let x = xterm(i);
        let d = distance(c, x);
        if fallback.is_none_or(|(_, bd)| d < bd) {
            fallback = Some((i, d));
        }
        if against.iter().all(|bg| contrast(x, *bg) >= MIN_CONTRAST)
            && best.is_none_or(|(_, bd)| d < bd)
        {
            best = Some((i, d));
        }
    }
    best.or(fallback).map(|(i, _)| i).unwrap_or(16)
}

/// Every colour token, resolved for both themes and both depths.
pub fn resolve_all(json: &Value) -> Vec<Resolved> {
    let tokens = json["color"]["tokens"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let names: Vec<String> = tokens
        .iter()
        .filter_map(|t| t["name"].as_str().map(str::to_string))
        .collect();
    let rgb = |name: &str, theme: ThemeId| resolve(&tokens, name, theme, 0);
    // Backgrounds first: their index is the plain nearest.
    let bg_index = |name: &str, theme: ThemeId| -> Option<u8> {
        rgb(name, theme).map(|c| nearest_index(c, &[]))
    };
    let mut out = Vec::new();
    for name in &names {
        let (Some(standard), Some(night_ops)) =
            (rgb(name, ThemeId::Standard), rgb(name, ThemeId::NightOps))
        else {
            continue;
        };
        let index_for = |theme: ThemeId, c: Rgb8| -> u8 {
            if let Some((_, _, i)) = INDEX_OVERRIDES
                .iter()
                .find(|(n, t, _)| *n == name.as_str() && *t == theme)
            {
                return *i;
            }
            let paired: Vec<Rgb8> = TEXT_PAIRS
                .iter()
                .filter(|(fg, _, std, night)| {
                    fg == name
                        && match theme {
                            ThemeId::Standard => *std,
                            ThemeId::NightOps => *night,
                        }
                })
                .filter_map(|(_, bg, _, _)| bg_index(bg, theme).map(xterm))
                .collect();
            nearest_index(c, &paired)
        };
        out.push(Resolved {
            name: name.clone(),
            standard,
            night_ops,
            standard_256: index_for(ThemeId::Standard, standard),
            night_ops_256: index_for(ThemeId::NightOps, night_ops),
        });
    }
    out
}

/// `TOKEN_NAME` for a `token-name`.
pub fn const_name(name: &str) -> String {
    name.to_ascii_uppercase().replace('-', "_")
}
