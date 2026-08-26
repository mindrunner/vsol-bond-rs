use anyhow::{Context, Result, bail};
use solana_client::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_hash::Hash;
use solana_instruction::Instruction;
use solana_keypair::Keypair;
use solana_signature::Signature;
use solana_signer::Signer;
use solana_transaction::Transaction;
use std::{thread, time::Duration};

const MAX_COMPUTE_UNIT_LIMIT: u32 = 1_400_000;
const MIN_COMPUTE_UNIT_LIMIT: u32 = 1_000;
const MAX_RECONCILIATION_POLLS: usize = 240;
const RECONCILIATION_POLL_INTERVAL: Duration = Duration::from_millis(500);

pub trait TxSubmitter {
    fn submit(&self, instructions: &[Instruction]) -> Result<Signature>;
}

pub struct RpcTxSubmitter<'a> {
    pub client: &'a RpcClient,
    pub payer: &'a Keypair,
    pub priority_fee_micro_lamports: u64,
    pub max_attempts: usize,
}

impl TxSubmitter for RpcTxSubmitter<'_> {
    fn submit(&self, instructions: &[Instruction]) -> Result<Signature> {
        submit_with_backend(
            self.client,
            self.payer,
            instructions,
            self.priority_fee_micro_lamports,
            self.max_attempts,
        )
    }
}

