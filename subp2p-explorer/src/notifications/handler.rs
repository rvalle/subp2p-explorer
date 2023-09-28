// Copyright 2023 Alexandru Vasile
// This file is dual-licensed as Apache-2.0 or GPL-3.0.
// see LICENSE for license details.

use crate::notifications::{
    behavior::ProtocolsData,
    messages::BlockAnnouncesHandshake,
    upgrades::{
        combine_upgrades::CombineUpgrades,
        handshake::{
            HandshakeInbound, HandshakeInboundSubstream, HandshakeOutbound,
            HandshakeOutboundSubstream,
        },
    },
};
use bytes::BytesMut;
use codec::Encode;
use futures::{channel::mpsc, prelude::*, SinkExt};
use libp2p::{
    core::ConnectedPoint,
    swarm::{
        handler::{ConnectionEvent, FullyNegotiatedInbound},
        ConnectionHandler, ConnectionHandlerEvent, KeepAlive, Stream as NegotiatedSubstream,
        SubstreamProtocol,
    },
    PeerId,
};
use std::{
    collections::VecDeque,
    mem,
    pin::Pin,
    task::{Context, Poll},
};

const LOG_TARGET: &str = "subp2p-handler";

/// Configuration for a notifications protocol.
pub struct ProtocolDetails {
    /// The name of the notification protocol.
    pub name: String,
    /// Handshake that is submitted upon connection.
    pub handshake: Vec<u8>,
    /// Upgrades the protocol by submitting the handshake and
    pub upgrade: HandshakeInbound,
    /// The state of the protocol.
    pub state: State,
}

pub struct NotificationsHandler {
    protocols: Vec<ProtocolDetails>,

    /// Events that are pending to be processed by `poll()`.
    pending_events: VecDeque<
        ConnectionHandlerEvent<
            HandshakeOutbound,
            usize,
            NotificationsHandlerToBehavior,
            NotificationsHandlerError,
        >,
    >,

    /// Whether we are the connection dialer or listener.
    endpoint: ConnectedPoint,
    /// Peer we are connected to.
    peer: PeerId,
}

/// Events generated from the network behavior to inform about the protocol connections.
#[derive(Debug, Clone)]
pub enum NotificationsHandlerFromBehavior {
    /// Open a new notification protocol.
    Open { index: usize },
    /// Close the notification protocol.
    Close { index: usize },
}

/// Events generated by this handler.
#[derive(Debug, Clone)]
pub enum NotificationsHandlerToBehavior {
    /// Response of [`NotificationsHandlerFromBehavior::Open`].
    ///
    /// Received handshake in expected format.
    HandshakeCompleted {
        index: usize,
        endpoint: ConnectedPoint,
        handshake: Vec<u8>,
        is_inbound: bool,
        sender: mpsc::Sender<Vec<u8>>,
    },
    /// Response of [`NotificationsHandlerFromBehavior::Open`].
    ///
    /// Handshake cannot be established.
    HandshakeError {
        index: usize,
    },
    OpenDesiredByRemote {
        index: usize,
    },
    CloseDesired {
        index: usize,
    },
    /// Response of [`NotificationsHandlerFromBehavior::Close`].
    Close {
        index: usize,
    },
    /// Notification received by this protocol.
    Notification {
        index: usize,
        bytes: BytesMut,
    },
}

/// The state of a notification protocol.
///
/// ### Transitions
///
/// Closed -> OpenDesiredByRemote
///                 |
///                 |
///                 ----------------> announce behavior about protocol open
///
///                 |---------------  behavior ack
///                 |
///           OpenDesiredByRemote -> Opening -> Open
///
pub enum State {
    /// Protocol is closed.
    Closed {
        /// True if we should open the protocol.
        pending_opening: bool,
    },
    /// Initiated a new substream.
    OpenDesiredByRemote {
        /// Handle handshake.
        inbound_substream: HandshakeInboundSubstream<NegotiatedSubstream>,
        /// True if we should open the protocol.
        pending_opening: bool,
    },
    /// Opening the protocol by handshake negociation.
    Opening {
        /// Set the first time. Contains a value when the handshake is in progress.
        inbound_substream: Option<HandshakeInboundSubstream<NegotiatedSubstream>>,
        /// Direction of substream.
        inbound: bool,
    },
    /// Protocol is opened, handshake has been negociated.
    Open {
        recv: stream::Peekable<mpsc::Receiver<Vec<u8>>>,
        inbound_substream: Option<HandshakeInboundSubstream<NegotiatedSubstream>>,
        outbound_substream: Option<HandshakeOutboundSubstream<NegotiatedSubstream>>,
    },
}

