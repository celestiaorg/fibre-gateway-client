//! Puts a file, keeps the blob_id and reads the blob back.
//!
//! ```sh
//! TOKEN=... cargo run --release --features http --example quickstart -- blob.bin
//! ```

use fibre_gateway_client::Client;

const GATEWAY: &str = "https://cf.celestia-corto.com:8443";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).ok_or("usage: quickstart <blob>")?;
    let data = std::fs::read(path)?;
    let client = Client::new(GATEWAY, std::env::var("TOKEN")?);

    // `put` checks the commitment proof against `data` before it returns the receipt.
    let receipt = client.put(&data)?;
    // After the check, `blob_id` is all you need to keep.
    std::fs::write("blob_id.txt", &receipt.blob_id)?;

    let back = client.get(&receipt.blob_id)?;
    if back != data {
        return Err("read-back does not match".into());
    }
    println!("ok {} {}", receipt.blob_id, receipt.tx_hash);
    Ok(())
}
