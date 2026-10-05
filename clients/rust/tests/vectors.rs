//! Checks the verifier against vectors encoded by the Fibre encoder in celestia-app.

use fibre_gateway_client::{
    commitment_roots, parse_blob_id, row_size, verify, verify_commitment, CommitmentProof,
    PutResponse, Receipt, VerifyError,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};

#[derive(Deserialize)]
struct File {
    vectors: Vec<Vector>,
}

#[derive(Deserialize)]
struct Vector {
    size: usize,
    data_sha256: String,
    row_size: usize,
    blob_id: String,
    row_root_siblings: Vec<String>,
    row_root: String,
    rlc_root: String,
}

fn vectors() -> Vec<Vector> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../testdata/commitment_vectors.json"
    );
    let raw = std::fs::read(path).unwrap();
    serde_json::from_slice::<File>(&raw).unwrap().vectors
}

/// Byte i is the top byte of the wrapping u32 product i * 2654435761.
fn data(size: usize) -> Vec<u8> {
    (0..size)
        .map(|i| ((i as u32).wrapping_mul(2654435761) >> 24) as u8)
        .collect()
}

fn receipt(blob_id: &str) -> Receipt {
    Receipt {
        chain_id: "chain".into(),
        tx_hash: "ab".repeat(32),
        blob_id: blob_id.into(),
        promise_height: 1,
    }
}

fn siblings(v: &Vector) -> Vec<[u8; 32]> {
    v.row_root_siblings
        .iter()
        .map(|s| hex::decode(s).unwrap().try_into().unwrap())
        .collect()
}

#[test]
fn go_vectors_verify() {
    for v in vectors() {
        let data = data(v.size);
        assert_eq!(
            hex::encode(Sha256::digest(&data)),
            v.data_sha256,
            "size {}",
            v.size
        );
        assert_eq!(row_size(v.size), v.row_size, "size {}", v.size);

        let roots = commitment_roots(&data, &siblings(&v));
        assert_eq!(hex::encode(roots.row_root), v.row_root, "size {}", v.size);
        assert_eq!(hex::encode(roots.rlc_root), v.rlc_root, "size {}", v.size);

        let proof = CommitmentProof {
            row_root_siblings: v.row_root_siblings.clone(),
        };
        verify(&data, &receipt(&v.blob_id), &proof).unwrap();
    }
}

#[test]
fn tampering_fails() {
    for v in vectors().into_iter().filter(|v| v.size <= 1 << 20) {
        let data = data(v.size);
        let id = parse_blob_id(&v.blob_id).unwrap();
        let s = siblings(&v);

        let mut tampered = data.clone();
        *tampered.last_mut().unwrap() ^= 1;
        assert_eq!(
            verify_commitment(&tampered, &id, &s),
            Err(VerifyError::Mismatch)
        );

        let mut longer = data.clone();
        longer.push(0);
        assert_eq!(
            verify_commitment(&longer, &id, &s),
            Err(VerifyError::Mismatch)
        );

        assert_eq!(
            verify_commitment(&data, &id, &[s[1], s[0]]),
            Err(VerifyError::Mismatch)
        );
        assert_eq!(
            verify_commitment(&data, &id, &s[..1]),
            Err(VerifyError::InvalidProof)
        );

        let mut other = id;
        other[32] ^= 1;
        assert_eq!(
            verify_commitment(&data, &other, &s),
            Err(VerifyError::Mismatch)
        );
        other = id;
        other[0] = 1;
        assert_eq!(
            verify_commitment(&data, &other, &s),
            Err(VerifyError::UnsupportedVersion(1))
        );
    }
}

#[test]
fn malformed_input_fails() {
    let v = &vectors()[0];
    let data = data(v.size);
    let good = CommitmentProof {
        row_root_siblings: v.row_root_siblings.clone(),
    };
    let bad_hex = CommitmentProof {
        row_root_siblings: vec!["zz".into(), v.row_root_siblings[1].clone()],
    };
    assert_eq!(
        verify(&data, &receipt(&v.blob_id), &bad_hex),
        Err(VerifyError::InvalidProof)
    );
    assert_eq!(
        verify(&data, &receipt(&v.blob_id[..10]), &good),
        Err(VerifyError::InvalidBlobId)
    );
    assert_eq!(
        verify(&[], &receipt(&v.blob_id), &good),
        Err(VerifyError::InvalidDataSize(0))
    );
}

#[test]
fn put_response_round_trips_with_or_without_proof() {
    let raw = r#"{"chain_id":"c","tx_hash":"t","blob_id":"b","promise_height":7,"commitment_proof":{"row_root_siblings":["aa","bb"]}}"#;
    let put: PutResponse = serde_json::from_str(raw).unwrap();
    assert_eq!(put.commitment_proof.row_root_siblings, ["aa", "bb"]);
    assert_eq!(serde_json::to_string(&put).unwrap(), raw);
    let receipt = serde_json::to_string(&put.receipt).unwrap();
    assert_eq!(
        receipt,
        r#"{"chain_id":"c","tx_hash":"t","blob_id":"b","promise_height":7}"#
    );

    assert!(serde_json::from_str::<PutResponse>(&receipt).is_err());
}

/// The step-by-step check from docs/client-guide.md, written out with rsema1d.
fn manual_check(data: &[u8], blob_id: &str, siblings: [[u8; 32]; 2]) -> bool {
    use fibre_gateway_client::{ORIGINAL_ROWS, PARITY_ROWS};
    use rsema1d::codec::compute_rlc;
    use rsema1d::crypto::{derive_coefficients, hash_internal, hash_leaf, sha256, MerkleTree};

    // blob_id = version byte 0x00 ‖ 32-byte commitment.
    let id = parse_blob_id(blob_id).unwrap();
    assert_eq!(id[0], 0);

    // Lay out header ‖ data ‖ zero padding over K = 4096 rows of row_size bytes.
    let size = row_size(data.len());
    let mut flat = vec![0u8; ORIGINAL_ROWS * size];
    flat[1..5].copy_from_slice(&(data.len() as u32).to_be_bytes());
    flat[5..5 + data.len()].copy_from_slice(data);
    let rows: Vec<&[u8]> = flat.chunks(size).collect();

    // Row root: Merkle root of the K rows, then fold in the two siblings.
    let leaves = rows.iter().map(|row| hash_leaf(row)).collect();
    let mut row_root = MerkleTree::from_leaf_hashes(leaves).root();
    for sibling in &siblings {
        row_root = hash_internal(&row_root, sibling);
    }

    // RLC root: RLC of every original row, with coefficients derived from the row root.
    let coeffs = derive_coefficients(&row_root, ORIGINAL_ROWS, PARITY_ROWS, size);
    let rlc_leaves = rows
        .iter()
        .map(|row| hash_leaf(&compute_rlc(row, &coeffs).to_bytes()))
        .collect();
    let rlc_root = MerkleTree::from_leaf_hashes(rlc_leaves).root();

    // commitment = SHA256(row_root ‖ rlc_root).
    sha256(&[row_root, rlc_root].concat()) == id[1..]
}

#[test]
fn manual_steps_match_vectors() {
    for v in vectors().into_iter().filter(|v| v.size <= 1 << 20) {
        let data = data(v.size);
        let s: [[u8; 32]; 2] = siblings(&v).try_into().unwrap();
        assert!(manual_check(&data, &v.blob_id, s), "size {}", v.size);
        let mut tampered = data.clone();
        tampered[0] ^= 1;
        assert!(!manual_check(&tampered, &v.blob_id, s), "size {}", v.size);
    }
}
