use bytes::BytesMut;
use chrono::Utc;
use codecs::{
    decoding::{DeserializerConfig, FramingConfig},
    StreamDecodingError,
};
use futures::StreamExt;
use listenfd::ListenFd;
use lookup::{lookup_v2::BorrowedSegment, path};
use tokio_util::codec::FramedRead;
use vector_common::internal_event::{ByteSize, BytesReceived, InternalEventHandle as _, Protocol};
use vector_config::{configurable_component, NamedComponent};
use vector_core::{
    config::{LegacyKey, LogNamespace},
    ByteSizeOf,
};

use crate::{
    codecs::Decoder,
    config::log_schema,
    event::Event,
    internal_events::{
        SocketBindError, SocketEventsReceived, SocketMode, SocketReceiveError, StreamClosedError,
    },
    serde::{default_decoding, default_framing_message_based},
    shutdown::ShutdownSignal,
    sources::{
        socket::SocketConfig,
        util::net::{try_bind_udp_socket, SocketListenAddr},
        Source,
    },
    udp, SourceSender,
};

/// UDP configuration for the `socket` source.
#[configurable_component]
#[derive(Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct UdpConfig {
    /// The address to listen for messages on.
    address: SocketListenAddr,

    /// The maximum buffer size, in bytes, of incoming messages.
    ///
    /// Messages larger than this are truncated.
    #[serde(default = "crate::serde::default_max_length")]
    pub(super) max_length: usize,

    /// Overrides the name of the log field used to add the peer host to each event.
    ///
    /// The value will be the peer host's address, including the port i.e. `1.2.3.4:9000`.
    ///
    /// By default, the [global `log_schema.host_key` option][global_host_key] is used.
    ///
    /// [global_host_key]: https://vector.dev/docs/reference/configuration/global-options/#log_schema.host_key
    host_key: Option<String>,

    /// Overrides the name of the log field used to add the peer host's port to each event.
    ///
    /// The value will be the peer host's port i.e. `9000`.
    ///
    /// By default, `"port"` is used.
    port_key: Option<String>,

    /// The size, in bytes, of the receive buffer used for the listening socket.
    ///
    /// This should not typically needed to be changed.
    receive_buffer_bytes: Option<usize>,

    #[configurable(derived)]
    #[serde(default = "default_framing_message_based")]
    pub(super) framing: FramingConfig,

    #[configurable(derived)]
    #[serde(default = "default_decoding")]
    decoding: DeserializerConfig,

    /// The namespace to use for logs. This overrides the global setting.
    #[serde(default)]
    pub log_namespace: Option<bool>,
}

impl UdpConfig {
    pub(super) const fn host_key(&self) -> &Option<String> {
        &self.host_key
    }

    pub const fn port_key(&self) -> &Option<String> {
        &self.port_key
    }

    pub(super) const fn framing(&self) -> &FramingConfig {
        &self.framing
    }

    pub(super) const fn decoding(&self) -> &DeserializerConfig {
        &self.decoding
    }

    pub(super) const fn address(&self) -> SocketListenAddr {
        self.address
    }

    pub fn from_address(address: SocketListenAddr) -> Self {
        Self {
            address,
            max_length: crate::serde::default_max_length(),
            host_key: None,
            port_key: Some(String::from("port")),
            receive_buffer_bytes: None,
            framing: default_framing_message_based(),
            decoding: default_decoding(),
            log_namespace: None,
        }
    }

    pub fn set_log_namespace(&mut self, val: Option<bool>) -> &mut Self {
        self.log_namespace = val;
        self
    }
}

