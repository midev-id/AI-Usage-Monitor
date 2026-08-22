use std::io::Read;
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::PollError;
use crate::diagnose;
use crate::models::{CreditsSection, UsageData};

const CREATE_NO_WINDOW: u32 = 0x08000000;
const STATS_TIMEOUT: Duration = Duration::from_secs(30);
const STATS_WINDOW_DAYS: u32 = 30;
const MONTHLY_LIMIT_ENV: &str = "OPENCODE_ZEN_MONTHLY_LIMIT_USD";

/// OpenCode Zen has no published API to read an account's balance or spending
/// cap, so this is a rough estimate rather than an authoritative reading: it
/// shells out to `opencode stats`, sums the cost of Zen-billed models over the
/// trailing month, and gauges that against an assumed monthly cap that has no
/// official source. `OPENCODE_ZEN_MONTHLY_LIMIT_USD` lets a user correct it to
/// their actual plan; there is no way to fetch that cap automatically.
const DEFAULT_MONTHLY_LIMIT_USD: f64 = 1000.0;

pub(super) fn poll_opencode_zen() -> Result<UsageData, PollError> {
    if resolve_opencode_command().is_none() {
        diagnose::log("OpenCode Zen usage poll failed: opencode CLI not found in PATH");
        return Err(PollError::NoCredentials);
    }

    let output = run_opencode_stats(STATS_WINDOW_DAYS)?;
    if !output.contains("MODEL USAGE") {
        diagnose::log(
            "OpenCode Zen usage poll failed: `opencode stats` output did not contain a model usage table",
        );
        return Err(PollError::RequestFailed);
    }

    let model_costs = parse_model_costs(&output);
    let usage = usage_from_model_costs(&model_costs, monthly_limit_usd());
    diagnose::log(format!(
        "OpenCode Zen usage: {}",
        usage
            .detail
            .as_deref()
            .unwrap_or("no Zen-billed models in the last 30 days")
    ));
    Ok(usage)
}

pub(super) fn credential_watch_snapshot(_all_sources: bool) -> Vec<String> {
    vec![match resolve_opencode_command() {
        Some(command) => format!("opencode-cli|present|{command}"),
        None => "opencode-cli|missing".to_string(),
    }]
}

fn monthly_limit_usd() -> f64 {
    std::env::var(MONTHLY_LIMIT_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value > 0.0)
        .unwrap_or(DEFAULT_MONTHLY_LIMIT_USD)
}

fn usage_from_model_costs(model_costs: &[(String, f64)], monthly_limit: f64) -> UsageData {
    let mut zen_costs: Vec<(&str, f64)> = model_costs
        .iter()
        .filter(|(name, _)| is_opencode_zen_model(name))
        .map(|(name, cost)| (name.as_str(), cost.max(0.0)))
        .collect();
    zen_costs.sort_by(|a, b| b.1.total_cmp(&a.1));
    let zen_total: f64 = zen_costs.iter().map(|(_, cost)| cost).sum();

    let percentage = if monthly_limit > 0.0 {
        (zen_total / monthly_limit * 100.0).clamp(0.0, 100.0)
    } else {
        0.0
    };

    let mut usage = UsageData {
        credits: Some(CreditsSection {
            percentage,
            remaining: (monthly_limit - zen_total).max(0.0),
            total: monthly_limit,
        }),
        ..Default::default()
    };
    if !zen_costs.is_empty() {
        usage.detail = Some(
            zen_costs
                .iter()
                .take(3)
                .map(|(name, cost)| format!("{name} ${cost:.2}"))
                .collect::<Vec<_>>()
                .join(", "),
        );
    }
    usage
}

fn is_opencode_zen_model(model_name: &str) -> bool {
    let normalized = model_name.trim().to_ascii_lowercase();
    normalized.starts_with("opencode/") || normalized.starts_with("opencode-go/")
}

