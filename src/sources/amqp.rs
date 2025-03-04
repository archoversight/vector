//! `AMQP` source.
//! Handles version AMQP 0.9.1 which is used by RabbitMQ.
use crate::{
    amqp::AmqpConfig,
    codecs::{Decoder, DecodingConfig},
    config::{Output, SourceConfig, SourceContext},
    event::{BatchNotifier, BatchStatus},
    internal_events::{
        source::{AmqpAckError, AmqpBytesReceived, AmqpEventError, AmqpRejectError},
        StreamClosedError,
    },
    serde::{bool_or_struct, default_decoding, default_framing_message_based},
    shutdown::ShutdownSignal,
    SourceSender,
};
use async_stream::stream;
use bytes::Bytes;
use chrono::{TimeZone, Utc};
use codecs::decoding::{DeserializerConfig, FramingConfig};
use futures::{FutureExt, StreamExt};
use futures_util::Stream;
use lapin::{acker::Acker, message::Delivery, Channel};
use lookup::{metadata_path, owned_value_path, path, PathPrefix};
use snafu::Snafu;
use std::{io::Cursor, pin::Pin};
use tokio_util::codec::FramedRead;
use value::Kind;
use vector_common::{finalizer::UnorderedFinalizer, internal_event::EventsReceived};
use vector_config::{configurable_component, NamedComponent};
use vector_core::{
    config::{log_schema, LegacyKey, LogNamespace, SourceAcknowledgementsConfig},
    event::Event,
    ByteSizeOf,
};

#[derive(Debug, Snafu)]
enum BuildError {
    #[snafu(display("Could not create AMQP consumer: {}", source))]
    AmqpCreateError {
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[snafu(display("Could not subscribe to AMQP queue: {}", source))]
    AmqpSubscribeError { source: lapin::Error },
}

/// Configuration for the `amqp` source.
///
/// Supports AMQP version 0.9.1
#[configurable_component(source("amqp"))]
#[derive(Clone, Debug, Derivative)]
#[derivative(Default)]
#[serde(deny_unknown_fields)]
pub struct AmqpSourceConfig {
    /// The name of the queue to consume.
    #[serde(default = "default_queue")]
    pub(crate) queue: String,

    /// The identifier for the consumer.
    #[serde(default = "default_consumer")]
    pub(crate) consumer: String,

    /// Connection options for `AMQP` source.
    pub(crate) connection: AmqpConfig,

    /// The `AMQP` routing key.
    #[serde(default = "default_routing_key_field")]
    pub(crate) routing_key_field: String,

    /// The `AMQP` exchange key.
    #[serde(default = "default_exchange_key")]
    pub(crate) exchange_key: String,

    /// The `AMQP` offset key.
    #[serde(default = "default_offset_key")]
    pub(crate) offset_key: String,

    /// The namespace to use for logs. This overrides the global setting.
    #[configurable(metadata(docs::hidden))]
    #[serde(default)]
    pub log_namespace: Option<bool>,

    #[configurable(derived)]
    #[serde(default = "default_framing_message_based")]
    #[derivative(Default(value = "default_framing_message_based()"))]
    pub(crate) framing: FramingConfig,

    #[configurable(derived)]
    #[serde(default = "default_decoding")]
    #[derivative(Default(value = "default_decoding()"))]
    pub(crate) decoding: DeserializerConfig,

    #[configurable(derived)]
    #[serde(default, deserialize_with = "bool_or_struct")]
    pub(crate) acknowledgements: SourceAcknowledgementsConfig,
}

fn default_queue() -> String {
    "vector".into()
}

fn default_consumer() -> String {
    "vector".into()
}

fn default_routing_key_field() -> String {
    "routing".into()
}

fn default_exchange_key() -> String {
    "exchange".into()
}

fn default_offset_key() -> String {
    "offset".into()
}

impl_generate_config_from_default!(AmqpSourceConfig);

impl AmqpSourceConfig {
    fn decoder(&self, log_namespace: LogNamespace) -> Decoder {
        DecodingConfig::new(self.framing.clone(), self.decoding.clone(), log_namespace).build()
    }
}

#[async_trait::async_trait]
impl SourceConfig for AmqpSourceConfig {
    async fn build(&self, cx: SourceContext) -> crate::Result<super::Source> {
        let log_namespace = cx.log_namespace(self.log_namespace);
        let acknowledgements = cx.do_acknowledgements(self.acknowledgements);

        amqp_source(self, cx.shutdown, cx.out, log_namespace, acknowledgements).await
    }

