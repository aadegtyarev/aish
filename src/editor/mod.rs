//! Prompt mode: our line editor (crate `lineedit-wip`) driven on top of the
//! bash PTY. `ctl` is the I/O controller, `scan` follows the output stream.

pub mod ctl;
pub mod resolve;
pub mod scan;
