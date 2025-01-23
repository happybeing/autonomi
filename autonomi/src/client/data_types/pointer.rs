// Copyright 2025 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

use crate::client::{payment::PayError, quote::CostError, Client};
use ant_evm::{Amount, AttoTokens, EvmWallet, EvmWalletError};
use ant_networking::{GetRecordCfg, GetRecordError, NetworkError, PutRecordCfg, VerificationKind};
use ant_protocol::{
    storage::{
        try_deserialize_record, try_serialize_record, DataTypes, RecordHeader, RecordKind,
        RetryStrategy,
    },
    NetworkAddress,
};
use bls::{PublicKey, SecretKey};
use libp2p::kad::{Quorum, Record};
use tracing::{debug, error, trace};

pub use ant_protocol::storage::{Pointer, PointerAddress, PointerTarget};

#[derive(Debug, thiserror::Error)]
pub enum PointerError {
    #[error("Cost error: {0}")]
    Cost(#[from] CostError),
    #[error("Network error")]
    Network(#[from] NetworkError),
    #[error("Serialization error")]
    Serialization,
    #[error("Pointer record corrupt: {0}")]
    Corrupt(String),
    #[error("Payment failure occurred during pointer creation.")]
    Pay(#[from] PayError),
    #[error("Failed to retrieve wallet payment")]
    Wallet(#[from] EvmWalletError),
    #[error("Received invalid quote from node, this node is possibly malfunctioning, try another node by trying another pointer name")]
    InvalidQuote,
    #[error("Pointer already exists at this address: {0:?}")]
    PointerAlreadyExists(PointerAddress),
    #[error("Pointer cannot be updated as it does not exist, please create it first or wait for it to be created")]
    CannotUpdateNewPointer,
}

impl Client {
    /// Get a pointer from the network
    pub async fn pointer_get(&self, address: PointerAddress) -> Result<Pointer, PointerError> {
        info!("Getting pointer: {address:?}");

        let key = NetworkAddress::from_pointer_address(address).to_record_key();
        debug!("Fetching pointer from network at: {key:?}");
        let get_cfg = GetRecordCfg {
            get_quorum: Quorum::Majority,
            retry_strategy: Some(RetryStrategy::Balanced),
            target_record: None,
            expected_holders: Default::default(),
        };

        let record = self
            .network
            .get_record_from_network(key.clone(), &get_cfg)
            .await
            .inspect_err(|err| error!("Error fetching pointer: {err:?}"))?;
        let header = RecordHeader::from_record(&record).map_err(|err| {
            PointerError::Corrupt(format!(
                "Failed to parse record header for pointer at {key:?}: {err:?}"
            ))
        })?;

        if matches!(header.kind, RecordKind::DataOnly(DataTypes::Pointer)) {
            let pointer: Pointer = try_deserialize_record(&record).map_err(|err| {
                PointerError::Corrupt(format!(
                    "Failed to parse record for pointer at {key:?}: {err:?}"
                ))
            })?;
            Ok(pointer)
        } else {
            error!(
                "Record kind mismatch: expected Pointer, got {:?}",
                header.kind
            );
            Err(NetworkError::RecordKindMismatch(RecordKind::DataOnly(DataTypes::Pointer)).into())
        }
    }

    /// Store a pointer on the network
    pub async fn pointer_put(
        &self,
        pointer: Pointer,
        wallet: &EvmWallet,
    ) -> Result<(AttoTokens, PointerAddress), PointerError> {
        let address = pointer.network_address();

        // pay for the pointer storage
        let xor_name = *address.xorname();
        debug!("Paying for pointer at address: {address:?}");
        let (payment_proofs, _skipped_payments) = self
            // TODO: define Pointer default size for pricing
            .pay(
                DataTypes::Pointer.get_index(),
                std::iter::once((xor_name, 128)),
                wallet,
            )
            .await
            .inspect_err(|err| {
                error!("Failed to pay for pointer at address: {address:?} : {err}")
            })?;

        // verify payment was successful
        let (proof, price) = match payment_proofs.get(&xor_name) {
            Some((proof, price)) => (Some(proof), price),
            None => {
                info!("Pointer at address: {address:?} was already paid for, update is free");
                (None, &AttoTokens::zero())
            }
        };
        let total_cost = *price;

        let (record, payees) = if let Some(proof) = proof {
            let payees = Some(proof.payees());
            let record = Record {
                key: NetworkAddress::from_pointer_address(address).to_record_key(),
                value: try_serialize_record(
                    &(proof, &pointer),
                    RecordKind::DataWithPayment(DataTypes::Pointer),
                )
                .map_err(|_| PointerError::Serialization)?
                .to_vec(),
                publisher: None,
                expires: None,
            };
            (record, payees)
        } else {
            let record = Record {
                key: NetworkAddress::from_pointer_address(address).to_record_key(),
                value: try_serialize_record(&pointer, RecordKind::DataOnly(DataTypes::Pointer))
                    .map_err(|_| PointerError::Serialization)?
                    .to_vec(),
                publisher: None,
                expires: None,
            };
            (record, None)
        };

        let get_cfg = GetRecordCfg {
            get_quorum: Quorum::Majority,
            retry_strategy: Some(RetryStrategy::default()),
            target_record: None,
            expected_holders: Default::default(),
        };

        let put_cfg = PutRecordCfg {
            put_quorum: Quorum::All,
            retry_strategy: None,
            verification: Some((VerificationKind::Crdt, get_cfg)),
            use_put_record_to: payees,
        };

        // store the pointer on the network
        debug!("Storing pointer at address {address:?} to the network");
        self.network
            .put_record(record, &put_cfg)
            .await
            .inspect_err(|err| {
                error!("Failed to put record - pointer {address:?} to the network: {err}")
            })?;

        Ok((total_cost, address))
    }

    /// Create a new pointer on the network
    /// Make sure that the owner key is not already used for another pointer as each key is associated with one pointer
    pub async fn pointer_create(
        &self,
        owner: &SecretKey,
        target: PointerTarget,
        wallet: &EvmWallet,
    ) -> Result<(AttoTokens, PointerAddress), PointerError> {
        let address = PointerAddress::from_owner(owner.public_key());
        let already_exists = match self.pointer_get(address).await {
            Ok(_) => true,
            Err(PointerError::Network(NetworkError::GetRecordError(
                GetRecordError::SplitRecord { .. },
            ))) => true,
            Err(PointerError::Network(NetworkError::GetRecordError(
                GetRecordError::RecordNotFound,
            ))) => false,
            Err(err) => return Err(err),
        };

        if already_exists {
            return Err(PointerError::PointerAlreadyExists(address));
        }

        let pointer = Pointer::new(owner, 0, target);
        self.pointer_put(pointer, wallet).await
    }

    /// Update an existing pointer to point to a new target on the network
    /// The pointer needs to be created first with [`Client::pointer_put`]
    /// This operation is free as the pointer was already paid for at creation
    pub async fn pointer_update(
        &self,
        owner: &SecretKey,
        target: PointerTarget,
    ) -> Result<(), PointerError> {
        let address = PointerAddress::from_owner(owner.public_key());
        let current = match self.pointer_get(address).await {
            Ok(pointer) => Some(pointer),
            Err(PointerError::Network(NetworkError::GetRecordError(
                GetRecordError::RecordNotFound,
            ))) => None,
            Err(PointerError::Network(NetworkError::GetRecordError(
                GetRecordError::SplitRecord { result_map },
            ))) => result_map
                .values()
                .filter_map(|(record, _)| try_deserialize_record::<Pointer>(record).ok())
                .max_by_key(|pointer: &Pointer| pointer.counter()),
            Err(err) => {
                return Err(err);
            }
        };

        let pointer = if let Some(p) = current {
            let version = p.counter() + 1;
            Pointer::new(owner, version, target)
        } else {
            warn!("Pointer at address {address:?} cannot be updated as it does not exist, please create it first or wait for it to be created");
            return Err(PointerError::CannotUpdateNewPointer);
        };

        // prepare the record to be stored
        let record = Record {
            key: NetworkAddress::from_pointer_address(address).to_record_key(),
            value: try_serialize_record(&pointer, RecordKind::DataOnly(DataTypes::Pointer))
                .map_err(|_| PointerError::Serialization)?
                .to_vec(),
            publisher: None,
            expires: None,
        };
        let get_cfg = GetRecordCfg {
            get_quorum: Quorum::Majority,
            retry_strategy: Some(RetryStrategy::default()),
            target_record: None,
            expected_holders: Default::default(),
        };
        let put_cfg = PutRecordCfg {
            put_quorum: Quorum::All,
            retry_strategy: None,
            verification: Some((VerificationKind::Crdt, get_cfg)),
            use_put_record_to: None,
        };

        // store the pointer on the network
        debug!("Updating pointer at address {address:?} to the network");
        self.network
            .put_record(record, &put_cfg)
            .await
            .inspect_err(|err| {
                error!("Failed to update pointer at address {address:?} to the network: {err}")
            })?;

        Ok(())
    }

    /// Calculate the cost of storing a pointer
    pub async fn pointer_cost(&self, key: PublicKey) -> Result<AttoTokens, PointerError> {
        trace!("Getting cost for pointer of {key:?}");

        let address = PointerAddress::from_owner(key);
        let xor = *address.xorname();
        // TODO: define default size of Pointer
        let store_quote = self
            .get_store_quotes(DataTypes::Pointer.get_index(), std::iter::once((xor, 128)))
            .await?;
        let total_cost = AttoTokens::from_atto(
            store_quote
                .0
                .values()
                .map(|quote| quote.price())
                .sum::<Amount>(),
        );
        debug!("Calculated the cost to create pointer of {key:?} is {total_cost}");
        Ok(total_cost)
    }
}
