// Smoldot
// Copyright (C) 2019-2021  Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

//! Background runtime download service.
//!
//! This service plugs on top of a [`sync_service`], listens for new best blocks and checks
//! whether the runtime has changed in any way. Its objective is to always provide an up-to-date
//! [`executor::host::HostVmPrototype`] ready to be called by other services.
//!
//! # Usage
//!
//! The runtime service lets user subscribe to best and finalized block updates, similar to
//! the [`sync_service`]. These subscriptions are implemented by subscribing to the underlying
//! [`sync_service`] and, for each notification, downloading the runtime code of the best or
//! finalized block. Therefore, these notifications always come with a delay compared to directly
//! using the [`sync_service`].
//!
//! Furthermore, if it isn't possible to download the runtime code of a block (for example because
//! peers refuse to answer or have already pruned the block) or if the runtime service already has
//! too many pending downloads, this block is simply skipped and not reported on the
//! subscriptions.
//!
//! Consequently, you are strongly encouraged to not use both the [`sync_service`] *and* the
//! [`RuntimeService`] of the same chain. They each provide a consistent view of the chain, but
//! this view isn't necessarily the same on both services.
//!
//! The main service offered by the runtime service is
//! [`RuntimeService::recent_best_block_runtime_call`], that performs a runtime call on the latest
//! reported best block or more recent.

// TODO: the doc above mentions that you can subscribe to the finalized block, but this is isn't implemented yet ^

use crate::{ffi, lossy_channel, sync_service};

use futures::{lock::Mutex, prelude::*};
use smoldot::{chain_spec, executor, header, metadata, network::protocol, trie::proof_verify};
use std::{iter, pin::Pin, sync::Arc, time::Duration};

pub use crate::lossy_channel::Receiver as NotificationsReceiver;

/// Configuration for a runtime service.
pub struct Config<'a> {
    /// Closure that spawns background tasks.
    pub tasks_executor: Box<dyn FnMut(String, Pin<Box<dyn Future<Output = ()> + Send>>) + Send>,

    /// Service responsible for synchronizing the chain.
    pub sync_service: Arc<sync_service::SyncService>,

    /// Specifications of the chain.
    pub chain_spec: &'a chain_spec::ChainSpec,

    /// Hash of the genesis block of the chain.
    ///
    /// > **Note**: This can be derived from a [`chain_spec::ChainSpec`]. While the
    /// >           [`RuntimeService::new`] function could in theory use the
    /// >           [`Config::chain_spec`] parameter to derive this value, doing so is quite
    /// >           expensive. We prefer to require this value from the upper layer instead, as
    /// >           it is most likely needed anyway.
    pub genesis_block_hash: [u8; 32],

    /// Hash of the storage trie root of the genesis block of the chain.
    ///
    /// > **Note**: This can be derived from a [`chain_spec::ChainSpec`]. While the
    /// >           [`RuntimeService::new`] function could in theory use the
    /// >           [`Config::chain_spec`] parameter to derive this value, doing so is quite
    /// >           expensive. We prefer to require this value from the upper layer instead.
    pub genesis_block_state_root: [u8; 32],
}

/// See [the module-level documentation](..).
pub struct RuntimeService {
    /// See [`Config::tasks_executor`].
    tasks_executor: Mutex<Box<dyn FnMut(String, Pin<Box<dyn Future<Output = ()> + Send>>) + Send>>,

    /// See [`Config::sync_service`].
    sync_service: Arc<sync_service::SyncService>,

    /// Initially contains the runtime code of the genesis block. Whenever a best block is
    /// received, updated with the runtime of this new best block.
    /// If, after a new best block, it isn't possible to determine whether the runtime has changed,
    /// the content will be left unchanged. However, if an error happens for example when compiling
    /// the new runtime, then the content will contain an error.
    latest_known_runtime: Mutex<LatestKnownRuntime>,
}