#[derive(Clone, Debug)]
pub struct SimulationResult {
    pub units_consumed: Option<u64>,
    pub error: Option<String>,
    pub logs: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReconcileOutcome {
    Confirmed,
    Failed(String),
    ExpiredNotFound,
    Unresolved(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ObservedStatus {
    Processed,
    Confirmed,
    Failed(String),
}

trait ReconciliationBackend {
    fn signature_status(&self, signature: &Signature) -> Result<Option<ObservedStatus>>;
    fn blockhash_valid(&self, blockhash: &Hash) -> Result<bool>;
    fn final_signature_status(&self, signature: &Signature) -> Result<Option<ObservedStatus>>;
    fn wait_before_poll(&self);
}

pub trait SubmissionBackend {
    fn latest_blockhash(&self) -> Result<Hash>;
    fn simulate(&self, transaction: &Transaction) -> Result<SimulationResult>;
    fn send(&self, transaction: &Transaction) -> Result<Signature>;
    fn reconcile(&self, signature: &Signature, blockhash: &Hash) -> Result<ReconcileOutcome>;
}

impl SubmissionBackend for RpcClient {
    fn latest_blockhash(&self) -> Result<Hash> {
        self.get_latest_blockhash()
            .context("failed to obtain latest blockhash")
    }

    fn simulate(&self, transaction: &Transaction) -> Result<SimulationResult> {
        let value = self
            .simulate_transaction(transaction)
            .context("transaction simulation RPC failed")?
            .value;
        Ok(SimulationResult {
            units_consumed: value.units_consumed,
            error: value.err.map(|error| error.to_string()),
            logs: value.logs.unwrap_or_default(),
        })
    }

    fn send(&self, transaction: &Transaction) -> Result<Signature> {
        self.send_transaction(transaction)
            .context("failed to send transaction")
    }

    fn reconcile(&self, signature: &Signature, blockhash: &Hash) -> Result<ReconcileOutcome> {
        reconcile_with_backend(self, signature, blockhash, MAX_RECONCILIATION_POLLS)
    }
}

impl ReconciliationBackend for RpcClient {
    fn signature_status(&self, signature: &Signature) -> Result<Option<ObservedStatus>> {
        let status = self
            .get_signature_statuses(&[*signature])
            .context("failed to query transaction status")?
            .value
            .into_iter()
            .next()
            .flatten();
        Ok(status.map(|status| {
            if let Some(error) = status.err {
                ObservedStatus::Failed(error.to_string())
            } else if status.satisfies_commitment(CommitmentConfig::confirmed()) {
                ObservedStatus::Confirmed
            } else {
                ObservedStatus::Processed
            }
        }))
    }

    fn blockhash_valid(&self, blockhash: &Hash) -> Result<bool> {
        self.is_blockhash_valid(blockhash, CommitmentConfig::processed())
            .context("failed to check blockhash validity")
    }

    fn final_signature_status(&self, signature: &Signature) -> Result<Option<ObservedStatus>> {
        Ok(self
            .get_signature_status_with_commitment_and_history(
                signature,
                CommitmentConfig::confirmed(),
                true,
            )
            .context("failed final transaction status reconciliation")?
            .map(|status| match status {
                Ok(()) => ObservedStatus::Confirmed,
                Err(error) => ObservedStatus::Failed(error.to_string()),
            }))
    }

    fn wait_before_poll(&self) {
        thread::sleep(RECONCILIATION_POLL_INTERVAL);
    }
}

fn reconcile_with_backend<B: ReconciliationBackend>(
    backend: &B,
    signature: &Signature,
    blockhash: &Hash,
    max_polls: usize,
) -> Result<ReconcileOutcome> {
    if max_polls == 0 {
        bail!("reconciliation poll limit must be positive");
    }
    let mut observed_processed = false;
    let mut uncertain_read = false;
    let mut last_transient_error = None;

    for poll in 0..max_polls {
        match backend.signature_status(signature) {
            Ok(Some(ObservedStatus::Confirmed)) => return Ok(ReconcileOutcome::Confirmed),
            Ok(Some(ObservedStatus::Failed(error))) => {
                return Ok(ReconcileOutcome::Failed(error));
            }
            Ok(Some(ObservedStatus::Processed)) => observed_processed = true,
            Ok(None) => {}
            Err(error) if is_retryable_anyhow(&error) => {
                uncertain_read = true;
                last_transient_error = Some(error.to_string());
            }
            Err(error) => return Err(error),
        }

        let blockhash_valid = match backend.blockhash_valid(blockhash) {
            Ok(valid) => valid,
            Err(error) if is_retryable_anyhow(&error) => {
                uncertain_read = true;
                last_transient_error = Some(error.to_string());
                if poll + 1 < max_polls {
                    backend.wait_before_poll();
                    continue;
                }
                break;
            }
            Err(error) => return Err(error),
        };

        if !blockhash_valid {
            match backend.final_signature_status(signature) {
                Ok(Some(ObservedStatus::Confirmed)) => return Ok(ReconcileOutcome::Confirmed),
                Ok(Some(ObservedStatus::Failed(error))) => {
                    return Ok(ReconcileOutcome::Failed(error));
                }
                Ok(Some(ObservedStatus::Processed)) => observed_processed = true,
                Ok(None) if !observed_processed && !uncertain_read => {
                    return Ok(ReconcileOutcome::ExpiredNotFound);
                }
                Ok(None) => {}
                Err(error) if is_retryable_anyhow(&error) => {
                    uncertain_read = true;
                    last_transient_error = Some(error.to_string());
                }
                Err(error) => return Err(error),
            }
        }

        if poll + 1 < max_polls {
            backend.wait_before_poll();
        }
    }

    let detail = last_transient_error
        .map(|error| format!("; last transient RPC error: {error}"))
        .unwrap_or_default();
    Ok(ReconcileOutcome::Unresolved(format!(
        "transaction status did not settle within {max_polls} polls{detail}"
    )))
}

pub fn compute_unit_limit_with_margin(units_consumed: u64) -> u32 {
    let scaled = (units_consumed as u128)
        .saturating_mul(120)
        .div_ceil(100)
        .min(u128::from(MAX_COMPUTE_UNIT_LIMIT));
    u32::try_from(scaled)
        .unwrap_or(MAX_COMPUTE_UNIT_LIMIT)
        .clamp(MIN_COMPUTE_UNIT_LIMIT, MAX_COMPUTE_UNIT_LIMIT)
}

fn with_compute_budget(
    instructions: &[Instruction],
    limit: u32,
    priority_fee_micro_lamports: u64,
) -> Vec<Instruction> {
    let mut output = Vec::with_capacity(instructions.len() + 2);
    output.push(ComputeBudgetInstruction::set_compute_unit_price(
        priority_fee_micro_lamports,
    ));
    output.push(ComputeBudgetInstruction::set_compute_unit_limit(limit));
    output.extend_from_slice(instructions);
    output
}

pub fn submit_with_backend<B: SubmissionBackend>(
    backend: &B,
    payer: &Keypair,
    instructions: &[Instruction],
    priority_fee_micro_lamports: u64,
    max_attempts: usize,
) -> Result<Signature> {
    if instructions.is_empty() {
        bail!("refusing to submit an empty transaction");
    }
    if max_attempts == 0 || max_attempts > 3 {
        bail!("max_attempts must be in 1..=3");
    }

    let mut last_error = None;
    for attempt in 0..max_attempts {
        let blockhash = match backend.latest_blockhash() {
            Ok(blockhash) => blockhash,
            Err(error) => {
                let retryable = is_retryable_anyhow(&error);
                last_error = Some(error);
                if retryable && attempt + 1 < max_attempts {
                    continue;
                }
                break;
            }
        };
        let simulation_instructions = with_compute_budget(
            instructions,
            MAX_COMPUTE_UNIT_LIMIT,
            priority_fee_micro_lamports,
        );
        let simulation_transaction = Transaction::new_signed_with_payer(
            &simulation_instructions,
            Some(&payer.pubkey()),
            &[payer],
            blockhash,
        );
        let simulation = match backend.simulate(&simulation_transaction) {
            Ok(simulation) => simulation,
            Err(error) => {
                let retryable = is_retryable_anyhow(&error);
                last_error = Some(error);
                if retryable && attempt + 1 < max_attempts {
                    continue;
                }
                break;
            }
        };
        if let Some(error) = simulation.error {
            let logs = if simulation.logs.is_empty() {
                String::new()
            } else {
                format!("\n{}", simulation.logs.join("\n"))
            };
            bail!("transaction simulation failed: {error}{logs}");
        }
        let limit = simulation
            .units_consumed
            .map(compute_unit_limit_with_margin)
            .unwrap_or(MAX_COMPUTE_UNIT_LIMIT);
        let final_instructions =
            with_compute_budget(instructions, limit, priority_fee_micro_lamports);
        let transaction = Transaction::new_signed_with_payer(
            &final_instructions,
            Some(&payer.pubkey()),
            &[payer],
            blockhash,
        );
        let local_signature = *transaction
            .signatures
            .first()
            .ok_or_else(|| anyhow::anyhow!("signed transaction has no signature"))?;
        match backend.send(&transaction) {
            Ok(returned_signature) if returned_signature != local_signature => {
                bail!("RPC returned a transaction signature mismatch");
            }
            Ok(_) => {}
            Err(error) if is_retryable_anyhow(&error) => {}
            Err(error) => return Err(error),
        }
        match backend.reconcile(&local_signature, &blockhash)? {
            ReconcileOutcome::Confirmed => return Ok(local_signature),
            ReconcileOutcome::Failed(error) => {
                bail!("transaction was confirmed with an error: {error}");
            }
            ReconcileOutcome::ExpiredNotFound => {
                last_error = Some(anyhow::anyhow!(
                    "transaction was not observed before blockhash expiry"
                ));
                if attempt + 1 == max_attempts {
                    break;
                }
            }
            ReconcileOutcome::Unresolved(reason) => {
                bail!("transaction outcome unresolved; refusing to retry: {reason}");
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("transaction submission failed")))
}

pub fn is_retryable_error(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    if message.contains("custom program error")
        || message.contains("instructionerror")
        || message.contains("instruction error")
        || message.contains("invalid account")
        || message.contains("insufficient funds")
    {
        return false;
    }
    [
        "blockhash not found",
        "block height exceeded",
        "timeout",
        "timed out",
        "node is unhealthy",
        "429",
        "transport",
        "connection reset",
        "temporarily unavailable",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

fn is_retryable_anyhow(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| is_retryable_error(&cause.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_pubkey::Pubkey;
    use std::{cell::RefCell, collections::VecDeque};

    struct MockBackend {
        blockhashes: RefCell<VecDeque<Hash>>,
        send_results: RefCell<VecDeque<anyhow::Result<()>>>,
        reconcile_results: RefCell<VecDeque<anyhow::Result<ReconcileOutcome>>>,
        latest_calls: RefCell<usize>,
        simulation_calls: RefCell<usize>,
        send_calls: RefCell<usize>,
        reconcile_calls: RefCell<usize>,
        sent_signatures: RefCell<Vec<Signature>>,
        simulation: SimulationResult,
    }

    impl SubmissionBackend for MockBackend {
        fn latest_blockhash(&self) -> anyhow::Result<Hash> {
            *self.latest_calls.borrow_mut() += 1;
            self.blockhashes
                .borrow_mut()
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("no blockhash"))
        }

        fn simulate(&self, _transaction: &Transaction) -> anyhow::Result<SimulationResult> {
            *self.simulation_calls.borrow_mut() += 1;
            Ok(self.simulation.clone())
        }

        fn send(&self, transaction: &Transaction) -> anyhow::Result<Signature> {
            *self.send_calls.borrow_mut() += 1;
            let signature = transaction.signatures[0];
            self.sent_signatures.borrow_mut().push(signature);
            self.send_results
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| Err(anyhow::anyhow!("no send result")))
                .map(|()| signature)
        }

        fn reconcile(
            &self,
            signature: &Signature,
            _blockhash: &Hash,
        ) -> anyhow::Result<ReconcileOutcome> {
            *self.reconcile_calls.borrow_mut() += 1;
            assert_eq!(self.sent_signatures.borrow().last(), Some(signature));
            self.reconcile_results
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| Err(anyhow::anyhow!("no reconcile result")))
        }
    }

    fn instruction() -> Instruction {
        Instruction::new_with_bytes(Pubkey::new_unique(), &[], vec![])
    }

    #[test]
    fn compute_math_applies_twenty_percent_and_clamps() {
        assert_eq!(compute_unit_limit_with_margin(166_000), 199_200);
        assert_eq!(compute_unit_limit_with_margin(0), 1_000);
        assert_eq!(compute_unit_limit_with_margin(2_000_000), 1_400_000);
    }

    #[test]
    fn retries_transient_errors_with_fresh_blockhash_and_stops_at_success() {
        let backend = MockBackend {
            blockhashes: RefCell::new(VecDeque::from([
                Hash::new_from_array([1; 32]),
                Hash::new_from_array([2; 32]),
            ])),
            send_results: RefCell::new(VecDeque::from([
                Err(anyhow::anyhow!("block height exceeded")),
                Ok(()),
            ])),
            reconcile_results: RefCell::new(VecDeque::from([
                Ok(ReconcileOutcome::ExpiredNotFound),
                Ok(ReconcileOutcome::Confirmed),
            ])),
            latest_calls: RefCell::new(0),
            simulation_calls: RefCell::new(0),
            send_calls: RefCell::new(0),
            reconcile_calls: RefCell::new(0),
            sent_signatures: RefCell::new(Vec::new()),
            simulation: SimulationResult {
                units_consumed: Some(100_000),
                error: None,
                logs: vec![],
            },
        };
        let payer = Keypair::new();
        let signature =
            submit_with_backend(&backend, &payer, &[instruction()], 1_000, 3).expect("success");
        assert_eq!(signature, backend.sent_signatures.borrow()[1]);
        assert_eq!(*backend.latest_calls.borrow(), 2);
        assert_eq!(*backend.simulation_calls.borrow(), 2);
        assert_eq!(*backend.send_calls.borrow(), 2);
        assert_eq!(*backend.reconcile_calls.borrow(), 2);
    }

    #[test]
    fn does_not_retry_deterministic_program_or_simulation_errors() {
        assert!(!is_retryable_error("custom program error: 0x1770"));
        assert!(!is_retryable_error(
            "InstructionError(0, InvalidAccountData)"
        ));
        assert!(is_retryable_error("Blockhash not found"));
        assert!(is_retryable_error("request timed out"));

        let backend = MockBackend {
            blockhashes: RefCell::new(VecDeque::from([Hash::new_from_array([1; 32])])),
            send_results: RefCell::new(VecDeque::new()),
            reconcile_results: RefCell::new(VecDeque::new()),
            latest_calls: RefCell::new(0),
            simulation_calls: RefCell::new(0),
            send_calls: RefCell::new(0),
            reconcile_calls: RefCell::new(0),
            sent_signatures: RefCell::new(Vec::new()),
            simulation: SimulationResult {
                units_consumed: Some(50_000),
                error: Some("custom program error: 0x1770".into()),
                logs: vec!["Program log: guarded failure".into()],
            },
        };
        let error = submit_with_backend(&backend, &Keypair::new(), &[instruction()], 0, 3)
            .expect_err("simulation must fail")
            .to_string();
        assert!(error.contains("custom program error"));
        assert!(error.contains("guarded failure"));
        assert_eq!(*backend.latest_calls.borrow(), 1);
        assert_eq!(*backend.send_calls.borrow(), 0);
    }

    #[test]
    fn retry_count_is_bounded() {
        let backend = MockBackend {
            blockhashes: RefCell::new(VecDeque::from([
                Hash::new_from_array([1; 32]),
                Hash::new_from_array([2; 32]),
                Hash::new_from_array([3; 32]),
            ])),
            send_results: RefCell::new(VecDeque::from([
                Err(anyhow::anyhow!("timeout")),
                Err(anyhow::anyhow!("timeout")),
                Err(anyhow::anyhow!("timeout")),
            ])),
            reconcile_results: RefCell::new(VecDeque::from([
                Ok(ReconcileOutcome::ExpiredNotFound),
                Ok(ReconcileOutcome::ExpiredNotFound),
                Ok(ReconcileOutcome::ExpiredNotFound),
            ])),
            latest_calls: RefCell::new(0),
            simulation_calls: RefCell::new(0),
            send_calls: RefCell::new(0),
            reconcile_calls: RefCell::new(0),
            sent_signatures: RefCell::new(Vec::new()),
            simulation: SimulationResult {
                units_consumed: None,
                error: None,
                logs: vec![],
            },
        };
        assert!(submit_with_backend(&backend, &Keypair::new(), &[instruction()], 0, 3).is_err());
        assert_eq!(*backend.send_calls.borrow(), 3);
    }

    #[test]
    fn ambiguous_timeout_returns_observed_success_without_resending() {
        let backend = MockBackend {
            blockhashes: RefCell::new(VecDeque::from([Hash::new_from_array([1; 32])])),
            send_results: RefCell::new(VecDeque::from([Err(anyhow::anyhow!("request timed out"))])),
            reconcile_results: RefCell::new(VecDeque::from([Ok(ReconcileOutcome::Confirmed)])),
            latest_calls: RefCell::new(0),
            simulation_calls: RefCell::new(0),
            send_calls: RefCell::new(0),
            reconcile_calls: RefCell::new(0),
            sent_signatures: RefCell::new(Vec::new()),
            simulation: SimulationResult {
                units_consumed: Some(100_000),
                error: None,
                logs: vec![],
            },
        };
        let signature = submit_with_backend(&backend, &Keypair::new(), &[instruction()], 1_000, 3)
            .expect("observed transaction must succeed");
        assert_eq!(signature, backend.sent_signatures.borrow()[0]);
        assert_eq!(*backend.send_calls.borrow(), 1);
        assert_eq!(*backend.reconcile_calls.borrow(), 1);
    }

    struct ReconciliationMock {
        statuses: RefCell<VecDeque<anyhow::Result<Option<ObservedStatus>>>>,
        validities: RefCell<VecDeque<anyhow::Result<bool>>>,
        final_statuses: RefCell<VecDeque<anyhow::Result<Option<ObservedStatus>>>>,
        validity_calls: RefCell<usize>,
    }

    impl ReconciliationBackend for ReconciliationMock {
        fn signature_status(
            &self,
            _signature: &Signature,
        ) -> anyhow::Result<Option<ObservedStatus>> {
            self.statuses.borrow_mut().pop_front().unwrap()
        }

        fn blockhash_valid(&self, _blockhash: &Hash) -> anyhow::Result<bool> {
            *self.validity_calls.borrow_mut() += 1;
            self.validities.borrow_mut().pop_front().unwrap()
        }

        fn final_signature_status(
            &self,
            _signature: &Signature,
        ) -> anyhow::Result<Option<ObservedStatus>> {
            self.final_statuses.borrow_mut().pop_front().unwrap()
        }

        fn wait_before_poll(&self) {}
    }

    #[test]
    fn reconciliation_recovers_from_transient_status_and_validity_reads() {
        let backend = ReconciliationMock {
            statuses: RefCell::new(VecDeque::from([
                Err(anyhow::anyhow!("transport timeout").context("status RPC failed")),
                Ok(None),
                Ok(Some(ObservedStatus::Confirmed)),
            ])),
            validities: RefCell::new(VecDeque::from([
                Err(anyhow::anyhow!("node temporarily unavailable").context("blockhash RPC failed")),
                Ok(true),
            ])),
            final_statuses: RefCell::new(VecDeque::new()),
            validity_calls: RefCell::new(0),
        };
        let outcome = reconcile_with_backend(
            &backend,
            &Signature::from([8; 64]),
            &Hash::new_from_array([7; 32]),
            4,
        )
        .expect("transient reads recover");
        assert_eq!(outcome, ReconcileOutcome::Confirmed);
        assert_eq!(*backend.validity_calls.borrow(), 2);
    }

    #[test]
    fn perpetually_processed_status_is_bounded_and_checks_expiry() {
        let backend = ReconciliationMock {
            statuses: RefCell::new(VecDeque::from([
                Ok(Some(ObservedStatus::Processed)),
                Ok(Some(ObservedStatus::Processed)),
                Ok(Some(ObservedStatus::Processed)),
            ])),
            validities: RefCell::new(VecDeque::from([Ok(false), Ok(false), Ok(false)])),
            final_statuses: RefCell::new(VecDeque::from([Ok(None), Ok(None), Ok(None)])),
            validity_calls: RefCell::new(0),
        };
        let outcome = reconcile_with_backend(
            &backend,
            &Signature::from([8; 64]),
            &Hash::new_from_array([7; 32]),
            3,
        )
        .expect("bounded reconciliation");
        assert!(matches!(outcome, ReconcileOutcome::Unresolved(_)));
        assert_eq!(*backend.validity_calls.borrow(), 3);
    }

    #[test]
    fn unresolved_reconciliation_does_not_duplicate_submission() {
        let backend = MockBackend {
            blockhashes: RefCell::new(VecDeque::from([Hash::new_from_array([1; 32])])),
            send_results: RefCell::new(VecDeque::from([Ok(())])),
            reconcile_results: RefCell::new(VecDeque::from([Ok(ReconcileOutcome::Unresolved(
                "processed status did not settle".into(),
            ))])),
            latest_calls: RefCell::new(0),
            simulation_calls: RefCell::new(0),
            send_calls: RefCell::new(0),
            reconcile_calls: RefCell::new(0),
            sent_signatures: RefCell::new(Vec::new()),
            simulation: SimulationResult {
                units_consumed: Some(100_000),
                error: None,
                logs: vec![],
            },
        };
        assert!(submit_with_backend(&backend, &Keypair::new(), &[instruction()], 0, 3).is_err());
        assert_eq!(*backend.send_calls.borrow(), 1);
        assert_eq!(*backend.latest_calls.borrow(), 1);
    }
}
