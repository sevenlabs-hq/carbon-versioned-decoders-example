use {
    async_trait::async_trait,
    carbon_core::{
        datasource::{Datasource, DatasourceId, TransactionUpdate, Update, UpdateType},
        error::{CarbonResult, Error},
        filter::SlotRangeFilter,
        instruction::InstructionProcessorInputType,
        pipeline::Pipeline,
        processor::Processor,
    },
    carbon_simple_dex_decoder_v1::{
        instructions::SimpleDexInstruction as V1Instruction, SimpleDexDecoder as V1Decoder,
        PROGRAM_ID as DEX_PROGRAM_ID,
    },
    carbon_simple_dex_decoder_v2::{
        instructions::SimpleDexInstruction as V2Instruction, SimpleDexDecoder as V2Decoder,
    },
    solana_hash::Hash,
    solana_message::{
        compiled_instruction::CompiledInstruction, legacy::Message, MessageHeader, VersionedMessage,
    },
    solana_pubkey::Pubkey,
    solana_signature::Signature,
    solana_transaction::versioned::VersionedTransaction,
    solana_transaction_status::TransactionStatusMeta,
    tokio::sync::mpsc,
    tokio_util::sync::CancellationToken,
};

// Upgrade point: slot 500, transaction index 10
// Instructions BEFORE this → V1 decoder
// Instructions FROM this onwards → V2 decoder
const UPGRADE_SLOT: u64 = 500;
const UPGRADE_TX_INDEX: u64 = 10;

// sha256("global:swap")[0..8]
const SWAP_DISCRIMINATOR: [u8; 8] = [248, 198, 158, 145, 225, 117, 135, 200];

// ─── Mock Datasource ─────────────────────────────────────────────────────────

struct MockDatasource {
    updates: Vec<Update>,
}

impl MockDatasource {
    fn new(updates: Vec<Update>) -> Self {
        Self { updates }
    }
}

#[async_trait]
impl Datasource for MockDatasource {
    async fn consume(
        &self,
        id: DatasourceId,
        sender: mpsc::Sender<(Update, DatasourceId)>,
        _cancellation_token: CancellationToken,
    ) -> CarbonResult<()> {
        for update in &self.updates {
            sender
                .send((update.clone(), id.clone()))
                .await
                .map_err(|e| Error::Custom(format!("send error: {e}")))?;
        }
        Ok(())
    }

    fn update_types(&self) -> Vec<UpdateType> {
        vec![UpdateType::Transaction]
    }
}

// ─── Transaction Builders ────────────────────────────────────────────────────

fn v1_swap_tx(slot: u64, index: Option<u64>, amount_in: u64, min_amount_out: u64) -> Update {
    let mut data = SWAP_DISCRIMINATOR.to_vec();
    data.extend_from_slice(&amount_in.to_le_bytes());
    data.extend_from_slice(&min_amount_out.to_le_bytes());
    make_tx(slot, index, data)
}

fn v2_swap_tx(
    slot: u64,
    index: Option<u64>,
    amount_in: u64,
    min_amount_out: u64,
    fee_tier: u8,
) -> Update {
    let mut data = SWAP_DISCRIMINATOR.to_vec();
    data.extend_from_slice(&amount_in.to_le_bytes());
    data.extend_from_slice(&min_amount_out.to_le_bytes());
    data.push(fee_tier);
    make_tx(slot, index, data)
}

fn make_tx(slot: u64, index: Option<u64>, instruction_data: Vec<u8>) -> Update {
    let payer = Pubkey::new_unique();
    let program_id = DEX_PROGRAM_ID;

    let message = VersionedMessage::Legacy(Message {
        header: MessageHeader {
            num_required_signatures: 1,
            num_readonly_signed_accounts: 0,
            num_readonly_unsigned_accounts: 0,
        },
        account_keys: vec![payer, program_id],
        recent_blockhash: Hash::default(),
        instructions: vec![CompiledInstruction {
            program_id_index: 1,
            accounts: vec![],
            data: instruction_data,
        }],
    });

    Update::Transaction(Box::new(TransactionUpdate {
        slot,
        signature: Signature::new_unique(),
        transaction: VersionedTransaction {
            signatures: vec![Signature::new_unique()],
            message,
        },
        meta: TransactionStatusMeta {
            log_messages: Some(vec![
                format!("Program {program_id} invoke [1]"),
                format!("Program {program_id} success"),
            ]),
            ..Default::default()
        },
        index,
        block_time: None,
        block_hash: None,
        is_vote: false,
    }))
}

