use std::path::PathBuf;

use rusqlite::Connection;

use chainstate::stacks::index::marf::{MarfConnection, MarfTransaction, MARF};
use chainstate::stacks::index::{Error, MarfTrieId};
use core::{FIRST_BURNCHAIN_CONSENSUS_HASH, FIRST_STACKS_BLOCK_HASH};
use util::db::IndexDBConn;
use vm::analysis::AnalysisDatabase;
use vm::database::{
    BurnStateDB, ClarityBackingStore, ClarityDatabase, HeadersDB, SqliteConnection,
};
use vm::errors::{IncomparableError, InterpreterError, InterpreterResult, RuntimeErrorType};
use vm::types::QualifiedContractIdentifier;

use crate::types::chainstate::{BlockHeaderHash, StacksBlockHeader};
use crate::types::chainstate::{MARFValue, StacksBlockId};
use crate::types::proof::{ClarityMarfTrieId, TrieHash, TrieMerkleProof};
use crate::util::db::Error as db_error;

/// The MarfedKV struct is used to wrap a MARF data structure and side-storage
///   for use as a K/V store for ClarityDB or the AnalysisDB.
/// The Clarity VM and type checker do not "know" to begin/commit the block they are currently processing:
///   each instantiation of the VM simply executes one transaction. So the block handling
///   loop will need to invoke these two methods (begin + commit) outside of the context of the VM.
///   NOTE: Clarity will panic if you try to execute it from a non-initialized MarfedKV context.
///   (See: vm::tests::with_marfed_environment())
pub struct MarfedKV {
    chain_tip: StacksBlockId,
    marf: MARF<StacksBlockId>,
}

impl MarfedKV {
    fn setup_db(path_str: &str, unconfirmed: bool) -> InterpreterResult<MARF<StacksBlockId>> {
        let mut path = PathBuf::from(path_str);

        std::fs::create_dir_all(&path)
            .map_err(|_| InterpreterError::FailedToCreateDataDirectory)?;

        path.push("marf.sqlite");
        let marf_path = path
            .to_str()
            .ok_or_else(|| InterpreterError::BadFileName)?
            .to_string();

        let mut marf: MARF<StacksBlockId> = if unconfirmed {
            MARF::from_path_unconfirmed(&marf_path)
                .map_err(|err| InterpreterError::MarfFailure(IncomparableError { err }))?
        } else {
            MARF::from_path(&marf_path)
                .map_err(|err| InterpreterError::MarfFailure(IncomparableError { err }))?
        };

        if SqliteConnection::check_schema(&marf.sqlite_conn()).is_ok() {
            // no need to initialize
            return Ok(marf);
        }

        let tx = marf
            .storage_tx()
            .map_err(|err| InterpreterError::DBError(IncomparableError { err }))?;

        SqliteConnection::initialize_conn(&tx)?;
        tx.commit()
            .map_err(|err| InterpreterError::SqliteError(IncomparableError { err }))?;

        Ok(marf)
    }

    pub fn open(path_str: &str, miner_tip: Option<&StacksBlockId>) -> InterpreterResult<MarfedKV> {
        let marf = MarfedKV::setup_db(path_str, false)?;
        let chain_tip = match miner_tip {
            Some(ref miner_tip) => *miner_tip.clone(),
            None => StacksBlockId::sentinel(),
        };

        Ok(MarfedKV { marf, chain_tip })
    }

    pub fn open_unconfirmed(
        path_str: &str,
        miner_tip: Option<&StacksBlockId>,
    ) -> InterpreterResult<MarfedKV> {
        let marf = MarfedKV::setup_db(path_str, true)?;
        let chain_tip = match miner_tip {
            Some(ref miner_tip) => *miner_tip.clone(),
            None => StacksBlockId::sentinel(),
        };

        Ok(MarfedKV { marf, chain_tip })
    }

    // used by benchmarks
    pub fn temporary() -> MarfedKV {
        use rand::Rng;
        use std::env;
        use util::hash::to_hex;

        let mut path = env::temp_dir();
        let random_bytes = rand::thread_rng().gen::<[u8; 32]>();
        path.push(to_hex(&random_bytes));

        let marf = MarfedKV::setup_db(
            path.to_str()
                .expect("Inexplicably non-UTF-8 character in filename"),
            false,
        )
        .unwrap();

        let chain_tip = StacksBlockId::sentinel();

        MarfedKV { marf, chain_tip }
    }

