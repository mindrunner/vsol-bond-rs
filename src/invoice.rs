use anyhow::{Result, bail};
use solana_pubkey::Pubkey;
use std::{collections::HashSet, str::FromStr};

pub const FIRST_INVOICE_EPOCH: u64 = 780;
pub const INVOICE_DISCRIMINATOR: [u8; 8] = [51, 194, 250, 114, 6, 104, 18, 164];
pub const INVOICER_DISCRIMINATOR: [u8; 8] = [130, 60, 88, 174, 12, 36, 237, 134];
pub const INVOICE_ACCOUNT_LEN: usize = 96;
// Minimum lengths of the known prefixes. The program has appended fields after
// them across upgrades (invoicer: 208 -> 272 -> 304 bytes), so trailing bytes
// are ignored; the discriminator and field checks catch real layout changes.
pub const INVOICER_ACCOUNT_LEN: usize = 208;

const INVOICER_PROGRAM: &str = "EpoivtVh9dgWFxE6MYgF3YnobYWtZr2VfCuP7iT3N927";
// Trusted invoicers, as in Vault's reference bot (SolanaVault/saas-payment-bot
// KNOWN_INVOICERS). create_invoicer is permissionless, so anyone can bill any
// vote account: only these are ever paid. Vault moved billing to VLP at the
// epoch-1064 cutover; the vSOL-era invoicer Fn5Fb... is retired. A new
// invoicer needs a release here; until then its invoices still show in the
// unpaid metrics, so the gap alerts instead of going silent.
pub const TRUSTED_INVOICERS: [&str; 2] = [
    "DzWNQFv5FPVwpiSehxDJJ8KMMMp89XEjVswsAmqB5ARF", // SaaS, VLP
    "9HE9R14dazhR1rz6Vamap7zTzkVihqiWVk5nS447PB6F", // community pool, VLP
];
// Absolute offset of the invoicer's settlement mint (304-byte layout).
pub const INVOICER_MINT_OFFSET: usize = 272;
pub const INVOICER_MINT_AWARE_LEN: usize = INVOICER_MINT_OFFSET + 32;
// Absolute offset of the invoice's vote account field.
pub const INVOICE_VOTE_OFFSET: usize = 8 + 32;
const LEGACY_INVOICER_BASE: &str = "vocefgUvSTg7q4ZfeTLg2RAgeYN6V7t6rNVNb3dzrh1";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainAccount {
    pub owner: Pubkey,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invoice {
    pub address: Pubkey,
    pub invoicer: Pubkey,
    pub vote_account: Pubkey,
    pub epoch: u64,
    pub amount_vsol: u64,
    pub balance_outstanding: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invoicer {
    pub address: Pubkey,
    /// Settlement mint the invoices of this invoicer are paid in.
    pub mint: Pubkey,
    pub base_key: Pubkey,
    pub bump: u8,
    pub vsol_reserves: Pubkey,
    pub owner: Pubkey,
    pub pending_owner: Pubkey,
    pub payment_withdrawer: Pubkey,
    pub invoice_creator: Pubkey,
}

pub fn invoicer_program_id() -> Pubkey {
    Pubkey::from_str(INVOICER_PROGRAM).expect("constant invoicer program")
}

pub fn trusted_invoicers() -> Vec<Pubkey> {
    TRUSTED_INVOICERS
        .iter()
        .map(|address| Pubkey::from_str(address).expect("constant trusted invoicer"))
        .collect()
}

pub fn find_invoicer_address_for(base: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[b"invoicer", base.as_ref()], &invoicer_program_id())
}

/// The original Vault invoicer (epochs up to 1063). Only for tests and
/// fixtures: payments use the invoicers discovered on chain.
pub fn invoicer_base() -> Pubkey {
    Pubkey::from_str(LEGACY_INVOICER_BASE).expect("constant invoicer base")
}

pub fn find_invoicer_address() -> (Pubkey, u8) {
    find_invoicer_address_for(&invoicer_base())
}

pub fn invoicer_address() -> Pubkey {
    find_invoicer_address().0
}

pub fn find_invoice_address(invoicer: &Pubkey, vote_account: &Pubkey, epoch: u64) -> Pubkey {
    Pubkey::find_program_address(
        &[
            b"invoice",
            invoicer.as_ref(),
            vote_account.as_ref(),
            &epoch.to_le_bytes(),
        ],
        &invoicer_program_id(),
    )
    .0
}

pub fn decode_invoice(
    address: Pubkey,
    account: &ChainAccount,
    expected_invoicer: Pubkey,
    expected_vote: Pubkey,
    expected_epoch: u64,
) -> Result<Invoice> {
    let invoice = decode_invoice_unchecked(address, account)?;
    validate_invoice(&invoice, expected_invoicer, expected_vote, expected_epoch)?;
    Ok(invoice)
}

/// Decodes an invoice found by a program scan: owner, discriminator, amounts
/// and the PDA derivation from its own fields are checked, the invoicer is not.
/// Only for reporting; payments go through `decode_invoice` against a trusted
/// invoicer.
pub fn decode_invoice_unchecked(address: Pubkey, account: &ChainAccount) -> Result<Invoice> {
    if account.owner != invoicer_program_id() {
        bail!("invoice has foreign owner");
    }
    if account.data.len() < INVOICE_ACCOUNT_LEN {
        bail!(
            "invoice data too short: {} < {INVOICE_ACCOUNT_LEN} bytes",
            account.data.len()
        );
    }
    if account.data[..8] != INVOICE_DISCRIMINATOR {
        bail!("invoice discriminator mismatch");
    }
    let (invoicer, vote_account, epoch, amount_vsol, balance_outstanding) =
        parse_invoice_layout(&account.data[8..INVOICE_ACCOUNT_LEN])?;
    let invoice = Invoice {
        address,
        invoicer,
        vote_account,
        epoch,
        amount_vsol,
        balance_outstanding,
    };
    validate_invoice(&invoice, invoicer, vote_account, epoch)?;
    Ok(invoice)
}

fn validate_invoice(
    invoice: &Invoice,
    expected_invoicer: Pubkey,
    expected_vote: Pubkey,
    expected_epoch: u64,
) -> Result<()> {
    if invoice.invoicer != expected_invoicer
        || invoice.vote_account != expected_vote
        || invoice.epoch != expected_epoch
    {
        bail!("invoice fields do not match requested invoice");
    }
    if invoice.address
        != find_invoice_address(&invoice.invoicer, &invoice.vote_account, invoice.epoch)
    {
        bail!("invoice address is not the expected PDA");
    }
    if invoice.amount_vsol == 0 || invoice.balance_outstanding > invoice.amount_vsol {
        bail!("invoice amount fields are invalid");
    }
    Ok(())
}

pub fn decode_invoicer(address: Pubkey, account: &ChainAccount) -> Result<Invoicer> {
    if account.owner != invoicer_program_id() {
        bail!("invalid invoicer owner");
    }
    if account.data.len() < INVOICER_MINT_AWARE_LEN {
        bail!(
            "invoicer data too short: {} < {INVOICER_MINT_AWARE_LEN} bytes (pre-mint layout)",
            account.data.len()
        );
    }
    if account.data[..8] != INVOICER_DISCRIMINATOR {
        bail!("invoicer discriminator mismatch");
    }
    let (
        base_key,
        bump,
        vsol_reserves,
        owner,
        pending_owner,
        payment_withdrawer,
        invoice_creator,
        padding,
    ) = parse_invoicer_layout(&account.data[8..INVOICER_ACCOUNT_LEN])?;
    if find_invoicer_address_for(&base_key) != (address, bump)
        || padding != [0; 7]
        || vsol_reserves == Pubkey::default()
    {
        bail!("invoicer derivation fields are invalid");
    }
    let mint = pubkey_at(account.data.as_slice(), INVOICER_MINT_OFFSET)?;
    if mint == Pubkey::default() {
        bail!("invoicer {address} has no settlement mint");
    }
    Ok(Invoicer {
        address,
        mint,
        base_key,
        bump,
        vsol_reserves,
        owner,
        pending_owner,
        payment_withdrawer,
        invoice_creator,
    })
}

fn parse_invoice_layout(data: &[u8]) -> Result<(Pubkey, Pubkey, u64, u64, u64)> {
    if data.len() != INVOICE_ACCOUNT_LEN - 8 {
        bail!("invalid invoice layout length");
    }
    Ok((
        pubkey_at(data, 0)?,
        pubkey_at(data, 32)?,
        u64_at(data, 64)?,
        u64_at(data, 72)?,
        u64_at(data, 80)?,
    ))
}

#[allow(clippy::type_complexity)]
fn parse_invoicer_layout(
    data: &[u8],
) -> Result<(Pubkey, u8, Pubkey, Pubkey, Pubkey, Pubkey, Pubkey, [u8; 7])> {
    if data.len() != INVOICER_ACCOUNT_LEN - 8 {
        bail!("invalid invoicer layout length");
    }
    Ok((
        pubkey_at(data, 0)?,
        data[32],
        pubkey_at(data, 40)?,
        pubkey_at(data, 72)?,
        pubkey_at(data, 104)?,
        pubkey_at(data, 136)?,
        pubkey_at(data, 168)?,
        data[33..40].try_into()?,
    ))
}

fn pubkey_at(data: &[u8], offset: usize) -> Result<Pubkey> {
    Ok(Pubkey::new_from_array(
        data.get(offset..offset + 32)
            .ok_or_else(|| anyhow::anyhow!("pubkey field out of bounds"))?
            .try_into()?,
    ))
}

fn u64_at(data: &[u8], offset: usize) -> Result<u64> {
    Ok(u64::from_le_bytes(
        data.get(offset..offset + 8)
            .ok_or_else(|| anyhow::anyhow!("u64 field out of bounds"))?
            .try_into()?,
    ))
}

pub fn candidate_epochs(current_epoch: u64) -> Vec<u64> {
    (1..=20)
        .filter_map(|offset| current_epoch.checked_sub(offset))
        .filter(|epoch| *epoch >= FIRST_INVOICE_EPOCH)
        .collect()
}

pub fn filter_invoices(
    mut invoices: Vec<Invoice>,
    current_epoch: u64,
    max_invoices: usize,
) -> Result<Vec<Invoice>> {
    if max_invoices == 0 || max_invoices > 6 {
        bail!("invoice cap must be in 1..=6");
    }
    let allowed = candidate_epochs(current_epoch)
        .into_iter()
        .collect::<HashSet<_>>();
    let mut seen = HashSet::new();
    for invoice in &invoices {
        validate_invoice(
            invoice,
            invoice.invoicer,
            invoice.vote_account,
            invoice.epoch,
        )?;
        if invoice.epoch < FIRST_INVOICE_EPOCH || !allowed.contains(&invoice.epoch) {
            continue;
        }
        if !seen.insert(invoice.address) {
            bail!("duplicate invoice");
        }
    }
    invoices.retain(|invoice| allowed.contains(&invoice.epoch) && invoice.balance_outstanding > 0);
    invoices.sort_unstable_by_key(|invoice| invoice.epoch);
    invoices.truncate(max_invoices);
    Ok(invoices)
}

/// Unpaid invoices for one vote account, whichever invoicer issued them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UnpaidInvoices {
    pub count: u64,
    /// Oldest unpaid epoch, 0 when nothing is unpaid.
    pub oldest_epoch: u64,
}

