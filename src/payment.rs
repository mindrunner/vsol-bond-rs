use crate::invoice::{ChainAccount, Invoice, UnpaidInvoices, find_invoice_address};
use anyhow::{Result, bail};
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::Pubkey;
use spl_associated_token_account_interface::{
    address::get_associated_token_address_with_program_id,
    instruction::create_associated_token_account_idempotent,
};
use std::str::FromStr;

pub const PAY_INVOICE_DISCRIMINATOR: [u8; 8] = [104, 6, 62, 239, 197, 206, 208, 220];
// Vault's liquid unstaker mints VLP for SOL (`deposit_sol`); invoices from
// epoch 1064 on settle in VLP (IDL: SolanaVault/saas-payment-bot).
pub const DEPOSIT_SOL_DISCRIMINATOR: [u8; 8] = [108, 81, 78, 117, 125, 155, 56, 200];
pub const VLP_POOL_DISCRIMINATOR: [u8; 8] = [241, 154, 109, 4, 17, 177, 109, 188];
pub const INVENTORY_SUMMARY_DISCRIMINATOR: [u8; 8] = [157, 77, 59, 213, 161, 246, 95, 57];
const LIQUID_UNSTAKER_PROGRAM: &str = "2rU1oCHtQ7WJUvy15tKtFvxdYNNSc3id7AzUcjeFSddo";
const VLP_POOL: &str = "9nyw5jxhzuSs88HxKJyDCsWBZMhxj2uNXsFcyHF5KBAb";
const VLP_MINT: &str = "EUWoTx5vQQrxaDFdeK2PLUVbnmRWjw4x6sBbmcxBaHjF";
const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const SYSTEM_PROGRAM: &str = "11111111111111111111111111111111";
const CLOCK_SYSVAR: &str = "SysvarC1ock11111111111111111111111111111111";

// Pool account offsets (Anchor, 8-byte discriminator, borsh packed).
const POOL_SOL_VAULT: usize = 8 + 32;
const POOL_LP_MINT: usize = 8 + 64;
const POOL_TOTAL_LP_TOKENS: usize = 8 + 128;
const POOL_TOTAL_DEACTIVATING_STAKE: usize = 8 + 144;
const POOL_SOL_VAULT_LAMPORTS: usize = 8 + 171;
const POOL_SOL_VAULT_LAMPORTS_CAP: usize = 8 + 179;
const POOL_HALTED_AT_EPOCH_TAG: usize = 8 + 267;
const POOL_MIN_LEN: usize = POOL_HALTED_AT_EPOCH_TAG + 1;
// InventorySummary: discriminator, pool, total_value_snapshot, ...
const INVENTORY_POOL: usize = 8;
const INVENTORY_TOTAL_VALUE_SNAPSHOT: usize = 8 + 32;
const INVENTORY_MIN_LEN: usize = INVENTORY_TOTAL_VALUE_SNAPSHOT + 8;

/// The VLP pool state needed to price and send a `deposit_sol`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatedVlpPool {
    pub sol_vault: Pubkey,
    pub inventory_summary: Pubkey,
    /// SOL in the vault + deactivating stake + LST inventory snapshot, the
    /// pool value Vault's reference bot prices VLP with.
    pub value_lamports: u64,
    pub total_lp_tokens: u64,
    pub sol_vault_lamports: u64,
    pub sol_vault_lamports_cap: u64,
}

#[derive(Clone, Debug)]
pub struct PaymentContext {
    pub payer: Pubkey,
    pub payer_sol_balance: u64,
    pub source_ata_exists: bool,
    pub ata_creation_cost_lamports: u64,
    /// Payer's balance in the payment token (VLP base units).
    pub existing_vsol: u64,
    pub invoices: Vec<Invoice>,
    /// The trusted invoicer whose invoices are paid this run.
    pub invoicer: Pubkey,
    /// That invoicer's reserves token account (validated VLP, owned by it).
    pub invoicer_reserves: Pubkey,
    pub vlp_pool: ValidatedVlpPool,
    pub current_epoch: u64,
    /// Every unpaid invoice for the vote account, from any invoicer.
    pub unpaid: UnpaidInvoices,
}

