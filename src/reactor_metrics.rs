#![allow(non_snake_case)]

use std::sync::atomic::{AtomicU64, Ordering};

use napi_derive::napi;

use crate::transport::RuntimeDriverKind;

#[napi(object)]
#[derive(Clone, Debug, Default)]
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
    pub rawQuicServerSessionsOpened: i64,
    pub rawQuicServerSessionsClosed: i64,
    pub h3ClientSessionsOpened: i64,
    pub h3ClientSessionsClosed: i64,
    pub h3ServerSessionsOpened: i64,
    pub h3ServerSessionsClosed: i64,
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
static RAW_QUIC_SERVER_SESSIONS_OPENED: AtomicU64 = AtomicU64::new(0);
static RAW_QUIC_SERVER_SESSIONS_CLOSED: AtomicU64 = AtomicU64::new(0);
static H3_CLIENT_SESSIONS_OPENED: AtomicU64 = AtomicU64::new(0);
static H3_CLIENT_SESSIONS_CLOSED: AtomicU64 = AtomicU64::new(0);
static H3_SERVER_SESSIONS_OPENED: AtomicU64 = AtomicU64::new(0);
static H3_SERVER_SESSIONS_CLOSED: AtomicU64 = AtomicU64::new(0);

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

pub(crate) fn record_driver_setup_attempt(kind: RuntimeDriverKind) {
    bump(&DRIVER_SETUP_ATTEMPTS_TOTAL);
    match kind {
        RuntimeDriverKind::IoUring => bump(&IO_URING_DRIVER_SETUP_ATTEMPTS),
        RuntimeDriverKind::Poll => bump(&POLL_DRIVER_SETUP_ATTEMPTS),
        RuntimeDriverKind::Kqueue => bump(&KQUEUE_DRIVER_SETUP_ATTEMPTS),
    }
}

pub(crate) fn record_driver_setup_success(kind: RuntimeDriverKind) {
    bump(&DRIVER_SETUP_SUCCESS_TOTAL);
    match kind {
        RuntimeDriverKind::IoUring => bump(&IO_URING_DRIVER_SETUP_SUCCESSES),
        RuntimeDriverKind::Poll => bump(&POLL_DRIVER_SETUP_SUCCESSES),
        RuntimeDriverKind::Kqueue => bump(&KQUEUE_DRIVER_SETUP_SUCCESSES),
    }
}

pub(crate) fn record_driver_setup_failure(kind: RuntimeDriverKind) {
    bump(&DRIVER_SETUP_FAILURE_TOTAL);
    match kind {
        RuntimeDriverKind::IoUring => bump(&IO_URING_DRIVER_SETUP_FAILURES),
        RuntimeDriverKind::Poll => bump(&POLL_DRIVER_SETUP_FAILURES),
        RuntimeDriverKind::Kqueue => bump(&KQUEUE_DRIVER_SETUP_FAILURES),
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
        rawQuicServerSessionsOpened: load(&RAW_QUIC_SERVER_SESSIONS_OPENED),
        rawQuicServerSessionsClosed: load(&RAW_QUIC_SERVER_SESSIONS_CLOSED),
        h3ClientSessionsOpened: load(&H3_CLIENT_SESSIONS_OPENED),
        h3ClientSessionsClosed: load(&H3_CLIENT_SESSIONS_CLOSED),
        h3ServerSessionsOpened: load(&H3_SERVER_SESSIONS_OPENED),
        h3ServerSessionsClosed: load(&H3_SERVER_SESSIONS_CLOSED),
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
        &RAW_QUIC_SERVER_SESSIONS_OPENED,
        &RAW_QUIC_SERVER_SESSIONS_CLOSED,
        &H3_CLIENT_SESSIONS_OPENED,
        &H3_CLIENT_SESSIONS_CLOSED,
        &H3_SERVER_SESSIONS_OPENED,
        &H3_SERVER_SESSIONS_CLOSED,
        &TX_BUFFERS_RECYCLED,
    ] {
        reset_counter(counter);
    }
}
