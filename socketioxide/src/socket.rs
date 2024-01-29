//! A [`Socket`] represents a client connected to a namespace.
//! The socket struct itself should not be used directly, but through a [`SocketRef`](crate::extract::SocketRef).
use std::{
    borrow::Cow,
    collections::HashMap,
    fmt::Debug,
    sync::Mutex,
    sync::{
        atomic::{AtomicI64, Ordering},
        Arc,
    },
    time::Duration,
};

use engineioxide::{sid::Sid, socket::DisconnectReason as EIoDisconnectReason};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;
use tokio::sync::oneshot;

#[cfg(feature = "extensions")]
use crate::extensions::Extensions;

use crate::{
    adapter::{Adapter, LocalAdapter, Room},
    errors::{AckError, Error},
    handler::{
        message::{GeneralMessage, MessageSender},
        BoxedDisconnectHandler, DisconnectHandler, MakeErasedHandler,
    },
    ns::Namespace,
    operators::{Operators, RoomParam},
    packet::{BinaryPacket, Packet, PacketData},
    SocketIoConfig,
};
use crate::{
    client::SocketData,
    errors::{AdapterError, SendError},
};

/// All the possible reasons for a [`Socket`] to be disconnected from a namespace.
///
/// It can be used as an extractor in the [`on_disconnect`](crate::handler::disconnect) handler.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum DisconnectReason {
    /// The client gracefully closed the connection
    TransportClose,

    /// The client sent multiple polling requests at the same time (it is forbidden according to the engine.io protocol)
    MultipleHttpPollingError,

    /// The client sent a bad request / the packet could not be parsed correctly
    PacketParsingError,

    /// The connection was closed (example: the user has lost connection, or the network was changed from WiFi to 4G)
    TransportError,

    /// The client did not send a PONG packet in the `ping timeout` delay
    HeartbeatTimeout,

    /// The client has manually disconnected the socket using [`socket.disconnect()`](https://socket.io/fr/docs/v4/client-api/#socketdisconnect)
    ClientNSDisconnect,

    /// The socket was forcefully disconnected from the namespace with [`Socket::disconnect`]
    ServerNSDisconnect,

    /// The server is being closed
    ClosingServer,

    /// Relay message through channel failed
    RelayError,
}

impl std::fmt::Display for DisconnectReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use DisconnectReason::*;
        let str: &'static str = match self {
            TransportClose => "client gracefully closed the connection",
            MultipleHttpPollingError => "client sent multiple polling requests at the same time",
            PacketParsingError => "client sent a bad request / the packet could not be parsed",
            TransportError => "The connection was abruptly closed",
            HeartbeatTimeout => "client did not send a PONG packet in time",
            ClientNSDisconnect => "client has manually disconnected the socket from the namespace",
            ServerNSDisconnect => "socket was forcefully disconnected from the namespace",
            ClosingServer => "server is being closed",
            RelayError => "relay message through channel failed",
        };
        f.write_str(str)
    }
}

impl From<EIoDisconnectReason> for DisconnectReason {
    fn from(reason: EIoDisconnectReason) -> Self {
        use DisconnectReason::*;
        match reason {
            EIoDisconnectReason::TransportClose => TransportClose,
            EIoDisconnectReason::TransportError => TransportError,
            EIoDisconnectReason::HeartbeatTimeout => HeartbeatTimeout,
            EIoDisconnectReason::MultipleHttpPollingError => MultipleHttpPollingError,
            EIoDisconnectReason::PacketParsingError => PacketParsingError,
            EIoDisconnectReason::ClosingServer => ClosingServer,
            EIoDisconnectReason::RelayError => RelayError,
        }
    }
}
/// An acknowledgement sent by the client.
/// It contains the data sent by the client and the binary payloads if there are any.
#[derive(Debug)]
pub struct AckResponse<T> {
    /// The data returned by the client
    pub data: T,
    /// Optional binary payloads
    ///
    /// If there is no binary payload, the `Vec` will be empty
    pub binary: Vec<Vec<u8>>,
}

