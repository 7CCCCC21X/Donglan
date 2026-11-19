//! The loop's small tools: an event (asyncio.Event's set / clear / wait), a task started at once (asyncio's eager start: its
//! future runs now, in this turn, up to its first real wait, before anything else gets a turn), timers.
use std::cell::Cell;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

#[derive(Default)]
pub struct Event {
    flag: Cell<bool>,
    notify: Notify,
}

impl Event {
    pub fn new() -> Event {
        Event::default()
    }
    pub fn set(&self) {
        self.flag.set(true);
        self.notify.notify_waiters();
    }
    pub fn clear(&self) {
        self.flag.set(false);
    }
    pub fn is_set(&self) -> bool {
        self.flag.get()
    }
    pub async fn wait(&self) {
        loop {
            let notified = self.notify.notified();
            if self.flag.get() {
                return;
            }
            notified.await;
        }
    }
    /// ``wait`` for at most ``seconds``: true when set.
    pub async fn wait_for(&self, seconds: f64) -> bool {
        tokio::time::timeout(std::time::Duration::from_secs_f64(seconds.max(0.0)), self.wait()).await.is_ok()
    }
}

/// The future run now up to its first real wait (its request's bytes out in this very call), then left to the loop.
pub fn start_now<F: Future<Output = ()> + 'static>(fut: F) -> Option<JoinHandle<()>> {
    // (unconstrained: the loop's cooperative budget, spent by whatever ran before in this turn, never makes the look or the
    // order's request yield on the way — its bytes go out now)
    let mut boxed: Pin<Box<dyn Future<Output = ()>>> = Box::pin(tokio::task::unconstrained(fut));
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    match boxed.as_mut().poll(&mut cx) {
        Poll::Ready(()) => None,
        // (polled again at once by its task with its own waker: a wake-up meant for the first poll is not lost — the readiness
        // it waited for is there to be seen)
        Poll::Pending => Some(tokio::task::spawn_local(boxed)),
    }
}

pub fn spawn<F: Future<Output = ()> + 'static>(fut: F) -> JoinHandle<()> {
    tokio::task::spawn_local(fut)
}

pub fn running(task: &Option<JoinHandle<()>>) -> bool {
    task.as_ref().map(|t| !t.is_finished()).unwrap_or(false)
}

pub async fn sleep(seconds: f64) {
    tokio::time::sleep(std::time::Duration::from_secs_f64(seconds.max(0.0))).await
}
