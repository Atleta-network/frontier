// This file is part of Frontier.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

//! Debug rpc interface.

use ethereum::AccessListItem;
use ethereum_types::{H160, H256, U256};
use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use serde::{de::Error, Deserializer, Deserialize};

use client_evm_tracing::types::{block, single};

use crate::types::{BlockNumberOrHash, Bytes};

#[derive(Clone, Eq, PartialEq, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TraceParams {
	pub disable_storage: Option<bool>,
	pub disable_memory: Option<bool>,
	pub disable_stack: Option<bool>,
	/// Javascript tracer (we just check if it's Blockscout tracer string)
	pub tracer: Option<String>,
	pub tracer_config: Option<single::TraceCallConfig>,
	pub timeout: Option<String>,
}

#[derive(Copy, Clone, Eq, PartialEq, Debug, Deserialize)]
#[serde(rename_all = "camelCase", untagged)]
pub enum RequestBlockId {
	Number(#[serde(deserialize_with = "deserialize_u32_0x")] u32),
	Hash(H256),
	Tag(RequestBlockTag),
}

#[derive(Copy, Clone, Eq, PartialEq, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RequestBlockTag {
	Earliest,
	Latest,
	Pending,
}

fn deserialize_u32_0x<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
	D: Deserializer<'de>,
{
	let buf = String::deserialize(deserializer)?;

	let parsed = match buf.strip_prefix("0x") {
		Some(buf) => u32::from_str_radix(&buf, 16),
		None => u32::from_str_radix(&buf, 10),
	};

	parsed.map_err(|e| Error::custom(format!("parsing error: {:?} from '{}'", e, buf)))
}



#[derive(Debug, Clone, Default, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TraceCallParams {
	/// Sender
	pub from: Option<H160>,
	/// Recipient
	pub to: H160,
	/// Gas Price, legacy.
	pub gas_price: Option<U256>,
	/// Max BaseFeePerGas the user is willing to pay.
	pub max_fee_per_gas: Option<U256>,
	/// The miner's tip.
	pub max_priority_fee_per_gas: Option<U256>,
	/// Gas
	pub gas: Option<U256>,
	/// Value of transaction in wei
	pub value: Option<U256>,
	/// Additional data sent with transaction
	pub data: Option<Bytes>,
	/// Nonce
	pub nonce: Option<U256>,
	/// EIP-2930 access list
	pub access_list: Option<Vec<AccessListItem>>,
	/// EIP-2718 type
	#[serde(rename = "type")]
	pub transaction_type: Option<U256>,
}

/// Net rpc interface.
#[rpc(server)]
#[async_trait]
pub trait DebugApi {
	/// Returns an RLP-encoded header with the given number or hash.
	#[method(name = "debug_getRawHeader")]
	async fn raw_header(&self, number: BlockNumberOrHash) -> RpcResult<Option<Bytes>>;

	/// Returns an RLP-encoded block with the given number or hash.
	#[method(name = "debug_getRawBlock")]
	async fn raw_block(&self, number: BlockNumberOrHash) -> RpcResult<Option<Bytes>>;

	/// Returns a EIP-2718 binary-encoded transaction with the given hash.
	#[method(name = "debug_getRawTransaction")]
	async fn raw_transaction(&self, hash: H256) -> RpcResult<Option<Bytes>>;

	/// Returns an array of EIP-2718 binary-encoded receipts with the given number of hash.
	#[method(name = "debug_getRawReceipts")]
	async fn raw_receipts(&self, number: BlockNumberOrHash) -> RpcResult<Vec<Bytes>>;

	/// Returns an array of recent bad blocks that the client has seen on the network.
	#[method(name = "debug_getBadBlocks")]
	fn bad_blocks(&self, number: BlockNumberOrHash) -> RpcResult<Vec<()>>;

	#[method(name = "debug_traceCall")]
	async fn trace_call(
		&self,
		call_params: TraceCallParams,
		id: RequestBlockId,
		params: Option<TraceParams>,
	) -> RpcResult<single::TransactionTrace>;
}
