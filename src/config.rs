use anyhow::{Context, Result, bail};
use serde::Deserialize;
use solana_pubkey::Pubkey;
use std::{
    fs,
    path::{Path, PathBuf},
    str::FromStr,
};

pub const HARD_MAX_TOTAL_VSOL: u64 = 5_000_000_000;
pub const MIN_REQUIRED_SOL_RESERVE_LAMPORTS: u64 = 100_000_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub rpc_url: String,
    pub vote_account: Pubkey,
    pub payer_pubkey: Pubkey,
    pub payer_keypair_path: PathBuf,
    pub metrics_path: PathBuf,
    pub max_total_vsol: u64,
    pub min_sol_reserve_lamports: u64,
    pub max_invoices: usize,
    pub max_attempts: usize,
    pub priority_fee_micro_lamports: u64,
    pub deposit_slippage_bps: u16,
    pub transaction_fee_buffer_lamports: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    rpc_url: String,
    vote_account: String,
    payer_pubkey: String,
    payer_keypair_path: PathBuf,
    metrics_path: PathBuf,
    #[serde(default = "default_max_total_vsol")]
    max_total_vsol: u64,
    #[serde(default = "default_min_sol_reserve")]
    min_sol_reserve_lamports: u64,
    #[serde(default = "default_max_invoices")]
    max_invoices: usize,
    #[serde(default = "default_max_attempts")]
    max_attempts: usize,
    #[serde(default = "default_priority_fee")]
    priority_fee_micro_lamports: u64,
    #[serde(default = "default_slippage_bps")]
    deposit_slippage_bps: u16,
    #[serde(default = "default_fee_buffer")]
    transaction_fee_buffer_lamports: u64,
}

const fn default_max_total_vsol() -> u64 {
    HARD_MAX_TOTAL_VSOL
}

const fn default_min_sol_reserve() -> u64 {
    MIN_REQUIRED_SOL_RESERVE_LAMPORTS
}

const fn default_max_invoices() -> usize {
    6
}

const fn default_max_attempts() -> usize {
    3
}

const fn default_priority_fee() -> u64 {
    1_000
}

const fn default_slippage_bps() -> u16 {
    50
}

const fn default_fee_buffer() -> u64 {
    1_000_000
}