/// A Socket represents a client connected to a namespace.
/// It is used to send and receive messages from the client, join and leave rooms, etc.
/// The socket struct itself should not be used directly, but through a [`SocketRef`](crate::extract::SocketRef).
pub struct Socket<A: Adapter = LocalAdapter> {
    config: Arc<SocketIoConfig>,
    ns: Arc<Namespace<A>>,
    disconnect_handler: Mutex<Option<BoxedDisconnectHandler<A>>>,
    ack_message: Mutex<HashMap<i64, oneshot::Sender<AckResponse<Value>>>>,
    ack_counter: AtomicI64,
    /// The socket id
    pub id: Sid,

    /// A type map of protocol extensions.
    /// It can be used to share data through the lifetime of the socket.
    /// Because it uses a [`DashMap`](dashmap::DashMap) internally, it is thread safe but be careful about deadlocks!
    ///
    /// **Note**: This is note the same data than the `extensions` field on the [`http::Request::extensions()`](http::Request) struct.
    #[cfg_attr(docsrs, doc(cfg(feature = "extensions")))]
    #[cfg(feature = "extensions")]
    pub extensions: Extensions,
    esocket: Arc<engineioxide::Socket<SocketData>>,
    message_sender: Option<MessageSender<A>>,
}

impl<A: Adapter> Socket<A> {
    pub(crate) fn new(
        sid: Sid,
        ns: Arc<Namespace<A>>,
        esocket: Arc<engineioxide::Socket<SocketData>>,
        config: Arc<SocketIoConfig>,
        message_sender: Option<MessageSender<A>>,
    ) -> Self {
        Self {
            ns,
            disconnect_handler: Mutex::new(None),
            ack_message: Mutex::new(HashMap::new()),
            ack_counter: AtomicI64::new(0),
            id: sid,
            #[cfg(feature = "extensions")]
            extensions: Extensions::new(),
            config,
            esocket,
            message_sender,
        }
    }

    /// ## Registers a disconnect handler.
    /// You can register only one disconnect handler per socket. If you register multiple handlers, only the last one will be used.
    ///
    /// * See the [`disconnect`](crate::handler::disconnect) module doc for more details on disconnect handler.
    /// * See the [`extract`](crate::extract) module doc for more details on available extractors.
    ///
    /// The callback will be called when the socket is disconnected from the server or the client or when the underlying connection crashes.
    /// A [`DisconnectReason`] is passed to the callback to indicate the reason for the disconnection.
    pub fn on_disconnect<C, T>(&self, callback: C)
    where
        C: DisconnectHandler<A, T> + Send + Sync + 'static,
        T: Send + Sync + 'static,
    {
        let handler = MakeErasedHandler::new_disconnect_boxed(callback);
        self.disconnect_handler.lock().unwrap().replace(handler);
    }

    /// Emits a message to the client
    /// ## Errors
    /// * If the data cannot be serialized to JSON, a [`SendError::Serialize`] is returned.
    /// * If the packet buffer is full, a [`SendError::InternalChannelFull`] is returned.
    /// See [`SocketIoBuilder::max_buffer_size`](crate::SocketIoBuilder) option for more infos on internal buffer config
    pub fn emit(
        &self,
        event: impl Into<Cow<'static, str>>,
        data: impl Serialize,
    ) -> Result<(), SendError> {
        let ns = self.ns();
        let data = serde_json::to_value(data)?;
        if let Err(e) = self.send(Packet::event(ns, event.into(), data)) {
            #[cfg(feature = "tracing")]
            tracing::debug!("sending error during emit message: {e:?}");
            return Err(e);
        }
        Ok(())
    }

