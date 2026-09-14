//! Bounded, nonblocking queues. Overflow makes the entire connection fail closed.
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
};
pub const CAPACITY: usize = 1024;
pub struct Sender<T> {
    inner: mpsc::SyncSender<T>,
    failed: Arc<AtomicBool>,
}
impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            failed: self.failed.clone(),
        }
    }
}
impl<T> Sender<T> {
    pub fn send(&self, value: T) -> Result<(), mpsc::TrySendError<T>> {
        if self.failed() {
            return Err(mpsc::TrySendError::Disconnected(value));
        }
        self.inner.try_send(value).map_err(|error| {
            self.failed.store(true, Ordering::Release);
            error
        })
    }
    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }
    pub fn failure_flag(&self) -> Arc<AtomicBool> {
        self.failed.clone()
    }
}
pub fn channel<T>() -> (Sender<T>, mpsc::Receiver<T>) {
    channel_with_flag(CAPACITY, Arc::new(AtomicBool::new(false)))
}
pub fn channel_with_flag<T>(
    capacity: usize,
    failed: Arc<AtomicBool>,
) -> (Sender<T>, mpsc::Receiver<T>) {
    let (inner, receiver) = mpsc::sync_channel(capacity);
    (Sender { inner, failed }, receiver)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn overflow_is_nonblocking_and_terminal_for_both_queues() {
        let flag = Arc::new(AtomicBool::new(false));
        let (a, ar) = channel_with_flag(1, flag.clone());
        let (b, _br) = channel_with_flag(1, flag);
        a.send(1).unwrap();
        assert!(matches!(a.send(2), Err(mpsc::TrySendError::Full(2))));
        assert!(b.send(3).is_err());
        assert_eq!(ar.recv().unwrap(), 1);
        assert!(a.send(4).is_err());
    }
}
