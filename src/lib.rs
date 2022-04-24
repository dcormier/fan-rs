#![deny(warnings)]

//! Simplifies fanning-out (and eventually -in) with channels

#[cfg(feature = "tokio")]
pub mod tokio;
