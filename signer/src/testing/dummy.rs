//! Utilities for generating dummy values on external types

use std::collections::BTreeMap;
use std::ops::Range;

use bitcoin::hashes::Hash as _;
use bitcoin::Address;
use bitcoin::Network;
use bitcoin::OutPoint;
use bitvec::array::BitArray;
use blockstack_lib::burnchains::Txid as StacksTxid;
use blockstack_lib::chainstate::{nakamoto, stacks};
use fake::Fake;
use rand::seq::IteratorRandom as _;
use rand::Rng;
use secp256k1::ecdsa::RecoverableSignature;
use stacks_common::address::C32_ADDRESS_VERSION_TESTNET_SINGLESIG;
use stacks_common::types::chainstate::StacksAddress;
use stacks_common::util::hash::Hash160;

use crate::keys::PrivateKey;
use crate::keys::PublicKey;
use crate::keys::SignerScriptPubKey as _;
use crate::stacks::events::CompletedDepositEvent;
use crate::stacks::events::WithdrawalAcceptEvent;
use crate::stacks::events::WithdrawalCreateEvent;
use crate::stacks::events::WithdrawalRejectEvent;
use crate::storage::model;

use crate::codec::Encode;
use crate::storage::model::BitcoinBlockHash;
use crate::storage::model::BitcoinTxId;
use crate::storage::model::StacksBlockHash;
use crate::storage::model::StacksPrincipal;
use crate::storage::model::StacksTxId;

/// Dummy block
pub fn block<R: rand::RngCore + ?Sized>(config: &fake::Faker, rng: &mut R) -> bitcoin::Block {
    let max_number_of_transactions = 20;

    let number_of_transactions = (rng.next_u32() % max_number_of_transactions) as usize;

    let mut txdata: Vec<bitcoin::Transaction> = std::iter::repeat_with(|| tx(config, rng))
        .take(number_of_transactions)
        .collect();

    txdata.insert(0, coinbase_tx(config, rng));

    let header = bitcoin::block::Header {
        version: bitcoin::block::Version::TWO,
        prev_blockhash: block_hash(config, rng),
        merkle_root: merkle_root(config, rng),
        time: config.fake_with_rng(rng),
        bits: bitcoin::CompactTarget::from_consensus(config.fake_with_rng(rng)),
        nonce: config.fake_with_rng(rng),
    };

    bitcoin::Block { header, txdata }
}

/// Dummy txid
pub fn txid<R: rand::RngCore + ?Sized>(config: &fake::Faker, rng: &mut R) -> bitcoin::Txid {
    let bytes: [u8; 32] = config.fake_with_rng(rng);
    bitcoin::Txid::from_byte_array(bytes)
}

/// Dummy transaction
pub fn tx<R: rand::RngCore + ?Sized>(config: &fake::Faker, rng: &mut R) -> bitcoin::Transaction {
    let max_input_size = 50;
    let max_output_size = 50;

    let input_size = (rng.next_u32() % max_input_size) as usize;
    let output_size = (rng.next_u32() % max_output_size) as usize;

    let input = std::iter::repeat_with(|| txin(config, rng))
        .take(input_size)
        .collect();
    let output = std::iter::repeat_with(|| txout(config, rng))
        .take(output_size)
        .collect();

    bitcoin::Transaction {
        version: bitcoin::transaction::Version::ONE,
        lock_time: bitcoin::absolute::LockTime::ZERO,
        input,
        output,
    }
}

/// Dummy transaction input
pub fn txin<R: rand::RngCore + ?Sized>(config: &fake::Faker, rng: &mut R) -> bitcoin::TxIn {
    bitcoin::TxIn {
        previous_output: bitcoin::OutPoint::new(txid(config, rng), config.fake_with_rng(rng)),
        sequence: bitcoin::Sequence::ZERO,
        script_sig: bitcoin::ScriptBuf::new(),
        witness: bitcoin::witness::Witness::new(),
    }
}

/// Dummy transaction output
pub fn txout<R: rand::RngCore + ?Sized>(config: &fake::Faker, rng: &mut R) -> bitcoin::TxOut {
    bitcoin::TxOut {
        value: bitcoin::Amount::from_sat(config.fake_with_rng(rng)),
        script_pubkey: bitcoin::ScriptBuf::new(),
    }
}

/// Dummy block hash
pub fn block_hash<R: rand::RngCore + ?Sized>(
    config: &fake::Faker,
    rng: &mut R,
) -> bitcoin::BlockHash {
    bitcoin::BlockHash::from_byte_array(config.fake_with_rng(rng))
}

/// Dummy merkle root
pub fn merkle_root<R: rand::RngCore + ?Sized>(
    config: &fake::Faker,
    rng: &mut R,
) -> bitcoin::TxMerkleNode {
    bitcoin::TxMerkleNode::from_byte_array(config.fake_with_rng(rng))
}

