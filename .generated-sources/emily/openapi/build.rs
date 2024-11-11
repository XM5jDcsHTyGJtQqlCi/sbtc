use emily_handler::api;
use emily_handler::common;
use serde_json::json;
use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use utoipa::OpenApi;

fn main() {
    build_emily();
}

#[derive(utoipa::OpenApi)]
#[openapi(
    paths(
        // Health check endpoints.
        api::handlers::health::get_health,
        // Deposit endpoints.
        api::handlers::deposit::get_deposit,
        api::handlers::deposit::get_deposits_for_transaction,
        api::handlers::deposit::get_deposits,
        api::handlers::deposit::create_deposit,
        api::handlers::deposit::update_deposits,
        // Withdrawal endpoints.
        api::handlers::withdrawal::get_withdrawal,
        api::handlers::withdrawal::get_withdrawals,
        api::handlers::withdrawal::create_withdrawal,
        api::handlers::withdrawal::update_withdrawals,
        // Chainstate endpoints.
        api::handlers::chainstate::get_chain_tip,
        api::handlers::chainstate::get_chainstate_at_height,
        api::handlers::chainstate::set_chainstate,
        api::handlers::chainstate::update_chainstate,
        // Testing endpoints.
        api::handlers::testing::wipe_databases,
    ),
    components(schemas(
        // Chainstate models.
        api::models::chainstate::Chainstate,

        // Deposit models.
        api::models::deposit::Deposit,
        api::models::deposit::DepositParameters,
        api::models::deposit::DepositInfo,
        api::models::deposit::requests::CreateDepositRequestBody,
        api::models::deposit::requests::DepositUpdate,
        api::models::deposit::requests::UpdateDepositsRequestBody,
        api::models::deposit::responses::GetDepositsForTransactionResponse,
        api::models::deposit::responses::GetDepositsResponse,
        api::models::deposit::responses::UpdateDepositsResponse,

        // Withdrawal Models.
        api::models::withdrawal::Withdrawal,
        api::models::withdrawal::WithdrawalInfo,
        api::models::withdrawal::WithdrawalParameters,
        api::models::withdrawal::requests::CreateWithdrawalRequestBody,
        api::models::withdrawal::requests::WithdrawalUpdate,
        api::models::withdrawal::requests::UpdateWithdrawalsRequestBody,
        api::models::withdrawal::responses::GetWithdrawalsResponse,
        api::models::withdrawal::responses::UpdateWithdrawalsResponse,

        // Health check datatypes.
        api::models::health::responses::HealthData,

        // Common models.
        api::models::common::Status,
        api::models::common::Fulfillment,

        // Errors.
        common::error::ErrorResponse,
    ))
)]
struct ApiDoc;

pub fn build_emily() {
    // Ensure that we rerun if the API changes or the build script changes.
    println!("cargo:rerun-if-changed=../../../emily/handler/api");
    println!("cargo:rerun-if-changed=build.rs");

    let mut api_doc = ApiDoc::openapi();
    let new_extensions: HashMap<String, serde_json::Value> = new_operation_extensions();

    // TODO(269): Change Emily API Lambda Integrations to use cdk constructs if possible instead of specification
    // alteration.
    //
    // Add AWS extension to openapi specification so AWS CDK can attach the appropriate lambda endpoint.
    api_doc
        .paths
        .paths
        .iter_mut()
        .flat_map(|(_, path_item)| path_item.operations.iter_mut())
        .for_each(|(_, operation)| {
            operation
                .extensions
                .get_or_insert(Default::default())
                .extend(new_extensions.clone())
        });

    // Generate string for api doc.
    let spec_json = api_doc
        .to_pretty_json()
        .expect("Failed to serialize OpenAPI spec");

    // Open and write to file.
    let mut file =
        File::create("emily-openapi-spec.json").expect("Failed to create OpenAPI spec file");
    file.write_all(spec_json.as_bytes())
        .expect("Failed to write OpenAPI spec file");
}

/// Creates the map of the extensions to be included in each operation.
fn new_operation_extensions() -> HashMap<String, serde_json::Value> {
    let mut extensions: HashMap<String, serde_json::Value> = HashMap::new();
    extensions.insert(
        "x-amazon-apigateway-integration".to_string(),
        json!({
            "type": "aws_proxy",
            // Note that it's always meant to be POST regardless of the verb in the api spec.
            "httpMethod": "POST",
            "uri": {
                "Fn::Sub": "arn:${AWS::Partition}:apigateway:${AWS::Region}:lambda:path/2015-03-31/functions/${OperationLambda}/invocations"
            }
        })
    );
    extensions
}