impl NotificationsHandler {
    pub fn new(peer: PeerId, endpoint: ConnectedPoint, data: ProtocolsData) -> Self {
        // The blocks announces protocol is hardcoded on index 0.
        // We must accept connections of this protocol to transition the substrate
        // view of our peer into accepted state. To achive this, the provided genesis
        // hash and therefore the handshake must be valid.
        //
        // This implementation does not fallback on the legacy supported protocols (ie `/dot/../1`).
        // The genesis hash must be hex-encoded without the "0x" sufix.
        let genesis_string = hex::encode(data.genesis_hash);
        let blocks = format!("/{}/block-announces/1", genesis_string);

        // Note:
        // `../grandpa/1` and `../statement/1` are currently not registered.

        // The transaction protocol substream will broadcast a vector of extrinsics that is scale-encoded.
        let tx = format!("/{}/transactions/1", genesis_string);

        let block_announces = BlockAnnouncesHandshake::from_genesis(data.genesis_hash);

        let protocols = vec![
            ProtocolDetails {
                name: blocks.clone(),
                handshake: block_announces.encode(),
                upgrade: HandshakeInbound {
                    name: blocks.clone(),
                },
                state: State::Closed {
                    pending_opening: false,
                },
            },
            ProtocolDetails {
                name: tx.clone(),
                // Any other protocol that doesn't have a handshake must submit the node role.
                handshake: vec![data.node_role.encoded()],
                upgrade: HandshakeInbound { name: tx.clone() },
                state: State::Closed {
                    pending_opening: false,
                },
            },
        ];

        NotificationsHandler {
            peer,
            pending_events: VecDeque::with_capacity(16),
            endpoint,
            protocols,
        }
    }
}

/// Error specific to the collection of protocols.
#[derive(Debug, thiserror::Error)]
pub enum NotificationsHandlerError {}

impl ConnectionHandler for NotificationsHandler {
    // Received and submitted events.
    type FromBehaviour = NotificationsHandlerFromBehavior;
    type ToBehaviour = NotificationsHandlerToBehavior;

    type Error = NotificationsHandlerError;

    // Handle handshakes.
    type InboundProtocol = CombineUpgrades<HandshakeInbound>;
    type OutboundProtocol = HandshakeOutbound;

    // Extra information upon connections.
    type OutboundOpenInfo = usize;
    type InboundOpenInfo = ();

    fn listen_protocol(&self) -> SubstreamProtocol<Self::InboundProtocol, ()> {
        let protocol_upgrades: Vec<_> = self.protocols.iter().map(|p| p.upgrade.clone()).collect();
        let combine_upgrades = CombineUpgrades::from(protocol_upgrades);
        SubstreamProtocol::new(combine_upgrades, ())
    }