    /// Emits a message to the client and wait for acknowledgement.
    ///
    /// The acknowledgement has a timeout specified in the config (5s by default)
    /// (see [`SocketIoBuilder::ack_timeout`](crate::SocketIoBuilder)) or with the `timeout()` operator.
    ///
    /// ## Errors
    /// * If the data cannot be serialized to JSON, a [`AckError::Serialize`] is returned.
    /// * If the packet could not be sent, a [`AckError::SendChannel`] is returned.
    /// * In case of timeout an [`AckError::Timeout`] is returned.
    pub async fn emit_with_ack<V>(
        &self,
        event: impl Into<Cow<'static, str>>,
        data: impl Serialize,
    ) -> Result<AckResponse<V>, AckError>
    where
        V: DeserializeOwned + Send + Sync + 'static,
    {
        let ns = self.ns();
        let data = serde_json::to_value(data)?;
        let packet = Packet::event(Cow::Borrowed(ns), event.into(), data);

        self.send_with_ack(packet, None).await
    }

    // Room actions

    /// Joins the given rooms.
    ///
    /// If the room does not exist, it will be created.
    ///
    /// ## Errors
    /// When using a distributed adapter, it can return an [`Adapter::Error`] which is mostly related to network errors.
    /// For the default [`LocalAdapter`] it is always an [`Infallible`](std::convert::Infallible) error
    pub fn join(&self, rooms: impl RoomParam) -> Result<(), A::Error> {
        self.ns.adapter.add_all(self.id, rooms)
    }

    /// Leaves the given rooms.
    ///
    /// If the room does not exist, it will do nothing
    /// ## Errors
    /// When using a distributed adapter, it can return an [`Adapter::Error`] which is mostly related to network errors.
    /// For the default [`LocalAdapter`] it is always an [`Infallible`](std::convert::Infallible) error
    pub fn leave(&self, rooms: impl RoomParam) -> Result<(), A::Error> {
        self.ns.adapter.del(self.id, rooms)
    }

    /// Leaves all rooms where the socket is connected.
    /// ## Errors
    /// When using a distributed adapter, it can return an [`Adapter::Error`] which is mostly related to network errors.
    /// For the default [`LocalAdapter`] it is always an [`Infallible`](std::convert::Infallible) error
    pub fn leave_all(&self) -> Result<(), A::Error> {
        self.ns.adapter.del_all(self.id)
    }

    /// Gets all rooms where the socket is connected.
    /// ## Errors
    /// When using a distributed adapter, it can return an [`Adapter::Error`] which is mostly related to network errors.
    /// For the default [`LocalAdapter`] it is always an [`Infallible`](std::convert::Infallible) error
    pub fn rooms(&self) -> Result<Vec<Room>, A::Error> {
        self.ns.adapter.socket_rooms(self.id)
    }

    // Socket operators

    /// Selects all clients in the given rooms except the current socket.
    ///
    /// If you want to include the current socket, use the `within()` operator.
    pub fn to(&self, rooms: impl RoomParam) -> Operators<A> {
        Operators::new(self.ns.clone(), Some(self.id)).to(rooms)
    }

    /// Selects all clients in the given rooms.
    ///
    /// It does include the current socket contrary to the `to()` operator.
    pub fn within(&self, rooms: impl RoomParam) -> Operators<A> {
        Operators::new(self.ns.clone(), Some(self.id)).within(rooms)
    }

    /// Filters out all clients selected with the previous operators which are in the given rooms.
    pub fn except(&self, rooms: impl RoomParam) -> Operators<A> {
        Operators::new(self.ns.clone(), Some(self.id)).except(rooms)
    }

    /// Broadcasts to all clients only connected on this node (when using multiple nodes).
    /// When using the default in-memory [`LocalAdapter`], this operator is a no-op.
    pub fn local(&self) -> Operators<A> {
        Operators::new(self.ns.clone(), Some(self.id)).local()
    }

    /// Sets a custom timeout when sending a message with an acknowledgement.
    pub fn timeout(&self, timeout: Duration) -> Operators<A> {
        Operators::new(self.ns.clone(), Some(self.id)).timeout(timeout)
    }

    /// Adds a binary payload to the message.
    pub fn bin(&self, binary: Vec<Vec<u8>>) -> Operators<A> {
        Operators::new(self.ns.clone(), Some(self.id)).bin(binary)
    }

