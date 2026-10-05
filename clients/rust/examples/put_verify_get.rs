//! Puts a file, verifies its commitment and the on-chain PayForFibre, then reads it back.
//!
//! ```sh
//! GW=https://cf.celestia-corto.com:8443 TOKEN=... RPC=https://v0.cf.celestia-corto.com:26657 \
//!   cargo run --release --features http --example put_verify_get -- blob.bin
//! ```

use base64::{engine::general_purpose::STANDARD, Engine};
use fibre_gateway_client::{parse_blob_id, Client};
use sha2::{Digest, Sha256};

type Error = Box<dyn std::error::Error>;

fn main() -> Result<(), Error> {
    let path = std::env::args()
        .nth(1)
        .ok_or("usage: put_verify_get <blob>")?;
    let data = std::fs::read(path)?;
    let digest = Sha256::digest(&data);
    let client = Client::new(std::env::var("GW")?, std::env::var("TOKEN")?);

    // 1. Put. `put` returns the receipt only after its commitment verified against `data`.
    let receipt = client.put(&data)?;

    // 2. Check that the chain accepted a PayForFibre for the same commitment.
    let url = format!("{}/tx?hash=0x{}", std::env::var("RPC")?, receipt.tx_hash);
    let tx: serde_json::Value = ureq::get(&url).call()?.into_json()?;
    let commitment = &parse_blob_id(&receipt.blob_id)?[1..];
    check_pay_for_fibre(&tx, commitment)?;

    // 3. Get, and check that the bytes are the ones we put.
    let back = client.get(&receipt.blob_id)?;
    if back.len() != data.len() || Sha256::digest(&back) != digest {
        return Err("read-back does not match".into());
    }
    println!("ok {} {}", receipt.blob_id, receipt.tx_hash);
    Ok(())
}

/// Checks that a CometBFT `/tx` response succeeded and holds an
/// `EventPayForFibre` for `commitment`.
fn check_pay_for_fibre(tx: &serde_json::Value, commitment: &[u8]) -> Result<(), Error> {
    let result = &tx["result"]["tx_result"];
    // CometBFT omits `code` when it is 0.
    if result["code"].as_u64().unwrap_or(0) != 0 {
        return Err(format!("tx failed: {tx}").into());
    }
    let events = result["events"].as_array().ok_or("no events")?;
    for event in events {
        if event["type"] != "celestia.fibre.v1.EventPayForFibre" {
            continue;
        }
        for attr in event["attributes"].as_array().ok_or("no attributes")? {
            if attr["key"] != "commitment" {
                continue;
            }
            // The value is a JSON string holding base64.
            let value: String = serde_json::from_str(attr["value"].as_str().ok_or("bad value")?)?;
            if STANDARD.decode(value)? == commitment {
                return Ok(());
            }
        }
    }
    Err("no EventPayForFibre for this commitment".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_commitment() {
        let commitment = [7u8; 32];
        let value = serde_json::to_string(&STANDARD.encode(commitment)).unwrap();
        let tx = |code: u64, value: &str| {
            let mut tx = serde_json::json!({"result": {"tx_result": {"events": [
                {"type": "message", "attributes": [{"key": "action", "value": "/celestia.fibre.v1.MsgPayForFibre"}]},
                {"type": "celestia.fibre.v1.EventPayForFibre", "attributes": [
                    {"key": "commitment", "value": value, "index": true},
                    {"key": "signer", "value": "\"celestia1...\"", "index": true},
                ]},
            ]}}});
            if code != 0 {
                tx["result"]["tx_result"]["code"] = code.into();
            }
            tx
        };
        check_pay_for_fibre(&tx(0, &value), &commitment).unwrap();
        assert!(check_pay_for_fibre(&tx(0, &value), &[8u8; 32]).is_err());
        assert!(check_pay_for_fibre(&tx(1, &value), &commitment).is_err());
        let rpc_error = serde_json::json!({"error": {"code": -32603, "message": "tx not found"}});
        assert!(check_pay_for_fibre(&rpc_error, &commitment).is_err());
    }
}
