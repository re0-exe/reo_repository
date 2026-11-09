//! Host-side pipeline: device discovery, transport, virtual display, capture
//! and encode. Decision logic lives in plain functions that build and test on
//! any platform; Windows API calls are confined to `#[cfg(windows)]` code.

pub mod adb;
pub mod capture;
pub mod cursor;
pub mod display;
pub mod encode;
pub mod gate;
pub mod session;
pub mod shutdown;
pub mod transport;