impl RuntimeService {
    /// Initializes a new runtime service.
    ///
    /// The future returned by this function is expected to finish relatively quickly and is
    /// necessary only for locking purposes.
    pub async fn new(config: Config<'_>) -> Arc<Self> {
        // Build the runtime of the genesis block.
        let latest_known_runtime = {
            let code = config
                .chain_spec
                .genesis_storage()
                .find(|(k, _)| k == b":code")
                .map(|(_, v)| v.to_vec());
            let heap_pages = config
                .chain_spec
                .genesis_storage()
                .find(|(k, _)| k == b":heappages")
                .map(|(_, v)| v.to_vec());

            // Note that in the absolute we don't need to panic in case of a problem, and could
            // simply store an `Err` and continue running.
            // However, in practice, it seems more sane to detect problems in the genesis block.
            let mut runtime = SuccessfulRuntime::from_params(&code, &heap_pages)
                .expect("invalid runtime at genesis block");

            // As documented in the `metadata` field, we must fill it using the genesis storage.
            let mut query = metadata::query_metadata(runtime.virtual_machine.take().unwrap());
            loop {
                match query {
                    metadata::Query::Finished(Ok((metadata, vm))) => {
                        runtime.virtual_machine = Some(vm);
                        runtime.metadata = Some(metadata);
                        break;
                    }
                    metadata::Query::StorageGet(get) => {
                        let key = get.key_as_vec();
                        let value = config
                            .chain_spec
                            .genesis_storage()
                            .find(|(k, _)| &**k == key)
                            .map(|(_, v)| v);
                        query = get.inject_value(value.map(iter::once));
                    }
                    metadata::Query::Finished(Err(err)) => {
                        panic!("Unable to generate genesis metadata: {}", err)
                    }
                }
            }

            LatestKnownRuntime {
                runtime: Ok(runtime),
                runtime_code: code,
                heap_pages,
                runtime_block_hash: config.genesis_block_hash,
                runtime_block_height: 0,
                runtime_block_state_root: config.genesis_block_state_root,
                runtime_version_subscriptions: Vec::new(),
                best_blocks_subscriptions: Vec::new(),
                best_near_head_of_chain: config
                    .sync_service
                    .is_near_head_of_chain_heuristic()
                    .await,
            }
        };

        let runtime_service = Arc::new(RuntimeService {
            tasks_executor: Mutex::new(config.tasks_executor),
            sync_service: config.sync_service,
            latest_known_runtime: Mutex::new(latest_known_runtime),
        });

        // Spawns a task that downloads the runtime code at every block to check whether it has
        // changed.
        //
        // This is strictly speaking not necessary as long as there is no active subscription.
        // However, in practice, there is most likely always going to be one. It is way easier to
        // always have a task active rather than create and destroy it.
        start_background_task(&runtime_service).await;

        runtime_service
    }

    /// Returns the current runtime version, plus an unlimited stream that produces one item every
    /// time the specs of the runtime of the best block are changed.
    ///
    /// The stream can generate an `Err(())` if the runtime in the best block is invalid.
    pub async fn subscribe_runtime_version(
        self: &Arc<RuntimeService>,
    ) -> (
        Result<executor::CoreVersion, ()>,
        NotificationsReceiver<Result<executor::CoreVersion, ()>>,
    ) {
        let (tx, rx) = lossy_channel::channel();
        let mut latest_known_runtime = self.latest_known_runtime.lock().await;
        latest_known_runtime.runtime_version_subscriptions.push(tx);
        let current_version = latest_known_runtime
            .runtime
            .as_ref()
            .map(|r| r.runtime_spec.clone())
            .map_err(|&()| ());
        (current_version, rx)
    }