    fn on_connection_event(
        &mut self,
        event: ConnectionEvent<
            '_,
            Self::InboundProtocol,
            Self::OutboundProtocol,
            Self::InboundOpenInfo,
            Self::OutboundOpenInfo,
        >,
    ) {
        match event {
            ConnectionEvent::FullyNegotiatedInbound(FullyNegotiatedInbound {
                protocol, ..
            }) => {
                let (mut stream, index) = (protocol.data, protocol.index);

                log::debug!(target: LOG_TARGET,
                    "Handler negotiated inbound peer={:?} index={:?}",
                    self.peer,
                    index
                );

                let proto = &mut self.protocols[index];
                match proto.state {
                    State::Closed { pending_opening } => {
                        log::trace!(
                            target: LOG_TARGET,
                            "Handler negotiated inbound Closed -> OpenDesiredByRemote peer={:?} index={:?}",
                            self.peer,
                            index
                        );

                        self.pending_events
                            .push_back(ConnectionHandlerEvent::NotifyBehaviour(
                                NotificationsHandlerToBehavior::OpenDesiredByRemote { index },
                            ));

                        proto.state = State::OpenDesiredByRemote {
                            inbound_substream: stream.substream,
                            pending_opening,
                        };
                    }
                    State::OpenDesiredByRemote { .. } => {
                        log::trace!(
                            target: LOG_TARGET,
                            "Handler negotiated inbound OpenDesiredByRemote peer={:?} index={:?}",
                            self.peer,
                            index
                        );
                    }
                    State::Opening {
                        ref mut inbound_substream,
                        ..
                    }
                    | State::Open {
                        ref mut inbound_substream,
                        ..
                    } => {
                        // Already handled.
                        if inbound_substream.is_some() {
                            log::trace!(
                                target: LOG_TARGET,
                                "Handler negotiated inbound handshake already handled peer={:?} index={:?}",
                                self.peer,
                                index
                            );
                            return;
                        }

                        log::trace!(
                            target: LOG_TARGET,
                            "Handler negotiated inbound setup handshake peer={:?} index={:?}",
                            self.peer,
                            index
                        );

                        let handshake_message = proto.handshake.clone();
                        stream.substream.set_handshake(handshake_message);
                        *inbound_substream = Some(stream.substream);
                    }
                }
            }
            ConnectionEvent::FullyNegotiatedOutbound(outbound) => {
                let (opened, index) = (outbound.protocol, outbound.info);

                log::debug!(
                    target: LOG_TARGET,
                    "Handler negotiated outbound peer={:?} index={:?}",
                    self.peer,
                    index
                );

                let proto = &mut self.protocols[index];
                match proto.state {
                    State::Closed {
                        ref mut pending_opening,
                    }
                    | State::OpenDesiredByRemote {
                        ref mut pending_opening,
                        ..
                    } => {
                        log::trace!(
                            target: LOG_TARGET,
                            "Handler negotiated outbound Closed|OpenDesiredByRemote peer={:?} index={:?}",
                            self.peer,
                            index
                        );

                        *pending_opening = false;
                    }
                    State::Opening {
                        ref mut inbound_substream,
                        inbound,
                    } => {
                        log::trace!(
                            target: LOG_TARGET,
                            "Handler negotiated outbound Opening successful peer={:?} index={:?}",
                            self.peer,
                            index
                        );

                        let (send, recv) = mpsc::channel(1024);
                        proto.state = State::Open {
                            inbound_substream: inbound_substream.take(),
                            outbound_substream: Some(opened.substream),
                            recv: recv.peekable(),
                        };

                        self.pending_events
                            .push_back(ConnectionHandlerEvent::NotifyBehaviour(
                                NotificationsHandlerToBehavior::HandshakeCompleted {
                                    index,
                                    endpoint: self.endpoint.clone(),
                                    handshake: opened.handshake,
                                    is_inbound: inbound,
                                    sender: send,
                                },
                            ));
                    }
                    State::Open { .. } => {
                        log::trace!(
                            target: LOG_TARGET,
                            "Handler negotiated outbound Open missmatch-state peer={:?} index={:?}",
                            self.peer,
                            index
                        );
                    }
                }
            }
            ConnectionEvent::DialUpgradeError(err) => {
                log::debug!(
                    target: LOG_TARGET,
                    "Handler DialError peer={:?} index={:?} error={:?}",
                    self.peer,
                    err.info,
                    err.error,
                );

                let proto = &mut self.protocols[err.info];

                match proto.state {
                    State::Closed {
                        ref mut pending_opening,
                    }
                    | State::OpenDesiredByRemote {
                        ref mut pending_opening,
                        ..
                    } => {
                        log::trace!(
                            target: LOG_TARGET,
                            "Handler DialError Closed|OpenDesiredByRemote peer={:?} info={:?}",
                            self.peer,
                            err.info,
                        );

                        *pending_opening = false;
                    }
                    State::Opening { .. } => {
                        proto.state = State::Closed {
                            pending_opening: false,
                        };

                        log::trace!(
                            target: LOG_TARGET,
                            "Handler DialError Opening -> Closed peer={:?} info={:?}",
                            self.peer,
                            err.info,
                        );

                        self.pending_events
                            .push_back(ConnectionHandlerEvent::NotifyBehaviour(
                                NotificationsHandlerToBehavior::HandshakeError { index: err.info },
                            ));
                    }
                    State::Open { .. } => {}
                }
            }
            _ => {}
        }
    }

