//! # Transaction coordinator
//!
//! This module contains the transaction coordinator, which is the component of the sBTC signer
//! responsible for constructing transactions and coordinating signing rounds.
//!
//! For more details, see the [`TxCoordinatorEventLoop`] documentation.

use std::collections::BTreeSet;
use std::time::Duration;

use blockstack_lib::chainstate::stacks::StacksTransaction;
use futures::future::try_join_all;
use futures::Stream;
use futures::StreamExt as _;
use sha2::Digest;

use crate::bitcoin::utxo;
use crate::bitcoin::utxo::Fees;
use crate::bitcoin::BitcoinInteract;
use crate::bitcoin::TransactionLookupHint;
use crate::context::Context;
use crate::context::P2PEvent;
use crate::context::RequestDeciderEvent;
use crate::context::SignerCommand;
use crate::context::SignerEvent;
use crate::context::SignerSignal;
use crate::context::TxCoordinatorEvent;
use crate::context::TxSignerEvent;
use crate::ecdsa::SignEcdsa as _;
use crate::ecdsa::Signed;
use crate::emily_client::EmilyInteract;
use crate::error::Error;
use crate::keys::PrivateKey;
use crate::keys::PublicKey;
use crate::message;
use crate::message::BitcoinPreSignRequest;
use crate::message::Payload;
use crate::message::SignerMessage;
use crate::message::StacksTransactionSignRequest;
use crate::message::SweepTransactionInfo;
use crate::network;
use crate::signature::SighashDigest;
use crate::signature::TaprootSignature;
use crate::stacks::api::FeePriority;
use crate::stacks::api::GetNakamotoStartHeight;
use crate::stacks::api::StacksInteract;
use crate::stacks::api::SubmitTxResponse;
use crate::stacks::contracts::AsTxPayload;
use crate::stacks::contracts::CompleteDepositV1;
use crate::stacks::contracts::ContractCall;
use crate::stacks::contracts::RotateKeysV1;
use crate::stacks::contracts::SmartContract;
use crate::stacks::contracts::SMART_CONTRACTS;
use crate::stacks::wallet::MultisigTx;
use crate::stacks::wallet::SignerWallet;
use crate::storage::model;
use crate::storage::model::StacksTxId;
use crate::storage::DbRead as _;
use crate::wsts_state_machine::CoordinatorStateMachine;

use bitcoin::hashes::Hash as _;
use wsts::net::SignatureType;
use wsts::state_machine::coordinator::Coordinator as _;
use wsts::state_machine::coordinator::State as WstsCoordinatorState;
use wsts::state_machine::OperationResult as WstsOperationResult;
use wsts::state_machine::StateMachine as _;

#[cfg_attr(doc, aquamarine::aquamarine)]
/// # Transaction coordinator event loop
///
/// This struct contains the implementation of the transaction coordinator
/// logic. The coordinator subscribes to [`TxSignerEvent::NewRequestsHandled`]
/// events (from the transaction signer) and listens to signer messages over the
/// signer P2P network.
///
/// The transaction coordinator will look up the canonical chain tip from the
/// database upon receiving a [`TxSignerEvent::NewRequestsHandled`] event from
/// the transaction signer. This tip is used to decide whether this particular
/// signer is selected as the signers' coordinator or if it should be passive in
/// favor of another signer as the coordinator in the signer network.
///
/// When the coordinator is selected, that coordinator will begin by looking up
/// the signer UTXO, and do a fee rate estimation for both Bitcoin and Stacks.
/// With that in place it will proceed to look up any pending[^1] and active[^2]
/// requests to process.
///
/// The pending requests are used to construct a transaction package, which is a
/// set of bitcoin transactions fulfilling a subset of the requests. Which
/// pending requests that end up in the transaction package depends on the
/// amount of signers deciding to accept the request, and on the maximum fee
/// allowed in the requests. Once the package has been constructed, the
/// coordinator proceeds by coordinating WSTS signing rounds for each of the
/// transactions in the package. The signed transactions are then broadcast to
/// bitcoin.

/// Pending deposit and withdrawal requests are used to construct a Bitcoin
/// transaction package consisting of a set of inputs and outputs that fulfill
/// these requests. The fulfillment of pending requests in the transaction
/// package depends on the number of signers agreeing to accept each request and
/// the maximum fee stipulated in the request. Once the package is assembled,
/// the coordinator coordinates WSTS signing rounds for each transaction within
/// the package. The successfully signed transactions are then broadcast to the
/// Bitcoin network.
///
/// For the active requests, the coordinator will go over each one and create
/// appropriate stacks response transactions (which are the `withdrawal-accept`,
/// `withdrawal-reject` and `deposit-accept` contract calls). These transactions
/// are sent through the signers for signatures, and once enough signatures has
/// been gathered, the coordinator broadcasts them to the Stacks blockchain.
///
/// [^1]: A deposit or withdraw request is considered pending if it is confirmed
///       on chain but hasn't been fulfilled in an sBTC transaction yet.
/// [^2]: A deposit or withdraw request is considered active if has been
///       fulfilled in an sBTC transaction,
///       but the result hasn't been acknowledged on Stacks as a
///       `deposit-accept`, `withdraw-accept` or `withdraw-reject` transaction.
///
/// The whole flow is illustrated in the following flowchart.
///
/// ```mermaid
/// flowchart TD
///     SM[New requests handled notification] --> GCT(Get canonical chain tip)
///     GCT --> ISC{Is selected?}
///     ISC --> |No| DONE[Done]
///     ISC --> |Yes| GSU(Get signer UTXO)
///     GSU --> ESF(Estimate fee rates)
///
///     ESF --> GPR(Get accepted pending requests)
///     GPR --> CTP(Compute transaction package)
///     CTP --> CSR(Coordinate signing rounds)
///     CSR --> BST(Broadcast signed transactions)
///
///     ESF --> GAR(Get active requests)
///     GAR --> CRT(Construct response transactions)
///     CRT --> CMS(Coordinate multisig signature gather)
///     CMS --> BST
///     BST --> DONE
/// ```
#[derive(Debug)]
pub struct TxCoordinatorEventLoop<Context, Network> {
    /// The signer context.
    pub context: Context,
    /// Interface to the signer network.
    pub network: Network,
    /// Private key of the coordinator for network communication.
    pub private_key: PrivateKey,
    /// the number of signatures required.
    pub threshold: u16,
    /// How many bitcoin blocks back from the chain tip the signer will
    /// look for requests.
    pub context_window: u16,
    /// The maximum duration of a signing round before the coordinator will
    /// time out and return an error.
    pub signing_round_max_duration: Duration,
    /// The maximum duration of distributed key generation before the
    /// coordinator will time out and return an error.
    pub dkg_max_duration: Duration,
    /// Whether the coordinator has already deployed the contracts.
    pub sbtc_contracts_deployed: bool,
    /// An indicator for whether the Stacks blockchain has reached Nakamoto
    /// 3. If we are not in Nakamoto 3 or later, then the coordinator does
    /// not do any work.
    pub is_epoch3: bool,
}

/// This function defines which messages this event loop is interested
/// in.
fn run_loop_message_filter(signal: &SignerSignal) -> bool {
    matches!(
        signal,
        SignerSignal::Event(SignerEvent::RequestDecider(
            RequestDeciderEvent::NewRequestsHandled,
        )) | SignerSignal::Command(SignerCommand::Shutdown)
    )
}

