use std::collections::{BTreeMap, BTreeSet};

use penumbra_crypto::{ka, merkle, note, Nullifier};
use penumbra_stake::{Delegate, IdentityKey, Undelegate, Validator};

mod stateful;
mod stateless;

// TODO: eliminate (#374)
pub use stateful::mark_genesis_as_verified;
pub use stateless::StatelessTransactionExt;

#[cfg(test)]
mod tests;

#[derive(Debug, Clone)]
pub struct NoteData {
    pub ephemeral_key: ka::Public,
    pub encrypted_note: [u8; note::NOTE_CIPHERTEXT_BYTES],
    pub transaction_id: [u8; 32],
}

#[derive(Debug, Clone)]
pub struct PositionedNoteData {
    pub position: u64,
    pub data: NoteData,
}

/// `PendingTransaction` holds data after stateless checks have been applied.
/// TODO this is a bad name
pub struct PendingTransaction {
    /// Transaction ID.
    pub id: [u8; 32],
    /// Root of the note commitment tree.
    pub root: merkle::Root,
    /// Note data to add from outputs in this transaction.
    pub new_notes: BTreeMap<note::Commitment, NoteData>,
    /// List of spent nullifiers from spends in this transaction.
    pub spent_nullifiers: BTreeSet<Nullifier>,
    /// Delegations performed in this transaction.
    pub delegations: Vec<Delegate>,
    /// Undelegations performed in this transaction.
    pub undelegations: Vec<Undelegate>,
    /// Validators defined in the transaction.
    pub validators: Vec<Validator>,
}

/// `VerifiedTransaction` represents a transaction after all checks have passed.
/// TODO this is a bad name
pub struct VerifiedTransaction {
    /// Transaction ID.
    pub id: [u8; 32],
    /// Note data to add from outputs in this transaction.
    pub new_notes: BTreeMap<note::Commitment, NoteData>,
    /// List of spent nullifiers from spends in this transaction.
    pub spent_nullifiers: BTreeSet<Nullifier>,
    /// Net delegations performed in this transaction.
    pub delegation_changes: BTreeMap<IdentityKey, i64>,
}
