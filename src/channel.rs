use crate::{
    acknowledgement::{Acknowledgements, DeliveryTag},
    auth::Credentials,
    channel_status::{ChannelState, ChannelStatus},
    close_on_drop,
    connection_status::ConnectionState,
    consumer::Consumer,
    executor::{Executor, ExecutorExt},
    frames::{ExpectedReply, Frames},
    id_sequence::IdSequence,
    internal_rpc::InternalRPCHandle,
    message::{BasicGetMessage, BasicReturnMessage, Delivery},
    protocol::{self, AMQPClass, AMQPError, AMQPHardError, AMQPSoftError},
    publisher_confirm::PublisherConfirm,
    queue::Queue,
    queues::Queues,
    returned_messages::ReturnedMessages,
    types::*,
    waker::Waker,
    BasicProperties, CloseOnDrop, Configuration, ConfirmationPromise, Connection, ConnectionStatus,
    Error, ExchangeKind, Promise, PromiseChain, PromiseResolver, Result,
};
use amq_protocol::frame::{AMQPContentHeader, AMQPFrame};
use log::{debug, error, info, log_enabled, trace, Level::Trace};
use parking_lot::Mutex;
use std::{convert::TryFrom, fmt, sync::Arc};

#[cfg(test)]
use crate::queue::QueueState;

#[derive(Clone)]
pub struct Channel {
    id: u16,
    configuration: Configuration,
    status: ChannelStatus,
    connection_status: ConnectionStatus,
    acknowledgements: Acknowledgements,
    delivery_tag: IdSequence<DeliveryTag>,
    queues: Queues,
    returned_messages: ReturnedMessages,
    waker: Waker,
    internal_rpc: InternalRPCHandle,
    frames: Frames,
    executor: Arc<dyn Executor>,
}

impl fmt::Debug for Channel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Channel")
            .field("id", &self.id)
            .field("configuration", &self.configuration)
            .field("status", &self.status)
            .field("connection_status", &self.connection_status)
            .field("acknowledgements", &self.acknowledgements)
            .field("delivery_tag", &self.delivery_tag)
            .field("queues", &self.queues)
            .field("returned_messages", &self.returned_messages)
            .field("waker", &self.waker)
            .field("frames", &self.frames)
            .field("executor", &self.executor)
            .finish()
    }
}

