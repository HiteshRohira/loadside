//! Async client for the daemon's Unix socket.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::protocol::{Event, Failure, Request, ServerMessage};

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, Failure>>>>>;

pub struct Client {
    writer: Mutex<tokio::net::unix::OwnedWriteHalf>,
    pending: Pending,
    next_id: AtomicU64,
    events: Mutex<Option<mpsc::UnboundedReceiver<Event>>>,
}

impl Client {
    pub async fn connect(path: &Path) -> Result<Self> {
        let stream = UnixStream::connect(path)
            .await
            .with_context(|| format!("couldn't connect to loadsided at {}", path.display()))?;
        let (r, w) = stream.into_split();
        let pending: Pending = Arc::default();
        let (etx, erx) = mpsc::unbounded_channel();
        let p = pending.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(r).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                match serde_json::from_str::<ServerMessage>(&line) {
                    Ok(ServerMessage::Response { id, result, error }) => {
                        if let Some(tx) = p.lock().await.remove(&id) {
                            let _ = tx.send(match error {
                                Some(e) => Err(e),
                                None => Ok(result.unwrap_or(Value::Null)),
                            });
                        }
                    }
                    Ok(ServerMessage::Event { event }) => {
                        let _ = etx.send(event);
                    }
                    Err(_) => {}
                }
            }
            // Connection closed: fail everything still waiting.
            for (_, tx) in p.lock().await.drain() {
                let _ = tx.send(Err(Failure::new("daemon_gone", "Lost connection to loadsided.")));
            }
        });
        Ok(Self { writer: Mutex::new(w), pending, next_id: AtomicU64::new(1), events: Mutex::new(Some(erx)) })
    }

    pub async fn connect_default() -> Result<Self> {
        Self::connect(&crate::paths::socket_path()).await
    }

    pub async fn call(&self, method: &str, params: Value) -> Result<Value, Failure> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let mut line = serde_json::to_string(&Request { id, method: method.into(), params })
            .map_err(|e| Failure::new("encode", e.to_string()))?;
        line.push('\n');
        if let Err(e) = self.writer.lock().await.write_all(line.as_bytes()).await {
            self.pending.lock().await.remove(&id);
            return Err(Failure::new("daemon_gone", format!("Lost connection to loadsided: {e}")));
        }
        rx.await.unwrap_or_else(|_| Err(Failure::new("daemon_gone", "Lost connection to loadsided.")))
    }

    /// Take the event stream (after calling `subscribe`). Only once.
    pub async fn take_events(&self) -> Option<mpsc::UnboundedReceiver<Event>> {
        self.events.lock().await.take()
    }
}
