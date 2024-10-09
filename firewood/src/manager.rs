// Copyright (C) 2023, Ava Labs, Inc. All rights reserved.
// See the file LICENSE.md for licensing terms.

#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::io::Error;
use std::num::NonZero;
use std::path::PathBuf;
use std::sync::Arc;

use storage::logger::warn;
use typed_builder::TypedBuilder;

use crate::v2::api::HashKey;

use storage::{Committed, FileBacked, ImmutableProposal, NodeStore, Parentable, TrieHash};

#[derive(Clone, Debug, TypedBuilder)]
pub struct RevisionManagerConfig {
    /// The number of historical revisions to keep in memory.
    #[builder(default = 128)]
    max_revisions: usize,

    #[builder(default_code = "NonZero::new(20480).expect(\"non-zero\")")]
    node_cache_size: NonZero<usize>,

    #[builder(default_code = "NonZero::new(10000).expect(\"non-zero\")")]
    free_list_cache_size: NonZero<usize>,
}

type CommittedRevision = Arc<NodeStore<Committed, FileBacked>>;
type ProposedRevision = Arc<NodeStore<Arc<ImmutableProposal>, FileBacked>>;

#[derive(Debug)]
pub(crate) struct RevisionManager {
    /// Maximum number of revisions to keep on disk
    max_revisions: usize,

    /// The underlying file storage
    filebacked: Arc<FileBacked>,

    /// The list of revisions that are on disk; these point to the different roots
    /// stored in the filebacked storage.
    historical: VecDeque<CommittedRevision>,
    proposals: Vec<ProposedRevision>,
    // committing_proposals: VecDeque<Arc<ProposedImmutable>>,
    by_hash: HashMap<TrieHash, CommittedRevision>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum RevisionManagerError {
    #[error("The proposal cannot be committed since a sibling was committed")]
    SiblingCommitted,
    #[error(
        "The proposal cannot be committed since it is not a direct child of the most recent commit"
    )]
    NotLatest,
    #[error("An IO error occurred during the commit")]
    IO(#[from] std::io::Error),
}

impl RevisionManager {
    pub fn new(
        filename: PathBuf,
        truncate: bool,
        config: RevisionManagerConfig,
    ) -> Result<Self, Error> {
        let storage = Arc::new(FileBacked::new(
            filename,
            config.node_cache_size,
            config.free_list_cache_size,
            truncate,
        )?);
        let nodestore = match truncate {
            true => Arc::new(NodeStore::new_empty_committed(storage.clone())?),
            false => Arc::new(NodeStore::open(storage.clone())?),
        };
        let mut manager = Self {
            max_revisions: config.max_revisions,
            filebacked: storage,
            historical: VecDeque::from([nodestore.clone()]),
            by_hash: Default::default(),
            proposals: Default::default(),
            // committing_proposals: Default::default(),
        };
        if nodestore.kind.root_hash().is_some() {
            manager.by_hash.insert(
                nodestore.kind.root_hash().expect("root hash is present"),
                nodestore.clone(),
            );
        }

        if truncate {
            nodestore.flush_header_with_padding()?;
        }

        Ok(manager)
    }

    pub fn all_hashes(&self) -> Vec<TrieHash> {
        self.historical
            .iter()
            .filter_map(|r| r.kind.root_hash())
            .chain(self.proposals.iter().filter_map(|p| p.kind.root_hash()))
            .collect()
    }

