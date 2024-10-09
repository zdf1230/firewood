// Copyright (C) 2023, Ava Labs, Inc. All rights reserved.
// See the file LICENSE.md for licensing terms.

use serde::{ser::SerializeStruct as _, Deserialize, Serialize};
use smallvec::SmallVec;

use crate::{LeafNode, LinearAddress, Node, Path, TrieHash};
use std::fmt::{Debug, Error as FmtError, Formatter};

#[derive(PartialEq, Eq, Clone, Debug)]
/// A child of a branch node.
pub enum Child {
    /// There is a child at this index, but we haven't hashed it
    /// or allocated space in storage for it yet.
    Node(Node),
    /// We know the child's address and hash.
    AddressWithHash(LinearAddress, TrieHash),
}

#[derive(PartialEq, Eq, Clone)]
/// A branch node
pub struct BranchNode {
    /// The partial path for this branch
    pub partial_path: Path,

    /// The value of the data for this branch, if any
    pub value: Option<Box<[u8]>>,

    /// The children of this branch.
    /// Element i is the child at index i, or None if there is no child at that index.
    /// Each element is (child_hash, child_address).
    /// child_address is None if we don't know the child's hash.
    pub children: [Option<Child>; Self::MAX_CHILDREN],
}

impl Serialize for BranchNode {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut state = serializer.serialize_struct("BranchNode", 3)?;
        state.serialize_field("partial_path", &self.partial_path)?;
        state.serialize_field("value", &self.value)?;

        let children: SmallVec<[(u8, LinearAddress, TrieHash); 16]> = self
            .children
            .iter()
            .enumerate()
            .filter_map(|(offset, child)| match child {
                None => None,
                Some(Child::Node(_)) => {
                    panic!("serializing in-memory node for disk storage")
                }
                Some(Child::AddressWithHash(addr, hash)) => {
                    Some((offset as u8, *addr, (*hash).clone()))
                }
            })
            .collect();

        state.serialize_field("children", &children)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for BranchNode {
    fn deserialize<D>(deserializer: D) -> Result<BranchNode, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct SerializedBranchNode {
            partial_path: Path,
            value: Option<Box<[u8]>>,
            children: SmallVec<[(u8, LinearAddress, TrieHash); 16]>,
        }

        let s: SerializedBranchNode = Deserialize::deserialize(deserializer)?;

        let mut children: [Option<Child>; BranchNode::MAX_CHILDREN] = Default::default();
        for (offset, addr, hash) in s.children.iter() {
            children[*offset as usize] = Some(Child::AddressWithHash(*addr, hash.clone()));
        }

        Ok(BranchNode {
            partial_path: s.partial_path,
            value: s.value,
            children,
        })
    }
}

impl Debug for BranchNode {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), FmtError> {
        write!(f, "[Branch")?;
        write!(f, r#" path="{:?}""#, self.partial_path)?;

        for (i, c) in self.children.iter().enumerate() {
            match c {
                None => {}
                Some(Child::Node(_)) => {} //TODO
                Some(Child::AddressWithHash(addr, hash)) => write!(
                    f,
                    "(index: {i:?}), address={addr:?}, hash={:?})",
                    hex::encode(hash),
                )?,
            }
        }

        write!(
            f,
            " v={}]",
            match &self.value {
                Some(v) => hex::encode(&**v),
                None => "nil".to_string(),
            }
        )
    }
}

impl BranchNode {
    /// The maximum number of children in a [BranchNode]
    pub const MAX_CHILDREN: usize = 16;

    /// Returns the address of the child at the given index.
    /// Panics if `child_index` >= [BranchNode::MAX_CHILDREN].
    pub fn child(&self, child_index: u8) -> &Option<Child> {
        self.children
            .get(child_index as usize)
            .expect("child_index is in bounds")
    }

    /// Update the child at `child_index` to be `new_child_addr`.
    /// If `new_child_addr` is None, the child is removed.
    pub fn update_child(&mut self, child_index: u8, new_child: Option<Child>) {
        let child = self
            .children
            .get_mut(child_index as usize)
            .expect("child_index is in bounds");

        *child = new_child;
    }

    /// Returns (index, hash) for each child that has a hash set.
    pub fn children_iter(&self) -> impl Iterator<Item = (usize, &TrieHash)> + Clone {
        self.children.iter().enumerate().filter_map(
            #[allow(clippy::indexing_slicing)]
            |(i, child)| match child {
                None => None,
                Some(Child::Node(_)) => unreachable!("TODO make unreachable"),
                Some(Child::AddressWithHash(_, hash)) => Some((i, hash)),
            },
        )
    }
}

impl From<&LeafNode> for BranchNode {
    fn from(leaf: &LeafNode) -> Self {
        BranchNode {
            partial_path: leaf.partial_path.clone(),
            value: Some(Box::from(&leaf.value[..])),
            children: Default::default(),
        }
    }
}