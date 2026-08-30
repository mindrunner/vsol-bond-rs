# vsol-bond-rs

Pays Vault epoch invoices in vSOL for a validator vote account. Built to run
as a one-shot binary from a daily systemd timer, but works standalone.

## How it works

Each run looks up the invoice PDAs for the previous 20 completed epochs,
decodes and validates every account against the deployed program layouts, and
picks the oldest unpaid invoices — at most six, and only as many as fit under
the 5 vSOL per-run cap. Anything above the cap waits for the next run.

If the payer's vSOL balance doesn't cover the total, the shortfall is bought
via a slippage-protected `deposit_sol_with_slippage` into the stake pool.
ATA creation (if needed), the deposit, and all invoice payments go into one
atomic transaction.

`plan` and `pay` are separate subcommands:

- `plan` does RPC reads and writes metrics. It never touches the keypair —
  the planner has no code path to it.
- `pay` loads `payer_keypair_path`, checks it matches `payer_pubkey`, then
  simulates, signs, and submits.

Submission gets up to three attempts with fresh blockhashes. A retry is only
allowed after proving the previous transaction expired unobserved (blockhash
invalid, block height past the last-valid height, and a history lookup coming
back empty); anything ambiguous aborts instead of risking a double payment.
Program errors surface immediately with their logs.

Hard limits (not raisable via config): 5 vSOL per run, 0.1 SOL payer reserve,
six invoices, three attempts.

## Configuration

```toml
rpc_url = "https://your-rpc.example"
vote_account = "YourValidatorVoteAccount"
payer_pubkey = "PublicKeyOfPayerKeypair"
payer_keypair_path = "/etc/vsol-payment/payer.json"
metrics_path = "/var/lib/alloy/textcollector/vsol_payment.prom"

max_total_vsol = 5000000000
min_sol_reserve_lamports = 100000000
max_invoices = 6
max_attempts = 3
priority_fee_micro_lamports = 1000
deposit_slippage_bps = 50
transaction_fee_buffer_lamports = 1000000
```

Key material only ever comes from the keypair file — no flags, no env vars.

## Usage

```console
$ vsol-bond-rs plan --config /etc/vsol-payment/config.toml
$ vsol-bond-rs pay --config /etc/vsol-payment/config.toml
```

`plan` prints invoice count, outstanding vSOL, existing vSOL, shortfall, and
the SOL deposit it would make. `pay` with nothing outstanding is a no-op.

## Metrics

Every run atomically replaces a Prometheus textfile with last-run
timestamp/success/phase, discovered and paid invoice counts, outstanding
vSOL, deposited SOL, and payer balances. The metrics path is write-tested
before any payment so a broken path fails the run early instead of after
money moved.

## Development

Rust is pinned in `rust-toolchain.toml`.

```console
$ cargo fmt --check
$ cargo clippy --all-targets -- -D warnings
$ cargo test --all-targets
```

Tests run against in-process RPC stubs and captured mainnet account fixtures
(`tests/fixtures/`); nothing contacts a cluster.

## License

Apache-2.0.
