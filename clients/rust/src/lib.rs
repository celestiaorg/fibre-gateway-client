//! Client side of the Fibre gateway `/v1/put` and `/v1/get` API.
//!
//! [`verify`] checks that a put receipt commits to the caller's own bytes. It
//! recomputes the commitment from the data and the two row tree siblings that
//! `POST /v1/put` returns, so it trusts nothing else the gateway says.

#![forbid(unsafe_code)]

#[cfg(feature = "http")]
mod http;

#[cfg(feature = "http")]
pub use http::{CapacityStatus, Client, HttpError, Timeouts};

use rsema1d::codec::compute_rlc;
use rsema1d::crypto::{derive_coefficients, hash_internal, hash_leaf, sha256, MerkleTree};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

/// Original rows (K) of blob version 0.
pub const ORIGINAL_ROWS: usize = 4096;
/// Parity rows (N) of blob version 0.
pub const PARITY_ROWS: usize = 12288;
/// Row tree siblings from the original rows' subtree root up to the row root.
pub const ROW_ROOT_SIBLINGS: usize = 2;
/// Blob header: version byte and big-endian u32 data length.
pub const BLOB_HEADER_LEN: usize = 5;
/// Largest blob version 0 data size.
pub const MAX_DATA_SIZE: usize = (1 << 31) - BLOB_HEADER_LEN;
/// Blob ID length: version byte and 32-byte commitment.
pub const BLOB_ID_LEN: usize = 33;

const MIN_ROW_SIZE: usize = 64;

/// The `/v1/put` receipt. `/v1/get` takes its `blob_id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub chain_id: String,
    pub tx_hash: String,
    pub blob_id: String,
    pub promise_height: u64,
}

/// Returned by `POST /v1/put`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitmentProof {
    /// Hex row tree nodes from the original rows' subtree root up to the row root, lowest first.
    pub row_root_siblings: Vec<String>,
}

/// The `/v1/put` response. `/v1/get` needs only its `blob_id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PutResponse {
    #[serde(flatten)]
    pub receipt: Receipt,
    pub commitment_proof: CommitmentProof,
}

/// Why a commitment did not verify.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VerifyError {
    #[error("invalid blob id")]
    InvalidBlobId,
    #[error("unsupported blob version {0}")]
    UnsupportedVersion(u8),
    #[error("data size {0} out of range")]
    InvalidDataSize(usize),
    #[error("expected {ROW_ROOT_SIBLINGS} row root siblings of 32 bytes")]
    InvalidProof,
    #[error("commitment does not match data")]
    Mismatch,
}

/// Checks that the receipt's blob ID commits to `data`.
pub fn verify(data: &[u8], receipt: &Receipt, proof: &CommitmentProof) -> Result<(), VerifyError> {
    let blob_id = parse_blob_id(&receipt.blob_id)?;
    if proof.row_root_siblings.len() != ROW_ROOT_SIBLINGS {
        return Err(VerifyError::InvalidProof);
    }
    let mut siblings = [[0u8; 32]; ROW_ROOT_SIBLINGS];
    for (dst, s) in siblings.iter_mut().zip(&proof.row_root_siblings) {
        hex::decode_to_slice(s, dst).map_err(|_| VerifyError::InvalidProof)?;
    }
    verify_commitment(data, &blob_id, &siblings)
}

/// Checks that `blob_id` commits to `data` given the row root siblings.
pub fn verify_commitment(
    data: &[u8],
    blob_id: &[u8; BLOB_ID_LEN],
    siblings: &[[u8; 32]],
) -> Result<(), VerifyError> {
    if blob_id[0] != 0 {
        return Err(VerifyError::UnsupportedVersion(blob_id[0]));
    }
    if data.is_empty() || data.len() > MAX_DATA_SIZE {
        return Err(VerifyError::InvalidDataSize(data.len()));
    }
    if siblings.len() != ROW_ROOT_SIBLINGS {
        return Err(VerifyError::InvalidProof);
    }
    let roots = commitment_roots(data, siblings);
    let mut pair = [0u8; 64];
    pair[..32].copy_from_slice(&roots.row_root);
    pair[32..].copy_from_slice(&roots.rlc_root);
    if sha256(&pair) != blob_id[1..] {
        return Err(VerifyError::Mismatch);
    }
    Ok(())
}

