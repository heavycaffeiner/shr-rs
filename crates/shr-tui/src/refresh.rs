use std::{
    sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    thread,
};

use crate::app::Snapshot;

pub type RefreshResult = Result<Snapshot, String>;

/// Runs potentially slow system inspection away from the terminal event loop.
///
/// At most one inspection can be in flight. Repeated timer ticks or `r` key
/// presses are coalesced so slow `smartctl` calls cannot create an unbounded
/// queue of probes.
pub struct RefreshWorker {
    requests: SyncSender<()>,
    results: Receiver<RefreshResult>,
    in_flight: bool,
}

impl RefreshWorker {
    pub fn spawn<F>(inspect: F) -> Self
    where
        F: Fn() -> RefreshResult + Send + 'static,
    {
        let (request_tx, request_rx) = mpsc::sync_channel::<()>(1);
        let (result_tx, result_rx) = mpsc::sync_channel::<RefreshResult>(1);
        thread::Builder::new()
            .name("shr-tui-inspect".into())
            .spawn(move || {
                while request_rx.recv().is_ok() {
                    if result_tx.send(inspect()).is_err() {
                        break;
                    }
                }
            })
            .expect("failed to start SHR-RS inspection worker");

        Self {
            requests: request_tx,
            results: result_rx,
            in_flight: false,
        }
    }

    /// Schedule a refresh without blocking. Returns false when a probe is
    /// already running or the worker has stopped.
    pub fn request(&mut self) -> bool {
        if self.in_flight {
            return false;
        }
        match self.requests.try_send(()) {
            Ok(()) => {
                self.in_flight = true;
                true
            }
            Err(TrySendError::Full(())) | Err(TrySendError::Disconnected(())) => false,
        }
    }

    /// Poll for a completed refresh without blocking the terminal event loop.
    pub fn try_result(&mut self) -> Option<RefreshResult> {
        match self.results.try_recv() {
            Ok(result) => {
                self.in_flight = false;
                Some(result)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) if self.in_flight => {
                self.in_flight = false;
                Some(Err("inspection worker stopped unexpectedly".into()))
            }
            Err(TryRecvError::Disconnected) => None,
        }
    }

    pub fn is_in_flight(&self) -> bool {
        self.in_flight
    }
}