/// Dummy stacks block
pub fn stacks_block<R: rand::RngCore + ?Sized>(
    config: &fake::Faker,
    rng: &mut R,
) -> nakamoto::NakamotoBlock {
    let max_number_of_transactions = 20;

    let number_of_transactions = (rng.next_u32() % max_number_of_transactions) as usize;

    let txs = std::iter::repeat_with(|| stacks_tx(config, rng))
        .take(number_of_transactions)
        .collect();

    let header = nakamoto::NakamotoBlockHeader::empty();

    nakamoto::NakamotoBlock { header, txs }
}

/// Dummy stacks transaction
pub fn stacks_tx<R: rand::RngCore + ?Sized>(
    config: &fake::Faker,
    rng: &mut R,
) -> stacks::StacksTransaction {
    stacks::StacksTransaction {
        version: stacks::TransactionVersion::Testnet,
        chain_id: config.fake_with_rng(rng),
        auth: stacks::TransactionAuth::from_p2sh(&[], 0).unwrap(),
        anchor_mode: stacks::TransactionAnchorMode::Any,
        post_condition_mode: stacks::TransactionPostConditionMode::Allow,
        post_conditions: Vec::new(),
        payload: stacks::TransactionPayload::new_smart_contract(
            fake::faker::name::en::FirstName().fake_with_rng(rng),
            fake::faker::lorem::en::Paragraph(3..5)
                .fake_with_rng::<String, _>(rng)
                .as_str(),
            None,
        )
        .unwrap(),
    }
}

/// Dummy stacks transaction ID
pub fn stacks_txid<R: rand::RngCore + ?Sized>(
    config: &fake::Faker,
    rng: &mut R,
) -> blockstack_lib::burnchains::Txid {
    blockstack_lib::burnchains::Txid(config.fake_with_rng(rng))
}

/// Dummy signature
pub fn recoverable_signature<R>(config: &fake::Faker, rng: &mut R) -> RecoverableSignature
where
    R: rand::RngCore + ?Sized,
{
    // Represent the signed message.
    let digest: [u8; 32] = config.fake_with_rng(rng);
    let msg = secp256k1::Message::from_digest(digest);
    PrivateKey::new(rng).sign_ecdsa_recoverable(&msg)
}

/// Encrypted dummy DKG shares
pub fn encrypted_dkg_shares<R: rand::RngCore + rand::CryptoRng>(
    _config: &fake::Faker,
    rng: &mut R,
    signer_private_key: &[u8; 32],
    group_key: PublicKey,
) -> model::EncryptedDkgShares {
    let party_state = wsts::traits::PartyState {
        polynomial: None,
        private_keys: vec![],
        nonce: wsts::common::Nonce::random(rng),
    };

    let signer_state = wsts::traits::SignerState {
        id: 0,
        key_ids: vec![1],
        num_keys: 1,
        num_parties: 1,
        threshold: 1,
        group_key: group_key.into(),
        parties: vec![(0, party_state)],
    };

    let encoded = signer_state
        .encode_to_vec()
        .expect("encoding to vec failed");

    let encrypted_private_shares =
        wsts::util::encrypt(signer_private_key, &encoded, rng).expect("failed to encrypt");
    let public_shares: BTreeMap<u32, wsts::net::DkgPublicShares> = BTreeMap::new();
    let public_shares = public_shares
        .encode_to_vec()
        .expect("encoding to vec failed");

    model::EncryptedDkgShares {
        aggregate_key: group_key,
        encrypted_private_shares,
        public_shares,
        tweaked_aggregate_key: group_key.signers_tweaked_pubkey().unwrap(),
        script_pubkey: group_key.signers_script_pubkey().into_bytes(),
    }
}

/// Coinbase transaction with random block height
fn coinbase_tx<R: rand::RngCore + ?Sized>(
    config: &fake::Faker,
    rng: &mut R,
) -> bitcoin::Transaction {
    // Numbers below 17 are encoded differently which messes with the block height decoding
    let min_block_height = 17;
    let max_block_height = 10000;
    let block_height = rng.gen_range(min_block_height..max_block_height);
    let coinbase_script = bitcoin::script::Builder::new()
        .push_int(block_height)
        .into_script();

    let mut coinbase_tx = tx(config, rng);
    let mut coinbase_input = txin(config, rng);
    coinbase_input.script_sig = coinbase_script;
    coinbase_tx.input = vec![coinbase_input];

    coinbase_tx
}

impl fake::Dummy<fake::Faker> for PublicKey {
    fn dummy_with_rng<R: rand::Rng + ?Sized>(_: &fake::Faker, rng: &mut R) -> Self {
        let sk = secp256k1::SecretKey::new(rng);
        Self::from(secp256k1::PublicKey::from_secret_key_global(&sk))
    }
}

/// Used to for fine-grained control of generating fake testing addresses.
#[derive(Debug)]
pub struct BitcoinAddresses(pub Range<usize>);

