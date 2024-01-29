//! ## A tower [`Layer`] for socket.io so it can be used as a middleware with frameworks supporting layers.
//!
//! #### Example with axum :
//! ```rust
//! # use socketioxide::SocketIo;
//! # use axum::routing::get;
//! // Create a socket.io layer
//! let (layer, io) = SocketIo::new_layer(None);
//!
//! // Add io namespaces and events...
//!
//! let app = axum::Router::<()>::new()
//!     .route("/", get(|| async { "Hello, World!" }))
//!     .layer(layer);
//!
//! // Spawn axum server
//!
//! ```
//!
//! #### Example with salvo :
//! ```no_run
//! # use salvo::prelude::*;
//! # use socketioxide::SocketIo;
//!
//! #[handler]
//! async fn hello() -> &'static str {
//!     "Hello World"
//! }
//!  // Create a socket.io layer
//! let (layer, io) = SocketIo::new_layer(None);
//!
//! // Add io namespaces and events...
//!
//! let layer = layer.compat();
//! let router = Router::with_path("/socket.io").hoop(layer).goal(hello);
//! // Spawn salvo server
//! ```
use std::sync::Arc;

use tower::Layer;

use crate::{
    adapter::LocalAdapter, client::Client, handler::message::MessageSender,
    service::SocketIoService, SocketIoConfig,
};

/// A [`Layer`] for [`SocketIoService`], acting as a middleware.
pub struct SocketIoLayer {
    client: Arc<Client>,
}

impl Clone for SocketIoLayer {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
        }
    }
}

impl SocketIoLayer {
    pub(crate) fn from_config(
        config: Arc<SocketIoConfig>,
        message_sender: MessageSender<LocalAdapter>,
    ) -> (Self, Arc<Client>) {
        let client = Arc::new(Client::new(config.clone(), message_sender));
        let layer = Self {
            client: client.clone(),
        };
        (layer, client)
    }
}

impl<S: Clone> Layer<S> for SocketIoLayer {
    type Service = SocketIoService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        SocketIoService::with_client(inner, self.client.clone())
    }
}
