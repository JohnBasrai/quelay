//! Cambat Information Center (CIC) — message types and dispatch loop.
//!
//! CIC is the center nerve center of `bw-cap-test`.  It owns:
//! - Both agent C2I handles (sender + receiver)
//! - The master dispatch map `HashMap<(uuid, Role), mpsc::Sender<TunerCmd>>`
//! - The `Vec<TunerPair>` for coordinated shutdown
//! - The duration timer and failsafe kill timer
//!
//! The [`Cic::run`] loop drives a `select!` over:
//! 1. `cic_rx`        — messages from callbacks and tuner tasks
//! 2. Duration timer  — fires [`TunerCmd::Shutdown`] to all sender tuners
//! 3. Failsafe timer  — fires [`TunerCmd::Kill`] to everything still alive

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::time::Duration;

// ---

//e anyhow::Context as _;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

// ---

use quelay_thrift::LinkState;
use quelay_thrift::StreamInfo;

// ---

use super::Role;
use super::TunerCmd;
//use super::TunerOutcome;
use super::TunerResult;

// ---------------------------------------------------------------------------
// CicMsg
// ---------------------------------------------------------------------------

/// All messages that flow into CIC — from callback actors and tuner tasks.
#[derive(Debug)]
pub enum CicMsg {
    // --- from callbacks (UUID-bearing → routed to the matching tuner) ---
    /// Agent opened an ephemeral port for this stream.
    StreamStarted {
        role: Role,
        uuid: String,
        info: StreamInfo,
        port: u16,
    },

    /// Agent finished transferring this stream normally.
    StreamDone {
        role: Role,
        uuid: String,
        bytes: u64,
    },

    /// Agent reported a stream failure.
    StreamFailed {
        role: Role,
        uuid: String,
        reason: String,
    },

    // --- from callbacks (no UUID → CIC handles directly) ---
    /// Link state changed on one side.
    LinkStatus { role: Role, state: LinkState },

    // --- from tuner tasks ---
    /// A tuner task has completed and is about to exit.
    TunerFinished {
        uuid: String,
        role: Role,
        result: TunerResult,
    },
}

// ---------------------------------------------------------------------------
// TunerPair
// ---------------------------------------------------------------------------

/// A matched (sender-uuid, receiver-uuid) pair.
///
/// In the current single-process model both fields carry the same UUID value —
/// the same UUID flows through the air agent and arrives at the ground agent.
/// The [`Role`] discriminant in the master map prevents key collisions.
pub struct TunerPair {
    // ---
    pub sender_uuid: String,
    pub receiver_uuid: String,
}

// ---------------------------------------------------------------------------
// CicHandle
// ---------------------------------------------------------------------------

/// Cloneable handle tuner tasks use to send [`CicMsg`]s back to CIC.
#[derive(Clone)]
pub struct CicHandle {
    // ---
    pub tx: mpsc::Sender<CicMsg>,
}

// ---------------------------------------------------------------------------
// CicConfig
// ---------------------------------------------------------------------------

/// Construction-time configuration for [`Cic`].
pub struct CicConfig {
    // ---
    pub sender_c2i: SocketAddr,
    pub receiver_c2i: SocketAddr,

    /// How long each sender tuner transmits before receiving [`TunerCmd::Shutdown`].
    pub duration: Duration,

    /// Additional time after `duration` before the failsafe [`TunerCmd::Kill`] fires.
    pub kill_headroom: Duration,
}

// ---------------------------------------------------------------------------
// Cic
// ---------------------------------------------------------------------------

type TunerJobKey = (String, Role);

/// Central Intelligence Controller.
pub struct Cic {
    // ---
    cfg: CicConfig,
    cic_rx: mpsc::Receiver<CicMsg>,
    cic_tx: mpsc::Sender<CicMsg>,

    /// Master dispatch map: `(uuid, role)` → tuner command channel.
    dispatch: HashMap<TunerJobKey, mpsc::Sender<TunerCmd>>,

