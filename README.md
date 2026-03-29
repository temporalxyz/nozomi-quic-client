# nozomi-quic-client

Ultra-low-latency QUIC/H3 client for submitting Solana transactions to [Nozomi](https://nozomi.temporal.xyz).

## Usage

```rust
use nozomi_quic_client::{NozomiQuicClient, NozomiConfig, NozomiEndpoint};

let mut client = NozomiQuicClient::new(
    NozomiConfig::from_endpoint(NozomiEndpoint::Auto, "your-api-key")
)?;
client.warmup().await?;

// Send a batch of serialized transactions
client.send_batch(&[&tx_bytes]).await?;
```

### Hot path

For minimum latency, pre-encode outside your hot loop:

```rust
let body = NozomiQuicClient::encode_batch(&[&tx1, &tx2])?;

loop {
    client.send_raw(body.clone()).await?; // clone is ~3ns (Arc bump)
}
```

## Endpoints

All direct (non-Cloudflare) endpoints are available via `NozomiEndpoint`:

| Variant | Host |
|---------|------|
| `Auto` | `edge.nozomi.temporal.xyz` (geo-DNS) |
| `Pittsburgh` | `pit1.nozomi.temporal.xyz` |
| `Tokyo` | `tyo1.nozomi.temporal.xyz` |
| `Singapore` | `sgp1.nozomi.temporal.xyz` |
| `Newark` | `ewr1.nozomi.temporal.xyz` |
| `Amsterdam` | `ams1.nozomi.temporal.xyz` |
| `Frankfurt` | `fra2.nozomi.temporal.xyz` |
| `Ashburn` | `ash1.nozomi.temporal.xyz` |
| `LosAngeles` | `lax1.nozomi.temporal.xyz` |
| `London` | `lon1.nozomi.temporal.xyz` |

## Examples

```bash
export NOZOMI_API_KEY="your-key"
export RPC_URL="https://api.mainnet-beta.solana.com"
export FEE_PAYER="~/.config/solana/id.json"

cargo run --example send_single
cargo run --example send_batch
```

## Wire format

Uses the `/api/sendBatch` binary endpoint. Each transaction is length-prefixed with a big-endian `u16`:

```
[len_hi][len_lo][tx_bytes...][len_hi][len_lo][tx_bytes...]...
```

Max 16 transactions per batch, each up to 1,232 bytes.