#[derive(Clone, Copy, Debug)]
pub struct PaymentLimits {
    pub max_total_vsol: u64,
    pub min_sol_reserve_lamports: u64,
    pub deposit_slippage_bps: u16,
    pub transaction_fee_buffer_lamports: u64,
}

/// Amounts named `*_vsol` are payment-token base units; since the epoch-1064
/// cutover that token is VLP (9 decimals like vSOL). The names stay so the
/// metrics keep their series.
#[derive(Clone, Debug)]
pub struct PaymentPlan {
    pub payer: Pubkey,
    pub payer_sol_balance_lamports: u64,
    pub source_ata: Pubkey,
    pub invoices: Vec<Invoice>,
    pub total_outstanding_vsol: u64,
    pub existing_vsol: u64,
    pub vsol_shortfall: u64,
    pub deposit_sol_lamports: u64,
    pub minimum_pool_tokens_out: u64,
    pub instructions: Vec<Instruction>,
    pub current_epoch: u64,
    pub unpaid: UnpaidInvoices,
}

pub fn liquid_unstaker_program_id() -> Pubkey {
    Pubkey::from_str(LIQUID_UNSTAKER_PROGRAM).expect("constant liquid unstaker program")
}

pub fn vlp_pool_address() -> Pubkey {
    Pubkey::from_str(VLP_POOL).expect("constant VLP pool")
}

pub fn vlp_mint() -> Pubkey {
    Pubkey::from_str(VLP_MINT).expect("constant VLP mint")
}

pub fn token_program_id() -> Pubkey {
    Pubkey::from_str(TOKEN_PROGRAM).expect("constant token program")
}

pub fn associated_token_program_id() -> Pubkey {
    spl_associated_token_account_interface::program::id()
}

pub fn associated_token_address(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    get_associated_token_address_with_program_id(owner, mint, &token_program_id())
}

fn vlp_pool_pda(seed: &[u8]) -> Pubkey {
    Pubkey::find_program_address(
        &[seed, vlp_pool_address().as_ref()],
        &liquid_unstaker_program_id(),
    )
    .0
}

pub fn vlp_sol_vault_address() -> Pubkey {
    vlp_pool_pda(b"sol_vault")
}

pub fn vlp_inventory_summary_address() -> Pubkey {
    vlp_pool_pda(b"inventory_summary")
}

/// Validates an SPL token account at `address` (any address) and returns its
/// amount.
pub fn validate_token_account_at(
    address: Pubkey,
    account: &ChainAccount,
    expected_authority: Pubkey,
    expected_mint: Pubkey,
) -> Result<u64> {
    if account.owner != token_program_id() || account.data.len() != 165 {
        bail!("token account {address} has invalid owner or length");
    }
    if account.data[108] != 1 {
        bail!("token account {address} is not initialized");
    }
    if account.data[..32] != expected_mint.to_bytes()
        || account.data[32..64] != expected_authority.to_bytes()
    {
        bail!("token account {address} mint or authority mismatch");
    }
    Ok(u64::from_le_bytes(account.data[64..72].try_into()?))
}

/// Validates the ATA of `expected_authority` for `expected_mint`.
pub fn validate_token_account(
    address: Pubkey,
    account: &ChainAccount,
    expected_authority: Pubkey,
    expected_mint: Pubkey,
) -> Result<u64> {
    if address != associated_token_address(&expected_authority, &expected_mint) {
        bail!("token account is not the expected ATA");
    }
    validate_token_account_at(address, account, expected_authority, expected_mint)
}

fn u64_at(data: &[u8], offset: usize) -> Result<u64> {
    Ok(u64::from_le_bytes(
        data.get(offset..offset + 8)
            .ok_or_else(|| anyhow::anyhow!("u64 field out of bounds"))?
            .try_into()?,
    ))
}

fn pubkey_at(data: &[u8], offset: usize) -> Result<Pubkey> {
    Ok(Pubkey::new_from_array(
        data.get(offset..offset + 32)
            .ok_or_else(|| anyhow::anyhow!("pubkey field out of bounds"))?
            .try_into()?,
    ))
}