    /// Paired UUIDs — used for coordinated shutdown.
    pairs: Vec<TunerPair>,

    /// Join handles for all spawned tuner tasks (cleanup only — results arrive
    /// via [`CicMsg::TunerFinished`]).
    #[allow(dead_code)]
    handles: Vec<(TunerJobKey, JoinHandle<anyhow::Result<()>>)>,

    /// Collected results, filled as [`CicMsg::TunerFinished`] messages arrive.
    results: Vec<TunerResult>,

    /// Keep track of Tuner have been given a StreamStarted
    ported: HashSet<TunerJobKey>,
}

// ---

impl Cic {
    /// Create a new CIC.  Returns the instance and a [`mpsc::Sender`] that
    /// callback actors and tuner tasks use to reach CIC.
    pub fn new(cfg: CicConfig) -> (Self, mpsc::Sender<CicMsg>) {
        // ---
        let (tx, rx) = mpsc::channel(256);
        let cic = Self {
            cfg,
            cic_rx: rx,
            cic_tx: tx.clone(),
            dispatch: HashMap::new(),
            pairs: Vec::new(),
            handles: Vec::new(),
            results: Vec::new(),
            ported: HashSet::new(),
        };
        (cic, tx)
    }

    /// Register a tuner task before starting the dispatch loop.
    pub fn register(
        &mut self,
        uuid: String,
        role: Role,
        cmd_tx: mpsc::Sender<TunerCmd>,
        handle: JoinHandle<anyhow::Result<()>>,
    ) {
        // ---
        self.dispatch.insert((uuid.clone(), role), cmd_tx);
        self.handles.push(((uuid, role), handle));
    }

    /// Register a [`TunerPair`] (once per sender+receiver pair).
    pub fn register_pair(&mut self, pair: TunerPair) {
        // ---
        self.pairs.push(pair);
    }

    /// Return a [`CicHandle`] suitable for cloning into tuner tasks.
    pub fn handle(&self) -> CicHandle {
        // ---
        CicHandle {
            tx: self.cic_tx.clone(),
        }
    }

    // ---------------------------------------------------------------------------
    // Main dispatch loop
    // ---------------------------------------------------------------------------