    fn on_behaviour_event(&mut self, message: NotificationsHandlerFromBehavior) {
        match message {
            NotificationsHandlerFromBehavior::Open { index } => {
                log::debug!(
                    target: LOG_TARGET,
                    "Handler from behavior Open peer={:?} index={:?}",
                    self.peer,
                    index
                );

                let proto = &mut self.protocols[index];

                match &mut proto.state {
                    State::Closed { pending_opening } => {
                        if !*pending_opening {
                            let protocol = HandshakeOutbound {
                                name: proto.name.clone(),
                                handshake: proto.handshake.clone(),
                            };

                            log::trace!(
                                target: LOG_TARGET,
                                "Handler from behavior Closed -> request new substream peer={:?} index={:?}",
                                self.peer,
                                index
                            );

                            self.pending_events.push_back(
                                ConnectionHandlerEvent::OutboundSubstreamRequest {
                                    protocol: SubstreamProtocol::new(protocol, index),
                                },
                            )
                        }

                        log::trace!(
                            target: LOG_TARGET,
                            "Handler from behavior Closed -> Opening peer={:?} index={:?}",
                            self.peer,
                            index
                        );

                        proto.state = State::Opening {
                            inbound_substream: None,
                            inbound: false,
                        };
                    }
                    State::OpenDesiredByRemote {
                        inbound_substream,
                        pending_opening,
                    } => {
                        if !*pending_opening {
                            let protocol = HandshakeOutbound {
                                name: proto.name.clone(),
                                handshake: proto.handshake.clone(),
                            };

                            log::trace!(
                                target: LOG_TARGET,
                                "Handler from behavior OpenDesiredByRemote -> request new substream peer={:?} index={:?}",
                                self.peer,
                                index
                            );

                            self.pending_events.push_back(
                                ConnectionHandlerEvent::OutboundSubstreamRequest {
                                    protocol: SubstreamProtocol::new(protocol, index),
                                },
                            )
                        }

                        log::trace!(
                            target: LOG_TARGET,
                            "Handler from behavior OpenDesiredByRemote setup handshake peer={:?} index={:?}",
                            self.peer,
                            index
                        );

                        let handshake = proto.handshake.clone();
                        inbound_substream.set_handshake(handshake);

                        let inbound_substream = match mem::replace(
                            &mut proto.state,
                            State::Opening {
                                inbound_substream: None,
                                inbound: false,
                            },
                        ) {
                            State::OpenDesiredByRemote {
                                inbound_substream, ..
                            } => inbound_substream,
                            _ => unreachable!(),
                        };
                        proto.state = State::Opening {
                            inbound_substream: Some(inbound_substream),
                            inbound: true,
                        };
                    }
                    State::Opening { .. } | State::Open { .. } => {
                        log::trace!(
                            target: LOG_TARGET,
                            "Handler from behavior Opening|Open statemissmatch peer={:?} index={:?}",
                            self.peer,
                            index
                        );
                    }
                }
            }

            NotificationsHandlerFromBehavior::Close { index } => {
                log::debug!(
                    target: LOG_TARGET,
                    "Handler from behavior Close peer={:?} index={:?}",
                    self.peer,
                    index
                );

                let proto = &mut self.protocols[index];

                match proto.state {
                    State::Closed { .. } => {}
                    State::OpenDesiredByRemote {
                        pending_opening, ..
                    } => {
                        proto.state = State::Closed { pending_opening };
                    }
                    State::Opening { .. } => {
                        proto.state = State::Closed {
                            pending_opening: true,
                        };

                        log::trace!(
                            target: LOG_TARGET,
                            "Handler from behavior Close with handshake in progress peer={:?} index={:?}",
                            self.peer,
                            index
                        );

                        self.pending_events
                            .push_back(ConnectionHandlerEvent::NotifyBehaviour(
                                NotificationsHandlerToBehavior::HandshakeError { index },
                            ));
                    }
                    State::Open { .. } => {
                        proto.state = State::Closed {
                            pending_opening: false,
                        };
                    }
                }

                self.pending_events
                    .push_back(ConnectionHandlerEvent::NotifyBehaviour(
                        NotificationsHandlerToBehavior::Close { index },
                    ));
            }
        }
    }

    fn connection_keep_alive(&self) -> KeepAlive {
        if self
            .protocols
            .iter()
            .any(|p| !matches!(p.state, State::Closed { .. }))
        {
            return KeepAlive::Yes;
        }

        KeepAlive::No
    }

