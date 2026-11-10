use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use crate::protocol::Response;

#[derive(Clone, Default)]
pub(crate) struct EventsHub {
    subs: Arc<Mutex<Vec<mpsc::Sender<Response>>>>,
}
impl EventsHub {
    pub(crate) fn subscribe(&self) -> mpsc::Receiver<Response> {
        let (tx, rx) = mpsc::channel(512);
        self.subs.lock().unwrap().push(tx);
        rx
    }

    pub(crate) fn broadcast(&self, r: Response) {
        let mut subs = self.subs.lock().unwrap();
        // Disconnect a lagging status subscriber so it can reconnect and obtain
        // current state, rather than retaining an unbounded stale event queue.
        subs.retain(|tx| tx.try_send(r.clone()).is_ok());
    }
}
