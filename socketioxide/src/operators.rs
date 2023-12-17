//! [`Operators`] are used to select sockets to send a packet to, or to configure the packet that will be emitted.
//! It uses the builder pattern to chain operators.
use std::borrow::Cow;
use std::{sync::Arc, time::Duration};

use engineioxide::sid::Sid;
use futures::stream::BoxStream;
use serde::de::DeserializeOwned;

use crate::adapter::LocalAdapter;
use crate::errors::BroadcastError;
use crate::extract::SocketRef;
use crate::socket::AckResponse;
use crate::{
    adapter::{Adapter, BroadcastFlags, BroadcastOptions, Room},
    errors::AckError,
    ns::Namespace,
    packet::Packet,
};

/// A trait for types that can be used as a room parameter.
///
/// [`String`], [`Vec<String>`], [`Vec<&str>`], [`&'static str`](str) and const arrays are implemented by default.
pub trait RoomParam: 'static {
    /// The type of the iterator returned by `into_room_iter`.
    type IntoIter: Iterator<Item = Room>;

    /// Convert `self` into an iterator of rooms.
    fn into_room_iter(self) -> Self::IntoIter;
}

impl RoomParam for Room {
    type IntoIter = std::iter::Once<Room>;
    #[inline(always)]
    fn into_room_iter(self) -> Self::IntoIter {
        std::iter::once(self)
    }
}
impl RoomParam for String {
    type IntoIter = std::iter::Once<Room>;
    #[inline(always)]
    fn into_room_iter(self) -> Self::IntoIter {
        std::iter::once(Cow::Owned(self))
    }
}
impl RoomParam for Vec<String> {
    type IntoIter = std::iter::Map<std::vec::IntoIter<String>, fn(String) -> Room>;
    #[inline(always)]
    fn into_room_iter(self) -> Self::IntoIter {
        self.into_iter().map(Cow::Owned)
    }
}
impl RoomParam for Vec<&'static str> {
    type IntoIter = std::iter::Map<std::vec::IntoIter<&'static str>, fn(&'static str) -> Room>;
    #[inline(always)]
    fn into_room_iter(self) -> Self::IntoIter {
        self.into_iter().map(Cow::Borrowed)
    }
}

impl RoomParam for Vec<Room> {
    type IntoIter = std::vec::IntoIter<Room>;
    #[inline(always)]
    fn into_room_iter(self) -> Self::IntoIter {
        self.into_iter()
    }
}
impl RoomParam for &'static str {
    type IntoIter = std::iter::Once<Room>;
    #[inline(always)]
    fn into_room_iter(self) -> Self::IntoIter {
        std::iter::once(Cow::Borrowed(self))
    }
}
impl<const COUNT: usize> RoomParam for [&'static str; COUNT] {
    type IntoIter =
        std::iter::Map<std::array::IntoIter<&'static str, COUNT>, fn(&'static str) -> Room>;

    #[inline(always)]
    fn into_room_iter(self) -> Self::IntoIter {
        self.into_iter().map(Cow::Borrowed)
    }
}
impl<const COUNT: usize> RoomParam for [String; COUNT] {
    type IntoIter = std::iter::Map<std::array::IntoIter<String, COUNT>, fn(String) -> Room>;
    #[inline(always)]
    fn into_room_iter(self) -> Self::IntoIter {
        self.into_iter().map(Cow::Owned)
    }
}
impl RoomParam for Sid {
    type IntoIter = std::iter::Once<Room>;
    #[inline(always)]
    fn into_room_iter(self) -> Self::IntoIter {
        std::iter::once(Cow::Owned(self.to_string()))
    }
}

/// Operators are used to select sockets to send a packet to, or to configure the packet that will be emitted.
#[derive(Debug)]
pub struct Operators<A: Adapter = LocalAdapter> {
    opts: BroadcastOptions,
    ns: Arc<Namespace<A>>,
    binary: Vec<Vec<u8>>,
}

impl<A: Adapter> Operators<A> {
    pub(crate) fn new(ns: Arc<Namespace<A>>, sid: Option<Sid>) -> Self {
        Self {
            opts: BroadcastOptions::new(sid),
            ns,
            binary: vec![],
        }
    }

    /// Selects all sockets in the given rooms except the current socket.
    /// If it is called from the `Namespace` level there will be no difference with the `within()` operator
    ///
    /// If you want to include the current socket, use the `within()` operator.
    pub fn to(mut self, rooms: impl RoomParam) -> Self {
        self.opts.rooms.extend(rooms.into_room_iter());
        self.opts.flags.insert(BroadcastFlags::Broadcast);
        self
    }

