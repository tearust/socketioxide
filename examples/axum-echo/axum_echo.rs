use axum::routing::get;
use serde_json::Value;
use socketioxide::{
    adapter::LocalAdapter,
    extract::{Data, SocketRef},
    handler::message::GeneralMessage,
    SocketIo,
};
use tokio::sync::mpsc::{channel, Receiver};
use tracing::info;
use tracing_subscriber::FmtSubscriber;

fn on_connect(socket: SocketRef, Data(data): Data<Value>) {
    info!(
        "=== Custom Socket.IO connected: {:?} {:?}",
        socket.ns(),
        socket.id
    );
    socket.emit("auth", data).ok();

    socket.on_disconnect(|s: SocketRef| {
        info!("=== Custom Socket.IO disconnected: {} {}", s.id, s.ns());
    });
}

async fn relay_loop(mut receiver: Receiver<GeneralMessage<LocalAdapter>>) {
    while let Some(msg) = receiver.recv().await {
        info!(
            "Relaying message, path: {}, key {}, value: {:?}, bin params: {:?}, ack id: {:?}",
            msg.path, msg.key, msg.value, msg.params, msg.ack_id
        );
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing::subscriber::set_global_default(FmtSubscriber::default())?;

    let (sender, receiver) = channel(10);
    let (layer, io) = SocketIo::new_layer(sender);
    tokio::spawn(relay_loop(receiver));

    io.ns("/", on_connect);

    let app = axum::Router::new()
        .route("/", get(|| async { "Hello, World!" }))
        .layer(layer);

    info!("Starting server");

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    axum::serve(listener, app).await.unwrap();

    Ok(())
}
