//! In-memory blockchain storage
use crate::eth::{
    backend::{
        db::{MaybeHashDatabase, StateDb},
        mem::cache::DiskStateCache,
    },
    pool::transactions::PoolTransaction,
};
use alloy_primitives::{Bytes, TxHash, B256, U256, U64};
use alloy_rpc_trace_types::{
    geth::{DefaultFrame, GethDefaultTracingOptions},
    parity::LocalizedTransactionTrace,
};
use alloy_rpc_types::{
    BlockId, BlockNumberOrTag, TransactionInfo as RethTransactionInfo, TransactionReceipt,
};
use anvil_core::eth::{
    block::{Block, PartialHeader},
    receipt::TypedReceipt,
    transaction::{MaybeImpersonatedTransaction, TransactionInfo},
};
use foundry_common::types::{ToAlloy, ToEthers};
use foundry_evm::{
    revm::primitives::Env,
    traces::{GethTraceBuilder, ParityTraceBuilder, TracingInspectorConfig},
};
use parking_lot::RwLock;
use std::{
    collections::{HashMap, VecDeque},
    fmt,
    sync::Arc,
    time::Duration,
};

// === various limits in number of blocks ===

const DEFAULT_HISTORY_LIMIT: usize = 500;
const MIN_HISTORY_LIMIT: usize = 10;
// 1hr of up-time at lowest 1s interval
const MAX_ON_DISK_HISTORY_LIMIT: usize = 3_600;

// === impl DiskStateCache ===

/// Represents the complete state of single block
pub struct InMemoryBlockStates {
    /// The states at a certain block
    states: HashMap<B256, StateDb>,
    /// states which data is moved to disk
    on_disk_states: HashMap<B256, StateDb>,
    /// How many states to store at most
    in_memory_limit: usize,
    /// minimum amount of states we keep in memory
    min_in_memory_limit: usize,
    /// maximum amount of states we keep on disk
    ///
    /// Limiting the states will prevent disk blow up, especially in interval mining mode
    max_on_disk_limit: usize,
    /// the oldest states written to disk
    oldest_on_disk: VecDeque<B256>,
    /// all states present, used to enforce `in_memory_limit`
    present: VecDeque<B256>,
    /// Stores old states on disk
    disk_cache: DiskStateCache,
}

// === impl InMemoryBlockStates ===

impl InMemoryBlockStates {
    /// Creates a new instance with limited slots
    pub fn new(limit: usize) -> Self {
        Self {
            states: Default::default(),
            on_disk_states: Default::default(),
            in_memory_limit: limit,
            min_in_memory_limit: limit.min(MIN_HISTORY_LIMIT),
            max_on_disk_limit: MAX_ON_DISK_HISTORY_LIMIT,
            oldest_on_disk: Default::default(),
            present: Default::default(),
            disk_cache: Default::default(),
        }
    }

    /// Configures no disk caching
    pub fn memory_only(mut self) -> Self {
        self.max_on_disk_limit = 0;
        self
    }

    /// This modifies the `limit` what to keep stored in memory.
    ///
    /// This will ensure the new limit adjusts based on the block time.
    /// The lowest blocktime is 1s which should increase the limit slightly
    pub fn update_interval_mine_block_time(&mut self, block_time: Duration) {
        let block_time = block_time.as_secs();
        // for block times lower than 2s we increase the mem limit since we're mining _small_ blocks
        // very fast
        // this will gradually be decreased once the max limit was reached
        if block_time <= 2 {
            self.in_memory_limit = DEFAULT_HISTORY_LIMIT * 3;
            self.enforce_limits();
        }
    }

    /// Returns true if only memory caching is supported.
    fn is_memory_only(&self) -> bool {
        self.max_on_disk_limit == 0
    }

    /// Inserts a new (hash -> state) pair
    ///
    /// When the configured limit for the number of states that can be stored in memory is reached,
    /// the oldest state is removed.
    ///
    /// Since we keep a snapshot of the entire state as history, the size of the state will increase
    /// with the transactions processed. To counter this, we gradually decrease the cache limit with
    /// the number of states/blocks until we reached the `min_limit`.
    ///
    /// When a state that was previously written to disk is requested, it is simply read from disk.
    pub fn insert(&mut self, hash: B256, state: StateDb) {
        if !self.is_memory_only() && self.present.len() >= self.in_memory_limit {
            // once we hit the max limit we gradually decrease it
            self.in_memory_limit =
                self.in_memory_limit.saturating_sub(1).max(self.min_in_memory_limit);
        }

        self.enforce_limits();

        self.states.insert(hash, state);
        self.present.push_back(hash);
    }

