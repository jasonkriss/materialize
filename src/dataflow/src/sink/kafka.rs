// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::any::Any;
use std::cell::RefCell;
use std::cmp;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use differential_dataflow::{Collection, Hashable};
use lazy_static::lazy_static;
use log::{error, info};
use prometheus::{
    register_int_counter_vec, register_uint_gauge_vec, IntCounter, IntCounterVec, UIntGauge,
    UIntGaugeVec,
};
use rdkafka::client::ClientContext;
use rdkafka::config::ClientConfig;
use rdkafka::error::{KafkaError, RDKafkaErrorCode};
use rdkafka::message::Message;
use rdkafka::producer::Producer;
use rdkafka::producer::{BaseRecord, DeliveryResult, ProducerContext, ThreadedProducer};
use timely::dataflow::channels::pact::Exchange;
use timely::dataflow::channels::pact::Pipeline;
use timely::dataflow::operators::generic::builder_rc::OperatorBuilder;
use timely::dataflow::operators::generic::{FrontieredInputHandle, InputHandle, OutputHandle};
use timely::dataflow::operators::Capability;
use timely::dataflow::{Scope, Stream};
use timely::progress::Antichain;

use dataflow_types::{KafkaSinkConnector, SinkAsOf};
use expr::GlobalId;
use interchange::avro::{self, Encoder};
use repr::{Diff, RelationDesc, Row, Timestamp};

use crate::source::timestamp::TimestampBindingRc;

/// Per-Kafka sink metrics.
#[derive(Clone)]
pub struct SinkMetrics {
    messages_sent_counter: IntCounter,
    message_send_errors_counter: IntCounter,
    message_delivery_errors_counter: IntCounter,
    rows_queued: UIntGauge,
    messages_in_flight: UIntGauge,
}

impl SinkMetrics {
    fn new(topic_name: &str, sink_id: &str, worker_id: &str) -> SinkMetrics {
        lazy_static! {
            static ref MESSAGES_SENT_COUNTER: IntCounterVec = register_int_counter_vec!(
                "mz_kafka_messages_sent_total",
                "The number of messages the Kafka producer successfully sent for this sink",
                &["topic", "sink_id", "worker_id"]
            )
            .unwrap();
            static ref MESSAGE_SEND_ERRORS_COUNTER: IntCounterVec = register_int_counter_vec!(
                "mz_kafka_message_send_errors_total",
                "The number of times the Kafka producer encountered an error on send",
                &["topic", "sink_id", "worker_id"]
            )
            .unwrap();
            static ref MESSAGE_DELIVERY_ERRORS_COUNTER: IntCounterVec = register_int_counter_vec!(
                "mz_kafka_message_delivery_errors_total",
                "The number of messages that the Kafka producer could not deliver to the topic",
                &["topic", "sink_id", "worker_id"]
            )
            .unwrap();
            static ref ROWS_QUEUED: UIntGaugeVec = register_uint_gauge_vec!(
                "mz_kafka_sink_rows_queued",
                "The current number of rows queued by the Kafka sink operator (note that one row can generate multiple Kafka messages)",
                &["topic", "sink_id", "worker_id"]
            )
            .unwrap();
            static ref MESSAGES_IN_FLIGHT: UIntGaugeVec = register_uint_gauge_vec!(
                "mz_kafka_sink_messages_in_flight",
                "The current number of messages waiting to be delivered by the Kafka producer",
                &["topic", "sink_id", "worker_id"]
            )
            .unwrap();
        }
        let labels = &[topic_name, sink_id, worker_id];
        SinkMetrics {
            messages_sent_counter: MESSAGES_SENT_COUNTER.with_label_values(labels),
            message_send_errors_counter: MESSAGE_SEND_ERRORS_COUNTER.with_label_values(labels),
            message_delivery_errors_counter: MESSAGE_DELIVERY_ERRORS_COUNTER
                .with_label_values(labels),
            rows_queued: ROWS_QUEUED.with_label_values(labels),
            messages_in_flight: MESSAGES_IN_FLIGHT.with_label_values(labels),
        }
    }
}

#[derive(Clone)]
pub struct SinkProducerContext {
    metrics: SinkMetrics,
    shutdown_flag: Arc<AtomicBool>,
}