pub fn decode_vlp_pool(pool: &ChainAccount, inventory: &ChainAccount) -> Result<ValidatedVlpPool> {
    if pool.owner != liquid_unstaker_program_id()
        || pool.data.len() < POOL_MIN_LEN
        || pool.data[..8] != VLP_POOL_DISCRIMINATOR
    {
        bail!("VLP pool has an invalid owner, length or discriminator");
    }
    let sol_vault = pubkey_at(&pool.data, POOL_SOL_VAULT)?;
    let lp_mint = pubkey_at(&pool.data, POOL_LP_MINT)?;
    if sol_vault != vlp_sol_vault_address() || lp_mint != vlp_mint() {
        bail!("VLP pool sol vault or LP mint does not match the guarded constants");
    }
    if pool.data[POOL_HALTED_AT_EPOCH_TAG] != 0 {
        bail!("VLP pool is halted");
    }
    if inventory.owner != liquid_unstaker_program_id()
        || inventory.data.len() < INVENTORY_MIN_LEN
        || inventory.data[..8] != INVENTORY_SUMMARY_DISCRIMINATOR
        || pubkey_at(&inventory.data, INVENTORY_POOL)? != vlp_pool_address()
    {
        bail!("VLP inventory summary is invalid or belongs to another pool");
    }
    let total_lp_tokens = u64_at(&pool.data, POOL_TOTAL_LP_TOKENS)?;
    let sol_vault_lamports = u64_at(&pool.data, POOL_SOL_VAULT_LAMPORTS)?;
    let deactivating = u64_at(&pool.data, POOL_TOTAL_DEACTIVATING_STAKE)?;
    let inventory_value = u64_at(&inventory.data, INVENTORY_TOTAL_VALUE_SNAPSHOT)?;
    let value_lamports = sol_vault_lamports
        .checked_add(deactivating)
        .and_then(|value| value.checked_add(inventory_value))
        .ok_or_else(|| anyhow::anyhow!("VLP pool value overflow"))?;
    if total_lp_tokens == 0 || value_lamports == 0 {
        bail!("VLP pool is empty");
    }
    Ok(ValidatedVlpPool {
        sol_vault,
        inventory_summary: vlp_inventory_summary_address(),
        value_lamports,
        total_lp_tokens,
        sol_vault_lamports,
        sol_vault_lamports_cap: u64_at(&pool.data, POOL_SOL_VAULT_LAMPORTS_CAP)?,
    })
}

/// SOL to deposit for `tokens` VLP: the pool rate (value / LP supply) rounded
/// up, plus the margin. The rate is conservative: real deposits mint at a
/// slightly better rate (2026-10-31: 1.00437 vs 1.00453 SOL/VLP); any surplus
/// stays in the payer's VLP balance for later invoices.
pub fn vlp_deposit_lamports(pool: &ValidatedVlpPool, tokens: u64, margin_bps: u16) -> Result<u64> {
    let quoted = (tokens as u128)
        .checked_mul(pool.value_lamports as u128)
        .ok_or_else(|| anyhow::anyhow!("deposit quote overflow"))?
        .div_ceil(pool.total_lp_tokens as u128);
    let with_margin = quoted
        .checked_mul(u128::from(10_000_u16 + margin_bps))
        .ok_or_else(|| anyhow::anyhow!("deposit margin overflow"))?
        .div_ceil(10_000);
    u64::try_from(with_margin).map_err(|_| anyhow::anyhow!("deposit quote overflow"))
}

pub fn deposit_sol_instruction(payer: Pubkey, destination: Pubkey, lamports: u64) -> Instruction {
    let mut data = Vec::with_capacity(16);
    data.extend_from_slice(&DEPOSIT_SOL_DISCRIMINATOR);
    data.extend_from_slice(&lamports.to_le_bytes());
    Instruction {
        program_id: liquid_unstaker_program_id(),
        accounts: vec![
            AccountMeta::new(vlp_pool_address(), false),
            AccountMeta::new(vlp_sol_vault_address(), false),
            AccountMeta::new(vlp_mint(), false),
            AccountMeta::new(payer, true),
            AccountMeta::new(destination, false),
            AccountMeta::new(vlp_inventory_summary_address(), false),
            AccountMeta::new_readonly(
                Pubkey::from_str(CLOCK_SYSVAR).expect("constant clock sysvar"),
                false,
            ),
            AccountMeta::new_readonly(
                Pubkey::from_str(SYSTEM_PROGRAM).expect("constant system program"),
                false,
            ),
            AccountMeta::new_readonly(token_program_id(), false),
        ],
        data,
    }
}

