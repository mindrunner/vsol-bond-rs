# vsol-bond-rs

Guarded Rust automation for paying Vault epoch invoices in vSOL. The bot reads
the previous 20 completed epochs, validates every account and PDA, and plans at
most six outstanding invoices before any signing is possible.

## Safety model

- `plan` is the default operator workflow. It performs RPC reads and writes the
  local metrics file, but cannot sign or submit because the planner has no
  transaction-submitter dependency and never reads the payer keypair.
- `pay` is the only command that loads `payer_keypair_path`. It verifies that
  the keypair matches the configured public key before signing.
- One atomic transaction contains an optional idempotent vSOL ATA creation,
  an optional slippage-protected stake-pool SOL deposit, and all invoice
  payments.
- Payment amounts always use each invoice's `balance_outstanding`, never its
  original amount.
- The hard payment cap is 5,000,000,000 vSOL base units (5 vSOL). It cannot be
  raised in configuration.
- The payer retains at least 100,000,000 lamports plus a transaction-fee
  buffer sized for every configured attempt.
- Existing vSOL in the payer ATA is used first; only the shortfall is deposited.
- Transactions are simulated before signing for submission. The compute limit
  is simulated usage plus 20%, clamped to 1,000..1,400,000 units.
- Submission retries are limited to three fresh blockhashes. Before using a
  new blockhash, the bot reconciles the locally derived signature through the
  prior blockhash's expiry and performs a final history lookup, so an ambiguous
  timeout cannot duplicate a landed payment. Program and simulation errors,
  including logs, are returned immediately.

The pinned stake-pool revision supports `deposit_sol_with_slippage`. The bot
quotes enough SOL for the required net vSOL after the on-chain SOL-deposit fee,
adds the configured SOL input buffer, and sets the minimum pool-token output to
the exact vSOL shortfall. The stake pool must be current for the cluster epoch
and must not require a separate SOL deposit authority.
The pool program owner, withdraw-authority PDA and bump, reserve stake state,
pool mint authority/supply/freeze state, manager fee token account, token
program, and configured vSOL mint are all validated before planning a deposit.

## Configuration

```toml
rpc_url = "https://your-existing-rpc.example"
vote_account = "YourValidatorVoteAccount"
payer_pubkey = "PublicKeyOfPayerKeypair"
payer_keypair_path = "/absolute/path/to/payer.json"
metrics_path = "/var/lib/node_exporter/textfile_collector/vsol-bond.prom"

# Guarded defaults shown explicitly.
max_total_vsol = 5000000000
min_sol_reserve_lamports = 100000000
max_invoices = 6
max_attempts = 3
priority_fee_micro_lamports = 1000
deposit_slippage_bps = 50
transaction_fee_buffer_lamports = 1000000
```

Private key bytes are not accepted through CLI flags, environment variables,
or TOML. The config only references a keypair file. Protect that file with
operator-appropriate filesystem permissions.

## Usage

```console
$ vsol-bond-rs plan --config /etc/vsol-bond.toml
$ vsol-bond-rs pay --config /etc/vsol-bond.toml
```

`plan` reports invoice count, outstanding vSOL, existing vSOL, shortfall, and
the required SOL deposit. `pay` does nothing when no outstanding invoice exists.
The application never prints keypair contents or serialized signed transactions.

## Metrics

The metrics destination is write-preflighted before payment, and the
Prometheus textfile is replaced atomically on success and failure. Operational
errors remain the primary error even if failure metrics cannot be written. If
an on-chain payment succeeds but its metrics update fails, the CLI reports that
the payment was confirmed and identifies the metrics failure. The file
contains last-run timestamp/success/phase, discovered and paid invoice counts,
outstanding vSOL, deposited SOL, and observed payer SOL/vSOL balances.
Transaction signatures are never metric labels.

## Development

Rust 1.93.0 is pinned in `rust-toolchain.toml`.

```console
$ cargo fmt --check
$ cargo clippy --all-targets -- -D warnings
$ cargo test --all-targets
$ cargo build --release
```

No Solana CLI subprocess is used. Tests use pure planners and recording RPC
seams; they do not contact a cluster.

## License

Apache-2.0.