    /// Enforces configured limits
    fn enforce_limits(&mut self) {
        // enforce memory limits
        while self.present.len() >= self.in_memory_limit {
            // evict the oldest block
            if let Some((hash, mut state)) = self
                .present
                .pop_front()
                .and_then(|hash| self.states.remove(&hash).map(|state| (hash, state)))
            {
                // only write to disk if supported
                if !self.is_memory_only() {
                    let snapshot = state.0.clear_into_snapshot();
                    self.disk_cache.write(hash, snapshot);
                    self.on_disk_states.insert(hash, state);
                    self.oldest_on_disk.push_back(hash);
                }
            }
        }

        // enforce on disk limit and purge the oldest state cached on disk
        while !self.is_memory_only() && self.oldest_on_disk.len() >= self.max_on_disk_limit {
            // evict the oldest block
            if let Some(hash) = self.oldest_on_disk.pop_front() {
                self.on_disk_states.remove(&hash);
                self.disk_cache.remove(hash);
            }
        }
    }

    /// Returns the state for the given `hash` if present
    pub fn get(&mut self, hash: &B256) -> Option<&StateDb> {
        self.states.get(hash).or_else(|| {
            if let Some(state) = self.on_disk_states.get_mut(hash) {
                if let Some(cached) = self.disk_cache.read(*hash) {
                    state.init_from_snapshot(cached);
                    return Some(state)
                }
            }
            None
        })
    }

    /// Sets the maximum number of stats we keep in memory
    pub fn set_cache_limit(&mut self, limit: usize) {
        self.in_memory_limit = limit;
    }

    /// Clears all entries
    pub fn clear(&mut self) {
        self.states.clear();
        self.on_disk_states.clear();
        self.present.clear();
        for on_disk in std::mem::take(&mut self.oldest_on_disk) {
            self.disk_cache.remove(on_disk)
        }
    }
}

impl fmt::Debug for InMemoryBlockStates {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InMemoryBlockStates")
            .field("in_memory_limit", &self.in_memory_limit)
            .field("min_in_memory_limit", &self.min_in_memory_limit)
            .field("max_on_disk_limit", &self.max_on_disk_limit)
            .field("oldest_on_disk", &self.oldest_on_disk)
            .field("present", &self.present)
            .finish_non_exhaustive()
    }
}

impl Default for InMemoryBlockStates {
    fn default() -> Self {
        // enough in memory to store `DEFAULT_HISTORY_LIMIT` blocks in memory
        Self::new(DEFAULT_HISTORY_LIMIT)
    }
}

/// Stores the blockchain data (blocks, transactions)
#[derive(Clone)]
pub struct BlockchainStorage {
    /// all stored blocks (block hash -> block)
    pub blocks: HashMap<B256, Block>,
    /// mapping from block number -> block hash
    pub hashes: HashMap<U64, B256>,
    /// The current best hash
    pub best_hash: B256,
    /// The current best block number
    pub best_number: U64,
    /// genesis hash of the chain
    pub genesis_hash: B256,
    /// Mapping from the transaction hash to a tuple containing the transaction as well as the
    /// transaction receipt
    pub transactions: HashMap<TxHash, MinedTransaction>,
    /// The total difficulty of the chain until this block
    pub total_difficulty: U256,
}

impl BlockchainStorage {
    /// Creates a new storage with a genesis block
    pub fn new(env: &Env, base_fee: Option<U256>, timestamp: u64) -> Self {
        // create a dummy genesis block
        let partial_header = PartialHeader {
            timestamp,
            base_fee: base_fee.map(|b| b.to_ethers()),
            gas_limit: env.block.gas_limit.to_ethers(),
            beneficiary: env.block.coinbase.to_ethers(),
            difficulty: env.block.difficulty.to_ethers(),
            ..Default::default()
        };
        let block = Block::new::<MaybeImpersonatedTransaction>(partial_header, vec![], vec![]);
        let genesis_hash = block.header.hash();
        let best_hash = genesis_hash;
        let best_number: U64 = U64::from(0u64);

        Self {
            blocks: HashMap::from([(genesis_hash.to_alloy(), block)]),
            hashes: HashMap::from([(best_number, genesis_hash.to_alloy())]),
            best_hash: best_hash.to_alloy(),
            best_number,
            genesis_hash: genesis_hash.to_alloy(),
            transactions: Default::default(),
            total_difficulty: Default::default(),
        }
    }

