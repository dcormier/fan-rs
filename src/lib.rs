#![deny(warnings)]

//! A crate to simplify fanning-out (and eventually -in) with channels.

#[cfg(feature = "tokio")]
pub mod tokio;