impl Config {
    pub fn metrics_path_hint(path: impl AsRef<Path>) -> Option<PathBuf> {
        let raw = fs::read_to_string(path).ok()?;
        toml::from_str::<toml::Value>(&raw)
            .ok()?
            .get("metrics_path")?
            .as_str()
            .map(PathBuf::from)
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read config {}", path.display()))?;
        let raw: RawConfig =
            toml::from_str(&raw).with_context(|| format!("invalid config {}", path.display()))?;
        let config = Self {
            rpc_url: raw.rpc_url,
            vote_account: Pubkey::from_str(&raw.vote_account).context("invalid vote_account")?,
            payer_pubkey: Pubkey::from_str(&raw.payer_pubkey).context("invalid payer_pubkey")?,
            payer_keypair_path: raw.payer_keypair_path,
            metrics_path: raw.metrics_path,
            max_total_vsol: raw.max_total_vsol,
            min_sol_reserve_lamports: raw.min_sol_reserve_lamports,
            max_invoices: raw.max_invoices,
            max_attempts: raw.max_attempts,
            priority_fee_micro_lamports: raw.priority_fee_micro_lamports,
            deposit_slippage_bps: raw.deposit_slippage_bps,
            transaction_fee_buffer_lamports: raw.transaction_fee_buffer_lamports,
        };
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if !(self.rpc_url.starts_with("https://") || self.rpc_url.starts_with("http://")) {
            bail!("rpc_url must use http or https");
        }
        if self.payer_keypair_path.as_os_str().is_empty() {
            bail!("payer_keypair_path must not be empty");
        }
        if self.metrics_path.as_os_str().is_empty() {
            bail!("metrics_path must not be empty");
        }
        if self.max_total_vsol == 0 || self.max_total_vsol > HARD_MAX_TOTAL_VSOL {
            bail!("max_total_vsol must be in 1..={HARD_MAX_TOTAL_VSOL}");
        }
        if self.min_sol_reserve_lamports < MIN_REQUIRED_SOL_RESERVE_LAMPORTS {
            bail!("min_sol_reserve_lamports must be at least {MIN_REQUIRED_SOL_RESERVE_LAMPORTS}");
        }
        if self.max_invoices == 0 || self.max_invoices > 6 {
            bail!("max_invoices must be in 1..=6");
        }
        if self.max_attempts == 0 || self.max_attempts > 3 {
            bail!("max_attempts must be in 1..=3");
        }
        if self.deposit_slippage_bps > 1_000 {
            bail!("deposit_slippage_bps must be at most 1000");
        }
        let priority_fee_lamports = (self.priority_fee_micro_lamports as u128)
            .checked_mul(1_400_000)
            .ok_or_else(|| anyhow::anyhow!("priority fee overflow"))?
            .div_ceil(1_000_000);
        let per_attempt_fee_buffer = u64::try_from(priority_fee_lamports)
            .ok()
            .and_then(|fee| fee.checked_add(10_000))
            .ok_or_else(|| anyhow::anyhow!("priority fee overflow"))?;
        let minimum_fee_buffer = per_attempt_fee_buffer
            .checked_mul(u64::try_from(self.max_attempts)?)
            .ok_or_else(|| anyhow::anyhow!("retry fee reserve overflow"))?;
        if self.transaction_fee_buffer_lamports < minimum_fee_buffer {
            bail!(
                "transaction_fee_buffer_lamports must be at least {minimum_fee_buffer} for the configured priority fee"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const VALID: &str = r#"
rpc_url = "https://rpc.example.test"
vote_account = "11111111111111111111111111111111"
payer_pubkey = "Vote111111111111111111111111111111111111111"
payer_keypair_path = "/secure/payer.json"
metrics_path = "/var/lib/node-exporter/vsol-bond.prom"
"#;

    fn load(raw: &str) -> anyhow::Result<Config> {
        let mut file = tempfile::NamedTempFile::new()?;
        file.write_all(raw.as_bytes())?;
        Config::load(file.path())
    }

    #[test]
    fn applies_guarded_defaults() {
        let config = load(VALID).expect("valid config");
        assert_eq!(config.max_total_vsol, HARD_MAX_TOTAL_VSOL);
        assert_eq!(config.min_sol_reserve_lamports, 100_000_000);
        assert_eq!(config.max_invoices, 6);
        assert_eq!(config.max_attempts, 3);
        assert!(config.priority_fee_micro_lamports > 0);
    }

    #[test]
    fn rejects_unsafe_or_invalid_values() {
        for replacement in [
            ("https://rpc.example.test", "file:///tmp/rpc"),
            ("max_total_vsol = 5000000000", "max_total_vsol = 5000000001"),
            (
                "min_sol_reserve_lamports = 100000000",
                "min_sol_reserve_lamports = 99999999",
            ),
            ("max_invoices = 6", "max_invoices = 7"),
        ] {
            let with_explicit_defaults = format!(
                "{VALID}\nmax_total_vsol = 5000000000\nmin_sol_reserve_lamports = 100000000\nmax_invoices = 6\n"
            );
            let invalid = with_explicit_defaults.replace(replacement.0, replacement.1);
            assert!(load(&invalid).is_err(), "accepted {invalid}");
        }
    }

    #[test]
    fn rejects_unknown_fields_and_bad_pubkeys() {
        assert!(load(&format!("{VALID}\nprivate_key = [1, 2, 3]\n")).is_err());
        assert!(
            load(&VALID.replace("Vote111111111111111111111111111111111111111", "not-a-key"))
                .is_err()
        );
    }

    #[test]
    fn fee_buffer_covers_configured_priority_fee() {
        assert!(
            load(&format!(
                "{VALID}\npriority_fee_micro_lamports = 1000\ntransaction_fee_buffer_lamports = 1\n"
            ))
            .is_err()
        );
    }

    #[test]
    fn fee_buffer_covers_all_configured_attempts() {
        let too_small = format!(
            "{VALID}\nmax_attempts = 3\npriority_fee_micro_lamports = 1000\ntransaction_fee_buffer_lamports = 34199\n"
        );
        assert!(load(&too_small).is_err());
        let exact = too_small.replace("34199", "34200");
        assert!(load(&exact).is_ok());
    }
}
