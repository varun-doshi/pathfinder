use std::sync::Arc;

use anyhow::Context;
use pathfinder_common::{BlockHeader, BlockId, TransactionHash};

use crate::context::RpcContext;

crate::error::generate_rpc_error_subset!(Error: BlockNotFound);

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Input {
    pub block_id: BlockId,
}

impl crate::dto::DeserializeForVersion for Input {
    fn deserialize(value: crate::dto::Value) -> Result<Self, serde_json::Error> {
        value.deserialize_map(|value| {
            Ok(Self {
                block_id: value.deserialize("block_id")?,
            })
        })
    }
}

#[derive(Debug)]
pub enum Output {
    Pending {
        header: Arc<starknet_gateway_types::reply::PendingBlock>,
        transactions: Vec<TransactionHash>,
    },
    Full {
        header: Box<BlockHeader>,
        transactions: Vec<TransactionHash>,
        l1_accepted: bool,
    },
}

/// Get block information with transaction hashes given the block id
pub async fn get_block_with_tx_hashes(context: RpcContext, input: Input) -> Result<Output, Error> {
    let span = tracing::Span::current();

    tokio::task::spawn_blocking(move || {
        let _g = span.enter();
        let mut connection = context
            .storage
            .connection()
            .context("Opening database connection")?;

        let transaction = connection
            .transaction()
            .context("Creating database transaction")?;

        let block_id = match input.block_id {
            BlockId::Pending => {
                let pending = context
                    .pending_data
                    .get(&transaction)
                    .context("Querying pending data")?;

                let transactions = pending.block.transactions.iter().map(|t| t.hash).collect();

                return Ok(Output::Pending {
                    header: pending.block,
                    transactions,
                });
            }
            other => other.try_into().expect("Only pending cast should fail"),
        };

        let header = transaction
            .block_header(block_id)
            .context("Reading block from database")?
            .ok_or(Error::BlockNotFound)?;

        let l1_accepted = transaction.block_is_l1_accepted(header.number.into())?;

        let transactions = transaction
            .transaction_hashes_for_block(header.number.into())
            .context("Reading transaction hashes")?
            .context("Transaction hashes missing")?;

        Ok(Output::Full {
            header: Box::new(header),
            transactions,
            l1_accepted,
        })
    })
    .await
    .context("Joining blocking task")?
}

impl crate::dto::serialize::SerializeForVersion for Output {
    fn serialize(
        &self,
        serializer: crate::dto::serialize::Serializer,
    ) -> Result<crate::dto::serialize::Ok, crate::dto::serialize::Error> {
        match self {
            Output::Pending {
                header,
                transactions,
            } => {
                let mut serializer = serializer.serialize_struct()?;
                serializer.flatten(&crate::dto::PendingBlockHeader(header))?;
                serializer.serialize_iter(
                    "transactions",
                    transactions.len(),
                    &mut transactions.iter().map(crate::dto::TxnHash),
                )?;
                serializer.end()
            }
            Output::Full {
                header,
                transactions,
                l1_accepted,
            } => {
                let mut serializer = serializer.serialize_struct()?;
                serializer.flatten(&crate::dto::BlockHeader(header))?;
                serializer.serialize_iter(
                    "transactions",
                    transactions.len(),
                    &mut transactions.iter().map(crate::dto::TxnHash),
                )?;
                serializer.serialize_field(
                    "status",
                    &if *l1_accepted {
                        "ACCEPTED_ON_L1"
                    } else {
                        "ACCEPTED_ON_L2"
                    },
                )?;
                serializer.end()
            }
        }
    }
}