    /// Broadcasts to all clients without any filtering (except the current socket).
    pub fn broadcast(&self) -> Operators<A> {
        Operators::new(self.ns.clone(), Some(self.id)).broadcast()
    }

    /// Disconnects the socket from the current namespace,
    ///
    /// It will also call the disconnect handler if it is set.
    pub fn disconnect(self: Arc<Self>) -> Result<(), SendError> {
        self.send(Packet::disconnect(&self.ns.path))?;
        self.close(DisconnectReason::ServerNSDisconnect)?;
        Ok(())
    }

    /// Closes the engine.io connection if it is not already closed.
    /// Return a future that resolves when the underlying transport is closed.
    pub(crate) async fn close_underlying_transport(&self) {
        if !self.esocket.is_closed() {
            #[cfg(feature = "tracing")]
            tracing::debug!("closing underlying transport for socket: {}", self.id);
            self.esocket.close(EIoDisconnectReason::ClosingServer);
        }
        self.esocket.closed().await;
    }

    /// Gets the current namespace path.
    pub fn ns(&self) -> &str {
        &self.ns.path
    }

    pub(crate) fn send(&self, mut packet: Packet<'_>) -> Result<(), SendError> {
        let bin_payloads = match packet.inner {
            PacketData::BinaryEvent(_, ref mut bin, _) | PacketData::BinaryAck(ref mut bin, _) => {
                Some(std::mem::take(&mut bin.bin))
            }
            _ => None,
        };

        let msg = packet.try_into()?;
        self.esocket.emit(msg)?;
        if let Some(bin_payloads) = bin_payloads {
            for bin in bin_payloads {
                self.esocket.emit_binary(bin)?;
            }
        }

        Ok(())
    }