impl SinkProducerContext {
    pub fn new(metrics: SinkMetrics, shutdown_flag: Arc<AtomicBool>) -> Self {
        SinkProducerContext {
            metrics,
            shutdown_flag,
        }
    }
}

impl ClientContext for SinkProducerContext {}
impl ProducerContext for SinkProducerContext {
    type DeliveryOpaque = ();

    fn delivery(&self, result: &DeliveryResult, _: Self::DeliveryOpaque) {
        match result {
            Ok(_) => (),
            Err((e, msg)) => {
                self.metrics.message_delivery_errors_counter.inc();
                error!(
                    "received error while writing to kafka sink topic {}: {}",
                    msg.topic(),
                    e
                );
                self.shutdown_flag.store(true, Ordering::SeqCst);
            }
        }
    }
}

struct KafkaSinkToken {
    shutdown_flag: Arc<AtomicBool>,
}

impl Drop for KafkaSinkToken {
    fn drop(&mut self) {
        self.shutdown_flag.store(true, Ordering::SeqCst);
    }
}

struct KafkaSink {
    name: String,
    shutdown_flag: Arc<AtomicBool>,
    metrics: SinkMetrics,
    producer: ThreadedProducer<SinkProducerContext>,
    activator: timely::scheduling::Activator,
    txn_timeout: Duration,
}

impl KafkaSink {
    fn transition_on_txn_error(
        &self,
        current_state: SendState,
        ts: u64,
        e: KafkaError,
    ) -> SendState {
        error!(
            "encountered error during kafka interaction. {} in state {:?} at time {} : {}",
            &self.name, current_state, ts, e
        );

        match e {
            KafkaError::Transaction(e) => {
                if e.txn_requires_abort() {
                    SendState::AbortTxn
                } else if e.is_retriable() {
                    current_state
                } else {
                    SendState::Shutdown
                }
            }
            _ => SendState::Shutdown,
        }
    }

    fn send(&self, record: BaseRecord<Vec<u8>, Vec<u8>>) -> Result<(), bool> {
        if let Err((e, _)) = self.producer.send(record) {
            error!("unable to produce message in {}: {}", self.name, e);
            self.metrics.message_send_errors_counter.inc();

            if let KafkaError::MessageProduction(RDKafkaErrorCode::QueueFull) = e {
                self.activator.activate_after(Duration::from_secs(60));
                Err(true)
            } else {
                // We've received an error that is not transient
                self.shutdown_flag.store(true, Ordering::SeqCst);
                Err(false)
            }
        } else {
            self.metrics.messages_sent_counter.inc();
            Ok(())
        }
    }
}

#[derive(Debug, Copy, Clone)]
enum SendState {
    // Initialize ourselves as a transactional producer with Kafka
    // Note that this only runs once across all workers - it should only execute
    // for the worker that will actually be publishing to kafka
    Init,
    // Corresponds to a Kafka begin_transaction call
    BeginTxn,
    // Write BEGIN consistency record
    Begin,
    // Flush pending rows for closed timestamps
    Draining {
        // row_index points to the current flushing row within the closed timestamp
        // we're processing
        row_index: usize,
        // multiple copies of a row may need to be sent if its cardinality is >1
        repeat_counter: usize,
        // a count of all rows sent, accounting for the repeat counter
        total_sent: i64,
    },
    // Write END consistency record
    End(i64),
    // Corresponds to a Kafka commit_transaction call
    CommitTxn,
    // Transitioned to when an error in a previous transactional state requires an abort
    AbortTxn,
    // Transitioned to when the sink needs to be closed
    Shutdown,
}

#[derive(Debug)]
struct EncodedRow {
    key: Option<Vec<u8>>,
    value: Option<Vec<u8>>,
    count: usize,
}