pub(super) fn udp(
    config: UdpConfig,
    decoder: Decoder,
    mut shutdown: ShutdownSignal,
    mut out: SourceSender,
    log_namespace: LogNamespace,
) -> Source {
    Box::pin(async move {
        let listenfd = ListenFd::from_env();
        let socket = try_bind_udp_socket(config.address, listenfd)
            .await
            .map_err(|error| {
                emit!(SocketBindError {
                    mode: SocketMode::Udp,
                    error,
                })
            })?;

        if let Some(receive_buffer_bytes) = config.receive_buffer_bytes {
            if let Err(error) = udp::set_receive_buffer_size(&socket, receive_buffer_bytes) {
                warn!(message = "Failed configuring receive buffer size on UDP socket.", %error);
            }
        }

        let max_length = match config.receive_buffer_bytes {
            Some(receive_buffer_bytes) => std::cmp::min(config.max_length, receive_buffer_bytes),
            None => config.max_length,
        };

        let bytes_received = register!(BytesReceived::from(Protocol::UDP));

        info!(message = "Listening.", address = %config.address);

        // We add 1 to the max_length in order to determine if the received data has been truncated.
        let mut buf = BytesMut::with_capacity(max_length + 1);
        loop {
            buf.resize(max_length + 1, 0);
            tokio::select! {
                recv = socket.recv_from(&mut buf) => {
                    let (byte_size, address) = match recv {
                        Ok(res) => res,
                        Err(error) => {
                            #[cfg(windows)]
                            if let Some(err) = error.raw_os_error() {
                                if err == 10040 {
                                    // 10040 is the Windows error that the Udp message has exceeded max_length
                                    warn!(
                                        message = "Discarding frame larger than max_length.",
                                        max_length = max_length,
                                        internal_log_rate_limit = true
                                    );
                                    continue;
                                }
                            }

                            return Err(emit!(SocketReceiveError {
                                mode: SocketMode::Udp,
                                error
                            }));
                       }
                    };

                    bytes_received.emit(ByteSize(byte_size));

                    let payload = buf.split_to(byte_size);
                    let truncated = byte_size == max_length + 1;

                    let mut stream = FramedRead::new(payload.as_ref(), decoder.clone()).peekable();

                    while let Some(result) = stream.next().await {
                        let last = Pin::new(&mut stream).peek().await.is_none();
                        match result {
                            Ok((mut events, _byte_size)) => {
                                if last && truncated {
                                    // The last event in this payload was truncated, so we want to drop it.
                                    let _ = events.pop();
                                    warn!(
                                        message = "Discarding frame larger than max_length.",
                                        max_length = max_length,
                                        internal_log_rate_limit = true
                                    );
                                }

                                if events.is_empty() {
                                    continue;
                                }

                                let count = events.len();
                                emit!(SocketEventsReceived {
                                    mode: SocketMode::Udp,
                                    byte_size: events.size_of(),
                                    count,
                                });

                                let now = Utc::now();

                                for event in &mut events {
                                    if let Event::Log(ref mut log) = event {
                                        log_namespace.insert_standard_vector_source_metadata(
                                            log,
                                            SocketConfig::NAME,
                                            now,
                                        );

                                        let host_key_path = config.host_key.as_ref().map_or_else(
                                            || [BorrowedSegment::from(log_schema().host_key())],
                                            |key| [BorrowedSegment::from(key)],
                                        );

                                        log_namespace.insert_source_metadata(
                                            SocketConfig::NAME,
                                            log,
                                            Some(LegacyKey::InsertIfEmpty(&host_key_path)),
                                            path!("host"),
                                            address.ip().to_string()
                                        );

                                        let port_key_path = config.port_key.as_ref().map_or_else(
                                            || [BorrowedSegment::from("port")],
                                            |key| [BorrowedSegment::from(key)],
                                        );

                                        log_namespace.insert_source_metadata(
                                            SocketConfig::NAME,
                                            log,
                                            Some(LegacyKey::InsertIfEmpty(&port_key_path)),
                                            path!("port"),
                                            address.port()
                                        );
                                    }
                                }

                                tokio::select!{
                                    result = out.send_batch(events) => {
                                        if let Err(error) = result {
                                            emit!(StreamClosedError { error, count });
                                            return Ok(())
                                        }
                                    }
                                    _ = &mut shutdown => return Ok(()),
                                }
                            }
                            Err(error) => {
                                // Error is logged by `crate::codecs::Decoder`, no
                                // further handling is needed here.
                                if !error.can_continue() {
                                    break;
                                }
                            }
                        }
                    }
                }
                _ = &mut shutdown => return Ok(()),
            }
        }
    })
}