    pub fn begin_read_only<'a>(
        &'a mut self,
        at_block: Option<&StacksBlockId>,
    ) -> ReadOnlyMarfStore<'a> {
        let chain_tip = if let Some(at_block) = at_block {
            self.marf.open_block(at_block).unwrap_or_else(|e| {
                error!(
                    "Failed to open read only connection at {}: {:?}",
                    at_block, &e
                );
                panic!()
            });
            at_block.clone()
        } else {
            self.chain_tip.clone()
        };
        ReadOnlyMarfStore {
            chain_tip,
            marf: &mut self.marf,
        }
    }

    pub fn begin_read_only_checked<'a>(
        &'a mut self,
        at_block: Option<&StacksBlockId>,
    ) -> InterpreterResult<ReadOnlyMarfStore<'a>> {
        let chain_tip = if let Some(at_block) = at_block {
            self.marf.open_block(at_block).map_err(|e| {
                debug!(
                    "Failed to open read only connection at {}: {:?}",
                    at_block, &e
                );
                InterpreterError::MarfFailure(IncomparableError {
                    err: Error::NotFoundError,
                })
            })?;
            at_block.clone()
        } else {
            self.chain_tip.clone()
        };
        Ok(ReadOnlyMarfStore {
            chain_tip,
            marf: &mut self.marf,
        })
    }

    /// begin, commit, rollback a save point identified by key
    ///    this is used to clean up any data from aborted blocks
    ///     (NOT aborted transactions that is handled by the clarity vm directly).
    /// The block header hash is used for identifying savepoints.
    ///     this _cannot_ be used to rollback to arbitrary prior block hash, because that
    ///     blockhash would already have committed and no longer exist in the save point stack.
    /// this is a "lower-level" rollback than the roll backs performed in
    ///   ClarityDatabase or AnalysisDatabase -- this is done at the backing store level.

    pub fn begin<'a>(
        &'a mut self,
        current: &StacksBlockId,
        next: &StacksBlockId,
    ) -> WritableMarfStore<'a> {
        let mut tx = self.marf.begin_tx().expect(&format!(
            "ERROR: Failed to begin new MARF block {} - {})",
            current, next
        ));
        tx.begin(current, next).expect(&format!(
            "ERROR: Failed to begin new MARF block {} - {})",
            current, next
        ));

        let chain_tip = tx
            .get_open_chain_tip()
            .expect("ERROR: Failed to get open MARF")
            .clone();

        WritableMarfStore {
            chain_tip,
            marf: tx,
        }
    }

    pub fn begin_unconfirmed<'a>(&'a mut self, current: &StacksBlockId) -> WritableMarfStore<'a> {
        let mut tx = self.marf.begin_tx().expect(&format!(
            "ERROR: Failed to begin new unconfirmed MARF block for {})",
            current
        ));
        tx.begin_unconfirmed(current).expect(&format!(
            "ERROR: Failed to begin new unconfirmed MARF block for {})",
            current
        ));

        let chain_tip = tx
            .get_open_chain_tip()
            .expect("ERROR: Failed to get open MARF")
            .clone();

        WritableMarfStore {
            chain_tip,
            marf: tx,
        }
    }

    pub fn get_chain_tip(&self) -> &StacksBlockId {
        &self.chain_tip
    }

    pub fn set_chain_tip(&mut self, bhh: &StacksBlockId) {
        self.chain_tip = bhh.clone();
    }

    // This function *should not* be called by
    //   a smart-contract, rather it should only be used by the VM
    pub fn get_root_hash(&mut self) -> TrieHash {
        self.marf
            .get_root_hash_at(&self.chain_tip)
            .expect("FATAL: Failed to read MARF root hash")
    }

    pub fn get_marf(&mut self) -> &mut MARF<StacksBlockId> {
        &mut self.marf
    }

    #[cfg(test)]
    pub fn sql_conn(&self) -> &Connection {
        self.marf.sqlite_conn()
    }

    pub fn index_conn<'a, C>(&'a self, context: C) -> IndexDBConn<'a, C, StacksBlockId> {
        IndexDBConn {
            index: &self.marf,
            context,
        }
    }
}

pub struct WritableMarfStore<'a> {
    chain_tip: StacksBlockId,
    marf: MarfTransaction<'a, StacksBlockId>,
}

pub struct ReadOnlyMarfStore<'a> {
    chain_tip: StacksBlockId,
    marf: &'a mut MARF<StacksBlockId>,
}

