use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::sleep;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::hook::VolumeEvent;

const WS_URL: &str = "ws://localhost:14564/api/websocket";
const VOLUME_STEP: i32 = 5;
const VOLUME_MIN: i32 = 0;
const VOLUME_MAX: i32 = 255;
const VOLUME_FALLBACK: i32 = 128;
const RECONNECT_DELAY: Duration = Duration::from_secs(3);

/// Outgoing WebSocket request envelope.
#[derive(Serialize)]
struct DaemonRequest {
    id: u64,
    data: Value,
}

/// Incoming WebSocket response envelope.
#[derive(Deserialize)]
struct DaemonResponse {
    #[allow(dead_code)]
    id: u64,
    data: Value,
}

pub async fn run_client(
    mut rx: UnboundedReceiver<VolumeEvent>,
    active_channel: Arc<RwLock<String>>,
) {
    loop {
        if connect_and_run(&mut rx, &active_channel).await.is_err() {
            // No console in windows_subsystem = "windows"; just retry.
            sleep(RECONNECT_DELAY).await;
            continue;
        }
        // Channel closed cleanly: exit.
        return;
    }
}

async fn connect_and_run(
    rx: &mut UnboundedReceiver<VolumeEvent>,
    active_channel: &Arc<RwLock<String>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let (ws_stream, _) = connect_async(WS_URL).await?;
    let (mut write, mut read) = ws_stream.split();

    let mut next_id: u64 = 1;

    // 1. Pull the full status to learn the mixer serial and current volumes.
    send_request(
        &mut write,
        &DaemonRequest {
            id: next_id,
            data: Value::String("GetStatus".into()),
        },
    )
    .await?;
    next_id += 1;

    let (serial, mut volumes) = wait_for_status(&mut read).await?;

    // 2. React to hook events and to incoming status patches.
    loop {
        tokio::select! {
            event = rx.recv() => {
                let Some(event) = event else { return Ok(()); };
                let channel = active_channel.read().unwrap().clone();
                let current = *volumes.get(&channel).unwrap_or(&VOLUME_FALLBACK);
                let new_volume = match event {
                    VolumeEvent::Up => (current + VOLUME_STEP).min(VOLUME_MAX),
                    VolumeEvent::Down => (current - VOLUME_STEP).max(VOLUME_MIN),
                };
                if new_volume != current {
                    volumes.insert(channel.clone(), new_volume);
                    send_request(
                        &mut write,
                        &DaemonRequest {
                            id: next_id,
                            data: json!({
                                "Command": [&serial, {"SetVolume": [&channel, new_volume]}]
                            }),
                        },
                    )
                    .await?;
                    next_id += 1;
                }
            }
            msg = read.next() => {
                let Some(msg) = msg else { return Err("websocket closed".into()); };
                handle_incoming(msg?, &serial, &mut volumes)?;
            }
        }
    }
}

async fn send_request<S>(
    write: &mut S,
    req: &DaemonRequest,
) -> Result<(), Box<dyn std::error::Error>>
where
    S: SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    let payload = serde_json::to_string(req)?;
    write.send(Message::Text(payload)).await?;
    Ok(())
}

async fn wait_for_status<S>(
    read: &mut S,
) -> Result<(String, HashMap<String, i32>), Box<dyn std::error::Error>>
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    while let Some(msg) = read.next().await {
        let msg = msg?;
        let Message::Text(text) = msg else { continue };
        let response: DaemonResponse = serde_json::from_str(&text)?;

        let Some(status) = response.data.get("Status") else {
            continue;
        };
        let Some(mixers) = status.get("mixers").and_then(|m| m.as_object()) else {
            continue;
        };
        let Some((serial, mixer)) = mixers.iter().next() else {
            return Err("no GoXLR mixer reported by the daemon".into());
        };

        let mut volumes = HashMap::new();
        if let Some(map) = mixer.pointer("/levels/volumes").and_then(|v| v.as_object()) {
            for (name, val) in map {
                if let Some(v) = val.as_i64() {
                    volumes.insert(name.clone(), (v as i32).clamp(VOLUME_MIN, VOLUME_MAX));
                }
            }
        }
        return Ok((serial.clone(), volumes));
    }
    Err("websocket closed before status received".into())
}

fn handle_incoming(
    msg: Message,
    serial: &str,
    volumes: &mut HashMap<String, i32>,
) -> Result<(), Box<dyn std::error::Error>> {
    let Message::Text(text) = msg else {
        return Ok(());
    };
    let response: DaemonResponse = serde_json::from_str(&text)?;

    // Patch paths look like: /mixers/<serial>/levels/volumes/<Channel>
    let Some(patches) = response.data.get("Patch").and_then(|p| p.as_array()) else {
        return Ok(());
    };

    let prefix = format!("/mixers/{}/levels/volumes/", serial);
    for patch in patches {
        let path = patch.get("path").and_then(|p| p.as_str()).unwrap_or("");
        if let Some(channel) = path.strip_prefix(&prefix) {
            if let Some(value) = patch.get("value").and_then(|v| v.as_i64()) {
                volumes.insert(
                    channel.to_string(),
                    (value as i32).clamp(VOLUME_MIN, VOLUME_MAX),
                );
            }
        }
    }
    Ok(())
}