impl Channel {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        channel_id: u16,
        configuration: Configuration,
        connection_status: ConnectionStatus,
        waker: Waker,
        internal_rpc: InternalRPCHandle,
        frames: Frames,
        executor: Arc<dyn Executor>,
    ) -> Channel {
        let returned_messages = ReturnedMessages::default();
        Channel {
            id: channel_id,
            configuration,
            status: ChannelStatus::default(),
            connection_status,
            acknowledgements: Acknowledgements::new(returned_messages.clone()),
            delivery_tag: IdSequence::new(false),
            queues: Queues::default(),
            returned_messages,
            waker,
            internal_rpc,
            frames,
            executor,
        }
    }

    pub fn status(&self) -> &ChannelStatus {
        &self.status
    }

    fn set_closed(&self, error: Error) -> Result<()> {
        self.set_state(ChannelState::Closed);
        self.error_publisher_confirms(error.clone());
        self.cancel_consumers()
            .and(self.internal_rpc.remove_channel(self.id, error))
    }

    fn set_error(&self, error: Error) -> Result<()> {
        self.set_state(ChannelState::Error);
        self.error_publisher_confirms(error.clone());
        self.error_consumers(error.clone())
            .and(self.internal_rpc.remove_channel(self.id, error))
    }

    fn register_internal_promise(&self, promise: Promise<()>) -> Result<()> {
        self.executor
            .spawn_internal(promise, self.internal_rpc.clone())
    }

    pub(crate) fn error_publisher_confirms(&self, error: Error) {
        self.acknowledgements.on_channel_error(self.id, error);
    }

    pub(crate) fn cancel_consumers(&self) -> Result<()> {
        self.queues.cancel_consumers()
    }

    pub(crate) fn error_consumers(&self, error: Error) -> Result<()> {
        self.queues.error_consumers(error)
    }

    pub(crate) fn set_state(&self, state: ChannelState) {
        self.status.set_state(state);
    }

    pub fn id(&self) -> u16 {
        self.id
    }

    fn wake(&self) -> Result<()> {
        trace!("channel {} wake", self.id);
        self.waker.wake()
    }

    fn assert_channel0(&self, class_id: u16, method_id: u16) -> Result<()> {
        if self.id == 0 {
            Ok(())
        } else {
            error!(
                "Got a connection frame on channel {}, closing connection",
                self.id
            );
            let error = AMQPError::new(
                AMQPHardError::COMMANDINVALID.into(),
                format!("connection frame received on channel {}", self.id).into(),
            );
            self.internal_rpc.close_connection(
                error.get_id(),
                error.get_message().to_string(),
                class_id,
                method_id,
            )?;
            Err(Error::ProtocolError(error))
        }
    }

    pub fn close(&self, reply_code: ShortUInt, reply_text: &str) -> Promise<()> {
        self.do_channel_close(reply_code, reply_text, 0, 0)
    }

    pub fn exchange_declare(
        &self,
        exchange: &str,
        kind: ExchangeKind,
        options: ExchangeDeclareOptions,
        arguments: FieldTable,
    ) -> Promise<()> {
        self.do_exchange_declare(exchange, kind.kind(), options, arguments)
    }

    pub fn wait_for_confirms(&self) -> ConfirmationPromise<Vec<BasicReturnMessage>> {
        if let Some(promise) = self.acknowledgements.get_last_pending() {
            trace!("Waiting for pending confirms");
            let returned_messages = self.returned_messages.clone();
            promise.traverse(move |_| Ok(returned_messages.drain()))
        } else {
            trace!("No confirms to wait for");
            ConfirmationPromise::new_with_data(Ok(Vec::default()))
        }
    }

    #[cfg(test)]
    pub(crate) fn register_queue(&self, queue: QueueState) {
        self.queues.register(queue);
    }

    pub(crate) fn send_method_frame(
        &self,
        method: AMQPClass,
        resolver: PromiseResolver<()>,
        expected_reply: Option<ExpectedReply>,
    ) -> Result<()> {
        self.send_frame(AMQPFrame::Method(self.id, method), resolver, expected_reply)
    }

    pub(crate) fn send_frame(
        &self,
        frame: AMQPFrame,
        resolver: PromiseResolver<()>,
        expected_reply: Option<ExpectedReply>,
    ) -> Result<()> {
        trace!("channel {} send_frame", self.id);
        self.frames.push(self.id, frame, resolver, expected_reply);
        self.wake()
    }

    fn send_method_frame_with_body(
        &self,
        method: AMQPClass,
        payload: Vec<u8>,
        properties: BasicProperties,
        publisher_confirms_result: Option<PublisherConfirm>,
    ) -> Result<PromiseChain<PublisherConfirm>> {
        let class_id = method.get_amqp_class_id();
        let header = AMQPContentHeader {
            class_id,
            weight: 0,
            body_size: payload.len() as u64,
            properties,
        };
        let frame_max = self.configuration.frame_max();
        let mut frames = vec![
            AMQPFrame::Method(self.id, method),
            AMQPFrame::Header(self.id, class_id, Box::new(header)),
        ];

        // a content body frame 8 bytes of overhead
        frames.extend(
            payload
                .as_slice()
                .chunks(frame_max as usize - 8)
                .map(|chunk| AMQPFrame::Body(self.id, chunk.into())),
        );

        // tweak to make rustc happy
        let data = Arc::new(Mutex::new((
            publisher_confirms_result,
            self.returned_messages.clone(),
        )));

        trace!("channel {} send_frames", self.id);
        let promise = self.frames.push_frames(frames);
        self.wake()?;
        Ok(promise.traverse(move |res| {
            res.map(|()| {
                let mut data = data.lock();
                data.0
                    .take()
                    .unwrap_or_else(|| PublisherConfirm::not_requested(data.1.clone()))
            })
        }))
    }

    pub(crate) fn handle_content_header_frame(
        &self,
        size: u64,
        properties: BasicProperties,
    ) -> Result<()> {
        if let ChannelState::WillReceiveContent(queue_name, request_id_or_consumer_tag) =
            self.status.state()
        {
            if size > 0 {
                self.set_state(ChannelState::ReceivingContent(
                    queue_name.clone(),
                    request_id_or_consumer_tag.clone(),
                    size as usize,
                ));
            } else {
                self.set_state(ChannelState::Connected);
            }
            if let Some(queue_name) = queue_name {
                self.queues.handle_content_header_frame(
                    queue_name.as_str(),
                    request_id_or_consumer_tag,
                    size,
                    properties,
                )?;
            } else {
                self.returned_messages.set_delivery_properties(properties);
                if size == 0 {
                    self.returned_messages
                        .new_delivery_complete(self.status.confirm());
                }
            }
            Ok(())
        } else {
            self.set_error(Error::InvalidChannelState(self.status.state()))
        }
    }

    pub(crate) fn handle_body_frame(&self, payload: Vec<u8>) -> Result<()> {
        let payload_size = payload.len();

        if let ChannelState::ReceivingContent(
            queue_name,
            request_id_or_consumer_tag,
            remaining_size,
        ) = self.status.state()
        {
            if remaining_size >= payload_size {
                if let Some(queue_name) = queue_name.as_ref() {
                    self.queues.handle_body_frame(
                        queue_name.as_str(),
                        request_id_or_consumer_tag.clone(),
                        remaining_size,
                        payload_size,
                        payload,
                    )?;
                } else {
                    self.returned_messages.receive_delivery_content(payload);
                    if remaining_size == payload_size {
                        self.returned_messages
                            .new_delivery_complete(self.status.confirm());
                    }
                }
                if remaining_size == payload_size {
                    self.set_state(ChannelState::Connected);
                } else {
                    self.set_state(ChannelState::ReceivingContent(
                        queue_name,
                        request_id_or_consumer_tag,
                        remaining_size - payload_size,
                    ));
                }
                Ok(())
            } else {
                error!("body frame too large");
                self.set_error(Error::InvalidBodyReceived)
            }
        } else {
            self.set_error(Error::InvalidChannelState(self.status.state()))
        }
    }

    fn before_basic_publish(&self) -> Option<PublisherConfirm> {
        if self.status.confirm() {
            let delivery_tag = self.delivery_tag.next();
            Some(
                self.acknowledgements
                    .register_pending(delivery_tag, self.id),
            )
        } else {
            None
        }
    }

    fn acknowledgement_error(&self, error: Error, class_id: u16, method_id: u16) -> Result<()> {
        error!("Got a bad acknowledgement from server, closing channel");
        self.register_internal_promise(self.do_channel_close(
            AMQPSoftError::PRECONDITIONFAILED.get_id(),
            "precondition failed",
            class_id,
            method_id,
        ))?;
        Err(error)
    }

    fn on_connection_start_ok_sent(
        &self,
        resolver: PromiseResolver<CloseOnDrop<Connection>>,
        connection: Connection,
        credentials: Credentials,
    ) -> Result<()> {
        self.connection_status
            .set_state(ConnectionState::SentStartOk(
                resolver,
                connection,
                credentials,
            ));
        Ok(())
    }

    fn on_connection_open_sent(
        &self,
        resolver: PromiseResolver<CloseOnDrop<Connection>>,
    ) -> Result<()> {
        self.connection_status
            .set_state(ConnectionState::SentOpen(resolver));
        Ok(())
    }

    fn on_connection_close_sent(&self) -> Result<()> {
        self.internal_rpc.set_connection_closing()
    }

    fn on_connection_close_ok_sent(&self, error: Error) -> Result<()> {
        if let Error::ProtocolError(_) = error {
            self.internal_rpc.set_connection_error(error)
        } else {
            self.internal_rpc.set_connection_closed(error)
        }
    }

    fn before_channel_close(&self) {
        self.set_state(ChannelState::Closing);
    }

    fn on_channel_close_ok_sent(&self, error: Error) -> Result<()> {
        self.set_closed(error)
    }

    fn on_basic_recover_async_sent(&self) -> Result<()> {
        self.queues.drop_prefetched_messages()
    }

    fn on_basic_ack_sent(&self, multiple: bool, delivery_tag: DeliveryTag) -> Result<()> {
        if multiple && delivery_tag == 0 {
            self.queues.drop_prefetched_messages()
        } else {
            Ok(())
        }
    }

    fn on_basic_nack_sent(&self, multiple: bool, delivery_tag: DeliveryTag) -> Result<()> {
        if multiple && delivery_tag == 0 {
            self.queues.drop_prefetched_messages()
        } else {
            Ok(())
        }
    }

    fn tune_connection_configuration(&self, channel_max: u16, frame_max: u32, heartbeat: u16) {
        // If we disable the heartbeat (0) but the server don't, follow it and enable it too
        // If both us and the server want heartbeat enabled, pick the lowest value.
        if self.configuration.heartbeat() == 0
            || (heartbeat != 0 && heartbeat < self.configuration.heartbeat())
        {
            self.configuration.set_heartbeat(heartbeat);
        }

        if channel_max != 0 {
            // 0 means we want to take the server's value
            // If both us and the server specified a channel_max, pick the lowest value.
            if self.configuration.channel_max() == 0
                || channel_max < self.configuration.channel_max()
            {
                self.configuration.set_channel_max(channel_max);
            }
        }
        if self.configuration.channel_max() == 0 {
            self.configuration.set_channel_max(u16::max_value());
        }

        if frame_max != 0 {
            // 0 means we want to take the server's value
            // If both us and the server specified a frame_max, pick the lowest value.
            if self.configuration.frame_max() == 0 || frame_max < self.configuration.frame_max() {
                self.configuration.set_frame_max(frame_max);
            }
        }
        if self.configuration.frame_max() == 0 {
            self.configuration.set_frame_max(u32::max_value());
        }
    }

    fn on_connection_start_received(&self, method: protocol::connection::Start) -> Result<()> {
        trace!("Server sent connection::Start: {:?}", method);
        let state = self.connection_status.state();
        if let ConnectionState::SentProtocolHeader(
            resolver,
            connection,
            credentials,
            mechanism,
            mut options,
        ) = state
        {
            let mechanism_str = mechanism.to_string();
            let locale = options.locale.clone();

            if !method
                .mechanisms
                .split_whitespace()
                .any(|m| m == mechanism_str)
            {
                error!("unsupported mechanism: {}", mechanism);
            }
            if !method.locales.split_whitespace().any(|l| l == locale) {
                error!("unsupported locale: {}", mechanism);
            }

            if !options.client_properties.contains_key("product")
                || !options.client_properties.contains_key("version")
            {
                options.client_properties.insert(
                    "product".into(),
                    AMQPValue::LongString(env!("CARGO_PKG_NAME").into()),
                );
                options.client_properties.insert(
                    "version".into(),
                    AMQPValue::LongString(env!("CARGO_PKG_VERSION").into()),
                );
            }

            options
                .client_properties
                .insert("platform".into(), AMQPValue::LongString("rust".into()));

            let mut capabilities = FieldTable::default();
            capabilities.insert("publisher_confirms".into(), AMQPValue::Boolean(true));
            capabilities.insert(
                "exchange_exchange_bindings".into(),
                AMQPValue::Boolean(true),
            );
            capabilities.insert("basic.nack".into(), AMQPValue::Boolean(true));
            capabilities.insert("consumer_cancel_notify".into(), AMQPValue::Boolean(true));
            capabilities.insert("connection.blocked".into(), AMQPValue::Boolean(true));
            // FIXME: consumer_priorities
            capabilities.insert(
                "authentication_failure_close".into(),
                AMQPValue::Boolean(true),
            );
            // FIXME: per_consumer_qos
            // FIXME: direct_reply_to

            options
                .client_properties
                .insert("capabilities".into(), AMQPValue::FieldTable(capabilities));

            self.register_internal_promise(self.connection_start_ok(
                options.client_properties,
                &mechanism_str,
                &credentials.sasl_auth_string(mechanism),
                &locale,
                resolver,
                connection,
                credentials,
            ))
        } else {
            error!("Invalid state: {:?}", state);
            let error = Error::InvalidConnectionState(state);
            self.internal_rpc.set_connection_error(error.clone())?;
            Err(error)
        }
    }

    fn on_connection_secure_received(&self, method: protocol::connection::Secure) -> Result<()> {
        trace!("Server sent connection::Secure: {:?}", method);

        let state = self.connection_status.state();
        if let ConnectionState::SentStartOk(_, _, credentials) = state {
            self.register_internal_promise(
                self.connection_secure_ok(&credentials.rabbit_cr_demo_answer()),
            )
        } else {
            error!("Invalid state: {:?}", state);
            let error = Error::InvalidConnectionState(state);
            self.internal_rpc.set_connection_error(error.clone())?;
            Err(error)
        }
    }

    fn on_connection_tune_received(&self, method: protocol::connection::Tune) -> Result<()> {
        debug!("Server sent Connection::Tune: {:?}", method);

        let state = self.connection_status.state();
        if let ConnectionState::SentStartOk(resolver, connection, _) = state {
            self.tune_connection_configuration(
                method.channel_max,
                method.frame_max,
                method.heartbeat,
            );

            self.register_internal_promise(self.connection_tune_ok(
                self.configuration.channel_max(),
                self.configuration.frame_max(),
                self.configuration.heartbeat(),
            ))?;
            self.register_internal_promise(self.connection_open(
                &self.connection_status.vhost(),
                connection,
                resolver,
            ))
        } else {
            error!("Invalid state: {:?}", state);
            let error = Error::InvalidConnectionState(state);
            self.internal_rpc.set_connection_error(error.clone())?;
            Err(error)
        }
    }

    fn on_connection_open_ok_received(
        &self,
        _: protocol::connection::OpenOk,
        connection: Connection,
    ) -> Result<()> {
        let state = self.connection_status.state();
        if let ConnectionState::SentOpen(resolver) = state {
            self.connection_status.set_state(ConnectionState::Connected);
            resolver.swear(Ok(CloseOnDrop::new(connection)));
            Ok(())
        } else {
            error!("Invalid state: {:?}", state);
            let error = Error::InvalidConnectionState(state);
            self.internal_rpc.set_connection_error(error.clone())?;
            Err(error)
        }
    }

    fn on_connection_close_received(&self, method: protocol::connection::Close) -> Result<()> {
        let error = AMQPError::try_from(method.clone())
            .map(|error| {
                error!(
                    "Connection closed on channel {} by {}:{} => {:?} => {}",
                    self.id, method.class_id, method.method_id, error, method.reply_text
                );
                Error::ProtocolError(error)
            })
            .unwrap_or_else(|error| {
                error!("{}", error);
                info!("Connection closed on channel {}: {:?}", self.id, method);
                Error::InvalidConnectionState(ConnectionState::Closed)
            });
        let state = self.connection_status.state();
        self.internal_rpc.set_connection_closing()?;
        self.frames.drop_pending(error.clone());
        match state {
            ConnectionState::SentProtocolHeader(resolver, ..) => resolver.swear(Err(error.clone())),
            ConnectionState::SentStartOk(resolver, ..) => resolver.swear(Err(error.clone())),
            ConnectionState::SentOpen(resolver) => resolver.swear(Err(error.clone())),
            _ => {}
        }
        self.internal_rpc.send_connection_close_ok(error)
    }

    fn on_connection_blocked_received(&self, _method: protocol::connection::Blocked) -> Result<()> {
        self.connection_status.block();
        Ok(())
    }

    fn on_connection_unblocked_received(
        &self,
        _method: protocol::connection::Unblocked,
    ) -> Result<()> {
        self.connection_status.unblock();
        self.wake()
    }

    fn on_connection_close_ok_received(&self) -> Result<()> {
        self.internal_rpc
            .set_connection_closed(Error::InvalidConnectionState(ConnectionState::Closed))
    }

    fn on_channel_open_ok_received(
        &self,
        _method: protocol::channel::OpenOk,
        resolver: PromiseResolver<CloseOnDrop<Channel>>,
    ) -> Result<()> {
        self.set_state(ChannelState::Connected);
        resolver.swear(Ok(CloseOnDrop::new(self.clone())));
        Ok(())
    }

    fn on_channel_flow_received(&self, method: protocol::channel::Flow) -> Result<()> {
        self.status.set_send_flow(method.active);
        self.register_internal_promise(self.channel_flow_ok(ChannelFlowOkOptions {
            active: method.active,
        }))
    }

    fn on_channel_flow_ok_received(
        &self,
        method: protocol::channel::FlowOk,
        resolver: PromiseResolver<Boolean>,
    ) -> Result<()> {
        // Nothing to do here, the server just confirmed that we paused/resumed the receiving flow
        resolver.swear(Ok(method.active));
        Ok(())
    }

    fn on_channel_close_received(&self, method: protocol::channel::Close) -> Result<()> {
        let error = AMQPError::try_from(method.clone())
            .map(|error| {
                error!(
                    "Channel closed on channel {} by {}:{} => {:?} => {}",
                    self.id, method.class_id, method.method_id, error, method.reply_text
                );
                Error::ProtocolError(error)
            })
            .unwrap_or_else(|error| {
                error!("{}", error);
                info!("Channel closed on channel {}: {:?}", self.id, method);
                Error::InvalidChannelState(ChannelState::Closing)
            });
        self.set_state(ChannelState::Closing);
        self.register_internal_promise(self.channel_close_ok(error))
    }

    fn on_channel_close_ok_received(&self) -> Result<()> {
        self.set_closed(Error::InvalidChannelState(ChannelState::Closed))
    }

    fn on_queue_delete_ok_received(
        &self,
        method: protocol::queue::DeleteOk,
        resolver: PromiseResolver<LongUInt>,
        queue: ShortString,
    ) -> Result<()> {
        self.queues.deregister(queue.as_str());
        resolver.swear(Ok(method.message_count));
        Ok(())
    }

    fn on_queue_purge_ok_received(
        &self,
        method: protocol::queue::PurgeOk,
        resolver: PromiseResolver<LongUInt>,
    ) -> Result<()> {
        resolver.swear(Ok(method.message_count));
        Ok(())
    }

    fn on_queue_declare_ok_received(
        &self,
        method: protocol::queue::DeclareOk,
        resolver: PromiseResolver<Queue>,
    ) -> Result<()> {
        let queue = Queue::new(method.queue, method.message_count, method.consumer_count);
        self.queues.register(queue.clone().into());
        resolver.swear(Ok(queue));
        Ok(())
    }

    fn on_basic_get_ok_received(
        &self,
        method: protocol::basic::GetOk,
        resolver: PromiseResolver<Option<BasicGetMessage>>,
        queue: ShortString,
    ) -> Result<()> {
        self.queues.start_basic_get_delivery(
            queue.as_str(),
            BasicGetMessage::new(
                method.delivery_tag,
                method.exchange,
                method.routing_key,
                method.redelivered,
                method.message_count,
            ),
            resolver,
        );
        self.set_state(ChannelState::WillReceiveContent(Some(queue), None));
        Ok(())
    }

    fn on_basic_get_empty_received(&self, _: protocol::basic::GetEmpty) -> Result<()> {
        match self.frames.next_expected_reply(self.id) {
            Some(Reply::BasicGetOk(resolver, _)) => {
                resolver.swear(Ok(None));
                Ok(())
            }
            _ => {
                self.set_error(Error::UnexpectedReply)?;
                Err(Error::UnexpectedReply)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn on_basic_consume_ok_received(
        &self,
        method: protocol::basic::ConsumeOk,
        resolver: PromiseResolver<Consumer>,
        queue: ShortString,
    ) -> Result<()> {
        let consumer = Consumer::new(method.consumer_tag.clone(), self.executor.clone());
        self.queues
            .register_consumer(queue.as_str(), method.consumer_tag, consumer.clone());
        resolver.swear(Ok(consumer));
        Ok(())
    }

    fn on_basic_deliver_received(&self, method: protocol::basic::Deliver) -> Result<()> {
        if let Some(queue_name) = self.queues.start_consumer_delivery(
            method.consumer_tag.as_str(),
            Delivery::new(
                method.delivery_tag,
                method.exchange,
                method.routing_key,
                method.redelivered,
            ),
        ) {
            self.set_state(ChannelState::WillReceiveContent(
                Some(queue_name),
                Some(method.consumer_tag),
            ));
        }
        Ok(())
    }

    fn on_basic_cancel_received(&self, method: protocol::basic::Cancel) -> Result<()> {
        self.queues
            .deregister_consumer(method.consumer_tag.as_str())
            .and(if !method.nowait {
                self.register_internal_promise(self.basic_cancel_ok(method.consumer_tag.as_str()))
            } else {
                Ok(())
            })
    }

    fn on_basic_cancel_ok_received(&self, method: protocol::basic::CancelOk) -> Result<()> {
        self.queues
            .deregister_consumer(method.consumer_tag.as_str())
    }

    fn on_basic_ack_received(&self, method: protocol::basic::Ack) -> Result<()> {
        if self.status.confirm() {
            if method.multiple {
                if method.delivery_tag > 0 {
                    self.acknowledgements
                        .ack_all_before(method.delivery_tag)
                        .or_else(|err| {
                            self.acknowledgement_error(
                                err,
                                method.get_amqp_class_id(),
                                method.get_amqp_method_id(),
                            )
                        })?;
                } else {
                    self.acknowledgements.ack_all_pending();
                }
            } else {
                self.acknowledgements
                    .ack(method.delivery_tag)
                    .or_else(|err| {
                        self.acknowledgement_error(
                            err,
                            method.get_amqp_class_id(),
                            method.get_amqp_method_id(),
                        )
                    })?;
            }
        }
        Ok(())
    }

    fn on_basic_nack_received(&self, method: protocol::basic::Nack) -> Result<()> {
        if self.status.confirm() {
            if method.multiple {
                if method.delivery_tag > 0 {
                    self.acknowledgements
                        .nack_all_before(method.delivery_tag)
                        .or_else(|err| {
                            self.acknowledgement_error(
                                err,
                                method.get_amqp_class_id(),
                                method.get_amqp_method_id(),
                            )
                        })?;
                } else {
                    self.acknowledgements.nack_all_pending();
                }
            } else {
                self.acknowledgements
                    .nack(method.delivery_tag)
                    .or_else(|err| {
                        self.acknowledgement_error(
                            err,
                            method.get_amqp_class_id(),
                            method.get_amqp_method_id(),
                        )
                    })?;
            }
        }
        Ok(())
    }

    fn on_basic_return_received(&self, method: protocol::basic::Return) -> Result<()> {
        self.returned_messages
            .start_new_delivery(BasicReturnMessage::new(
                method.exchange,
                method.routing_key,
                method.reply_code,
                method.reply_text,
            ));
        self.set_state(ChannelState::WillReceiveContent(None, None));
        Ok(())
    }

    fn on_basic_recover_ok_received(&self) -> Result<()> {
        self.queues.drop_prefetched_messages()
    }

    fn on_confirm_select_ok_received(&self) -> Result<()> {
        self.status.set_confirm();
        Ok(())
    }

    fn on_access_request_ok_received(&self, _: protocol::access::RequestOk) -> Result<()> {
        Ok(())
    }
}

impl close_on_drop::__private::Closable for Channel {
    fn close(&self, reply_code: ShortUInt, reply_text: &str) -> Promise<()> {
        if self.status().is_connected() {
            Channel::close(self, reply_code, reply_text)
        } else {
            Promise::new_with_data(Ok(()))
        }
    }
}

include!(concat!(env!("OUT_DIR"), "/channel.rs"));