// ─── Resolver ────────────────────────────────────────────────────────────────

struct ResolvedSwap {
    amount_in: u64,
    min_amount_out: u64,
    fee_tier: Option<u8>,
    slot: u64,
    tx_idx: Option<u64>,
}

// Single processor handles both decoder versions by resolving to a common type.
struct SwapProcessor;

impl SwapProcessor {
    fn handle(&mut self, swap: ResolvedSwap) -> CarbonResult<()> {
        match swap.fee_tier {
            None => log::info!(
                "[V1] Swap | slot={} tx_idx={:?} | amount_in={} min_amount_out={}",
                swap.slot, swap.tx_idx, swap.amount_in, swap.min_amount_out,
            ),
            Some(tier) => log::info!(
                "[V2] Swap | slot={} tx_idx={:?} | amount_in={} min_amount_out={} fee_tier={}",
                swap.slot, swap.tx_idx, swap.amount_in, swap.min_amount_out, tier,
            ),
        }
        Ok(())
    }
}

impl Processor<InstructionProcessorInputType<'_, V1Instruction>> for SwapProcessor {
    async fn process(
        &mut self,
        input: &InstructionProcessorInputType<'_, V1Instruction>,
    ) -> CarbonResult<()> {
        let V1Instruction::Swap { data, .. } = input.decoded_instruction;
        self.handle(ResolvedSwap {
            amount_in: data.amount_in,
            min_amount_out: data.min_amount_out,
            fee_tier: None,
            slot: input.metadata.transaction_metadata.slot,
            tx_idx: input.metadata.transaction_metadata.index,
        })?;
        Ok(())
    }
}

impl Processor<InstructionProcessorInputType<'_, V2Instruction>> for SwapProcessor {
    async fn process(
        &mut self,
        input: &InstructionProcessorInputType<'_, V2Instruction>,
    ) -> CarbonResult<()> {
        let V2Instruction::Swap { data, .. } = input.decoded_instruction;
        self.handle(ResolvedSwap {
            amount_in: data.amount_in,
            min_amount_out: data.min_amount_out,
            fee_tier: Some(data.fee_tier),
            slot: input.metadata.transaction_metadata.slot,
            tx_idx: input.metadata.transaction_metadata.index,
        })?;
        Ok(())
    }
}

// ─── Main ────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> CarbonResult<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    log::info!("=== Versioned Decoders Example ===");
    log::info!(
        "Program upgrade at slot={UPGRADE_SLOT}, tx_index={UPGRADE_TX_INDEX}"
    );
    log::info!("V1 decoder: slots < {UPGRADE_SLOT} OR (slot={UPGRADE_SLOT} AND tx_idx < {UPGRADE_TX_INDEX})");
    log::info!("V2 decoder: slots > {UPGRADE_SLOT} OR (slot={UPGRADE_SLOT} AND tx_idx >= {UPGRADE_TX_INDEX})");
    log::info!("");

    // SlotRangeFilter splits traffic at the exact upgrade boundary
    let v1_filter = SlotRangeFilter::to(UPGRADE_SLOT, Some(UPGRADE_TX_INDEX));
    let v2_filter = SlotRangeFilter::from(UPGRADE_SLOT, Some(UPGRADE_TX_INDEX));

    // Mock transactions representing the before/at/after upgrade timeline
    let updates = vec![
        // V1 era: old layout, no fee_tier field
        v1_swap_tx(499, Some(0), 1_000_000, 990_000),
        v1_swap_tx(500, Some(9), 2_000_000, 1_980_000),
        // V2 era: new layout with fee_tier
        v2_swap_tx(500, Some(10), 3_000_000, 2_970_000, 5),
        v2_swap_tx(501, Some(0), 4_000_000, 3_960_000, 10),
    ];

    Pipeline::builder()
        .datasource(MockDatasource::new(updates))
        .instruction_with_filters(V1Decoder, SwapProcessor, vec![Box::new(v1_filter)])
        .instruction_with_filters(V2Decoder, SwapProcessor, vec![Box::new(v2_filter)])
        .build()?
        .run()
        .await?;

    log::info!("Done.");
    Ok(())
}
