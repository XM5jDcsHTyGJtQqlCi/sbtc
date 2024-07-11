use signer::storage;
use signer::testing;

use futures::StreamExt;
use rand::RngCore;
use sqlx::Executor;

async fn test_environment(
    pool: sqlx::PgPool,
) -> testing::transaction_signer::TestEnvironment<impl FnMut() -> storage::postgres::PgStore> {
    let num_signers = 3;
    let signing_threshold = 2;
    let context_window = 3;
    let test_databases: Vec<_> = futures::stream::iter(0..num_signers)
        .then(|_| async { new_database(&pool).await })
        .collect()
        .await;

    let mut idx = 0;

    let test_model_parameters = testing::storage::model::Params {
        num_bitcoin_blocks: 20,
        num_stacks_blocks_per_bitcoin_block: 3,
        num_deposit_requests_per_block: 5,
        num_withdraw_requests_per_block: 5,
    };

    testing::transaction_signer::TestEnvironment {
        storage_constructor: move || {
            idx = (idx + 1) % test_databases.len();
            storage::postgres::PgStore::from(test_databases.get(idx).unwrap().clone())
        },
        context_window,
        num_signers,
        signing_threshold,
        test_model_parameters,
    }
}

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();

pub async fn new_database(pool: &sqlx::PgPool) -> sqlx::PgPool {
    let mut rng = rand::rngs::OsRng;
    let db_name = format!("test_db_{}", rng.next_u64());

    let create_db = format!("CREATE DATABASE \"{db_name}\";");
    pool.execute(create_db.as_str())
        .await
        .expect("failed to create test database");

    let base_url =
        std::env::var("DATABASE_URL").expect("DATABASE_URL must be set in the environment");
    let test_db_url = base_url.replace("signer", &db_name);

    let test_pool = sqlx::PgPool::connect(&test_db_url)
        .await
        .expect("failed to connect to test database");
    MIGRATOR
        .run(&test_pool)
        .await
        .expect("failed to run migrations against test database");

    test_pool
}

#[cfg_attr(not(feature = "integration-tests"), ignore)]
#[sqlx::test(migrations = false)]
async fn should_store_decisions_for_pending_deposit_requests(pool: sqlx::PgPool) {
    test_environment(pool)
        .await
        .assert_should_store_decisions_for_pending_deposit_requests()
        .await;
}

#[cfg_attr(not(feature = "integration-tests"), ignore)]
#[sqlx::test]
async fn should_store_decisions_for_pending_withdraw_requests(pool: sqlx::PgPool) {
    test_environment(pool)
        .await
        .assert_should_store_decisions_for_pending_withdraw_requests()
        .await;
}

#[cfg_attr(not(feature = "integration-tests"), ignore)]
#[sqlx::test(migrations = false)]
async fn should_store_decisions_received_from_other_signers(pool: sqlx::PgPool) {
    test_environment(pool)
        .await
        .assert_should_store_decisions_received_from_other_signers()
        .await;
}

#[cfg_attr(not(feature = "integration-tests"), ignore)]
#[sqlx::test(migrations = false)]
async fn should_respond_to_bitcoin_transaction_sign_request(pool: sqlx::PgPool) {
    test_environment(pool)
        .await
        .assert_should_respond_to_bitcoin_transaction_sign_requests()
        .await;
}

#[cfg_attr(not(feature = "integration-tests"), ignore)]
#[sqlx::test(migrations = false)]
async fn should_be_able_to_participate_in_signing_round(pool: sqlx::PgPool) {
    test_environment(pool)
        .await
        .assert_should_be_able_to_participate_in_signing_round()
        .await;
}
