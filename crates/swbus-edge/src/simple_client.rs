use crate::SwbusEdgeRuntime;
use std::collections::HashMap;
use std::sync::Arc;
use swbus_proto::{
    message_id_generator::MessageIdGenerator,
    result::Result,
    swbus::{
        request_response::ResponseBody, swbus_message::Body, DataRequest, ManagementQueryResult, ManagementRequest,
        ManagementRequestType, RequestResponse, ServicePath, SwbusErrorCode, SwbusMessage, SwbusMessageHeader,
    },
};
use tokio::sync::{
    mpsc::{channel, Receiver},
    Mutex,
};

/// The type used by Swbus for message ids. Alias for `u64`.
pub type MessageId = u64;

/// Simplified interface to [`SwbusEdgeRuntime`] that does not expose infra messages, message id
/// generation, raw message construction, and other internal details to Swbus clients.
pub struct SimpleSwbusEdgeClient {
    rt: Arc<SwbusEdgeRuntime>,
    handler_rx: Mutex<Receiver<SwbusMessage>>,
    source: ServicePath,
    id_generator: MessageIdGenerator,
    sink: bool,
}

impl SimpleSwbusEdgeClient {
    /// Create and connect a new client.
    ///
    /// `public` determines whether the client is registered using [`SwbusEdgeRuntime::add_handler`] or [`SwbusEdgeRuntime::add_private_handler`].
    pub fn new(rt: Arc<SwbusEdgeRuntime>, source: ServicePath, public: bool, sink: bool) -> Self {
        let (handler_tx, handler_rx) = channel::<SwbusMessage>(crate::edge_runtime::SWBUS_RECV_QUEUE_SIZE);
        if public {
            rt.add_handler(source.clone(), handler_tx);
        } else {
            rt.add_private_handler(source.clone(), handler_tx);
        }

        Self {
            rt,
            handler_rx: Mutex::new(handler_rx),
            source,
            id_generator: MessageIdGenerator::new(),
            sink,
        }
    }

    pub fn get_edge_runtime(&self) -> &Arc<SwbusEdgeRuntime> {
        &self.rt
    }

    /// Receive a message.
    ///
    /// Returns `None` when no more messages will ever be received.
    pub async fn recv(&self) -> Option<IncomingMessage> {
        loop {
            let msg = self.handler_rx.lock().await.recv().await?;
            match self.handle_received_message(msg) {
                HandleReceivedMessage::PassToActor(msg) => break Some(msg),
                HandleReceivedMessage::Respond(msg) => self.rt.send(msg).await.unwrap(),
                HandleReceivedMessage::Ignore => {}
            }
        }
    }

    fn handle_received_message(&self, msg: SwbusMessage) -> HandleReceivedMessage {
        let header = msg.header.unwrap();
        let id = header.id;
        let source = header.source.unwrap();
        let destination = header.destination.unwrap();
        let body = msg.body.unwrap();

        if self.sink && destination != self.source {
            // sink will drop all messages not to itself and reply with NoRoute
            return HandleReceivedMessage::Respond(SwbusMessage::new(
                SwbusMessageHeader::new(self.source.clone(), source, self.id_generator.generate()),
                Body::Response(RequestResponse::infra_error(
                    id,
                    SwbusErrorCode::NoRoute,
                    "Route not found",
                )),
            ));
        }

        match body {
            Body::DataRequest(DataRequest { payload }) => HandleReceivedMessage::PassToActor(IncomingMessage {
                id,
                source,
                destination,
                body: MessageBody::Request { payload },
            }),
            Body::Response(RequestResponse {
                request_id,
                error_code,
                error_message,
                ..
            }) => HandleReceivedMessage::PassToActor(IncomingMessage {
                id,
                source,
                destination,
                body: MessageBody::Response {
                    request_id,
                    error_code: SwbusErrorCode::try_from(error_code).unwrap_or(SwbusErrorCode::UnknownError),
                    error_message,
                    response_body: None,
                },
            }),
            Body::PingRequest(_) => HandleReceivedMessage::Respond(SwbusMessage::new(
                SwbusMessageHeader::new(destination, source, self.id_generator.generate()),
                Body::Response(RequestResponse::ok(id)),
            )),
            Body::TraceRouteRequest(_) => HandleReceivedMessage::Respond(SwbusMessage::new(
                SwbusMessageHeader::new(destination, source, self.id_generator.generate()),
                Body::Response(RequestResponse::ok(id)),
            )),
            Body::ManagementRequest(ManagementRequest { request, arguments }) => {
                let request_type = match ManagementRequestType::try_from(request) {
                    Ok(request_type) => request_type,
                    Err(_) => {
                        // TODO: Log error
                        return HandleReceivedMessage::Ignore;
                    }
                };
                HandleReceivedMessage::PassToActor(IncomingMessage {
                    id,
                    source,
                    destination,
                    body: MessageBody::ManagementRequest {
                        request: request_type,
                        args: arguments
                            .iter()
                            .map(|arg| (arg.name.clone(), arg.value.clone()))
                            .collect(),
                    },
                })
            }
            _ => HandleReceivedMessage::Ignore,
        }
    }