impl<'a> ReadOnlyMarfStore<'a> {
    pub fn as_clarity_db<'b>(
        &'b mut self,
        headers_db: &'b dyn HeadersDB,
        burn_state_db: &'b dyn BurnStateDB,
    ) -> ClarityDatabase<'b> {
        ClarityDatabase::new(self, headers_db, burn_state_db)
    }

    pub fn as_analysis_db<'b>(&'b mut self) -> AnalysisDatabase<'b> {
        AnalysisDatabase::new(self)
    }

    pub fn trie_exists_for_block(&mut self, bhh: &StacksBlockId) -> Result<bool, db_error> {
        self.marf.with_conn(|conn| match conn.has_block(bhh) {
            Ok(res) => Ok(res),
            Err(e) => Err(db_error::IndexError(e)),
        })
    }
}

impl<'a> ClarityBackingStore for ReadOnlyMarfStore<'a> {
    fn get_side_store(&mut self) -> &Connection {
        self.marf.sqlite_conn()
    }

    fn set_block_hash(&mut self, bhh: StacksBlockId) -> InterpreterResult<StacksBlockId> {
        self.marf
            .check_ancestor_block_hash(&bhh)
            .map_err(|e| match e {
                Error::NotFoundError => {
                    test_debug!("No such block {:?} (NotFoundError)", &bhh);
                    RuntimeErrorType::UnknownBlockHeaderHash(BlockHeaderHash(bhh.0))
                }
                Error::NonMatchingForks(_bh1, _bh2) => {
                    test_debug!(
                        "No such block {:?} (NonMatchingForks({}, {}))",
                        &bhh,
                        BlockHeaderHash(_bh1),
                        BlockHeaderHash(_bh2)
                    );
                    RuntimeErrorType::UnknownBlockHeaderHash(BlockHeaderHash(bhh.0))
                }
                _ => panic!("ERROR: Unexpected MARF failure: {}", e),
            })?;

        let result = Ok(self.chain_tip);
        self.chain_tip = bhh;

        result
    }

    fn get_current_block_height(&mut self) -> u32 {
        match self
            .marf
            .get_block_height_of(&self.chain_tip, &self.chain_tip)
        {
            Ok(Some(x)) => x,
            Ok(None) => {
                let first_tip = StacksBlockHeader::make_index_block_hash(
                    &FIRST_BURNCHAIN_CONSENSUS_HASH,
                    &FIRST_STACKS_BLOCK_HASH,
                );
                if self.chain_tip == first_tip || self.chain_tip == StacksBlockId([0u8; 32]) {
                    // the current block height should always work, except if it's the first block
                    // height (in which case, the current chain tip should match the first-ever
                    // index block hash).
                    return 0;
                }

                // should never happen
                let msg = format!(
                    "Failed to obtain current block height of {} (got None)",
                    &self.chain_tip
                );
                error!("{}", &msg);
                panic!("{}", &msg);
            }
            Err(e) => {
                let msg = format!(
                    "Unexpected MARF failure: Failed to get current block height of {}: {:?}",
                    &self.chain_tip, &e
                );
                error!("{}", &msg);
                panic!("{}", &msg);
            }
        }
    }

    fn get_block_at_height(&mut self, block_height: u32) -> Option<StacksBlockId> {
        self.marf
            .get_bhh_at_height(&self.chain_tip, block_height)
            .expect(&format!(
                "Unexpected MARF failure: failed to get block at height {} off of {}.",
                block_height, &self.chain_tip
            ))
            .map(|x| StacksBlockId(x.to_bytes()))
    }

    fn get_open_chain_tip(&mut self) -> StacksBlockId {
        StacksBlockId(
            self.marf
                .get_open_chain_tip()
                .expect("Attempted to get the open chain tip from an unopened context.")
                .clone()
                .to_bytes(),
        )
    }

    fn get_open_chain_tip_height(&mut self) -> u32 {
        self.marf
            .get_open_chain_tip_height()
            .expect("Attempted to get the open chain tip from an unopened context.")
    }

    fn get_with_proof(&mut self, key: &str) -> Option<(String, TrieMerkleProof<StacksBlockId>)> {
        self.marf
            .get_with_proof(&self.chain_tip, key)
            .or_else(|e| match e {
                Error::NotFoundError => Ok(None),
                _ => Err(e),
            })
            .expect("ERROR: Unexpected MARF Failure on GET")
            .map(|(marf_value, proof)| {
                let side_key = marf_value.to_hex();
                let data =
                    SqliteConnection::get(self.get_side_store(), &side_key).expect(&format!(
                        "ERROR: MARF contained value_hash not found in side storage: {}",
                        side_key
                    ));
                (data, proof)
            })
    }

