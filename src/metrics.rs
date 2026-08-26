use anyhow::{Context, Result};
use std::{
    fmt::Write as _,
    fs::{self, OpenOptions},
    io::Write as _,
    path::Path,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunPhase {
    Plan,
    Pay,
}

impl RunPhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Pay => "pay",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunMetrics {
    pub last_run_timestamp_seconds: u64,
    pub success: bool,
    pub phase: RunPhase,
    pub discovered_invoice_count: usize,
    pub paid_invoice_count: usize,
    pub outstanding_vsol_base_units: u64,
    pub deposited_sol_lamports: u64,
    pub payer_sol_balance_lamports: u64,
    pub payer_vsol_balance_base_units: u64,
}

pub fn render_prometheus(metrics: &RunMetrics) -> Result<String> {
    let mut output = String::new();
    writeln!(
        output,
        "# HELP vsol_bond_last_run_timestamp_seconds Unix timestamp of the last completed run"
    )?;
    writeln!(output, "# TYPE vsol_bond_last_run_timestamp_seconds gauge")?;
    writeln!(
        output,
        "vsol_bond_last_run_timestamp_seconds {}",
        metrics.last_run_timestamp_seconds
    )?;
    writeln!(
        output,
        "# HELP vsol_bond_last_run_success Whether the last run succeeded"
    )?;
    writeln!(output, "# TYPE vsol_bond_last_run_success gauge")?;
    writeln!(
        output,
        "vsol_bond_last_run_success {}",
        u8::from(metrics.success)
    )?;
    writeln!(
        output,
        "vsol_bond_run_phase{{phase=\"{}\"}} 1",
        metrics.phase.as_str()
    )?;
    writeln!(
        output,
        "vsol_bond_discovered_invoice_count {}",
        metrics.discovered_invoice_count
    )?;
    writeln!(
        output,
        "vsol_bond_paid_invoice_count {}",
        metrics.paid_invoice_count
    )?;
    writeln!(
        output,
        "vsol_bond_outstanding_vsol_base_units {}",
        metrics.outstanding_vsol_base_units
    )?;
    writeln!(
        output,
        "vsol_bond_deposited_sol_lamports {}",
        metrics.deposited_sol_lamports
    )?;
    writeln!(
        output,
        "vsol_bond_payer_sol_balance_lamports {}",
        metrics.payer_sol_balance_lamports
    )?;
    writeln!(
        output,
        "vsol_bond_payer_vsol_balance_base_units {}",
        metrics.payer_vsol_balance_base_units
    )?;
    Ok(output)
}

pub fn write_metrics(path: &Path, metrics: &RunMetrics) -> Result<()> {
    write_atomically(path, &render_prometheus(metrics)?)
}

pub fn write_atomically(path: &Path, body: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("vsol-bond.prom");
    let temporary = path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temporary)
            .with_context(|| format!("failed to create {}", temporary.display()))?;
        file.write_all(body.as_bytes())
            .with_context(|| format!("failed to write {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("failed to sync {}", temporary.display()))?;
        fs::rename(&temporary, path)
            .with_context(|| format!("failed to rename metrics to {}", path.display()))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(success: bool) -> RunMetrics {
        RunMetrics {
            last_run_timestamp_seconds: 1_700_000_000,
            success,
            phase: RunPhase::Pay,
            discovered_invoice_count: 3,
            paid_invoice_count: if success { 3 } else { 0 },
            outstanding_vsol_base_units: 42,
            deposited_sol_lamports: 100,
            payer_sol_balance_lamports: 1_000,
            payer_vsol_balance_base_units: 7,
        }
    }

    #[test]
    fn renders_required_prometheus_metrics_without_signature_labels() {
        let body = render_prometheus(&sample(true)).expect("render");
        for metric in [
            "vsol_bond_last_run_timestamp_seconds 1700000000",
            "vsol_bond_last_run_success 1",
            "vsol_bond_run_phase{phase=\"pay\"} 1",
            "vsol_bond_discovered_invoice_count 3",
            "vsol_bond_paid_invoice_count 3",
            "vsol_bond_outstanding_vsol_base_units 42",
            "vsol_bond_deposited_sol_lamports 100",
            "vsol_bond_payer_sol_balance_lamports 1000",
            "vsol_bond_payer_vsol_balance_base_units 7",
        ] {
            assert!(body.contains(metric), "missing {metric}");
        }
        assert!(!body.contains("signature"));
    }

    #[test]
    fn writes_success_and_failure_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vsol.prom");
        write_atomically(&path, &render_prometheus(&sample(true)).unwrap()).unwrap();
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("success 1")
        );
        write_atomically(&path, &render_prometheus(&sample(false)).unwrap()).unwrap();
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("success 0")
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