    fn outputs(&self, global_log_namespace: LogNamespace) -> Vec<Output> {
        let log_namespace = global_log_namespace.merge(self.log_namespace);
        let schema_definition = self
            .decoding
            .schema_definition(log_namespace)
            .with_standard_vector_source_metadata()
            .with_source_metadata(
                AmqpSourceConfig::NAME,
                None,
                &owned_value_path!("timestamp"),
                Kind::timestamp(),
                Some("timestamp"),
            )
            .with_source_metadata(
                AmqpSourceConfig::NAME,
                Some(LegacyKey::Overwrite(owned_value_path!(
                    &self.routing_key_field
                ))),
                &owned_value_path!("routing"),
                Kind::bytes(),
                None,
            )
            .with_source_metadata(
                AmqpSourceConfig::NAME,
                Some(LegacyKey::Overwrite(owned_value_path!(&self.exchange_key))),
                &owned_value_path!("exchange"),
                Kind::bytes(),
                None,
            )
            .with_source_metadata(
                AmqpSourceConfig::NAME,
                Some(LegacyKey::Overwrite(owned_value_path!(&self.offset_key))),
                &owned_value_path!("offset"),
                Kind::integer(),
                None,
            );

        vec![Output::default(self.decoding.output_type()).with_schema_definition(schema_definition)]
    }

    fn can_acknowledge(&self) -> bool {
        true
    }
}

#[derive(Debug)]
struct FinalizerEntry {
    acker: Acker,
}

impl From<Delivery> for FinalizerEntry {
    fn from(delivery: Delivery) -> Self {
        Self {
            acker: delivery.acker,
        }
    }
}

pub(crate) async fn amqp_source(
    config: &AmqpSourceConfig,
    shutdown: ShutdownSignal,
    out: SourceSender,
    log_namespace: LogNamespace,
    acknowledgements: bool,
) -> crate::Result<super::Source> {
    let config = config.clone();
    let (_conn, channel) = config
        .connection
        .connect()
        .await
        .map_err(|source| BuildError::AmqpCreateError { source })?;

    Ok(Box::pin(run_amqp_source(
        config,
        shutdown,
        out,
        channel,
        log_namespace,
        acknowledgements,
    )))
}

struct Keys<'a> {
    routing_key_field: &'a str,
    routing: &'a str,
    exchange_key: &'a str,
    exchange: &'a str,
    offset_key: &'a str,
    delivery_tag: i64,
}

/// Populates the decoded event with extra metadata.
fn populate_event(
    event: &mut Event,
    timestamp: Option<chrono::DateTime<Utc>>,
    keys: &Keys<'_>,
    log_namespace: LogNamespace,
) {
    let log = event.as_mut_log();

    log_namespace.insert_source_metadata(
        AmqpSourceConfig::NAME,
        log,
        Some(LegacyKey::InsertIfEmpty(keys.routing_key_field)),
        "routing",
        keys.routing.to_string(),
    );

    log_namespace.insert_source_metadata(
        AmqpSourceConfig::NAME,
        log,
        Some(LegacyKey::InsertIfEmpty(keys.exchange_key)),
        "exchange",
        keys.exchange.to_string(),
    );

    log_namespace.insert_source_metadata(
        AmqpSourceConfig::NAME,
        log,
        Some(LegacyKey::InsertIfEmpty(keys.offset_key)),
        "offset",
        keys.delivery_tag,
    );

    log_namespace.insert_vector_metadata(
        log,
        path!(log_schema().source_type_key()),
        path!("source_type"),
        Bytes::from_static(AmqpSourceConfig::NAME.as_bytes()),
    );

    // This handles the transition from the original timestamp logic. Originally the
    // `timestamp_key` was populated by the `properties.timestamp()` time on the message, falling
    // back to calling `now()`.
    match log_namespace {
        LogNamespace::Vector => {
            if let Some(timestamp) = timestamp {
                log.insert(
                    metadata_path!(AmqpSourceConfig::NAME, "timestamp"),
                    timestamp,
                );
            };

            log.insert(metadata_path!("vector", "ingest_timestamp"), Utc::now());
        }
        LogNamespace::Legacy => {
            log.try_insert(
                (PathPrefix::Event, log_schema().timestamp_key()),
                timestamp.unwrap_or_else(Utc::now),
            );
        }
    };
}