    pub fn forked(block_number: u64, block_hash: B256, total_difficulty: U256) -> Self {
        BlockchainStorage {
            blocks: Default::default(),
            hashes: HashMap::from([(U64::from(block_number), block_hash)]),
            best_hash: block_hash,
            best_number: U64::from(block_number),
            genesis_hash: Default::default(),
            transactions: Default::default(),
            total_difficulty,
        }
    }

    #[allow(unused)]
    pub fn empty() -> Self {
        Self {
            blocks: Default::default(),
            hashes: Default::default(),
            best_hash: Default::default(),
            best_number: Default::default(),
            genesis_hash: Default::default(),
            transactions: Default::default(),
            total_difficulty: Default::default(),
        }
    }

    /// Removes all stored transactions for the given block number
    pub fn remove_block_transactions_by_number(&mut self, num: u64) {
        if let Some(hash) = self.hashes.get(&(U64::from(num))).copied() {
            self.remove_block_transactions(hash);
        }
    }

    /// Removes all stored transactions for the given block hash
    pub fn remove_block_transactions(&mut self, block_hash: B256) {
        if let Some(block) = self.blocks.get_mut(&block_hash) {
            for tx in block.transactions.iter() {
                self.transactions.remove(&tx.hash().to_alloy());
            }
            block.transactions.clear();
        }
    }
}

// === impl BlockchainStorage ===

impl BlockchainStorage {
    /// Returns the hash for [BlockNumberOrTag]
    pub fn hash(&self, number: BlockNumberOrTag) -> Option<B256> {
        let slots_in_an_epoch = U64::from(32u64);
        match number {
            BlockNumberOrTag::Latest => Some(self.best_hash),
            BlockNumberOrTag::Earliest => Some(self.genesis_hash),
            BlockNumberOrTag::Pending => None,
            BlockNumberOrTag::Number(num) => self.hashes.get(&U64::from(num)).copied(),
            BlockNumberOrTag::Safe => {
                if self.best_number > (slots_in_an_epoch) {
                    self.hashes.get(&(self.best_number - (slots_in_an_epoch))).copied()
                } else {
                    Some(self.genesis_hash) // treat the genesis block as safe "by definition"
                }
            }
            BlockNumberOrTag::Finalized => {
                if self.best_number > (slots_in_an_epoch * U64::from(2)) {
                    self.hashes
                        .get(&(self.best_number - (slots_in_an_epoch * U64::from(2))))
                        .copied()
                } else {
                    Some(self.genesis_hash)
                }
            }
        }
    }
}

/// A simple in-memory blockchain
#[derive(Clone)]
pub struct Blockchain {
    /// underlying storage that supports concurrent reads
    pub storage: Arc<RwLock<BlockchainStorage>>,
}

// === impl BlockchainStorage ===

impl Blockchain {
    /// Creates a new storage with a genesis block
    pub fn new(env: &Env, base_fee: Option<U256>, timestamp: u64) -> Self {
        Self { storage: Arc::new(RwLock::new(BlockchainStorage::new(env, base_fee, timestamp))) }
    }

    pub fn forked(block_number: u64, block_hash: B256, total_difficulty: U256) -> Self {
        Self {
            storage: Arc::new(RwLock::new(BlockchainStorage::forked(
                block_number,
                block_hash,
                total_difficulty,
            ))),
        }
    }

    /// returns the header hash of given block
    pub fn hash(&self, id: BlockId) -> Option<B256> {
        match id {
            BlockId::Hash(h) => Some(h.block_hash),
            BlockId::Number(num) => self.storage.read().hash(num),
        }
    }

    pub fn get_block_by_hash(&self, hash: &B256) -> Option<Block> {
        self.storage.read().blocks.get(hash).cloned()
    }

    pub fn get_transaction_by_hash(&self, hash: &B256) -> Option<MinedTransaction> {
        self.storage.read().transactions.get(hash).cloned()
    }

    /// Returns the total number of blocks
    pub fn blocks_count(&self) -> usize {
        self.storage.read().blocks.len()
    }
}