/// During DKG or message signing, we only need the following message
/// types, so we construct a stream with only these messages.
fn signed_message_filter(event: &SignerSignal) -> bool {
    matches!(
        event,
        SignerSignal::Event(SignerEvent::TxSigner(TxSignerEvent::MessageGenerated(_)))
            | SignerSignal::Event(SignerEvent::P2P(P2PEvent::MessageReceived(_)))
    )
}

impl<C, N> TxCoordinatorEventLoop<C, N>
where
    C: Context,
    N: network::MessageTransfer,
{
    /// Run the coordinator event loop
    #[tracing::instrument(skip_all, name = "tx-coordinator")]
    pub async fn run(mut self) -> Result<(), Error> {
        tracing::info!("starting transaction coordinator event loop");
        let mut signal_stream = self.context.as_signal_stream(run_loop_message_filter);

        while let Some(message) = signal_stream.next().await {
            match message {
                SignerSignal::Command(SignerCommand::Shutdown) => break,
                SignerSignal::Command(SignerCommand::P2PPublish(_)) => {}
                SignerSignal::Event(event) => {
                    if let SignerEvent::RequestDecider(RequestDeciderEvent::NewRequestsHandled) =
                        event
                    {
                        tracing::debug!("received signal; processing requests");
                        if let Err(error) = self.process_new_blocks().await {
                            tracing::error!(
                                %error,
                                "error processing requests; skipping this round"
                            );
                        }
                        tracing::trace!("sending tenure completed signal");
                        self.context
                            .signal(TxCoordinatorEvent::TenureCompleted.into())?;
                    }
                }
            }
        }

        tracing::info!("transaction coordinator event loop is stopping");

        Ok(())
    }

    /// A function that filters the [`Context::as_signal_stream`] stream
    /// for items that the coordinator might care about, which includes
    /// some network messages and transaction signer messages.
    async fn to_signed_message(event: SignerSignal) -> Option<Signed<SignerMessage>> {
        match event {
            SignerSignal::Event(SignerEvent::TxSigner(TxSignerEvent::MessageGenerated(msg)))
            | SignerSignal::Event(SignerEvent::P2P(P2PEvent::MessageReceived(msg))) => Some(msg),
            _ => None,
        }
    }

    async fn is_epoch3(&mut self) -> Result<bool, Error> {
        if self.is_epoch3 {
            return Ok(true);
        }
        tracing::debug!("checked for whether we are in Epoch 3 or later");
        let pox_info = self.context.get_stacks_client().get_pox_info().await?;

        let Some(nakamoto_start_height) = pox_info.nakamoto_start_height() else {
            return Ok(false);
        };

        let is_epoch3 = pox_info.current_burnchain_block_height > nakamoto_start_height;
        if is_epoch3 {
            self.is_epoch3 = is_epoch3;
            tracing::debug!("we are in Epoch 3 or later; time to do work");
        }
        Ok(is_epoch3)
    }

    #[tracing::instrument(
        skip_all,
        fields(public_key = %self.signer_public_key(), chain_tip = tracing::field::Empty)
    )]
    async fn process_new_blocks(&mut self) -> Result<(), Error> {
        if !self.is_epoch3().await? {
            return Ok(());
        }

        let bitcoin_processing_delay = self.context.config().signer.bitcoin_processing_delay;
        if bitcoin_processing_delay > Duration::ZERO {
            tracing::debug!("sleeping before processing new Bitcoin block.");
            tokio::time::sleep(bitcoin_processing_delay).await;
        }

        let bitcoin_chain_tip = self
            .context
            .get_storage()
            .get_bitcoin_canonical_chain_tip()
            .await?
            .ok_or(Error::NoChainTip)?;

        let span = tracing::Span::current();
        span.record("chain_tip", tracing::field::display(&bitcoin_chain_tip));

        // We first need to determine if we are the coordinator, so we need
        // to know the current signing set. If we are the coordinator then
        // we need to know the aggregate key for constructing bitcoin
        // transactions. We need to know the current signing set and the
        // current aggregate key.
        let (maybe_aggregate_key, signer_public_keys) = self
            .get_signer_set_and_aggregate_key(&bitcoin_chain_tip)
            .await?;

        // If we are not the coordinator, then we have no business
        // coordinating DKG or constructing bitcoin and stacks
        // transactions, might as well return early.
        if !self.is_coordinator(&bitcoin_chain_tip, &signer_public_keys) {
            tracing::debug!("we are not the coordinator, so nothing to do");
            return Ok(());
        }

        tracing::debug!("we are the coordinator, we may need to coordinate DKG");
        // If Self::get_signer_set_and_aggregate_key did not return an
        // aggregate key, then we know that we have not run DKG yet. Since
        // we are the coordinator, we should coordinate DKG.
        let aggregate_key = match maybe_aggregate_key {
            Some(key) => key,
            // This function returns the new DKG aggregate key.
            None => {
                let dkg_result = self.coordinate_dkg(&bitcoin_chain_tip).await?;
                // TODO: in `run_dkg_from_scratch` test, `dkg_result` differs from
                // value fetched from the db. Adding a temporary fix for the (probably)
                // race condition, but we should address this properly.
                self.get_signer_set_and_aggregate_key(&bitcoin_chain_tip)
                    .await
                    .ok()
                    .and_then(|res| res.0)
                    .unwrap_or(dkg_result)
            }
        };

        self.deploy_smart_contracts(&bitcoin_chain_tip, &aggregate_key)
            .await?;

        self.check_and_submit_rotate_key_transaction(&bitcoin_chain_tip, &aggregate_key)
            .await?;

        self.construct_and_sign_bitcoin_sbtc_transactions(
            &bitcoin_chain_tip,
            &aggregate_key,
            &signer_public_keys,
        )
        .await?;

        self.construct_and_sign_stacks_sbtc_response_transactions(
            &bitcoin_chain_tip,
            &aggregate_key,
        )
        .await?;

        Ok(())
    }

    /// Submit the rotate key tx for the latest DKG shares, if the aggregate key
    /// differs from the one in the smart contract registry
    #[tracing::instrument(skip_all)]
    async fn check_and_submit_rotate_key_transaction(
        &mut self,
        bitcoin_chain_tip: &model::BitcoinBlockHash,
        aggregate_key: &PublicKey,
    ) -> Result<(), Error> {
        if !self.all_smart_contracts_deployed().await? {
            return Ok(());
        }

        let last_dkg = self
            .context
            .get_storage()
            .get_latest_encrypted_dkg_shares()
            .await?;

        // If we don't have DKG shares nothing to do here
        let Some(last_dkg) = last_dkg else {
            return Ok(());
        };

        let current_aggregate_key = self
            .context
            .get_stacks_client()
            .get_current_signers_aggregate_key(&self.context.config().signer.deployer)
            .await?;

        // If the latest DKG aggregate key matches on-chain data, nothing to do here
        if Some(last_dkg.aggregate_key) == current_aggregate_key {
            tracing::debug!("stacks-core is up to date with the current aggregate key");
            return Ok(());
        }

        let wallet = self.get_signer_wallet(bitcoin_chain_tip).await?;
        // current_aggregate_key define which wallet can sign stacks tx interacting
        // with the registry smart contract; fallbacks to `aggregate_key` if it's
        // the first rotate key tx.
        let signing_key = &current_aggregate_key.unwrap_or(*aggregate_key);

        self.construct_and_sign_rotate_key_transaction(
            bitcoin_chain_tip,
            signing_key,
            &last_dkg.aggregate_key,
            &wallet,
        )
        .await
        .map(|_| ())
    }

    /// Construct and coordinate WSTS signing rounds for sBTC transactions on Bitcoin,
    /// fulfilling pending deposit and withdraw requests.
    #[tracing::instrument(skip_all)]
    async fn construct_and_sign_bitcoin_sbtc_transactions(
        &mut self,
        bitcoin_chain_tip: &model::BitcoinBlockHash,
        aggregate_key: &PublicKey,
        signer_public_keys: &BTreeSet<PublicKey>,
    ) -> Result<(), Error> {
        tracing::debug!("fetching the stacks chain tip");
        let stacks_chain_tip = self
            .context
            .get_storage()
            .get_stacks_chain_tip(bitcoin_chain_tip)
            .await?
            .ok_or(Error::NoStacksChainTip)?;

        tracing::debug!(
            stacks_chain_tip = %stacks_chain_tip.block_hash,
            "retrieved the stacks chain tip"
        );

        let pending_requests_fut =
            self.get_pending_requests(bitcoin_chain_tip, aggregate_key, signer_public_keys);

        // If Self::get_pending_requests returns Ok(None) then there are no
        // requests to respond to, so let's just exit.
        let Some(pending_requests) = pending_requests_fut.await? else {
            tracing::debug!("no requests to handle, exiting");
            return Ok(());
        };
        tracing::debug!(
            num_deposits = %pending_requests.deposits.len(),
            num_withdrawals = pending_requests.withdrawals.len(),
            "fetched requests"
        );
        // Construct the transaction package and store it in the database.
        let transaction_package = pending_requests.construct_transactions()?;
        // Get the requests from the transaction package because they have been split into
        // multiple transactions.
        let sbtc_requests = BitcoinPreSignRequest {
            requests: transaction_package
                .iter()
                .map(|tx| (&tx.requests).into())
                .collect(),
            fee_rate: pending_requests.signer_state.fee_rate,
            last_fees: pending_requests.signer_state.last_fees.map(Into::into),
        };

        // Share the list of requests with the signers.
        self.send_message(sbtc_requests, bitcoin_chain_tip).await?;
        // Wait to reduce chance that the other signers will receive the subsequent
        // messages before the BitcoinPreSignRequest one.
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        for mut transaction in transaction_package {
            self.sign_and_broadcast(
                bitcoin_chain_tip,
                aggregate_key,
                signer_public_keys,
                &mut transaction,
            )
            .await?;

            // TODO: if this (considering also fallback clients) fails, we will
            // need to handle the inconsistency of having the sweep tx confirmed
            // but emily deposit still marked as pending.
            self.context
                .get_emily_client()
                .accept_deposits(&transaction, &stacks_chain_tip)
                .await?;
        }

        Ok(())
    }

    /// Construct and coordinate signing rounds for `deposit-accept`,
    /// `withdraw-accept` and `withdraw-reject` transactions.
    ///
    /// # Notes
    ///
    /// This function does the following.
    /// 1. Load the stacks wallet from the database. This wallet is
    ///    determined by the public keys and threshold stored in the last
    ///    [`RotateKeysTransaction`] object that is returned from the
    ///    database.
    /// 2. Fetch all requests from the database where we can finish the
    ///    fulfillment with only a Stacks transaction. These are requests
    ///    that where we have a response transactions on bitcoin fulfilling
    ///    the deposit or withdrawal request.
    /// 3. Construct a sign-request object for each of the requests
    ///    identified in (2).
    /// 4. Broadcast this sign-request to the network and wait for
    ///    responses.
    /// 5. If there are enough signatures then broadcast the transaction.
    #[tracing::instrument(skip_all)]
    async fn construct_and_sign_stacks_sbtc_response_transactions(
        &mut self,
        chain_tip: &model::BitcoinBlockHash,
        bitcoin_aggregate_key: &PublicKey,
    ) -> Result<(), Error> {
        let wallet = SignerWallet::load(&self.context, chain_tip).await?;
        let stacks = self.context.get_stacks_client();

        // Fetch deposit and withdrawal requests from the database where
        // there has been a confirmed bitcoin transaction associated with
        // the request.
        //
        // For deposits, there will be at most one such bitcoin transaction
        // on the blockchain identified by the chain tip, where an input is
        // the deposit UTXO.
        //
        // For withdrawals, we need to have a record of the `request_id`
        // associated with the bitcoin transaction's outputs.

        let deposit_requests = self
            .context
            .get_storage()
            .get_swept_deposit_requests(chain_tip, self.context_window)
            .await?;

        if deposit_requests.is_empty() {
            tracing::debug!("no stacks transactions to create, exiting");
            return Ok(());
        }

        tracing::debug!(
            num_deposits = %deposit_requests.len(),
            "we have deposit requests that have been swept that may need minting"
        );
        // We need to know the nonce to use, so we reach out to our stacks
        // node for the account information for our multi-sig address.
        //
        // Note that the wallet object will automatically increment the
        // nonce for each transaction that it creates.
        let account = stacks.get_account(wallet.address()).await?;
        wallet.set_nonce(account.nonce);

        for req in deposit_requests {
            let outpoint = req.deposit_outpoint();
            let sign_request_fut =
                self.construct_deposit_stacks_sign_request(req, bitcoin_aggregate_key, &wallet);

            let (sign_request, multi_tx) = match sign_request_fut.await {
                Ok(res) => res,
                Err(error) => {
                    tracing::error!(%error, "could not construct a transaction completing the deposit request");
                    continue;
                }
            };

            // If we fail to sign the transaction for some reason, we
            // decrement the nonce by one, and try the next transaction.
            // This is not a fatal error, since we could fail to sign the
            // transaction because someone else is now the coordinator, and
            // all the signers are now ignoring us.
            let process_request_fut =
                self.process_sign_request(sign_request, chain_tip, multi_tx, &wallet);

            match process_request_fut.await {
                Ok(txid) => {
                    tracing::info!(%txid, "successfully submitted complete-deposit transaction")
                }
                Err(error) => {
                    tracing::warn!(
                        %error,
                        txid = %outpoint.txid,
                        vout = %outpoint.vout,
                        "could not process the stacks sign request for a deposit"
                    );
                    wallet.set_nonce(wallet.get_nonce().saturating_sub(1));
                }
            }
        }

        Ok(())
    }

    /// Construct and coordinate signing round for a `rotate-keys-wrapper` transaction.
    #[tracing::instrument(skip_all)]
    async fn construct_and_sign_rotate_key_transaction(
        &mut self,
        bitcoin_chain_tip: &model::BitcoinBlockHash,
        aggregate_key: &PublicKey,
        rotate_key_aggregate_key: &PublicKey,
        wallet: &SignerWallet,
    ) -> Result<StacksTxId, Error> {
        // TODO: we should validate the contract call before asking others
        // to sign it.
        let contract_call = ContractCall::RotateKeysV1(RotateKeysV1::new(
            wallet,
            self.context.config().signer.deployer,
            rotate_key_aggregate_key,
        ));

        // Rotate key transactions should be done as soon as possible, so
        // we set the fee rate to the high priority fee.
        let tx_fee = self
            .context
            .get_stacks_client()
            .estimate_fees(wallet, &contract_call, FeePriority::High)
            .await?;

        let multi_tx = MultisigTx::new_tx(&contract_call, wallet, tx_fee);
        let tx = multi_tx.tx();

        let sign_request = StacksTransactionSignRequest {
            aggregate_key: *aggregate_key,
            contract_tx: contract_call.into(),
            nonce: tx.get_origin_nonce(),
            tx_fee: tx.get_tx_fee(),
            digest: tx.digest(),
            txid: tx.txid(),
        };

        self.process_sign_request(sign_request, bitcoin_chain_tip, multi_tx, wallet)
            .await
    }

    /// Sign and broadcast the stacks transaction
    #[tracing::instrument(skip_all)]
    async fn process_sign_request(
        &mut self,
        sign_request: StacksTransactionSignRequest,
        chain_tip: &model::BitcoinBlockHash,
        multi_tx: MultisigTx,
        wallet: &SignerWallet,
    ) -> Result<StacksTxId, Error> {
        let tx = self
            .sign_stacks_transaction(sign_request, multi_tx, chain_tip, wallet)
            .await?;

        match self.context.get_stacks_client().submit_tx(&tx).await {
            Ok(SubmitTxResponse::Acceptance(txid)) => Ok(txid.into()),
            Ok(SubmitTxResponse::Rejection(err)) => Err(err.into()),
            Err(err) => Err(err),
        }
    }

    /// Transform the swept deposit request into a Stacks sign request
    /// object.
    ///
    /// This function uses bitcoin-core to help with the fee assessment of
    /// the deposit request, and stacks-core for fee estimation of the
    /// transaction.
    #[tracing::instrument(skip_all)]
    async fn construct_deposit_stacks_sign_request(
        &self,
        req: model::SweptDepositRequest,
        bitcoin_aggregate_key: &PublicKey,
        wallet: &SignerWallet,
    ) -> Result<(StacksTransactionSignRequest, MultisigTx), Error> {
        // Retrieve the Bitcoin sweep transaction from the Bitcoin node. We
        // can't get it from the database because the transaction is
        // only in the node's mempool at this point.
        let tx_info = self
            .context
            .get_bitcoin_client()
            .get_tx_info(&req.sweep_txid, &req.sweep_block_hash)
            .await?
            .ok_or_else(|| {
                Error::BitcoinTxMissing(req.sweep_txid.into(), Some(req.sweep_block_hash.into()))
            })?;

        let outpoint = req.deposit_outpoint();
        let assessed_bitcoin_fee = tx_info
            .assess_input_fee(&outpoint)
            .ok_or_else(|| Error::OutPointMissing(outpoint))?;

        // TODO: we should validate the contract call before asking others
        // to sign it.
        let contract_call = ContractCall::CompleteDepositV1(CompleteDepositV1 {
            amount: req.amount - assessed_bitcoin_fee.to_sat(),
            outpoint,
            recipient: req.recipient.into(),
            deployer: self.context.config().signer.deployer,
            sweep_txid: req.sweep_txid,
            sweep_block_hash: req.sweep_block_hash,
            sweep_block_height: req.sweep_block_height,
        });

        // Complete deposit requests should be done as soon as possible, so
        // we set the fee rate to the high priority fee.
        let tx_fee = self
            .context
            .get_stacks_client()
            .estimate_fees(wallet, &contract_call, FeePriority::High)
            .await?;

        let multi_tx = MultisigTx::new_tx(&contract_call, wallet, tx_fee);
        let tx = multi_tx.tx();

        let sign_request = StacksTransactionSignRequest {
            aggregate_key: *bitcoin_aggregate_key,
            contract_tx: contract_call.into(),
            nonce: tx.get_origin_nonce(),
            tx_fee: tx.get_tx_fee(),
            digest: tx.digest(),
            txid: tx.txid(),
        };

        Ok((sign_request, multi_tx))
    }

    /// Attempt to sign the stacks transaction.
    #[tracing::instrument(skip_all)]
    async fn sign_stacks_transaction(
        &mut self,
        req: StacksTransactionSignRequest,
        mut multi_tx: MultisigTx,
        chain_tip: &model::BitcoinBlockHash,
        wallet: &SignerWallet,
    ) -> Result<StacksTransaction, Error> {
        let txid = req.txid;

        // We ask for the signers to sign our transaction (including
        // ourselves, via our tx signer event loop)
        self.send_message(req, chain_tip).await?;

        let max_duration = self.signing_round_max_duration;
        let signal_stream = self
            .context
            .as_signal_stream(signed_message_filter)
            .filter_map(Self::to_signed_message);

        tokio::pin!(signal_stream);

        let future = async {
            while multi_tx.num_signatures() < wallet.signatures_required() {
                // If signal_stream.next() returns None then one of the
                // underlying streams has closed. That means either the
                // network stream, the internal message stream, or the
                // termination handler stream has closed. This is all bad,
                // so we trigger a shutdown.
                let Some(msg) = signal_stream.next().await else {
                    tracing::warn!("signal stream returned None, shutting down");
                    self.context.get_termination_handle().signal_shutdown();
                    return Err(Error::SignerShutdown);
                };
                // TODO: We need to verify these messages, but it is best
                // to do that at the source when we receive the message.

                if &msg.bitcoin_chain_tip != chain_tip {
                    tracing::warn!(
                        sender = %msg.signer_pub_key,
                        "concurrent signing round message observed"
                    );
                    continue;
                }

                let sig = match msg.inner.payload {
                    Payload::StacksTransactionSignature(sig) if sig.txid == txid => sig,
                    _ => continue,
                };

                if let Err(error) = multi_tx.add_signature(sig.signature) {
                    tracing::warn!(
                        %txid,
                        %error,
                        offending_public_key = %msg.signer_pub_key,
                        "got an invalid signature"
                    );
                }
            }

            Ok::<_, Error>(multi_tx.finalize_transaction())
        };

        tokio::time::timeout(max_duration, future)
            .await
            .map_err(|_| Error::SignatureTimeout(txid))?
    }

    /// Coordinate a signing round for the given request
    /// and broadcast it once it's signed.
    #[tracing::instrument(skip_all)]
    async fn sign_and_broadcast(
        &mut self,
        bitcoin_chain_tip: &model::BitcoinBlockHash,
        aggregate_key: &PublicKey,
        signer_public_keys: &BTreeSet<PublicKey>,
        transaction: &mut utxo::UnsignedTransaction<'_>,
    ) -> Result<(), Error> {
        let mut coordinator_state_machine = CoordinatorStateMachine::load(
            &mut self.context.get_storage_mut(),
            *aggregate_key,
            signer_public_keys.clone(),
            self.threshold,
            self.private_key,
        )
        .await?;
        let sighashes = transaction.construct_digests()?;
        let msg = sighashes.signers.to_raw_hash().to_byte_array();

        let txid = transaction.tx.compute_txid();

        let signature = self
            .coordinate_signing_round(
                bitcoin_chain_tip,
                &mut coordinator_state_machine,
                txid,
                &msg,
                SignatureType::Taproot(None),
            )
            .await?;

        let signer_witness = bitcoin::Witness::p2tr_key_spend(&signature.into());

        let mut deposit_witness = Vec::new();

        for (deposit, sighash) in sighashes.deposits.into_iter() {
            let msg = sighash.to_raw_hash().to_byte_array();

            let mut coordinator_state_machine = CoordinatorStateMachine::load(
                &mut self.context.get_storage_mut(),
                *aggregate_key,
                signer_public_keys.clone(),
                self.threshold,
                self.private_key,
            )
            .await?;

            let signature = self
                .coordinate_signing_round(
                    bitcoin_chain_tip,
                    &mut coordinator_state_machine,
                    txid,
                    &msg,
                    SignatureType::Schnorr,
                )
                .await?;

            let witness = deposit.construct_witness_data(signature.into());

            deposit_witness.push(witness);
        }

        let witness_data: Vec<bitcoin::Witness> = std::iter::once(signer_witness)
            .chain(deposit_witness)
            .collect();

        transaction
            .tx
            .input
            .iter_mut()
            .zip(witness_data)
            .for_each(|(tx_in, witness)| {
                tx_in.witness = witness;
            });

        tracing::info!("broadcasting bitcoin transaction");
        // Broadcast the transaction to the Bitcoin network.
        self.context
            .get_bitcoin_client()
            .broadcast_transaction(&transaction.tx)
            .await?;

        // Publish the transaction to the P2P network so that peers get advance
        // knowledge of the sweep.
        self.send_message(
            SweepTransactionInfo::from_unsigned_at_block(bitcoin_chain_tip, transaction),
            bitcoin_chain_tip,
        )
        .await?;

        tracing::info!("bitcoin transaction accepted by bitcoin-core");

        Ok(())
    }

    #[tracing::instrument(skip_all)]
    async fn coordinate_signing_round(
        &mut self,
        bitcoin_chain_tip: &model::BitcoinBlockHash,
        coordinator_state_machine: &mut CoordinatorStateMachine,
        txid: bitcoin::Txid,
        msg: &[u8],
        signature_type: SignatureType,
    ) -> Result<TaprootSignature, Error> {
        let outbound = coordinator_state_machine
            .start_signing_round(msg, signature_type)
            .map_err(Error::wsts_coordinator)?;

        // We create a signal stream before sending a message so that there
        // is no race condition with the steam and the getting a response.
        let signal_stream = self
            .context
            .as_signal_stream(signed_message_filter)
            .filter_map(Self::to_signed_message);

        let msg = message::WstsMessage { txid, inner: outbound.msg };
        self.send_message(msg, bitcoin_chain_tip).await?;

        let max_duration = self.signing_round_max_duration;
        let run_signing_round = self.drive_wsts_state_machine(
            signal_stream,
            bitcoin_chain_tip,
            coordinator_state_machine,
            txid,
        );

        let operation_result = tokio::time::timeout(max_duration, run_signing_round)
            .await
            .map_err(|_| Error::CoordinatorTimeout(max_duration.as_secs()))??;

        match operation_result {
            WstsOperationResult::SignTaproot(sig) | WstsOperationResult::SignSchnorr(sig) => {
                Ok(sig.into())
            }
            result => Err(Error::UnexpectedOperationResult(Box::new(result))),
        }
    }

    /// Set up a WSTS coordinator state machine and run DKG with the other
    /// signers in the signing set.
    #[tracing::instrument(skip_all)]
    async fn coordinate_dkg(
        &mut self,
        chain_tip: &model::BitcoinBlockHash,
    ) -> Result<PublicKey, Error> {
        tracing::info!("Coordinating DKG");
        // Get the current signer set for running DKG.
        //
        // Also, note that in order to change the signing set we must first
        // run DKG (which the current function is doing), and DKG requires
        // us to define signing set (which is returned in the next
        // non-comment line). That function essentially uses the signing
        // set of the last DKG (either through the last rotate-keys
        // contract call or from the `dkg_shares` table) so we wind up
        // never changing the signing set.
        let (_, signer_set) = self.get_signer_set_and_aggregate_key(chain_tip).await?;

        let mut state_machine =
            CoordinatorStateMachine::new(signer_set, self.threshold, self.private_key);

        // Okay let's move the coordinator state machine to the beginning
        // of the DKG phase.
        state_machine
            .move_to(WstsCoordinatorState::DkgPublicDistribute)
            .map_err(Error::wsts_coordinator)?;

        let outbound = state_machine
            .start_public_shares()
            .map_err(Error::wsts_coordinator)?;

        // We identify the DKG round by a 32-byte hash which we throw
        // around as a bitcoin transaction ID, even when it is not one. We
        // should probably change this
        let identifier = self.coordinator_id(chain_tip);
        let txid = bitcoin::Txid::from_byte_array(identifier);
        let msg = message::WstsMessage { txid, inner: outbound.msg };

        // We create a signal stream before sending a message so that there
        // is no race condition with the steam and the getting a response.
        let signal_stream = self
            .context
            .as_signal_stream(signed_message_filter)
            .filter_map(Self::to_signed_message);

        // This message effectively kicks off DKG. The `TxSignerEventLoop`s
        // running on the signers will pick up this message and act on it,
        // including our own. When they do they create a signing state
        // machine and begin DKG.
        self.send_message(msg, chain_tip).await?;

        // Now that DKG has "begun" we need to drive it to completion.
        let max_duration = self.dkg_max_duration;
        let dkg_fut =
            self.drive_wsts_state_machine(signal_stream, chain_tip, &mut state_machine, txid);

        let operation_result = tokio::time::timeout(max_duration, dkg_fut)
            .await
            .map_err(|_| Error::CoordinatorTimeout(max_duration.as_secs()))??;

        match operation_result {
            WstsOperationResult::Dkg(aggregate_key) => PublicKey::try_from(&aggregate_key),
            result => Err(Error::UnexpectedOperationResult(Box::new(result))),
        }
    }

    #[tracing::instrument(skip_all)]
    async fn drive_wsts_state_machine<S>(
        &mut self,
        signal_stream: S,
        bitcoin_chain_tip: &model::BitcoinBlockHash,
        coordinator_state_machine: &mut CoordinatorStateMachine,
        txid: bitcoin::Txid,
    ) -> Result<WstsOperationResult, Error>
    where
        S: Stream<Item = Signed<SignerMessage>>,
    {
        // this assumes that the signer set doesn't change for the duration of this call,
        // but we're already assuming that the bitcoin chain tip doesn't change
        // alternately we could hit the DB every time we get a new message
        let (_, signer_set) = self
            .get_signer_set_and_aggregate_key(bitcoin_chain_tip)
            .await?;

        tokio::pin!(signal_stream);

        coordinator_state_machine.save();
        // Let's get the next message from the network or the
        // TxSignerEventLoop.
        //
        // If signal_stream.next() returns None then one of the underlying
        // streams has closed. That means either the internal message
        // channel, or the termination handler channel has closed. This is
        // all bad, so we trigger a shutdown.
        while let Some(msg) = signal_stream.next().await {
            if &msg.bitcoin_chain_tip != bitcoin_chain_tip {
                tracing::warn!(sender = %msg.signer_pub_key, "concurrent WSTS activity observed");
                continue;
            }

            if !msg.verify() {
                tracing::warn!(?msg, "invalid signature");
                continue;
            }

            let Payload::WstsMessage(wsts_msg) = msg.inner.payload else {
                continue;
            };

            let packet = wsts::net::Packet {
                msg: wsts_msg.inner,
                sig: Vec::new(),
            };

            let msg_public_key = msg.signer_pub_key;

            let sender_is_coordinator =
                given_key_is_coordinator(msg_public_key, bitcoin_chain_tip, &signer_set);

            let public_keys = &coordinator_state_machine.get_config().signer_public_keys;
            let public_key_point = p256k1::point::Point::from(msg_public_key);

            // check that messages were signed by correct key
            let is_authenticated = Self::authenticate_message(
                &packet,
                public_keys,
                public_key_point,
                sender_is_coordinator,
            );

            if !is_authenticated {
                continue;
            }

            let (outbound_packet, operation_result) =
                match coordinator_state_machine.process_message(&packet) {
                    Ok(val) => val,
                    Err(err) => {
                        tracing::warn!(?packet, reason = %err, "ignoring packet");
                        continue;
                    }
                };

            if let Some(packet) = outbound_packet {
                let msg = message::WstsMessage { txid, inner: packet.msg };
                self.send_message(msg, bitcoin_chain_tip).await?;
            }

            match operation_result {
                Some(res) => return Ok(res),
                None => continue,
            }
        }

        tracing::warn!("signal stream returned None, shutting down");
        self.context.get_termination_handle().signal_shutdown();
        Err(Error::SignerShutdown)
    }

    fn authenticate_message(
        packet: &wsts::net::Packet,
        public_keys: &hashbrown::HashMap<u32, p256k1::point::Point>,
        public_key_point: p256k1::point::Point,
        sender_is_coordinator: bool,
    ) -> bool {
        let check_signer_public_key = |signer_id| match public_keys.get(&signer_id) {
            Some(signer_public_key) if public_key_point != *signer_public_key => {
                tracing::warn!(
                    ?packet.msg,
                    reason = "message was signed by the wrong signer",
                    "ignoring packet"
                );
                false
            }
            None => {
                tracing::warn!(
                    ?packet.msg,
                    reason = "no public key for signer",
                    %signer_id,
                    "ignoring packet"
                );
                false
            }
            _ => true,
        };
        match &packet.msg {
            wsts::net::Message::DkgBegin(_)
            | wsts::net::Message::DkgPrivateBegin(_)
            | wsts::net::Message::DkgEndBegin(_)
            | wsts::net::Message::NonceRequest(_)
            | wsts::net::Message::SignatureShareRequest(_) => {
                if !sender_is_coordinator {
                    tracing::warn!(
                        ?packet,
                        reason = "got coordinator message from sender who is not coordinator",
                        "ignoring packet"
                    );
                    false
                } else {
                    true
                }
            }

            wsts::net::Message::DkgPublicShares(dkg_public_shares) => {
                check_signer_public_key(dkg_public_shares.signer_id)
            }
            wsts::net::Message::DkgPrivateShares(dkg_private_shares) => {
                check_signer_public_key(dkg_private_shares.signer_id)
            }
            wsts::net::Message::DkgEnd(dkg_end) => check_signer_public_key(dkg_end.signer_id),
            wsts::net::Message::NonceResponse(nonce_response) => {
                check_signer_public_key(nonce_response.signer_id)
            }
            wsts::net::Message::SignatureShareResponse(sig_share_response) => {
                check_signer_public_key(sig_share_response.signer_id)
            }
        }
    }

    // Determine if the current coordinator is the coordinator.
    //
    // The coordinator is decided using the hash of the bitcoin
    // chain tip. We don't use the chain tip directly because
    // it typically starts with a lot of leading zeros.
    //
    // Note that this function is technically not fallible,
    // but for now we have chosen to return phantom errors
    // instead of adding expects/unwraps in the code.
    // Ideally the code should be formulated in a way to guarantee
    // it being infallible without relying on sequentially coupling
    // expressions. However, that is left for future work.
    fn is_coordinator(
        &self,
        bitcoin_chain_tip: &model::BitcoinBlockHash,
        signer_public_keys: &BTreeSet<PublicKey>,
    ) -> bool {
        given_key_is_coordinator(self.pub_key(), bitcoin_chain_tip, signer_public_keys)
    }

    /// Constructs a new [`utxo::SignerBtcState`] based on the current market
    /// fee rate, the signer's UTXO, and the last sweep package.
    #[tracing::instrument(skip(self, aggregate_key))]
    pub async fn get_btc_state(
        &mut self,
        chain_tip: &model::BitcoinBlockHash,
        aggregate_key: &PublicKey,
    ) -> Result<utxo::SignerBtcState, Error> {
        let bitcoin_client = self.context.get_bitcoin_client();
        let fee_rate = bitcoin_client.estimate_fee_rate().await?;

        // Retrieve the signer's current UTXO.
        let utxo = self
            .context
            .get_storage()
            .get_signer_utxo(chain_tip, self.context_window)
            .await?
            .ok_or(Error::MissingSignerUtxo)?;

        let last_fees = self.assess_mempool_sweep_transaction_fees(&utxo).await?;

        Ok(utxo::SignerBtcState {
            fee_rate,
            utxo,
            public_key: bitcoin::XOnlyPublicKey::from(aggregate_key),
            last_fees,
            magic_bytes: [b'T', b'3'], //TODO(#472): Use the correct magic bytes.
        })
    }

    /// TODO(#742): This function needs to filter deposit requests based on
    /// time as well. We need to do this because deposit requests are locked
    /// using OP_CSV, which lock up coins based on block height or
    /// multiples of 512 seconds measure by the median time past.
    #[tracing::instrument(skip_all)]
    async fn get_pending_requests(
        &mut self,
        bitcoin_chain_tip: &model::BitcoinBlockHash,
        aggregate_key: &PublicKey,
        signer_public_keys: &BTreeSet<PublicKey>,
    ) -> Result<Option<utxo::SbtcRequests>, Error> {
        tracing::debug!("Fetching pending deposit and withdrawal requests");
        let context_window = self.context_window;
        let threshold = self.threshold;

        let pending_deposit_requests = self
            .context
            .get_storage()
            .get_pending_accepted_deposit_requests(bitcoin_chain_tip, context_window, threshold)
            .await?;

        let pending_withdraw_requests = self
            .context
            .get_storage()
            .get_pending_accepted_withdrawal_requests(bitcoin_chain_tip, context_window, threshold)
            .await?;

        let mut deposits: Vec<utxo::DepositRequest> = Vec::new();

        for req in pending_deposit_requests {
            let votes = self
                .context
                .get_storage()
                .get_deposit_request_signer_votes(&req.txid, req.output_index, aggregate_key)
                .await?;

            let deposit = utxo::DepositRequest::from_model(req, votes);
            deposits.push(deposit);
        }

        let mut withdrawals: Vec<utxo::WithdrawalRequest> = Vec::new();

        for req in pending_withdraw_requests {
            let votes = self
                .context
                .get_storage()
                .get_withdrawal_request_signer_votes(&req.qualified_id(), aggregate_key)
                .await?;

            let withdrawal = utxo::WithdrawalRequest::from_model(req, votes);
            withdrawals.push(withdrawal);
        }

        let num_signers = signer_public_keys
            .len()
            .try_into()
            .map_err(|_| Error::TypeConversion)?;

        if deposits.is_empty() && withdrawals.is_empty() {
            return Ok(None);
        }

        Ok(Some(utxo::SbtcRequests {
            deposits,
            withdrawals,
            signer_state: self.get_btc_state(bitcoin_chain_tip, aggregate_key).await?,
            accept_threshold: threshold,
            num_signers,
        }))
    }

    /// Return the signing set that can make sBTC related contract calls
    /// along with the current aggregate key to use for locking UTXOs on
    /// bitcoin.
    ///
    /// The aggregate key fetched here is the one confirmed on the
    /// canonical Stacks blockchain as part of a `rotate-keys` contract
    /// call. It will be the public key that is the result of a DKG run. If
    /// there are no rotate-keys transactions on the canonical stacks
    /// blockchain, then we fall back on the last known DKG shares row in
    /// our database, and return None as the aggregate key if no DKG shares
    /// can be found, implying that this signer has not participated in
    /// DKG.
    #[tracing::instrument(skip_all)]
    pub async fn get_signer_set_and_aggregate_key(
        &self,
        bitcoin_chain_tip: &model::BitcoinBlockHash,
    ) -> Result<(Option<PublicKey>, BTreeSet<PublicKey>), Error> {
        let db = self.context.get_storage();

        // We are supposed to submit a rotate-keys transaction after
        // running DKG, but that transaction may not have been submitted
        // yet (if we have just run DKG) or it may not have been confirmed
        // on the canonical Stacks blockchain.
        //
        // If the signers have already run DKG, then we know that all
        // participating signers should have the same view of the latest
        // aggregate key, so we can fall back on the stored DKG shares for
        // getting the current aggregate key and associated signing set.
        match db.get_last_key_rotation(bitcoin_chain_tip).await? {
            Some(last_key) => {
                let aggregate_key = last_key.aggregate_key;
                let signer_set = last_key.signer_set.into_iter().collect();
                Ok((Some(aggregate_key), signer_set))
            }
            None => match db.get_latest_encrypted_dkg_shares().await? {
                Some(shares) => {
                    let signer_set = shares.signer_set_public_keys.into_iter().collect();
                    Ok((Some(shares.aggregate_key), signer_set))
                }
                None => Ok((None, self.context.config().signer.bootstrap_signing_set())),
            },
        }
    }

    fn pub_key(&self) -> PublicKey {
        PublicKey::from_private_key(&self.private_key)
    }

    /// This function provides a deterministic 32-byte identifier for the
    /// signer.
    fn coordinator_id(&self, chain_tip: &model::BitcoinBlockHash) -> [u8; 32] {
        sha2::Sha256::new_with_prefix("SIGNER_COORDINATOR_ID")
            .chain_update(self.pub_key().serialize())
            .chain_update(chain_tip.into_bytes())
            .finalize()
            .into()
    }

    #[tracing::instrument(skip_all)]
    async fn send_message(
        &mut self,
        msg: impl Into<Payload>,
        bitcoin_chain_tip: &model::BitcoinBlockHash,
    ) -> Result<(), Error> {
        let msg = msg
            .into()
            .to_message(*bitcoin_chain_tip)
            .sign_ecdsa(&self.private_key)?;

        self.network.broadcast(msg.clone()).await?;
        self.context
            .signal(TxCoordinatorEvent::MessageGenerated(msg).into())?;

        Ok(())
    }

    /// Deploy an sBTC smart contract to the stacks node.
    async fn deploy_smart_contract(
        &mut self,
        contract_deploy: SmartContract,
        chain_tip: &model::BitcoinBlockHash,
        bitcoin_aggregate_key: &PublicKey,
        wallet: &SignerWallet,
    ) -> Result<(), Error> {
        let stacks = self.context.get_stacks_client();

        // Maybe this smart contract has already been deployed, let's check
        // that first.
        let deployer = self.context.config().signer.deployer;
        if contract_deploy.is_deployed(&stacks, &deployer).await? {
            return Ok(());
        }

        // The contract is not deployed yet, so we can proceed
        tracing::info!("Contract not deployed yet, proceeding with deployment");

        let sign_request_fut = self.construct_deploy_contracts_stacks_sign_request(
            contract_deploy,
            bitcoin_aggregate_key,
            wallet,
        );

        let (sign_request, multi_tx) = sign_request_fut.await?;

        // If we fail to sign the transaction for some reason, we
        // decrement the nonce by one, and try the next transaction.
        // This is not a fatal error, since we could fail to sign the
        // transaction because someone else is now the coordinator, and
        // all the signers are now ignoring us.
        let process_request_fut =
            self.process_sign_request(sign_request, chain_tip, multi_tx, wallet);

        match process_request_fut.await {
            Ok(txid) => {
                tracing::info!(%txid, "successfully submitted contract deploy transaction");
                Ok(())
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    "could not process the stacks sign request for a contract deploy"
                );
                wallet.set_nonce(wallet.get_nonce().saturating_sub(1));
                Err(error)
            }
        }
    }

    async fn construct_deploy_contracts_stacks_sign_request(
        &self,
        contract_deploy: SmartContract,
        bitcoin_aggregate_key: &PublicKey,
        wallet: &SignerWallet,
    ) -> Result<(StacksTransactionSignRequest, MultisigTx), Error> {
        let tx_fee = self
            .context
            .get_stacks_client()
            .estimate_fees(wallet, &contract_deploy.tx_payload(), FeePriority::High)
            .await?;
        let multi_tx = MultisigTx::new_tx(&contract_deploy, wallet, tx_fee);
        let tx = multi_tx.tx();

        let sign_request = StacksTransactionSignRequest {
            aggregate_key: *bitcoin_aggregate_key,
            contract_tx: contract_deploy.into(),
            nonce: tx.get_origin_nonce(),
            tx_fee: tx.get_tx_fee(),
            digest: tx.digest(),
            txid: tx.txid(),
        };

        Ok((sign_request, multi_tx))
    }

    /// Deploy all sBTC smart contracts to the stacks node (if not already deployed).
    /// If a contract fails to deploy, the function will return an error.
    #[tracing::instrument(skip_all)]
    pub async fn deploy_smart_contracts(
        &mut self,
        chain_tip: &model::BitcoinBlockHash,
        bitcoin_aggregate_key: &PublicKey,
    ) -> Result<(), Error> {
        if self.all_smart_contracts_deployed().await? {
            return Ok(());
        }

        let wallet = self.get_signer_wallet(chain_tip).await?;
        for contract in SMART_CONTRACTS {
            self.deploy_smart_contract(contract, chain_tip, bitcoin_aggregate_key, &wallet)
                .await?;
        }

        Ok(())
    }

    async fn all_smart_contracts_deployed(&mut self) -> Result<bool, Error> {
        if self.sbtc_contracts_deployed {
            return Ok(true);
        }

        let stacks = self.context.get_stacks_client();
        let deployer = self.context.config().signer.deployer;

        for contract in SMART_CONTRACTS {
            if !contract.is_deployed(&stacks, &deployer).await? {
                return Ok(false);
            }
        }

        self.sbtc_contracts_deployed = true;
        Ok(true)
    }

    async fn get_signer_wallet(
        &self,
        chain_tip: &model::BitcoinBlockHash,
    ) -> Result<SignerWallet, Error> {
        let wallet = SignerWallet::load(&self.context, chain_tip).await?;

        // We need to know the nonce to use, so we reach out to our stacks
        // node for the account information for our multi-sig address.
        //
        // Note that the wallet object will automatically increment the
        // nonce for each transaction that it creates.
        let stacks = self.context.get_stacks_client();
        let account = stacks.get_account(wallet.address()).await?;
        wallet.set_nonce(account.nonce);

        Ok(wallet)
    }

    fn signer_public_key(&self) -> PublicKey {
        PublicKey::from_private_key(&self.private_key)
    }

    /// Assesses the total fees paid for any outstanding sweep transactions in
    /// the mempool which may need to be RBF'd. If there are no sweep
    /// transactions which are spending the signer's UTXO, then this function
    /// will return [`None`].
    ///
    /// TODO: This method currently blindly assumes that the mempool transactions
    /// are correct. Maybe we need some validation?
    #[tracing::instrument(skip_all, fields(signer_utxo = %signer_utxo.outpoint))]
    pub async fn assess_mempool_sweep_transaction_fees(
        &self,
        signer_utxo: &utxo::SignerUtxo,
    ) -> Result<Option<Fees>, Error> {
        let bitcoin_client = self.context.get_bitcoin_client();

        // Find the mempool transactions that are spending the provided UTXO.
        let mempool_txs_spending_utxo = bitcoin_client
            .find_mempool_transactions_spending_output(&signer_utxo.outpoint)
            .await?;

        // If no transactions are found, we have nothing to do.
        if mempool_txs_spending_utxo.is_empty() {
            tracing::debug!(
                outpoint = %signer_utxo.outpoint,
                "no mempool transactions found spending signer output; nothing to do"
            );
            return Ok(None);
        }

        tracing::debug!(
            outpoint = %signer_utxo.outpoint,
            "found mempool transactions spending signer output; assessing fees"
        );

        // If we have some transactions, we need to find the one that pays the
        // highest fee. This is the transaction that we will use as the root of
        // the sweep package. Note that even if only one transaction was
        // returned above, we still need to get the fee for it, which is why
        // there's no special logic for one vs multiple.
        //
        // This can technically error if the mempool transactions are not found,
        // but it shouldn't happen since we got the transaction ids from
        // bitcoin-core itself.
        let best_sweep_root = try_join_all(mempool_txs_spending_utxo.iter().map(|txid| {
            let bitcoin_client = bitcoin_client.clone();
            async move {
                bitcoin_client
                    .get_transaction_fee(txid, Some(TransactionLookupHint::Mempool))
                    .await
                    .map(|fee| (txid, fee))
            }
        }))
        .await?
        .into_iter()
        .max_by_key(|(_, fees)| fees.fee);

        // Since we got the transaction ids from bitcoin-core, these should
        // not be missing, but we double-check here just in case (it could
        // happen that the client has failed-over to the next node which isn't
        // in sync with the previous one, for example).
        let Some((best_sweep_root_txid, fees)) = best_sweep_root else {
            tracing::warn!(
                outpoint = %signer_utxo.outpoint,
                "no fees found for mempool transactions spending signer output"
            );
            return Ok(None);
        };

        // Retrieve all descendant transactions of the best sweep root.
        let descendant_txids = bitcoin_client
            .find_mempool_descendants(best_sweep_root_txid)
            .await?;

        // Retrieve fees for all descendant transactions. If there were no
        // descendants then this will just result in an empty list.
        let descendant_fees = try_join_all(descendant_txids.iter().map(|txid| {
            let bitcoin_client = bitcoin_client.clone();
            async move {
                bitcoin_client
                    .get_transaction_fee(txid, Some(TransactionLookupHint::Mempool))
                    .await
            }
        }))
        .await?;

        // Sum the fees of the best sweep root and its descendants, while also
        // summing the vsize of the transactions for fee-rate calculation later.
        // If there were no descendants then this will just be the fee and size
        // from the best root sweep transaction.
        let (total_fees, total_vsize) = descendant_fees
            .into_iter()
            .fold((fees.fee, fees.vsize), |acc, fees| {
                (acc.0 + fees.fee, acc.1 + fees.vsize)
            });

        // Calculate the fee rate based on the total fees and vsizes of the
        // transactions which we've found. Since this is returning transactions
        // from bitcoin-core, we should have valid fees and sizes, so we don't
        // need to check for division by zero.
        let rate = total_fees as f64 / total_vsize as f64;

        Ok(Some(Fees { total: total_fees, rate }))
    }
}

