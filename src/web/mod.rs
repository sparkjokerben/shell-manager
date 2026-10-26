//! Browser-facing bridge to the smd daemon.
//!
//! The HTTP and auth layers are added in a later stage; these modules already
//! hold the control-pipe client, the data-pipe attachment and the embedded
//! frontend assets.

#![allow(dead_code)]

pub mod bridge;
pub mod static_assets;
