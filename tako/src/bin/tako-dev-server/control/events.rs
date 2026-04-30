use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use crate::protocol::Response;

#[derive(Clone, Default)]
pub(crate) struct EventsHub {
    subs: Arc<Mutex<Vec<mpsc::UnboundedSender<Response>>>>,
}
impl EventsHub {
    pub(crate) fn subscribe(&self) -> mpsc::UnboundedReceiver<Response> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.subs.lock().unwrap().push(tx);
        rx
    }

    pub(crate) fn broadcast(&self, r: Response) {
        let mut subs = self.subs.lock().unwrap();
        subs.retain(|tx| tx.send(r.clone()).is_ok());
    }
}