    fn get(&mut self, key: &str) -> Option<String> {
        trace!("MarfedKV get: {:?} tip={}", key, &self.chain_tip);
        self.marf
            .get(&self.chain_tip, key)
            .or_else(|e| match e {
                Error::NotFoundError => {
                    trace!(
                        "MarfedKV get {:?} off of {:?}: not found",
                        key,
                        &self.chain_tip
                    );
                    Ok(None)
                }
                _ => Err(e),
            })
            .expect("ERROR: Unexpected MARF Failure on GET")
            .map(|marf_value| {
                let side_key = marf_value.to_hex();
                trace!("MarfedKV get side-key for {:?}: {:?}", key, &side_key);
                SqliteConnection::get(self.get_side_store(), &side_key).expect(&format!(
                    "ERROR: MARF contained value_hash not found in side storage: {}",
                    side_key
                ))
            })
    }

    fn put_all(&mut self, _items: Vec<(String, String)>) {
        error!("Attempted to commit changes to read-only MARF");
        panic!("BUG: attempted commit to read-only MARF");
    }
}

impl<'a> WritableMarfStore<'a> {
    pub fn as_clarity_db<'b>(
        &'b mut self,
        headers_db: &'b dyn HeadersDB,
        burn_state_db: &'b dyn BurnStateDB,
    ) -> ClarityDatabase<'b> {
        ClarityDatabase::new(self, headers_db, burn_state_db)
    }

    pub fn as_analysis_db<'b>(&'b mut self) -> AnalysisDatabase<'b> {
        AnalysisDatabase::new(self)
    }

    pub fn rollback_block(self) {
        self.marf.drop_current();
    }

    pub fn rollback_unconfirmed(self) {
        debug!("Drop unconfirmed MARF trie {}", &self.chain_tip);
        SqliteConnection::drop_metadata(self.marf.sqlite_tx(), &self.chain_tip);
        self.marf.drop_unconfirmed();
    }

    pub fn commit_to(self, final_bhh: &StacksBlockId) {
        debug!("commit_to({})", final_bhh);
        SqliteConnection::commit_metadata_to(self.marf.sqlite_tx(), &self.chain_tip, final_bhh);

        let _ = self.marf.commit_to(final_bhh).map_err(|e| {
            error!("Failed to commit to MARF block {}: {:?}", &final_bhh, &e);
            panic!();
        });
    }

    #[cfg(test)]
    pub fn test_commit(self) {
        let bhh = self.chain_tip.clone();
        self.commit_to(&bhh);
    }

    pub fn commit_unconfirmed(self) {
        debug!("commit_unconfirmed()");
        // NOTE: Can omit commit_metadata_to, since the block header hash won't change
        // commit_metadata_to(&self.chain_tip, final_bhh);
        self.marf
            .commit()
            .expect("ERROR: Failed to commit MARF block");
    }

    // This is used by miners
    //   so that the block validation and processing logic doesn't
    //   reprocess the same data as if it were already loaded
    pub fn commit_mined_block(self, will_move_to: &StacksBlockId) {
        debug!(
            "commit_mined_block: ({}->{})",
            &self.chain_tip, will_move_to
        );
        // rollback the side_store
        //    the side_store shouldn't commit data for blocks that won't be
        //    included in the processed chainstate (like a block constructed during mining)
        //    _if_ for some reason, we do want to be able to access that mined chain state in the future,
        //    we should probably commit the data to a different table which does not have uniqueness constraints.
        SqliteConnection::drop_metadata(self.marf.sqlite_tx(), &self.chain_tip);
        let _ = self.marf.commit_mined(will_move_to).map_err(|e| {
            error!(
                "Failed to commit to mined MARF block {}: {:?}",
                &will_move_to, &e
            );
            panic!();
        });
    }

    // This function *should not* be called by
    //   a smart-contract, rather it should only be used by the VM
    pub fn get_root_hash(&mut self) -> TrieHash {
        self.marf
            .get_root_hash_at(&self.chain_tip)
            .expect("FATAL: Failed to read MARF root hash")
    }
}

impl<'a> ClarityBackingStore for WritableMarfStore<'a> {
    fn set_block_hash(&mut self, bhh: StacksBlockId) -> InterpreterResult<StacksBlockId> {
        self.marf
            .check_ancestor_block_hash(&bhh)
            .map_err(|e| match e {
                Error::NotFoundError => {
                    test_debug!("No such block {:?} (NotFoundError)", &bhh);
                    RuntimeErrorType::UnknownBlockHeaderHash(BlockHeaderHash(bhh.0))
                }
                Error::NonMatchingForks(_bh1, _bh2) => {
                    test_debug!(
                        "No such block {:?} (NonMatchingForks({}, {}))",
                        &bhh,
                        BlockHeaderHash(_bh1),
                        BlockHeaderHash(_bh2)
                    );
                    RuntimeErrorType::UnknownBlockHeaderHash(BlockHeaderHash(bhh.0))
                }
                _ => panic!("ERROR: Unexpected MARF failure: {}", e),
            })?;

        let result = Ok(self.chain_tip);
        self.chain_tip = bhh;

        result
    }

