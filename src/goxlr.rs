use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::{sleep, MissedTickBehavior};
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::hook::VolumeEvent;

const WS_URL: &str = "ws://localhost:14564/api/websocket";
const VOLUME_STEP: i32 = 5;
const VOLUME_MIN: i32 = 0;
const VOLUME_MAX: i32 = 255;
const VOLUME_FALLBACK: i32 = 128;
const RECONNECT_DELAY: Duration = Duration::from_secs(3);

/// Maximum send rate per channel: a wheel burst is coalesced into one
/// SetVolume command per tick, so the motor receives a smooth sequence of
/// commands instead of being hammered.
const SEND_INTERVAL: Duration = Duration::from_millis(25);

/// After we send a SetVolume on a channel, the daemon echoes a Patch back
/// with the same value. While the motor is moving, several echoes can also
/// arrive in flight. Within this window any inbound patch on the channel is
/// considered our own echo (or a transient reading) and ignored — otherwise
/// it would clobber the local cache and make the next wheel event compute
/// from a stale value, causing the fader to visibly bounce backwards.
const COMMAND_GRACE: Duration = Duration::from_millis(500);

#[derive(Serialize)]
struct DaemonRequest {
    id: u64,
    data: Value,
}

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
            sleep(RECONNECT_DELAY).await;
            continue;
        }
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

    let mut pending: HashMap<String, i32> = HashMap::new();
    let mut last_command_at: HashMap<String, Instant> = HashMap::new();

    let mut ticker = tokio::time::interval(SEND_INTERVAL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            event = rx.recv() => {
                let Some(event) = event else { return Ok(()); };
                let channel = active_channel.read().unwrap().clone();
                // Read pending first so a burst stacks on its own intent
                // instead of resetting back to the cached value each tick.
                let current = pending
                    .get(&channel)
                    .copied()
                    .unwrap_or_else(|| volumes.get(&channel).copied().unwrap_or(VOLUME_FALLBACK));
                let new_volume = match event {
                    VolumeEvent::Up => (current + VOLUME_STEP).min(VOLUME_MAX),
                    VolumeEvent::Down => (current - VOLUME_STEP).max(VOLUME_MIN),
                };
                if new_volume != current {
                    pending.insert(channel, new_volume);
                }
            }
            _ = ticker.tick() => {
                if pending.is_empty() {
                    continue;
                }
                let to_send: Vec<(String, i32)> = pending.drain().collect();
                let now = Instant::now();
                for (channel, value) in to_send {
                    let cached = volumes.get(&channel).copied().unwrap_or(VOLUME_FALLBACK);
                    if value == cached {
                        continue;
                    }
                    send_request(
                        &mut write,
                        &DaemonRequest {
                            id: next_id,
                            data: json!({
                                "Command": [&serial, {"SetVolume": [&channel, value]}]
                            }),
                        },
                    )
                    .await?;
                    next_id += 1;
                    volumes.insert(channel.clone(), value);
                    last_command_at.insert(channel, now);
                }
            }
            msg = read.next() => {
                let Some(msg) = msg else { return Err("websocket closed".into()); };
                handle_incoming(msg?, &serial, &mut volumes, &last_command_at)?;
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
    last_command_at: &HashMap<String, Instant>,
) -> Result<(), Box<dyn std::error::Error>> {
    let Message::Text(text) = msg else {
        return Ok(());
    };
    let response: DaemonResponse = serde_json::from_str(&text)?;

    let Some(patches) = response.data.get("Patch").and_then(|p| p.as_array()) else {
        return Ok(());
    };

    let prefix = format!("/mixers/{}/levels/volumes/", serial);
    for patch in patches {
        let path = patch.get("path").and_then(|p| p.as_str()).unwrap_or("");
        if let Some(channel) = path.strip_prefix(&prefix) {
            // Suppress echoes / in-flight intermediates of our own commands.
            if let Some(at) = last_command_at.get(channel) {
                if at.elapsed() < COMMAND_GRACE {
                    continue;
                }
            }
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
