# Vector design system, vendored

`vector-tokens.json` is a copy of the Vector design system's `tokens.json`, the one source for every colour the terminal UI uses. The Rust theme module is generated from it at build time (`crates/vectorcable/build.rs`), one constant per colour token, for the Standard and Night Ops themes. No colour is written by hand in `crates/vectorcable/src`.

| Field | Value |
|---|---|
| Design system version | 1791246508-04c7 |
| Sync date | 2026-10-05 |
| Tokens file version | 2 (synced upstream 2026-09-23) |

## How the terminal uses the tokens

- 24-bit colour when `COLORTERM` is `truecolor` or `24bit`: the token's RGB as given. Translucent tokens (`rgba`) are composited over `panel-fill` of the same theme.
- 256-colour terminals: the nearest xterm-256 index, except that every foreground used as text is moved to the nearest index that still passes 4.5:1 against every background it is paired with. A test checks every pair.
- `NO_COLOR` set, or a terminal with 16 colours or fewer: no colour at all; bold and reverse video carry the meaning.

## Departures from the Vector README, for the design-system owner

Shared panel borders at 80 x 24, the Listen Only verdict and its ○ glyph, a terminal row in the platform mapping, and the wordmark with no glyph. Tracked in `DOCS/STATUS.md`, Follow-ups, "Design system".