pub fn build_payment_plan(context: PaymentContext, limits: PaymentLimits) -> Result<PaymentPlan> {
    let mut invoices = context.invoices;
    if invoices.is_empty() {
        return Ok(PaymentPlan {
            payer: context.payer,
            payer_sol_balance_lamports: context.payer_sol_balance,
            source_ata: associated_token_address(&context.payer, &vlp_mint()),
            invoices: Vec::new(),
            total_outstanding_vsol: 0,
            existing_vsol: context.existing_vsol,
            vsol_shortfall: 0,
            deposit_sol_lamports: 0,
            minimum_pool_tokens_out: 0,
            instructions: Vec::new(),
            current_epoch: context.current_epoch,
            unpaid: context.unpaid,
        });
    }
    if invoices.len() > 6 {
        bail!("at most six invoices may be paid");
    }
    for (index, invoice) in invoices.iter().enumerate() {
        if invoice.balance_outstanding == 0
            || invoice.balance_outstanding > invoice.amount_vsol
            || invoice.invoicer != context.invoicer
            || invoice.address
                != find_invoice_address(&invoice.invoicer, &invoice.vote_account, invoice.epoch)
        {
            bail!("invalid invoice in payment plan");
        }
        if index > 0 && invoice.epoch <= invoices[index - 1].epoch {
            bail!("invoices must be unique and ordered oldest first");
        }
    }
    // Pay the longest oldest-first prefix that fits under the cap; the rest
    // wait for later runs. Refusing outright would deadlock once accumulated
    // arrears exceed one run's cap.
    let cap = limits.max_total_vsol.min(5_000_000_000);
    let mut total = 0_u64;
    let mut payable = 0_usize;
    for invoice in &invoices {
        match total.checked_add(invoice.balance_outstanding) {
            Some(next) if next <= cap => {
                total = next;
                payable += 1;
            }
            _ => break,
        }
    }
    if payable == 0 {
        bail!(
            "oldest invoice outstanding {} exceeds the payment cap {cap}",
            invoices[0].balance_outstanding,
        );
    }
    invoices.truncate(payable);
    let source_ata = associated_token_address(&context.payer, &vlp_mint());
    let shortfall = total.saturating_sub(context.existing_vsol);
    let deposit_lamports = if shortfall == 0 {
        0
    } else {
        vlp_deposit_lamports(&context.vlp_pool, shortfall, limits.deposit_slippage_bps)?
    };
    if deposit_lamports > 0
        && context
            .vlp_pool
            .sol_vault_lamports
            .checked_add(deposit_lamports)
            .is_none_or(|after| after > context.vlp_pool.sol_vault_lamports_cap)
    {
        bail!("VLP deposit would exceed the pool's SOL vault cap");
    }
    let retained = limits
        .min_sol_reserve_lamports
        .checked_add(limits.transaction_fee_buffer_lamports)
        .ok_or_else(|| anyhow::anyhow!("SOL reserve overflow"))?;
    let ata_cost = if context.source_ata_exists {
        if context.ata_creation_cost_lamports != 0 {
            bail!("existing source ATA cannot have a creation cost");
        }
        0
    } else {
        if context.ata_creation_cost_lamports == 0 {
            bail!("missing source ATA requires a rent estimate");
        }
        context.ata_creation_cost_lamports
    };
    let required_sol = deposit_lamports
        .checked_add(retained)
        .and_then(|amount| amount.checked_add(ata_cost))
        .ok_or_else(|| anyhow::anyhow!("required SOL overflow"))?;
    if context.payer_sol_balance < required_sol {
        bail!("payer SOL balance would breach reserve");
    }

    let mut instructions = Vec::with_capacity(invoices.len() + 2);
    if !context.source_ata_exists {
        if context.existing_vsol != 0 {
            bail!("missing source ATA cannot have an existing balance");
        }
        instructions.push(create_associated_token_account_idempotent(
            &context.payer,
            &context.payer,
            &vlp_mint(),
            &token_program_id(),
        ));
    }
    if shortfall > 0 {
        instructions.push(deposit_sol_instruction(
            context.payer,
            source_ata,
            deposit_lamports,
        ));
    }
    for invoice in &invoices {
        instructions.push(pay_invoice_instruction(
            invoice,
            context.payer,
            source_ata,
            context.invoicer_reserves,
        )?);
    }
    Ok(PaymentPlan {
        payer: context.payer,
        payer_sol_balance_lamports: context.payer_sol_balance,
        source_ata,
        invoices,
        total_outstanding_vsol: total,
        existing_vsol: context.existing_vsol,
        vsol_shortfall: shortfall,
        deposit_sol_lamports: deposit_lamports,
        minimum_pool_tokens_out: shortfall,
        instructions,
        current_epoch: context.current_epoch,
        unpaid: context.unpaid,
    })
}