/// Check if the provided public key is the coordinator for the provided chain tip
pub fn given_key_is_coordinator(
    pub_key: PublicKey,
    bitcoin_chain_tip: &model::BitcoinBlockHash,
    signer_public_keys: &BTreeSet<PublicKey>,
) -> bool {
    coordinator_public_key(bitcoin_chain_tip, signer_public_keys) == Some(pub_key)
}

/// Find the coordinator public key
pub fn coordinator_public_key(
    bitcoin_chain_tip: &model::BitcoinBlockHash,
    signer_public_keys: &BTreeSet<PublicKey>,
) -> Option<PublicKey> {
    let mut hasher = sha2::Sha256::new();
    hasher.update(bitcoin_chain_tip.into_bytes());
    let digest: [u8; 32] = hasher.finalize().into();
    // <[u8; 32]>::first_chunk<N> will return None if the requested slice
    // is greater than 32 bytes. Since we are converting to a `usize`, the
    // number of bytes necessary depends on the width of pointers on the
    // machine that compiled this binary. Since we only support systems
    // with a target pointer width of either 4 or 8 bytes, the <[u8;
    // 32]>::first_chunk<N> call will return Some(_) since N > 4 or 8.
    // Also, do humans even make machines where the pointer width is
    // greater than 32 bytes?
    let index = usize::from_be_bytes(*digest.first_chunk()?);
    let num_signers = signer_public_keys.len();

    signer_public_keys.iter().nth(index % num_signers).copied()
}