/// Receives an event from `AMQP` and pushes it along the pipeline.
async fn receive_event(
    config: &AmqpSourceConfig,
    out: &mut SourceSender,
    log_namespace: LogNamespace,
    finalizer: Option<&UnorderedFinalizer<FinalizerEntry>>,
    msg: Delivery,
) -> Result<(), ()> {
    let payload = Cursor::new(Bytes::copy_from_slice(&msg.data));
    let mut stream = FramedRead::new(payload, config.decoder(log_namespace));

    // Extract timestamp from AMQP message
    let timestamp = msg
        .properties
        .timestamp()
        .and_then(|millis| Utc.timestamp_millis_opt(millis as _).latest());

    let routing = msg.routing_key.to_string();
    let exchange = msg.exchange.to_string();
    let keys = Keys {
        routing_key_field: config.routing_key_field.as_str(),
        exchange_key: config.exchange_key.as_str(),
        offset_key: config.offset_key.as_str(),
        routing: &routing,
        exchange: &exchange,
        delivery_tag: msg.delivery_tag as i64,
    };

    let stream = stream! {
        while let Some(result) = stream.next().await {
            match result {
                Ok((events, byte_size)) => {
                    emit!(AmqpBytesReceived {
                        byte_size,
                        protocol: "amqp_0_9_1",
                    });

                    emit!(EventsReceived {
                        byte_size: events.size_of(),
                        count: events.len(),
                    });

                    for mut event in events {
                        populate_event(&mut event,
                                       timestamp,
                                       &keys,
                                       log_namespace);

                        yield event;
                    }
                }
                Err(error) => {
                    use codecs::StreamDecodingError as _;

                    // Error is logged by `codecs::Decoder`, no further handling
                    // is needed here.
                    if !error.can_continue() {
                        break;
                    }
                }
            }
        }
    }
    .boxed();

    finalize_event_stream(finalizer, out, stream, msg).await;

    Ok(())
}

/// Send the event stream created by the framed read to the `out` stream.
async fn finalize_event_stream(
    finalizer: Option<&UnorderedFinalizer<FinalizerEntry>>,
    out: &mut SourceSender,
    mut stream: Pin<Box<dyn Stream<Item = Event> + Send + '_>>,
    msg: Delivery,
) {
    match finalizer {
        Some(finalizer) => {
            let (batch, receiver) = BatchNotifier::new_with_receiver();
            let mut stream = stream.map(|event| event.with_batch_notifier(&batch));

            match out.send_event_stream(&mut stream).await {
                Err(error) => {
                    emit!(StreamClosedError { error, count: 1 });
                }
                Ok(_) => {
                    finalizer.add(msg.into(), receiver);
                }
            }
        }
        None => match out.send_event_stream(&mut stream).await {
            Err(error) => {
                emit!(StreamClosedError { error, count: 1 });
            }
            Ok(_) => {
                let ack_options = lapin::options::BasicAckOptions::default();
                if let Err(error) = msg.acker.ack(ack_options).await {
                    emit!(AmqpAckError { error });
                }
            }
        },
    }
}