impl fake::Dummy<BitcoinAddresses> for Vec<String> {
    fn dummy_with_rng<R: rand::Rng + ?Sized>(config: &BitcoinAddresses, rng: &mut R) -> Self {
        let num_addresses = config.0.clone().choose(rng).unwrap_or(1);
        std::iter::repeat_with(|| secp256k1::Keypair::new_global(rng))
            .take(num_addresses)
            .map(|kp| {
                let pk = bitcoin::CompressedPublicKey(kp.public_key());
                Address::p2wpkh(&pk, Network::Regtest).to_string()
            })
            .collect()
    }
}

impl fake::Dummy<fake::Faker> for WithdrawalAcceptEvent {
    fn dummy_with_rng<R: Rng + ?Sized>(config: &fake::Faker, rng: &mut R) -> Self {
        let bitmap = rng.next_u64() as u128;
        WithdrawalAcceptEvent {
            txid: blockstack_lib::burnchains::Txid(config.fake_with_rng(rng)),
            request_id: rng.next_u32() as u64,
            signer_bitmap: BitArray::new(bitmap.to_le_bytes()),
            outpoint: OutPoint {
                txid: txid(config, rng),
                vout: rng.next_u32(),
            },
            fee: rng.next_u32() as u64,
        }
    }
}

impl fake::Dummy<fake::Faker> for WithdrawalRejectEvent {
    fn dummy_with_rng<R: Rng + ?Sized>(config: &fake::Faker, rng: &mut R) -> Self {
        let bitmap = rng.next_u64() as u128;
        WithdrawalRejectEvent {
            txid: blockstack_lib::burnchains::Txid(config.fake_with_rng(rng)),
            request_id: rng.next_u32() as u64,
            signer_bitmap: BitArray::new(bitmap.to_le_bytes()),
        }
    }
}

impl fake::Dummy<fake::Faker> for WithdrawalCreateEvent {
    fn dummy_with_rng<R: Rng + ?Sized>(config: &fake::Faker, rng: &mut R) -> Self {
        let address_hash: [u8; 20] = config.fake_with_rng(rng);
        let version = C32_ADDRESS_VERSION_TESTNET_SINGLESIG;
        let kp = secp256k1::Keypair::new_global(rng);
        let pk = bitcoin::CompressedPublicKey(kp.public_key());

        WithdrawalCreateEvent {
            txid: StacksTxid(config.fake_with_rng(rng)),
            request_id: rng.next_u32() as u64,
            amount: rng.next_u32() as u64,
            sender: StacksAddress::new(version, Hash160(address_hash)).into(),
            recipient: Address::p2wpkh(&pk, Network::Regtest),
            max_fee: rng.next_u32() as u64,
            block_height: rng.next_u32() as u64,
        }
    }
}

impl fake::Dummy<fake::Faker> for CompletedDepositEvent {
    fn dummy_with_rng<R: Rng + ?Sized>(config: &fake::Faker, rng: &mut R) -> Self {
        CompletedDepositEvent {
            txid: blockstack_lib::burnchains::Txid(config.fake_with_rng(rng)),
            outpoint: OutPoint {
                txid: txid(config, rng),
                vout: rng.next_u32(),
            },
            amount: rng.next_u32() as u64,
        }
    }
}

impl fake::Dummy<fake::Faker> for BitcoinTxId {
    fn dummy_with_rng<R: Rng + ?Sized>(config: &fake::Faker, rng: &mut R) -> Self {
        From::<[u8; 32]>::from(config.fake_with_rng(rng))
    }
}

impl fake::Dummy<fake::Faker> for BitcoinBlockHash {
    fn dummy_with_rng<R: Rng + ?Sized>(config: &fake::Faker, rng: &mut R) -> Self {
        From::<[u8; 32]>::from(config.fake_with_rng(rng))
    }
}

impl fake::Dummy<fake::Faker> for StacksBlockHash {
    fn dummy_with_rng<R: Rng + ?Sized>(config: &fake::Faker, rng: &mut R) -> Self {
        From::<[u8; 32]>::from(config.fake_with_rng(rng))
    }
}

impl fake::Dummy<fake::Faker> for StacksTxId {
    fn dummy_with_rng<R: Rng + ?Sized>(config: &fake::Faker, rng: &mut R) -> Self {
        From::<[u8; 32]>::from(config.fake_with_rng(rng))
    }
}

impl fake::Dummy<fake::Faker> for StacksPrincipal {
    fn dummy_with_rng<R: Rng + ?Sized>(config: &fake::Faker, rng: &mut R) -> Self {
        let public_key: PublicKey = config.fake_with_rng(rng);
        let pubkey = stacks_common::util::secp256k1::Secp256k1PublicKey::from(&public_key);
        let address = StacksAddress::p2pkh(false, &pubkey);
        StacksPrincipal::from(clarity::vm::types::PrincipalData::from(address))
    }
}