    /// Returns the runtime version of the block with the given hash.
    // TODO: better error type
    pub async fn runtime_version_of_block(
        self: &Arc<RuntimeService>,
        block_hash: &[u8; 32],
    ) -> Result<executor::CoreVersion, ()> {
        // If the requested block is the best known block, optimize by
        // immediately returning the cached spec.
        {
            let latest_known_runtime = self.latest_known_runtime.lock().await;
            if latest_known_runtime.runtime_block_hash == *block_hash {
                return latest_known_runtime
                    .runtime
                    .as_ref()
                    .map(|r| r.runtime_spec.clone())
                    .map_err(|&()| ());
            }
        }

        // Ask the network for the header of this block, as we need to know the state root.
        let state_root = {
            let result = self
                .sync_service
                .clone()
                .block_query(
                    *block_hash,
                    protocol::BlocksRequestFields {
                        header: true,
                        body: false,
                        justification: false,
                    },
                )
                .await;

            // Note that the `block_query` method guarantees that the header is present
            // and valid.
            let header = if let Ok(block) = result {
                block.header.unwrap()
            } else {
                return Err(());
            };

            *header::decode(&header).map_err(|_| ())?.state_root
        };

        // Download the runtime code of this block.
        let code_query_result = self
            .sync_service
            .clone()
            .storage_query(
                block_hash,
                &state_root,
                iter::once(&b":code"[..]).chain(iter::once(&b":heappages"[..])),
            )
            .await;

        let (code, heap_pages) = {
            let mut results = match code_query_result {
                Ok(c) => c,
                Err(_) => return Err(()),
            };

            let heap_pages = results.pop().unwrap();
            let code = results.pop().unwrap();
            (code, heap_pages)
        };

        SuccessfulRuntime::from_params(&code, &heap_pages).map(|r| r.runtime_spec)
    }

    /// Returns the runtime version of the current best block.
    pub async fn best_block_runtime(
        self: &Arc<RuntimeService>,
    ) -> Result<executor::CoreVersion, ()> {
        let latest_known_runtime = self.latest_known_runtime.lock().await;
        latest_known_runtime
            .runtime
            .as_ref()
            .map(|r| r.runtime_spec.clone())
            .map_err(|&()| ())
    }

    /// Returns the SCALE-encoded header of the current best block, plus an unlimited stream that
    /// produces one item every time the best block is changed.
    ///
    /// This function is similar to [`sync_service::SyncService::subscribe_best`], except that
    /// it is called less often. Additionally, it is guaranteed that when a notification is sent
    /// out, calling [`RuntimeService::recent_best_block_runtime_call`] will operate on this
    /// block or more recent. In other words, if you call
    /// [`RuntimeService::recent_best_block_runtime_call`] and the stream of notifications is
    /// empty, you are guaranteed that the call has been performed on the best block.
    pub async fn subscribe_best(
        self: &Arc<RuntimeService>,
    ) -> (Vec<u8>, NotificationsReceiver<Vec<u8>>) {
        let (tx, rx) = lossy_channel::channel();
        let mut latest_known_runtime = self.latest_known_runtime.lock().await;
        latest_known_runtime.best_blocks_subscriptions.push(tx);
        drop(latest_known_runtime);
        let (current, _) = self.sync_service.subscribe_best().await; // TODO: not correct; should load from latest_known_runtime
        (current, rx)
    }