#[cfg(test)]
mod tests {
    use crate::bitcoin::MockBitcoinInteract;
    use crate::emily_client::MockEmilyInteract;
    use crate::stacks::api::MockStacksInteract;
    use crate::storage::in_memory::SharedStore;
    use crate::testing;
    use crate::testing::context::*;
    use crate::testing::transaction_coordinator::TestEnvironment;

    use test_log::test;

    fn test_environment() -> TestEnvironment<
        TestContext<
            SharedStore,
            WrappedMock<MockBitcoinInteract>,
            WrappedMock<MockStacksInteract>,
            WrappedMock<MockEmilyInteract>,
        >,
    > {
        let test_model_parameters = testing::storage::model::Params {
            num_bitcoin_blocks: 20,
            num_stacks_blocks_per_bitcoin_block: 3,
            num_deposit_requests_per_block: 5,
            num_withdraw_requests_per_block: 5,
            num_signers_per_request: 7,
        };

        let context = TestContext::builder()
            .with_in_memory_storage()
            .with_mocked_clients()
            .build();

        // TODO: fix tech debt #893 then raise threshold to 5
        TestEnvironment {
            context,
            context_window: 5,
            num_signers: 7,
            signing_threshold: 3,
            test_model_parameters,
        }
    }

    #[ignore = "we have a test for this"]
    #[test(tokio::test)]
    async fn should_be_able_to_coordinate_signing_rounds() {
        test_environment()
            .assert_should_be_able_to_coordinate_signing_rounds(std::time::Duration::ZERO)
            .await;
    }