/// Runs the `AMQP` source involving the main loop pulling data from the server.
async fn run_amqp_source(
    config: AmqpSourceConfig,
    shutdown: ShutdownSignal,
    mut out: SourceSender,
    channel: Channel,
    log_namespace: LogNamespace,
    acknowledgements: bool,
) -> Result<(), ()> {
    let (finalizer, mut ack_stream) =
        UnorderedFinalizer::<FinalizerEntry>::maybe_new(acknowledgements, shutdown.clone());

    debug!("Starting amqp source, listening to queue {}.", config.queue);
    let mut consumer = channel
        .basic_consume(
            &config.queue,
            &config.consumer,
            lapin::options::BasicConsumeOptions::default(),
            lapin::types::FieldTable::default(),
        )
        .await
        .map_err(|error| {
            error!(message = "Failed to consume.", error = ?error, internal_log_rate_limit = true);
        })?
        .fuse();
    let mut shutdown = shutdown.fuse();
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            entry = ack_stream.next() => {
                if let Some((status, entry)) = entry {
                    handle_ack(status, entry).await;
                }
            },
            opt_m = consumer.next() => {
                if let Some(try_m) = opt_m {
                    match try_m {
                        Err(error) => {
                            emit!(AmqpEventError { error });
                            return Err(());
                        }
                        Ok(msg) => {
                            receive_event(&config, &mut out, log_namespace, finalizer.as_ref(), msg).await?
                        }
                    }
                } else {
                    break
                }
            }
        };
    }

    Ok(())
}

async fn handle_ack(status: BatchStatus, entry: FinalizerEntry) {
    match status {
        BatchStatus::Delivered => {
            let ack_options = lapin::options::BasicAckOptions::default();
            if let Err(error) = entry.acker.ack(ack_options).await {
                emit!(AmqpAckError { error });
            }
        }
        BatchStatus::Errored => {
            let ack_options = lapin::options::BasicRejectOptions::default();
            if let Err(error) = entry.acker.reject(ack_options).await {
                emit!(AmqpRejectError { error });
            }
        }
        BatchStatus::Rejected => {
            let ack_options = lapin::options::BasicRejectOptions::default();
            if let Err(error) = entry.acker.reject(ack_options).await {
                emit!(AmqpRejectError { error });
            }
        }
    }
}

#[cfg(test)]
pub mod test {
    use lookup::LookupBuf;
    use value::kind::Collection;
    use vector_core::schema::Definition;

    use super::*;

    #[test]
    fn generate_config() {
        crate::test_util::test_generate_config::<AmqpSourceConfig>();
    }

    pub fn make_config() -> AmqpSourceConfig {
        let mut config = AmqpSourceConfig {
            queue: "it".to_string(),
            ..Default::default()
        };
        let user = std::env::var("AMQP_USER").unwrap_or_else(|_| "guest".to_string());
        let pass = std::env::var("AMQP_PASSWORD").unwrap_or_else(|_| "guest".to_string());
        let vhost = std::env::var("AMQP_VHOST").unwrap_or_else(|_| "%2f".to_string());
        config.connection.connection_string =
            format!("amqp://{}:{}@rabbitmq:5672/{}", user, pass, vhost);
        config
    }

    #[test]
    fn output_schema_definition_vector_namespace() {
        let config = AmqpSourceConfig {
            log_namespace: Some(true),
            ..Default::default()
        };

        let definition = config.outputs(LogNamespace::Vector)[0]
            .clone()
            .log_schema_definition
            .unwrap();

        let expected_definition =
            Definition::new_with_default_metadata(Kind::bytes(), [LogNamespace::Vector])
                .with_meaning(LookupBuf::root(), "message")
                .with_metadata_field(&owned_value_path!("vector", "source_type"), Kind::bytes())
                .with_metadata_field(
                    &owned_value_path!("vector", "ingest_timestamp"),
                    Kind::timestamp(),
                )
                .with_metadata_field(&owned_value_path!("amqp", "timestamp"), Kind::timestamp())
                .with_metadata_field(&owned_value_path!("amqp", "routing"), Kind::bytes())
                .with_metadata_field(&owned_value_path!("amqp", "exchange"), Kind::bytes())
                .with_metadata_field(&owned_value_path!("amqp", "offset"), Kind::integer());

        assert_eq!(definition, expected_definition);
    }

    #[test]
    fn output_schema_definition_legacy_namespace() {
        let config = AmqpSourceConfig {
            routing_key_field: "routing".to_string(),
            exchange_key: "exchange".to_string(),
            offset_key: "offset".to_string(),
            ..Default::default()
        };

        let definition = config.outputs(LogNamespace::Legacy)[0]
            .clone()
            .log_schema_definition
            .unwrap();

        let expected_definition = Definition::new_with_default_metadata(
            Kind::object(Collection::empty()),
            [LogNamespace::Legacy],
        )
        .with_event_field(
            &owned_value_path!("message"),
            Kind::bytes(),
            Some("message"),
        )
        .with_event_field(&owned_value_path!("timestamp"), Kind::timestamp(), None)
        .with_event_field(&owned_value_path!("source_type"), Kind::bytes(), None)
        .with_event_field(&owned_value_path!("routing"), Kind::bytes(), None)
        .with_event_field(&owned_value_path!("exchange"), Kind::bytes(), None)
        .with_event_field(&owned_value_path!("offset"), Kind::integer(), None);

        assert_eq!(definition, expected_definition);
    }
}