    pub(crate) async fn send_with_ack<'a, V: DeserializeOwned>(
        &self,
        mut packet: Packet<'a>,
        timeout: Option<Duration>,
    ) -> Result<AckResponse<V>, AckError> {
        let (tx, rx) = oneshot::channel();
        let ack = self.ack_counter.fetch_add(1, Ordering::SeqCst) + 1;
        self.ack_message.lock().unwrap().insert(ack, tx);
        packet.inner.set_ack_id(ack);
        self.send(packet)?;
        let timeout = timeout.unwrap_or(self.config.ack_timeout);
        let v = tokio::time::timeout(timeout, rx).await??;
        Ok(AckResponse {
            data: serde_json::from_value(v.data)?,
            binary: v.binary,
        })
    }

    /// Called when the socket is gracefully disconnected from the server or the client
    ///
    /// It maybe also close when the underlying transport is closed or failed.
    pub(crate) fn close(self: Arc<Self>, reason: DisconnectReason) -> Result<(), AdapterError> {
        if let Some(handler) = self.disconnect_handler.lock().unwrap().take() {
            handler.call(self.clone(), reason);
        }

        self.ns.remove_socket(self.id)?;
        Ok(())
    }

    // Receives data from client:
    pub(crate) fn recv(self: Arc<Self>, packet: PacketData<'_>) -> Result<(), Error> {
        match packet {
            PacketData::Event(e, data, ack) => self.recv_event(&e, data, ack),
            PacketData::EventAck(data, ack_id) => self.recv_ack(data, ack_id),
            PacketData::BinaryEvent(e, packet, ack) => self.recv_bin_event(&e, packet, ack),
            PacketData::BinaryAck(packet, ack) => self.recv_bin_ack(packet, ack),
            PacketData::Disconnect => self
                .close(DisconnectReason::ClientNSDisconnect)
                .map_err(Error::from),
            _ => unreachable!(),
        }
    }

    /// Gets the request info made by the client to connect
    ///
    /// Note that the `extensions` field will be empty and will not
    /// contain extensions set in the previous http layers for requests initialized with ws transport.
    ///
    /// It is because [`http::Extensions`] is not cloneable and is needed for ws upgrade.
    pub fn req_parts(&self) -> &http::request::Parts {
        &self.esocket.req_parts
    }

    /// Gets the [`TransportType`](crate::TransportType) used by the client to connect with this [`Socket`]
    ///
    /// It can also be accessed as an extractor:
    /// ```
    /// # use socketioxide::{SocketIo, TransportType, extract::*};
    ///
    /// let (_, io) = SocketIo::new_svc(None);
    /// io.ns("/", |socket: SocketRef, transport: TransportType| {
    ///     assert_eq!(socket.transport_type(), transport);
    /// });
    pub fn transport_type(&self) -> crate::TransportType {
        self.esocket.transport_type()
    }

    /// Gets the socket.io [`ProtocolVersion`](crate::ProtocolVersion) used by the client to connect with this [`Socket`]
    ///
    /// It can also be accessed as an extractor:
    /// ## Example
    /// ```
    /// # use socketioxide::{SocketIo, ProtocolVersion, extract::*};
    ///
    /// let (_, io) = SocketIo::new_svc(None);
    /// io.ns("/", |socket: SocketRef, v: ProtocolVersion| {
    ///     assert_eq!(socket.protocol(), v);
    /// });
    pub fn protocol(&self) -> crate::ProtocolVersion {
        self.esocket.protocol.into()
    }

    fn recv_event(self: Arc<Self>, e: &str, data: Value, ack: Option<i64>) -> Result<(), Error> {
        if let Some(sender) = self.message_sender.as_ref() {
            try_send_message(
                GeneralMessage {
                    key: e.to_string(),
                    socket: Some(self.clone()),
                    value: data,
                    params: vec![],
                    ack_id: ack,
                },
                sender,
            )?;
        }
        Ok(())
    }

    fn recv_bin_event(
        self: Arc<Self>,
        e: &str,
        packet: BinaryPacket,
        ack: Option<i64>,
    ) -> Result<(), Error> {
        if let Some(sender) = self.message_sender.as_ref() {
            try_send_message(
                GeneralMessage {
                    key: e.to_string(),
                    socket: Some(self.clone()),
                    value: packet.data,
                    params: packet.bin,
                    ack_id: ack,
                },
                sender,
            )?;
        }
        Ok(())
    }

    fn recv_ack(self: Arc<Self>, data: Value, ack: i64) -> Result<(), Error> {
        if let Some(tx) = self.ack_message.lock().unwrap().remove(&ack) {
            let res = AckResponse {
                data,
                binary: vec![],
            };
            tx.send(res).ok();
        }
        Ok(())
    }

    fn recv_bin_ack(self: Arc<Self>, packet: BinaryPacket, ack: i64) -> Result<(), Error> {
        if let Some(tx) = self.ack_message.lock().unwrap().remove(&ack) {
            let res = AckResponse {
                data: packet.data,
                binary: packet.bin,
            };
            tx.send(res).ok();
        }
        Ok(())
    }
}

impl<A: Adapter> Debug for Socket<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Socket")
            .field("ns", &self.ns())
            .field("ack_message", &self.ack_message)
            .field("ack_counter", &self.ack_counter)
            .field("sid", &self.id)
            .finish()
    }
}

#[cfg(test)]
impl<A: Adapter> Socket<A> {
    pub fn new_dummy(sid: Sid, ns: Arc<Namespace<A>>) -> Socket<A> {
        let close_fn = Box::new(move |_, _| ());
        Socket::new(
            sid,
            ns,
            engineioxide::Socket::new_dummy(sid, close_fn).into(),
            Arc::new(SocketIoConfig::default()),
            None,
        )
    }
}

pub(crate) fn try_send_message<A: Adapter>(
    msg: GeneralMessage<A>,
    sender: &MessageSender<A>,
) -> Result<(), Error> {
    // TODO: use while let until timeout if channel is full
    if let Err(e) = sender.try_send(msg) {
        return Err(Error::MessageSendFailed(e.to_string()));
    }
    Ok(())
}
