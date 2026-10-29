//! `vbcore`: the VectorCable core.
//!
//! Everything that touches the VectorBridge wire protocol, RX timing, the TX
//! silence gate and the session safety rules lives here. The logic modules take
//! time and packets as arguments and do no I/O, so every rule is unit-testable;
//! the thin I/O modules (`http`, `discovery`, `runner`, `clockcheck::listener`)
//! sit on top of them.
//!
//! Wire contract: `vector-bridge/DOCS/protocol.md`, version 1.

pub mod assembler;
pub mod audio;
pub mod clockcheck;
pub mod discovery;
pub mod http;
pub mod pin;
pub mod playback;
pub mod protocol;
pub mod rigrelay;
pub mod runner;
pub mod session;
pub mod status;
pub mod time;
pub mod tx;
