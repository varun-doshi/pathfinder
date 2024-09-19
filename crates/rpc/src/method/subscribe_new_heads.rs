use std::sync::Arc;

use axum::async_trait;
use pathfinder_common::{BlockId, BlockNumber};
use tokio::sync::mpsc;

use crate::context::RpcContext;
use crate::jsonrpc::{RpcError, RpcSubscriptionFlow};

pub struct SubscribeNewHeads;

#[derive(Debug)]
pub struct Request {
    block: BlockId,
}

impl crate::dto::DeserializeForVersion for Request {
    fn deserialize(value: crate::dto::Value) -> Result<Self, serde_json::Error> {
        value.deserialize_map(|value| {
            Ok(Self {
                block: value.deserialize_serde("block")?,
            })
        })
    }
}

#[derive(Debug)]
pub struct Message(Arc<pathfinder_common::BlockHeader>);

impl crate::dto::serialize::SerializeForVersion for Message {
    fn serialize(
        &self,
        serializer: crate::dto::serialize::Serializer,
    ) -> Result<crate::dto::serialize::Ok, crate::dto::serialize::Error> {
        crate::dto::BlockHeader(&self.0).serialize(serializer)
    }
}

#[async_trait]
impl RpcSubscriptionFlow for SubscribeNewHeads {
    type Request = Request;
    type Notification = Message;