// TODO@jldlaughlin: What guarantees does this sink support? #1728
pub fn kafka<G>(
    collection: Collection<G, (Option<Row>, Option<Row>)>,
    id: GlobalId,
    connector: KafkaSinkConnector,
    key_desc: Option<RelationDesc>,
    value_desc: RelationDesc,
    as_of: SinkAsOf,
    source_timestamp_histories: Vec<TimestampBindingRc>,
    write_frontier: Rc<RefCell<Antichain<Timestamp>>>,
) -> Box<dyn Any>
where
    G: Scope<Timestamp = Timestamp>,
{
    let name = format!("kafka-{}", id);

    let stream = &collection.inner;

    let encoder = Encoder::new(key_desc, value_desc, connector.consistency.is_some());
    let key_schema_id = connector.key_schema_id;
    let value_schema_id = connector.value_schema_id;

    let encoded_stream = avro_encode_stream(
        stream,
        as_of.clone(),
        connector
            .consistency
            .clone()
            .and_then(|consistency| consistency.gate_ts),
        encoder,
        key_schema_id,
        value_schema_id,
        connector.fuel,
        name.clone(),
    );

    produce_to_kafka(
        encoded_stream,
        id,
        name,
        connector,
        as_of,
        source_timestamp_histories,
        write_frontier,
    )
}