    /// Performs a runtime call using the best block, or a recent best block.
    ///
    /// The [`RuntimeService`] maintains the code of the runtime of a recent best block locally,
    /// but doesn't know anything about the storage, which the runtime might have to access. In
    /// order to make this work, a "call proof" is performed on the network in order to obtain
    /// the storage values corresponding to this call.
    pub async fn recent_best_block_runtime_call<'a>(
        self: &'a Arc<RuntimeService>,
        method: &str,
        parameter_vectored: impl Iterator<Item = impl AsRef<[u8]>> + Clone,
    ) -> Result<Vec<u8>, RuntimeCallError> {
        self.recent_best_block_runtime_call_inner(method, parameter_vectored)
            .await
            .map(|(ret, _)| ret)
    }

    /// See [`RuntimeService::recent_best_block_runtime_call`].
    ///
    /// The latest known runtime might be updated during the execution of this function. If you
    /// call this function, then re-lock the latest known runtime afterwards, you might not find
    /// the same runtime as the one that has actually performed the call. To solve that, in
    /// addition to the value generated by the runtime call, also returns a lock to the latest
    /// known runtime. This can allow inspecting the runtime that has been used in order to
    /// perform the call.
    async fn recent_best_block_runtime_call_inner<'a>(
        self: &'a Arc<RuntimeService>,
        method: &str,
        parameter_vectored: impl Iterator<Item = impl AsRef<[u8]>> + Clone,
    ) -> Result<(Vec<u8>, futures::lock::MutexGuard<'a, LatestKnownRuntime>), RuntimeCallError>
    {
        // `latest_known_runtime` should be kept locked as little as possible.
        // In order to handle the possibility a runtime upgrade happening during the operation,
        // every time `latest_known_runtime` is locked, we compare the runtime version stored in
        // it with the value previously found. If there is a mismatch, the entire runtime call
        // is restarted from scratch.
        loop {
            // Get `runtime_block_hash`, `runtime_block_height` and `runtime_block_state_root`,
            // the hash, height, and state trie root of a recent best block that uses this runtime.
            let (spec_version, runtime_block_hash, runtime_block_height, runtime_block_state_root) = {
                let lock = self.latest_known_runtime.lock().await;
                (
                    lock.runtime
                        .as_ref()
                        .map_err(|()| RuntimeCallError::InvalidRuntime)?
                        .runtime_spec
                        .decode()
                        .spec_version,
                    lock.runtime_block_hash,
                    lock.runtime_block_height,
                    lock.runtime_block_state_root,
                )
            };

            // Perform the call proof request.
            // Note that `latest_known_runtime` is not locked.
            // If the call proof fail, do as if the proof was empty. This will enable the
            // fallback consisting in performing individual storage proof requests.
            let call_proof = self
                .sync_service
                .clone()
                .call_proof_query(
                    runtime_block_height,
                    protocol::CallProofRequestConfig {
                        block_hash: runtime_block_hash,
                        method,
                        parameter_vectored: parameter_vectored.clone(),
                    },
                )
                .await
                .unwrap_or(Vec::new());

            // Lock `latest_known_runtime_lock` again. `continue` if the runtime has changed
            // in-between.
            let mut latest_known_runtime_lock = self.latest_known_runtime.lock().await;
            let runtime = latest_known_runtime_lock
                .runtime
                .as_mut()
                .map_err(|()| RuntimeCallError::InvalidRuntime)?;
            if runtime.runtime_spec.decode().spec_version != spec_version {
                continue;
            }

            // Perform the actual runtime call locally.
            let mut runtime_call = match executor::read_only_runtime_host::run(
                executor::read_only_runtime_host::Config {
                    virtual_machine: runtime.virtual_machine.take().unwrap(),
                    function_to_call: method,
                    parameter: parameter_vectored,
                },
            ) {
                Ok(vm) => vm,
                Err((err, prototype)) => {
                    runtime.virtual_machine = Some(prototype);
                    return Err(RuntimeCallError::StartError(err));
                }
            };

            loop {
                match runtime_call {
                    executor::read_only_runtime_host::RuntimeHostVm::Finished(Ok(success)) => {
                        if !success.logs.is_empty() {
                            log::debug!(
                                target: "runtime",
                                "Runtime logs: {}",
                                success.logs
                            );
                        }

                        let return_value = success.virtual_machine.value().as_ref().to_owned();
                        runtime.virtual_machine = Some(success.virtual_machine.into_prototype());
                        return Ok((return_value, latest_known_runtime_lock));
                    }
                    executor::read_only_runtime_host::RuntimeHostVm::Finished(Err(error)) => {
                        runtime.virtual_machine = Some(error.prototype);
                        return Err(RuntimeCallError::CallError(error.detail));
                    }
                    executor::read_only_runtime_host::RuntimeHostVm::StorageGet(get) => {
                        let requested_key = get.key_as_vec(); // TODO: optimization: don't use as_vec
                        let storage_value =
                            match proof_verify::verify_proof(proof_verify::VerifyProofConfig {
                                requested_key: &requested_key,
                                trie_root_hash: &runtime_block_state_root,
                                proof: call_proof.iter().map(|v| &v[..]),
                            }) {
                                Ok(v) => v,
                                Err(err) => {
                                    // TODO: shouldn't return if error but do a storage_proof instead
                                    runtime.virtual_machine = Some(
                                    executor::read_only_runtime_host::RuntimeHostVm::StorageGet(
                                        get,
                                    )
                                    .into_prototype(),
                                );
                                    return Err(RuntimeCallError::StorageRetrieval(err));
                                }
                            };
                        runtime_call = get.inject_value(storage_value.as_ref().map(iter::once));
                    }
                    executor::read_only_runtime_host::RuntimeHostVm::NextKey(_) => {
                        todo!() // TODO:
                    }
                    executor::read_only_runtime_host::RuntimeHostVm::StorageRoot(storage_root) => {
                        runtime_call = storage_root.resume(&runtime_block_state_root);
                    }
                }
            }
        }
    }

    /// Obtain the metadata of the runtime of the current best block.
    ///
    /// > **Note**: Keep in mind that this function is subject to race conditions. The runtime
    /// >           of the best block can change at any time. This method should ideally be called
    /// >           again after every runtime change.
    pub async fn metadata(self: Arc<RuntimeService>) -> Result<Vec<u8>, MetadataError> {
        // First, try the cache.
        {
            let latest_known_runtime_lock = self.latest_known_runtime.lock().await;
            if let Ok(runtime) = latest_known_runtime_lock.runtime.as_ref() {
                if let Some(metadata) = runtime.metadata.as_ref() {
                    return Ok(metadata.clone());
                }
            } else {
                return Err(MetadataError::InvalidRuntime);
            }
        }

        // TODO: duplicated code compared to smoldot's metadata module
        match self
            .recent_best_block_runtime_call_inner("Metadata_metadata", iter::empty::<Vec<u8>>())
            .await
        {
            Ok((return_value, mut latest_known_runtime_lock)) => {
                match metadata::remove_metadata_length_prefix(&return_value) {
                    Ok(metadata) => {
                        // TODO: lot of cloning
                        latest_known_runtime_lock.runtime.as_mut().unwrap().metadata =
                            Some(metadata.to_vec());
                        Ok(metadata.to_vec())
                    }
                    Err(error) => {
                        log::warn!(
                            target: "runtime",
                            "Failed to call Metadata_metadata on runtime: {}",
                            error
                        );
                        Err(MetadataError::MetadataDecode(error))
                    }
                }
            }
            Err(error) => {
                log::warn!(
                    target: "runtime",
                    "Failed to call Metadata_metadata on runtime: {}",
                    error
                );
                Err(MetadataError::CallError(error))
            }
        }
    }

    /// Returns true if it is believed that we are near the head of the chain.
    ///
    /// The way this method is implemented is opaque and cannot be relied on. The return value
    /// should only ever be shown to the user and not used for any meaningful logic.
    pub async fn is_near_head_of_chain_heuristic(&self) -> bool {
        // The runtime service adds a delay between the moment a best block is reported by the
        // sync service and the moment it is reported by the runtime service.
        // Because of this, any "far from head of chain" to "near head of chain" transition
        // must take that delay into account. The other way around ("near" to "far") is
        // unaffected.

        // If the sync service is far from the head, the runtime service is also far.
        if !self.sync_service.is_near_head_of_chain_heuristic().await {
            return false;
        }

        // If the sync service is near, report the result of `is_near_head_of_chain_heuristic()`
        // when called at the latest best block that the runtime service reported through its API,
        // to make sure that we don't report "near" while having reported only blocks that were
        // far.
        self.latest_known_runtime
            .lock()
            .await
            .best_near_head_of_chain
    }
}