    fn subscription_name() -> &'static str {
        "starknet_subscriptionNewHeads"
    }

    fn starting_block(req: &Self::Request) -> BlockId {
        req.block
    }

    async fn catch_up(
        state: &RpcContext,
        _req: &Self::Request,
        from: BlockNumber,
        to: BlockNumber,
    ) -> Result<Vec<(Self::Notification, BlockNumber)>, RpcError> {
        let storage = state.storage.clone();
        let headers = tokio::task::spawn_blocking(move || -> Result<_, RpcError> {
            let mut conn = storage.connection().map_err(RpcError::InternalError)?;
            let db = conn.transaction().map_err(RpcError::InternalError)?;
            db.block_range(from, to).map_err(RpcError::InternalError)
        })
        .await
        .map_err(|e| RpcError::InternalError(e.into()))??;
        Ok(headers
            .into_iter()
            .map(|header| {
                let block_number = header.number;
                (Message(header.into()), block_number)
            })
            .collect())
    }

    async fn subscribe(state: RpcContext, tx: mpsc::Sender<(Self::Notification, BlockNumber)>) {
        let mut rx = state.notifications.block_headers.subscribe();
        loop {
            match rx.recv().await {
                Ok(header) => {
                    let block_number = header.number;
                    if tx.send((Message(header), block_number)).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    tracing::debug!(
                        "Error receiving block header from notifications channel, node might be \
                         lagging: {:?}",
                        e
                    );
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::extract::ws::Message;
    use pathfinder_common::{BlockHash, BlockHeader, BlockNumber, ChainId};
    use pathfinder_crypto::Felt;
    use pathfinder_storage::StorageBuilder;
    use starknet_gateway_client::Client;
    use tokio::sync::mpsc;

    use crate::context::{RpcConfig, RpcContext};
    use crate::jsonrpc::{handle_json_rpc_socket, RpcResponse, RpcRouter};
    use crate::pending::PendingWatcher;
    use crate::v02::types::syncing::Syncing;
    use crate::{v08, Notifications, SubscriptionId, SyncState};

    #[tokio::test]
    async fn happy_path_with_historic_blocks() {
        happy_path_test(1000).await;
    }

    #[tokio::test]
    async fn happy_path_with_historic_blocks_no_batching() {
        happy_path_test(10).await;
    }

    #[tokio::test]
    async fn happy_path_with_historic_blocks_batching_edge_case() {
        happy_path_test(128).await;
    }

    #[tokio::test]
    async fn happy_path_with_no_historic_blocks() {
        happy_path_test(0).await;
    }

    #[tokio::test]
    async fn race_condition_with_historic_blocks() {
        let num_blocks = 1000;
        let router = setup(num_blocks);
        let (sender_tx, mut sender_rx) = mpsc::channel(1024);
        let (receiver_tx, receiver_rx) = mpsc::channel(1024);
        handle_json_rpc_socket(router.clone(), sender_tx, receiver_rx);
        receiver_tx
            .send(Ok(Message::Text(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "starknet_subscribeNewHeads",
                    "params": {"block": {"block_number": 0}}
                })
                .to_string(),
            )))
            .await
            .unwrap();
        let res = sender_rx.recv().await.unwrap().unwrap();
        let subscription_id = match res {
            Message::Text(json) => {
                let json: serde_json::Value = serde_json::from_str(&json).unwrap();
                assert_eq!(json["jsonrpc"], "2.0");
                assert_eq!(json["id"], 1);
                json["result"]["subscription_id"].as_u64().unwrap()
            }
            _ => panic!("Expected text message"),
        };
        for i in 0..num_blocks {
            let expected = sample_new_heads_message(i, subscription_id);
            let header = sender_rx.recv().await.unwrap().unwrap();
            let json: serde_json::Value = match header {
                Message::Text(json) => serde_json::from_str(&json).unwrap(),
                _ => panic!("Expected text message"),
            };
            assert_eq!(json, expected);
        }
        // Ensure that the background task processes beyond the catch-up phase.
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        // Insert more blocks before the active updates kick in. This simulates a
        // real-world race condition.
        for i in 0..num_blocks {
            let mut conn = router.context.storage.connection().unwrap();
            let db = conn.transaction().unwrap();
            let header = sample_header(i + num_blocks);
            db.insert_block_header(&header).unwrap();
            db.commit().unwrap();
        }
        for i in 0..10 {
            router
                .context
                .notifications
                .block_headers
                .send(sample_header(i + 2 * num_blocks).into())
                .unwrap();
            if i == 0 {
                // First, expect all the newly inserted blocks.
                for j in 0..num_blocks {
                    let expected = sample_new_heads_message(j + num_blocks, subscription_id);
                    let header = sender_rx.recv().await.unwrap().unwrap();
                    let json: serde_json::Value = match header {
                        Message::Text(json) => serde_json::from_str(&json).unwrap(),
                        _ => panic!("Expected text message"),
                    };
                    assert_eq!(json, expected);
                }
            }
            // Then, expect the block updates.
            let expected = sample_new_heads_message(i + 2 * num_blocks, subscription_id);
            let header = sender_rx.recv().await.unwrap().unwrap();
            let json: serde_json::Value = match header {
                Message::Text(json) => serde_json::from_str(&json).unwrap(),
                _ => panic!("Expected text message"),
            };
            assert_eq!(json, expected);
        }
        assert!(sender_rx.is_empty());
    }

    #[tokio::test]
    async fn unsubscribe() {
        let (tx, mut rx, subscription_id, router) = happy_path_test(0).await;
        tx.send(Ok(Message::Text(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 100,
                "method": "starknet_unsubscribe",
                "params": {"subscription_id": subscription_id.0}
            })
            .to_string(),
        )))
        .await
        .unwrap();
        let res = rx.recv().await.unwrap().unwrap();
        let json: serde_json::Value = match res {
            Message::Text(json) => serde_json::from_str(&json).unwrap(),
            _ => panic!("Expected text message"),
        };
        assert_eq!(
            json,
            serde_json::json!({"jsonrpc": "2.0", "id": 100, "result": true})
        );
        router
            .context
            .notifications
            .block_headers
            .send(sample_header(10).into())
            // Might error if the receiver is closed.
            .ok();
        // Give time for background tasks to process.
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        // Since the subscription was cancelled, no more messages should be received.
        assert!(rx.is_empty());
    }

    fn setup(num_blocks: u64) -> RpcRouter {
        let storage = StorageBuilder::in_memory().unwrap();
        let mut conn = storage.connection().unwrap();
        let db = conn.transaction().unwrap();
        for i in 0..num_blocks {
            let header = sample_header(i);
            db.insert_block_header(&header).unwrap();
        }
        db.commit().unwrap();
        let (_, pending_data) = tokio::sync::watch::channel(Default::default());
        let notifications = Notifications::default();
        let ctx = RpcContext {
            cache: Default::default(),
            storage,
            execution_storage: StorageBuilder::in_memory().unwrap(),
            pending_data: PendingWatcher::new(pending_data),
            sync_status: SyncState {
                status: Syncing::False(false).into(),
            }
            .into(),
            chain_id: ChainId::MAINNET,
            sequencer: Client::mainnet(Duration::from_secs(10)),
            websocket: None,
            notifications,
            config: RpcConfig {
                batch_concurrency_limit: 1.try_into().unwrap(),
                get_events_max_blocks_to_scan: 1.try_into().unwrap(),
                get_events_max_uncached_bloom_filters_to_load: 1.try_into().unwrap(),
                custom_versioned_constants: None,
            },
        };
        v08::register_routes().build(ctx)
    }

    async fn happy_path_test(
        num_blocks: u64,
    ) -> (
        mpsc::Sender<Result<Message, axum::Error>>,
        mpsc::Receiver<Result<Message, RpcResponse>>,
        SubscriptionId,
        RpcRouter,
    ) {
        let router = setup(num_blocks);
        let (sender_tx, mut sender_rx) = mpsc::channel(1024);
        let (receiver_tx, receiver_rx) = mpsc::channel(1024);
        handle_json_rpc_socket(router.clone(), sender_tx, receiver_rx);
        let params = if num_blocks == 0 {
            serde_json::json!(
                {"block": "latest"}
            )
        } else {
            serde_json::json!(
                {"block": {"block_number": 0}}
            )
        };
        receiver_tx
            .send(Ok(Message::Text(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "starknet_subscribeNewHeads",
                    "params": params
                })
                .to_string(),
            )))
            .await
            .unwrap();
        let res = sender_rx.recv().await.unwrap().unwrap();
        let subscription_id = match res {
            Message::Text(json) => {
                let json: serde_json::Value = serde_json::from_str(&json).unwrap();
                assert_eq!(json["jsonrpc"], "2.0");
                assert_eq!(json["id"], 1);
                json["result"]["subscription_id"].as_u64().unwrap()
            }
            _ => panic!("Expected text message"),
        };
        for i in 0..num_blocks {
            let expected = sample_new_heads_message(i, subscription_id);
            let header = sender_rx.recv().await.unwrap().unwrap();
            let json: serde_json::Value = match header {
                Message::Text(json) => serde_json::from_str(&json).unwrap(),
                _ => panic!("Expected text message"),
            };
            assert_eq!(json, expected);
        }
        for i in 0..10 {
            retry(|| {
                router
                    .context
                    .notifications
                    .block_headers
                    .send(sample_header(i).into())
            })
            .await
            .unwrap();
            let expected = sample_new_heads_message(i, subscription_id);
            let header = sender_rx.recv().await.unwrap().unwrap();
            let json: serde_json::Value = match header {
                Message::Text(json) => serde_json::from_str(&json).unwrap(),
                _ => panic!("Expected text message"),
            };
            assert_eq!(json, expected);
        }
        assert!(sender_rx.is_empty());
        (
            receiver_tx,
            sender_rx,
            SubscriptionId(subscription_id.try_into().unwrap()),
            router,
        )
    }

    fn sample_header(block_number: u64) -> BlockHeader {
        BlockHeader {
            hash: BlockHash(Felt::from_u64(block_number)),
            number: BlockNumber::new_or_panic(block_number),
            parent_hash: BlockHash::ZERO,
            ..Default::default()
        }
    }

    fn sample_new_heads_message(block_number: u64, subscription_id: u64) -> serde_json::Value {
        let hash = Felt::from_u64(block_number);
        serde_json::json!({
            "jsonrpc":"2.0",
            "method":"starknet_subscriptionNewHeads",
            "params": {
                "result": {
                    "block_hash": hash,
                    "block_number": block_number,
                    "l1_da_mode": "CALLDATA",
                    "l1_data_gas_price": { "price_in_fri": "0x0", "price_in_wei": "0x0" },
                    "l1_gas_price":{ "price_in_fri": "0x0", "price_in_wei": "0x0" },
                    "new_root": "0x0",
                    "parent_hash": "0x0",
                    "sequencer_address": "0x0",
                    "starknet_version": "",
                    "timestamp": 0
                },
                "subscription_id": subscription_id
            }
        })
    }

    // Retry to let other tasks make progress.
    async fn retry<T, E>(cb: impl Fn() -> Result<T, E>) -> Result<T, E>
    where
        E: std::fmt::Debug,
    {
        for i in 0..10 {
            match cb() {
                Ok(result) => return Ok(result),
                Err(e) => {
                    if i == 9 {
                        return Err(e);
                    }
                    tokio::task::yield_now().await;
                }
            }
        }
        unreachable!()
    }
}