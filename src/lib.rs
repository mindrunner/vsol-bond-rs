pub mod config;
pub mod invoice;
pub mod metrics;
pub mod payment;
pub mod transaction;

use anyhow::{Context, Result, bail};
use config::Config;
use invoice::{
    ChainAccount, INVOICE_DISCRIMINATOR, INVOICE_VOTE_OFFSET, Invoice, Invoicer, candidate_epochs,
    decode_invoice, decode_invoice_unchecked, decode_invoicer, filter_invoices,
    find_invoice_address, invoicer_program_id, summarize_unpaid, trusted_invoicers,
};
use payment::{
    PaymentContext, PaymentLimits, PaymentPlan, associated_token_address, build_payment_plan,
    decode_vlp_pool, validate_token_account, validate_token_account_at,
    vlp_inventory_summary_address, vlp_mint, vlp_pool_address,
};
use solana_client::{
    rpc_client::RpcClient,
    rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig, UiAccountEncoding},
    rpc_filter::{Memcmp, RpcFilterType},
};
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use std::str::FromStr;
use transaction::TxSubmitter;

/// getMultipleAccounts accepts at most 100 addresses per request.
const MAX_MULTIPLE_ACCOUNTS: usize = 100;

pub trait ChainDataSource {
    fn load_payment_context(&self, config: &Config) -> Result<PaymentContext>;
}

pub struct RpcChainDataSource<'a> {
    pub client: &'a RpcClient,
}

impl RpcChainDataSource<'_> {
    fn scan_program(&self, filters: Vec<RpcFilterType>) -> Result<Vec<(Pubkey, ChainAccount)>> {
        let accounts = self
            .client
            .get_program_ui_accounts_with_config(
                &invoicer_program_id(),
                RpcProgramAccountsConfig {
                    filters: Some(filters),
                    account_config: RpcAccountInfoConfig {
                        encoding: Some(UiAccountEncoding::Base64),
                        ..RpcAccountInfoConfig::default()
                    },
                    ..RpcProgramAccountsConfig::default()
                },
            )
            .context("failed to scan the invoicer program")?;
        accounts
            .into_iter()
            .map(|(address, account)| {
                let data = account
                    .data
                    .decode()
                    .with_context(|| format!("undecodable account data for {address}"))?;
                let owner = Pubkey::from_str(&account.owner)
                    .with_context(|| format!("invalid owner for {address}"))?;
                Ok((address, ChainAccount { owner, data }))
            })
            .collect()
    }

    /// The trusted invoicers that exist on chain and settle in VLP. A missing
    /// one is skipped (it cannot have billed us); one with another settlement
    /// mint is refused, since Vault's billing is VLP only since epoch 1064.
    fn trusted_invoicers(&self) -> Result<Vec<Invoicer>> {
        let addresses = trusted_invoicers();
        let accounts = self.get_accounts(&addresses)?;
        let mut invoicers = Vec::new();
        for (address, account) in addresses.into_iter().zip(accounts) {
            let Some(account) = account else { continue };
            let invoicer = decode_invoicer(address, &account)?;
            if invoicer.mint != vlp_mint() {
                bail!(
                    "trusted invoicer {address} settles in {}, not VLP; refusing to pay",
                    invoicer.mint
                );
            }
            invoicers.push(invoicer);
        }
        Ok(invoicers)
    }

    /// Every invoice for the vote account, from any invoicer. Reporting only.
    fn scan_invoices(&self, vote_account: &Pubkey) -> Result<Vec<Invoice>> {
        self.scan_program(vec![
            RpcFilterType::Memcmp(Memcmp::new_base58_encoded(0, &INVOICE_DISCRIMINATOR)),
            RpcFilterType::Memcmp(Memcmp::new_base58_encoded(
                INVOICE_VOTE_OFFSET,
                vote_account.as_ref(),
            )),
        ])?
        .into_iter()
        .map(|(address, account)| decode_invoice_unchecked(address, &account))
        .collect()
    }

    fn get_accounts(&self, addresses: &[Pubkey]) -> Result<Vec<Option<ChainAccount>>> {
        let mut out = Vec::with_capacity(addresses.len());
        for chunk in addresses.chunks(MAX_MULTIPLE_ACCOUNTS) {
            let accounts = self
                .client
                .get_multiple_accounts(chunk)
                .context("failed to fetch guarded payment accounts")?;
            if accounts.len() != chunk.len() {
                bail!("RPC returned an unexpected account count");
            }
            out.extend(accounts.into_iter().map(|value| {
                value.map(|value| ChainAccount {
                    owner: value.owner,
                    data: value.data,
                })
            }));
        }
        Ok(out)
    }
}

