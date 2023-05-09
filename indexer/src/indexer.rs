use std::time::Duration;

use bombastic_event_bus::{Event, EventBus, EventConsumer, Topic};
use bombastic_index::Index;
use bombastic_storage::Storage;
use futures::pin_mut;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::select;

use crate::sbom::SBOM;

#[derive(Deserialize, Debug)]
pub struct StorageEvent {
    #[serde(rename = "EventName")]
    event_name: String,
    #[serde(rename = "Key")]
    key: String,
}

const PUT_EVENT: &str = "s3:ObjectCreated:Put";

pub async fn run<E: EventBus>(
    mut index: Index,
    storage: Storage,
    bus: E,
    sync_interval: Duration,
) -> Result<(), anyhow::Error> {
    let mut interval = tokio::time::interval(sync_interval);
    let mut changed = false;
    let consumer = bus.subscribe("indexer", &[Topic::STORED])?;
    loop {
        let tick = interval.tick();
        pin_mut!(tick);
        select! {
            event = consumer.next() => match event {
                Ok(event) => loop {
                    if let Some(payload) = event.payload() {
                        if let Ok(data) = serde_json::from_slice::<StorageEvent>(payload) {
                            tracing::trace!("Got payload from event: {:?}", data);
                            if data.event_name == PUT_EVENT {
                                if storage.is_index(&data.key) {
                                    break;
                                }
                                if let Some(key) = storage.extract_key(&data.key) {
                                    match storage.get(key).await {
                                        Ok(data) => {
                                            if let Ok(sbom) = SBOM::parse(&data) {
                                                if let Some(purl) = sbom.purl() {
                                                    let mut hasher = Sha256::new();
                                                    hasher.update(&data);
                                                    let hash = hasher.finalize();
                                                    match index.insert(&purl, &format!("{:x}", hash), key).await {
                                                        Ok(_) => {
                                                            tracing::debug!("Inserted entry into index");
                                                            bus.send(Topic::INDEXED, key.as_bytes()).await?;
                                                            changed = true;
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
                                                    let failure = serde_json::json!( {
                                                        "key": key,
                                                        "error": "Unable to locate package URL (pURL) for SBOM",
                                                    }).to_string();
                                                    bus.send(Topic::FAILED, failure.as_bytes()).await?;
                                                }
                                            } else {
                                                tracing::debug!("Error parsing event data, ignoring");
                                            }
                                        }
                                        Err(e) => {
                                            tracing::debug!("Error retrieving document event data, ignoring (error: {:?})", e);
                                        }
                                    }
                                } else {
                                    tracing::warn!("Error extracting key from event: {:?}", data)
                                };
                            }
                        }
                    }
                    match event.commit() {
                        Ok(_) => {
                            tracing::trace!("Event committed successfully");
                            break;
                        }
                        Err(e) => {
                            tracing::warn!("Error committing event: {:?}", e)
                        }
                    }
                },
                Err(e) => {
                    tracing::warn!("Error polling for event: {:?}", e);
                }
            },
            _ = tick => {
                if changed {
                    tracing::debug!("Taking index snapshot and pushing to storage");
                    match index.snapshot() {
                        Ok(data) => {
                            match storage.put_index(&data).await {
                                Ok(_) => {
                                    tracing::trace!("Index updated successfully");
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
                    tracing::info!("No changes to index");
                }
            }
        }
    }
}