    /// Commit a proposal
    /// To commit a proposal involves a few steps:
    /// 1. Commit check.
    ///    The proposal’s parent must be the last committed revision, otherwise the commit fails.
    /// 2. Persist delete list.
    ///    The list of all nodes that were to be deleted for this proposal must be fully flushed to disk.
    ///    The address of the root node and the root hash is also persisted.
    ///    Note that this is *not* a write ahead log.
    ///    It only contains the address of the nodes that are deleted, which should be very small.
    /// 3. Revision reaping. If more than the maximum number of revisions are kept in memory, the
    ///    oldest revision is reaped.
    /// 4. Set last committed revision.
    ///    Set last committed revision in memory.
    ///    Another commit can start after this but before the node flush is completed.
    /// 5. Free list flush.
    ///    Persist/write the free list header.
    ///    The free list is flushed first to prevent future allocations from using the space allocated to this proposal.
    ///    This should be done in a single write since the free list headers are small, and must be persisted to disk before starting the next step.
    /// 6. Node flush.
    ///    Persist/write all the nodes to disk.
    ///    Note that since these are all freshly allocated nodes, they will never be referred to by any prior commit.
    ///    After flushing all nodes, the file should be flushed to disk (fsync) before performing the next step.
    /// 7. Root move.
    ///    The root address on disk must be updated.
    ///    This write can be delayed, but would mean that recovery will not roll forward to this revision.
    /// 8. Proposal Cleanup.
    ///    Any other proposals that have this proposal as a parent should be reparented to the committed version.
    pub fn commit(&mut self, proposal: ProposedRevision) -> Result<(), RevisionManagerError> {
        // 1. Commit check
        let current_revision = self.current_revision();
        if !proposal
            .kind
            .parent_hash_is(current_revision.kind.root_hash())
        {
            return Err(RevisionManagerError::NotLatest);
        }

        let committed = proposal.as_committed();

        // 2. Persist delete list for this committed revision to disk for recovery

        // 3 Take the deleted entries from the oldest revision and mark them as free for this revision
        // If you crash after freeing some of these, then the free list will point to nodes that are not actually free.
        // TODO: Handle the case where we get something off the free list that is not free
        while self.historical.len() >= self.max_revisions {
            let oldest = self.historical.pop_front().expect("must be present");
            if let Some(oldest_hash) = oldest.kind.root_hash() {
                self.by_hash.remove(&oldest_hash);
            }

            // This `try_unwrap` is safe because nobody else will call `try_unwrap` on this Arc
            // in a different thread, so we don't have to worry about the race condition where
            // the Arc we get back is not usable as indicated in the docs for `try_unwrap`.
            // This guarantee is there because we have a `&mut self` reference to the manager, so
            // the compiler guarantees we are the only one using this manager.
            match Arc::try_unwrap(oldest) {
                Ok(oldest) => oldest.reap_deleted()?,
                Err(original) => {
                    warn!("Oldest revision could not be reaped; still referenced");
                    self.historical.push_front(original);
                }
            }
        }

        // 4. Set last committed revision
        let committed: CommittedRevision = committed.into();
        self.historical.push_back(committed.clone());
        if let Some(hash) = committed.kind.root_hash() {
            self.by_hash.insert(hash, committed.clone());
        }
        // TODO: We could allow other commits to start here using the pending list

        // 5. Free list flush, which will prevent allocating on top of the nodes we are about to write
        proposal.flush_freelist()?;

        // 6. Node flush
        proposal.flush_nodes()?;

        // 7. Root move
        proposal.flush_header()?;

        // 8. Proposal Cleanup
        // first remove the committing proposal from the list of outstanding proposals
        self.proposals.retain(|p| !Arc::ptr_eq(&proposal, p));

        // then reparent any proposals that have this proposal as a parent
        for p in self.proposals.iter() {
            proposal.commit_reparent(p);
        }

        Ok(())
    }
}

impl RevisionManager {
    pub fn add_proposal(&mut self, proposal: ProposedRevision) {
        self.proposals.push(proposal);
    }

    pub fn revision(&self, root_hash: HashKey) -> Result<CommittedRevision, RevisionManagerError> {
        self.by_hash
            .get(&root_hash)
            .cloned()
            .ok_or(RevisionManagerError::IO(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Revision not found",
            )))
    }

    pub fn root_hash(&self) -> Result<Option<HashKey>, RevisionManagerError> {
        self.current_revision()
            .kind
            .root_hash()
            .map(Option::Some)
            .ok_or(RevisionManagerError::IO(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Root hash not found",
            )))
    }

    pub fn current_revision(&self) -> CommittedRevision {
        self.historical
            .back()
            .expect("there is always one revision")
            .clone()
    }
}

#[cfg(test)]
mod tests {
    // TODO
}