    #[ignore = "we have a test for this"]
    #[tokio::test]
    async fn should_be_able_to_skip_deploy_sbtc_contracts() {
        test_environment()
            .assert_skips_deploy_sbtc_contracts()
            .await;
    }

    #[ignore = "This is sensitive to the values set in the config"]
    #[tokio::test]
    async fn should_wait_before_processing_bitcoin_blocks() {
        // NOTE: Above test `should_be_able_to_coordinate_signing_rounds`
        // could be removed as redundant now.

        // Measure baseline.
        let baseline_start = std::time::Instant::now();
        test_environment()
            .assert_should_be_able_to_coordinate_signing_rounds(std::time::Duration::ZERO)
            .await;
        // Locally this takes a couple seconds to execute.
        // This truncates the decimals.
        let baseline_elapsed = std::time::Duration::from_secs(baseline_start.elapsed().as_secs());

        let delay_i = 3;
        let delay = std::time::Duration::from_secs(delay_i);
        std::env::set_var(
            "SIGNER_SIGNER__BITCOIN_PROCESSING_DELAY",
            delay_i.to_string(),
        );
        let start = std::time::Instant::now();
        test_environment()
            .assert_should_be_able_to_coordinate_signing_rounds(delay)
            .await;
        more_asserts::assert_gt!(start.elapsed(), delay + baseline_elapsed);
    }

    #[tokio::test]
    async fn should_get_signer_utxo_simple() {
        test_environment().assert_get_signer_utxo_simple().await;
    }

    #[tokio::test]
    async fn should_get_signer_utxo_fork() {
        test_environment().assert_get_signer_utxo_fork().await;
    }

    #[tokio::test]
    async fn should_get_signer_utxo_unspent() {
        test_environment().assert_get_signer_utxo_unspent().await;
    }

    #[tokio::test]
    async fn should_get_signer_utxo_donations() {
        test_environment().assert_get_signer_utxo_donations().await;
    }
}
