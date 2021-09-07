// Copyright (C) 2019-2021 Aleo Systems Inc.
// This file is part of the snarkOS library.

// The snarkOS library is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// The snarkOS library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with the snarkOS library. If not, see <https://www.gnu.org/licenses/>.

use std::sync::Arc;

use crate::{
    error::ConsensusError,
    memory_pool::MempoolEntry,
    Consensus,
    ConsensusParameters,
    CreatePartialTransactionRequest,
    DynLedger,
    MemoryPool,
};
use anyhow::*;
use snarkos_storage::{
    BlockFilter,
    BlockOrder,
    BlockStatus,
    Digest,
    DynStorage,
    ForkDescription,
    SerialBlock,
    SerialTransaction,
    VMTransaction,
};
use snarkvm_dpc::{
    testnet1::{instantiated::Components, Record as DPCRecord, TransactionKernel},
    DPCScheme,
};
use snarkvm_posw::txids_to_roots;
use snarkvm_utilities::has_duplicates;
use tokio::sync::mpsc;

use snarkos_metrics::misc::*;

use rand::thread_rng;

use super::message::{ConsensusMessage, CreateTransactionRequest, TransactionResponse};

mod agent;
mod commit;
mod transaction;

pub struct ConsensusInner {
    pub public: Arc<Consensus>,
    pub ledger: DynLedger,
    pub memory_pool: MemoryPool,
    pub storage: DynStorage,
}

impl ConsensusInner {
    /// scans uncommitted blocks with a known path to the canon chain for forks
    async fn scan_forks(&mut self) -> Result<Vec<(Digest, Digest)>> {
        let canon_hashes = self
            .storage
            .get_block_hashes(
                Some(crate::OLDEST_FORK_THRESHOLD as u32),
                BlockFilter::CanonOnly(BlockOrder::Descending),
            )
            .await?;

        if canon_hashes.len() < 2 {
            // windows will panic if len < 2
            return Ok(vec![]);
        }

        let mut known_forks = vec![];

        for canon_hashes in canon_hashes.windows(2) {
            // windows will ignore last block (furthest down), so we pull one extra above
            let target_hash = &canon_hashes[1];
            let ignore_child_hash = &canon_hashes[0];
            let children = self.storage.get_block_children(target_hash).await?;
            if children.len() == 1 && &children[0] == ignore_child_hash {
                continue;
            }
            for child in children {
                if &child != ignore_child_hash {
                    known_forks.push((target_hash.clone(), child));
                }
            }
        }

        Ok(known_forks)
    }

    /// Adds entry to memory pool if valid in the current ledger.
    pub(crate) fn insert_into_mempool(
        &mut self,
        transaction: SerialTransaction,
    ) -> Result<Option<Digest>, ConsensusError> {
        let transaction_id: Digest = transaction.id.into();

        if has_duplicates(&transaction.old_serial_numbers)
            || has_duplicates(&transaction.new_commitments)
            || self.memory_pool.transactions.contains_key(&transaction_id)
        {
            return Ok(None);
        }

        for sn in &transaction.old_serial_numbers {
            if self.ledger.contains_serial(sn) || self.memory_pool.serial_numbers.contains(sn) {
                return Ok(None);
            }
        }

        for cm in &transaction.new_commitments {
            if self.ledger.contains_commitment(cm) || self.memory_pool.commitments.contains(cm) {
                return Ok(None);
            }
        }

        if self.ledger.contains_memo(&transaction.memorandum)
            || self.memory_pool.memos.contains(&transaction.memorandum)
        {
            return Ok(None);
        }

        for sn in &transaction.old_serial_numbers {
            self.memory_pool.serial_numbers.insert(sn.clone());
        }

        for cm in &transaction.new_commitments {
            self.memory_pool.commitments.insert(cm.clone());
        }

        self.memory_pool.memos.insert(transaction.memorandum.clone());

        self.memory_pool
            .transactions
            .insert(transaction_id.clone(), MempoolEntry {
                size_in_bytes: transaction.size(),
                transaction,
            });

        Ok(Some(transaction_id))
    }

    /// Cleanse the memory pool of outdated transactions.
    pub(crate) fn cleanse_memory_pool(&mut self) -> Result<(), ConsensusError> {
        let old_mempool = std::mem::take(&mut self.memory_pool);

        for (_, entry) in &old_mempool.transactions {
            if let Err(e) = self.insert_into_mempool(entry.transaction.clone()) {
                self.memory_pool = old_mempool;
                return Err(e);
            }
        }

        Ok(())
    }
}
