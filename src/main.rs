use anyhow::Result;
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
    metrics::{FileMetricsSink, MetricsSink, RunMetrics, RunPhase},
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
    match run_with_sink(Cli::parse(), &FileMetricsSink) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run_with_sink<S: MetricsSink>(cli: Cli, sink: &S) -> Result<()> {
    let (config_path, phase) = match &cli.command {
        Command::Plan { config } => (config, RunPhase::Plan),
        Command::Pay { config } => (config, RunPhase::Pay),
    };
    let config = match Config::load(config_path) {
        Ok(config) => config,
        Err(error) => {
            if let Some(metrics_path) = Config::metrics_path_hint(config_path) {
                return Err(operational_failure(
                    sink,
                    &metrics_path,
                    failure_metrics(phase, None),
                    error,
                ));
            }
            return Err(error);
        }
    };
    if let Err(error) = sink.preflight(&config.metrics_path) {
        return Err(operational_failure(
            sink,
            &config.metrics_path,
            failure_metrics(phase, None),
            error,
        ));
    }
    let client = RpcClient::new(config.rpc_url.clone());
    let source = RpcChainDataSource { client: &client };
    let planned = plan(&source, &config);
    let payment_plan = match planned {
        Ok(plan) => plan,
        Err(error) => {
            return Err(operational_failure(
                sink,
                &config.metrics_path,
                failure_metrics(phase, None),
                error,
            ));
        }
    };

    match cli.command {
        Command::Plan { .. } => {
            record_success(
                sink,
                &config.metrics_path,
                metrics_for(&payment_plan, phase, true, 0),
                false,
            )?;
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
                    return Err(operational_failure(
                        sink,
                        &config.metrics_path,
                        failure_metrics(phase, Some(&payment_plan)),
                        error,
                    ));
                }
            };
            if payer.pubkey() != config.payer_pubkey {
                return Err(operational_failure(
                    sink,
                    &config.metrics_path,
                    failure_metrics(phase, Some(&payment_plan)),
                    anyhow::anyhow!("payer keypair does not match configured payer_pubkey"),
                ));
            }
            let submitter = RpcTxSubmitter {
                client: &client,
                payer: &payer,
                priority_fee_micro_lamports: config.priority_fee_micro_lamports,
                max_attempts: config.max_attempts,
            };
            match submit_payment(&submitter, &payment_plan) {
                Ok(signature) => {
                    let paid = if signature.is_some() {
                        payment_plan.invoices.len()
                    } else {
                        0
                    };
                    record_success(
                        sink,
                        &config.metrics_path,
                        metrics_for(&payment_plan, phase, true, paid),
                        signature.is_some(),
                    )?;
                    println!(
                        "paid {paid} invoice(s); deposited {} lamports",
                        payment_plan.deposit_sol_lamports
                    );
                    Ok(())
                }
                Err(error) => Err(operational_failure(
                    sink,
                    &config.metrics_path,
                    failure_metrics(phase, Some(&payment_plan)),
                    error,
                )),
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

fn failure_metrics(phase: RunPhase, plan: Option<&PaymentPlan>) -> RunMetrics {
    if let Some(plan) = plan {
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
    }
}

fn operational_failure<S: MetricsSink>(
    sink: &S,
    path: &std::path::Path,
    metrics: RunMetrics,
    primary: anyhow::Error,
) -> anyhow::Error {
    match sink.write(path, &metrics) {
        Ok(()) => primary,
        Err(metrics_error) => anyhow::anyhow!(
            "{primary:#}; additionally failed to update failure metrics: {metrics_error}"
        ),
    }
}

fn record_success<S: MetricsSink>(
    sink: &S,
    path: &std::path::Path,
    metrics: RunMetrics,
    payment_confirmed: bool,
) -> Result<()> {
    sink.write(path, &metrics).map_err(|error| {
        if payment_confirmed {
            anyhow::anyhow!("payment confirmed successfully; metrics update failed: {error}")
        } else {
            anyhow::anyhow!("operation succeeded; metrics update failed: {error}")
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use clap::Parser;
    use solana_pubkey::Pubkey;
    use solana_stake_interface::state::{Authorized, Lockup, Meta, StakeStateV2};
    use spl_stake_pool::state::{AccountType, StakePool};
    use std::{
        cell::{Cell, RefCell},
        path::{Path, PathBuf},
        sync::{Arc, Mutex},
        thread::JoinHandle,
    };
    use tiny_http::{Response, Server};
    use vsol_bond_rs::{
        invoice::{
            INVOICER_ACCOUNT_LEN, INVOICER_DISCRIMINATOR, find_invoicer_address, invoicer_address,
            invoicer_base, invoicer_program_id,
        },
        payment::{
            associated_token_address, stake_pool_address, stake_pool_program_id, token_program_id,
            vsol_mint,
        },
    };

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

    struct FailingSink {
        writes: Cell<usize>,
    }

    impl MetricsSink for FailingSink {
        fn preflight(&self, _path: &std::path::Path) -> Result<()> {
            Err(anyhow::anyhow!("metrics preflight denied"))
        }

        fn write(&self, _path: &std::path::Path, _metrics: &RunMetrics) -> Result<()> {
            self.writes.set(self.writes.get() + 1);
            Err(anyhow::anyhow!("metrics sink denied"))
        }
    }

    struct RecordingSink {
        preflight_error: Option<&'static str>,
        writes: Cell<usize>,
        successes: RefCell<Vec<bool>>,
    }

    impl MetricsSink for RecordingSink {
        fn preflight(&self, _path: &Path) -> Result<()> {
            match self.preflight_error {
                Some(message) => Err(anyhow::anyhow!(message)),
                None => Ok(()),
            }
        }

        fn write(&self, _path: &Path, metrics: &RunMetrics) -> Result<()> {
            self.writes.set(self.writes.get() + 1);
            self.successes.borrow_mut().push(metrics.success);
            Ok(())
        }
    }

    #[test]
    fn failure_metrics_never_mask_primary_operational_error() {
        let sink = FailingSink {
            writes: Cell::new(0),
        };
        let error = operational_failure(
            &sink,
            std::path::Path::new("/metrics.prom"),
            failure_metrics(RunPhase::Pay, None),
            anyhow::anyhow!("primary RPC failure"),
        );
        assert!(error.to_string().contains("primary RPC failure"));
        assert!(error.to_string().contains("metrics sink denied"));
        assert_eq!(sink.writes.get(), 1);
    }

    #[test]
    fn successful_payment_metrics_failure_is_reported_as_observability_failure() {
        let sink = FailingSink {
            writes: Cell::new(0),
        };
        let mut metrics = failure_metrics(RunPhase::Pay, None);
        metrics.success = true;
        let error = record_success(&sink, std::path::Path::new("/metrics.prom"), metrics, true)
            .expect_err("sink failure");
        assert!(error.to_string().contains("payment confirmed successfully"));
        assert!(error.to_string().contains("metrics sink denied"));
    }

    #[test]
    fn invalid_config_with_metrics_hint_records_failure() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("invalid.toml");
        std::fs::write(
            &config,
            format!(
                "metrics_path = \"{}\"\nrpc_url = \"not-a-url\"\n",
                dir.path().join("metrics.prom").display()
            ),
        )
        .unwrap();
        let sink = RecordingSink {
            preflight_error: None,
            writes: Cell::new(0),
            successes: RefCell::new(Vec::new()),
        };
        let error = run_with_sink(
            Cli {
                command: Command::Pay { config },
            },
            &sink,
        )
        .expect_err("invalid config");
        assert!(error.to_string().contains("invalid config"));
        assert_eq!(sink.writes.get(), 1);
        assert_eq!(&*sink.successes.borrow(), &[false]);
    }

    #[test]
    fn metrics_preflight_failure_stops_before_rpc_and_records_failure_best_effort() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(
            &config,
            format!(
                "rpc_url = \"http://127.0.0.1:9\"\nvote_account = \"11111111111111111111111111111111\"\npayer_pubkey = \"Vote111111111111111111111111111111111111111\"\npayer_keypair_path = \"/missing\"\nmetrics_path = \"{}\"\n",
                dir.path().join("metrics.prom").display()
            ),
        )
        .unwrap();
        let sink = RecordingSink {
            preflight_error: Some("metrics preflight denied"),
            writes: Cell::new(0),
            successes: RefCell::new(Vec::new()),
        };
        let error = run_with_sink(
            Cli {
                command: Command::Pay { config },
            },
            &sink,
        )
        .expect_err("preflight");
        assert!(error.to_string().contains("metrics preflight denied"));
        assert_eq!(sink.writes.get(), 1);
        assert_eq!(&*sink.successes.borrow(), &[false]);
    }

    struct PlanRpcFixture {
        url: String,
        payer: Pubkey,
        vote: Pubkey,
        methods: Arc<Mutex<Vec<String>>>,
        handle: JoinHandle<()>,
    }

    impl PlanRpcFixture {
        fn config(&self, metrics_path: &Path, payer_path: PathBuf) -> String {
            format!(
                "rpc_url = \"{}\"\nvote_account = \"{}\"\npayer_pubkey = \"{}\"\npayer_keypair_path = \"{}\"\nmetrics_path = \"{}\"\n",
                self.url,
                self.vote,
                self.payer,
                payer_path.display(),
                metrics_path.display()
            )
        }

        fn finish(self) -> Vec<String> {
            self.handle.join().unwrap();
            Arc::try_unwrap(self.methods).unwrap().into_inner().unwrap()
        }
    }

    fn start_plan_rpc_stub() -> PlanRpcFixture {
        let server = Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}", server.server_addr());
        let payer = Pubkey::new_unique();
        let vote = Pubkey::new_unique();
        let manager = Pubkey::new_unique();
        let manager_fee = Pubkey::new_unique();
        let reserve = Pubkey::new_unique();
        let (withdraw_authority, withdraw_bump) =
            spl_stake_pool::find_withdraw_authority_program_address(
                &stake_pool_program_id(),
                &stake_pool_address(),
            );
        let reserves = associated_token_address(&invoicer_address(), &vsol_mint());

        let mut invoicer_data = Vec::with_capacity(INVOICER_ACCOUNT_LEN);
        invoicer_data.extend_from_slice(&INVOICER_DISCRIMINATOR);
        invoicer_data.extend_from_slice(invoicer_base().as_ref());
        invoicer_data.push(find_invoicer_address().1);
        invoicer_data.extend_from_slice(&[0; 7]);
        invoicer_data.extend_from_slice(reserves.as_ref());
        for fill in 1..=4 {
            invoicer_data.extend_from_slice(&[fill; 32]);
        }

        let mut mint_data = vec![0; 82];
        mint_data[..4].copy_from_slice(&1_u32.to_le_bytes());
        mint_data[4..36].copy_from_slice(withdraw_authority.as_ref());
        mint_data[36..44].copy_from_slice(&10_000_u64.to_le_bytes());
        mint_data[44] = 9;
        mint_data[45] = 1;

        let pool = StakePool {
            account_type: AccountType::StakePool,
            manager,
            stake_withdraw_bump_seed: withdraw_bump,
            reserve_stake: reserve,
            pool_mint: vsol_mint(),
            manager_fee_account: manager_fee,
            token_program_id: token_program_id(),
            total_lamports: 10_000,
            pool_token_supply: 10_000,
            last_update_epoch: 780,
            ..StakePool::default()
        };
        let pool_data = borsh::to_vec(&pool).unwrap();

        let token_data = |owner: Pubkey, amount: u64| {
            let mut data = vec![0; 165];
            data[..32].copy_from_slice(vsol_mint().as_ref());
            data[32..64].copy_from_slice(owner.as_ref());
            data[64..72].copy_from_slice(&amount.to_le_bytes());
            data[108] = 1;
            data
        };
        let mut reserve_data = bincode::serialize(&StakeStateV2::Initialized(Meta {
            rent_exempt_reserve: 1,
            authorized: Authorized::auto(&withdraw_authority),
            lockup: Lockup::default(),
        }))
        .unwrap();
        reserve_data.resize(200, 0);
        let first_accounts = vec![
            rpc_account(invoicer_program_id(), invoicer_data),
            rpc_account(token_program_id(), mint_data),
            rpc_account(stake_pool_program_id(), pool_data),
            rpc_account(token_program_id(), token_data(payer, 0)),
            rpc_account(token_program_id(), token_data(invoicer_address(), 0)),
        ];
        let second_accounts = vec![
            rpc_account(solana_stake_interface::program::id(), reserve_data),
            rpc_account(token_program_id(), token_data(manager, 0)),
        ];
        let methods = Arc::new(Mutex::new(Vec::new()));
        let thread_methods = Arc::clone(&methods);
        let handle = std::thread::spawn(move || {
            let mut account_call = 0;
            for mut request in server.incoming_requests().take(4) {
                let mut body = String::new();
                request.as_reader().read_to_string(&mut body).unwrap();
                let payload: serde_json::Value = serde_json::from_str(&body).unwrap();
                let method = payload["method"].as_str().unwrap().to_string();
                thread_methods.lock().unwrap().push(method.clone());
                let result = match method.as_str() {
                    "getEpochInfo" => serde_json::json!({
                        "absoluteSlot": 1,
                        "blockHeight": 1,
                        "epoch": 780,
                        "slotIndex": 0,
                        "slotsInEpoch": 432000,
                        "transactionCount": 0
                    }),
                    "getMultipleAccounts" => {
                        account_call += 1;
                        let value = if account_call == 1 {
                            first_accounts.clone()
                        } else {
                            second_accounts.clone()
                        };
                        serde_json::json!({"context": {"slot": 1}, "value": value})
                    }
                    "getBalance" => {
                        serde_json::json!({"context": {"slot": 1}, "value": 200000000})
                    }
                    other => panic!("unexpected plan RPC method {other}"),
                };
                let response = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": payload["id"],
                    "result": result
                });
                request
                    .respond(Response::from_string(response.to_string()))
                    .unwrap();
            }
        });
        PlanRpcFixture {
            url,
            payer,
            vote,
            methods,
            handle,
        }
    }

    fn rpc_account(owner: Pubkey, data: Vec<u8>) -> serde_json::Value {
        serde_json::json!({
            "lamports": 1,
            "data": [base64::engine::general_purpose::STANDARD.encode(&data), "base64"],
            "owner": owner.to_string(),
            "executable": false,
            "rentEpoch": 0,
            "space": data.len()
        })
    }

    #[test]
    fn real_plan_rpc_path_never_loads_key_or_submits() {
        let fixture = start_plan_rpc_stub();
        let dir = tempfile::tempdir().unwrap();
        let metrics_path = dir.path().join("vsol.prom");
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            fixture.config(&metrics_path, dir.path().join("missing-payer.json")),
        )
        .unwrap();

        run_with_sink(
            Cli {
                command: Command::Plan {
                    config: config_path,
                },
            },
            &FileMetricsSink,
        )
        .expect("plan must succeed without payer keypair");
        let methods = fixture.finish();
        assert!(!methods.iter().any(|method| {
            matches!(
                method.as_str(),
                "simulateTransaction" | "sendTransaction" | "getSignatureStatuses"
            )
        }));
    }
}