/// Error that can happen when calling a runtime function.
#[derive(Debug, derive_more::Display)]
pub enum RuntimeCallError {
    /// Error during the runtime call.
    #[display(fmt = "{}", _0)]
    CallError(executor::read_only_runtime_host::ErrorDetail),
    /// Error initializing the runtime call.
    #[display(fmt = "{}", _0)]
    StartError(executor::host::StartErr),
    /// Runtime of the best block isn't valid.
    #[display(fmt = "Runtime of the best block isn't valid")]
    InvalidRuntime,
    /// Error while retrieving the storage item from other nodes.
    // TODO: change error type?
    #[display(fmt = "{}", _0)]
    StorageRetrieval(proof_verify::Error),
}

impl RuntimeCallError {
    /// Returns `true` if this is caused by networking issues, as opposed to a consensus-related
    /// issue.
    pub fn is_network_problem(&self) -> bool {
        match self {
            RuntimeCallError::CallError(_) => false,
            RuntimeCallError::StartError(_) => false,
            RuntimeCallError::InvalidRuntime => false,
            // TODO: as a temporary hack, we consider `TrieRootNotFound` as the remote not knowing about the requested block; see https://github.com/paritytech/substrate/pull/8046
            RuntimeCallError::StorageRetrieval(proof_verify::Error::TrieRootNotFound) => true,
            RuntimeCallError::StorageRetrieval(_) => false,
        }
    }
}

