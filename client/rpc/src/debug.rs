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

use std::{marker::PhantomData, sync::Arc};

use ethereum::EnvelopedEncodable;
use ethereum_types::H256;
use jsonrpsee::core::{async_trait, RpcResult};
use rlp::Encodable;
// Substrate
use sc_client_api::{backend::{Backend, StorageProvider}, BlockBackend};
use sp_api::ProvideRuntimeApi;
use sp_blockchain::HeaderBackend;
use sp_runtime::{
	generic::BlockId,
	traits::{Block as BlockT, Header as HeaderT, UniqueSaturatedInto},
};
// Frontier
use client_evm_tracing::{
	formatters::{self, ResponseFormatter},
	listeners::{call_list::Listener as CallListListener, raw::Listener as RawListener},
	types::single::{self, TraceType},
};
use fc_rpc_core::{types::*, DebugApiServer, RequestBlockId, RequestBlockTag, TraceParams};
use fc_storage::StorageOverride;
use fp_rpc::EthereumRuntimeRPCApi;
use rpc_primitives_debug::DebugRuntimeApi;

use crate::{cache::EthBlockDataCacheTask, frontier_backend_client, internal_err};

const RAW_TRACE_MAX_MEMORY_USAGE: usize = 32 * 1024 * 1024;

#[derive(Clone, Copy)]
enum TracerInput {
	None,
	Blockscout,
	CallTracer,
}

/// Debug API implementation.
pub struct Debug<B: BlockT, C, BE> {
	client: Arc<C>,
	backend: Arc<dyn fc_api::Backend<B>>,
	storage_override: Arc<dyn StorageOverride<B>>,
	block_data_cache: Arc<EthBlockDataCacheTask<B>>,
	_marker: PhantomData<BE>,
}

impl<B: BlockT, C, BE> Debug<B, C, BE> {
	pub fn new(
		client: Arc<C>,
		backend: Arc<dyn fc_api::Backend<B>>,
		storage_override: Arc<dyn StorageOverride<B>>,
		block_data_cache: Arc<EthBlockDataCacheTask<B>>,
	) -> Self {
		Self {
			client,
			backend,
			storage_override,
			block_data_cache,
			_marker: PhantomData,
		}
	}

	async fn block_by(&self, number: BlockNumberOrHash) -> RpcResult<Option<ethereum::BlockV2>>
	where
		C: HeaderBackend<B> + StorageProvider<B, BE> + 'static,
		BE: Backend<B>,
	{
		let id = match frontier_backend_client::native_block_id::<B, C>(
			self.client.as_ref(),
			self.backend.as_ref(),
			Some(number),
		)
		.await?
		{
			Some(id) => id,
			None => return Ok(None),
		};

		let substrate_hash = self
			.client
			.expect_block_hash_from_id(&id)
			.map_err(|_| internal_err(format!("Expect block number from id: {}", id)))?;
		let block = self.block_data_cache.current_block(substrate_hash).await;
		Ok(block)
	}

	async fn transaction_by(
		&self,
		transaction_hash: H256,
	) -> RpcResult<Option<ethereum::TransactionV2>>
	where
		C: HeaderBackend<B> + StorageProvider<B, BE> + 'static,
		BE: Backend<B>,
	{
		let (eth_block_hash, index) = match frontier_backend_client::load_transactions::<B, C>(
			self.client.as_ref(),
			self.backend.as_ref(),
			transaction_hash,
			true,
		)
		.await?
		{
			Some((hash, index)) => (hash, index as usize),
			None => return Ok(None),
		};

		let substrate_hash = match frontier_backend_client::load_hash::<B, C>(
			self.client.as_ref(),
			self.backend.as_ref(),
			eth_block_hash,
		)
		.await?
		{
			Some(hash) => hash,
			None => return Ok(None),
		};

		let block = self.block_data_cache.current_block(substrate_hash).await;
		if let Some(block) = block {
			Ok(Some(block.transactions[index].clone()))
		} else {
			Ok(None)
		}
	}

	async fn receipts_by(
		&self,
		number: BlockNumberOrHash,
	) -> RpcResult<Option<Vec<ethereum::ReceiptV3>>>
	where
		C: HeaderBackend<B> + StorageProvider<B, BE> + 'static,
		BE: Backend<B>,
	{
		let id = match frontier_backend_client::native_block_id::<B, C>(
			self.client.as_ref(),
			self.backend.as_ref(),
			Some(number),
		)
		.await?
		{
			Some(id) => id,
			None => return Ok(None),
		};

		let substrate_hash = self
			.client
			.expect_block_hash_from_id(&id)
			.map_err(|_| internal_err(format!("Expect block number from id: {}", id)))?;

		// TODO: use data cache in the future
		let receipts = self.storage_override.current_receipts(substrate_hash);
		Ok(receipts)
	}

