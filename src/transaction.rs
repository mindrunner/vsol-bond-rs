use anyhow::{Context, Result, bail};
use solana_client::rpc_client::RpcClient;
use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_hash::Hash;
use solana_instruction::Instruction;
use solana_keypair::Keypair;
use solana_signature::Signature;
use solana_signer::Signer;
use solana_transaction::Transaction;

const MAX_COMPUTE_UNIT_LIMIT: u32 = 1_400_000;
const MIN_COMPUTE_UNIT_LIMIT: u32 = 1_000;

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

pub trait SubmissionBackend {
    fn latest_blockhash(&self) -> Result<Hash>;
    fn simulate(&self, transaction: &Transaction) -> Result<SimulationResult>;
    fn send_and_confirm(&self, transaction: &Transaction) -> Result<Signature>;
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

    fn send_and_confirm(&self, transaction: &Transaction) -> Result<Signature> {
        self.send_and_confirm_transaction(transaction)
            .context("failed to send and confirm transaction")
    }
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
                let retryable = is_retryable_error(&error.to_string());
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
                let retryable = is_retryable_error(&error.to_string());
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
        match backend.send_and_confirm(&transaction) {
            Ok(signature) => return Ok(signature),
            Err(error) => {
                let retryable = is_retryable_error(&error.to_string());
                last_error = Some(error);
                if !retryable || attempt + 1 == max_attempts {
                    break;
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use solana_pubkey::Pubkey;
    use std::{cell::RefCell, collections::VecDeque};

    struct MockBackend {
        blockhashes: RefCell<VecDeque<Hash>>,
        send_results: RefCell<VecDeque<anyhow::Result<Signature>>>,
        latest_calls: RefCell<usize>,
        simulation_calls: RefCell<usize>,
        send_calls: RefCell<usize>,
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

        fn send_and_confirm(&self, _transaction: &Transaction) -> anyhow::Result<Signature> {
            *self.send_calls.borrow_mut() += 1;
            self.send_results
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| Err(anyhow::anyhow!("no send result")))
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
                Ok(Signature::from([3; 64])),
            ])),
            latest_calls: RefCell::new(0),
            simulation_calls: RefCell::new(0),
            send_calls: RefCell::new(0),
            simulation: SimulationResult {
                units_consumed: Some(100_000),
                error: None,
                logs: vec![],
            },
        };
        let payer = Keypair::new();
        let signature =
            submit_with_backend(&backend, &payer, &[instruction()], 1_000, 3).expect("success");
        assert_eq!(signature, Signature::from([3; 64]));
        assert_eq!(*backend.latest_calls.borrow(), 2);
        assert_eq!(*backend.simulation_calls.borrow(), 2);
        assert_eq!(*backend.send_calls.borrow(), 2);
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
            latest_calls: RefCell::new(0),
            simulation_calls: RefCell::new(0),
            send_calls: RefCell::new(0),
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
            latest_calls: RefCell::new(0),
            simulation_calls: RefCell::new(0),
            send_calls: RefCell::new(0),
            simulation: SimulationResult {
                units_consumed: None,
                error: None,
                logs: vec![],
            },
        };
        assert!(submit_with_backend(&backend, &Keypair::new(), &[instruction()], 0, 3).is_err());
        assert_eq!(*backend.send_calls.borrow(), 3);
    }
}
