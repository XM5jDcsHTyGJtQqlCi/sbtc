//! Populates the Emily database with entries.
//!
//! Note: This whole file is tech debt; there should be a command line method for
//! populating the database.
//!
//! TODO(370): Move this functionality to a CLI command.

use emily_handler::api::models::{
    chainstate::Chainstate,
    deposit::requests::CreateDepositRequestBody,
    withdrawal::{requests::CreateWithdrawalRequestBody, WithdrawalParameters},
};
use rand::Rng;

use crate::util::TestClient;

const NUM_ENTRIES: u32 = 30;

/// Populates emily.
#[cfg_attr(not(feature = "populate"), ignore)]
#[tokio::test]
pub async fn populate_emily() {
    let client = TestClient::new();
    create_deposits(&client).await;
    create_withdrawals(&client).await;
    create_chainstates(&client).await;
}

async fn create_deposits(client: &TestClient) {
    let mut rng = rand::thread_rng();
    for i in 0..NUM_ENTRIES {
        let n = rng.gen_range(1..=3);
        for j in 0..n {
            let offset = rng.gen_range(1..=4);
            let create_request = CreateDepositRequestBody {
                bitcoin_txid: format!("txid-{i}"),
                bitcoin_tx_output_index: j + offset,
                reclaim: format!("reclaim-script-{i}"),
                deposit: format!("deposit-script-{i}"),
            };
            client.create_deposit(&create_request).await;
        }
    }
}

async fn create_withdrawals(client: &TestClient) {
    let mut rng = rand::thread_rng();
    for i in 0..NUM_ENTRIES {
        let create_request = CreateWithdrawalRequestBody {
            request_id: i as u64,
            stacks_block_hash: format!("stacks-block-hash-{i}"),
            recipient: format!("recipient-{i}"),
            amount: rng.gen_range(1000..=1000000) as u64,
            parameters: WithdrawalParameters {
                max_fee: rng.gen_range(100..=300),
            },
        };
        client.create_withdrawal(&create_request).await;
    }
}

async fn create_chainstates(client: &TestClient) {
    for i in 0..NUM_ENTRIES {
        let create_request = Chainstate {
            stacks_block_height: i as u64,
            stacks_block_hash: format!("stacks-block-hash-{i}"),
        };
        client.create_chainstate(&create_request).await;
    }
}