/// Integration tests use the docker compose files in `scripts/integration/docker-compose.amqp.yml`.
#[cfg(feature = "amqp-integration-tests")]
#[cfg(test)]
mod integration_test {
    use super::test::*;
    use super::*;
    use crate::{
        shutdown::ShutdownSignal,
        test_util::{
            components::{run_and_assert_source_compliance, SOURCE_TAGS},
            random_string,
        },
        SourceSender,
    };
    use chrono::Utc;
    use lapin::options::*;
    use lapin::BasicProperties;
    use tokio::time::Duration;
    use vector_core::config::log_schema;

    #[tokio::test]
    async fn amqp_source_create_ok() {
        let config = make_config();
        assert!(amqp_source(
            &config,
            ShutdownSignal::noop(),
            SourceSender::new_test().0,
            LogNamespace::Legacy,
            false,
        )
        .await
        .is_ok());
    }

    async fn send_event(
        channel: &lapin::Channel,
        exchange: &str,
        routing_key: &str,
        text: &str,
        _timestamp: i64,
    ) {
        let payload = text.as_bytes();
        let payload_len = payload.len();
        trace!("Sending message of length {} to {}.", payload_len, exchange,);

        channel
            .basic_publish(
                exchange,
                routing_key,
                BasicPublishOptions::default(),
                payload.as_ref(),
                BasicProperties::default(),
            )
            .await
            .unwrap()
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn amqp_source_consume_event() {
        let exchange = format!("test-{}-exchange", random_string(10));
        let queue = format!("test-{}-queue", random_string(10));
        let routing_key = "my_key";
        trace!("Test exchange name: {}.", exchange);
        let consumer = format!("test-consumer-{}", random_string(10));

        let mut config = make_config();
        config.consumer = consumer;
        config.queue = queue;
        config.routing_key_field = "message_key".to_string();
        config.exchange_key = "exchange".to_string();
        let (_conn, channel) = config.connection.connect().await.unwrap();
        let exchange_opts = lapin::options::ExchangeDeclareOptions {
            auto_delete: true,
            ..Default::default()
        };

        channel
            .exchange_declare(
                &exchange,
                lapin::ExchangeKind::Fanout,
                exchange_opts,
                lapin::types::FieldTable::default(),
            )
            .await
            .unwrap();

        let queue_opts = QueueDeclareOptions {
            auto_delete: true,
            ..Default::default()
        };
        channel
            .queue_declare(
                &config.queue,
                queue_opts,
                lapin::types::FieldTable::default(),
            )
            .await
            .unwrap();

        channel
            .queue_bind(
                &config.queue,
                &exchange,
                "",
                lapin::options::QueueBindOptions::default(),
                lapin::types::FieldTable::default(),
            )
            .await
            .unwrap();

        trace!("Sending event...");
        let now = Utc::now();
        send_event(
            &channel,
            &exchange,
            routing_key,
            "my message",
            now.timestamp_millis(),
        )
        .await;

        trace!("Receiving event...");
        let events =
            run_and_assert_source_compliance(config, Duration::from_secs(1), &SOURCE_TAGS).await;
        assert!(!events.is_empty());

        let log = events[0].as_log();
        trace!("{:?}", log);
        assert_eq!(log[log_schema().message_key()], "my message".into());
        assert_eq!(log["message_key"], routing_key.into());
        assert_eq!(log[log_schema().source_type_key()], "amqp".into());
        let log_ts = log[log_schema().timestamp_key()].as_timestamp().unwrap();
        assert!(log_ts.signed_duration_since(now) < chrono::Duration::seconds(1));
        assert_eq!(log["exchange"], exchange.into());
    }
}
