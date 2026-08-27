pub trait EventEmitter: Send + Sync {
    fn emit_event(&self, event: &str, payload: &str) -> Result<(), String>;
}

#[derive(Clone)]
pub struct TauriEmitter {
    handle: tauri::AppHandle,
}

impl TauriEmitter {
    pub fn new(handle: tauri::AppHandle) -> Self {
        Self { handle }
    }
}

impl EventEmitter for TauriEmitter {
    fn emit_event(&self, event: &str, payload: &str) -> Result<(), String> {
        use tauri::Emitter;
        self.handle
            .emit(event, payload.to_string())
            .map_err(|e| e.to_string())
    }
}

#[derive(Clone)]
pub struct BroadcasterEmitter {
    tx: tokio::sync::broadcast::Sender<String>,
}

impl BroadcasterEmitter {
    pub fn new(tx: tokio::sync::broadcast::Sender<String>) -> Self {
        Self { tx }
    }
}

impl EventEmitter for BroadcasterEmitter {
    fn emit_event(&self, event: &str, payload: &str) -> Result<(), String> {
        let msg = serde_json::json!({
            "event": event,
            "payload": payload
        })
        .to_string();
        let _ = self.tx.send(msg);
        Ok(())
    }
}

#[derive(Clone)]
pub struct NoopEmitter;

impl EventEmitter for NoopEmitter {
    fn emit_event(&self, _event: &str, _payload: &str) -> Result<(), String> {
        Ok(())
    }
}
