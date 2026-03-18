use crossbeam_channel::Sender;

use crate::event_loop::{EventBatcher, EventBatcherStatsHandle, EventSink};
use crate::h3_event::{JsEventMeta, JsH3Event};

#[cfg(feature = "node-api")]
use crate::event_loop::{EventTsfn, TsfnEventSink};

pub struct TaggedEventBatch {
    pub source: String,
    pub events: Vec<JsH3Event>,
}

#[allow(dead_code)]
struct NoopEventSink;

impl EventSink for NoopEventSink {
    fn kind(&self) -> &'static str {
        "noop"
    }

    fn emit(
        &mut self,
        _events: Vec<JsH3Event>,
        _stats: &EventBatcherStatsHandle,
    ) -> bool {
        true
    }
}

#[allow(dead_code)]
struct CountingEventSink;

impl EventSink for CountingEventSink {
    fn kind(&self) -> &'static str {
        "counting"
    }

    fn emit(
        &mut self,
        _events: Vec<JsH3Event>,
        _stats: &EventBatcherStatsHandle,
    ) -> bool {
        true
    }
}

struct ChannelEventSink {
    source: String,
    sender: Sender<TaggedEventBatch>,
}

struct CompositeEventSink<A, B> {
    kind: &'static str,
    primary: A,
    secondary: B,
}

impl EventSink for ChannelEventSink {
    fn kind(&self) -> &'static str {
        "channel"
    }

    fn emit(
        &mut self,
        events: Vec<JsH3Event>,
        stats: &EventBatcherStatsHandle,
    ) -> bool {
        let count = events.len();
        self.sender
            .send(TaggedEventBatch {
                source: self.source.clone(),
                events,
            })
            .map_or_else(
                |_| {
                    stats.record_sink_error();
                    stats.record_drop(count);
                    false
                },
                |_| true,
            )
    }
}

impl<A: EventSink, B: EventSink> EventSink for CompositeEventSink<A, B> {
    fn kind(&self) -> &'static str {
        self.kind
    }

    fn emit(
        &mut self,
        events: Vec<JsH3Event>,
        stats: &EventBatcherStatsHandle,
    ) -> bool {
        let mirrored = clone_events(&events);
        let primary_ok = self.primary.emit(events, stats);
        let secondary_ok = self.secondary.emit(mirrored, stats);
        primary_ok && secondary_ok
    }
}

#[allow(dead_code)]
pub(crate) fn noop_batcher() -> (EventBatcher, EventBatcherStatsHandle) {
    let batcher = EventBatcher::with_sink(NoopEventSink);
    let stats = batcher.stats_handle();
    (batcher, stats)
}

#[allow(dead_code)]
pub(crate) fn counting_batcher() -> (EventBatcher, EventBatcherStatsHandle) {
    let batcher = EventBatcher::with_sink(CountingEventSink);
    let stats = batcher.stats_handle();
    (batcher, stats)
}

pub(crate) fn channel_batcher(
    source: impl Into<String>,
    sender: Sender<TaggedEventBatch>,
) -> (EventBatcher, EventBatcherStatsHandle) {
    let batcher = EventBatcher::with_sink(ChannelEventSink {
        source: source.into(),
        sender,
    });
    let stats = batcher.stats_handle();
    (batcher, stats)
}

pub(crate) fn channel_and_counting_batcher(
    source: impl Into<String>,
    sender: Sender<TaggedEventBatch>,
) -> (EventBatcher, EventBatcherStatsHandle) {
    let batcher = EventBatcher::with_sink(CompositeEventSink {
        kind: "channel+counting",
        primary: ChannelEventSink {
            source: source.into(),
            sender,
        },
        secondary: CountingEventSink,
    });
    let stats = batcher.stats_handle();
    (batcher, stats)
}

#[cfg(feature = "node-api")]
pub(crate) fn channel_and_tsfn_batcher(
    source: impl Into<String>,
    sender: Sender<TaggedEventBatch>,
    tsfn: EventTsfn,
) -> (EventBatcher, EventBatcherStatsHandle) {
    let batcher = EventBatcher::with_sink(CompositeEventSink {
        kind: "channel+tsfn",
        primary: ChannelEventSink {
            source: source.into(),
            sender,
        },
        secondary: TsfnEventSink::new(tsfn),
    });
    let stats = batcher.stats_handle();
    (batcher, stats)
}

fn clone_events(events: &[JsH3Event]) -> Vec<JsH3Event> {
    events.iter().map(clone_event).collect()
}

fn clone_event(event: &JsH3Event) -> JsH3Event {
    JsH3Event {
        event_type: event.event_type,
        conn_handle: event.conn_handle,
        stream_id: event.stream_id,
        headers: event.headers.clone(),
        data: event.data.as_ref().map(|data| data.to_vec().into()),
        fin: event.fin,
        meta: event.meta.as_ref().map(clone_event_meta),
        metrics: event.metrics.clone(),
    }
}

fn clone_event_meta(meta: &JsEventMeta) -> JsEventMeta {
    JsEventMeta {
        error_code: meta.error_code,
        error_reason: meta.error_reason.clone(),
        error_category: meta.error_category.clone(),
        remote_addr: meta.remote_addr.clone(),
        remote_port: meta.remote_port,
        server_name: meta.server_name.clone(),
        reason_code: meta.reason_code.clone(),
        runtime_driver: meta.runtime_driver.clone(),
        runtime_mode: meta.runtime_mode.clone(),
        requested_runtime_mode: meta.requested_runtime_mode.clone(),
        fallback_occurred: meta.fallback_occurred,
        errno: meta.errno,
        syscall: meta.syscall.clone(),
    }
}