/// Produces/sends a stream of encoded rows (as `Vec<u8>`) to Kafka.
///
/// This operator exchanges all updates to a single worker by hashing on the given sink `id`.
///
/// Updates are only sent to Kafka once the input frontier has passed their `time`. Updates are
/// sent in ascending timestamp order. The order of updates at the same timstamp will not be changed.
/// However, it is important to keep in mind that this operator exchanges updates so if the input
/// stream is sharded updates will likely arrive at this operator in some non-deterministic order.
///
/// Updates that are not beyond the given [`SinkAsOf`] and/or the `gate_ts` in
/// [`KafkaSinkConnector`] will be discarded without producing them.
pub fn produce_to_kafka<G>(
    stream: Stream<G, ((Option<Vec<u8>>, Option<Vec<u8>>), Timestamp, Diff)>,
    id: GlobalId,
    name: String,
    connector: KafkaSinkConnector,
    as_of: SinkAsOf,
    source_timestamp_histories: Vec<TimestampBindingRc>,
    write_frontier: Rc<RefCell<Antichain<Timestamp>>>,
) -> Box<dyn Any>
where
    G: Scope<Timestamp = Timestamp>,
{
    let mut config = ClientConfig::new();
    config.set("bootstrap.servers", &connector.addrs.to_string());

    // Ensure that messages are sinked in order and without duplicates. Note that
    // this only applies to a single instance of a producer - in the case of restarts,
    // all bets are off and full exactly once support is required.
    config.set("enable.idempotence", "true");

    // Increase limits for the Kafka producer's internal buffering of messages
    // Currently we don't have a great backpressure mechanism to tell indexes or
    // views to slow down, so the only thing we can do with a message that we
    // can't immediately send is to put it in a buffer and there's no point
    // having buffers within the dataflow layer and Kafka
    // If the sink starts falling behind and the buffers start consuming
    // too much memory the best thing to do is to drop the sink
    // Sets the buffer size to be 16 GB (note that this setting is in KB)
    config.set("queue.buffering.max.kbytes", &format!("{}", 16 << 20));

    // Set the max messages buffered by the producer at any time to 10MM which
    // is the maximum allowed value
    config.set("queue.buffering.max.messages", &format!("{}", 10_000_000));

    // Make the Kafka producer wait at least 10 ms before sending out MessageSets
    // TODO(rkhaitan): experiment with different settings for this value to see
    // if it makes a big difference
    config.set("queue.buffering.max.ms", &format!("{}", 10));

    for (k, v) in connector.config_options.iter() {
        // We explicitly reject `statistics.interval.ms` here so that we don't
        // flood the INFO log with statistics messages.
        // TODO: properly support statistics on Kafka sinks
        if k != "statistics.interval.ms" {
            config.set(k, v);
        }
    }

    let transactional = if connector.exactly_once {
        // TODO(aljoscha): this only works for now, once there's an actual
        // Kafka producer on each worker they would step on each others toes
        let transactional_id = format!("mz-producer-{}", connector.topic);
        config.set("transactional.id", transactional_id);
        true
    } else {
        false
    };

    let shutdown_flag = Arc::new(AtomicBool::new(false));
    let mut builder = OperatorBuilder::new(name.clone(), stream.scope());

    let s = {
        let metrics = SinkMetrics::new(
            &connector.topic,
            &id.to_string(),
            &stream.scope().index().to_string(),
        );

        let producer = config
            .create_with_context::<_, ThreadedProducer<_>>(SinkProducerContext::new(
                metrics.clone(),
                shutdown_flag.clone(),
            ))
            .expect("creating kafka producer for kafka sinks failed");

        let activator = stream
            .scope()
            .activator_for(&builder.operator_info().address[..]);

        KafkaSink {
            name,
            shutdown_flag: shutdown_flag.clone(),
            metrics,
            producer,
            activator,
            txn_timeout: Duration::from_secs(5),
        }
    };

    let mut pending_rows: HashMap<Timestamp, Vec<EncodedRow>> = HashMap::new();
    let mut ready_rows: VecDeque<(Timestamp, Vec<EncodedRow>)> = VecDeque::new();
    let mut state = SendState::Init;
    let mut vector = Vec::new();

    let mut sink_logic = move |input: &mut FrontieredInputHandle<
        _,
        ((Option<Vec<u8>>, Option<Vec<u8>>), Timestamp, Diff),
        _,
    >| {
        if s.shutdown_flag.load(Ordering::SeqCst) {
            info!("shutting down sink: {}", &s.name);
            return false;
        }

        // Queue all pending rows waiting to be sent to kafka
        input.for_each(|_, rows| {
            rows.swap(&mut vector);
            for ((key, value), time, diff) in vector.drain(..) {
                let should_emit = if as_of.strict {
                    as_of.frontier.less_than(&time)
                } else {
                    as_of.frontier.less_equal(&time)
                };

                let previously_published = match &connector.consistency {
                    Some(consistency) => match consistency.gate_ts {
                        Some(gate_ts) => time <= gate_ts,
                        None => false,
                    },
                    None => false,
                };

                if !should_emit || previously_published {
                    // Skip stale data for already published timestamps
                    continue;
                }

                assert!(diff >= 0, "can't sink negative multiplicities");
                if diff == 0 {
                    // Explicitly refuse to send no-op records
                    continue;
                };
                let diff = diff as usize;

                let rows = pending_rows.entry(time).or_default();
                rows.push(EncodedRow {
                    key,
                    value,
                    count: diff,
                });
                s.metrics.rows_queued.inc();
            }
        });

        // Figure out the durablity frontier for all sources we depent on
        let mut durability_frontier = Antichain::new();

        for history in source_timestamp_histories.iter() {
            use differential_dataflow::lattice::Lattice;
            durability_frontier.meet_assign(&history.durability_frontier());
        }
        // Move any newly closed timestamps from pending to ready
        let mut closed_ts: Vec<u64> = pending_rows
            .iter()
            .filter(|(ts, _)| {
                !input.frontier.less_equal(*ts) && !durability_frontier.less_equal(*ts)
            })
            .map(|(&ts, _)| ts)
            .collect();
        closed_ts.sort_unstable();
        closed_ts.into_iter().for_each(|ts| {
            let rows = pending_rows.remove(&ts).unwrap();
            ready_rows.push_back((ts, rows));
        });

        // Send a bounded number of records to Kafka from the ready queue.
        // This loop has explicitly been designed so that each iteration sends
        // at most one record to Kafka
        for _ in 0..connector.fuel {
            if let Some((ts, rows)) = ready_rows.front() {
                state = match state {
                    SendState::Init => {
                        let result = if transactional {
                            s.producer.init_transactions(s.txn_timeout)
                        } else {
                            Ok(())
                        };

                        match result {
                            Ok(()) => SendState::BeginTxn,
                            Err(e) => s.transition_on_txn_error(state, *ts, e),
                        }
                    }
                    SendState::BeginTxn => {
                        let result = if transactional {
                            s.producer.begin_transaction()
                        } else {
                            Ok(())
                        };

                        match result {
                            Ok(()) => SendState::Begin,
                            Err(e) => s.transition_on_txn_error(state, *ts, e),
                        }
                    }
                    SendState::Begin => {
                        if let Some(consistency) = &connector.consistency {
                            let encoded = avro::encode_debezium_transaction_unchecked(
                                consistency.schema_id,
                                &ts.to_string(),
                                "BEGIN",
                                None,
                            );

                            let record = BaseRecord::to(&consistency.topic).payload(&encoded);
                            if let Err(retry) = s.send(record) {
                                return retry;
                            }
                        }
                        SendState::Draining {
                            row_index: 0,
                            repeat_counter: 0,
                            total_sent: 0,
                        }
                    }
                    SendState::Draining {
                        mut row_index,
                        mut repeat_counter,
                        mut total_sent,
                    } => {
                        let encoded_row = &rows[row_index];
                        let record = BaseRecord::to(&connector.topic);
                        let record = if encoded_row.value.is_some() {
                            record.payload(encoded_row.value.as_ref().unwrap())
                        } else {
                            record
                        };
                        let record = if encoded_row.key.is_some() {
                            record.key(encoded_row.key.as_ref().unwrap())
                        } else {
                            record
                        };
                        if let Err(retry) = s.send(record) {
                            return retry;
                        }

                        // advance to the next repetition of this row, or the next row if all
                        // reptitions are exhausted
                        total_sent += 1;
                        repeat_counter += 1;
                        if repeat_counter == encoded_row.count {
                            repeat_counter = 0;
                            row_index += 1;
                            s.metrics.rows_queued.dec();
                        }

                        // move to the end state if we've finished all rows in this timestamp
                        if row_index == rows.len() {
                            SendState::End(total_sent)
                        } else {
                            SendState::Draining {
                                row_index,
                                repeat_counter,
                                total_sent,
                            }
                        }
                    }
                    SendState::End(total_count) => {
                        if let Some(consistency) = &connector.consistency {
                            let encoded = avro::encode_debezium_transaction_unchecked(
                                consistency.schema_id,
                                &ts.to_string(),
                                "END",
                                Some(total_count),
                            );

                            let record = BaseRecord::to(&consistency.topic).payload(&encoded);
                            if let Err(retry) = s.send(record) {
                                return retry;
                            }
                        }
                        SendState::CommitTxn
                    }
                    SendState::CommitTxn => {
                        let result = if transactional {
                            s.producer.commit_transaction(s.txn_timeout)
                        } else {
                            Ok(())
                        };

                        match result {
                            Ok(()) => {
                                assert!(write_frontier.borrow().less_equal(ts));
                                write_frontier.borrow_mut().clear();
                                write_frontier.borrow_mut().insert(*ts);
                                ready_rows.pop_front();
                                SendState::BeginTxn
                            }
                            Err(e) => s.transition_on_txn_error(state, *ts, e),
                        }
                    }
                    SendState::AbortTxn => {
                        let result = if transactional {
                            s.producer.abort_transaction(s.txn_timeout)
                        } else {
                            Ok(())
                        };

                        match result {
                            Ok(()) => SendState::BeginTxn,
                            Err(e) => s.transition_on_txn_error(state, *ts, e),
                        }
                    }
                    SendState::Shutdown => {
                        s.shutdown_flag.store(false, Ordering::SeqCst);
                        break;
                    }
                };
            } else {
                break;
            }
        }

        let in_flight = s.producer.in_flight_count();
        s.metrics.messages_in_flight.set(in_flight as u64);

        if !ready_rows.is_empty() {
            // We need timely to reschedule this operator as we have pending
            // items that we need to send to Kafka
            s.activator.activate();
            return true;
        }

        if in_flight > 0 {
            // We still have messages that need to be flushed out to Kafka
            // Let's make sure to keep the sink operator around until
            // we flush them out
            s.activator.activate_after(Duration::from_secs(5));
            return true;
        }

        false
    };

    // We want exactly one worker to send all the data to the sink topic.
    let hashed_id = id.hashed();
    let mut input = builder.new_input(&stream, Exchange::new(move |_| hashed_id));

    builder.build_reschedule(|_capabilities| {
        move |frontiers| {
            let mut input_handle = FrontieredInputHandle::new(&mut input, &frontiers[0]);
            sink_logic(&mut input_handle)
        }
    });

    Box::new(KafkaSinkToken { shutdown_flag })
}

