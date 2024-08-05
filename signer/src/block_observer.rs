//! # Block observer
//!
//! This module contains the block observer implementation for the sBTC signer.
//! The block observer is responsible for populating the signer database with
//! information from the Bitcoin and Stacks blockchains, and notifying
//! the signer event loop whenever the state has been updated.
//!
//! The following information is extracted by the block observer:
//! - Bitcoin blocks
//! - Stacks blocks
//! - Deposit requests
//! - sBTC transactions
//! - Withdraw requests
//! - Deposit accept transactions
//! - Withdraw accept transactions
//! - Withdraw reject transactions
//! - Update signer set transactions
//! - Set aggregate key transactions

use std::collections::HashMap;

use crate::bitcoin::BitcoinInteract;
use crate::error;
use crate::error::Error;
use crate::stacks::api::StacksInteract;
use crate::storage;
use crate::storage::model;
use crate::storage::DbRead;
use crate::storage::DbWrite;
use bitcoin::hashes::Hash as _;
use bitcoin::Transaction;
use bitcoin::Txid;
use blockstack_lib::chainstate::nakamoto;
use futures::stream::StreamExt;
use sbtc::deposits::CreateDepositRequest;
use sbtc::deposits::Deposit;
use sbtc::rpc::BitcoinClient;

/// Block observer
#[derive(Debug)]
pub struct BlockObserver<BitcoinClient, StacksClient, EmilyClient, BlockHashStream, Storage> {
    /// Bitcoin client
    pub bitcoin_client: BitcoinClient,
    /// Stacks client
    pub stacks_client: StacksClient,
    /// Emily client
    pub emily_client: EmilyClient,
    /// Stream of blocks from the block notifier
    pub bitcoin_blocks: BlockHashStream,
    /// Database connection
    pub storage: Storage,
    /// Used to notify any other system that the database has been updated
    pub subscribers: tokio::sync::watch::Sender<()>,
    /// How far back in time the observer should look
    pub horizon: usize,
    /// An in memory map of deposit requests that haven't been confirmed
    /// on bitcoin yet.
    pub deposit_requests: HashMap<Txid, Vec<Deposit>>,
    /// The bitcoin network
    pub network: bitcoin::Network,
}

