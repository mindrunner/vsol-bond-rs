use anyhow::{Result, bail};
use bytemuck::{Pod, Zeroable};
use solana_pubkey::Pubkey;
use std::{collections::HashSet, str::FromStr};

pub const FIRST_INVOICE_EPOCH: u64 = 780;
pub const INVOICE_DISCRIMINATOR: [u8; 8] = [51, 194, 250, 114, 6, 104, 18, 164];
pub const INVOICER_DISCRIMINATOR: [u8; 8] = [130, 60, 88, 174, 12, 36, 237, 134];
pub const INVOICE_ACCOUNT_LEN: usize = 96;
pub const INVOICER_ACCOUNT_LEN: usize = 208;

const INVOICER_PROGRAM: &str = "EpoivtVh9dgWFxE6MYgF3YnobYWtZr2VfCuP7iT3N927";
const INVOICER_BASE: &str = "vocefgUvSTg7q4ZfeTLg2RAgeYN6V7t6rNVNb3dzrh1";

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
    pub base_key: Pubkey,
    pub bump: u8,
    pub vsol_reserves: Pubkey,
    pub owner: Pubkey,
    pub pending_owner: Pubkey,
    pub payment_withdrawer: Pubkey,
    pub invoice_creator: Pubkey,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct RawInvoice {
    invoicer: [u8; 32],
    vote_account: [u8; 32],
    epoch: [u8; 8],
    amount_vsol: [u8; 8],
    balance_outstanding: [u8; 8],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct RawInvoicer {
    base_key: [u8; 32],
    bump: u8,
    padding: [u8; 7],
    vsol_reserves: [u8; 32],
    owner: [u8; 32],
    pending_owner: [u8; 32],
    payment_withdrawer: [u8; 32],
    invoice_creator: [u8; 32],
}

pub fn invoicer_program_id() -> Pubkey {
    Pubkey::from_str(INVOICER_PROGRAM).expect("constant invoicer program")
}

pub fn invoicer_base() -> Pubkey {
    Pubkey::from_str(INVOICER_BASE).expect("constant invoicer base")
}

pub fn find_invoicer_address() -> (Pubkey, u8) {
    Pubkey::find_program_address(
        &[b"invoicer", invoicer_base().as_ref()],
        &invoicer_program_id(),
    )
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
    expected_vote: Pubkey,
    expected_epoch: u64,
) -> Result<Invoice> {
    if account.owner != invoicer_program_id() {
        bail!("invoice has foreign owner");
    }
    if account.data.len() != INVOICE_ACCOUNT_LEN {
        bail!("invoice has invalid data length");
    }
    if account.data[..8] != INVOICE_DISCRIMINATOR {
        bail!("invoice discriminator mismatch");
    }
    let raw = bytemuck::try_from_bytes::<RawInvoice>(&account.data[8..])
        .map_err(|_| anyhow::anyhow!("invalid invoice layout"))?;
    let invoice = Invoice {
        address,
        invoicer: Pubkey::new_from_array(raw.invoicer),
        vote_account: Pubkey::new_from_array(raw.vote_account),
        epoch: u64::from_le_bytes(raw.epoch),
        amount_vsol: u64::from_le_bytes(raw.amount_vsol),
        balance_outstanding: u64::from_le_bytes(raw.balance_outstanding),
    };
    validate_invoice(&invoice, expected_vote, expected_epoch)?;
    Ok(invoice)
}

fn validate_invoice(invoice: &Invoice, expected_vote: Pubkey, expected_epoch: u64) -> Result<()> {
    if invoice.invoicer != invoicer_address()
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
    let (expected_address, expected_bump) = find_invoicer_address();
    if address != expected_address || account.owner != invoicer_program_id() {
        bail!("invalid invoicer address or owner");
    }
    if account.data.len() != INVOICER_ACCOUNT_LEN {
        bail!("invoicer has invalid data length");
    }
    if account.data[..8] != INVOICER_DISCRIMINATOR {
        bail!("invoicer discriminator mismatch");
    }
    let raw = bytemuck::try_from_bytes::<RawInvoicer>(&account.data[8..])
        .map_err(|_| anyhow::anyhow!("invalid invoicer layout"))?;
    if raw.base_key != invoicer_base().to_bytes()
        || raw.bump != expected_bump
        || raw.padding != [0; 7]
        || raw.vsol_reserves == [0; 32]
    {
        bail!("invoicer derivation fields are invalid");
    }
    Ok(Invoicer {
        base_key: Pubkey::new_from_array(raw.base_key),
        bump: raw.bump,
        vsol_reserves: Pubkey::new_from_array(raw.vsol_reserves),
        owner: Pubkey::new_from_array(raw.owner),
        pending_owner: Pubkey::new_from_array(raw.pending_owner),
        payment_withdrawer: Pubkey::new_from_array(raw.payment_withdrawer),
        invoice_creator: Pubkey::new_from_array(raw.invoice_creator),
    })
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
        validate_invoice(invoice, invoice.vote_account, invoice.epoch)?;
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
        let (address, bump) = find_invoicer_address();
        let mut data = Vec::with_capacity(INVOICER_ACCOUNT_LEN);
        data.extend_from_slice(&INVOICER_DISCRIMINATOR);
        data.extend_from_slice(invoicer_base().as_ref());
        data.push(bump);
        data.extend_from_slice(&[0; 7]);
        data.extend_from_slice(reserves.as_ref());
        for fill in 1..=4 {
            data.extend_from_slice(&[fill; 32]);
        }
        assert_eq!(address, invoicer_address());
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
        let invoice = decode_invoice(address, &account, vote, 780).expect("decode");
        assert_eq!(invoice.balance_outstanding, 25);
        assert_eq!(invoice.amount_vsol, 50);

        let mut bad = account.clone();
        bad.owner = Pubkey::new_unique();
        assert!(decode_invoice(address, &bad, vote, 780).is_err());
        let mut bad = account.clone();
        bad.data[0] ^= 1;
        assert!(decode_invoice(address, &bad, vote, 780).is_err());
        let mut bad = account.clone();
        bad.data.push(0);
        assert!(decode_invoice(address, &bad, vote, 780).is_err());
        let mut bad = account;
        bad.data[88..96].copy_from_slice(&51_u64.to_le_bytes());
        assert!(decode_invoice(address, &bad, vote, 780).is_err());
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
