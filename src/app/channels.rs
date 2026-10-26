//! Channel helpers shared by the app loop and the workers: non-blocking
//! drains and sends with drop counting, and the bounded worker join.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError};
use std::thread;
use std::time::Duration;

/// Drain every currently queued value without blocking.
///
/// `Ok(())` means the queue is empty. `Err(())` means the sender is gone: the
/// worker stopped, which must not be mistaken for a temporarily empty queue.
pub(super) fn drain_try_recv<T>(rx: &Receiver<T>, mut apply: impl FnMut(T)) -> Result<(), ()> {
    loop {
        match rx.try_recv() {
            Ok(value) => apply(value),
            Err(TryRecvError::Empty) => return Ok(()),
            Err(TryRecvError::Disconnected) => return Err(()),
        }
    }
}

/// Send without blocking. A full channel keeps the newest UI state and counts
/// the dropped update; `false` means the receiver is gone and the worker must
/// stop.
pub(super) fn try_send_snapshot<T>(tx: &SyncSender<T>, value: T, drops: &AtomicU64) -> bool {
    match tx.try_send(value) {
        Ok(()) => true,
        Err(TrySendError::Full(_)) => {
            drops.fetch_add(1, Ordering::Relaxed);
            true
        }
        Err(TrySendError::Disconnected(_)) => false,
    }
}

/// Wait up to `timeout` for a worker and preserve its actual outcome.
///
/// A panicked worker, a shutdown timeout and a broken watcher are distinct
/// errors. The watcher keeps a wedged worker from blocking the process
/// indefinitely; on timeout it is left detached.
pub(super) fn join_bounded(
    handle: thread::JoinHandle<()>,
    name: &str,
    timeout: Duration,
) -> Result<(), String> {
    let (done_tx, done_rx) = mpsc::channel::<thread::Result<()>>();
    thread::spawn(move || {
        let _ = done_tx.send(handle.join());
    });
    match done_rx.recv_timeout(timeout) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) => Err(format!("{name} worker panicked")),
        Err(RecvTimeoutError::Timeout) => Err(format!("{name} worker shutdown timed out")),
        Err(RecvTimeoutError::Disconnected) => Err(format!("{name} join watcher disconnected")),
    }
}