/// Error that can happen when calling [`RuntimeService::metadata`].
#[derive(Debug, derive_more::Display)]
pub enum MetadataError {
    /// Error during the runtime call.
    #[display(fmt = "{}", _0)]
    CallError(RuntimeCallError),
    /// Runtime of the best block isn't valid.
    #[display(fmt = "Runtime of the best block isn't valid")]
    InvalidRuntime,
    /// Error while decoding metadata fetched from runtime.
    #[display(fmt = "{}", _0)]
    MetadataDecode(metadata::RemoveMetadataLengthPrefixError),
}

struct LatestKnownRuntime {
    /// Successfully-compiled runtime and all its information. Can contain an error if an error
    /// happened, including a problem when obtaining the runtime specs or the metadata. It is
    /// better to report to the user an error about for example the metadata not being extractable
    /// compared to returning an obsolete version.
    runtime: Result<SuccessfulRuntime, ()>,

    /// Undecoded storage value of `:code` corresponding to the [`LatestKnownRuntime::runtime`]
    /// field.
    runtime_code: Option<Vec<u8>>,
    /// Undecoded storage value of `:heappages` corresponding to the
    /// [`LatestKnownRuntime::runtime`] field.
    heap_pages: Option<Vec<u8>>,
    /// Hash of a block known to have the runtime found in the [`LatestKnownRuntime::runtime`]
    /// field. Always updated to a recent block having this runtime.
    runtime_block_hash: [u8; 32],
    /// Height of the block whose hash is [`LatestKnownRuntime::runtime_block_hash`].
    runtime_block_height: u64,
    /// Storage trie root of the block whose hash is [`LatestKnownRuntime::runtime_block_hash`].
    runtime_block_state_root: [u8; 32],

    /// List of senders that get notified when the runtime specs of the best block changes.
    /// Whenever [`LatestKnownRuntime::runtime`] is updated, one should emit an item on each
    /// sender.
    /// See [`RuntimeService::subscribe_runtime_version`].
    runtime_version_subscriptions: Vec<lossy_channel::Sender<Result<executor::CoreVersion, ()>>>,

    /// List of senders that get notified when the best block is updated.
    /// See [`RuntimeService::subscribe_best`].
    best_blocks_subscriptions: Vec<lossy_channel::Sender<Vec<u8>>>,

    /// Return value of calling [`sync_service::SyncService::is_near_head_of_chain_heuristic`]
    /// after the latest best block update.
    best_near_head_of_chain: bool,
}

struct SuccessfulRuntime {
    /// Cache of the metadata extracted from the runtime. `None` if unknown.
    ///
    /// This cache is filled lazily whenever it is requested through the public API.
    ///
    /// Note that building the metadata might require access to the storage, just like obtaining
    /// the runtime code. if the runtime code gets an update, we can reasonably assume that the
    /// network is able to serve us the storage of recent blocks, and thus the changes of being
    /// able to build the metadata are very high.
    ///
    /// If the runtime is the one found in the genesis storage, the metadata must have been been
    /// filled using the genesis storage as well. If we build the metadata of the genesis runtime
    /// lazily, chances are that the network wouldn't be able to serve the storage of blocks near
    /// the genesis.
    ///
    /// As documented in the smoldot metadata module, the metadata might access the storage, but
    /// we intentionally don't watch for changes in these storage keys to refresh the metadata.
    metadata: Option<Vec<u8>>,

