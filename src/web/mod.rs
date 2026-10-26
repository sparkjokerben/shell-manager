//! Browser-facing bridge to the smd daemon.
//!
//! These modules hold the control-pipe client, the data-pipe attachment, the
//! embedded frontend assets, token authentication and the HTTP/WebSocket
//! surface served by `smw`.

#![allow(dead_code)]

use std::sync::Arc;

use bridge::{AttachRegistry, ControlClient};

pub mod auth;
pub mod bridge;
pub mod http;
pub mod static_assets;

/// State shared by every HTTP and WebSocket handler.
#[derive(Clone)]
pub struct AppState {
    pub control: Arc<ControlClient>,
    pub registry: AttachRegistry,
    pub token: Option<Arc<String>>,
    pub instance: String,
}