    fn poll(
        &mut self,
        cx: &mut Context,
    ) -> Poll<
        ConnectionHandlerEvent<
            Self::OutboundProtocol,
            Self::OutboundOpenInfo,
            Self::ToBehaviour,
            Self::Error,
        >,
    > {
        if let Some(ev) = self.pending_events.pop_front() {
            return Poll::Ready(ev);
        }

        // Propagate user submitted message for the given protocol.
        for index in 0..self.protocols.len() {
            if let State::Open {
                outbound_substream: Some(outbound_substream),
                recv,
                ..
            } = &mut self.protocols[index].state
            {
                loop {
                    // Step 1. Check if we received a messages from the user.
                    // Step 2. Check if the peer substream is ready to receive the message.
                    // Step 3. Fetch the message from the user channel.
                    // Step 4. Send the message on the peer substream.

                    match Pin::new(&mut *recv).as_mut().poll_peek(cx) {
                        Poll::Ready(Some(..)) => {}
                        _ => break,
                    };

                    match outbound_substream.poll_ready_unpin(cx) {
                        Poll::Ready(_) => {}
                        Poll::Pending => break,
                    };

                    let message = match recv.poll_next_unpin(cx) {
                        Poll::Ready(Some(message)) => message,
                        Poll::Ready(None) | Poll::Pending => {
                            // Should never be reached, as per `poll_peek` above.
                            debug_assert!(false);
                            break;
                        }
                    };

                    log::trace!(
                        target: LOG_TARGET,
                        "Handler poll send message peer={:?} index={:?} message={:?}",
                        self.peer,
                        index,
                        message
                    );

                    // Flush all outbound streams below.
                    let _ = outbound_substream.start_send_unpin(message);
                }
            }
        }

        // Flush outbound stream.
        for index in 0..self.protocols.len() {
            if let State::Open {
                outbound_substream: outbound_substream @ Some(_),
                ..
            } = &mut self.protocols[index].state
            {
                match Sink::poll_flush(Pin::new(outbound_substream.as_mut().unwrap()), cx) {
                    Poll::Pending | Poll::Ready(Ok(())) => {}
                    Poll::Ready(Err(_)) => {
                        *outbound_substream = None;

                        return Poll::Ready(ConnectionHandlerEvent::NotifyBehaviour(
                            NotificationsHandlerToBehavior::CloseDesired { index },
                        ));
                    }
                }
            }
        }

        // Poll inbound stream.
        for index in 0..self.protocols.len() {
            match &mut self.protocols[index].state {
                State::Open {
                    inbound_substream: inbound_substream @ Some(_),
                    ..
                } => match Stream::poll_next(Pin::new(inbound_substream.as_mut().unwrap()), cx) {
                    Poll::Pending => {}
                    Poll::Ready(Some(Ok(bytes))) => {
                        let event = NotificationsHandlerToBehavior::Notification { index, bytes };
                        return Poll::Ready(ConnectionHandlerEvent::NotifyBehaviour(event));
                    }
                    Poll::Ready(None) | Poll::Ready(Some(Err(_))) => *inbound_substream = None,
                },

                State::OpenDesiredByRemote {
                    inbound_substream,
                    pending_opening,
                } => match HandshakeInboundSubstream::poll_process(Pin::new(inbound_substream), cx)
                {
                    Poll::Pending => {}
                    Poll::Ready(Ok(void)) => match void {},
                    Poll::Ready(Err(_)) => {
                        self.protocols[index].state = State::Closed {
                            pending_opening: *pending_opening,
                        };
                        return Poll::Ready(ConnectionHandlerEvent::NotifyBehaviour(
                            NotificationsHandlerToBehavior::CloseDesired { index },
                        ));
                    }
                },

                State::Opening {
                    inbound_substream: inbound_substream @ Some(_),
                    ..
                } => match HandshakeInboundSubstream::poll_process(
                    Pin::new(inbound_substream.as_mut().unwrap()),
                    cx,
                ) {
                    Poll::Pending => {}
                    Poll::Ready(Ok(void)) => match void {},
                    Poll::Ready(Err(_)) => *inbound_substream = None,
                },

                _ => (),
            }
        }

        Poll::Pending
    }
}