    /// Selects all sockets in the given rooms.
    ///
    /// It does include the current socket contrary to the `to()` operator.
    /// If it is called from the `Namespace` level there will be no difference with the `to()` operator
    pub fn within(mut self, rooms: impl RoomParam) -> Self {
        self.opts.rooms.extend(rooms.into_room_iter());
        self
    }

    /// Filters out all sockets selected with the previous operators which are in the given rooms.
    pub fn except(mut self, rooms: impl RoomParam) -> Self {
        self.opts.except.extend(rooms.into_room_iter());
        self.opts.flags.insert(BroadcastFlags::Broadcast);
        self
    }

    /// Broadcasts to all sockets only connected on this node (when using multiple nodes).
    /// When using the default in-memory adapter, this operator is a no-op.
    pub fn local(mut self) -> Self {
        self.opts.flags.insert(BroadcastFlags::Local);
        self
    }

    /// Broadcasts to all sockets without any filtering (except the current socket).
    pub fn broadcast(mut self) -> Self {
        self.opts.flags.insert(BroadcastFlags::Broadcast);
        self
    }

    /// Sets a custom timeout when sending a message with an acknowledgement.
    ///
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.opts.flags.insert(BroadcastFlags::Timeout(timeout));
        self
    }

    /// Adds a binary payload to the message.
    pub fn bin(mut self, binary: Vec<Vec<u8>>) -> Self {
        self.binary = binary;
        self
    }

    /// Emits a message to all sockets selected with the previous operators.
    pub fn emit(
        mut self,
        event: impl Into<Cow<'static, str>>,
        data: impl serde::Serialize,
    ) -> Result<(), BroadcastError> {
        let packet = self.get_packet(event, data)?;
        if let Err(e) = self.ns.adapter.broadcast(packet, self.opts) {
            #[cfg(feature = "tracing")]
            tracing::debug!("broadcast error: {e:?}");
            return Err(e);
        }
        Ok(())
    }

    /// Emits a message to all sockets selected with the previous operators and return a stream of acknowledgements.
    ///
    /// Each acknowledgement has a timeout specified in the config (5s by default) or with the `timeout()` operator.
    pub fn emit_with_ack<V: DeserializeOwned + Send>(
        mut self,
        event: impl Into<Cow<'static, str>>,
        data: impl serde::Serialize,
    ) -> Result<BoxStream<'static, Result<AckResponse<V>, AckError>>, BroadcastError> {
        let packet = self.get_packet(event, data)?;
        self.ns.adapter.broadcast_with_ack(packet, self.opts)
    }

    /// Gets all sockets selected with the previous operators.
    ///
    /// It can be used to retrieve any extension data (with the `extensions` feature enabled) from the sockets or to make some sockets join other rooms.
    pub fn sockets(self) -> Result<Vec<SocketRef<A>>, A::Error> {
        self.ns.adapter.fetch_sockets(self.opts)
    }

    /// Disconnects all sockets selected with the previous operators.
    ///
    pub fn disconnect(self) -> Result<(), BroadcastError> {
        self.ns.adapter.disconnect_socket(self.opts)
    }

    /// Makes all sockets selected with the previous operators join the given room(s).
    ///
    pub fn join(self, rooms: impl RoomParam) -> Result<(), A::Error> {
        self.ns.adapter.add_sockets(self.opts, rooms)
    }

    /// Makes all sockets selected with the previous operators leave the given room(s).
    ///
    pub fn leave(self, rooms: impl RoomParam) -> Result<(), A::Error> {
        self.ns.adapter.del_sockets(self.opts, rooms)
    }

    /// Creates a packet with the given event and data.
    fn get_packet(
        &mut self,
        event: impl Into<Cow<'static, str>>,
        data: impl serde::Serialize,
    ) -> Result<Packet<'static>, serde_json::Error> {
        let ns = self.ns.path.clone();
        let data = serde_json::to_value(data)?;
        let packet = if self.binary.is_empty() {
            Packet::event(ns, event.into(), data)
        } else {
            let binary = std::mem::take(&mut self.binary);
            Packet::bin_event(ns, event.into(), data, binary)
        };
        Ok(packet)
    }
}

#[cfg(feature = "test-utils")]
impl<A: Adapter> Operators<A> {
    #[allow(dead_code)]
    pub(crate) fn is_broadcast(&self) -> bool {
        self.opts.flags.contains(&BroadcastFlags::Broadcast)
    }
}
