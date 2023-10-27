use hyper::server::{conn::AddrIncoming, Builder};

use casper_json_rpc::{CorsOrigin, RequestHandlersBuilder};
use casper_types::ProtocolVersion;

use super::ReactorEventT;
use crate::{
    effect::EffectBuilder,
    rpcs::{
        speculative_exec::{SpeculativeExec, SpeculativeExecTxn},
        RpcWithParams,
    },
};

/// The URL path for all JSON-RPC requests.
pub const SPECULATIVE_EXEC_API_PATH: &str = "rpc";

pub const SPECULATIVE_EXEC_SERVER_NAME: &str = "speculative execution";

/// Run the speculative execution server.
pub(super) async fn run<REv: ReactorEventT>(
    builder: Builder<AddrIncoming>,
    effect_builder: EffectBuilder<REv>,
    api_version: ProtocolVersion,
    qps_limit: u64,
    max_body_bytes: u32,
    cors_origin: String,
) {
    let mut handlers = RequestHandlersBuilder::new();
    SpeculativeExecTxn::register_as_handler(effect_builder, api_version, &mut handlers);
    SpeculativeExec::register_as_handler(effect_builder, api_version, &mut handlers);
    let handlers = handlers.build();

    match cors_origin.as_str() {
        "" => {
            super::rpcs::run(
                builder,
                handlers,
                qps_limit,
                max_body_bytes,
                SPECULATIVE_EXEC_API_PATH,
                SPECULATIVE_EXEC_SERVER_NAME,
            )
            .await;
        }
        "*" => {
            super::rpcs::run_with_cors(
                builder,
                handlers,
                qps_limit,
                max_body_bytes,
                SPECULATIVE_EXEC_API_PATH,
                SPECULATIVE_EXEC_SERVER_NAME,
                CorsOrigin::Any,
            )
            .await
        }
        _ => {
            super::rpcs::run_with_cors(
                builder,
                handlers,
                qps_limit,
                max_body_bytes,
                SPECULATIVE_EXEC_API_PATH,
                SPECULATIVE_EXEC_SERVER_NAME,
                CorsOrigin::Specified(cors_origin),
            )
            .await
        }
    }
}