/// Encodes a stream of `(Option<Row>, Option<Row>)` updates using Avro.
///
/// This operator will only encode `fuel` number of updates per invocation. If necessary, it will
/// stash updates and use an [`timely::scheduling::Activator`] to re-schedule future invocations.
///
/// Input [`Row`] updates must me compatible with the given [`Encoder`].
///
/// Updates that are not beyond the given [`SinkAsOf`] and/or the `gate_ts` will be discarded
/// without encoding them.
///
/// Input updates do not have to be partitioned and/or sorted. This operator will not exchange
/// data. Updates with lower timestamps will be processed before updates with higher timestamps
/// if they arrive in order. However, this is not a guarantee, as this operator does not wait
/// for the frontier to signal completeness. It is an optimization for downstream operators
/// that behave suboptimal when receiving updates that are too far in the future with respect
/// to the current frontier. The order of updates that arrive at the same timestamp will not be
/// changed.
fn avro_encode_stream<G>(
    input_stream: &Stream<G, ((Option<Row>, Option<Row>), Timestamp, Diff)>,
    as_of: SinkAsOf,
    gate_ts: Option<Timestamp>,
    encoder: Encoder,
    key_schema_id: Option<i32>,
    value_schema_id: i32,
    fuel: usize,
    name_prefix: String,
) -> Stream<G, ((Option<Vec<u8>>, Option<Vec<u8>>), Timestamp, Diff)>
where
    G: Scope<Timestamp = Timestamp>,
{
    let name = format!("{}-avro_encode", name_prefix);

    let mut builder = OperatorBuilder::new(name, input_stream.scope());
    let mut input = builder.new_input(&input_stream, Pipeline);
    let (mut output, output_stream) = builder.new_output();
    builder.set_notify(false);

    let activator = input_stream
        .scope()
        .activator_for(&builder.operator_info().address[..]);

    let mut stash: HashMap<Capability<Timestamp>, Vec<_>> = HashMap::new();
    let mut vector = Vec::new();
    let mut encode_logic = move |input: &mut InputHandle<
        Timestamp,
        ((Option<Row>, Option<Row>), Timestamp, Diff),
        _,
    >,
                                 output: &mut OutputHandle<
        _,
        ((Option<Vec<u8>>, Option<Vec<u8>>), Timestamp, Diff),
        _,
    >| {
        let mut fuel_remaining = fuel;
        // stash away all the input we get, we want to be a nice citizen
        input.for_each(|cap, data| {
            data.swap(&mut vector);
            let stashed = stash.entry(cap.retain()).or_default();
            for update in vector.drain(..) {
                let time = update.1;

                let should_emit = if as_of.strict {
                    as_of.frontier.less_than(&time)
                } else {
                    as_of.frontier.less_equal(&time)
                };

                let ts_gated = match gate_ts {
                    Some(gate_ts) => time <= gate_ts,
                    None => false,
                };

                if !should_emit || ts_gated {
                    // Skip stale data for already published timestamps
                    continue;
                }
                stashed.push(update);
            }
        });

        // work off some of our data and then yield, can't be hogging
        // the worker for minutes at a time

        while fuel_remaining > 0 && !stash.is_empty() {
            let lowest_ts = stash
                .keys()
                .min_by(|x, y| x.time().cmp(y.time()))
                .expect("known to exist")
                .clone();
            let records = stash.get_mut(&lowest_ts).expect("known to exist");

            let mut session = output.session(&lowest_ts);
            let num_records_to_drain = cmp::min(records.len(), fuel_remaining);
            records
                .drain(..num_records_to_drain)
                .for_each(|((key, value), time, diff)| {
                    let key =
                        key.map(|key| encoder.encode_key_unchecked(key_schema_id.unwrap(), key));
                    let value =
                        value.map(|value| encoder.encode_value_unchecked(value_schema_id, value));
                    session.give(((key, value), time, diff));
                });

            fuel_remaining -= num_records_to_drain;

            if records.is_empty() {
                // drop our capability for this time
                stash.remove(&lowest_ts);
            }
        }

        if !stash.is_empty() {
            activator.activate();
            return true;
        }
        // signal that we're complete now
        false
    };

    builder.build_reschedule(|_capabilities| {
        move |_frontiers| {
            let mut output_handle = output.activate();
            encode_logic(&mut input, &mut output_handle)
        }
    });

    output_stream
}