    fn get(&mut self, key: &str) -> Option<String> {
        trace!("MarfedKV get: {:?} tip={}", key, &self.chain_tip);
        self.marf
            .get(&self.chain_tip, key)
            .or_else(|e| match e {
                Error::NotFoundError => {
                    trace!(
                        "MarfedKV get {:?} off of {:?}: not found",
                        key,
                        &self.chain_tip
                    );
                    Ok(None)
                }
                _ => Err(e),
            })
            .expect("ERROR: Unexpected MARF Failure on GET")
            .map(|marf_value| {
                let side_key = marf_value.to_hex();
                trace!("MarfedKV get side-key for {:?}: {:?}", key, &side_key);
                SqliteConnection::get(self.marf.sqlite_tx(), &side_key).expect(&format!(
                    "ERROR: MARF contained value_hash not found in side storage: {}",
                    side_key
                ))
            })
    }

    fn get_with_proof(&mut self, key: &str) -> Option<(String, TrieMerkleProof<StacksBlockId>)> {
        self.marf
            .get_with_proof(&self.chain_tip, key)
            .or_else(|e| match e {
                Error::NotFoundError => Ok(None),
                _ => Err(e),
            })
            .expect("ERROR: Unexpected MARF Failure on GET")
            .map(|(marf_value, proof)| {
                let side_key = marf_value.to_hex();
                let data =
                    SqliteConnection::get(self.marf.sqlite_tx(), &side_key).expect(&format!(
                        "ERROR: MARF contained value_hash not found in side storage: {}",
                        side_key
                    ));
                (data, proof)
            })
    }

    fn get_side_store(&mut self) -> &Connection {
        self.marf.sqlite_tx()
    }

    fn get_block_at_height(&mut self, height: u32) -> Option<StacksBlockId> {
        self.marf
            .get_block_at_height(height, &self.chain_tip)
            .expect(&format!(
                "Unexpected MARF failure: failed to get block at height {} off of {}.",
                height, &self.chain_tip
            ))
    }

    fn get_open_chain_tip(&mut self) -> StacksBlockId {
        self.marf
            .get_open_chain_tip()
            .expect("Attempted to get the open chain tip from an unopened context.")
            .clone()
    }

    fn get_open_chain_tip_height(&mut self) -> u32 {
        self.marf
            .get_open_chain_tip_height()
            .expect("Attempted to get the open chain tip from an unopened context.")
    }

    fn get_current_block_height(&mut self) -> u32 {
        match self
            .marf
            .get_block_height_of(&self.chain_tip, &self.chain_tip)
        {
            Ok(Some(x)) => x,
            Ok(None) => {
                let first_tip = StacksBlockHeader::make_index_block_hash(
                    &FIRST_BURNCHAIN_CONSENSUS_HASH,
                    &FIRST_STACKS_BLOCK_HASH,
                );
                if self.chain_tip == first_tip || self.chain_tip == StacksBlockId([0u8; 32]) {
                    // the current block height should always work, except if it's the first block
                    // height (in which case, the current chain tip should match the first-ever
                    // index block hash).
                    return 0;
                }

                // should never happen
                let msg = format!(
                    "Failed to obtain current block height of {} (got None)",
                    &self.chain_tip
                );
                error!("{}", &msg);
                panic!("{}", &msg);
            }
            Err(e) => {
                let msg = format!(
                    "Unexpected MARF failure: Failed to get current block height of {}: {:?}",
                    &self.chain_tip, &e
                );
                error!("{}", &msg);
                panic!("{}", &msg);
            }
        }
    }

    fn put_all(&mut self, items: Vec<(String, String)>) {
        let mut keys = Vec::new();
        let mut values = Vec::new();
        for (key, value) in items.into_iter() {
            trace!("MarfedKV put '{}' = '{}'", &key, &value);
            let marf_value = MARFValue::from_value(&value);
            SqliteConnection::put(self.get_side_store(), &marf_value.to_hex(), &value);
            keys.push(key);
            values.push(marf_value);
        }
        self.marf
            .insert_batch(&keys, values)
            .expect("ERROR: Unexpected MARF Failure");
    }
}
