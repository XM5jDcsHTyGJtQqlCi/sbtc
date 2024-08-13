//! Test utilities for signer message

use bitcoin::hashes::Hash;
use fake::Fake;
use rand::seq::SliceRandom;

use crate::keys::PrivateKey;
use crate::keys::PublicKey;
use crate::message;
use crate::testing::dummy;

impl message::SignerMessage {
    /// Construct a random message
    pub fn random<R: rand::RngCore + ?Sized>(rng: &mut R) -> Self {
        fake::Faker.fake_with_rng(rng)
    }

    /// Construct a random message with the given payload type
    pub fn random_with_payload_type<
        P: Into<message::Payload> + fake::Dummy<fake::Faker>,
        R: rand::RngCore + ?Sized,
    >(
        rng: &mut R,
    ) -> Self {
        let payload = dummy_payload::<P, _>(&fake::Faker, rng);
        Self::random_with_payload(rng, payload)
    }

    /// Construct a random message with the given payload
    fn random_with_payload<R: rand::RngCore + ?Sized>(
        rng: &mut R,
        payload: message::Payload,
    ) -> Self {
        let mut block_hash_data = [0; 32];
        rng.fill_bytes(&mut block_hash_data);
        let block_hash = bitcoin::BlockHash::from_slice(&block_hash_data).unwrap();

        payload.to_message(block_hash)
    }
}

impl fake::Dummy<fake::Faker> for message::Payload {
    fn dummy_with_rng<R: rand::RngCore + ?Sized>(config: &fake::Faker, rng: &mut R) -> Self {
        let variants = [
            dummy_payload::<message::SignerDepositDecision, _>,
            dummy_payload::<message::SignerWithdrawDecision, _>,
            dummy_payload::<message::WstsMessage, _>,
        ];

        variants.choose(rng).unwrap()(config, rng)
    }
}

impl fake::Dummy<fake::Faker> for message::SignerMessage {
    fn dummy_with_rng<R: rand::RngCore + ?Sized>(config: &fake::Faker, rng: &mut R) -> Self {
        let payload: message::Payload = config.fake_with_rng(rng);

        Self::random_with_payload(rng, payload)
    }
}

impl fake::Dummy<fake::Faker> for message::SignerDepositDecision {
    fn dummy_with_rng<R: rand::RngCore + ?Sized>(config: &fake::Faker, rng: &mut R) -> Self {
        Self {
            output_index: config.fake_with_rng(rng),
            txid: dummy::txid(config, rng),
            accepted: config.fake_with_rng(rng),
        }
    }
}

impl fake::Dummy<fake::Faker> for message::BitcoinTransactionSignRequest {
    fn dummy_with_rng<R: rand::RngCore + ?Sized>(config: &fake::Faker, rng: &mut R) -> Self {
        let mut bytes: [u8; 32] = [0; 32];
        rng.fill_bytes(&mut bytes);
        let private_key = PrivateKey::new(rng);

        Self {
            tx: dummy::tx(config, rng),
            aggregate_key: PublicKey::from_private_key(&private_key),
        }
    }
}

impl fake::Dummy<fake::Faker> for message::BitcoinTransactionSignAck {
    fn dummy_with_rng<R: rand::RngCore + ?Sized>(config: &fake::Faker, rng: &mut R) -> Self {
        Self { txid: dummy::txid(config, rng) }
    }
}

impl fake::Dummy<fake::Faker> for message::StacksTransactionSignRequest {
    fn dummy_with_rng<R: rand::RngCore + ?Sized>(config: &fake::Faker, rng: &mut R) -> Self {
        Self {
            tx: dummy::stacks_tx(config, rng),
        }
    }
}

impl fake::Dummy<fake::Faker> for message::StacksTransactionSignature {
    fn dummy_with_rng<R: rand::RngCore + ?Sized>(config: &fake::Faker, rng: &mut R) -> Self {
        Self {
            txid: dummy::stacks_txid(config, rng),
            signature: dummy::signature(config, rng),
        }
    }
}

impl fake::Dummy<fake::Faker> for message::WstsMessage {
    fn dummy_with_rng<R: rand::RngCore + ?Sized>(config: &fake::Faker, rng: &mut R) -> Self {
        let dkg_end_begin = wsts::net::DkgEndBegin {
            dkg_id: config.fake_with_rng(rng),
            signer_ids: config.fake_with_rng(rng),
            key_ids: config.fake_with_rng(rng),
        };

        Self {
            txid: dummy::txid(config, rng),
            inner: wsts::net::Message::DkgEndBegin(dkg_end_begin),
        }
    }
}

fn dummy_payload<P, R>(config: &fake::Faker, rng: &mut R) -> message::Payload
where
    P: Into<message::Payload> + fake::Dummy<fake::Faker>,
    R: rand::RngCore + ?Sized,
{
    config.fake_with_rng::<P, _>(rng).into()
}
