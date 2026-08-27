use crate::invoice::{ChainAccount, Invoice, find_invoice_address, invoicer_address};
use anyhow::{Result, bail};
use borsh::BorshDeserialize;
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::Pubkey;
use solana_stake_interface::state::StakeStateV2;
use spl_associated_token_account_interface::{
    address::get_associated_token_address_with_program_id,
    instruction::create_associated_token_account_idempotent,
};
use spl_stake_pool::state::{AccountType, StakePool};
use std::str::FromStr;

pub const PAY_INVOICE_DISCRIMINATOR: [u8; 8] = [104, 6, 62, 239, 197, 206, 208, 220];
const STAKE_POOL: &str = "Fu9BYC6tWBo1KMKaP3CFoKfRhqv9akmy3DuYwnCyWiyC";
const VSOL_MINT: &str = "vSoLxydx6akxyMD9XEcPvGYNGq6Nn66oqVb3UkGkei7";
const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatedStakePool {
    pub manager: Pubkey,
    pub withdraw_authority: Pubkey,
    pub withdraw_bump_seed: u8,
    pub reserve_stake: Pubkey,
    pub pool_mint: Pubkey,
    pub manager_fee_account: Pubkey,
    pub token_program_id: Pubkey,
    pub total_lamports: u64,
    pub pool_token_supply: u64,
    pub sol_deposit_fee_numerator: u64,
    pub sol_deposit_fee_denominator: u64,
    pub sol_deposit_authority: Option<Pubkey>,
}

#[derive(Clone, Debug)]
pub struct PaymentContext {
    pub payer: Pubkey,
    pub payer_sol_balance: u64,
    pub source_ata_exists: bool,
    pub ata_creation_cost_lamports: u64,
    pub existing_vsol: u64,
    pub invoices: Vec<Invoice>,
    pub invoicer_reserves: Pubkey,
    pub stake_pool: ValidatedStakePool,
}

#[derive(Clone, Copy, Debug)]
pub struct PaymentLimits {
    pub max_total_vsol: u64,
    pub min_sol_reserve_lamports: u64,
    pub deposit_slippage_bps: u16,
    pub transaction_fee_buffer_lamports: u64,
}

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
}

pub fn stake_pool_address() -> Pubkey {
    Pubkey::from_str(STAKE_POOL).expect("constant stake pool")
}

pub fn stake_pool_program_id() -> Pubkey {
    spl_stake_pool::id()
}

