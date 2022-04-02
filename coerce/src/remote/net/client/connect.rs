use crate::actor::context::ActorContext;
use crate::actor::message::{Handler, Message};
use crate::actor::scheduler::timer::Timer;
use crate::actor::{Actor, IntoActor, LocalActorRef};
use crate::remote::actor::message::ClientConnected;
use crate::remote::cluster::node::{RemoteNode, RemoteNodeState};
use crate::remote::net::client::ping::PingTick;
use crate::remote::net::client::receive::{ClientMessageReceiver, HandshakeAcknowledge};
use crate::remote::net::client::send::write_bytes;
use crate::remote::net::client::{
    BeginHandshake, ClientState, ClientType, ConnectionState, HandshakeStatus, RemoteClient,
};
use crate::remote::net::codec::NetworkCodec;
use crate::remote::net::message::{datetime_to_timestamp, SessionEvent};
use crate::remote::net::proto::network as proto;
use crate::remote::net::{receive_loop, StreamData};
use crate::remote::system::{NodeId, RemoteActorSystem};
use crate::remote::tracing::extract_trace_identifier;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio::sync::oneshot::Sender;
use tokio_util::codec::{FramedRead, FramedWrite};

pub struct Connect;

pub struct OnConnect(pub Sender<(LocalActorRef<RemoteClient>, RemoteNode)>);

impl RemoteClient {
    pub async fn connect(
        &mut self,
        _connect: Connect,
        ctx: &mut ActorContext,
    ) -> Option<ConnectionState> {
        let span = tracing::trace_span!("RemoteClient::connect", address = self.addr.as_str());

        let _enter = span.enter();
        let stream = TcpStream::connect(&self.addr).await;
        if stream.is_err() {
            return None;
        }

        let stream = stream.unwrap();
        let (read, writer) = tokio::io::split(stream);

        let reader = FramedRead::new(read, NetworkCodec);
        let mut write = FramedWrite::new(writer, NetworkCodec);

        let (identity_tx, identity_rx) = oneshot::channel();

        let remote = ctx.system().remote_owned();
        let receive_task = tokio::spawn(receive_loop(
            remote.clone(),
            reader,
            ClientMessageReceiver::new(self.actor_ref(ctx), identity_tx),
        ));

        let ping_timer = Some(Timer::start_immediately(
            self.actor_ref(ctx),
            Duration::from_millis(500),
            PingTick,
        ));

        let identity = match identity_rx.await {
            Ok(identity) => identity,
            Err(_) => {
                warn!("no identity received (addr={})", &self.addr);
                return None;
            }
        };

        Some(ConnectionState {
            identity,
            handshake: HandshakeStatus::None,
            write,
            receive_task,
            ping_timer,
        })
    }
}

pub struct Disconnected;

const RECONNECT_DELAY: Duration = Duration::from_millis(1000);

#[async_trait]
impl Handler<Connect> for RemoteClient {
    async fn handle(&mut self, message: Connect, ctx: &mut ActorContext) {
        let span = tracing::trace_span!("RemoteClient::connect", actor_id = ctx.id().as_str(),);

        let _enter = span.enter();

        if let Some(connection_state) = self.connect(message, ctx).await {
            while let Some(callback) = self.on_identified_callbacks.pop() {
                let _ = callback.send(Some(connection_state.identity.clone()));
            }

            let client_actor_ref = self.actor_ref(ctx);

            let _ = ctx
                .system()
                .remote()
                .client_registry()
                .send(ClientConnected {
                    addr: self.addr.clone(),
                    remote_node_id: connection_state.identity.node.id,
                    client_actor_ref,
                })
                .await;

            self.state = Some(ClientState::Connected(connection_state));

            debug!("RemoteClient connected to node (addr={})", &self.addr);

            self.flush_buffered_writes().await;
        } else {
            warn!("RemoteClient failed to connect");
            while let Some(callback) = self.on_identified_callbacks.pop() {
                let _ = callback.send(None);
            }

            self.handle(Disconnected, ctx).await;
        }
    }
}

#[async_trait]
impl Handler<BeginHandshake> for RemoteClient {
    async fn handle(&mut self, message: BeginHandshake, ctx: &mut ActorContext) {
        let mut connection = match &mut self.state {
            Some(ClientState::Connected(connection)) => connection,
            _ => return,
        };

        let remote = ctx.system().remote_owned();
        let node_id = remote.node_id();
        let node_tag = remote.node_tag().to_string();

        trace!("writing handshake");

        write_bytes(
            SessionEvent::Handshake(proto::SessionHandshake {
                node_id,
                node_tag,
                token: vec![],
                client_type: self.client_type.into(),
                trace_id: String::new(),
                nodes: message
                    .seed_nodes
                    .into_iter()
                    .map(|node| proto::RemoteNode {
                        node_id: node.id,
                        addr: node.addr,
                        tag: node.tag,
                        node_started_at: node
                            .node_started_at
                            .as_ref()
                            .map(datetime_to_timestamp)
                            .into(),
                        ..proto::RemoteNode::default()
                    })
                    .collect(),
                ..proto::SessionHandshake::default()
            })
            .write_to_bytes()
            .unwrap()
            .as_ref(),
            &mut connection.write,
        )
        .await
        .expect("write handshake");

        self.on_handshake_ack_callbacks.push(message.on_handshake_complete);
    }
}

#[async_trait]
impl Handler<HandshakeAcknowledge> for RemoteClient {
    async fn handle(&mut self, message: HandshakeAcknowledge, _ctx: &mut ActorContext) {
        info!(
            "handshake acknowledged (addr={}, node_id={}, node_tag={})",
            &self.addr, &message.node_id, &message.node_tag
        );

        match &mut self.state {
            Some(ClientState::Connected(state)) => {
                state.handshake = HandshakeStatus::Acknowledged(message);

                while let Some(callback) = self.on_handshake_ack_callbacks.pop() {
                    let _ = callback.send(());
                }
            }
            _ => {
                warn!("received handshakeack but the client connection state is invalid");
            }
        }
    }
}

#[async_trait]
impl Handler<Disconnected> for RemoteClient {
    async fn handle(&mut self, _msg: Disconnected, ctx: &mut ActorContext) {
        // TODO: try to connect again, if fails after {n} attempts with a timeout,
        //       we should quarantine the node and ensuring the node no longer
        //       participates in cluster activities/sharding

        warn!(
            "RemoteClient connection to node (addr={}) closed/failed, retrying in {}ms",
            &self.addr,
            RECONNECT_DELAY.as_millis()
        );

        let state = match self.state.take().unwrap() {
            ClientState::Idle {
                connection_attempts,
            } => {
                let connection_attempts = connection_attempts + 1;

                ClientState::Idle {
                    connection_attempts,
                }
            }

            ClientState::Quarantined {
                since,
                connection_attempts,
            } => {
                let connection_attempts = connection_attempts + 1;

                ClientState::Quarantined {
                    since,
                    connection_attempts,
                }
            }

            ClientState::Connected(mut state) => {
                state.disconnected().await;

                ClientState::Idle {
                    connection_attempts: 1,
                }
            }
        };

        self.state = Some(state);

        let self_ref = self.actor_ref(ctx);
        tokio::spawn(async move {
            tokio::time::sleep(RECONNECT_DELAY).await;
            let _res = self_ref.send(Connect).await;
        });
    }
}

impl Message for Connect {
    type Result = ();
}

impl Message for Disconnected {
    type Result = ();
}