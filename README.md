# Versioned Decoders Example

Demonstrates how to use `SlotRangeFilter` to route transactions to the correct decoder version when a Solana program upgrades mid-stream.

## What It Shows

A fictional DEX program upgrades at **slot 500, tx_index 10**, adding a `fee_tier: u8` field to its `Swap` instruction:

- **V1** `Swap { amount_in: u64, min_amount_out: u64 }`
- **V2** `Swap { amount_in: u64, min_amount_out: u64, fee_tier: u8 }`

The pipeline uses two decoders with `SlotRangeFilter` so each instruction hits exactly one decoder based on when it occurred.

## Structure

```
test/versioned-decoders-example/
├── idls/
│   ├── dex_v1.json          # Anchor IDL — swap (2 args)
│   └── dex_v2.json          # Anchor IDL — swap (3 args, added fee_tier)
├── decoder-v1/              # Generated: carbon-simple-dex-decoder-v1
├── decoder-v2/              # Generated: carbon-simple-dex-decoder-v2
├── indexer/
│   ├── Cargo.toml
│   └── src/main.rs          # MockDatasource + SlotRangeFilter pipeline
├── Cargo.toml               # Workspace (decoder-v1, decoder-v2, indexer)
└── README.md
```

## Full Flow — How This Was Built

### Step 1 — Write the IDLs

`idls/dex_v1.json` — original program layout:
```json
{
  "address": "Dex1111111111111111111111111111111111111111",
  "metadata": { "name": "simple_dex", "version": "0.1.0", "spec": "0.1.0" },
  "instructions": [{
    "name": "swap",
    "discriminator": [248, 198, 158, 145, 225, 117, 135, 200],
    "accounts": [],
    "args": [
      { "name": "amountIn", "type": "u64" },
      { "name": "minAmountOut", "type": "u64" }
    ]
  }]
}
```

`idls/dex_v2.json` — upgraded layout (added `feeTier`):
```json
{
  "address": "Dex1111111111111111111111111111111111111111",
  "metadata": { "name": "simple_dex", "version": "0.2.0", "spec": "0.1.0" },
  "instructions": [{
    "name": "swap",
    "discriminator": [248, 198, 158, 145, 225, 117, 135, 200],
    "accounts": [],
    "args": [
      { "name": "amountIn", "type": "u64" },
      { "name": "minAmountOut", "type": "u64" },
      { "name": "feeTier", "type": "u8" }
    ]
  }]
}
```

> **Discriminator** = first 8 bytes of `sha256("global:<instruction_name>")`.
> Compute with: `node -e "const c=require('crypto');console.log(Array.from(c.createHash('sha256').update('global:swap').digest().slice(0,8)))"`

### Step 2 — Generate Decoders

Run from the carbon repo root. Uses `node packages/cli/dist/cli.js` with the `parse` command:

```bash
# V1 decoder → carbon-simple-dex-decoder-v1
node packages/cli/dist/cli.js parse \
  --idl test/versioned-decoders-example/idls/dex_v1.json \
  --out-dir test/versioned-decoders-example/decoder-v1 \
  --name simple-dex \
  --version-name v1 \
  --as-crate \
  --standard anchor \
  --with-postgres false \
  --with-graphql false \
  --standalone true

# V2 decoder → carbon-simple-dex-decoder-v2
node packages/cli/dist/cli.js parse \
  --idl test/versioned-decoders-example/idls/dex_v2.json \
  --out-dir test/versioned-decoders-example/decoder-v2 \
  --name simple-dex \
  --version-name v2 \
  --as-crate \
  --standard anchor \
  --with-postgres false \
  --with-graphql false \
  --standalone true
```

The `--version-name` flag appends `-v1` / `-v2` to the crate name so both can coexist in the same workspace.

After generation, remove the `[workspace]` line the CLI adds to each decoder's `Cargo.toml` (it conflicts with the parent workspace):

```bash
# In decoder-v1/Cargo.toml and decoder-v2/Cargo.toml, delete the line:
[workspace]
```

### Step 3 — Create the Workspace

`test/versioned-decoders-example/Cargo.toml`:
```toml
[workspace]
members = ["decoder-v1", "decoder-v2", "indexer"]
resolver = "2"
```

This prevents cargo from walking up to `test/Cargo.toml`.

### Step 4 — Write the Indexer

Key pieces in `indexer/src/main.rs`:

```rust
const UPGRADE_SLOT: u64 = 500;
const UPGRADE_TX_INDEX: u64 = 10;

// V1: everything strictly before the upgrade
let v1_filter = SlotRangeFilter::to(UPGRADE_SLOT, Some(UPGRADE_TX_INDEX));
// V2: everything from the upgrade onwards
let v2_filter = SlotRangeFilter::from(UPGRADE_SLOT, Some(UPGRADE_TX_INDEX));

Pipeline::builder()
    .datasource(MockDatasource::new(updates))
    .instruction_with_filters(V1Decoder, V1Processor, vec![Box::new(v1_filter)])
    .instruction_with_filters(V2Decoder, V2Processor, vec![Box::new(v2_filter)])
    .build()?
    .run()
    .await?;
```

### Step 5 — Run

```bash
cd test/versioned-decoders-example
cargo run -p versioned-decoders-example-indexer
```

## Expected Output

```
INFO  === Versioned Decoders Example ===
INFO  Program upgrade at slot=500, tx_index=10
INFO  [V1] Swap | slot=499 tx_idx=Some(0) | amount_in=1000000 min_amount_out=990000
INFO  [V1] Swap | slot=500 tx_idx=Some(9) | amount_in=2000000 min_amount_out=1980000
INFO  [V2] Swap | slot=500 tx_idx=Some(10) | amount_in=3000000 min_amount_out=2970000 fee_tier=5
INFO  [V2] Swap | slot=501 tx_idx=Some(0) | amount_in=4000000 min_amount_out=3960000 fee_tier=10
```

## Routing Logic

```
slot=499, tx_idx=0    → V1 filter: slot < 500 ✅  |  V2 filter: slot < 500 ❌
slot=500, tx_idx=9    → V1 filter: slot=500, idx < 10 ✅  |  V2 filter: idx < 10 ❌
slot=500, tx_idx=10   → V1 filter: slot=500, idx >= 10 ❌  |  V2 filter: idx >= 10 ✅  ← upgrade point
slot=501, tx_idx=0    → V1 filter: slot > 500 ❌  |  V2 filter: slot > 500 ✅
```

## SlotRangeFilter API

```rust
// Open-ended from slot S, tx index I (inclusive lower bound)
SlotRangeFilter::from(slot, Some(tx_index))

// Open-ended up to slot S, tx index I (exclusive upper bound at boundary)
SlotRangeFilter::to(slot, Some(tx_index))

// Closed range between two points
SlotRangeFilter::between(from_slot, Some(from_tx_index), to_slot, Some(to_tx_index))
```

Boundary semantics:
- At `from_slot`: accepts tx_index `>= from_tx_index`
- At `to_slot`: accepts tx_index `< to_tx_index`
- Pass `None` for tx_index to match any transaction in that slot