/// Represents the outcome of mining a new block
#[derive(Clone, Debug)]
pub struct MinedBlockOutcome {
    /// The block that was mined
    pub block_number: U64,
    /// All transactions included in the block
    pub included: Vec<Arc<PoolTransaction>>,
    /// All transactions that were attempted to be included but were invalid at the time of
    /// execution
    pub invalid: Vec<Arc<PoolTransaction>>,
}

/// Container type for a mined transaction
#[derive(Clone, Debug)]
pub struct MinedTransaction {
    pub info: TransactionInfo,
    pub receipt: TypedReceipt,
    pub block_hash: B256,
    pub block_number: u64,
}

// === impl MinedTransaction ===

impl MinedTransaction {
    /// Returns the traces of the transaction for `trace_transaction`
    pub fn parity_traces(&self) -> Vec<LocalizedTransactionTrace> {
        ParityTraceBuilder::new(
            self.info.traces.clone(),
            None,
            TracingInspectorConfig::default_parity(),
        )
        .into_localized_transaction_traces(RethTransactionInfo {
            hash: Some(self.info.transaction_hash.to_alloy()),
            index: Some(self.info.transaction_index as u64),
            block_hash: Some(self.block_hash),
            block_number: Some(self.block_number),
            base_fee: None,
        })
    }

    pub fn geth_trace(&self, opts: GethDefaultTracingOptions) -> DefaultFrame {
        GethTraceBuilder::new(self.info.traces.clone(), TracingInspectorConfig::default_geth())
            .geth_traces(
                self.receipt.gas_used().as_u64(),
                self.info.out.clone().unwrap_or_default().0.into(),
                opts,
            )
    }
}

/// Intermediary Anvil representation of a receipt
#[derive(Clone, Debug)]
pub struct MinedTransactionReceipt {
    /// The actual json rpc receipt object
    pub inner: TransactionReceipt,
    /// Output data fo the transaction
    pub out: Option<Bytes>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eth::backend::db::Db;
    use alloy_primitives::{Address, B256, U256};
    use foundry_evm::{
        backend::MemDb,
        revm::{
            db::DatabaseRef,
            primitives::{AccountInfo, U256 as rU256},
        },
    };

    #[test]
    fn test_interval_update() {
        let mut storage = InMemoryBlockStates::default();
        storage.update_interval_mine_block_time(Duration::from_secs(1));
        assert_eq!(storage.in_memory_limit, DEFAULT_HISTORY_LIMIT * 3);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn can_read_write_cached_state() {
        let mut storage = InMemoryBlockStates::new(1);
        let one = B256::from(U256::from(1));
        let two = B256::from(U256::from(2));

        let mut state = MemDb::default();
        let addr = Address::random();
        let info = AccountInfo::from_balance(rU256::from(1337));
        state.insert_account(addr, info);
        storage.insert(one, StateDb::new(state));
        storage.insert(two, StateDb::new(MemDb::default()));

        // wait for files to be flushed
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        assert_eq!(storage.on_disk_states.len(), 1);
        assert!(storage.on_disk_states.get(&one).is_some());

        let loaded = storage.get(&one).unwrap();

        let acc = loaded.basic_ref(addr).unwrap().unwrap();
        assert_eq!(acc.balance, rU256::from(1337u64));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn can_decrease_state_cache_size() {
        let limit = 15;
        let mut storage = InMemoryBlockStates::new(limit);

        let num_states = 30;
        for idx in 0..num_states {
            let mut state = MemDb::default();
            let hash = B256::from(U256::from(idx));
            let addr = Address::from_word(hash);
            let balance = (idx * 2) as u64;
            let info = AccountInfo::from_balance(rU256::from(balance));
            state.insert_account(addr, info);
            storage.insert(hash, StateDb::new(state));
        }

        // wait for files to be flushed
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        assert_eq!(storage.on_disk_states.len(), num_states - storage.min_in_memory_limit);
        assert_eq!(storage.present.len(), storage.min_in_memory_limit);

        for idx in 0..num_states {
            let hash = B256::from(U256::from(idx));
            let addr = Address::from_word(hash);
            let loaded = storage.get(&hash).unwrap();
            let acc = loaded.basic_ref(addr).unwrap().unwrap();
            let balance = (idx * 2) as u64;
            assert_eq!(acc.balance, rU256::from(balance));
        }
    }
}