/// `reserves` must be the invoicer's own reserves account; the caller
/// validates it on chain (VLP token account owned by the invoicer).
pub fn pay_invoice_instruction(
    invoice: &Invoice,
    payer: Pubkey,
    source: Pubkey,
    reserves: Pubkey,
) -> Result<Instruction> {
    if invoice.balance_outstanding == 0
        || invoice.balance_outstanding > invoice.amount_vsol
        || invoice.address
            != find_invoice_address(&invoice.invoicer, &invoice.vote_account, invoice.epoch)
        || source != associated_token_address(&payer, &vlp_mint())
        || reserves == source
    {
        bail!("invalid pay_invoice relationships");
    }
    let mut data = Vec::with_capacity(16);
    data.extend_from_slice(&PAY_INVOICE_DISCRIMINATOR);
    data.extend_from_slice(&invoice.balance_outstanding.to_le_bytes());
    Ok(Instruction {
        program_id: crate::invoice::invoicer_program_id(),
        accounts: vec![
            AccountMeta::new_readonly(invoice.invoicer, false),
            AccountMeta::new(invoice.address, false),
            AccountMeta::new(source, false),
            AccountMeta::new_readonly(payer, true),
            AccountMeta::new(reserves, false),
            AccountMeta::new_readonly(token_program_id(), false),
        ],
        data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::invoice::{Invoice, UnpaidInvoices, find_invoice_address, invoicer_address};

    fn invoice(epoch: u64, amount: u64, outstanding: u64) -> Invoice {
        let vote = Pubkey::new_from_array([9; 32]);
        Invoice {
            address: find_invoice_address(&invoicer_address(), &vote, epoch),
            invoicer: invoicer_address(),
            vote_account: vote,
            epoch,
            amount_vsol: amount,
            balance_outstanding: outstanding,
        }
    }

    /// Rate exactly 1 SOL per VLP, so quotes equal the token amounts.
    fn pool() -> ValidatedVlpPool {
        ValidatedVlpPool {
            sol_vault: vlp_sol_vault_address(),
            inventory_summary: vlp_inventory_summary_address(),
            value_lamports: 10_000,
            total_lp_tokens: 10_000,
            sol_vault_lamports: 1_000,
            sol_vault_lamports_cap: u64::MAX,
        }
    }

    fn reserves() -> Pubkey {
        Pubkey::new_from_array([7; 32])
    }

    fn context(existing_vsol: u64, payer_sol: u64) -> PaymentContext {
        PaymentContext {
            payer: Pubkey::new_unique(),
            payer_sol_balance: payer_sol,
            source_ata_exists: true,
            ata_creation_cost_lamports: 0,
            existing_vsol,
            invoices: vec![invoice(780, 10, 3), invoice(781, 10, 2)],
            invoicer: invoicer_address(),
            invoicer_reserves: reserves(),
            vlp_pool: pool(),
            current_epoch: 782,
            unpaid: UnpaidInvoices {
                invoices: vec![
                    (invoice(780, 10, 3).address, 780),
                    (invoice(781, 10, 2).address, 781),
                ],
            },
        }
    }

    fn limits() -> PaymentLimits {
        PaymentLimits {
            max_total_vsol: 5_000_000_000,
            min_sol_reserve_lamports: 100_000_000,
            deposit_slippage_bps: 0,
            transaction_fee_buffer_lamports: 1_000_000,
        }
    }

    #[test]
    fn pays_outstanding_and_only_deposits_shortfall() {
        let plan = build_payment_plan(context(2, 200_000_000), limits()).expect("plan");
        assert_eq!(plan.total_outstanding_vsol, 5);
        assert_eq!(plan.existing_vsol, 2);
        assert_eq!(plan.vsol_shortfall, 3);
        assert_eq!(plan.deposit_sol_lamports, 3);
        assert_eq!(plan.instructions.len(), 3);

        let funded = build_payment_plan(context(5, 200_000_000), limits()).expect("funded");
        assert_eq!(funded.vsol_shortfall, 0);
        assert_eq!(funded.deposit_sol_lamports, 0);
        assert_eq!(funded.instructions.len(), 2);
    }

    #[test]
    fn prices_vlp_at_the_pool_rate_rounded_up_plus_margin() {
        let mut pool = pool();
        // Mainnet 2026-10-31: value / supply = 1.0045336 SOL per VLP.
        pool.value_lamports = 5_118_842_764_631;
        pool.total_lp_tokens = 5_095_740_849_202;
        assert_eq!(
            vlp_deposit_lamports(&pool, 1_000_000_000, 0).unwrap(),
            1_004_533_574
        );
        assert_eq!(
            vlp_deposit_lamports(&pool, 1_000_000_000, 50).unwrap(),
            1_009_556_242
        );
        // A real deposit minted 145_030_838 VLP for 145_665_151 lamports.
        assert!(vlp_deposit_lamports(&pool, 145_030_838, 0).unwrap() >= 145_665_151);
    }

    #[test]
    fn refuses_deposits_above_the_vault_cap() {
        let mut ctx = context(0, 200_000_000);
        ctx.vlp_pool.sol_vault_lamports = 1_000;
        ctx.vlp_pool.sol_vault_lamports_cap = 1_004;
        assert!(build_payment_plan(ctx.clone(), limits()).is_err());
        ctx.vlp_pool.sol_vault_lamports_cap = 1_005;
        assert!(build_payment_plan(ctx, limits()).is_ok());
    }

    #[test]
    fn enforces_five_token_cap_and_point_one_sol_reserve() {
        let mut over_cap = context(0, 10_000_000_000);
        over_cap.invoices = vec![invoice(780, 5_000_000_001, 5_000_000_001)];
        assert!(build_payment_plan(over_cap, limits()).is_err());

        let exact_required = 5 + 100_000_000 + 1_000_000;
        assert!(build_payment_plan(context(0, exact_required), limits()).is_ok());
        assert!(build_payment_plan(context(0, exact_required - 1), limits()).is_err());
    }

    #[test]
    fn trims_to_oldest_prefix_when_arrears_exceed_cap() {
        let mut ctx = context(0, 10_000_000_000);
        ctx.invoices = vec![
            invoice(780, 3_000_000_000, 3_000_000_000),
            invoice(781, 1_500_000_000, 1_500_000_000),
            invoice(782, 1_000_000_000, 1_000_000_000),
        ];
        let plan = build_payment_plan(ctx, limits()).expect("plan");
        assert_eq!(
            plan.invoices.iter().map(|i| i.epoch).collect::<Vec<_>>(),
            vec![780, 781]
        );
        assert_eq!(plan.total_outstanding_vsol, 4_500_000_000);
        assert_eq!(plan.instructions.len(), 3);
    }

    #[test]
    fn rejects_unordered_or_duplicate_invoices() {
        let mut ctx = context(0, 10_000_000_000);
        ctx.invoices = vec![invoice(781, 10, 3), invoice(780, 10, 2)];
        assert!(build_payment_plan(ctx, limits()).is_err());

        let mut ctx = context(0, 10_000_000_000);
        ctx.invoices = vec![invoice(780, 10, 3), invoice(780, 10, 3)];
        assert!(build_payment_plan(ctx, limits()).is_err());
    }

    #[test]
    fn reserve_check_includes_missing_ata_rent() {
        let mut ctx = context(0, 5 + 100_000_000 + 1_000_000 + 2_000_000 - 1);
        ctx.source_ata_exists = false;
        ctx.ata_creation_cost_lamports = 2_000_000;
        assert!(build_payment_plan(ctx, limits()).is_err());
    }

    #[test]
    fn creates_ata_then_vlp_deposit_then_payments_atomically() {
        let mut ctx = context(0, 200_000_000);
        ctx.source_ata_exists = false;
        ctx.ata_creation_cost_lamports = 2_000_000;
        let plan = build_payment_plan(ctx, limits()).expect("plan");
        assert_eq!(
            plan.instructions[0].program_id,
            associated_token_program_id()
        );
        assert_eq!(
            plan.instructions[1].program_id,
            liquid_unstaker_program_id()
        );
        assert!(
            plan.instructions[2..]
                .iter()
                .all(|ix| ix.program_id == crate::invoice::invoicer_program_id())
        );
    }

    #[test]
    fn deposit_instruction_matches_the_idl_account_order() {
        let payer = Pubkey::new_unique();
        let destination = associated_token_address(&payer, &vlp_mint());
        let ix = deposit_sol_instruction(payer, destination, 42);
        assert_eq!(&ix.data[..8], &DEPOSIT_SOL_DISCRIMINATOR);
        assert_eq!(u64::from_le_bytes(ix.data[8..].try_into().unwrap()), 42);
        let keys = ix.accounts.iter().map(|a| a.pubkey).collect::<Vec<_>>();
        assert_eq!(
            keys[..6],
            [
                vlp_pool_address(),
                vlp_sol_vault_address(),
                vlp_mint(),
                payer,
                destination,
                vlp_inventory_summary_address(),
            ]
        );
        // Mainnet PDAs of pool 9nyw5j... (seen in Vault's own deposits).
        assert_eq!(
            vlp_sol_vault_address().to_string(),
            "6RLKARrt6oPCyuMCdYdUHmJxd4wUa6ZeyiC8VSMcYxRv"
        );
        assert_eq!(
            vlp_inventory_summary_address().to_string(),
            "8baArWS1aoaBMK8sAPrWcsoXHxnNTAVTcrUWxnRPg2Jk"
        );
        assert!(ix.accounts[3].is_signer && ix.accounts[3].is_writable);
        assert!(ix.accounts[6..].iter().all(|a| !a.is_writable));
    }

    #[test]
    fn pay_instruction_uses_outstanding_amount_and_idl_account_order() {
        let payer = Pubkey::new_unique();
        let source = associated_token_address(&payer, &vlp_mint());
        let invoice = invoice(780, 99, 7);
        let ix = pay_invoice_instruction(&invoice, payer, source, reserves()).expect("instruction");
        assert_eq!(&ix.data[..8], &PAY_INVOICE_DISCRIMINATOR);
        assert_eq!(u64::from_le_bytes(ix.data[8..].try_into().unwrap()), 7);
        assert_eq!(
            ix.accounts.iter().map(|a| a.pubkey).collect::<Vec<_>>(),
            vec![
                invoicer_address(),
                invoice.address,
                source,
                payer,
                reserves(),
                token_program_id(),
            ]
        );
        assert!(!ix.accounts[0].is_writable);
        assert!(ix.accounts[1].is_writable);
        assert!(ix.accounts[3].is_signer);
        assert!(pay_invoice_instruction(&invoice, payer, source, source).is_err());
    }

    #[test]
    fn refuses_invoices_from_another_invoicer_than_the_selected_one() {
        let other = Pubkey::new_unique();
        let mut context = context(10, 1_000_000_000);
        context.invoices[1].invoicer = other;
        context.invoices[1].address =
            find_invoice_address(&other, &context.invoices[1].vote_account, 781);
        assert!(build_payment_plan(context, limits()).is_err());
    }

    #[test]
    fn pays_the_selected_invoicer_into_its_own_reserves() {
        let migrated = Pubkey::new_unique();
        let mut context = context(10, 1_000_000_000);
        for invoice in &mut context.invoices {
            invoice.invoicer = migrated;
            invoice.address = find_invoice_address(&migrated, &invoice.vote_account, invoice.epoch);
        }
        context.invoicer = migrated;
        let plan = build_payment_plan(context.clone(), limits()).expect("plan");
        let pay = plan
            .instructions
            .iter()
            .filter(|ix| ix.program_id == crate::invoice::invoicer_program_id())
            .collect::<Vec<_>>();
        assert_eq!(pay.len(), 2);
        for ix in pay {
            assert_eq!(ix.accounts[0].pubkey, migrated);
            assert_eq!(ix.accounts[4].pubkey, context.invoicer_reserves);
        }
        assert_eq!(plan.unpaid, context.unpaid);
        assert_eq!(plan.current_epoch, 782);
    }

    #[test]
    fn validates_source_and_reserve_token_relationships() {
        let payer = Pubkey::new_unique();
        let source = associated_token_address(&payer, &vlp_mint());
        let source_data = token_account_data(vlp_mint(), payer, 42);
        let account = |data: Vec<u8>| ChainAccount {
            owner: token_program_id(),
            data,
        };
        assert_eq!(
            validate_token_account(source, &account(source_data.clone()), payer, vlp_mint())
                .unwrap(),
            42
        );
        let mut foreign = source_data;
        foreign[..32].copy_from_slice(Pubkey::new_unique().as_ref());
        assert!(validate_token_account(source, &account(foreign), payer, vlp_mint()).is_err());
        // Reserves need not be an ATA, but mint and authority must match.
        let invoicer = Pubkey::new_unique();
        let reserves_data = token_account_data(vlp_mint(), invoicer, 0);
        assert!(
            validate_token_account_at(
                reserves(),
                &account(reserves_data.clone()),
                invoicer,
                vlp_mint()
            )
            .is_ok()
        );
        assert!(
            validate_token_account(reserves(), &account(reserves_data), invoicer, vlp_mint())
                .is_err()
        );
    }

    #[test]
    fn decodes_real_mainnet_vlp_pool_and_inventory() {
        let pool_account = ChainAccount {
            owner: liquid_unstaker_program_id(),
            data: include_bytes!("../tests/fixtures/mainnet_vlp_pool.bin").to_vec(),
        };
        let inventory = ChainAccount {
            owner: liquid_unstaker_program_id(),
            data: include_bytes!("../tests/fixtures/mainnet_vlp_inventory_summary.bin").to_vec(),
        };
        let pool = decode_vlp_pool(&pool_account, &inventory).expect("real pool");
        assert_eq!(pool.sol_vault, vlp_sol_vault_address());
        assert_eq!(pool.total_lp_tokens, 5_095_740_849_202);
        assert_eq!(pool.value_lamports, 5_118_842_764_631);
        assert_eq!(pool.sol_vault_lamports_cap, 300_000_000_000_000);

        let mut halted = pool_account.clone();
        halted.data[POOL_HALTED_AT_EPOCH_TAG] = 1;
        assert!(decode_vlp_pool(&halted, &inventory).is_err());
        let mut foreign = pool_account.clone();
        foreign.owner = Pubkey::new_unique();
        assert!(decode_vlp_pool(&foreign, &inventory).is_err());
        let mut wrong_mint = pool_account.clone();
        wrong_mint.data[POOL_LP_MINT] ^= 1;
        assert!(decode_vlp_pool(&wrong_mint, &inventory).is_err());
        let mut other_pool = inventory.clone();
        other_pool.data[INVENTORY_POOL] ^= 1;
        assert!(decode_vlp_pool(&pool_account, &other_pool).is_err());
    }

    fn token_account_data(mint: Pubkey, owner: Pubkey, amount: u64) -> Vec<u8> {
        let mut data = vec![0; 165];
        data[..32].copy_from_slice(mint.as_ref());
        data[32..64].copy_from_slice(owner.as_ref());
        data[64..72].copy_from_slice(&amount.to_le_bytes());
        data[108] = 1;
        data
    }
}