	fn handle_params(
		params: Option<TraceParams>,
	) -> RpcResult<(TracerInput, TraceType, Option<single::TraceCallConfig>)> {
		match params {
			Some(TraceParams {
				tracer: Some(tracer),
				tracer_config,
				..
			}) => {
				if tracer == "callTracer" {
					Ok((TracerInput::CallTracer, TraceType::CallList, tracer_config))
				} else if tracer == "blockscout" {
					Ok((TracerInput::Blockscout, TraceType::CallList, tracer_config))
				} else {
					Err(internal_err(format!("unsupported tracer: {tracer}")))
				}
			}
			Some(params) => Ok((
				TracerInput::None,
				TraceType::Raw {
					disable_storage: params.disable_storage.unwrap_or(false),
					disable_memory: params.disable_memory.unwrap_or(false),
					disable_stack: params.disable_stack.unwrap_or(false),
				},
				params.tracer_config,
			)),
			None => Ok((
				TracerInput::None,
				TraceType::Raw {
					disable_storage: false,
					disable_memory: false,
					disable_stack: false,
				},
				None,
			)),
		}
	}
}

#[async_trait]
impl<B, C, BE> DebugApiServer for Debug<B, C, BE>
where
	B: BlockT,
	C: ProvideRuntimeApi<B>,
	C::Api: EthereumRuntimeRPCApi<B> + DebugRuntimeApi<B>,
	C: BlockBackend<B> + HeaderBackend<B> + StorageProvider<B, BE> + 'static,
	BE: Backend<B> + 'static,
{
	async fn raw_header(&self, number: BlockNumberOrHash) -> RpcResult<Option<Bytes>> {
		let block = self.block_by(number).await?;
		Ok(block.map(|block| Bytes::new(block.header.rlp_bytes().to_vec())))
	}

	async fn raw_block(&self, number: BlockNumberOrHash) -> RpcResult<Option<Bytes>> {
		let block = self.block_by(number).await?;
		Ok(block.map(|block| Bytes::new(block.rlp_bytes().to_vec())))
	}

	async fn raw_transaction(&self, hash: H256) -> RpcResult<Option<Bytes>> {
		let transaction = self.transaction_by(hash).await?;
		Ok(transaction.map(|transaction| Bytes::new(transaction.encode().to_vec())))
	}

	async fn raw_receipts(&self, number: BlockNumberOrHash) -> RpcResult<Vec<Bytes>> {
		let receipts = self.receipts_by(number).await?.unwrap_or_default();
		Ok(receipts
			.into_iter()
			.map(|receipt| Bytes::new(receipt.encode().to_vec()))
			.collect::<Vec<_>>())
	}

	fn bad_blocks(&self, _number: BlockNumberOrHash) -> RpcResult<Vec<()>> {
		// `debug_getBadBlocks` wouldn't really be useful in a Substrate context.
		// The rationale for that is for debugging multi-client consensus issues, which we'll never face
		// (we may have multiple clients in the future, but for runtime it's only "multi-wasm-runtime", never "multi-EVM").
		// We can simply return empty array for this API.
		Ok(vec![])
	}

	async fn trace_transaction(
		&self,
		hash: H256,
		params: Option<TraceParams>,
	) -> RpcResult<single::TransactionTrace> {
		let (tracer_input, trace_type, tracer_config) = Self::handle_params(params)?;

		let transaction = self
			.transaction_by(hash)
			.await?
			.ok_or_else(|| internal_err("Transaction hash not found"))?;

		let (eth_block_hash, _) = frontier_backend_client::load_transactions::<B, C>(
			self.client.as_ref(),
			self.backend.as_ref(),
			hash,
			true,
		)
		.await?
		.ok_or_else(|| internal_err("Transaction hash not found"))?;

		let reference_hash = frontier_backend_client::load_hash::<B, C>(
			self.client.as_ref(),
			self.backend.as_ref(),
			eth_block_hash,
		)
		.await?
		.ok_or_else(|| internal_err("Block hash not found"))?;

		let header = self
			.client
			.header(reference_hash)
			.map_err(|e| internal_err(format!("Failed to read header: {e:?}")))?
			.ok_or_else(|| internal_err("Block header not found"))?;
		let parent_block_hash = *header.parent_hash();

		let extrinsics = self
			.client
			.block_body(reference_hash)
			.map_err(|e| internal_err(format!("Failed to read block body: {e:?}")))?
			.unwrap_or_default();

		let mut api = self.client.runtime_api();
		let execute = |api: &mut C::Api| -> RpcResult<()> {
			api.trace_transaction(
				parent_block_hash,
				extrinsics.clone(),
				&transaction,
				&header,
			)
			.map_err(|e| internal_err(format!("Runtime api access error: {e:?}")))?
			.map_err(|e| internal_err(format!("DispatchError: {e:?}")))?;

			Ok(())
		};

		match trace_type {
			TraceType::Raw {
				disable_storage,
				disable_memory,
				disable_stack,
			} => {
				let mut proxy = RawListener::new(
					disable_storage,
					disable_memory,
					disable_stack,
					RAW_TRACE_MAX_MEMORY_USAGE,
				);
				proxy.using(|| execute(&mut api))?;

				formatters::Raw::format(proxy).ok_or_else(|| {
					internal_err(
						"replayed transaction generated too much data; try disabling memory or storage",
					)
				})
			}
			TraceType::CallList => {
				let mut proxy = CallListListener::default();
				proxy.with_log = tracer_config.map_or(false, |cfg| cfg.with_log);
				proxy.using(|| execute(&mut api))?;
				proxy.finish_transaction();

				match tracer_input {
					TracerInput::CallTracer => {
						let mut traces = formatters::CallTracer::format(proxy)
							.ok_or_else(|| internal_err("Trace result is empty"))?;
						traces
							.pop()
							.map(|trace| trace.result)
							.ok_or_else(|| internal_err("Trace result is empty"))
					}
					TracerInput::Blockscout | TracerInput::None => formatters::Blockscout::format(proxy)
						.ok_or_else(|| internal_err("Trace result is empty")),
				}
			}
			_ => Err(internal_err(
				"unsupported trace mode for debug_traceTransaction",
			)),
		}
	}

	async fn trace_block(
		&self,
		id: BlockNumberOrHash,
		params: Option<TraceParams>,
	) -> RpcResult<Vec<single::TransactionTrace>> {
		let (tracer_input, trace_type, tracer_config) = Self::handle_params(params)?;

		let block = self
			.block_by(id)
			.await?
			.ok_or_else(|| internal_err("Block not found"))?;
		let known_transactions = block
			.transactions
			.iter()
			.map(|transaction| transaction.hash())
			.collect::<Vec<_>>();

		let reference_hash = frontier_backend_client::load_hash::<B, C>(
			self.client.as_ref(),
			self.backend.as_ref(),
			block.header.hash(),
		)
		.await?
		.ok_or_else(|| internal_err("Block hash not found"))?;

		let header = self
			.client
			.header(reference_hash)
			.map_err(|e| internal_err(format!("Failed to read header: {e:?}")))?
			.ok_or_else(|| internal_err("Block header not found"))?;
		let parent_block_hash = *header.parent_hash();

		let extrinsics = self
			.client
			.block_body(reference_hash)
			.map_err(|e| internal_err(format!("Failed to read block body: {e:?}")))?
			.unwrap_or_default();

		let mut api = self.client.runtime_api();
		let execute = |api: &mut C::Api| -> RpcResult<()> {
			api.trace_block(
				parent_block_hash,
				extrinsics.clone(),
				known_transactions.clone(),
				&header,
			)
			.map_err(|e| internal_err(format!("Runtime api access error: {e:?}")))?
			.map_err(|e| internal_err(format!("DispatchError: {e:?}")))?;

			Ok(())
		};

		match trace_type {
			TraceType::CallList => {
				let mut proxy = CallListListener::default();
				proxy.with_log = tracer_config.map_or(false, |cfg| cfg.with_log);
				proxy.using(|| execute(&mut api))?;
				proxy.finish_transaction();

				match tracer_input {
					TracerInput::CallTracer => formatters::CallTracer::format(proxy)
						.map(|traces| {
							traces
								.into_iter()
								.map(|trace| trace.result)
								.collect::<Vec<_>>()
						})
						.ok_or_else(|| internal_err("Trace result is empty")),
					TracerInput::Blockscout | TracerInput::None => {
						let traces = std::mem::take(&mut proxy.entries)
							.into_iter()
							.filter(|entry| !entry.is_empty())
							.filter_map(|entry| {
								let mut listener = CallListListener::default();
								listener.entries.push(entry);
								formatters::Blockscout::format(listener)
							})
							.collect::<Vec<_>>();

						if traces.is_empty() {
							Err(internal_err("Trace result is empty"))
						} else {
							Ok(traces)
						}
					}
				}
			}
			_ => Err(internal_err(
				"debug_traceBlockByNumber only supports call list mode",
			)),
		}
	}

	async fn trace_call(
		&self,
		call_params: fc_rpc_core::TraceCallParams,
		id: RequestBlockId,
		params: Option<TraceParams>,
	) -> RpcResult<single::TransactionTrace> {
		let (tracer_input, trace_type, tracer_config) = Self::handle_params(params)?;

		let reference_id: BlockId<B> = match id {
			RequestBlockId::Number(n) => Ok(BlockId::Number(n.unique_saturated_into())),
			RequestBlockId::Tag(RequestBlockTag::Latest) => {
				Ok(BlockId::Number(self.client.info().best_number))
			}
			RequestBlockId::Tag(RequestBlockTag::Earliest) => {
				Ok(BlockId::Number(0u32.unique_saturated_into()))
			}
			RequestBlockId::Tag(RequestBlockTag::Pending) => {
				Err(internal_err("'pending' blocks are not supported"))
			}
			RequestBlockId::Hash(eth_hash) => {
				match frontier_backend_client::load_hash::<B, C>(
					self.client.as_ref(),
					self.backend.as_ref(),
					eth_hash,
				)
				.await
				{
					Ok(Some(hash)) => Ok(BlockId::Hash(hash)),
					Ok(None) => Err(internal_err("Block hash not found")),
					Err(e) => Err(e),
				}
			}
		}?;

		let mut api = self.client.runtime_api();

		let hash = self
			.client
			.expect_block_hash_from_id(&reference_id)
			.map_err(|_| internal_err("Block header not found"))?;
		let header = self
			.client
			.header(hash)
			.map_err(|e| internal_err(format!("Failed to read header: {e:?}")))?
			.ok_or_else(|| internal_err("Block header not found"))?;
		let parent_block_hash = *header.parent_hash();

		let fc_rpc_core::TraceCallParams {
			from,
			to,
			gas_price,
			max_fee_per_gas,
			max_priority_fee_per_gas,
			gas,
			value,
			data,
			nonce,
			access_list,
			..
		} = call_params;

		let (max_fee_per_gas, max_priority_fee_per_gas) =
			match (gas_price, max_fee_per_gas, max_priority_fee_per_gas) {
				(gas_price, None, None) => {
					let gas_price = if gas_price.unwrap_or_default().is_zero() {
						None
					} else {
						gas_price
					};
					(gas_price, gas_price)
				}
				(_, max_fee, max_priority) => {
					let max_fee = if max_fee.unwrap_or_default().is_zero() {
						None
					} else {
						max_fee
					};
					if let Some(max_priority) = max_priority {
						let max_fee = max_fee.unwrap_or_default();
						if max_priority > max_fee {
							return Err(internal_err(
								"Invalid input: `max_priority_fee_per_gas` greater than `max_fee_per_gas`",
							));
						}
					}
					(max_fee, max_priority)
				}
			};

		let gas_limit = match gas {
			Some(amount) => amount,
			None => {
				let block = api
					.current_block(parent_block_hash)
					.map_err(|e| internal_err(format!("Runtime api access error: {e:?}")))?
					.ok_or_else(|| {
						internal_err("block unavailable, cannot query gas limit")
					})?;
				block.header.gas_limit
			}
		};

		let data = data.map(|d| d.0).unwrap_or_default();
		let value = value.unwrap_or_default();
		let access_list = access_list
			.unwrap_or_default()
			.into_iter()
			.map(|item| (item.address, item.storage_keys))
			.collect::<Vec<_>>();

		let execute = |api: &mut C::Api| -> RpcResult<()> {
			api.trace_call(
				parent_block_hash,
				&header,
				from.unwrap_or_default(),
				to,
				data.clone(),
				value,
				gas_limit,
				max_fee_per_gas,
				max_priority_fee_per_gas,
				nonce,
				Some(access_list.clone()),
			)
			.map_err(|e| internal_err(format!("Runtime api access error: {e:?}")))?
			.map_err(|e| internal_err(format!("DispatchError: {e:?}")))?;

			Ok(())
		};

		match trace_type {
			TraceType::Raw {
				disable_storage,
				disable_memory,
				disable_stack,
			} => {
				let mut proxy = RawListener::new(
					disable_storage,
					disable_memory,
					disable_stack,
					RAW_TRACE_MAX_MEMORY_USAGE,
				);
				proxy.using(|| execute(&mut api))?;

				formatters::Raw::format(proxy).ok_or_else(|| {
					internal_err(
						"replayed transaction generated too much data; try disabling memory or storage",
					)
				})
			}
			TraceType::CallList => {
				let mut proxy = CallListListener::default();
				proxy.with_log = tracer_config.map_or(false, |cfg| cfg.with_log);
				proxy.using(|| execute(&mut api))?;
				proxy.finish_transaction();

				match tracer_input {
					TracerInput::CallTracer => {
						let mut traces = formatters::CallTracer::format(proxy)
							.ok_or_else(|| internal_err("Trace result is empty"))?;
						traces
							.pop()
							.map(|trace| trace.result)
							.ok_or_else(|| internal_err("Trace result is empty"))
					}
					TracerInput::Blockscout | TracerInput::None => formatters::Blockscout::format(proxy)
						.ok_or_else(|| internal_err("Trace result is empty")),
				}
			}
			_ => Err(internal_err("unsupported trace mode for debug_traceCall")),
		}
	}
}
