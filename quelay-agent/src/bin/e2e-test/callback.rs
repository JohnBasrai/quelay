// ---------------------------------------------------------------------------
// TestCallbackEvent
// ---------------------------------------------------------------------------

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::*;

#[derive(Debug)]
pub enum TestCallbackEvent {
    // ---
    #[allow(unused)]
    Started {
        uuid: String,
        port: u16,
    },
    Done {
        #[allow(unused)]
        uuid: String,
        bytes: u64,
    },
    Failed {
        #[allow(unused)]
        uuid: String,
        reason: String,
    },
    #[allow(unused)]
    LinkState(LinkState),
    QueueStatus(QueueStatus),
}

// ---------------------------------------------------------------------------
// TestCallbackHandler
// ---------------------------------------------------------------------------

pub struct TestCallbackHandler {
    tx: Mutex<mpsc::Sender<TestCallbackEvent>>,
    progress_count: Arc<AtomicUsize>,
}

impl QueLayCallbackSyncHandler for TestCallbackHandler {
    // ---

    fn handle_ping(&self) -> thrift::Result<()> {
        Ok(())
    }

    fn handle_stream_started(
        &self,
        uuid: String,
        _info: StreamInfo,
        port: i32,
    ) -> thrift::Result<()> {
        tracing::info!(%uuid, port, "callback: stream_started");
        let _ = self.tx.lock().unwrap().send(TestCallbackEvent::Started {
            uuid,
            port: port as u16,
        });
        Ok(())
    }

    fn handle_stream_progress(
        &self,
        _uuid: String,
        _progress: quelay_thrift::ProgressInfo,
    ) -> thrift::Result<()> {
        self.progress_count.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn handle_stream_done(&self, uuid: String, bytes_transferred: i64) -> thrift::Result<()> {
        tracing::info!(%uuid, bytes_transferred, "callback: stream_done");
        let _ = self.tx.lock().unwrap().send(TestCallbackEvent::Done {
            uuid,
            bytes: bytes_transferred as u64,
        });
        Ok(())
    }

    fn handle_stream_failed(
        &self,
        uuid: String,
        _code: FailReason,
        reason: String,
    ) -> thrift::Result<()> {
        tracing::warn!(%uuid, %reason, "callback: stream_failed");
        let _ = self
            .tx
            .lock()
            .unwrap()
            .send(TestCallbackEvent::Failed { uuid, reason });
        Ok(())
    }

    fn handle_link_status_update(&self, state: LinkState) -> thrift::Result<()> {
        tracing::info!(?state, "callback: link_status_update");
        let _ = self
            .tx
            .lock()
            .unwrap()
            .send(TestCallbackEvent::LinkState(state));
        Ok(())
    }

    fn handle_queue_status_update(&self, status: QueueStatus) -> thrift::Result<()> {
        let _ = self
            .tx
            .lock()
            .unwrap()
            .send(TestCallbackEvent::QueueStatus(status));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// TestCallbackServer
// ---------------------------------------------------------------------------

pub struct TestCallbackServer {
    addr: SocketAddr,
    rx: mpsc::Receiver<TestCallbackEvent>,
    progress_count: Arc<AtomicUsize>,
}

impl TestCallbackServer {
    // ---

    pub fn bind() -> anyhow::Result<Self> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let addr_str = addr.to_string();
        let (tx, rx) = mpsc::channel();
        let progress_count = Arc::new(AtomicUsize::new(0));

        let handler = TestCallbackHandler {
            tx: Mutex::new(tx),
            progress_count: Arc::clone(&progress_count),
        };
        let processor = QueLayCallbackSyncProcessor::new(handler);
        let (ready_tx, ready_rx) = mpsc::channel::<()>();

        std::thread::Builder::new()
            .name("quelay-test-cb".into())
            .spawn(move || {
                drop(listener);
                let mut server = TServer::new(
                    TBufferedReadTransportFactory::new(),
                    TBinaryInputProtocolFactory::new(),
                    TBufferedWriteTransportFactory::new(),
                    TBinaryOutputProtocolFactory::new(),
                    processor,
                    4,
                );
                let _ = ready_tx.send(());
                if let Err(e) = server.listen(&addr_str) {
                    tracing::warn!("test callback server exiting: {e}");
                }
            })?;

        ready_rx
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| anyhow::anyhow!("test callback server did not start within 2s"))?;

        std::thread::sleep(Duration::from_millis(10));
        Ok(Self {
            addr,
            rx,
            progress_count,
        })
    }

    pub fn endpoint(&self) -> String {
        self.addr.to_string()
    }

    pub fn progress_count(&self) -> usize {
        self.progress_count.load(Ordering::Relaxed)
    }

    pub fn recv_event(&self, timeout: Duration) -> anyhow::Result<TestCallbackEvent> {
        self.rx
            .recv_timeout(timeout)
            .map_err(|e| anyhow::anyhow!("test callback recv timeout: {e}"))
    }

    /// Drain all pending events and return the last `QueueStatus` seen,
    /// or `None` if none arrived within `timeout`.  Other event types discarded.
    pub fn last_queue_status(&self, timeout: Duration) -> Option<QueueStatus> {
        let deadline = std::time::Instant::now() + timeout;
        let mut last: Option<QueueStatus> = None;

        while let Some(d) = deadline.checked_duration_since(std::time::Instant::now()) {
            let remaining = d;

            match self.rx.recv_timeout(remaining) {
                Ok(TestCallbackEvent::QueueStatus(s)) => last = Some(s),
                Ok(_) => {} // discard stream lifecycle and link events
                Err(_) => break,
            }
        }
        last
    }

    /// Like `recv_event`, but discards events whose UUID does not match
    /// `uuid`.  Use when multiple streams are active on a single callback
    /// endpoint and you need events for one specific stream.
    pub fn recv_event_for(
        &self,
        uuid: &str,
        timeout: Duration,
    ) -> anyhow::Result<TestCallbackEvent> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let remaining = deadline
                .checked_duration_since(std::time::Instant::now())
                .ok_or_else(|| {
                    anyhow::anyhow!("test callback recv timeout waiting for uuid={uuid}")
                })?;
            let event = self.rx.recv_timeout(remaining).map_err(|e| {
                anyhow::anyhow!("test callback recv timeout waiting for uuid={uuid}: {e}")
            })?;
            let event_uuid = match &event {
                TestCallbackEvent::Started { uuid, .. } => Some(uuid.as_str()),
                TestCallbackEvent::Done { uuid, .. } => Some(uuid.as_str()),
                TestCallbackEvent::Failed { uuid, .. } => Some(uuid.as_str()),
                TestCallbackEvent::LinkState(_) => None,
                TestCallbackEvent::QueueStatus(_) => None,
            };
            if event_uuid == Some(uuid) {
                return Ok(event);
            }
            // Not our stream — discard and keep waiting.
            tracing::debug!(
                event_uuid = ?event_uuid,
                wanted = uuid,
                "recv_event_for: discarding event for other stream"
            );
        }
    }
}
