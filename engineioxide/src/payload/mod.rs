use crate::{errors::Error, packet::Packet, service::ProtocolVersion};
use futures::{future::Either, Stream};
use http::{header::CONTENT_TYPE, Request};
use tokio::sync::{mpsc::Receiver, MutexGuard};
use tracing::debug;

mod buf;
mod decoder;
mod encoder;

#[cfg(feature = "v4")]
const PACKET_SEPARATOR_V4: u8 = b'\x1e';
#[cfg(feature = "v3")]
const STRING_PACKET_SEPARATOR_V3: u8 = b':';
#[cfg(feature = "v3")]
const BINARY_PACKET_SEPARATOR_V3: u8 = 0xff;
#[cfg(feature = "v3")]
const STRING_PACKET_IDENTIFIER_V3: u8 = 0x00;
#[cfg(feature = "v3")]
const BINARY_PACKET_IDENTIFIER_V3: u8 = 0x01;

pub fn decoder(
    body: Request<impl http_body::Body<Error = impl std::fmt::Debug> + Unpin>,
    #[allow(unused_variables)] protocol: ProtocolVersion,
    max_payload: u64,
) -> impl Stream<Item = Result<Packet, Error>> {
    #[cfg(all(feature = "v3", feature = "v4"))]
    {
        debug!("decoding payload {:?}", body.headers().get(CONTENT_TYPE));
        let is_binary =
            body.headers().get(CONTENT_TYPE) == Some(&"application/octet-stream".parse().unwrap());
        match protocol {
            ProtocolVersion::V4 => Either::Left(decoder::v4_decoder(body, max_payload)),
            ProtocolVersion::V3 if is_binary => {
                Either::Right(Either::Left(decoder::v3_binary_decoder(body, max_payload)))
            }
            ProtocolVersion::V3 => {
                Either::Right(Either::Right(decoder::v3_string_decoder(body, max_payload)))
            }
        }
    }

    #[cfg(all(feature = "v3", not(feature = "v4")))]
    {
        if is_binary {
            Either::Left(decoder::v3_binary_decoder(body, max_payload))
        } else {
            Either::Right(decoder::v3_binary_decoder(body, max_payload))
        }
    }
    #[cfg(all(feature = "v4", not(feature = "v3")))]
    {
        decoder::v4_decoder(body, max_payload)
    }
}

pub async fn encoder(
    rx: MutexGuard<'_, Receiver<Packet>>,
    #[allow(unused_variables)] protocol: ProtocolVersion,
    #[cfg(feature = "v3")] supports_binary: bool,
) -> Result<Vec<u8>, Error> {
    #[cfg(all(feature = "v3", feature = "v4"))]
    {
        match protocol {
            ProtocolVersion::V4 => encoder::v4_encoder(rx).await,
            ProtocolVersion::V3 if supports_binary => encoder::v3_binary_encoder(rx).await,
            ProtocolVersion::V3 => encoder::v3_string_encoder(rx).await,
        }
    }

    #[cfg(all(feature = "v3", not(feature = "v4")))]
    {
        if supports_binary {
            encoder::v3_binary_encoder(rx).await
        } else {
            encoder::v3_string_encoder(rx).await
        }
    }
    #[cfg(all(feature = "v4", not(feature = "v3")))]
    {
        encoder::v4_encoder(rx).await
    }
}
