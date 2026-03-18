#![allow(non_snake_case)]

use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(feature = "node-api")]
use napi_derive::napi;
use serde::Serialize;

use crate::transport::RuntimeDriverKind;

#[cfg_attr(feature = "node-api", napi(object))]
#[derive(Clone, Debug, Default, Serialize)]
pub struct JsReactorTelemetrySnapshot {
    pub driverSetupAttemptsTotal: i64,
    pub driverSetupSuccessTotal: i64,
    pub driverSetupFailureTotal: i64,
    pub ioUringDriverSetupAttempts: i64,
    pub ioUringDriverSetupSuccesses: i64,
    pub ioUringDriverSetupFailures: i64,
    pub pollDriverSetupAttempts: i64,
    pub pollDriverSetupSuccesses: i64,
    pub pollDriverSetupFailures: i64,
    pub kqueueDriverSetupAttempts: i64,
    pub kqueueDriverSetupSuccesses: i64,
    pub kqueueDriverSetupFailures: i64,
    pub workerThreadSpawnsTotal: i64,
    pub rawQuicServerWorkerSpawns: i64,
    pub rawQuicClientDedicatedWorkerSpawns: i64,
    pub rawQuicClientSharedWorkersCreated: i64,
    pub rawQuicClientSharedWorkerReuses: i64,
    pub h3ServerWorkerSpawns: i64,
    pub h3ClientDedicatedWorkerSpawns: i64,
    pub h3ClientSharedWorkersCreated: i64,
    pub h3ClientSharedWorkerReuses: i64,
    pub clientLocalPortReuseHits: i64,
    pub rawQuicClientSessionsOpened: i64,
    pub rawQuicClientSessionsClosed: i64,
    pub rawQuicFinObservations: i64,
    pub rawQuicFinishedEventEmits: i64,
    pub rawQuicDrainEventEmits: i64,
    pub rawQuicBlockedStreamHighWatermark: i64,
    pub rawQuicClientPendingWriteHighWatermark: i64,
    pub rawQuicClientReapsWithPendingWrites: i64,
    pub rawQuicClientReapsWithBlockedStreams: i64,
    pub rawQuicClientReapsWithKnownStreams: i64,
    pub rawQuicClientCloseByPacket: i64,
    pub rawQuicClientCloseByTimeout: i64,
    pub rawQuicClientCloseByShutdown: i64,
    pub rawQuicClientCloseByRelease: i64,
    pub rawQuicServerSessionsOpened: i64,
    pub rawQuicServerSessionsClosed: i64,
    pub h3ClientSessionsOpened: i64,
    pub h3ClientSessionsClosed: i64,
    pub h3ServerSessionsOpened: i64,
    pub h3ServerSessionsClosed: i64,
    pub ioUringRxInFlightHighWatermark: i64,
    pub ioUringTxInFlightHighWatermark: i64,
    pub ioUringPendingTxHighWatermark: i64,
    pub ioUringRetryableSendCompletions: i64,
    pub ioUringSubmitCalls: i64,
    pub ioUringSubmitWithArgsCalls: i64,
    pub ioUringSubmittedSqesTotal: i64,
    pub ioUringCompletionTotal: i64,
    pub ioUringCompletionBatchHighWatermark: i64,
    pub ioUringWakeCompletions: i64,
    pub ioUringWakeWrites: i64,
    pub ioUringTimeoutPolls: i64,
    pub ioUringRxDatagramsTotal: i64,
    pub ioUringTxDatagramsSubmittedTotal: i64,
    pub ioUringTxDatagramsCompletedTotal: i64,
    pub ioUringSqFullEvents: i64,
    pub kqueueUnsentHighWatermark: i64,
    pub kqueueWouldBlockSends: i64,
    pub kqueueWriteWakeups: i64,
    pub txBuffersRecycled: i64,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum WorkerSpawnKind {
    RawQuicServer,
    RawQuicClientDedicated,
    RawQuicClientShared,
    H3Server,
    H3ClientDedicated,
    H3ClientShared,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum SessionKind {
    RawQuicClient,
    RawQuicServer,
    H3Client,
    H3Server,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum RawQuicClientCloseCause {
    Packet,
    Timeout,
    Shutdown,
    Release,
}

static DRIVER_SETUP_ATTEMPTS_TOTAL: AtomicU64 = AtomicU64::new(0);
static DRIVER_SETUP_SUCCESS_TOTAL: AtomicU64 = AtomicU64::new(0);
static DRIVER_SETUP_FAILURE_TOTAL: AtomicU64 = AtomicU64::new(0);

static IO_URING_DRIVER_SETUP_ATTEMPTS: AtomicU64 = AtomicU64::new(0);
static IO_URING_DRIVER_SETUP_SUCCESSES: AtomicU64 = AtomicU64::new(0);
static IO_URING_DRIVER_SETUP_FAILURES: AtomicU64 = AtomicU64::new(0);

static POLL_DRIVER_SETUP_ATTEMPTS: AtomicU64 = AtomicU64::new(0);
static POLL_DRIVER_SETUP_SUCCESSES: AtomicU64 = AtomicU64::new(0);
static POLL_DRIVER_SETUP_FAILURES: AtomicU64 = AtomicU64::new(0);

static KQUEUE_DRIVER_SETUP_ATTEMPTS: AtomicU64 = AtomicU64::new(0);
static KQUEUE_DRIVER_SETUP_SUCCESSES: AtomicU64 = AtomicU64::new(0);
static KQUEUE_DRIVER_SETUP_FAILURES: AtomicU64 = AtomicU64::new(0);

static WORKER_THREAD_SPAWNS_TOTAL: AtomicU64 = AtomicU64::new(0);
static RAW_QUIC_SERVER_WORKER_SPAWNS: AtomicU64 = AtomicU64::new(0);
static RAW_QUIC_CLIENT_DEDICATED_WORKER_SPAWNS: AtomicU64 = AtomicU64::new(0);
static RAW_QUIC_CLIENT_SHARED_WORKERS_CREATED: AtomicU64 = AtomicU64::new(0);
static RAW_QUIC_CLIENT_SHARED_WORKER_REUSES: AtomicU64 = AtomicU64::new(0);
static H3_SERVER_WORKER_SPAWNS: AtomicU64 = AtomicU64::new(0);
static H3_CLIENT_DEDICATED_WORKER_SPAWNS: AtomicU64 = AtomicU64::new(0);
static H3_CLIENT_SHARED_WORKERS_CREATED: AtomicU64 = AtomicU64::new(0);
static H3_CLIENT_SHARED_WORKER_REUSES: AtomicU64 = AtomicU64::new(0);
static CLIENT_LOCAL_PORT_REUSE_HITS: AtomicU64 = AtomicU64::new(0);

static RAW_QUIC_CLIENT_SESSIONS_OPENED: AtomicU64 = AtomicU64::new(0);
static RAW_QUIC_CLIENT_SESSIONS_CLOSED: AtomicU64 = AtomicU64::new(0);
static RAW_QUIC_FIN_OBSERVATIONS: AtomicU64 = AtomicU64::new(0);
static RAW_QUIC_FINISHED_EVENT_EMITS: AtomicU64 = AtomicU64::new(0);
static RAW_QUIC_DRAIN_EVENT_EMITS: AtomicU64 = AtomicU64::new(0);
static RAW_QUIC_BLOCKED_STREAM_HIGH_WATERMARK: AtomicU64 = AtomicU64::new(0);
static RAW_QUIC_CLIENT_PENDING_WRITE_HIGH_WATERMARK: AtomicU64 = AtomicU64::new(0);
static RAW_QUIC_CLIENT_REAPS_WITH_PENDING_WRITES: AtomicU64 = AtomicU64::new(0);
static RAW_QUIC_CLIENT_REAPS_WITH_BLOCKED_STREAMS: AtomicU64 = AtomicU64::new(0);
static RAW_QUIC_CLIENT_REAPS_WITH_KNOWN_STREAMS: AtomicU64 = AtomicU64::new(0);
static RAW_QUIC_CLIENT_CLOSE_BY_PACKET: AtomicU64 = AtomicU64::new(0);
static RAW_QUIC_CLIENT_CLOSE_BY_TIMEOUT: AtomicU64 = AtomicU64::new(0);
static RAW_QUIC_CLIENT_CLOSE_BY_SHUTDOWN: AtomicU64 = AtomicU64::new(0);
static RAW_QUIC_CLIENT_CLOSE_BY_RELEASE: AtomicU64 = AtomicU64::new(0);
static RAW_QUIC_SERVER_SESSIONS_OPENED: AtomicU64 = AtomicU64::new(0);
static RAW_QUIC_SERVER_SESSIONS_CLOSED: AtomicU64 = AtomicU64::new(0);
static H3_CLIENT_SESSIONS_OPENED: AtomicU64 = AtomicU64::new(0);
static H3_CLIENT_SESSIONS_CLOSED: AtomicU64 = AtomicU64::new(0);
static H3_SERVER_SESSIONS_OPENED: AtomicU64 = AtomicU64::new(0);
static H3_SERVER_SESSIONS_CLOSED: AtomicU64 = AtomicU64::new(0);

static IO_URING_RX_IN_FLIGHT_HIGH_WATERMARK: AtomicU64 = AtomicU64::new(0);
static IO_URING_TX_IN_FLIGHT_HIGH_WATERMARK: AtomicU64 = AtomicU64::new(0);
static IO_URING_PENDING_TX_HIGH_WATERMARK: AtomicU64 = AtomicU64::new(0);
static IO_URING_RETRYABLE_SEND_COMPLETIONS: AtomicU64 = AtomicU64::new(0);
static IO_URING_SUBMIT_CALLS: AtomicU64 = AtomicU64::new(0);
static IO_URING_SUBMIT_WITH_ARGS_CALLS: AtomicU64 = AtomicU64::new(0);
static IO_URING_SUBMITTED_SQES_TOTAL: AtomicU64 = AtomicU64::new(0);
static IO_URING_COMPLETION_TOTAL: AtomicU64 = AtomicU64::new(0);
static IO_URING_COMPLETION_BATCH_HIGH_WATERMARK: AtomicU64 = AtomicU64::new(0);
static IO_URING_WAKE_COMPLETIONS: AtomicU64 = AtomicU64::new(0);
static IO_URING_WAKE_WRITES: AtomicU64 = AtomicU64::new(0);
static IO_URING_TIMEOUT_POLLS: AtomicU64 = AtomicU64::new(0);
static IO_URING_RX_DATAGRAMS_TOTAL: AtomicU64 = AtomicU64::new(0);
static IO_URING_TX_DATAGRAMS_SUBMITTED_TOTAL: AtomicU64 = AtomicU64::new(0);
static IO_URING_TX_DATAGRAMS_COMPLETED_TOTAL: AtomicU64 = AtomicU64::new(0);
static IO_URING_SQ_FULL_EVENTS: AtomicU64 = AtomicU64::new(0);
static KQUEUE_UNSENT_HIGH_WATERMARK: AtomicU64 = AtomicU64::new(0);
static KQUEUE_WOULD_BLOCK_SENDS: AtomicU64 = AtomicU64::new(0);
static KQUEUE_WRITE_WAKEUPS: AtomicU64 = AtomicU64::new(0);

static TX_BUFFERS_RECYCLED: AtomicU64 = AtomicU64::new(0);

fn load(counter: &AtomicU64) -> i64 {
    counter.load(Ordering::Relaxed) as i64
}

fn reset_counter(counter: &AtomicU64) {
    counter.store(0, Ordering::Relaxed);
}

fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

fn observe_max(counter: &AtomicU64, value: usize) {
    counter.fetch_max(value as u64, Ordering::Relaxed);
}

pub(crate) fn record_driver_setup_attempt(kind: RuntimeDriverKind) {
    bump(&DRIVER_SETUP_ATTEMPTS_TOTAL);
    match kind {
        RuntimeDriverKind::IoUring => bump(&IO_URING_DRIVER_SETUP_ATTEMPTS),
        RuntimeDriverKind::Poll => bump(&POLL_DRIVER_SETUP_ATTEMPTS),
        RuntimeDriverKind::Kqueue => bump(&KQUEUE_DRIVER_SETUP_ATTEMPTS),
        RuntimeDriverKind::Mock => {}
    }
}

pub(crate) fn record_driver_setup_success(kind: RuntimeDriverKind) {
    bump(&DRIVER_SETUP_SUCCESS_TOTAL);
    match kind {
        RuntimeDriverKind::IoUring => bump(&IO_URING_DRIVER_SETUP_SUCCESSES),
        RuntimeDriverKind::Poll => bump(&POLL_DRIVER_SETUP_SUCCESSES),
        RuntimeDriverKind::Kqueue => bump(&KQUEUE_DRIVER_SETUP_SUCCESSES),
        RuntimeDriverKind::Mock => {}
    }
}

pub(crate) fn record_driver_setup_failure(kind: RuntimeDriverKind) {
    bump(&DRIVER_SETUP_FAILURE_TOTAL);
    match kind {
        RuntimeDriverKind::IoUring => bump(&IO_URING_DRIVER_SETUP_FAILURES),
        RuntimeDriverKind::Poll => bump(&POLL_DRIVER_SETUP_FAILURES),
        RuntimeDriverKind::Kqueue => bump(&KQUEUE_DRIVER_SETUP_FAILURES),
        RuntimeDriverKind::Mock => {}
    }
}

pub(crate) fn record_worker_thread_spawn(kind: WorkerSpawnKind) {
    bump(&WORKER_THREAD_SPAWNS_TOTAL);
    match kind {
        WorkerSpawnKind::RawQuicServer => bump(&RAW_QUIC_SERVER_WORKER_SPAWNS),
        WorkerSpawnKind::RawQuicClientDedicated => {
            bump(&RAW_QUIC_CLIENT_DEDICATED_WORKER_SPAWNS)
        }
        WorkerSpawnKind::RawQuicClientShared => bump(&RAW_QUIC_CLIENT_SHARED_WORKERS_CREATED),
        WorkerSpawnKind::H3Server => bump(&H3_SERVER_WORKER_SPAWNS),
        WorkerSpawnKind::H3ClientDedicated => bump(&H3_CLIENT_DEDICATED_WORKER_SPAWNS),
        WorkerSpawnKind::H3ClientShared => bump(&H3_CLIENT_SHARED_WORKERS_CREATED),
    }
}

pub(crate) fn record_shared_worker_reuse(kind: WorkerSpawnKind) {
    match kind {
        WorkerSpawnKind::RawQuicClientShared => bump(&RAW_QUIC_CLIENT_SHARED_WORKER_REUSES),
        WorkerSpawnKind::H3ClientShared => bump(&H3_CLIENT_SHARED_WORKER_REUSES),
        _ => {}
    }
    bump(&CLIENT_LOCAL_PORT_REUSE_HITS);
}

pub(crate) fn record_session_open(kind: SessionKind) {
    match kind {
        SessionKind::RawQuicClient => bump(&RAW_QUIC_CLIENT_SESSIONS_OPENED),
        SessionKind::RawQuicServer => bump(&RAW_QUIC_SERVER_SESSIONS_OPENED),
        SessionKind::H3Client => bump(&H3_CLIENT_SESSIONS_OPENED),
        SessionKind::H3Server => bump(&H3_SERVER_SESSIONS_OPENED),
    }
}

pub(crate) fn record_session_close(kind: SessionKind) {
    match kind {
        SessionKind::RawQuicClient => bump(&RAW_QUIC_CLIENT_SESSIONS_CLOSED),
        SessionKind::RawQuicServer => bump(&RAW_QUIC_SERVER_SESSIONS_CLOSED),
        SessionKind::H3Client => bump(&H3_CLIENT_SESSIONS_CLOSED),
        SessionKind::H3Server => bump(&H3_SERVER_SESSIONS_CLOSED),
    }
}

pub(crate) fn record_raw_quic_fin_observed() {
    bump(&RAW_QUIC_FIN_OBSERVATIONS);
}

pub(crate) fn record_raw_quic_finished_event() {
    bump(&RAW_QUIC_FINISHED_EVENT_EMITS);
}

pub(crate) fn record_raw_quic_drain_event() {
    bump(&RAW_QUIC_DRAIN_EVENT_EMITS);
}

pub(crate) fn record_raw_quic_blocked_streams(count: usize) {
    observe_max(&RAW_QUIC_BLOCKED_STREAM_HIGH_WATERMARK, count);
}

pub(crate) fn record_raw_quic_client_pending_writes(count: usize) {
    observe_max(&RAW_QUIC_CLIENT_PENDING_WRITE_HIGH_WATERMARK, count);
}

pub(crate) fn record_raw_quic_client_reap(
    pending_writes: usize,
    blocked_streams: usize,
    known_streams: usize,
) {
    if pending_writes > 0 {
        bump(&RAW_QUIC_CLIENT_REAPS_WITH_PENDING_WRITES);
    }
    if blocked_streams > 0 {
        bump(&RAW_QUIC_CLIENT_REAPS_WITH_BLOCKED_STREAMS);
    }
    if known_streams > 0 {
        bump(&RAW_QUIC_CLIENT_REAPS_WITH_KNOWN_STREAMS);
    }
}

pub(crate) fn record_raw_quic_client_close_cause(cause: RawQuicClientCloseCause) {
    match cause {
        RawQuicClientCloseCause::Packet => bump(&RAW_QUIC_CLIENT_CLOSE_BY_PACKET),
        RawQuicClientCloseCause::Timeout => bump(&RAW_QUIC_CLIENT_CLOSE_BY_TIMEOUT),
        RawQuicClientCloseCause::Shutdown => bump(&RAW_QUIC_CLIENT_CLOSE_BY_SHUTDOWN),
        RawQuicClientCloseCause::Release => bump(&RAW_QUIC_CLIENT_CLOSE_BY_RELEASE),
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn record_io_uring_rx_in_flight(count: usize) {
    observe_max(&IO_URING_RX_IN_FLIGHT_HIGH_WATERMARK, count);
}

#[cfg(target_os = "linux")]
pub(crate) fn record_io_uring_tx_in_flight(count: usize) {
    observe_max(&IO_URING_TX_IN_FLIGHT_HIGH_WATERMARK, count);
}

#[cfg(target_os = "linux")]
pub(crate) fn record_io_uring_pending_tx(count: usize) {
    observe_max(&IO_URING_PENDING_TX_HIGH_WATERMARK, count);
}

#[cfg(target_os = "linux")]
pub(crate) fn record_io_uring_retryable_send_completion() {
    bump(&IO_URING_RETRYABLE_SEND_COMPLETIONS);
}

#[cfg(target_os = "linux")]
pub(crate) fn record_io_uring_submit_call() {
    bump(&IO_URING_SUBMIT_CALLS);
}

#[cfg(target_os = "linux")]
pub(crate) fn record_io_uring_submit_with_args_call() {
    bump(&IO_URING_SUBMIT_WITH_ARGS_CALLS);
}

#[cfg(target_os = "linux")]
pub(crate) fn record_io_uring_submitted_sqes(count: usize) {
    IO_URING_SUBMITTED_SQES_TOTAL.fetch_add(count as u64, Ordering::Relaxed);
}

#[cfg(target_os = "linux")]
pub(crate) fn record_io_uring_completions(count: usize) {
    IO_URING_COMPLETION_TOTAL.fetch_add(count as u64, Ordering::Relaxed);
    observe_max(&IO_URING_COMPLETION_BATCH_HIGH_WATERMARK, count);
}

#[cfg(target_os = "linux")]
pub(crate) fn record_io_uring_wake_completion() {
    bump(&IO_URING_WAKE_COMPLETIONS);
}

#[cfg(target_os = "linux")]
pub(crate) fn record_io_uring_wake_write() {
    bump(&IO_URING_WAKE_WRITES);
}

#[cfg(target_os = "linux")]
pub(crate) fn record_io_uring_timeout_poll() {
    bump(&IO_URING_TIMEOUT_POLLS);
}

#[cfg(target_os = "linux")]
pub(crate) fn record_io_uring_rx_datagrams(count: usize) {
    IO_URING_RX_DATAGRAMS_TOTAL.fetch_add(count as u64, Ordering::Relaxed);
}

#[cfg(target_os = "linux")]
pub(crate) fn record_io_uring_tx_datagrams_submitted(count: usize) {
    IO_URING_TX_DATAGRAMS_SUBMITTED_TOTAL.fetch_add(count as u64, Ordering::Relaxed);
}

#[cfg(target_os = "linux")]
pub(crate) fn record_io_uring_tx_datagrams_completed(count: usize) {
    IO_URING_TX_DATAGRAMS_COMPLETED_TOTAL.fetch_add(count as u64, Ordering::Relaxed);
}

#[cfg(target_os = "linux")]
pub(crate) fn record_io_uring_sq_full_event() {
    bump(&IO_URING_SQ_FULL_EVENTS);
}

#[cfg(target_os = "macos")]
pub(crate) fn record_kqueue_unsent_depth(count: usize) {
    observe_max(&KQUEUE_UNSENT_HIGH_WATERMARK, count);
}

#[cfg(target_os = "macos")]
pub(crate) fn record_kqueue_would_block_send() {
    bump(&KQUEUE_WOULD_BLOCK_SENDS);
}

#[cfg(target_os = "macos")]
pub(crate) fn record_kqueue_write_wakeup() {
    bump(&KQUEUE_WRITE_WAKEUPS);
}

pub(crate) fn record_tx_buffers_recycled(count: usize) {
    TX_BUFFERS_RECYCLED.fetch_add(count as u64, Ordering::Relaxed);
}

pub fn snapshot() -> JsReactorTelemetrySnapshot {
    JsReactorTelemetrySnapshot {
        driverSetupAttemptsTotal: load(&DRIVER_SETUP_ATTEMPTS_TOTAL),
        driverSetupSuccessTotal: load(&DRIVER_SETUP_SUCCESS_TOTAL),
        driverSetupFailureTotal: load(&DRIVER_SETUP_FAILURE_TOTAL),
        ioUringDriverSetupAttempts: load(&IO_URING_DRIVER_SETUP_ATTEMPTS),
        ioUringDriverSetupSuccesses: load(&IO_URING_DRIVER_SETUP_SUCCESSES),
        ioUringDriverSetupFailures: load(&IO_URING_DRIVER_SETUP_FAILURES),
        pollDriverSetupAttempts: load(&POLL_DRIVER_SETUP_ATTEMPTS),
        pollDriverSetupSuccesses: load(&POLL_DRIVER_SETUP_SUCCESSES),
        pollDriverSetupFailures: load(&POLL_DRIVER_SETUP_FAILURES),
        kqueueDriverSetupAttempts: load(&KQUEUE_DRIVER_SETUP_ATTEMPTS),
        kqueueDriverSetupSuccesses: load(&KQUEUE_DRIVER_SETUP_SUCCESSES),
        kqueueDriverSetupFailures: load(&KQUEUE_DRIVER_SETUP_FAILURES),
        workerThreadSpawnsTotal: load(&WORKER_THREAD_SPAWNS_TOTAL),
        rawQuicServerWorkerSpawns: load(&RAW_QUIC_SERVER_WORKER_SPAWNS),
        rawQuicClientDedicatedWorkerSpawns: load(&RAW_QUIC_CLIENT_DEDICATED_WORKER_SPAWNS),
        rawQuicClientSharedWorkersCreated: load(&RAW_QUIC_CLIENT_SHARED_WORKERS_CREATED),
        rawQuicClientSharedWorkerReuses: load(&RAW_QUIC_CLIENT_SHARED_WORKER_REUSES),
        h3ServerWorkerSpawns: load(&H3_SERVER_WORKER_SPAWNS),
        h3ClientDedicatedWorkerSpawns: load(&H3_CLIENT_DEDICATED_WORKER_SPAWNS),
        h3ClientSharedWorkersCreated: load(&H3_CLIENT_SHARED_WORKERS_CREATED),
        h3ClientSharedWorkerReuses: load(&H3_CLIENT_SHARED_WORKER_REUSES),
        clientLocalPortReuseHits: load(&CLIENT_LOCAL_PORT_REUSE_HITS),
        rawQuicClientSessionsOpened: load(&RAW_QUIC_CLIENT_SESSIONS_OPENED),
        rawQuicClientSessionsClosed: load(&RAW_QUIC_CLIENT_SESSIONS_CLOSED),
        rawQuicFinObservations: load(&RAW_QUIC_FIN_OBSERVATIONS),
        rawQuicFinishedEventEmits: load(&RAW_QUIC_FINISHED_EVENT_EMITS),
        rawQuicDrainEventEmits: load(&RAW_QUIC_DRAIN_EVENT_EMITS),
        rawQuicBlockedStreamHighWatermark: load(&RAW_QUIC_BLOCKED_STREAM_HIGH_WATERMARK),
        rawQuicClientPendingWriteHighWatermark: load(
            &RAW_QUIC_CLIENT_PENDING_WRITE_HIGH_WATERMARK,
        ),
        rawQuicClientReapsWithPendingWrites: load(&RAW_QUIC_CLIENT_REAPS_WITH_PENDING_WRITES),
        rawQuicClientReapsWithBlockedStreams: load(&RAW_QUIC_CLIENT_REAPS_WITH_BLOCKED_STREAMS),
        rawQuicClientReapsWithKnownStreams: load(&RAW_QUIC_CLIENT_REAPS_WITH_KNOWN_STREAMS),
        rawQuicClientCloseByPacket: load(&RAW_QUIC_CLIENT_CLOSE_BY_PACKET),
        rawQuicClientCloseByTimeout: load(&RAW_QUIC_CLIENT_CLOSE_BY_TIMEOUT),
        rawQuicClientCloseByShutdown: load(&RAW_QUIC_CLIENT_CLOSE_BY_SHUTDOWN),
        rawQuicClientCloseByRelease: load(&RAW_QUIC_CLIENT_CLOSE_BY_RELEASE),
        rawQuicServerSessionsOpened: load(&RAW_QUIC_SERVER_SESSIONS_OPENED),
        rawQuicServerSessionsClosed: load(&RAW_QUIC_SERVER_SESSIONS_CLOSED),
        h3ClientSessionsOpened: load(&H3_CLIENT_SESSIONS_OPENED),
        h3ClientSessionsClosed: load(&H3_CLIENT_SESSIONS_CLOSED),
        h3ServerSessionsOpened: load(&H3_SERVER_SESSIONS_OPENED),
        h3ServerSessionsClosed: load(&H3_SERVER_SESSIONS_CLOSED),
        ioUringRxInFlightHighWatermark: load(&IO_URING_RX_IN_FLIGHT_HIGH_WATERMARK),
        ioUringTxInFlightHighWatermark: load(&IO_URING_TX_IN_FLIGHT_HIGH_WATERMARK),
        ioUringPendingTxHighWatermark: load(&IO_URING_PENDING_TX_HIGH_WATERMARK),
        ioUringRetryableSendCompletions: load(&IO_URING_RETRYABLE_SEND_COMPLETIONS),
        ioUringSubmitCalls: load(&IO_URING_SUBMIT_CALLS),
        ioUringSubmitWithArgsCalls: load(&IO_URING_SUBMIT_WITH_ARGS_CALLS),
        ioUringSubmittedSqesTotal: load(&IO_URING_SUBMITTED_SQES_TOTAL),
        ioUringCompletionTotal: load(&IO_URING_COMPLETION_TOTAL),
        ioUringCompletionBatchHighWatermark: load(&IO_URING_COMPLETION_BATCH_HIGH_WATERMARK),
        ioUringWakeCompletions: load(&IO_URING_WAKE_COMPLETIONS),
        ioUringWakeWrites: load(&IO_URING_WAKE_WRITES),
        ioUringTimeoutPolls: load(&IO_URING_TIMEOUT_POLLS),
        ioUringRxDatagramsTotal: load(&IO_URING_RX_DATAGRAMS_TOTAL),
        ioUringTxDatagramsSubmittedTotal: load(&IO_URING_TX_DATAGRAMS_SUBMITTED_TOTAL),
        ioUringTxDatagramsCompletedTotal: load(&IO_URING_TX_DATAGRAMS_COMPLETED_TOTAL),
        ioUringSqFullEvents: load(&IO_URING_SQ_FULL_EVENTS),
        kqueueUnsentHighWatermark: load(&KQUEUE_UNSENT_HIGH_WATERMARK),
        kqueueWouldBlockSends: load(&KQUEUE_WOULD_BLOCK_SENDS),
        kqueueWriteWakeups: load(&KQUEUE_WRITE_WAKEUPS),
        txBuffersRecycled: load(&TX_BUFFERS_RECYCLED),
    }
}

pub fn reset() {
    for counter in [
        &DRIVER_SETUP_ATTEMPTS_TOTAL,
        &DRIVER_SETUP_SUCCESS_TOTAL,
        &DRIVER_SETUP_FAILURE_TOTAL,
        &IO_URING_DRIVER_SETUP_ATTEMPTS,
        &IO_URING_DRIVER_SETUP_SUCCESSES,
        &IO_URING_DRIVER_SETUP_FAILURES,
        &POLL_DRIVER_SETUP_ATTEMPTS,
        &POLL_DRIVER_SETUP_SUCCESSES,
        &POLL_DRIVER_SETUP_FAILURES,
        &KQUEUE_DRIVER_SETUP_ATTEMPTS,
        &KQUEUE_DRIVER_SETUP_SUCCESSES,
        &KQUEUE_DRIVER_SETUP_FAILURES,
        &WORKER_THREAD_SPAWNS_TOTAL,
        &RAW_QUIC_SERVER_WORKER_SPAWNS,
        &RAW_QUIC_CLIENT_DEDICATED_WORKER_SPAWNS,
        &RAW_QUIC_CLIENT_SHARED_WORKERS_CREATED,
        &RAW_QUIC_CLIENT_SHARED_WORKER_REUSES,
        &H3_SERVER_WORKER_SPAWNS,
        &H3_CLIENT_DEDICATED_WORKER_SPAWNS,
        &H3_CLIENT_SHARED_WORKERS_CREATED,
        &H3_CLIENT_SHARED_WORKER_REUSES,
        &CLIENT_LOCAL_PORT_REUSE_HITS,
        &RAW_QUIC_CLIENT_SESSIONS_OPENED,
        &RAW_QUIC_CLIENT_SESSIONS_CLOSED,
        &RAW_QUIC_FIN_OBSERVATIONS,
        &RAW_QUIC_FINISHED_EVENT_EMITS,
        &RAW_QUIC_DRAIN_EVENT_EMITS,
        &RAW_QUIC_BLOCKED_STREAM_HIGH_WATERMARK,
        &RAW_QUIC_CLIENT_PENDING_WRITE_HIGH_WATERMARK,
        &RAW_QUIC_CLIENT_REAPS_WITH_PENDING_WRITES,
        &RAW_QUIC_CLIENT_REAPS_WITH_BLOCKED_STREAMS,
        &RAW_QUIC_CLIENT_REAPS_WITH_KNOWN_STREAMS,
        &RAW_QUIC_CLIENT_CLOSE_BY_PACKET,
        &RAW_QUIC_CLIENT_CLOSE_BY_TIMEOUT,
        &RAW_QUIC_CLIENT_CLOSE_BY_SHUTDOWN,
        &RAW_QUIC_CLIENT_CLOSE_BY_RELEASE,
        &RAW_QUIC_SERVER_SESSIONS_OPENED,
        &RAW_QUIC_SERVER_SESSIONS_CLOSED,
        &H3_CLIENT_SESSIONS_OPENED,
        &H3_CLIENT_SESSIONS_CLOSED,
        &H3_SERVER_SESSIONS_OPENED,
        &H3_SERVER_SESSIONS_CLOSED,
        &IO_URING_RX_IN_FLIGHT_HIGH_WATERMARK,
        &IO_URING_TX_IN_FLIGHT_HIGH_WATERMARK,
        &IO_URING_PENDING_TX_HIGH_WATERMARK,
        &IO_URING_RETRYABLE_SEND_COMPLETIONS,
        &IO_URING_SUBMIT_CALLS,
        &IO_URING_SUBMIT_WITH_ARGS_CALLS,
        &IO_URING_SUBMITTED_SQES_TOTAL,
        &IO_URING_COMPLETION_TOTAL,
        &IO_URING_COMPLETION_BATCH_HIGH_WATERMARK,
        &IO_URING_WAKE_COMPLETIONS,
        &IO_URING_WAKE_WRITES,
        &IO_URING_TIMEOUT_POLLS,
        &IO_URING_RX_DATAGRAMS_TOTAL,
        &IO_URING_TX_DATAGRAMS_SUBMITTED_TOTAL,
        &IO_URING_TX_DATAGRAMS_COMPLETED_TOTAL,
        &IO_URING_SQ_FULL_EVENTS,
        &KQUEUE_UNSENT_HIGH_WATERMARK,
        &KQUEUE_WOULD_BLOCK_SENDS,
        &KQUEUE_WRITE_WAKEUPS,
        &TX_BUFFERS_RECYCLED,
    ] {
        reset_counter(counter);
    }
}