impl ChainDataSource for RpcChainDataSource<'_> {
    fn load_payment_context(&self, config: &Config) -> Result<PaymentContext> {
        let current_epoch = self
            .client
            .get_epoch_info()
            .context("failed to fetch epoch info")?
            .epoch;
        let epochs = candidate_epochs(current_epoch);

        let invoicers = self.trusted_invoicers()?;
        if invoicers.is_empty() {
            bail!("none of the trusted invoicers exists on chain");
        }
        let invoice_addresses = invoicers
            .iter()
            .flat_map(|invoicer| {
                epochs.iter().map(|epoch| {
                    find_invoice_address(&invoicer.address, &config.vote_account, *epoch)
                })
            })
            .collect::<Vec<_>>();
        let invoice_accounts = self.get_accounts(&invoice_addresses)?;
        // Pay one invoicer per run: the one holding the oldest payable invoice.
        let mut selected: Option<(&Invoicer, Vec<Invoice>)> = None;
        for (index, invoicer) in invoicers.iter().enumerate() {
            let mut invoices = Vec::new();
            for (offset, epoch) in epochs.iter().enumerate() {
                let slot = index * epochs.len() + offset;
                if let Some(account) = invoice_accounts[slot].as_ref() {
                    invoices.push(decode_invoice(
                        invoice_addresses[slot],
                        account,
                        invoicer.address,
                        config.vote_account,
                        *epoch,
                    )?);
                }
            }
            let invoices = filter_invoices(invoices, current_epoch, config.max_invoices)?;
            let Some(oldest) = invoices.first().map(|invoice| invoice.epoch) else {
                continue;
            };
            if selected
                .as_ref()
                .is_none_or(|(_, current)| oldest < current[0].epoch)
            {
                selected = Some((invoicer, invoices));
            }
        }
        let (invoicer, invoices) = selected.unwrap_or((&invoicers[0], Vec::new()));
        let reserves = invoicer.vsol_reserves;

        let source_ata = associated_token_address(&config.payer_pubkey, &vlp_mint());
        let addresses = [
            vlp_pool_address(),
            vlp_inventory_summary_address(),
            source_ata,
            reserves,
        ];
        let accounts = self.get_accounts(&addresses)?;
        let account = |index: usize, name: &str| -> Result<ChainAccount> {
            accounts[index]
                .clone()
                .with_context(|| format!("missing {name} account"))
        };
        let vlp_pool = decode_vlp_pool(
            &account(0, "VLP pool")?,
            &account(1, "VLP inventory summary")?,
        )?;
        let (source_ata_exists, existing_vsol) = match accounts[2].as_ref() {
            Some(account) => (
                true,
                validate_token_account(source_ata, account, config.payer_pubkey, vlp_mint())?,
            ),
            None => (false, 0),
        };
        let ata_creation_cost_lamports = if source_ata_exists {
            0
        } else {
            self.client
                .get_minimum_balance_for_rent_exemption(165)
                .context("failed to fetch token-account rent requirement")?
        };
        validate_token_account_at(
            reserves,
            &account(3, "invoicer reserves")?,
            invoicer.address,
            vlp_mint(),
        )?;

        let unpaid = summarize_unpaid(
            &self.scan_invoices(&config.vote_account)?,
            config.vote_account,
        );
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
            invoicer: invoicer.address,
            invoicer_reserves: reserves,
            vlp_pool,
            current_epoch,
            unpaid,
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
        payment::{PaymentContext, ValidatedVlpPool, vlp_inventory_summary_address},
    };
    use anyhow::Result;
    use solana_pubkey::Pubkey;
    use std::path::PathBuf;

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
                invoicer: invoicer_address(),
                invoicer_reserves: Pubkey::new_unique(),
                current_epoch: 781,
                unpaid: crate::invoice::UnpaidInvoices {
                    count: 1,
                    oldest_epoch: 780,
                },
                vlp_pool: ValidatedVlpPool {
                    sol_vault: crate::payment::vlp_sol_vault_address(),
                    inventory_summary: vlp_inventory_summary_address(),
                    value_lamports: 1,
                    total_lp_tokens: 1,
                    sol_vault_lamports: 0,
                    sol_vault_lamports_cap: u64::MAX,
                },
            })
        }
    }

    #[test]
    fn pure_planner_builds_without_a_submission_dependency() {
        let config = Config {
            rpc_url: "https://rpc.example.test".into(),
            vote_account: Pubkey::new_unique(),
            payer_pubkey: Pubkey::new_unique(),
            payer_keypair_path: PathBuf::from("/secret/payer.json"),
            metrics_path: PathBuf::from("/tmp/vsol.prom"),
            max_total_vsol: 5_000_000_000,
            min_sol_reserve_lamports: 100_000_000,
            max_invoices: 6,
            max_attempts: 3,
            priority_fee_micro_lamports: 1_000,
            deposit_slippage_bps: 50,
            transaction_fee_buffer_lamports: 1_000_000,
        };
        let plan = plan(&StaticSource, &config).expect("plan");
        assert_eq!(plan.total_outstanding_vsol, 1);
    }
}
