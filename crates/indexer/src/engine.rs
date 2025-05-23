use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Debug;
use std::hash::Hash;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use bitflags::bitflags;
use dojo_utils::provider as provider_utils;
use dojo_world::contracts::world::WorldContractReader;
use futures_util::future::try_join_all;
use hashlink::LinkedHashMap;
use starknet::core::types::requests::{
    GetBlockWithTxHashesRequest, GetEventsRequest, GetTransactionByHashRequest,
};
use starknet::core::types::{
    BlockHashAndNumber, BlockId, BlockTag, EmittedEvent, Event, EventFilter, EventFilterWithPage,
    MaybePendingBlockWithReceipts, MaybePendingBlockWithTxHashes, PendingBlockWithReceipts,
    ResultPageRequest, Transaction, TransactionReceipt, TransactionWithReceipt,
};
use starknet::macros::selector;
use starknet::providers::{Provider, ProviderRequestData, ProviderResponseData};
use starknet_crypto::Felt;
use tokio::sync::broadcast::Sender;
use tokio::time::{sleep, Instant};
use torii_processors::{EventProcessorConfig, Processors};
use torii_sqlite::cache::ContractClassCache;
use torii_sqlite::types::{Contract, ContractType};
use torii_sqlite::{Cursors, Sql};
use tracing::{debug, error, info, trace};

use crate::constants::LOG_TARGET;
use torii_processors::task_manager::{ParallelizedEvent, TaskManager};

bitflags! {
    #[derive(Debug, Clone)]
    pub struct IndexingFlags: u32 {
        const TRANSACTIONS = 0b00000001;
        const RAW_EVENTS = 0b00000010;
        const PENDING_BLOCKS = 0b00000100;
    }
}

#[derive(Debug)]
pub struct EngineConfig {
    pub polling_interval: Duration,
    pub batch_chunk_size: usize,
    pub blocks_chunk_size: u64,
    pub events_chunk_size: u64,
    pub max_concurrent_tasks: usize,
    pub flags: IndexingFlags,
    pub event_processor_config: EventProcessorConfig,
    pub world_block: u64,
}

#[derive(Debug)]
pub enum FetchDataResult {
    Range(FetchRangeResult),
    Pending(FetchPendingResult),
    None,
}

impl FetchDataResult {
    pub fn block_id(&self) -> Option<BlockId> {
        match self {
            FetchDataResult::Range(range) => {
                Some(BlockId::Number(*range.blocks.keys().last().unwrap()))
            }
            FetchDataResult::Pending(_pending) => Some(BlockId::Tag(BlockTag::Pending)),
            FetchDataResult::None => None,
        }
    }
}

#[derive(Debug)]
pub struct FetchRangeTransaction {
    // this is Some if the transactions indexing flag
    // is enabled
    pub transaction: Option<Transaction>,
    pub events: Vec<EmittedEvent>,
}

#[derive(Debug)]
pub struct FetchRangeResult {
    // block_number -> (transaction_hash -> events)
    pub transactions: BTreeMap<u64, LinkedHashMap<Felt, FetchRangeTransaction>>,
    // block_number -> block_timestamp
    pub blocks: BTreeMap<u64, u64>,
}

#[derive(Debug)]
pub struct FetchPendingResult {
    pub pending_block: Box<PendingBlockWithReceipts>,
    pub last_pending_block_tx: Option<Felt>,
    pub block_number: u64,
}

#[allow(missing_debug_implementations)]
pub struct Engine<P: Provider + Send + Sync + std::fmt::Debug + 'static> {
    world: Arc<WorldContractReader<P>>,
    db: Sql,
    provider: Arc<P>,
    processors: Arc<Processors<P>>,
    config: EngineConfig,
    shutdown_tx: Sender<()>,
    task_manager: TaskManager<P>,
    contracts: Arc<HashMap<Felt, ContractType>>,
    contract_class_cache: Arc<ContractClassCache<P>>,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            polling_interval: Duration::from_millis(500),
            batch_chunk_size: 1024,
            blocks_chunk_size: 10240,
            events_chunk_size: 1024,
            max_concurrent_tasks: 100,
            flags: IndexingFlags::empty(),
            event_processor_config: EventProcessorConfig::default(),
            world_block: 0,
        }
    }
}

