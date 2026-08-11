use super::{
    EvmLogsArgs, alloy_err, block_on, connect, log_execution, parse_block_tag, rpc_call,
    strip_hex_prefix,
};
use crate::{BlockchainError, truncate_tool_output};
use alloy::primitives::{Address, B256};
use alloy::providers::Provider;
use alloy::rpc::types::eth::Filter;
use std::str::FromStr;

async fn evm_logs_impl(
    rpc_url: &str,
    address_str: Option<&str>,
    topic0_str: Option<&str>,
    from_block_str: Option<&str>,
    to_block_str: Option<&str>,
) -> Result<String, BlockchainError> {
    let provider = connect(rpc_url)?;

    let mut filter = Filter::new();

    if let Some(addr) = address_str {
        let address = Address::from_str(addr)
            .map_err(|e| BlockchainError::Other(format!("invalid address: {e}")))?;
        filter = filter.address(address);
    }

    if let Some(t0) = topic0_str {
        // Shared helper tolerates both `0x` and `0X` — a raw 32-byte topic is
        // also accepted (alloy's B256 parse is case-insensitive on the hex).
        let topic = B256::from_str(strip_hex_prefix(t0))
            .map_err(|e| BlockchainError::Other(format!("invalid topic0: {e}")))?;
        filter = filter.event_signature(topic);
    }

    if let Some(fb) = from_block_str {
        filter = filter.from_block(parse_block_tag(fb)?);
    }

    if let Some(tb) = to_block_str {
        filter = filter.to_block(parse_block_tag(tb)?);
    }

    let logs = provider.get_logs(&filter).await.map_err(alloy_err)?;

    if logs.is_empty() {
        return Ok("no logs found for this filter".to_string());
    }

    let mut out = String::new();
    out.push_str(&format!("log_count: {}\n\n", logs.len()));

    for (i, log) in logs.iter().enumerate() {
        let log_address = log.address();
        let topics: Vec<String> = log.topics().iter().map(|t| format!("{t:#x}")).collect();
        let data_hex = hex::encode(log.data().data.clone());
        out.push_str(&format!(
            "log[{i}]:\n  address: {log_address}\n  topics: [{}]\n  data: 0x{data_hex}\n\n",
            topics.join(", ")
        ));
    }

    Ok(out)
}

/// Synchronous entry point: runs [`evm_logs_impl`] on the sidecar runtime and
/// caps the output at the shared byte budget.
pub fn execute_evm_logs(args: &EvmLogsArgs) -> Result<String, BlockchainError> {
    log_execution("evm_logs", &args.rpc_url);
    let output = block_on(rpc_call(evm_logs_impl(
        &args.rpc_url,
        args.address.as_deref(),
        args.topic0.as_deref(),
        args.from_block.as_deref(),
        args.to_block.as_deref(),
    )))??;
    Ok(truncate_tool_output(&output))
}

pub fn describe_evm_logs_invocation(args: &EvmLogsArgs) -> String {
    let mut desc = format!("Querying event logs on {}.", args.rpc_url);
    if let Some(addr) = args.address.as_deref() {
        desc.push_str(&format!(" Address: {addr}."));
    }
    if let Some(t0) = args.topic0.as_deref() {
        desc.push_str(&format!(" Topic0: {t0}."));
    }
    desc
}