pub fn vsol_mint() -> Pubkey {
    Pubkey::from_str(VSOL_MINT).expect("constant vSOL mint")
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

pub fn validate_token_account(
    address: Pubkey,
    account: &ChainAccount,
    expected_authority: Pubkey,
    expected_mint: Pubkey,
) -> Result<u64> {
    if address != associated_token_address(&expected_authority, &expected_mint) {
        bail!("token account is not the expected ATA");
    }
    if account.owner != token_program_id() || account.data.len() != 165 {
        bail!("token account has invalid owner or length");
    }
    if account.data[108] != 1 {
        bail!("token account is not initialized");
    }
    if account.data[..32] != expected_mint.to_bytes()
        || account.data[32..64] != expected_authority.to_bytes()
    {
        bail!("token account mint or authority mismatch");
    }
    Ok(u64::from_le_bytes(account.data[64..72].try_into()?))
}

pub fn decode_stake_pool(account: &ChainAccount, current_epoch: u64) -> Result<ValidatedStakePool> {
    if account.owner != stake_pool_program_id() {
        bail!("stake pool has foreign owner");
    }
    // Stake pool accounts are allocated larger than their borsh payload, so
    // trailing bytes must be tolerated (SPL's try_from_slice_unchecked
    // semantics); try_from_slice would reject every real account.
    let pool = StakePool::deserialize(&mut account.data.as_slice())
        .map_err(|error| anyhow::anyhow!("invalid stake pool data: {error}"))?;
    let (withdraw_authority, withdraw_bump_seed) =
        spl_stake_pool::find_withdraw_authority_program_address(
            &stake_pool_program_id(),
            &stake_pool_address(),
        );
    if pool.account_type != AccountType::StakePool
        || pool.pool_mint != vsol_mint()
        || pool.token_program_id != token_program_id()
        || pool.stake_withdraw_bump_seed != withdraw_bump_seed
        || pool.total_lamports == 0
        || pool.pool_token_supply == 0
        || pool.last_update_epoch != current_epoch
    {
        bail!(
            "stake pool state does not match guarded constants (mint {}, \
             token program {}, withdraw bump {}, total lamports {}, pool \
             token supply {}, last update epoch {}, current epoch \
             {current_epoch})",
            pool.pool_mint,
            pool.token_program_id,
            pool.stake_withdraw_bump_seed,
            pool.total_lamports,
            pool.pool_token_supply,
            pool.last_update_epoch,
        );
    }
    if pool.sol_deposit_authority.is_some() {
        bail!("stake pool requires an unsupported SOL deposit authority");
    }
    Ok(ValidatedStakePool {
        manager: pool.manager,
        withdraw_authority,
        withdraw_bump_seed,
        reserve_stake: pool.reserve_stake,
        pool_mint: pool.pool_mint,
        manager_fee_account: pool.manager_fee_account,
        token_program_id: pool.token_program_id,
        total_lamports: pool.total_lamports,
        pool_token_supply: pool.pool_token_supply,
        sol_deposit_fee_numerator: pool.sol_deposit_fee.numerator,
        sol_deposit_fee_denominator: pool.sol_deposit_fee.denominator,
        sol_deposit_authority: pool.sol_deposit_authority,
    })
}

pub fn validate_deposit_accounts(
    pool: &ValidatedStakePool,
    mint_account: &ChainAccount,
    reserve_account: &ChainAccount,
    manager_fee_address: Pubkey,
    manager_fee_account: &ChainAccount,
) -> Result<()> {
    if pool.pool_mint != vsol_mint()
        || pool.token_program_id != token_program_id()
        || spl_stake_pool::find_withdraw_authority_program_address(
            &stake_pool_program_id(),
            &stake_pool_address(),
        ) != (pool.withdraw_authority, pool.withdraw_bump_seed)
    {
        bail!("stake pool program relationships are invalid");
    }
    validate_pool_mint(pool, mint_account)?;
    validate_reserve_stake(pool, reserve_account)?;
    validate_manager_fee(pool, manager_fee_address, manager_fee_account)
}

fn validate_pool_mint(pool: &ValidatedStakePool, account: &ChainAccount) -> Result<()> {
    if account.owner != token_program_id() || account.data.len() != 82 {
        bail!("pool mint has invalid program owner or length");
    }
    let mint_authority_tag = u32::from_le_bytes(account.data[0..4].try_into()?);
    let mint_authority = Pubkey::new_from_array(account.data[4..36].try_into()?);
    let supply = u64::from_le_bytes(account.data[36..44].try_into()?);
    let freeze_authority_tag = u32::from_le_bytes(account.data[46..50].try_into()?);
    if mint_authority_tag != 1
        || mint_authority != pool.withdraw_authority
        || account.data[44] != 9
        || account.data[45] != 1
        || freeze_authority_tag != 0
    {
        bail!(
            "pool mint authority, decimals, or freeze state mismatch \
             (authority tag {mint_authority_tag}, authority {mint_authority}, \
             expected authority {}, decimals {}, initialized {}, freeze tag \
             {freeze_authority_tag})",
            pool.withdraw_authority,
            account.data[44],
            account.data[45],
        );
    }
    // Only the pool's withdraw authority can mint, so the mint supply can
    // never legitimately exceed the pool's recorded supply. It can drift
    // below it intra-epoch through direct burns; the on-chain program
    // tolerates that and re-syncs pool_token_supply from the mint at each
    // epoch's UpdateStakePoolBalance, and deposit math uses
    // pool_token_supply on both sides, so drift is harmless here.
    if supply > pool.pool_token_supply {
        bail!(
            "pool mint supply {supply} exceeds recorded pool token supply {}",
            pool.pool_token_supply,
        );
    }
    Ok(())
}

fn validate_reserve_stake(pool: &ValidatedStakePool, account: &ChainAccount) -> Result<()> {
    if account.owner != solana_stake_interface::program::id() {
        bail!("reserve stake has foreign owner");
    }
    let state: StakeStateV2 = bincode::deserialize(&account.data)
        .map_err(|_| anyhow::anyhow!("invalid reserve stake"))?;
    match state {
        StakeStateV2::Initialized(meta)
            if meta.authorized.staker == pool.withdraw_authority
                && meta.authorized.withdrawer == pool.withdraw_authority
                && meta.lockup == Default::default() =>
        {
            Ok(())
        }
        _ => bail!("reserve stake state or authorities are invalid"),
    }
}

fn validate_manager_fee(
    pool: &ValidatedStakePool,
    address: Pubkey,
    account: &ChainAccount,
) -> Result<()> {
    if address != pool.manager_fee_account
        || account.owner != token_program_id()
        || account.data.len() != 165
    {
        bail!("manager fee account has invalid program owner or length");
    }
    let mint = Pubkey::new_from_array(account.data[0..32].try_into()?);
    if mint != vsol_mint() || account.data[108] != 1 {
        bail!("manager fee token account relationships are invalid");
    }
    Ok(())
}

pub fn build_payment_plan(context: PaymentContext, limits: PaymentLimits) -> Result<PaymentPlan> {
    if context.invoices.is_empty() {
        return Ok(PaymentPlan {
            payer: context.payer,
            payer_sol_balance_lamports: context.payer_sol_balance,
            source_ata: associated_token_address(&context.payer, &vsol_mint()),
            invoices: Vec::new(),
            total_outstanding_vsol: 0,
            existing_vsol: context.existing_vsol,
            vsol_shortfall: 0,
            deposit_sol_lamports: 0,
            minimum_pool_tokens_out: 0,
            instructions: Vec::new(),
        });
    }
    if context.invoices.len() > 6 {
        bail!("at most six invoices may be paid");
    }
    let total = context.invoices.iter().try_fold(0_u64, |sum, invoice| {
        if invoice.balance_outstanding == 0
            || invoice.balance_outstanding > invoice.amount_vsol
            || invoice.invoicer != invoicer_address()
            || invoice.address
                != find_invoice_address(&invoice.invoicer, &invoice.vote_account, invoice.epoch)
        {
            bail!("invalid invoice in payment plan");
        }
        sum.checked_add(invoice.balance_outstanding)
            .ok_or_else(|| anyhow::anyhow!("invoice total overflow"))
    })?;
    if total == 0 || total > limits.max_total_vsol || total > 5_000_000_000 {
        bail!("invoice total exceeds payment cap");
    }
    if context.stake_pool.pool_mint != vsol_mint()
        || context.stake_pool.token_program_id != token_program_id()
        || context.stake_pool.sol_deposit_authority.is_some()
        || context.stake_pool.total_lamports == 0
        || context.stake_pool.pool_token_supply == 0
        || spl_stake_pool::find_withdraw_authority_program_address(
            &stake_pool_program_id(),
            &stake_pool_address(),
        ) != (
            context.stake_pool.withdraw_authority,
            context.stake_pool.withdraw_bump_seed,
        )
    {
        bail!("invalid stake pool relationships");
    }
    let expected_reserves = associated_token_address(&invoicer_address(), &vsol_mint());
    if context.invoicer_reserves != expected_reserves {
        bail!("invoicer reserves are not its vSOL ATA");
    }
    let source_ata = associated_token_address(&context.payer, &vsol_mint());
    let shortfall = total.saturating_sub(context.existing_vsol);
    let deposit_lamports = if shortfall == 0 {
        0
    } else {
        let quoted = lamports_for_net_pool_tokens(&context.stake_pool, shortfall)?;
        apply_slippage_buffer(quoted, limits.deposit_slippage_bps)?
    };
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

    let mut instructions = Vec::with_capacity(context.invoices.len() + 2);
    if !context.source_ata_exists {
        if context.existing_vsol != 0 {
            bail!("missing source ATA cannot have an existing balance");
        }
        instructions.push(create_associated_token_account_idempotent(
            &context.payer,
            &context.payer,
            &vsol_mint(),
            &token_program_id(),
        ));
    }
    if shortfall > 0 {
        let withdraw_authority = spl_stake_pool::find_withdraw_authority_program_address(
            &stake_pool_program_id(),
            &stake_pool_address(),
        )
        .0;
        instructions.push(spl_stake_pool::instruction::deposit_sol_with_slippage(
            &stake_pool_program_id(),
            &stake_pool_address(),
            &withdraw_authority,
            &context.stake_pool.reserve_stake,
            &context.payer,
            &source_ata,
            &context.stake_pool.manager_fee_account,
            &source_ata,
            &vsol_mint(),
            &token_program_id(),
            deposit_lamports,
            shortfall,
        ));
    }
    for invoice in &context.invoices {
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
        invoices: context.invoices,
        total_outstanding_vsol: total,
        existing_vsol: context.existing_vsol,
        vsol_shortfall: shortfall,
        deposit_sol_lamports: deposit_lamports,
        minimum_pool_tokens_out: shortfall,
        instructions,
    })
}

