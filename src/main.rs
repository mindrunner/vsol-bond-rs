use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use solana_client::rpc_client::RpcClient;
use solana_keypair::read_keypair_file;
use solana_signer::Signer;
use std::{
    path::PathBuf,
    process::ExitCode,
    time::{SystemTime, UNIX_EPOCH},
};
use vsol_bond_rs::{
    RpcChainDataSource,
    config::Config,
    metrics::{RunMetrics, RunPhase, write_metrics},
    payment::PaymentPlan,
    plan, submit_payment,
    transaction::RpcTxSubmitter,
};

#[derive(Debug, Parser)]
#[command(about = "Guarded vSOL invoice payment automation")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Inspect and validate a payment plan without loading the payer keypair or submitting.
    Plan {
        #[arg(long)]
        config: PathBuf,
    },
    /// Explicitly sign, submit, and confirm the validated payment transaction.
    Pay {
        #[arg(long)]
        config: PathBuf,
    },
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    let (config_path, phase) = match &cli.command {
        Command::Plan { config } => (config, RunPhase::Plan),
        Command::Pay { config } => (config, RunPhase::Pay),
    };
    let config = Config::load(config_path)?;
    let client = RpcClient::new(config.rpc_url.clone());
    let source = RpcChainDataSource { client: &client };
    let planned = plan(&source, &config);
    let payment_plan = match planned {
        Ok(plan) => plan,
        Err(error) => {
            record_failure(&config, phase, None)?;
            return Err(error);
        }
    };

    match cli.command {
        Command::Plan { .. } => {
            record(&config, metrics_for(&payment_plan, phase, true, 0))?;
            print_plan(&payment_plan);
            Ok(())
        }
        Command::Pay { .. } => {
            let payer = read_keypair_file(&config.payer_keypair_path).map_err(|_| {
                anyhow::anyhow!(
                    "failed to read payer keypair {}",
                    config.payer_keypair_path.display()
                )
            });
            let payer = match payer {
                Ok(payer) => payer,
                Err(error) => {
                    record_failure(&config, phase, Some(&payment_plan))?;
                    return Err(error);
                }
            };
            if payer.pubkey() != config.payer_pubkey {
                record_failure(&config, phase, Some(&payment_plan))?;
                bail!("payer keypair does not match configured payer_pubkey");
            }
            let submitter = RpcTxSubmitter {
                client: &client,
                payer: &payer,
                priority_fee_micro_lamports: config.priority_fee_micro_lamports,
                max_attempts: 3,
            };
            match submit_payment(&submitter, &payment_plan) {
                Ok(signature) => {
                    let paid = if signature.is_some() {
                        payment_plan.invoices.len()
                    } else {
                        0
                    };
                    record(&config, metrics_for(&payment_plan, phase, true, paid))?;
                    println!(
                        "paid {paid} invoice(s); deposited {} lamports",
                        payment_plan.deposit_sol_lamports
                    );
                    Ok(())
                }
                Err(error) => {
                    record_failure(&config, phase, Some(&payment_plan))?;
                    Err(error)
                }
            }
        }
    }
}

fn print_plan(plan: &PaymentPlan) {
    println!(
        "plan: {} invoice(s), {} vSOL base units outstanding, {} existing, {} shortfall, {} SOL lamports to deposit",
        plan.invoices.len(),
        plan.total_outstanding_vsol,
        plan.existing_vsol,
        plan.vsol_shortfall,
        plan.deposit_sol_lamports
    );
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn metrics_for(
    plan: &PaymentPlan,
    phase: RunPhase,
    success: bool,
    paid_invoice_count: usize,
) -> RunMetrics {
    RunMetrics {
        last_run_timestamp_seconds: now_seconds(),
        success,
        phase,
        discovered_invoice_count: plan.invoices.len(),
        paid_invoice_count,
        outstanding_vsol_base_units: plan.total_outstanding_vsol,
        deposited_sol_lamports: if success && paid_invoice_count > 0 {
            plan.deposit_sol_lamports
        } else {
            0
        },
        payer_sol_balance_lamports: plan.payer_sol_balance_lamports,
        payer_vsol_balance_base_units: plan.existing_vsol,
    }
}

fn record_failure(config: &Config, phase: RunPhase, plan: Option<&PaymentPlan>) -> Result<()> {
    let metrics = if let Some(plan) = plan {
        metrics_for(plan, phase, false, 0)
    } else {
        RunMetrics {
            last_run_timestamp_seconds: now_seconds(),
            success: false,
            phase,
            discovered_invoice_count: 0,
            paid_invoice_count: 0,
            outstanding_vsol_base_units: 0,
            deposited_sol_lamports: 0,
            payer_sol_balance_lamports: 0,
            payer_vsol_balance_base_units: 0,
        }
    };
    record(config, metrics)
}

fn record(config: &Config, metrics: RunMetrics) -> Result<()> {
    write_metrics(&config.metrics_path, &metrics)
        .with_context(|| format!("failed to update metrics {}", config.metrics_path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::path::PathBuf;

    #[test]
    fn parses_explicit_plan_and_pay_commands() {
        for name in ["plan", "pay"] {
            let cli = Cli::try_parse_from(["vsol-bond-rs", name, "--config", "/tmp/config.toml"])
                .expect("parse command");
            match cli.command {
                Command::Plan { config } | Command::Pay { config } => {
                    assert_eq!(config, PathBuf::from("/tmp/config.toml"));
                }
            }
        }
    }

    #[test]
    fn refuses_implicit_mutating_mode() {
        assert!(Cli::try_parse_from(["vsol-bond-rs", "--config", "/tmp/config.toml"]).is_err());
    }
}
