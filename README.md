# vsol-bond-rs

Pays Vault epoch invoices (in VLP since epoch 1064) for a validator vote account. Built to run
as a one-shot binary from a daily systemd timer, but works standalone.

## How it works

Since the epoch-1064 cutover The Vault bills in VLP, not vSOL (SaaS fees for
undirected stake and V-pool fees alike); the vSOL-era invoicer `Fn5Fb...` is
retired. Creating an invoicer is permissionless, so the tool pays only the
invoicers Vault's reference bot (SolanaVault/saas-payment-bot) trusts:
`DzWNQFv5FPVwpiSehxDJJ8KMMMp89XEjVswsAmqB5ARF` (SaaS) and
`9HE9R14dazhR1rz6Vamap7zTzkVihqiWVk5nS447PB6F` (community pool). Each must be
the PDA of its own base and settle in the VLP mint
`EUWoTx5vQQrxaDFdeK2PLUVbnmRWjw4x6sBbmcxBaHjF` (304-byte layout, mint at byte
272). Payments go to the invoicer's own reserves account, checked to be a VLP
token account owned by the invoicer.

For each trusted invoicer the run looks up the invoice PDAs for the previous 20
completed epochs, decodes and validates every account against the deployed
program layouts, and pays the invoicer that holds the oldest unpaid invoice:
its oldest unpaid invoices, at most six, and only as many as fit under the
5 VLP per-run cap. Anything above the cap, or held by the other invoicer,
waits for the next run.

If the payer's VLP balance doesn't cover the total, the shortfall is bought
with SOL through Vault's liquid unstaker (`deposit_sol` on pool
`9nyw5jxhzuSs88HxKJyDCsWBZMhxj2uNXsFcyHF5KBAb`), priced at the pool rate
(SOL vault + deactivating stake + LST inventory snapshot, over the VLP supply)
plus `deposit_slippage_bps`. Real deposits mint at a slightly better rate, and
any surplus stays in the payer's VLP balance. The run refuses when the pool is
halted or the deposit would pass its SOL vault cap. ATA creation (if needed),
the deposit, and all invoice payments go into one atomic transaction.

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

Hard limits (not raisable via config): 5 VLP per run, 0.1 SOL payer reserve,
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

`plan` prints invoice count, outstanding and existing VLP, shortfall, the
SOL deposit it would make, each invoice it would pay, and the unpaid summary
below. `pay` with nothing outstanding is a no-op.

## Metrics

Every run atomically replaces a Prometheus textfile with last-run
timestamp/success/phase, discovered and paid invoice counts, outstanding
payment-token (VLP) amounts, deposited SOL, and payer balances. Metric names
keep their `vsol` wording so existing series continue.

It also reports every unpaid invoice for the vote account, whichever invoicer
issued it (trusted or not) and also outside the 20-epoch payment window (from
epoch 780 on): `vsol_bond_unpaid_invoice_count`,
`vsol_bond_oldest_unpaid_invoice_epoch` (0 when none) and
`vsol_bond_current_epoch`. Alert on
`vsol_bond_current_epoch - vsol_bond_oldest_unpaid_invoice_epoch` when the
count is above 0: a run that succeeds but pays nothing is otherwise silent.

The metrics path is write-tested before any payment so a broken path fails the
run early instead of after money moved.

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