fn apply_slippage_buffer(lamports: u64, slippage_bps: u16) -> Result<u64> {
    let numerator = (lamports as u128)
        .checked_mul(u128::from(10_000_u16 + slippage_bps))
        .ok_or_else(|| anyhow::anyhow!("deposit slippage overflow"))?;
    u64::try_from(numerator.div_ceil(10_000))
        .map_err(|_| anyhow::anyhow!("deposit slippage overflow"))
}

fn lamports_for_net_pool_tokens(pool: &ValidatedStakePool, target: u64) -> Result<u64> {
    let mut high = u64::try_from(
        (target as u128)
            .checked_mul(pool.total_lamports as u128)
            .ok_or_else(|| anyhow::anyhow!("deposit quote overflow"))?
            .div_ceil(pool.pool_token_supply as u128),
    )?
    .max(1);
    while net_pool_tokens(pool, high)? < target {
        high = high
            .checked_mul(2)
            .ok_or_else(|| anyhow::anyhow!("deposit quote overflow"))?;
    }
    let mut low = 0_u64;
    while low + 1 < high {
        let mid = low + (high - low) / 2;
        if net_pool_tokens(pool, mid)? >= target {
            high = mid;
        } else {
            low = mid;
        }
    }
    Ok(high)
}

fn net_pool_tokens(pool: &ValidatedStakePool, lamports: u64) -> Result<u64> {
    let gross = u64::try_from(
        (lamports as u128)
            .checked_mul(pool.pool_token_supply as u128)
            .ok_or_else(|| anyhow::anyhow!("deposit quote overflow"))?
            / pool.total_lamports as u128,
    )?;
    let fee = if pool.sol_deposit_fee_denominator == 0 || gross == 0 {
        0
    } else {
        u64::try_from(
            (gross as u128)
                .checked_mul(pool.sol_deposit_fee_numerator as u128)
                .ok_or_else(|| anyhow::anyhow!("deposit fee overflow"))?
                .div_ceil(pool.sol_deposit_fee_denominator as u128),
        )?
    };
    gross
        .checked_sub(fee)
        .ok_or_else(|| anyhow::anyhow!("deposit fee exceeds output"))
}

