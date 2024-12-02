// Copyright 2024 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

use super::address::TransactionAddress;
use serde::{Deserialize, Serialize};

// re-exports
pub use bls::{PublicKey, Signature};

/// Content of a transaction, limited to 32 bytes
pub type TransactionContent = [u8; 32];

/// A generic Transaction on the Network
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Hash, Ord, PartialOrd)]
pub struct Transaction {
    pub owner: PublicKey,
    pub parent: Vec<PublicKey>,
    pub content: TransactionContent,
    pub outputs: Vec<(PublicKey, TransactionContent)>,
    /// signs the above 4 fields with the owners key
    pub signature: Signature,
}

impl Transaction {
    pub fn new(
        owner: PublicKey,
        parent: Vec<PublicKey>,
        content: TransactionContent,
        outputs: Vec<(PublicKey, TransactionContent)>,
        signature: Signature,
    ) -> Self {
        Self {
            owner,
            parent,
            content,
            outputs,
            signature,
        }
    }

    pub fn address(&self) -> TransactionAddress {
        TransactionAddress::from_owner(self.owner)
    }

    pub fn bytes_for_signature(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&self.owner.to_bytes());
        bytes.extend_from_slice("parent".as_bytes());
        bytes.extend_from_slice(
            &self
                .parent
                .iter()
                .map(|p| p.to_bytes())
                .collect::<Vec<_>>()
                .concat(),
        );
        bytes.extend_from_slice("content".as_bytes());
        bytes.extend_from_slice(&self.content);
        bytes.extend_from_slice("outputs".as_bytes());
        bytes.extend_from_slice(
            &self
                .outputs
                .iter()
                .flat_map(|(p, c)| [&p.to_bytes(), c.as_slice()].concat())
                .collect::<Vec<_>>(),
        );
        bytes
    }

    pub fn verify(&self) -> bool {
        self.owner
            .verify(&self.signature, self.bytes_for_signature())
    }
}
