//! Prompt mode: our line editor (crate `aish-editor`) driven on top of the
//! bash PTY. `ctl` is the I/O controller, `scan` follows the output stream.

pub mod ctl;
pub mod overlay;
pub mod resolve;
pub mod scan;
