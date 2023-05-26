use std::time::Duration;

use bombastic_index::Index;
use futures::pin_mut;
use tokio::select;
use trustification_event_bus::{Event, EventBus, EventConsumer, Topic};
use trustification_storage::{EventType, Storage};

pub async fn run<E: EventBus>(
    mut index: Index,
    storage: Storage,
    bus: E,
    sync_interval: Duration,
) -> Result<(), anyhow::Error> {
    let mut interval = tokio::time::interval(sync_interval);
    let mut events = 0;
    let consumer = bus.subscribe("indexer", &[Topic::STORED]).await?;
    let mut uncommitted_events = Vec::new();
    loop {
        let tick = interval.tick();
        pin_mut!(tick);
        select! {
            event = consumer.next() => match event {
                Ok(Some(event)) => {
                    if let Some(payload) = event.payload() {
                        if let Ok(data) = storage.decode_event(&payload) {
                            if data.event_type == EventType::Put {
                                if storage.is_index(&data.key) {
                                    tracing::trace!("It's an index event, ignoring");
                                } else {
                                    if let Some(key) = storage.extract_key(&data.key) {
                                        match storage.get(key).await {
                                            Ok(data) => {
                                                if let Some(hash) = data.annotations.get("digest") {
                                                    match index.insert_or_replace(&data.key, hash.as_str(), key).await {
                                                        Ok(_) => {
                                                            tracing::trace!("Inserted entry into index");
                                                            bus.send(Topic::INDEXED, key.as_bytes()).await?;
                                                            events += 1;
                                                        }
                                                        Err(e) => {
                                                            let failure = serde_json::json!( {
                                                                "key": key,
                                                                "error": e.to_string(),
                                                            }).to_string();
                                                            bus.send(Topic::FAILED, failure.as_bytes()).await?;
                                                            tracing::warn!("Error inserting entry into index: {:?}", e)
                                                        }
                                                    }
                                                } else {
                                                    tracing::debug!("Digest not found in annotations");
                                                }
                                            }
                                            Err(e) => {
                                                tracing::debug!("Error retrieving document event data, ignoring (error: {:?})", e);
                                            }
                                        }
                                    } else {
                                        tracing::warn!("Error extracting key from event: {:?}", data)
                                    }
                                }
                            }
                        } else if let Err(e) = storage.decode_event(&payload) {
                            tracing::warn!("Error decoding event: {:?}", e);
                        }
                    }
                    uncommitted_events.push(event);
                }
                Ok(None) => {
                    tracing::debug!("Polling returned no events, retrying");
                }
                Err(e) => {
                    tracing::warn!("Error polling for event: {:?}", e);
                }
            },
            _ = tick => {
                if events > 0 {
                    tracing::debug!("{} new events added, pushing new index to storage", events);
                    match index.snapshot() {
                        Ok(data) => {
                            match storage.put_index(&data).await {
                                Ok(_) => {
                                    tracing::trace!("Index updated successfully");
                                    match consumer.commit(&uncommitted_events[..]).await {
                                        Ok(_) => {
                                            tracing::trace!("Event committed successfully");
                                            uncommitted_events.clear();
                                        }
                                        Err(e) => {
                                            tracing::warn!("Error committing event: {:?}", e)
                                        }
                                    }
                                    events = 0;
                                }
                                Err(e) => {
                                    tracing::warn!("Error updating index: {:?}", e)
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!("Error taking index snapshot: {:?}", e);
                        }
                    }
                } else {
                    tracing::trace!("No changes to index");
                }
            }
        }
    }
}
