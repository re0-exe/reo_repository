//! Host-side pipeline: device discovery, transport, virtual display, capture
//! and encode. Decision logic lives in plain functions that build and test on
//! any platform; Windows API calls are confined to `#[cfg(windows)]` code.

pub mod adb;
pub mod display;
pub mod gate;
pub mod transport;