impl<BC, SC, EC, BHS, S> BlockObserver<BC, SC, EC, BHS, S>
where
    BC: BitcoinInteract + BitcoinClient,
    SC: StacksInteract,
    EC: EmilyInteract,
    S: DbWrite + DbRead + Send + Sync,
    BHS: futures::stream::Stream<Item = bitcoin::BlockHash> + Unpin,
    error::Error: From<<S as DbRead>::Error>,
    error::Error: From<<S as DbWrite>::Error>,
    error::Error: From<<BC as BitcoinInteract>::Error>,
{
    /// Run the block observer
    #[tracing::instrument(skip(self))]
    pub async fn run(mut self) -> Result<(), error::Error> {
        while let Some(new_block_hash) = self.bitcoin_blocks.next().await {
            self.load_latest_deposit_requests().await;

            for block in self.next_blocks_to_process(new_block_hash).await? {
                self.process_bitcoin_block(block).await?;
            }

            if self.subscribers.send(()).is_err() {
                tracing::info!("block observer has no subscribers");
                break;
            }
        }

        tracing::info!("shutting down block observer");

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    async fn load_latest_deposit_requests(&mut self) {
        let deposit_requests = self.emily_client.get_deposits().await;

        deposit_requests
            .into_iter()
            .flat_map(|request| {
                request
                    .validate(&self.bitcoin_client)
                    .inspect_err(|err| tracing::warn!("could not validate deposit request: {err}"))
            })
            .for_each(|deposit| {
                self.deposit_requests
                    .entry(deposit.info.outpoint.txid)
                    .or_default()
                    .push(deposit)
            });
    }

    #[tracing::instrument(skip(self))]
    async fn next_blocks_to_process(
        &mut self,
        mut block_hash: bitcoin::BlockHash,
    ) -> Result<Vec<bitcoin::Block>, error::Error> {
        let mut blocks = Vec::new();

        for _ in 0..self.horizon {
            if self.have_already_processed_block(block_hash).await? {
                break;
            }

            let block = self
                .bitcoin_client
                .get_block(&block_hash)
                .await?
                .ok_or(error::Error::MissingBlock)?;

            block_hash = block.header.prev_blockhash;
            blocks.push(block);
        }

        // Make order chronological
        blocks.reverse();
        Ok(blocks)
    }

    #[tracing::instrument(skip(self))]
    async fn have_already_processed_block(
        &mut self,
        block_hash: bitcoin::BlockHash,
    ) -> Result<bool, error::Error> {
        Ok(self
            .storage
            .get_bitcoin_block(&block_hash.to_byte_array().into())
            .await?
            .is_some())
    }

    #[tracing::instrument(skip(self))]
    async fn process_bitcoin_block(&mut self, block: bitcoin::Block) -> Result<(), error::Error> {
        let info = self.stacks_client.get_tenure_info().await?;
        let stacks_blocks = crate::stacks::api::fetch_unknown_ancestors(
            &self.stacks_client,
            &self.storage,
            info.tip_block_id,
        )
        .await?;

        self.extract_deposit_requests(&block.txdata).await?;
        self.extract_sbtc_transactions(&block.txdata);

        self.write_stacks_blocks(&stacks_blocks).await?;
        self.write_bitcoin_block(&block).await?;

        Ok(())
    }

    async fn extract_deposit_requests(&mut self, txs: &[Transaction]) -> Result<(), Error> {
        let deposit_request: Vec<model::DepositRequest> = txs
            .iter()
            .filter_map(|tx| self.deposit_requests.remove(&tx.compute_txid()))
            .flatten()
            .map(|deposit| model::DepositRequest::from_deposit(&deposit, self.network))
            .collect();

        self.storage.write_deposit_requests(deposit_request).await?;
        Ok(())
    }

    fn extract_sbtc_transactions(&self, _transactions: &[bitcoin::Transaction]) {
        // TODO(#204): Implement
    }

    async fn write_stacks_blocks(
        &mut self,
        blocks: &[nakamoto::NakamotoBlock],
    ) -> Result<(), error::Error> {
        let txs = storage::postgres::extract_relevant_transactions(blocks);
        let headers = blocks
            .iter()
            .map(model::StacksBlock::try_from)
            .collect::<Result<_, _>>()?;

        self.storage.write_stacks_block_headers(headers).await?;
        self.storage.write_stacks_transactions(txs).await?;
        Ok(())
    }

    async fn write_bitcoin_block(&mut self, block: &bitcoin::Block) -> Result<(), error::Error> {
        let db_block = model::BitcoinBlock {
            block_hash: block.block_hash().to_byte_array().to_vec(),
            block_height: block
                .bip34_block_height()
                .expect("Failed to get block height") as i64,
            parent_hash: block.header.prev_blockhash.to_byte_array().to_vec(),
            confirms: Vec::new(),
            created_at: time::OffsetDateTime::now_utc(),
        };

        self.storage.write_bitcoin_block(&db_block).await?;

        Ok(())
    }
}

// Placehoder traits. To be replaced with the actual traits once implemented.

/// Placeholder trait
pub trait EmilyInteract {
    /// Get deposits
    fn get_deposits(&mut self) -> impl std::future::Future<Output = Vec<CreateDepositRequest>>;
}

#[cfg(test)]
mod tests {
    use bitcoin::BlockHash;
    use blockstack_lib::chainstate::burn::ConsensusHash;
    use blockstack_lib::chainstate::nakamoto::NakamotoBlock;
    use blockstack_lib::chainstate::nakamoto::NakamotoBlockHeader;
    use blockstack_lib::net::api::gettenureinfo::RPCGetTenureInfo;
    use blockstack_lib::types::chainstate::StacksBlockId;
    use rand::seq::IteratorRandom;
    use rand::SeedableRng;
    use sbtc::rpc::BitcoinClient;

    use crate::stacks::api::FeePriority;
    use crate::storage;
    use crate::testing::dummy;

    use super::*;

    #[derive(Debug, Clone)]
    struct DummyEmily(pub Vec<CreateDepositRequest>);

    impl EmilyInteract for DummyEmily {
        async fn get_deposits(&mut self) -> Vec<CreateDepositRequest> {
            self.0.clone()
        }
    }

    #[tokio::test]
    async fn should_be_able_to_extract_bitcoin_blocks_given_a_block_header_stream() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(46);
        let storage = storage::in_memory::Store::new_shared();
        let test_harness = TestHarness::generate(&mut rng, 20, 0..5);
        let block_hash_stream = test_harness.spawn_block_hash_stream();
        let (subscribers, subscriber_rx) = tokio::sync::watch::channel(());

        let block_observer = BlockObserver {
            bitcoin_client: test_harness.clone(),
            stacks_client: test_harness.clone(),
            emily_client: (),
            bitcoin_blocks: block_hash_stream,
            storage: storage.clone(),
            subscribers,
            horizon: 1,
            deposit_requests: HashMap::new(),
            network: bitcoin::Network::Regtest,
        };

        block_observer.run().await.expect("block observer failed");

        for block in test_harness.bitcoin_blocks {
            let persisted = storage
                .get_bitcoin_block(&block.block_hash().to_byte_array().to_vec())
                .await
                .expect("storage error")
                .expect("block wasn't persisted");

            assert_eq!(persisted.block_hash, block.block_hash().to_byte_array())
        }

        std::mem::drop(subscriber_rx);
    }

    /// Test that `BlockObserver::load_latest_deposit_requests` takes
    /// deposits from emily, validates them and only keeps the ones that
    /// pass validation.
    #[tokio::test]
    async fn validated_deposits_get_added_to_state() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(46);
        let mut test_harness = TestHarness::generate(&mut rng, 20, 0..5);

        let lock_time = 150;
        let max_fee = 32000;
        let amount = 500_000;

        // We're going to create two deposit requests, the first one valid
        // and the second one invalid. Emily will return both the valid and
        // invalid requests (even though it should've validated them) and
        // BitcoinClient will return the right transaction for both of
        // them.
        let tx_setup0 = sbtc::testing::deposits::tx_setup(lock_time, max_fee, amount);
        let deposit_request0 = CreateDepositRequest {
            outpoint: bitcoin::OutPoint {
                txid: tx_setup0.tx.compute_txid(),
                vout: 0,
            },
            deposit_script: tx_setup0.deposit.deposit_script(),
            reclaim_script: tx_setup0.reclaim.reclaim_script(),
        };
        // When we validate the deposit request, we fetch the transaction
        // from bitcoin-core's mempool or blockchain. The stubs out that
        // response.
        let get_tx_resp0 = sbtc::rpc::GetTxResponse {
            tx: tx_setup0.tx.clone(),
            block_hash: None,
            confirmations: None,
            block_time: None,
            in_active_chain: None,
        };

        let tx_setup1 = sbtc::testing::deposits::tx_setup(300, 2000, amount);
        // This one is an invalid deposit request because the deposit
        // script is wrong
        let deposit_request1 = CreateDepositRequest {
            outpoint: bitcoin::OutPoint {
                txid: tx_setup1.tx.compute_txid(),
                vout: 0,
            },
            deposit_script: bitcoin::ScriptBuf::new(),
            reclaim_script: tx_setup1.reclaim.reclaim_script(),
        };
        // The transaction is also in the mempool, even though it is an
        // invalid deposit.
        let get_tx_resp1 = sbtc::rpc::GetTxResponse {
            tx: tx_setup1.tx.clone(),
            block_hash: None,
            confirmations: None,
            block_time: None,
            in_active_chain: None,
        };

        // Let's add the "responses" to the field that feeds the
        // response to the `BitcoinClient::get_tx` call.
        test_harness
            .deposits
            .insert(get_tx_resp0.tx.compute_txid(), get_tx_resp0);
        test_harness
            .deposits
            .insert(get_tx_resp1.tx.compute_txid(), get_tx_resp1);

        // Now we finish setting up the block observer.
        let storage = storage::in_memory::Store::new_shared();
        let block_hash_stream = test_harness.spawn_block_hash_stream();
        let (subscribers, _subscriber_rx) = tokio::sync::watch::channel(());

        let mut block_observer = BlockObserver {
            bitcoin_client: test_harness.clone(),
            stacks_client: test_harness.clone(),
            emily_client: DummyEmily(vec![deposit_request0, deposit_request1]),
            bitcoin_blocks: block_hash_stream,
            storage: storage.clone(),
            subscribers,
            horizon: 1,
            deposit_requests: HashMap::new(),
            network: bitcoin::Network::Regtest,
        };

        block_observer.load_latest_deposit_requests().await;
        // Only the transaction from tx_setup0 was valid.
        assert_eq!(block_observer.deposit_requests.len(), 1);

        let deposit = block_observer
            .deposit_requests
            .get(&tx_setup0.tx.compute_txid())
            .cloned()
            .unwrap();
        assert_eq!(deposit.len(), 1);
        assert_eq!(deposit[0].tx, tx_setup0.tx);
    }

    /// Test that `BlockObserver::extract_deposit_requests` after
    /// `BlockObserver::load_latest_deposit_requests` stores validated
    /// deposit requests into "storage".
    #[tokio::test]
    async fn extract_deposit_requests_stores_validated_deposits() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(365);
        let mut test_harness = TestHarness::generate(&mut rng, 20, 0..5);

        let lock_time = 150;
        let max_fee = 32000;
        let amount = 500_000;

        // We're going to create two deposit requests, the first one valid
        // and the second one invalid. Emily will return both the valid and
        // invalid requests (even though it should've validated them) and
        // BitcoinClient will return the right transaction for both of
        // them.
        let tx_setup0 = sbtc::testing::deposits::tx_setup(lock_time, max_fee, amount);
        let deposit_request0 = CreateDepositRequest {
            outpoint: bitcoin::OutPoint {
                txid: tx_setup0.tx.compute_txid(),
                vout: 0,
            },
            deposit_script: tx_setup0.deposit.deposit_script(),
            reclaim_script: tx_setup0.reclaim.reclaim_script(),
        };
        // When we validate the deposit request, we fetch the transaction
        // from bitcoin-core's mempool or blockchain. The stubs out that
        // response.
        let get_tx_resp0 = sbtc::rpc::GetTxResponse {
            tx: tx_setup0.tx.clone(),
            block_hash: None,
            confirmations: None,
            block_time: None,
            in_active_chain: None,
        };

        // Let's add the "responses" to the field that feeds the
        // response to the `BitcoinClient::get_tx` call.
        test_harness
            .deposits
            .insert(get_tx_resp0.tx.compute_txid(), get_tx_resp0);

        // Now we finish setting up the block observer.
        let storage = storage::in_memory::Store::new_shared();
        let block_hash_stream = test_harness.spawn_block_hash_stream();
        let (subscribers, _subscriber_rx) = tokio::sync::watch::channel(());

        let mut block_observer = BlockObserver {
            bitcoin_client: test_harness.clone(),
            stacks_client: test_harness.clone(),
            emily_client: DummyEmily(vec![deposit_request0]),
            bitcoin_blocks: block_hash_stream,
            storage: storage.clone(),
            subscribers,
            horizon: 1,
            deposit_requests: HashMap::new(),
            network: bitcoin::Network::Regtest,
        };

        block_observer.load_latest_deposit_requests().await;
        // The transaction from tx_setup0 was valid.
        assert_eq!(block_observer.deposit_requests.len(), 1);

        block_observer
            .extract_deposit_requests(&[tx_setup0.tx.clone()])
            .await
            .unwrap();
        let storage = block_observer.storage.lock().await;
        assert_eq!(storage.deposit_requests.len(), 1);
        let db_outpoint = (tx_setup0.tx.compute_txid().to_byte_array().to_vec(), 0);
        assert!(storage.deposit_requests.get(&db_outpoint).is_some());

        // Now the deposit_requests thing should be empty now, since we stored the things.
        assert!(block_observer.deposit_requests.is_empty());
    }

    #[derive(Debug, Clone)]
    struct TestHarness {
        bitcoin_blocks: Vec<bitcoin::Block>,
        /// This represents the Stacks block chain. The bitcoin::BlockHash
        /// is used to identify tenures. That is, all NakamotoBlocks that
        /// have the same bitcoin::BlockHash occur within the same tenure.
        stacks_blocks: Vec<(StacksBlockId, NakamotoBlock, BlockHash)>,
        /// This represents deposit transactions
        deposits: HashMap<Txid, sbtc::rpc::GetTxResponse>,
    }

    impl TestHarness {
        fn generate(
            rng: &mut impl rand::RngCore,
            num_bitcoin_blocks: usize,
            num_stacks_blocks_per_bitcoin_block: std::ops::Range<usize>,
        ) -> Self {
            let mut bitcoin_blocks: Vec<_> =
                std::iter::repeat_with(|| dummy::block(&fake::Faker, rng))
                    .take(num_bitcoin_blocks)
                    .collect();

            for idx in 1..bitcoin_blocks.len() {
                bitcoin_blocks[idx].header.prev_blockhash = bitcoin_blocks[idx - 1].block_hash();
            }

            let first_header = NakamotoBlockHeader::empty();
            let stacks_blocks: Vec<(StacksBlockId, NakamotoBlock, BlockHash)> = bitcoin_blocks
                .iter()
                .scan(first_header, |previous_stx_block_header, btc_block| {
                    let num_blocks = num_stacks_blocks_per_bitcoin_block
                        .clone()
                        .choose(rng)
                        .unwrap_or_default();
                    let initial_state = previous_stx_block_header.clone();
                    let stacks_blocks: Vec<(StacksBlockId, NakamotoBlock, BlockHash)> =
                        std::iter::repeat_with(|| dummy::stacks_block(&fake::Faker, rng))
                            .take(num_blocks)
                            .scan(initial_state, |last_stx_block_header, mut stx_block| {
                                stx_block.header.parent_block_id = last_stx_block_header.block_id();
                                stx_block.header.chain_length =
                                    last_stx_block_header.chain_length + 1;
                                *last_stx_block_header = stx_block.header.clone();
                                Some((stx_block.block_id(), stx_block, btc_block.block_hash()))
                            })
                            .collect();

                    if let Some((_, stx_block, _)) = stacks_blocks.last() {
                        *previous_stx_block_header = stx_block.header.clone()
                    };

                    Some(stacks_blocks)
                })
                .flatten()
                .collect();

            Self {
                bitcoin_blocks,
                stacks_blocks,
                deposits: HashMap::new(),
            }
        }

        fn spawn_block_hash_stream(
            &self,
        ) -> tokio_stream::wrappers::ReceiverStream<bitcoin::BlockHash> {
            let headers: Vec<_> = self
                .bitcoin_blocks
                .iter()
                .map(|block| block.block_hash())
                .collect();

            let (tx, rx) = tokio::sync::mpsc::channel(128);

            tokio::spawn(async move {
                for header in headers {
                    tx.send(header).await.expect("failed to send header");
                }
            });

            rx.into()
        }
    }

    impl BitcoinInteract for TestHarness {
        type Error = error::Error;
        async fn get_block(
            &mut self,
            block_hash: &bitcoin::BlockHash,
        ) -> Result<Option<bitcoin::Block>, Self::Error> {
            Ok(self
                .bitcoin_blocks
                .iter()
                .find(|block| &block.block_hash() == block_hash)
                .cloned())
        }

        async fn estimate_fee_rate(&mut self) -> Result<f64, Self::Error> {
            unimplemented!()
        }
    }

    impl BitcoinClient for TestHarness {
        type Error = Error;
        fn get_tx(&self, txid: &Txid) -> Result<sbtc::rpc::GetTxResponse, Self::Error> {
            self.deposits.get(txid).cloned().ok_or(Error::Encryption)
        }
    }

    impl StacksInteract for TestHarness {
        async fn get_block(&self, block_id: StacksBlockId) -> Result<NakamotoBlock, error::Error> {
            self.stacks_blocks
                .iter()
                .skip_while(|(id, _, _)| &block_id != id)
                .map(|(_, block, _)| block)
                .next()
                .cloned()
                .ok_or(error::Error::MissingBlock)
        }
        async fn get_tenure(
            &self,
            block_id: StacksBlockId,
        ) -> Result<Vec<NakamotoBlock>, error::Error> {
            let (stx_block_id, stx_block, btc_block_id) = self
                .stacks_blocks
                .iter()
                .skip_while(|(id, _, _)| &block_id != id)
                .next()
                .ok_or(error::Error::MissingBlock)?;

            let blocks: Vec<NakamotoBlock> = self
                .stacks_blocks
                .iter()
                .skip_while(|(_, _, block_id)| block_id != btc_block_id)
                .take_while(|(block_id, _, _)| block_id != stx_block_id)
                .map(|(_, block, _)| block)
                .chain(std::iter::once(stx_block))
                .cloned()
                .collect();

            Ok(blocks)
        }
        async fn get_tenure_info(&self) -> Result<RPCGetTenureInfo, error::Error> {
            let (_, _, btc_block_id) = self.stacks_blocks.last().unwrap();

            Ok(RPCGetTenureInfo {
                consensus_hash: ConsensusHash([0; 20]),
                tenure_start_block_id: self
                    .stacks_blocks
                    .iter()
                    .skip_while(|(_, _, block_id)| block_id != btc_block_id)
                    .next()
                    .map(|(stx_block_id, _, _)| *stx_block_id)
                    .unwrap(),
                parent_consensus_hash: ConsensusHash([0; 20]),
                parent_tenure_start_block_id: StacksBlockId::first_mined(),
                tip_block_id: self
                    .stacks_blocks
                    .last()
                    .map(|(block_id, _, _)| *block_id)
                    .unwrap(),
                tip_height: self.stacks_blocks.len() as u64,
                reward_cycle: 0,
            })
        }

        fn nakamoto_start_height(&self) -> u64 {
            self.stacks_blocks
                .first()
                .map(|(_, block, _)| block.header.chain_length)
                .unwrap_or_default()
        }
        async fn estimate_fees<T>(&self, _: &T, _: FeePriority) -> Result<u64, error::Error>
        where
            T: crate::stacks::contracts::AsTxPayload,
        {
            Ok(500_000)
        }
    }

    impl EmilyInteract for () {
        async fn get_deposits(&mut self) -> Vec<CreateDepositRequest> {
            Vec::new()
        }
    }
}