/// Parses `opencode stats --models` output into (model name, cost) pairs. The
/// CLI prints a box-drawing table; only the "MODEL USAGE" section matters
/// here, where each model name row is followed by indented metric rows.
fn parse_model_costs(output: &str) -> Vec<(String, f64)> {
    const METRIC_PREFIXES: [&str; 13] = [
        "Sessions",
        "Messages",
        "Days",
        "Total Cost",
        "Avg Cost/Day",
        "Avg Tokens/Session",
        "Median Tokens/Session",
        "Input",
        "Output",
        "Input Tokens",
        "Output Tokens",
        "Cache Read",
        "Cache Write",
    ];
    const SECTION_HEADERS: [&str; 4] = ["OVERVIEW", "COST & TOKENS", "MODEL USAGE", "TOOL USAGE"];

    let mut costs: Vec<(String, f64)> = Vec::new();
    let mut current_model: Option<String> = None;
    let mut in_model_usage_section = false;

    for raw_line in output.lines() {
        let line = strip_ansi(raw_line);
        let trimmed = line.trim();

        let Some(cell) = table_cell(trimmed) else {
            if trimmed.starts_with('├') || trimmed.starts_with('└') {
                current_model = None;
            }
            continue;
        };
        if cell.is_empty() {
            continue;
        }

        if SECTION_HEADERS.contains(&cell) {
            in_model_usage_section = cell == "MODEL USAGE";
            current_model = None;
            continue;
        }
        if !in_model_usage_section {
            continue;
        }

        if cell.starts_with("Cost") {
            if let (Some(model), Some(cost)) = (&current_model, dollar_value(cell)) {
                match costs.iter_mut().find(|(name, _)| name == model) {
                    Some(entry) => entry.1 = cost,
                    None => costs.push((model.clone(), cost)),
                }
            }
            continue;
        }
        if METRIC_PREFIXES.iter().any(|prefix| cell.starts_with(prefix)) {
            continue;
        }

        current_model = Some(cell.to_string());
    }

    costs
}

fn table_cell(line: &str) -> Option<&str> {
    let without_leading = line.strip_prefix('│')?;
    let end = without_leading.rfind('│').unwrap_or(without_leading.len());
    Some(without_leading[..end].trim())
}

fn dollar_value(text: &str) -> Option<f64> {
    let dollar_index = text.rfind('$')?;
    let value: String = text[dollar_index + 1..]
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    if value.is_empty() {
        None
    } else {
        value.parse().ok()
    }
}

fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1B}' && chars.peek() == Some(&'[') {
            chars.next();
            for next in chars.by_ref() {
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// Resolves the `opencode` CLI the same way `codex.rs` resolves the Codex
/// CLI: try spawning known executable/script names directly (works when
/// PATHEXT already covers them), then fall back to `where.exe` for names it
/// can locate but that a raw spawn does not recognise as runnable.
fn resolve_opencode_command() -> Option<String> {
    const CANDIDATES: [&str; 4] = ["opencode.cmd", "opencode.exe", "opencode.ps1", "opencode"];

    for name in CANDIDATES {
        if Command::new(name)
            .arg("--version")
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok()
        {
            return Some(name.to_string());
        }
    }

    for name in CANDIDATES {
        let Ok(output) = Command::new("where.exe")
            .arg(name)
            .creation_flags(CREATE_NO_WINDOW)
            .output()
        else {
            continue;
        };
        if !output.status.success() {
            continue;
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        if let Some(path) = stdout
            .lines()
            .next()
            .map(str::trim)
            .filter(|path| !path.is_empty())
        {
            return Some(path.to_string());
        }
    }

    None
}

fn run_opencode_stats(days: u32) -> Result<String, PollError> {
    let opencode_command = resolve_opencode_command().ok_or(PollError::NoCredentials)?;
    let lower = opencode_command.to_lowercase();
    let is_cmd = lower.ends_with(".cmd");
    let is_ps1 = lower.ends_with(".ps1");

    let days = days.to_string();
    let mut command = if is_cmd {
        let mut command = Command::new("cmd.exe");
        command
            .arg("/c")
            .arg(&opencode_command)
            .args(["stats", "--days", &days, "--models"]);
        command
    } else if is_ps1 {
        let mut command = Command::new("powershell.exe");
        command
            .arg("-NoProfile")
            .arg("-ExecutionPolicy")
            .arg("Bypass")
            .arg("-File")
            .arg(&opencode_command)
            .args(["stats", "--days", &days, "--models"]);
        command
    } else {
        let mut command = Command::new(&opencode_command);
        command.args(["stats", "--days", &days, "--models"]);
        command
    };

    let mut child = command
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| {
            diagnose::log_error("unable to spawn the opencode CLI", error);
            PollError::RequestFailed
        })?;

    // Drain stdout concurrently with waiting: `opencode stats` output can
    // exceed the OS pipe buffer, and waiting for exit before reading would
    // deadlock against a child blocked writing into a full pipe.
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let reader = std::thread::spawn(move || {
        let mut buffer = String::new();
        let _ = stdout.read_to_string(&mut buffer);
        buffer
    });

    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() > STATS_TIMEOUT => {
                diagnose::log("opencode stats timed out; killing the process");
                let _ = child.kill();
                break;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(200)),
            Err(_) => break,
        }
    }

    reader.join().map_err(|_| PollError::RequestFailed)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_OUTPUT: &str = "\
┌───────────────────────────────┐
│ OVERVIEW                       │
├───────────────────────────────┤
│ Sessions            12         │
└───────────────────────────────┘
┌─────────────────────────────────────────┐
│ MODEL USAGE                              │
├─────────────────────────────────────────┤
│ opencode/claude-sonnet-5                 │
│ Cost                $12.34               │
│ Messages            42                   │
├─────────────────────────────────────────┤
│ opencode-go/gpt-5-mini                   │
│ Cost                $3.20                │
├─────────────────────────────────────────┤
│ anthropic/claude-sonnet-5                │
│ Cost                $50.00               │
└─────────────────────────────────────────┘
┌─────────────────────────────────────────┐
│ TOOL USAGE                               │
├─────────────────────────────────────────┤
│ opencode/claude-sonnet-5                 │
│ Cost                $999.00              │
└─────────────────────────────────────────┘";

    #[test]
    fn parses_model_costs_only_within_the_model_usage_section() {
        let costs = parse_model_costs(SAMPLE_OUTPUT);
        assert_eq!(
            costs,
            vec![
                ("opencode/claude-sonnet-5".to_string(), 12.34),
                ("opencode-go/gpt-5-mini".to_string(), 3.20),
                ("anthropic/claude-sonnet-5".to_string(), 50.00),
            ]
        );
    }

    #[test]
    fn zen_models_are_identified_by_their_provider_prefix() {
        assert!(is_opencode_zen_model("opencode/claude-sonnet-5"));
        assert!(is_opencode_zen_model("opencode-go/gpt-5-mini"));
        assert!(is_opencode_zen_model(" OpenCode/Claude "));
        assert!(!is_opencode_zen_model("anthropic/claude-sonnet-5"));
        assert!(!is_opencode_zen_model("openai/gpt-5"));
    }

    #[test]
    fn usage_gauges_only_zen_billed_cost_against_the_monthly_cap() {
        let model_costs = parse_model_costs(SAMPLE_OUTPUT);
        let usage = usage_from_model_costs(&model_costs, 100.0);

        let credits = usage.credits.expect("gauge");
        // 12.34 + 3.20 = 15.54 of a 100.0 cap; the non-Zen $50.00 row is excluded.
        assert!((credits.percentage - 15.54).abs() < 1e-9);
        assert!((credits.remaining - 84.46).abs() < 1e-9);
        assert_eq!(credits.total, 100.0);
        assert_eq!(
            usage.detail.as_deref(),
            Some("opencode/claude-sonnet-5 $12.34, opencode-go/gpt-5-mini $3.20")
        );
    }

    #[test]
    fn no_zen_usage_gauges_to_zero_without_a_detail_line() {
        let usage = usage_from_model_costs(&[], 100.0);
        let credits = usage.credits.expect("gauge");
        assert_eq!(credits.percentage, 0.0);
        assert_eq!(credits.remaining, 100.0);
        assert!(usage.detail.is_none());
    }

    #[test]
    fn spend_past_the_cap_clamps_to_full() {
        let model_costs = vec![("opencode/x".to_string(), 250.0)];
        let usage = usage_from_model_costs(&model_costs, 100.0);
        let credits = usage.credits.expect("gauge");
        assert_eq!(credits.percentage, 100.0);
        assert_eq!(credits.remaining, 0.0);
    }

    #[test]
    fn ansi_escapes_are_stripped() {
        assert_eq!(strip_ansi("\u{1B}[1;32mCost\u{1B}[0m $1.00"), "Cost $1.00");
    }

    #[test]
    fn dollar_value_reads_the_last_amount_on_the_line() {
        assert_eq!(dollar_value("Cost   $12.34"), Some(12.34));
        assert_eq!(dollar_value("no amount here"), None);
    }
}
