# Third-party notices

VectorCable is GPL-3.0-only (`LICENSE`). It ships with the following third-party work.

## macOS audio driver

`VectorBridgeRX.driver` and `VectorBridgeTX.driver` are built from
[BlackHole](https://github.com/ExistentialAudio/BlackHole) v0.7.1
(commit `62953f570329839e6188f02cc28934904147f617`), GPL-3.0, by Existential Audio Inc.
The fork's only changes are the device names, a `kLatency_Frame_Size` guard and a
deployment-target bump (`macos/driver/`). Each plug-in carries BlackHole's LICENSE in its Resources.

## Rust crates linked into the macOS `vectorcable` binary

Generated with `cargo tree -p vectorcable --target aarch64-apple-darwin -e normal --prefix none --format '{p} | {l}'`
on 2026-10-27. Every crate is under a permissive licence compatible with GPL-3.0.

| Crate | Version | Licence |
|---|---|---|
| allocator-api2 | v0.2.21 | MIT OR Apache-2.0 |
| bitflags | v2.13.2 | MIT OR Apache-2.0 |
| castaway | v0.2.4 | MIT |
| cfg-if | v1.0.5 | MIT OR Apache-2.0 |
| compact_str | v0.9.1 | MIT |
| convert_case | v0.10.0 | MIT |
| critical-section | v1.2.0 | MIT OR Apache-2.0 |
| crossterm | v0.29.0 | MIT |
| darling | v0.24.1 | MIT |
| darling_core | v0.24.1 | MIT |
| darling_macro | v0.24.1 | MIT |
| deranged | v0.5.8 | MIT OR Apache-2.0 |
| derive_more | v2.1.1 | MIT |
| derive_more-impl | v2.1.1 | MIT |
| document-features | v0.2.12 | MIT OR Apache-2.0 |
| either | v1.18.0 | MIT OR Apache-2.0 |
| equivalent | v1.0.2 | Apache-2.0 OR MIT |
| errno | v0.3.14 | MIT OR Apache-2.0 |
| foldhash | v0.2.0 | Zlib |
| hashbrown | v0.16.1 | MIT OR Apache-2.0 |
| hashbrown | v0.17.1 | MIT OR Apache-2.0 |
| heck | v0.5.0 | MIT OR Apache-2.0 |
| ident_case | v1.0.1 | MIT/Apache-2.0 |
| indoc | v2.0.7 | MIT OR Apache-2.0 |
| instability | v0.3.14 | see crate |
| instability | v0.3.14 | MIT |
| itertools | v0.14.0 | MIT OR Apache-2.0 |
| itoa | v1.0.18 | MIT OR Apache-2.0 |
| kasuari | v0.4.12 | MIT OR Apache-2.0 |
| libc | v0.2.190 | MIT OR Apache-2.0 |
| line-clipping | v0.3.8 | MIT OR Apache-2.0 |
| litrs | v1.0.0 | MIT OR Apache-2.0 |
| lock_api | v0.4.14 | MIT OR Apache-2.0 |
| log | v0.4.34 | MIT OR Apache-2.0 |
| lru | v0.18.5 | MIT |
| memchr | v2.8.3 | Unlicense OR MIT |
| mio | v1.2.4 | MIT |
| num_threads | v0.1.7 | MIT OR Apache-2.0 |
| num-conv | v0.2.2 | MIT OR Apache-2.0 |
| parking_lot | v0.12.5 | MIT OR Apache-2.0 |
| parking_lot_core | v0.9.12 | MIT OR Apache-2.0 |
| powerfmt | v0.2.1 | MIT OR Apache-2.0 |
| proc-macro2 | v1.0.107 | MIT OR Apache-2.0 |
| quote | v1.0.47 | MIT OR Apache-2.0 |
| ratatui | v0.30.2 | MIT |
| ratatui-core | v0.1.2 | MIT |
| ratatui-crossterm | v0.1.2 | MIT |
| ratatui-macros | v0.7.2 | MIT |
| ratatui-widgets | v0.3.2 | MIT |
| rustix | v1.1.5 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| rustversion | v1.0.23 | MIT OR Apache-2.0 |
| ryu | v1.0.23 | Apache-2.0 OR BSL-1.0 |
| scopeguard | v1.2.0 | MIT OR Apache-2.0 |
| serde | v1.0.229 | MIT OR Apache-2.0 |
| serde_core | v1.0.229 | MIT OR Apache-2.0 |
| serde_derive | v1.0.229 | MIT OR Apache-2.0 |
| serde_json | v1.0.151 | MIT OR Apache-2.0 |
| signal-hook | v0.3.18 | Apache-2.0/MIT |
| signal-hook-mio | v0.2.5 | MIT OR Apache-2.0 |
| signal-hook-registry | v1.4.8 | MIT OR Apache-2.0 |
| smallvec | v1.16.2 | MIT OR Apache-2.0 |
| static_assertions | v1.1.0 | MIT OR Apache-2.0 |
| strsim | v0.11.1 | MIT |
| strum | v0.28.0 | MIT |
| strum_macros | v0.28.0 | MIT |
| syn | v2.0.119 | MIT OR Apache-2.0 |
| syn | v3.0.6 | MIT OR Apache-2.0 |
| thiserror | v2.0.21 | MIT OR Apache-2.0 |
| thiserror-impl | v2.0.21 | MIT OR Apache-2.0 |
| time | v0.3.55 | MIT OR Apache-2.0 |
| time-core | v0.1.9 | MIT OR Apache-2.0 |
| unicode-ident | v1.0.26 | (MIT OR Apache-2.0) AND Unicode-3.0 |
| unicode-segmentation | v1.13.3 | MIT OR Apache-2.0 |
| unicode-truncate | v2.0.1 | MIT OR Apache-2.0 |
| unicode-width | v0.2.2 | MIT OR Apache-2.0 |
| zmij | v1.0.23 | MIT |

`unicode-ident` is `(MIT OR Apache-2.0) AND Unicode-3.0`; `instability` is MIT (its manifest omits the field).

## Design tokens

`design/vector-tokens.json` is a copy of the Vector design system's token file (see `design/README.md`).