pub fn summarize_unpaid(invoices: &[Invoice], vote_account: Pubkey) -> UnpaidInvoices {
    let mut summary = UnpaidInvoices::default();
    for invoice in invoices {
        if invoice.vote_account != vote_account
            || invoice.epoch < FIRST_INVOICE_EPOCH
            || invoice.balance_outstanding == 0
        {
            continue;
        }
        summary.count += 1;
        if summary.oldest_epoch == 0 || invoice.epoch < summary.oldest_epoch {
            summary.oldest_epoch = invoice.epoch;
        }
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn key(value: &str) -> Pubkey {
        Pubkey::from_str(value).unwrap()
    }

    fn invoice_data(
        invoicer: Pubkey,
        vote: Pubkey,
        epoch: u64,
        amount: u64,
        outstanding: u64,
    ) -> Vec<u8> {
        let mut data = Vec::with_capacity(INVOICE_ACCOUNT_LEN);
        data.extend_from_slice(&INVOICE_DISCRIMINATOR);
        data.extend_from_slice(invoicer.as_ref());
        data.extend_from_slice(vote.as_ref());
        data.extend_from_slice(&epoch.to_le_bytes());
        data.extend_from_slice(&amount.to_le_bytes());
        data.extend_from_slice(&outstanding.to_le_bytes());
        data
    }

    fn invoicer_data(reserves: Pubkey) -> Vec<u8> {
        invoicer_data_for(&invoicer_base(), reserves, Pubkey::new_from_array([5; 32]))
    }

    fn invoicer_data_for(base: &Pubkey, reserves: Pubkey, mint: Pubkey) -> Vec<u8> {
        let (_, bump) = find_invoicer_address_for(base);
        let mut data = Vec::with_capacity(INVOICER_ACCOUNT_LEN);
        data.extend_from_slice(&INVOICER_DISCRIMINATOR);
        data.extend_from_slice(base.as_ref());
        data.push(bump);
        data.extend_from_slice(&[0; 7]);
        data.extend_from_slice(reserves.as_ref());
        for fill in 1..=4 {
            data.extend_from_slice(&[fill; 32]);
        }
        data.resize(INVOICER_MINT_OFFSET, 6);
        data.extend_from_slice(mint.as_ref());
        data
    }

    #[test]
    fn derives_deterministic_pda_vectors() {
        assert_eq!(
            invoicer_address().to_string(),
            "Fn5FbRbJzohohUBnwcAYHuQyAz89Q4VBHwsR5hZSGkDa"
        );
        assert_eq!(
            find_invoice_address(&invoicer_address(), &vote_program_id(), 780).to_string(),
            "5jUXMj78dkrxPzkd1HSeLTEnrfibnw5iS25YJ4SDeChx"
        );
    }

    #[test]
    fn strictly_decodes_and_validates_invoice() {
        let vote = vote_program_id();
        let address = find_invoice_address(&invoicer_address(), &vote, 780);
        let account = ChainAccount {
            owner: invoicer_program_id(),
            data: invoice_data(invoicer_address(), vote, 780, 50, 25),
        };
        let invoice =
            decode_invoice(address, &account, invoicer_address(), vote, 780).expect("decode");
        assert_eq!(invoice.balance_outstanding, 25);
        assert_eq!(invoice.amount_vsol, 50);

        let mut bad = account.clone();
        bad.owner = Pubkey::new_unique();
        assert!(decode_invoice(address, &bad, invoicer_address(), vote, 780).is_err());
        let mut bad = account.clone();
        bad.data[0] ^= 1;
        assert!(decode_invoice(address, &bad, invoicer_address(), vote, 780).is_err());
        let mut extended = account.clone();
        extended.data.extend_from_slice(&[7; 32]);
        decode_invoice(address, &extended, invoicer_address(), vote, 780)
            .expect("trailing bytes are ignored");
        let mut bad = account.clone();
        bad.data.pop();
        assert!(decode_invoice(address, &bad, invoicer_address(), vote, 780).is_err());
        let mut bad = account;
        bad.data[88..96].copy_from_slice(&51_u64.to_le_bytes());
        assert!(decode_invoice(address, &bad, invoicer_address(), vote, 780).is_err());
    }

    #[test]
    fn decode_invoice_rejects_another_invoicer() {
        let vote = vote_program_id();
        let address = find_invoice_address(&invoicer_address(), &vote, 780);
        let account = ChainAccount {
            owner: invoicer_program_id(),
            data: invoice_data(invoicer_address(), vote, 780, 50, 25),
        };
        let other =
            find_invoicer_address_for(&key("F3yy3FVpwq9MV321AzALFcDWZp9XBBHbMas3t4AtEtCW")).0;
        assert!(decode_invoice(address, &account, other, vote, 780).is_err());
        let unchecked = decode_invoice_unchecked(address, &account).expect("scan decode");
        assert_eq!(unchecked.invoicer, invoicer_address());
    }

    #[test]
    fn decodes_the_real_saas_invoicer_and_its_vlp_mint() {
        let address = key(TRUSTED_INVOICERS[0]);
        assert_eq!(
            find_invoicer_address_for(&key("F3yy3FVpwq9MV321AzALFcDWZp9XBBHbMas3t4AtEtCW")).0,
            address
        );
        let account = ChainAccount {
            owner: invoicer_program_id(),
            data: include_bytes!("../tests/fixtures/mainnet_saas_invoicer.bin").to_vec(),
        };
        let decoded = decode_invoicer(address, &account).expect("real SaaS invoicer");
        assert_eq!(
            decoded.mint.to_string(),
            "EUWoTx5vQQrxaDFdeK2PLUVbnmRWjw4x6sBbmcxBaHjF"
        );
        assert_eq!(
            decoded.vsol_reserves.to_string(),
            "B5CP3t1hdABdjXox5zzcTsjoEiv57aXjm4iBgcYMDYLw"
        );
        // Data claims a base whose PDA is not this address.
        assert!(decode_invoicer(invoicer_address(), &account).is_err());
        // Not owned by the invoicer program.
        let mut wrong_program = account.clone();
        wrong_program.owner = Pubkey::new_unique();
        assert!(decode_invoicer(address, &wrong_program).is_err());
        // Zeroed mint: pre-mint-aware settlement.
        let mut no_mint = account;
        no_mint.data[INVOICER_MINT_OFFSET..INVOICER_MINT_AWARE_LEN].fill(0);
        assert!(decode_invoicer(address, &no_mint).is_err());
        assert_eq!(trusted_invoicers().len(), 2);
    }

    #[test]
    fn summarizes_unpaid_invoices_across_invoicers() {
        let vote = vote_program_id();
        let new_invoicer =
            find_invoicer_address_for(&key("F3yy3FVpwq9MV321AzALFcDWZp9XBBHbMas3t4AtEtCW")).0;
        let invoice = |invoicer: Pubkey, epoch: u64, outstanding: u64| Invoice {
            address: find_invoice_address(&invoicer, &vote, epoch),
            invoicer,
            vote_account: vote,
            epoch,
            amount_vsol: 10,
            balance_outstanding: outstanding,
        };
        let invoices = vec![
            invoice(invoicer_address(), 768, 10),
            invoice(invoicer_address(), 1063, 0),
            invoice(new_invoicer, 1066, 10),
            invoice(new_invoicer, 1065, 4),
            Invoice {
                vote_account: Pubkey::new_unique(),
                ..invoice(new_invoicer, 1000, 10)
            },
        ];
        assert_eq!(
            summarize_unpaid(&invoices, vote),
            UnpaidInvoices {
                count: 2,
                oldest_epoch: 1065
            }
        );
        assert_eq!(summarize_unpaid(&[], vote), UnpaidInvoices::default());
    }

    #[test]
    fn strictly_decodes_and_validates_invoicer() {
        let reserves = Pubkey::new_unique();
        let account = ChainAccount {
            owner: invoicer_program_id(),
            data: invoicer_data(reserves),
        };
        let decoded =
            decode_invoicer(invoicer_address(), &account).expect("valid invoicer account");
        assert_eq!(decoded.vsol_reserves, reserves);

        let mut bad = account.clone();
        bad.data[41] = 1;
        assert!(decode_invoicer(invoicer_address(), &bad).is_err());
        let mut bad = account;
        bad.data[40] ^= 1;
        assert!(decode_invoicer(invoicer_address(), &bad).is_err());
    }

    #[test]
    fn decodes_extended_invoicer_layouts() {
        let reserves = Pubkey::new_unique();
        // 304 is the live mint-aware length; 305 covers arbitrary trailing data.
        for extra in [0, 1] {
            let mut data = invoicer_data(reserves);
            data.extend(std::iter::repeat_n(7, extra));
            let account = ChainAccount {
                owner: invoicer_program_id(),
                data,
            };
            let decoded =
                decode_invoicer(invoicer_address(), &account).expect("extended invoicer account");
            assert_eq!(decoded.vsol_reserves, reserves);
        }

        // 272 bytes is the pre-mint layout of the retired vSOL era.
        let mut truncated = invoicer_data(reserves);
        truncated.truncate(272);
        let account = ChainAccount {
            owner: invoicer_program_id(),
            data: truncated,
        };
        assert!(decode_invoicer(invoicer_address(), &account).is_err());
    }

    #[test]
    fn decodes_account_fields_from_unaligned_slices() {
        let vote = vote_program_id();
        let invoice = invoice_data(invoicer_address(), vote, 780, 50, 25);
        let mut unaligned = vec![0xff];
        unaligned.extend_from_slice(&invoice);
        let parsed = parse_invoice_layout(&unaligned[9..]).expect("unaligned invoice payload");
        assert_eq!(parsed.2, 780);

        let reserves = Pubkey::new_unique();
        let invoicer = invoicer_data(reserves);
        let mut unaligned = vec![0xff];
        unaligned.extend_from_slice(&invoicer);
        let parsed = parse_invoicer_layout(&unaligned[9..9 + INVOICER_ACCOUNT_LEN - 8])
            .expect("unaligned invoicer payload");
        assert_eq!(parsed.2, reserves);
    }

    #[test]
    fn filters_completed_epochs_sorts_oldest_first_and_caps() {
        let vote = vote_program_id();
        let mut invoices = (781..=790)
            .rev()
            .map(|epoch| Invoice {
                address: find_invoice_address(&invoicer_address(), &vote, epoch),
                invoicer: invoicer_address(),
                vote_account: vote,
                epoch,
                amount_vsol: 1,
                balance_outstanding: 1,
            })
            .collect::<Vec<_>>();
        invoices.push(Invoice {
            address: find_invoice_address(&invoicer_address(), &vote, 780),
            epoch: 780,
            ..invoices[0].clone()
        });
        invoices.push(Invoice {
            address: find_invoice_address(&invoicer_address(), &vote, 800),
            epoch: 800,
            ..invoices[0].clone()
        });

        let filtered = filter_invoices(invoices, 801, 6).expect("filter");
        assert_eq!(
            filtered.iter().map(|x| x.epoch).collect::<Vec<_>>(),
            vec![781, 782, 783, 784, 785, 786]
        );
        assert_eq!(candidate_epochs(801).len(), 20);
        assert_eq!(candidate_epochs(801)[0], 800);
    }

    #[test]
    fn ignores_paid_invoices_and_rejects_duplicate_invoice_data() {
        let vote = vote_program_id();
        let mut invoice = Invoice {
            address: find_invoice_address(&invoicer_address(), &vote, 780),
            invoicer: invoicer_address(),
            vote_account: vote,
            epoch: 780,
            amount_vsol: 1,
            balance_outstanding: 1,
        };
        invoice.balance_outstanding = 0;
        assert!(filter_invoices(vec![invoice], 781, 6).unwrap().is_empty());

        let duplicate = Invoice {
            address: find_invoice_address(&invoicer_address(), &vote, 780),
            invoicer: invoicer_address(),
            vote_account: vote,
            epoch: 780,
            amount_vsol: 1,
            balance_outstanding: 1,
        };
        assert!(filter_invoices(vec![duplicate.clone(), duplicate], 781, 6).is_err());
    }

    fn vote_program_id() -> Pubkey {
        key("Vote111111111111111111111111111111111111111")
    }
}
