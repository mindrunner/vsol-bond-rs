pub mod config;
pub mod invoice;
pub mod metrics;
pub mod payment;
pub mod transaction;

use anyhow::{Context, Result, bail};
use config::Config;
use invoice::{
    ChainAccount, candidate_epochs, decode_invoice, decode_invoicer, filter_invoices,
    find_invoice_address, invoicer_address,
};
use payment::{
    PaymentContext, PaymentLimits, PaymentPlan, associated_token_address, build_payment_plan,
    decode_stake_pool, stake_pool_address, validate_token_account, validate_vsol_mint, vsol_mint,
};
use solana_client::rpc_client::RpcClient;
use solana_signature::Signature;
use transaction::TxSubmitter;

pub trait ChainDataSource {
    fn load_payment_context(&self, config: &Config) -> Result<PaymentContext>;
}

pub struct RpcChainDataSource<'a> {
    pub client: &'a RpcClient,
}

impl ChainDataSource for RpcChainDataSource<'_> {
    fn load_payment_context(&self, config: &Config) -> Result<PaymentContext> {
        let current_epoch = self
            .client
            .get_epoch_info()
            .context("failed to fetch epoch info")?
            .epoch;
        let epochs = candidate_epochs(current_epoch);
        let source_ata = associated_token_address(&config.payer_pubkey, &vsol_mint());
        let expected_reserves = associated_token_address(&invoicer_address(), &vsol_mint());
        let mut addresses = vec![
            invoicer_address(),
            vsol_mint(),
            stake_pool_address(),
            source_ata,
            expected_reserves,
        ];
        addresses.extend(
            epochs.iter().map(|epoch| {
                find_invoice_address(&invoicer_address(), &config.vote_account, *epoch)
            }),
        );
        let accounts = self
            .client
            .get_multiple_accounts(&addresses)
            .context("failed to fetch guarded payment accounts")?;
        if accounts.len() != addresses.len() {
            bail!("RPC returned an unexpected account count");
        }
        let account = |index: usize, name: &str| -> Result<ChainAccount> {
            let value = accounts[index]
                .as_ref()
                .with_context(|| format!("missing {name} account"))?;
            Ok(ChainAccount {
                owner: value.owner,
                data: value.data.clone(),
            })
        };

        let invoicer_account = account(0, "invoicer")?;
        let invoicer = decode_invoicer(invoicer_address(), &invoicer_account)?;
        if invoicer.vsol_reserves != expected_reserves {
            bail!("invoicer reserves do not match its vSOL ATA");
        }
        validate_vsol_mint(&account(1, "vSOL mint")?)?;
        let stake_pool = decode_stake_pool(&account(2, "stake pool")?, current_epoch)?;
        let (source_ata_exists, existing_vsol) = match accounts[3].as_ref() {
            Some(value) => {
                let account = ChainAccount {
                    owner: value.owner,
                    data: value.data.clone(),
                };
                (
                    true,
                    validate_token_account(source_ata, &account, config.payer_pubkey, vsol_mint())?,
                )
            }
            None => (false, 0),
        };
        let ata_creation_cost_lamports = if source_ata_exists {
            0
        } else {
            self.client
                .get_minimum_balance_for_rent_exemption(165)
                .context("failed to fetch token-account rent requirement")?
        };
        let reserves_account = account(4, "invoicer reserves")?;
        validate_token_account(
            expected_reserves,
            &reserves_account,
            invoicer_address(),
            vsol_mint(),
        )?;

        let mut invoices = Vec::new();
        for (offset, epoch) in epochs.iter().enumerate() {
            if let Some(value) = accounts[5 + offset].as_ref() {
                let account = ChainAccount {
                    owner: value.owner,
                    data: value.data.clone(),
                };
                let address =
                    find_invoice_address(&invoicer_address(), &config.vote_account, *epoch);
                invoices.push(decode_invoice(
                    address,
                    &account,
                    config.vote_account,
                    *epoch,
                )?);
            }
        }
        let invoices = filter_invoices(invoices, current_epoch, config.max_invoices)?;
        let payer_sol_balance = self
            .client
            .get_balance(&config.payer_pubkey)
            .context("failed to fetch payer SOL balance")?;
        Ok(PaymentContext {
            payer: config.payer_pubkey,
            payer_sol_balance,
            source_ata_exists,
            ata_creation_cost_lamports,
            existing_vsol,
            invoices,
            invoicer_reserves: expected_reserves,
            stake_pool,
        })
    }
}