    /// Runtime specs extracted from the runtime.
    runtime_spec: executor::CoreVersion,

    /// Virtual machine itself, to perform additional calls.
    ///
    /// Always `Some`, except for temporary extractions. Should always be `Some`, when the
    /// [`SuccessfulRuntime`] is accessed.
    virtual_machine: Option<executor::host::HostVmPrototype>,
}

impl SuccessfulRuntime {
    fn from_params(code: &Option<Vec<u8>>, heap_pages: &Option<Vec<u8>>) -> Result<Self, ()> {
        let vm = match executor::host::HostVmPrototype::new(
            code.as_ref().ok_or(())?,
            executor::storage_heap_pages_to_value(heap_pages.as_deref()).map_err(|_| ())?,
            executor::vm::ExecHint::CompileAheadOfTime,
        ) {
            Ok(vm) => vm,
            Err(error) => {
                log::warn!(target: "runtime", "Failed to compile best block runtime: {}", error);
                return Err(());
            }
        };

        let (runtime_spec, vm) = match executor::core_version(vm) {
            Ok(v) => v,
            Err(_error) => {
                log::warn!(
                    target: "runtime",
                    "Failed to call Core_version on new runtime",  // TODO: print error message as well ; at the moment the type of the error is `()`
                );
                return Err(());
            }
        };

        Ok(SuccessfulRuntime {
            metadata: None,
            runtime_spec,
            virtual_machine: Some(vm),
        })
    }
}