    /// Send a message.
    pub async fn send(&self, msg: OutgoingMessage) -> Result<MessageId> {
        let (id, msg) = self.outgoing_message_to_swbus_message(msg);
        self.send_raw(msg).await?;
        Ok(id)
    }

    /// Send a raw [`SwbusMessage`].
    ///
    /// The message should be created with [`outgoing_message_to_swbus_message`](Self::outgoing_message_to_swbus_message).
    /// Otherwise, message ids will be inconsistent and may collide.
    ///
    /// This method is intended to be used to implement message resending - repeating a message with the same id.
    pub async fn send_raw(&self, msg: SwbusMessage) -> Result<()> {
        self.rt.send(msg).await
    }

    /// Compile an [`OutgoingMessage`] into an [`SwbusMessage`] for use with [`send_raw`](Self::send_raw).
    pub fn outgoing_message_to_swbus_message(&self, msg: OutgoingMessage) -> (MessageId, SwbusMessage) {
        let id = self.id_generator.generate();
        let msg = SwbusMessage {
            header: Some(SwbusMessageHeader::new(self.source.clone(), msg.destination, id)),
            body: Some(match msg.body {
                MessageBody::Request { payload } => Body::DataRequest(DataRequest { payload }),
                MessageBody::Response {
                    request_id,
                    error_code,
                    error_message,
                    response_body,
                } => {
                    let response_body = response_body.map(|MessageResponseBody::ManagementQueryResult { payload }| {
                        ResponseBody::ManagementQueryResult(ManagementQueryResult { value: payload })
                    });

                    Body::Response(RequestResponse {
                        request_id,
                        error_code: error_code.into(),
                        error_message,
                        response_body,
                    })
                }
                MessageBody::ManagementRequest { .. } => unimplemented!(),
            }),
        };
        (id, msg)
    }

    pub fn get_service_path(self: &Arc<Self>) -> &ServicePath {
        &self.source
    }
}

#[allow(clippy::large_enum_variant)]
enum HandleReceivedMessage {
    PassToActor(IncomingMessage),
    Respond(SwbusMessage),
    Ignore,
}

/// A simplified version of [`Body`], that excludes infra messages.
#[derive(Debug, Clone)]
pub enum MessageBody {
    Request {
        payload: Vec<u8>,
    },
    Response {
        request_id: MessageId,
        error_code: SwbusErrorCode,
        error_message: String,
        response_body: Option<MessageResponseBody>,
    },
    ManagementRequest {
        request: ManagementRequestType,
        args: HashMap<String, String>,
    },
}

#[derive(Debug, Clone)]
pub enum MessageResponseBody {
    ManagementQueryResult { payload: String },
}

/// A message received from another Swbus client.
#[derive(Debug, Clone)]
pub struct IncomingMessage {
    pub id: MessageId,
    pub source: ServicePath,
    pub destination: ServicePath,
    pub body: MessageBody,
}

/// A message to send to another Swbus client.
#[derive(Debug, Clone)]
pub struct OutgoingMessage {
    pub destination: ServicePath,
    pub body: MessageBody,
}