    /// Run the CIC dispatch loop until all tuners have finished or the
    /// failsafe timer fires.  Returns collected [`TunerResult`]s.
    pub async fn run(mut self) -> anyhow::Result<Vec<TunerResult>> {
        // ---
        let total_tasks = self.handles.len();
        let duration = self.cfg.duration;
        let kill_after = duration + self.cfg.kill_headroom;

        let duration_sleep = tokio::time::sleep(duration);
        let failsafe_sleep = tokio::time::sleep(kill_after);

        tokio::pin!(duration_sleep);
        tokio::pin!(failsafe_sleep);

        let mut shutdown_sent = false;

        loop {
            tokio::select! {
                biased;

                // Failsafe always wins over duration if both ready.
                () = &mut failsafe_sleep => {
                    tracing::warn!("CIC: failsafe timer fired — killing all tuners");
                    self.broadcast(TunerCmd::Kill).await;
                    break;
                }

                () = &mut duration_sleep, if !shutdown_sent => {
                    tracing::info!("CIC: duration elapsed — shutting down sender tuners");
                    self.shutdown_senders().await;
                    shutdown_sent = true;
                }

                Some(msg) = self.cic_rx.recv() => {
                    match msg {
                        CicMsg::StreamStarted { role, uuid, port, .. } => {
                            let key = (uuid.clone(), role);
                            if self.ported.insert(key) {
                                self.route(&uuid, role, TunerCmd::Port(port)).await;
                            } else {
                                tracing::debug!(%uuid, ?role, port, "CIC: ignoring duplicate StreamStarted");
                            }
                        }

                        CicMsg::StreamDone { role, uuid, bytes } => {
                            self.route(&uuid, role, TunerCmd::Done { role, bytes }).await;
                        }

                        CicMsg::StreamFailed { role, uuid, reason } => {
                            self.route(&uuid, role, TunerCmd::Failed { role, reason }).await;
                        }

                        CicMsg::LinkStatus { role, state } => {
                            tracing::info!(?role, ?state, "CIC: link status");
                        }

                        CicMsg::TunerFinished { uuid, role, result } => {
                            tracing::info!(%uuid, ?role, ?result.outcome, "CIC: tuner finished");
                            self.dispatch.remove(&(uuid, role));
                            self.results.push(result);
                            if self.results.len() == total_tasks {
                                tracing::info!("CIC: all tuners finished");
                                break;
                            }
                        }
                    }
                }

                else => break,
            }
        }

        // Join all handles for cleanup; results already collected via channel.
        for ((uuid, role), handle) in self.handles {
            match tokio::time::timeout(Duration::from_secs(5), handle).await {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(e))) => tracing::warn!(%uuid, ?role, "tuner task error: {e}"),
                Ok(Err(_)) => tracing::warn!(%uuid, ?role, "tuner task panicked"),
                Err(_) => tracing::warn!(%uuid, ?role, "tuner task drain timeout"),
            }
        }

        Ok(self.results)
    }

    // ---------------------------------------------------------------------------
    // Helpers
    // ---------------------------------------------------------------------------

    async fn route(&self, uuid: &str, role: Role, cmd: TunerCmd) {
        // ---
        match self.dispatch.get(&(uuid.to_string(), role)) {
            Some(tx) => {
                if tx.send(cmd).await.is_err() {
                    tracing::warn!(%uuid, ?role, "CIC: tuner channel closed");
                }
            }
            None => tracing::warn!(%uuid, ?role, "CIC: no tuner for uuid+role"),
        }
    }

    async fn shutdown_senders(&self) {
        // ---
        for pair in &self.pairs {
            self.route(&pair.sender_uuid, Role::Sender, TunerCmd::Shutdown)
                .await;
        }
    }

    async fn broadcast(&self, cmd: TunerCmd)
    where
        TunerCmd: Clone,
    {
        // ---
        for ((uuid, role), tx) in &self.dispatch {
            if tx.send(cmd.clone()).await.is_err() {
                tracing::warn!(%uuid, ?role, "CIC: broadcast send failed");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Post-run BW assertion
// ---------------------------------------------------------------------------

/// Validate that aggregate sender throughput is within `±tolerance` of `cap_mbps`.
///
/// Uses only [`Role::Sender`] results — the sender bytes reflect what the rate
/// limiter actually shaped.
pub fn assert_aggregate_bw(
    results: &[TunerResult],
    cap_mbps: u32,
    wall_elapsed: Duration,
    tolerance: f64,
) -> anyhow::Result<()> {
    // ---
    let total_bytes: u64 = results
        .iter()
        .filter(|r| r.role == Role::Sender)
        .map(|r| r.bytes)
        .sum();

    let cap_bps = cap_mbps as f64 * 1_000_000.0 / 8.0;
    let realized_bps = total_bytes as f64 / wall_elapsed.as_secs_f64();
    let low = cap_bps * (1.0 - tolerance);
    let high = cap_bps * (1.0 + tolerance);

    println!(
        "\n  Aggregate BW: {:.1} KB/s  cap: {:.1} KB/s  ({:.1}%)",
        realized_bps / 1_000.0,
        cap_bps / 1_000.0,
        realized_bps / cap_bps * 100.0,
    );

    anyhow::ensure!(
        realized_bps >= low && realized_bps <= high,
        "Aggregate BW outside ±{:.0}% tolerance: \
         realized {:.1} KB/s, cap {:.1} KB/s (expected [{:.1}, {:.1}])",
        tolerance * 100.0,
        realized_bps / 1_000.0,
        cap_bps / 1_000.0,
        low / 1_000.0,
        high / 1_000.0,
    );

    println!("  Aggregate BW within ±{:.0}% ✓", tolerance * 100.0);
    Ok(())
}
