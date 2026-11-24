//! Questions the daemon needs a human to answer (2FA, certificate limit).
//! Prompts are broadcast to interactive clients; the first `respond` wins.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use loadside_core::protocol::{Event, Failure, Prompt};
use serde_json::Value;
use tokio::sync::{broadcast, oneshot};

pub struct Prompter {
    pending: Mutex<HashMap<String, (Prompt, oneshot::Sender<Value>)>>,
    interactive: AtomicUsize,
    events: broadcast::Sender<Event>,
}

impl Prompter {
    pub fn new(events: broadcast::Sender<Event>) -> Self {
        Self { pending: Mutex::default(), interactive: AtomicUsize::new(0), events }
    }

    pub fn has_interactive_client(&self) -> bool {
        self.interactive.load(Ordering::SeqCst) > 0
    }

    pub fn client_connected(&self) {
        self.interactive.fetch_add(1, Ordering::SeqCst);
    }

    pub fn client_disconnected(&self) {
        // Same lock as `ask`'s check-and-insert, so a prompt can't slip in
        // right after the last client left and wait out its full timeout.
        let mut pending = self.pending.lock().unwrap();
        if self.interactive.fetch_sub(1, Ordering::SeqCst) == 1 {
            // Nobody left to answer: fail outstanding prompts right away.
            let drained: Vec<_> = pending.drain().collect();
            drop(pending);
            for (id, _) in drained {
                let _ = self.events.send(Event::PromptClosed { id });
            }
        }
    }

    pub fn open(&self) -> Vec<Prompt> {
        let mut v: Vec<_> = self.pending.lock().unwrap().values().map(|(p, _)| p.clone()).collect();
        v.sort_by_key(|p| p.created_at);
        v
    }

    pub fn respond(&self, id: &str, value: Value) -> bool {
        match self.pending.lock().unwrap().remove(id) {
            Some((_, tx)) => {
                let _ = tx.send(value);
                let _ = self.events.send(Event::PromptClosed { id: id.to_string() });
                true
            }
            None => false,
        }
    }

    pub async fn ask(&self, kind: &str, payload: Value, timeout: Duration) -> Result<Value, Failure> {
        let id = uuid_like();
        let prompt = Prompt { id: id.clone(), kind: kind.into(), payload, created_at: loadside_core::now() };
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().unwrap();
            if !self.has_interactive_client() {
                return Err(Failure::new("needs_interaction", "Loadside needs your input, but no Loadside window is open.")
                    .with_hint("Open the Loadside app (or run `loadside login`) and try again."));
            }
            pending.insert(id.clone(), (prompt.clone(), tx));
        }
        let _ = self.events.send(Event::Prompt { prompt });
        let r = tokio::time::timeout(timeout, rx).await;
        if self.pending.lock().unwrap().remove(&id).is_some() {
            let _ = self.events.send(Event::PromptClosed { id });
        }
        match r {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(_)) => Err(Failure::new("prompt_cancelled", "The request for input was closed before it was answered.")),
            Err(_) => Err(Failure::new("prompt_timeout", "Timed out waiting for your answer.")),
        }
    }
}

fn uuid_like() -> String {
    use std::sync::atomic::AtomicU64;
    static N: AtomicU64 = AtomicU64::new(1);
    format!("p{}-{}", loadside_core::now(), N.fetch_add(1, Ordering::Relaxed))
}