pub fn pay_invoice_instruction(
    invoice: &Invoice,
    payer: Pubkey,
    source: Pubkey,
    reserves: Pubkey,
) -> Result<Instruction> {
    if invoice.balance_outstanding == 0
        || invoice.balance_outstanding > invoice.amount_vsol
        || invoice.invoicer != invoicer_address()
        || invoice.address
            != find_invoice_address(&invoice.invoicer, &invoice.vote_account, invoice.epoch)
        || source != associated_token_address(&payer, &vsol_mint())
        || reserves != associated_token_address(&invoicer_address(), &vsol_mint())
    {
        bail!("invalid pay_invoice relationships");
    }
    let mut data = Vec::with_capacity(16);
    data.extend_from_slice(&PAY_INVOICE_DISCRIMINATOR);
    data.extend_from_slice(&invoice.balance_outstanding.to_le_bytes());
    Ok(Instruction {
        program_id: crate::invoice::invoicer_program_id(),
        accounts: vec![
            AccountMeta::new_readonly(invoicer_address(), false),
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
    use crate::invoice::{Invoice, find_invoice_address, invoicer_address};
    use solana_stake_interface::state::{Authorized, Lockup, Meta, StakeStateV2};

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

    fn pool() -> ValidatedStakePool {
        let (withdraw_authority, withdraw_bump_seed) =
            spl_stake_pool::find_withdraw_authority_program_address(
                &stake_pool_program_id(),
                &stake_pool_address(),
            );
        ValidatedStakePool {
            manager: Pubkey::new_unique(),
            withdraw_authority,
            withdraw_bump_seed,
            reserve_stake: Pubkey::new_unique(),
            pool_mint: vsol_mint(),
            manager_fee_account: Pubkey::new_unique(),
            token_program_id: token_program_id(),
            total_lamports: 10_000,
            pool_token_supply: 10_000,
            sol_deposit_fee_numerator: 0,
            sol_deposit_fee_denominator: 0,
            sol_deposit_authority: None,
        }
    }

    fn context(existing_vsol: u64, payer_sol: u64) -> PaymentContext {
        PaymentContext {
            payer: Pubkey::new_unique(),
            payer_sol_balance: payer_sol,
            source_ata_exists: true,
            ata_creation_cost_lamports: 0,
            existing_vsol,
            invoices: vec![invoice(780, 10, 3), invoice(781, 10, 2)],
            invoicer_reserves: associated_token_address(&invoicer_address(), &vsol_mint()),
            stake_pool: pool(),
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
        assert_eq!(plan.minimum_pool_tokens_out, 3);
        assert_eq!(plan.instructions.len(), 3);

        let funded = build_payment_plan(context(5, 200_000_000), limits()).expect("funded");
        assert_eq!(funded.vsol_shortfall, 0);
        assert_eq!(funded.deposit_sol_lamports, 0);
        assert_eq!(funded.instructions.len(), 2);
    }

    #[test]
    fn enforces_five_vsol_cap_and_point_one_sol_reserve() {
        let mut over_cap = context(0, 10_000_000_000);
        over_cap.invoices = vec![invoice(780, 5_000_000_001, 5_000_000_001)];
        assert!(build_payment_plan(over_cap, limits()).is_err());

        let exact_required = 5 + 100_000_000 + 1_000_000;
        assert!(build_payment_plan(context(0, exact_required), limits()).is_ok());
        assert!(build_payment_plan(context(0, exact_required - 1), limits()).is_err());
    }

    #[test]
    fn reserve_check_includes_missing_ata_rent() {
        let mut ctx = context(0, 5 + 100_000_000 + 1_000_000 + 2_000_000 - 1);
        ctx.source_ata_exists = false;
        ctx.ata_creation_cost_lamports = 2_000_000;
        assert!(build_payment_plan(ctx, limits()).is_err());
    }

    #[test]
    fn creates_ata_then_slippage_deposit_then_payments_atomically() {
        let mut ctx = context(0, 200_000_000);
        ctx.source_ata_exists = false;
        ctx.ata_creation_cost_lamports = 2_000_000;
        let plan = build_payment_plan(ctx, limits()).expect("plan");
        assert_eq!(
            plan.instructions[0].program_id,
            associated_token_program_id()
        );
        assert_eq!(plan.instructions[1].program_id, stake_pool_program_id());
        assert!(
            plan.instructions[2..]
                .iter()
                .all(|ix| ix.program_id == crate::invoice::invoicer_program_id())
        );
    }

    #[test]
    fn pay_instruction_uses_outstanding_amount_and_idl_account_order() {
        let payer = Pubkey::new_unique();
        let source = associated_token_address(&payer, &vsol_mint());
        let reserves = associated_token_address(&invoicer_address(), &vsol_mint());
        let invoice = invoice(780, 99, 7);
        let ix = pay_invoice_instruction(&invoice, payer, source, reserves).expect("instruction");
        assert_eq!(&ix.data[..8], &PAY_INVOICE_DISCRIMINATOR);
        assert_eq!(u64::from_le_bytes(ix.data[8..].try_into().unwrap()), 7);
        assert_eq!(
            ix.accounts.iter().map(|a| a.pubkey).collect::<Vec<_>>(),
            vec![
                invoicer_address(),
                invoice.address,
                source,
                payer,
                reserves,
                token_program_id(),
            ]
        );
        assert!(!ix.accounts[0].is_writable);
        assert!(ix.accounts[1].is_writable);
        assert!(ix.accounts[3].is_signer);
    }

    #[test]
    fn validates_source_and_reserve_token_relationships() {
        let payer = Pubkey::new_unique();
        let source = associated_token_address(&payer, &vsol_mint());
        let source_data = token_account_data(vsol_mint(), payer, 42);
        assert_eq!(
            validate_token_account(
                source,
                &ChainAccount {
                    owner: token_program_id(),
                    data: source_data.clone(),
                },
                payer,
                vsol_mint(),
            )
            .unwrap(),
            42
        );
        let mut foreign = source_data;
        foreign[..32].copy_from_slice(Pubkey::new_unique().as_ref());
        assert!(
            validate_token_account(
                source,
                &ChainAccount {
                    owner: token_program_id(),
                    data: foreign,
                },
                payer,
                vsol_mint(),
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_malformed_stake_pool_deposit_accounts() {
        let pool = pool();
        let mint = ChainAccount {
            owner: token_program_id(),
            data: mint_data(pool.withdraw_authority, pool.pool_token_supply),
        };
        let mut reserve_data = bincode::serialize(&StakeStateV2::Initialized(Meta {
            rent_exempt_reserve: 1,
            authorized: Authorized::auto(&pool.withdraw_authority),
            lockup: Lockup::default(),
        }))
        .unwrap();
        reserve_data.resize(200, 0);
        let reserve = ChainAccount {
            owner: solana_stake_interface::program::id(),
            data: reserve_data,
        };
        let manager_fee = ChainAccount {
            owner: token_program_id(),
            data: token_account_data(vsol_mint(), pool.manager, 0),
        };
        validate_deposit_accounts(
            &pool,
            &mint,
            &reserve,
            pool.manager_fee_account,
            &manager_fee,
        )
        .expect("valid accounts");

        let mut bad_mint = mint.clone();
        bad_mint.data[4..36].copy_from_slice(Pubkey::new_unique().as_ref());
        assert!(
            validate_deposit_accounts(
                &pool,
                &bad_mint,
                &reserve,
                pool.manager_fee_account,
                &manager_fee,
            )
            .is_err()
        );
        let mut bad_mint = mint.clone();
        bad_mint.data[36..44].copy_from_slice(&10_001_u64.to_le_bytes());
        assert!(
            validate_deposit_accounts(
                &pool,
                &bad_mint,
                &reserve,
                pool.manager_fee_account,
                &manager_fee,
            )
            .is_err()
        );
        let mut bad_mint = mint.clone();
        bad_mint.data[46..50].copy_from_slice(&1_u32.to_le_bytes());
        assert!(
            validate_deposit_accounts(
                &pool,
                &bad_mint,
                &reserve,
                pool.manager_fee_account,
                &manager_fee,
            )
            .is_err()
        );

        let mut bad_reserve = reserve.clone();
        bad_reserve.owner = Pubkey::new_unique();
        assert!(
            validate_deposit_accounts(
                &pool,
                &mint,
                &bad_reserve,
                pool.manager_fee_account,
                &manager_fee,
            )
            .is_err()
        );
        let mut bad_reserve = reserve.clone();
        bad_reserve.data = bincode::serialize(&StakeStateV2::Uninitialized).unwrap();
        assert!(
            validate_deposit_accounts(
                &pool,
                &mint,
                &bad_reserve,
                pool.manager_fee_account,
                &manager_fee,
            )
            .is_err()
        );
        let mut bad_reserve = reserve.clone();
        bad_reserve.data = bincode::serialize(&StakeStateV2::Initialized(Meta {
            rent_exempt_reserve: 1,
            authorized: Authorized::auto(&Pubkey::new_unique()),
            lockup: Lockup::default(),
        }))
        .unwrap();
        assert!(
            validate_deposit_accounts(
                &pool,
                &mint,
                &bad_reserve,
                pool.manager_fee_account,
                &manager_fee,
            )
            .is_err()
        );

        assert!(
            validate_deposit_accounts(&pool, &mint, &reserve, Pubkey::new_unique(), &manager_fee,)
                .is_err()
        );
        let mut bad_fee = manager_fee.clone();
        bad_fee.data[..32].copy_from_slice(Pubkey::new_unique().as_ref());
        assert!(
            validate_deposit_accounts(&pool, &mint, &reserve, pool.manager_fee_account, &bad_fee,)
                .is_err()
        );
        let mut bad_fee = manager_fee;
        bad_fee.owner = Pubkey::new_unique();
        assert!(
            validate_deposit_accounts(&pool, &mint, &reserve, pool.manager_fee_account, &bad_fee,)
                .is_err()
        );
    }

    #[test]
    fn accepts_manager_fee_token_authority_different_from_pool_manager() {
        let pool = pool();
        let mint = ChainAccount {
            owner: token_program_id(),
            data: mint_data(pool.withdraw_authority, pool.pool_token_supply),
        };
        let mut reserve_data = bincode::serialize(&StakeStateV2::Initialized(Meta {
            rent_exempt_reserve: 1,
            authorized: Authorized::auto(&pool.withdraw_authority),
            lockup: Lockup::default(),
        }))
        .unwrap();
        reserve_data.resize(200, 0);
        let reserve = ChainAccount {
            owner: solana_stake_interface::program::id(),
            data: reserve_data,
        };
        let fee_authority = Pubkey::new_unique();
        assert_ne!(fee_authority, pool.manager);
        let manager_fee = ChainAccount {
            owner: token_program_id(),
            data: token_account_data(vsol_mint(), fee_authority, 0),
        };

        validate_deposit_accounts(
            &pool,
            &mint,
            &reserve,
            pool.manager_fee_account,
            &manager_fee,
        )
        .expect("SPL Stake Pool permits a distinct fee token authority");
    }

    #[test]
    fn decodes_real_mainnet_stake_pool_account_bytes() {
        // Captured verbatim from mainnet account
        // Fu9BYC6tWBo1KMKaP3CFoKfRhqv9akmy3DuYwnCyWiyC (611 bytes: borsh
        // payload plus allocation padding, which on-chain readers must
        // tolerate like SPL's try_from_slice_unchecked does).
        let data = include_bytes!("../tests/fixtures/mainnet_stake_pool.bin").to_vec();
        let last_update_epoch = u64::from_le_bytes(data[274..282].try_into().unwrap());
        let account = ChainAccount {
            owner: stake_pool_program_id(),
            data,
        };
        let decoded = decode_stake_pool(&account, last_update_epoch)
            .expect("real mainnet stake pool account");
        assert_eq!(decoded.pool_mint, vsol_mint());
        assert_eq!(decoded.token_program_id, token_program_id());
        assert!(decoded.total_lamports > 0 && decoded.pool_token_supply > 0);
        assert_eq!(decoded.sol_deposit_authority, None);
    }

    #[test]
    fn stake_pool_decode_rejects_wrong_withdraw_bump() {
        let raw = StakePool {
            account_type: AccountType::StakePool,
            manager: Pubkey::new_unique(),
            reserve_stake: Pubkey::new_unique(),
            pool_mint: vsol_mint(),
            manager_fee_account: Pubkey::new_unique(),
            token_program_id: token_program_id(),
            total_lamports: 1,
            pool_token_supply: 1,
            last_update_epoch: 780,
            stake_withdraw_bump_seed: pool().withdraw_bump_seed.wrapping_add(1),
            ..StakePool::default()
        };
        let account = ChainAccount {
            owner: stake_pool_program_id(),
            data: borsh::to_vec(&raw).unwrap(),
        };
        assert!(decode_stake_pool(&account, 780).is_err());
    }

    #[test]
    fn accepts_real_mainnet_mint_bytes_with_burn_drifted_pool_supply() {
        // Captured verbatim from mainnet mint
        // vSoLxydx6akxyMD9XEcPvGYNGq6Nn66oqVb3UkGkei7. Direct burns pull the
        // mint supply below the pool's recorded pool_token_supply between
        // epoch updates; the live pool showed exactly that drift (85 base
        // units on 2026-08-27) and it must not fail validation.
        let data = include_bytes!("../tests/fixtures/mainnet_vsol_mint.bin").to_vec();
        let supply = u64::from_le_bytes(data[36..44].try_into().unwrap());
        let mut pool = pool();
        pool.withdraw_authority = Pubkey::new_from_array(data[4..36].try_into().unwrap());
        pool.pool_token_supply = supply + 85;
        let mint = ChainAccount {
            owner: token_program_id(),
            data,
        };
        validate_pool_mint(&pool, &mint).expect("burn-drifted real mainnet mint");
    }

    #[test]
    fn rejects_mint_supply_above_recorded_pool_supply() {
        let mut pool = pool();
        pool.pool_token_supply = 10_000;
        let mint = ChainAccount {
            owner: token_program_id(),
            data: mint_data(pool.withdraw_authority, 10_001),
        };
        let error = validate_pool_mint(&pool, &mint).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("exceeds recorded pool token supply")
        );
    }

    fn mint_data(authority: Pubkey, supply: u64) -> Vec<u8> {
        let mut data = vec![0; 82];
        data[..4].copy_from_slice(&1_u32.to_le_bytes());
        data[4..36].copy_from_slice(authority.as_ref());
        data[36..44].copy_from_slice(&supply.to_le_bytes());
        data[44] = 9;
        data[45] = 1;
        data
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