struct UnprocessedEvent {
    keys: Vec<String>,
    data: Vec<String>,
}

impl<P: Provider + Send + Sync + std::fmt::Debug + 'static> Engine<P> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        world: WorldContractReader<P>,
        db: Sql,
        provider: P,
        processors: Processors<P>,
        config: EngineConfig,
        shutdown_tx: Sender<()>,
        contracts: &[Contract],
    ) -> Self {
        let contracts = Arc::new(
            contracts
                .iter()
                .map(|contract| (contract.address, contract.r#type))
                .collect(),
        );
        let world = Arc::new(world);
        let processors = Arc::new(processors);
        let max_concurrent_tasks = config.max_concurrent_tasks;
        let event_processor_config = config.event_processor_config.clone();
        let provider = Arc::new(provider);

        Self {
            world: world.clone(),
            db: db.clone(),
            provider: provider.clone(),
            processors: processors.clone(),
            config,
            shutdown_tx,
            contracts,
            task_manager: TaskManager::new(
                db,
                world,
                processors,
                max_concurrent_tasks,
                event_processor_config,
            ),
            contract_class_cache: Arc::new(ContractClassCache::new(provider)),
        }
    }

    pub async fn start(&mut self) -> Result<()> {
        if let Err(e) = provider_utils::health_check_provider(self.provider.clone()).await {
            error!(target: LOG_TARGET,"Provider health check failed during engine start");
            return Err(e);
        }

        let mut backoff_delay = Duration::from_secs(1);
        let max_backoff_delay = Duration::from_secs(60);

        let mut shutdown_rx = self.shutdown_tx.subscribe();

        let mut erroring_out = false;
        loop {
            let cursors = self.db.cursors().await?;
            tokio::select! {
                _ = shutdown_rx.recv() => {
                    break Ok(());
                }
                res = self.fetch_data(&cursors) => {
                    match res {
                        Ok(fetch_result) => {
                            let instant = Instant::now();
                            if erroring_out {
                                erroring_out = false;
                                backoff_delay = Duration::from_secs(1);
                                info!(target: LOG_TARGET, "Syncing reestablished.");
                            }

                            let block_id = fetch_result.block_id();
                            match self.process(fetch_result).await {
                                Ok(_) => {
                                    // Its only `None` when `FetchDataResult::None` in which case
                                    // we don't need to flush or apply cache diff
                                    if let Some(block_id) = block_id {
                                        self.db.apply_cache_diff().await?;
                                        self.db.execute().await?;
                                        debug!(target: LOG_TARGET, block_number = ?block_id, "Applied cache diff and executed.");
                                    }
                                },
                                Err(e) => {
                                    error!(target: LOG_TARGET, error = %e, "Processing fetched data.");
                                    erroring_out = true;
                                    // incase of error rollback the transaction
                                    self.db.rollback().await?;
                                    self.task_manager.clear_tasks();
                                    sleep(backoff_delay).await;
                                    if backoff_delay < max_backoff_delay {
                                        backoff_delay *= 2;
                                    }
                                }
                            }
                            debug!(target: LOG_TARGET, duration = ?instant.elapsed(), "Processed fetched data.");
                        }
                        Err(e) => {
                            erroring_out = true;
                            error!(target: LOG_TARGET, error = %e, "Fetching data.");
                            sleep(backoff_delay).await;
                            if backoff_delay < max_backoff_delay {
                                backoff_delay *= 2;
                            }
                        }
                    };
                    sleep(self.config.polling_interval).await;
                }
            }
        }
    }

    pub async fn fetch_data(&mut self, cursors: &Cursors) -> Result<FetchDataResult> {
        let latest_block = self.provider.block_hash_and_number().await?;
        let from = cursors.head.unwrap_or(self.config.world_block);
        // this is non-inclusive. this just means that we stop doing events pages fetches once we
        // reach a page with an event that is after the latest block. so in our final
        // commit; we could end up with a higher head than this one.
        let to = latest_block
            .block_number
            .min(from + self.config.blocks_chunk_size);

        let instant = Instant::now();
        let result = if from < latest_block.block_number {
            let from = if from == 0 { from } else { from + 1 };

            // Fetch all events from 'from' to our blocks chunk size
            let range = self
                .fetch_range(from, to, &cursors.cursor_map, latest_block.block_number)
                .await?;

            debug!(target: LOG_TARGET, duration = ?instant.elapsed(), from = %from, to = %range.blocks.keys().last().unwrap(), "Fetched data for range.");
            FetchDataResult::Range(range)
        } else if self.config.flags.contains(IndexingFlags::PENDING_BLOCKS) {
            let data = self
                .fetch_pending(latest_block.clone(), cursors.last_pending_block_tx)
                .await?;
            debug!(target: LOG_TARGET, duration = ?instant.elapsed(), latest_block_number = %latest_block.block_number, "Fetched pending data.");
            if let Some(data) = data {
                FetchDataResult::Pending(data)
            } else {
                FetchDataResult::None
            }
        } else {
            FetchDataResult::None
        };

        Ok(result)
    }

    pub async fn fetch_range(
        &self,
        from: u64,
        to: u64,
        cursor_map: &HashMap<Felt, Felt>,
        latest_block_number: u64,
    ) -> Result<FetchRangeResult> {
        let mut events = vec![];

        // Create initial batch requests for all contracts
        let mut event_requests = Vec::new();
        for (contract_address, _) in self.contracts.iter() {
            let events_filter = EventFilter {
                from_block: Some(BlockId::Number(from)),
                to_block: Some(BlockId::Tag(BlockTag::Latest)),
                address: Some(*contract_address),
                keys: None,
            };

            event_requests.push((
                *contract_address,
                ProviderRequestData::GetEvents(GetEventsRequest {
                    filter: EventFilterWithPage {
                        event_filter: events_filter,
                        result_page_request: ResultPageRequest {
                            continuation_token: None,
                            chunk_size: self.config.events_chunk_size,
                        },
                    },
                }),
            ));
        }

        // Recursively fetch all events using batch requests
        events.extend(self.fetch_events(event_requests, cursor_map, to).await?);

        // Process events to get unique blocks and transactions
        let mut blocks = BTreeMap::new();
        let mut transactions = BTreeMap::new();
        let mut block_numbers = HashSet::new();

        for event in events {
            let block_number = match event.block_number {
                Some(block_number) => block_number,
                None => unreachable!("In fetch range all events should have block number"),
            };

            block_numbers.insert(block_number);

            transactions
                .entry(block_number)
                .or_insert(LinkedHashMap::new())
                .entry(event.transaction_hash)
                .or_insert(FetchRangeTransaction {
                    transaction: None,
                    events: vec![],
                })
                .events
                .push(event);
        }

        // If transactions indexing flag is enabled, we should batch request all
        // of our recolted transactions
        if self.config.flags.contains(IndexingFlags::TRANSACTIONS) && !transactions.is_empty() {
            let mut transaction_requests = Vec::with_capacity(transactions.len());
            let mut block_numbers = Vec::with_capacity(transactions.len());
            for (block_number, transactions) in &transactions {
                for (transaction_hash, _) in transactions {
                    transaction_requests.push(ProviderRequestData::GetTransactionByHash(
                        GetTransactionByHashRequest {
                            transaction_hash: *transaction_hash,
                        },
                    ));
                    block_numbers.push(*block_number);
                }
            }

            let transaction_results = self.chunked_batch_requests(&transaction_requests).await?;

            for (block_number, result) in block_numbers.into_iter().zip(transaction_results) {
                match result {
                    ProviderResponseData::GetTransactionByHash(transaction) => {
                        transactions.entry(block_number).and_modify(|txns| {
                            txns.entry(*transaction.transaction_hash())
                                .and_modify(|tx| tx.transaction = Some(transaction));
                        });
                    }
                    _ => unreachable!(),
                }
            }
        }

        // Always ensure the latest block number is included
        block_numbers.insert(to);

        // Batch request block timestamps
        let mut timestamp_requests = Vec::new();
        for block_number in &block_numbers {
            timestamp_requests.push(ProviderRequestData::GetBlockWithTxHashes(
                GetBlockWithTxHashesRequest {
                    block_id: if *block_number == latest_block_number {
                        BlockId::Tag(BlockTag::Latest)
                    } else {
                        BlockId::Number(*block_number)
                    },
                },
            ));
        }

        // Execute timestamp requests in batch
        if !timestamp_requests.is_empty() {
            let timestamp_results = self.chunked_batch_requests(&timestamp_requests).await?;

            // Process timestamp results
            for (block_number, result) in block_numbers.iter().zip(timestamp_results) {
                match result {
                    ProviderResponseData::GetBlockWithTxHashes(block) => {
                        let timestamp = match block {
                            MaybePendingBlockWithTxHashes::Block(block) => block.timestamp,
                            MaybePendingBlockWithTxHashes::PendingBlock(block) => block.timestamp,
                        };
                        blocks.insert(*block_number, timestamp);
                    }
                    _ => unreachable!(),
                }
            }
        }

        trace!(target: LOG_TARGET, "Transactions: {}", &transactions.len());
        trace!(target: LOG_TARGET, "Blocks: {}", &blocks.len());

        Ok(FetchRangeResult {
            transactions,
            blocks,
        })
    }

    async fn fetch_events(
        &self,
        initial_requests: Vec<(Felt, ProviderRequestData)>,
        cursor_map: &HashMap<Felt, Felt>,
        to: u64,
    ) -> Result<Vec<EmittedEvent>> {
        let mut all_events = Vec::new();
        let mut current_requests = initial_requests;

        while !current_requests.is_empty() {
            let mut next_requests = Vec::new();
            let mut events = Vec::new();

            // Extract just the requests without the contract addresses
            let batch_requests: Vec<ProviderRequestData> = current_requests
                .iter()
                .map(|(_, req)| req.clone())
                .collect();
            let batch_results = self.chunked_batch_requests(&batch_requests).await?;

            // Process results and prepare next batch of requests if needed
            for ((contract_address, original_request), result) in
                current_requests.into_iter().zip(batch_results)
            {
                let last_contract_tx = cursor_map.get(&contract_address).cloned();
                let mut last_contract_tx_tmp = last_contract_tx;

                match result {
                    ProviderResponseData::GetEvents(events_page) => {
                        let last_block_number = events_page
                            .events
                            .last()
                            .map_or(0, |e| e.block_number.unwrap());

                        // Process events for this page, only including events up to our target
                        // block
                        for event in events_page.events.clone() {
                            let block_number = event.block_number.unwrap();
                            if block_number > to {
                                continue;
                            }

                            // Then we skip all transactions until we reach the last pending
                            // processed transaction (if any)
                            if let Some(last_contract_tx) = last_contract_tx_tmp {
                                if event.transaction_hash != last_contract_tx {
                                    continue;
                                }
                                last_contract_tx_tmp = None;
                            }

                            // Skip the latest pending block transaction events
                            // * as we might have multiple events for the same transaction
                            if let Some(last_contract_tx) = last_contract_tx {
                                if event.transaction_hash == last_contract_tx {
                                    continue;
                                }
                            }

                            events.push(event);
                        }

                        // Add continuation request to next_requests instead of recursing
                        if let Some(continuation_token) = events_page.continuation_token {
                            if last_block_number < to && !events_page.events.is_empty() {
                                if let ProviderRequestData::GetEvents(mut next_request) =
                                    original_request
                                {
                                    next_request.filter.result_page_request.continuation_token =
                                        Some(continuation_token);
                                    next_requests.push((
                                        contract_address,
                                        ProviderRequestData::GetEvents(next_request),
                                    ));
                                }
                            }
                        }
                    }
                    _ => {
                        return Err(anyhow::anyhow!(
                            "Unexpected response type from batch events request"
                        ));
                    }
                }
            }

            all_events.extend(events);
            current_requests = next_requests;
        }

        Ok(all_events)
    }

    async fn fetch_pending(
        &self,
        block: BlockHashAndNumber,
        last_pending_block_tx: Option<Felt>,
    ) -> Result<Option<FetchPendingResult>> {
        let pending_block = if let MaybePendingBlockWithReceipts::PendingBlock(pending) = self
            .provider
            .get_block_with_receipts(BlockId::Tag(BlockTag::Pending))
            .await?
        {
            // if the parent hash is not the hash of the latest block that we fetched, then it means
            // a new block got mined just after we fetched the latest block information
            if block.block_hash != pending.parent_hash {
                return Ok(None);
            }

            pending
        } else {
            // TODO: change this to unreachable once katana is updated to return PendingBlockWithTxs
            // when BlockTag is Pending unreachable!("We requested pending block, so it
            // must be pending");
            return Ok(None);
        };

        Ok(Some(FetchPendingResult {
            pending_block: Box::new(pending_block),
            block_number: block.block_number + 1,
            last_pending_block_tx,
        }))
    }

    pub async fn process(&mut self, fetch_result: FetchDataResult) -> Result<()> {
        match fetch_result {
            FetchDataResult::Range(range) => self.process_range(range).await?,
            FetchDataResult::Pending(data) => self.process_pending(data).await?,
            FetchDataResult::None => {}
        };

        Ok(())
    }

    pub async fn process_pending(&mut self, data: FetchPendingResult) -> Result<()> {
        // Skip transactions that have been processed already
        // Our cursor is the last processed transaction

        let mut last_pending_block_tx_cursor = data.last_pending_block_tx;
        let mut last_pending_block_tx = data.last_pending_block_tx;

        let timestamp = data.pending_block.timestamp;

        let mut cursor_map = HashMap::new();
        for t in data.pending_block.transactions {
            let transaction_hash = t.transaction.transaction_hash();
            if let Some(tx) = last_pending_block_tx_cursor {
                if transaction_hash != &tx {
                    continue;
                }

                last_pending_block_tx_cursor = None;
                continue;
            }

            if let Err(e) = self
                .process_transaction_with_receipt(&t, data.block_number, timestamp, &mut cursor_map)
                .await
            {
                error!(target: LOG_TARGET, error = %e, transaction_hash = %format!("{:#x}", transaction_hash), "Processing pending transaction.");
                return Err(e);
            }

            last_pending_block_tx = Some(*transaction_hash);
            debug!(target: LOG_TARGET, transaction_hash = %format!("{:#x}", transaction_hash), "Processed pending transaction.");
        }

        // Process parallelized events
        self.task_manager.process_tasks().await?;

        self.db.update_cursors(
            data.block_number - 1,
            timestamp,
            last_pending_block_tx,
            cursor_map,
        )?;

        Ok(())
    }

    pub async fn process_range(&mut self, range: FetchRangeResult) -> Result<()> {
        let mut processed_blocks = HashSet::new();
        let mut cursor_map = HashMap::new();

        // Process all transactions in the chunk
        for (block_number, transactions) in range.transactions {
            for (transaction_hash, tx) in transactions {
                trace!(target: LOG_TARGET, "Processing transaction hash: {:#x}", transaction_hash);

                self.process_transaction_with_events(
                    transaction_hash,
                    tx.events.as_slice(),
                    block_number,
                    range.blocks[&block_number],
                    tx.transaction,
                    &mut cursor_map,
                )
                .await?;
            }

            // Process block
            if !processed_blocks.contains(&block_number) {
                self.process_block(block_number, range.blocks[&block_number])
                    .await?;
                processed_blocks.insert(block_number);
            }
        }

        // Process parallelized events
        self.task_manager.process_tasks().await?;

        let (last_block_number, last_block_timestamp) = range.blocks.iter().last().unwrap();
        self.db
            .update_cursors(*last_block_number, *last_block_timestamp, None, cursor_map)?;

        Ok(())
    }

    async fn process_transaction_with_events(
        &mut self,
        transaction_hash: Felt,
        events: &[EmittedEvent],
        block_number: u64,
        block_timestamp: u64,
        transaction: Option<Transaction>,
        cursor_map: &mut HashMap<Felt, (Felt, u64)>,
    ) -> Result<()> {
        let mut unique_contracts = HashSet::new();
        let mut unique_models = HashSet::new();
        // Contract -> Cursor
        for (event_idx, event) in events.iter().enumerate() {
            // NOTE: erc* processors expect the event_id to be in this format to get
            // transaction_hash:
            let event_id = format!(
                "{:#064x}:{:#x}:{:#04x}",
                block_number, transaction_hash, event_idx
            );

            let event = Event {
                from_address: event.from_address,
                keys: event.keys.clone(),
                data: event.data.clone(),
            };

            let Some(&contract_type) = self.contracts.get(&event.from_address) else {
                continue;
            };

            unique_contracts.insert(event.from_address);
            let event_key = event.keys[0];
            if contract_type == ContractType::WORLD
                && (event_key == selector!("StoreSetRecord")
                    || event_key == selector!("StoreUpdateRecord")
                    || event_key == selector!("StoreDelRecord")
                    || event_key == selector!("StoreUpdateMember")
                    || event_key == selector!("EventEmitted"))
            {
                unique_models.insert(event.keys[1]);
            }

            self.process_event(
                block_number,
                block_timestamp,
                &event_id,
                &event,
                transaction_hash,
                contract_type,
            )
            .await?;
        }

        for contract in &unique_contracts {
            let entry = cursor_map.entry(*contract).or_insert((transaction_hash, 0));
            entry.0 = transaction_hash;
            entry.1 += 1;
        }

        if let Some(ref transaction) = transaction {
            Self::process_transaction(
                self,
                block_number,
                block_timestamp,
                transaction_hash,
                &unique_contracts,
                transaction,
                &unique_models,
            )
            .await?;
        }

        Ok(())
    }

    // Process a transaction and events from its receipt.
    // Returns whether the transaction has a world event.
    async fn process_transaction_with_receipt(
        &mut self,
        transaction_with_receipt: &TransactionWithReceipt,
        block_number: u64,
        block_timestamp: u64,
        cursor_map: &mut HashMap<Felt, (Felt, u64)>,
    ) -> Result<()> {
        let transaction_hash = transaction_with_receipt.transaction.transaction_hash();
        let events = match &transaction_with_receipt.receipt {
            TransactionReceipt::Invoke(receipt) => Some(&receipt.events),
            TransactionReceipt::L1Handler(receipt) => Some(&receipt.events),
            _ => None,
        };

        let mut unique_contracts = HashSet::new();
        let mut unique_models = HashSet::new();
        if let Some(events) = events {
            for (event_idx, event) in events.iter().enumerate() {
                // Skip events that are not from a contract we are indexing
                let Some(&contract_type) = self.contracts.get(&event.from_address) else {
                    continue;
                };

                unique_contracts.insert(event.from_address);
                let event_key = event.keys[0];
                if contract_type == ContractType::WORLD
                    && (event_key == selector!("StoreSetRecord")
                        || event_key == selector!("StoreUpdateRecord")
                        || event_key == selector!("StoreDelRecord")
                        || event_key == selector!("StoreUpdateMember")
                        || event_key == selector!("EventEmitted"))
                {
                    unique_models.insert(event.keys[1]);
                }
                // NOTE: erc* processors expect the event_id to be in this format to get
                // transaction_hash:
                let event_id: String = format!(
                    "{:#064x}:{:#x}:{:#04x}",
                    block_number, *transaction_hash, event_idx
                );

                self.process_event(
                    block_number,
                    block_timestamp,
                    &event_id,
                    event,
                    *transaction_hash,
                    contract_type,
                )
                .await?;
            }

            // Process transaction if we have at least one an event from a contract we are indexing
            if self.config.flags.contains(IndexingFlags::TRANSACTIONS)
                && !unique_contracts.is_empty()
            {
                self.process_transaction(
                    block_number,
                    block_timestamp,
                    *transaction_hash,
                    &unique_contracts,
                    &transaction_with_receipt.transaction,
                    &unique_models,
                )
                .await?;
            }
        }

        for contract in unique_contracts {
            let entry = cursor_map.entry(contract).or_insert((*transaction_hash, 0));
            entry.0 = *transaction_hash;
            entry.1 += 1;
        }

        Ok(())
    }

    async fn process_block(&mut self, block_number: u64, block_timestamp: u64) -> Result<()> {
        for processor in &self.processors.block {
            processor
                .process(
                    &mut self.db,
                    self.provider.as_ref(),
                    block_number,
                    block_timestamp,
                )
                .await?
        }

        trace!(target: LOG_TARGET, block_number = %block_number, "Processed block.");
        Ok(())
    }

    async fn process_transaction(
        &mut self,
        block_number: u64,
        block_timestamp: u64,
        transaction_hash: Felt,
        contract_addresses: &HashSet<Felt>,
        transaction: &Transaction,
        unique_models: &HashSet<Felt>,
    ) -> Result<()> {
        for processor in &self.processors.transaction {
            processor
                .process(
                    &mut self.db,
                    self.provider.as_ref(),
                    block_number,
                    block_timestamp,
                    transaction_hash,
                    contract_addresses,
                    transaction,
                    self.contract_class_cache.as_ref(),
                    unique_models,
                )
                .await?
        }

        Ok(())
    }

    async fn process_event(
        &mut self,
        block_number: u64,
        block_timestamp: u64,
        event_id: &str,
        event: &Event,
        transaction_hash: Felt,
        contract_type: ContractType,
    ) -> Result<()> {
        if self.config.flags.contains(IndexingFlags::RAW_EVENTS) {
            self.db
                .store_event(event_id, event, transaction_hash, block_timestamp)?;
        }

        let event_key = event.keys[0];

        let processors = self.processors.get_event_processors(contract_type);
        let Some(processors) = processors.get(&event_key) else {
            // if we dont have a processor for this event, we try the catch all processor
            if self.processors.catch_all_event.validate(event) {
                if let Err(e) = self
                    .processors
                    .catch_all_event
                    .process(
                        self.world.clone(),
                        &mut self.db,
                        block_number,
                        block_timestamp,
                        event_id,
                        event,
                        &self.config.event_processor_config,
                    )
                    .await
                {
                    error!(target: LOG_TARGET, error = %e, "Processing catch all event processor.");
                    return Err(e);
                }
            } else {
                let unprocessed_event = UnprocessedEvent {
                    keys: event.keys.iter().map(|k| format!("{:#x}", k)).collect(),
                    data: event.data.iter().map(|d| format!("{:#x}", d)).collect(),
                };

                trace!(
                    target: LOG_TARGET,
                    keys = ?unprocessed_event.keys,
                    data = ?unprocessed_event.data,
                    "Unprocessed event.",
                );
            }

            return Ok(());
        };

        let processor = processors
            .iter()
            .find(|p| p.validate(event))
            .expect("Must find atleast one processor for the event");

        let task_identifier = processor.task_identifier(event);
        let dependencies = processor.task_dependencies(event);

        self.task_manager.add_parallelized_event_with_dependencies(
            task_identifier,
            dependencies,
            ParallelizedEvent {
                contract_type,
                event_id: event_id.to_string(),
                event: event.clone(),
                block_number,
                block_timestamp,
            },
        );

        Ok(())
    }

    async fn chunked_batch_requests(
        &self,
        requests: &[ProviderRequestData],
    ) -> Result<Vec<ProviderResponseData>> {
        if requests.is_empty() {
            return Ok(Vec::new());
        }

        let mut futures = Vec::new();
        for chunk in requests.chunks(self.config.batch_chunk_size) {
            futures.push(async move { self.provider.batch_requests(chunk).await });
        }

        let results_of_chunks: Vec<Vec<ProviderResponseData>> = try_join_all(futures)
            .await
            .with_context(|| {
                format!(
                    "One or more batch requests failed during chunked execution. This could be due to the provider being overloaded. You can try reducing the batch chunk size. Total requests: {}. Batch chunk size: {}",
                    requests.len(),
                    self.config.batch_chunk_size
                )
            })?;

        let flattened_results = results_of_chunks.into_iter().flatten().collect();

        Ok(flattened_results)
    }
}

// event_id format: block_number:transaction_hash:event_idx
pub fn get_transaction_hash_from_event_id(event_id: &str) -> String {
    event_id.split(':').nth(1).unwrap().to_string()
}