pub fn plan<S: ChainDataSource>(source: &S, config: &Config) -> Result<PaymentPlan> {
    config.validate()?;
    let context = source.load_payment_context(config)?;
    if context.payer != config.payer_pubkey {
        bail!("data source returned the wrong payer");
    }
    build_payment_plan(
        context,
        PaymentLimits {
            max_total_vsol: config.max_total_vsol,
            min_sol_reserve_lamports: config.min_sol_reserve_lamports,
            deposit_slippage_bps: config.deposit_slippage_bps,
            transaction_fee_buffer_lamports: config.transaction_fee_buffer_lamports,
        },
    )
}

pub fn submit_payment<T: TxSubmitter>(
    submitter: &T,
    plan: &PaymentPlan,
) -> Result<Option<Signature>> {
    if plan.instructions.is_empty() {
        return Ok(None);
    }
    submitter.submit(&plan.instructions).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::Config,
        invoice::{Invoice, find_invoice_address, invoicer_address},
        payment::{
            PaymentContext, ValidatedStakePool, associated_token_address, token_program_id,
            vsol_mint,
        },
        transaction::TxSubmitter,
    };
    use anyhow::Result;
    use solana_instruction::Instruction;
    use solana_pubkey::Pubkey;
    use solana_signature::Signature;
    use std::{cell::Cell, path::PathBuf};

    struct StaticSource;

    impl ChainDataSource for StaticSource {
        fn load_payment_context(&self, config: &Config) -> Result<PaymentContext> {
            let payer = config.payer_pubkey;
            let vote = Pubkey::new_unique();
            Ok(PaymentContext {
                payer,
                payer_sol_balance: 200_000_000,
                source_ata_exists: true,
                ata_creation_cost_lamports: 0,
                existing_vsol: 1,
                invoices: vec![Invoice {
                    address: find_invoice_address(&invoicer_address(), &vote, 780),
                    invoicer: invoicer_address(),
                    vote_account: vote,
                    epoch: 780,
                    amount_vsol: 1,
                    balance_outstanding: 1,
                }],
                invoicer_reserves: associated_token_address(&invoicer_address(), &vsol_mint()),
                stake_pool: ValidatedStakePool {
                    reserve_stake: Pubkey::new_unique(),
                    pool_mint: vsol_mint(),
                    manager_fee_account: Pubkey::new_unique(),
                    token_program_id: token_program_id(),
                    total_lamports: 1,
                    pool_token_supply: 1,
                    sol_deposit_fee_numerator: 0,
                    sol_deposit_fee_denominator: 0,
                    sol_deposit_authority: None,
                },
            })
        }
    }

    struct RecordingSubmitter(Cell<usize>);

    impl TxSubmitter for RecordingSubmitter {
        fn submit(&self, _instructions: &[Instruction]) -> Result<Signature> {
            self.0.set(self.0.get() + 1);
            Ok(Signature::from([1; 64]))
        }
    }

    #[test]
    fn plan_never_calls_submit() {
        let submitter = RecordingSubmitter(Cell::new(0));
        let config = Config {
            rpc_url: "https://rpc.example.test".into(),
            vote_account: Pubkey::new_unique(),
            payer_pubkey: Pubkey::new_unique(),
            payer_keypair_path: PathBuf::from("/secret/payer.json"),
            metrics_path: PathBuf::from("/tmp/vsol.prom"),
            max_total_vsol: 5_000_000_000,
            min_sol_reserve_lamports: 100_000_000,
            max_invoices: 6,
            priority_fee_micro_lamports: 1_000,
            deposit_slippage_bps: 50,
            transaction_fee_buffer_lamports: 1_000_000,
        };
        let plan = plan(&StaticSource, &config).expect("plan");
        assert_eq!(plan.total_outstanding_vsol, 1);
        assert_eq!(submitter.0.get(), 0);
    }
}