/// Starts the background task that updates the [`LatestKnownRuntime`].
async fn start_background_task(runtime_service: &Arc<RuntimeService>) {
    (runtime_service.tasks_executor.lock().await)("runtime-download".into(), {
        let runtime_service = runtime_service.clone();
        let blocks_stream = {
            let (best_block_header, best_blocks_subscription) =
                runtime_service.sync_service.subscribe_best().await;
            stream::once(future::ready(best_block_header)).chain(best_blocks_subscription)
        };

        // Set to `true` when we expect the runtime in `latest_known_runtime` to match the runtime
        // of the best block. Initially `false`, as `latest_known_runtime` uses the genesis
        // runtime.
        let mut runtime_matches_best_block = false;

        Box::pin(async move {
            futures::pin_mut!(blocks_stream);

            loop {
                // While major-syncing a chain, best blocks are updated continously. In that
                // situation, the delay below is too short to prevent the runtime code from being
                // continuously downloaded.
                // To avoid using too much bandwidth, we force another delay between two runtime
                // code downloads.
                // This delay is done at the beginning of the loop because the runtime is built
                // as part of the initialization of the `RuntimeService`, and in order to make it
                // possible to use `continue` without accidentally skipping this delay.
                ffi::Delay::new(Duration::from_secs(3)).await;

                // Wait until a new best block is known.
                let mut new_best_block = match blocks_stream.next().await {
                    Some(b) => b,
                    None => break, // Stream is finished.
                };

                // While the chain is running, it is often the case that more than one blocks
                // is generated and announced roughly at the same time.
                // We would like to avoid a situation where we receive a new best block, start
                // downloading the runtime code, then a few milliseconds later receive another
                // block that becomes the new best, and download the runtime code of that new
                // block as well. This would lead to downloading the runtime code twice (or more,
                // if more than two blocks are received) in a small time frame, which is usually a
                // waste of bandwidth.
                // Instead, whenever a new best block is received, we wait a little bit before
                // downloading the runtime, in order to see if there isn't any other new best
                // block already on the way.
                // This delay needs to be long enough to de-duplicate forks, but it should still
                // be small, as it adds artifical latency to the detecting runtime upgrades.
                ffi::Delay::new(Duration::from_millis(500)).await;
                while let Some(best_update) = blocks_stream.next().now_or_never() {
                    new_best_block = match best_update {
                        Some(b) => b,
                        None => break, // Stream is finished.
                    };
                }

                // Download the runtime code of this new best block.
                let new_best_block_decoded = header::decode(&new_best_block).unwrap();
                let new_best_block_hash = header::hash_from_scale_encoded_header(&new_best_block);
                let code_query_result = runtime_service
                    .sync_service
                    .clone()
                    .storage_query(
                        &new_best_block_hash,
                        new_best_block_decoded.state_root,
                        iter::once(&b":code"[..]).chain(iter::once(&b":heappages"[..])),
                    )
                    .await;

                let best_near_head_of_chain = runtime_service
                    .sync_service
                    .is_near_head_of_chain_heuristic()
                    .await;

                // Only lock `latest_known_runtime` now that everything is synchronous.
                let mut latest_known_runtime = runtime_service.latest_known_runtime.lock().await;
                let latest_known_runtime = &mut *latest_known_runtime;

                // Whatever the result of `code_query_result` is, notify the best block
                // subscriptions. After this, we shouldn't unlock `latest_known_runtime` ever
                // again to avoid giving the possibility to inspect the runtime in response
                // to the notification.

                // Elements in `best_blocks_subscriptions` are removed one by one and inserted
                // back if the channel is still open.
                for index in (0..latest_known_runtime.best_blocks_subscriptions.len()).rev() {
                    let mut subscription = latest_known_runtime
                        .best_blocks_subscriptions
                        .swap_remove(index);
                    if subscription.send(new_best_block.clone()).is_ok() {
                        latest_known_runtime
                            .best_blocks_subscriptions
                            .push(subscription);
                    }
                }

                latest_known_runtime
                    .best_blocks_subscriptions
                    .shrink_to_fit();

                latest_known_runtime.best_near_head_of_chain = best_near_head_of_chain;

                let (new_code, new_heap_pages) = {
                    let mut results = match code_query_result {
                        Ok(c) => c,
                        Err(error) => {
                            log::log!(
                                target: "runtime",
                                if error.is_network_problem() { log::Level::Debug } else { log::Level::Warn },
                                "Failed to download :code and :heappages of new best block: {}",
                                error
                            );
                            continue;
                        }
                    };

                    let new_heap_pages = results.pop().unwrap();
                    let new_code = results.pop().unwrap();
                    (new_code, new_heap_pages)
                };

                // `runtime_block_hash` is always updated in order to have the most recent
                // block possible.
                latest_known_runtime.runtime_block_hash = new_best_block_hash;
                latest_known_runtime.runtime_block_height = new_best_block_decoded.number;
                latest_known_runtime.runtime_block_state_root = *new_best_block_decoded.state_root;

                // `continue` if there wasn't any change in `:code` and `:heappages`.
                if new_code == latest_known_runtime.runtime_code
                    && new_heap_pages == latest_known_runtime.heap_pages
                {
                    runtime_matches_best_block = true;
                    continue;
                }

                // Don't notify the user of an upgrade if we didn't expect the runtime to match
                // the best block in the first place.
                if runtime_matches_best_block {
                    log::info!(
                        target: "runtime",
                        "New runtime code detected around block #{} (block number might be wrong)",
                        new_best_block_decoded.number
                    );
                }

                runtime_matches_best_block = true;
                latest_known_runtime.runtime_code = new_code;
                latest_known_runtime.heap_pages = new_heap_pages;
                latest_known_runtime.runtime = SuccessfulRuntime::from_params(
                    &latest_known_runtime.runtime_code,
                    &latest_known_runtime.heap_pages,
                );

                // Elements in `runtime_version_subscriptions` are removed one by one and inserted
                // back if the channel is still open.
                for index in (0..latest_known_runtime.runtime_version_subscriptions.len()).rev() {
                    let mut subscription = latest_known_runtime
                        .runtime_version_subscriptions
                        .swap_remove(index);
                    let to_send = latest_known_runtime
                        .runtime
                        .as_ref()
                        .map(|r| r.runtime_spec.clone())
                        .map_err(|&()| ());
                    if subscription.send(to_send).is_ok() {
                        latest_known_runtime
                            .runtime_version_subscriptions
                            .push(subscription);
                    }
                }

                latest_known_runtime
                    .runtime_version_subscriptions
                    .shrink_to_fit();
            }
        })
    });
}