/// Decodes a hex blob ID.
pub fn parse_blob_id(s: &str) -> Result<[u8; BLOB_ID_LEN], VerifyError> {
    let mut id = [0u8; BLOB_ID_LEN];
    hex::decode_to_slice(s, &mut id).map_err(|_| VerifyError::InvalidBlobId)?;
    Ok(id)
}

/// Row size for `data_len` bytes of blob version 0 data.
pub fn row_size(data_len: usize) -> usize {
    (data_len + BLOB_HEADER_LEN)
        .div_ceil(ORIGINAL_ROWS)
        .next_multiple_of(MIN_ROW_SIZE)
}

/// The row and RLC roots that `data` and `siblings` give.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommitmentRoots {
    pub row_root: [u8; 32],
    pub rlc_root: [u8; 32],
}

/// Recomputes the row root and RLC root. `siblings` must hold [`ROW_ROOT_SIBLINGS`] nodes.
pub fn commitment_roots(data: &[u8], siblings: &[[u8; 32]]) -> CommitmentRoots {
    let row_size = row_size(data.len());
    let rows = OriginalRows::new(data, row_size);

    let leaves = (0..ORIGINAL_ROWS)
        .map(|i| hash_leaf(&rows.row(i)))
        .collect();
    let row_root = siblings.iter().fold(
        MerkleTree::from_leaf_hashes(leaves).root(),
        |node, sibling| hash_internal(&node, sibling),
    );

    let coeffs = derive_coefficients(&row_root, ORIGINAL_ROWS, PARITY_ROWS, row_size);
    let rlc_leaves = (0..ORIGINAL_ROWS)
        .map(|i| hash_leaf(&compute_rlc(&rows.row(i), &coeffs).to_bytes()))
        .collect();
    let rlc_root = MerkleTree::from_leaf_hashes(rlc_leaves).root();
    CommitmentRoots { row_root, rlc_root }
}

/// The K original rows: header, data, zero padding.
struct OriginalRows<'a> {
    data: &'a [u8],
    row_size: usize,
}

impl<'a> OriginalRows<'a> {
    fn new(data: &'a [u8], row_size: usize) -> Self {
        Self { data, row_size }
    }

    /// Row `i`, borrowed from data when it is a full row.
    fn row(&self, i: usize) -> Cow<'a, [u8]> {
        let size = self.row_size;
        if i == 0 {
            let mut row = vec![0u8; size];
            row[1..BLOB_HEADER_LEN].copy_from_slice(&(self.data.len() as u32).to_be_bytes());
            let n = self.data.len().min(size - BLOB_HEADER_LEN);
            row[BLOB_HEADER_LEN..BLOB_HEADER_LEN + n].copy_from_slice(&self.data[..n]);
            return Cow::Owned(row);
        }
        let start = i * size - BLOB_HEADER_LEN;
        if start + size <= self.data.len() {
            return Cow::Borrowed(&self.data[start..start + size]);
        }
        let mut row = vec![0u8; size];
        if start < self.data.len() {
            let tail = &self.data[start..];
            row[..tail.len()].copy_from_slice(tail);
        }
        Cow::Owned(row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_size_matches_go() {
        assert_eq!(row_size(1), 64);
        assert_eq!(row_size(4096 * 64 - 5), 64);
        assert_eq!(row_size(4096 * 64 - 4), 128);
        assert_eq!(row_size((128 << 20) - 5), 32768);
    }

    #[test]
    fn rows_lay_out_header_and_data() {
        let data: Vec<u8> = (0..200u32).map(|i| i as u8).collect();
        let rows = OriginalRows::new(&data, 64);
        let mut flat = Vec::new();
        for i in 0..ORIGINAL_ROWS {
            flat.extend_from_slice(&rows.row(i));
        }
        assert_eq!(&flat[..5], &[0, 0, 0, 0, 200]);
        assert_eq!(&flat[5..205], &data[..]);
        assert!(flat[205..].iter().all(|&b| b == 0));
        assert_eq!(flat.len(), ORIGINAL_ROWS * 64);
    }
}
