//! vela-ctl — command-line client for the Vela REST API.
//!
//! A short-lived CLI, separate from the `vela` server binary. Uses a
//! blocking reqwest client since there is no reason to carry an async
//! runtime for a handful of sequential HTTP calls that exit immediately.

use std::process::ExitCode;
use std::time::Duration;

use chrono::{DateTime, Local, Utc};
use clap::{Parser, Subcommand};
use colored::{ColoredString, Colorize};
use serde::de::DeserializeOwned;
use serde::Deserialize;

const DEFAULT_URL: &str = "http://127.0.0.1:7700";

#[derive(Parser)]
#[command(
    name = "vela-ctl",
    version,
    about = "Command-line client for the Vela API"
)]
struct Cli {
    /// Base URL of the Vela API. Overrides VELA_URL. Default: http://127.0.0.1:7700
    #[arg(long, global = true)]
    url: Option<String>,

    /// API key for authentication. Overrides VELA_API_KEY.
    #[arg(long, global = true)]
    key: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show aggregated status of all services
    Status,
    /// List all services, or inspect one service and its history
    Services {
        /// Service ID. Omit to list all services (same as `vela-ctl status`).
        id: Option<String>,
        /// Sub-resource to view for the given service: checks | alerts | restarts
        resource: Option<String>,
    },
}

// ─── API response shapes (mirrors the JSON src/api.rs produces) ────────────
// Local, minimal structs — only the fields this CLI actually displays.
// The server's response types (models.rs) are Serialize-only by design;
// the CLI deserializes its own view of the same JSON contract.

#[derive(Deserialize)]
struct Envelope<T> {
    ok: bool,
    data: Option<T>,
    error: Option<String>,
}

#[derive(Deserialize)]
struct ServiceSummary {
    service_id: String,
    service_name: String,
    status: String,
    consecutive_failures: u32,
    restart_count: u32,
    last_checked_at: Option<DateTime<Utc>>,
    last_ok_at: Option<DateTime<Utc>>,
}

#[derive(Deserialize)]
struct StatusResponse {
    healthy_count: usize,
    degraded_count: usize,
    failed_count: usize,
    unknown_count: usize,
    total_services: usize,
    services: Vec<ServiceSummary>,
}

#[derive(Deserialize)]
struct HealthCheckRecord {
    success: bool,
    latency_ms: u64,
    checked_at: DateTime<Utc>,
    error: Option<String>,
}

#[derive(Deserialize)]
struct AlertRecord {
    kind: String,
    delivered: bool,
    trigger: String,
    sent_at: DateTime<Utc>,
}

#[derive(Deserialize)]
struct RestartRecord {
    attempted_at: DateTime<Utc>,
    succeeded: bool,
    attempt_number: u32,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    let base_url = cli
        .url
        .or_else(|| std::env::var("VELA_URL").ok())
        .unwrap_or_else(|| DEFAULT_URL.to_string());
    let base_url = base_url.trim_end_matches('/').to_string();

    let api_key = match cli.key.or_else(|| std::env::var("VELA_API_KEY").ok()) {
        Some(k) if !k.trim().is_empty() => k,
        _ => {
            eprintln!(
                "{} VELA_API_KEY is not set.\n\n\
                 Set it with:\n  export VELA_API_KEY=\"your-api-key\"\n\n\
                 Or pass it directly:\n  vela-ctl --key \"your-api-key\" status",
                "Error:".red().bold()
            );
            return ExitCode::FAILURE;
        }
    };

    let client = match reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "{} failed to build HTTP client: {}",
                "Error:".red().bold(),
                e
            );
            return ExitCode::FAILURE;
        }
    };

    let result = match cli.command {
        Command::Status => run_status(&client, &base_url, &api_key),
        Command::Services {
            id: None,
            resource: _,
        } => run_status(&client, &base_url, &api_key),
        Command::Services {
            id: Some(id),
            resource: None,
        } => run_service_detail(&client, &base_url, &api_key, &id),
        Command::Services {
            id: Some(id),
            resource: Some(r),
        } => match r.as_str() {
            "checks" => run_checks(&client, &base_url, &api_key, &id),
            "alerts" => run_alerts(&client, &base_url, &api_key, &id),
            "restarts" => run_restarts(&client, &base_url, &api_key, &id),
            other => Err(format!(
                "Unknown sub-resource '{}'. Expected one of: checks, alerts, restarts",
                other
            )),
        },
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("{} {}", "Error:".red().bold(), msg);
            ExitCode::FAILURE
        }
    }
}

/// Performs an authenticated GET against the Vela API and unwraps the
/// standard response envelope. `not_found_id`, when given, produces a
/// precise "Service '<id>' not found." message on a 404 instead of a
/// generic one.
fn get_json<T: DeserializeOwned>(
    client: &reqwest::blocking::Client,
    base_url: &str,
    api_key: &str,
    path: &str,
    not_found_id: Option<&str>,
) -> Result<T, String> {
    let url = format!("{}{}", base_url, path);

    let response = client
        .get(&url)
        .header("Authorization", format!("Bearer {}", api_key))
        .send()
        .map_err(|e| {
            if e.is_connect() || e.is_timeout() {
                format!("Cannot reach Vela at {}. Is it running?", base_url)
            } else {
                format!("Request failed: {}", e)
            }
        })?;

    let status = response.status();

    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err("Authentication failed. Check your API key.".to_string());
    }

    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(match not_found_id {
            Some(id) => format!("Service '{}' not found.", id),
            None => "Not found.".to_string(),
        });
    }

    if !status.is_success() {
        return Err(format!("Vela API returned HTTP {}.", status.as_u16()));
    }

    let envelope: Envelope<T> = response
        .json()
        .map_err(|e| format!("Failed to parse response from Vela: {}", e))?;

    if !envelope.ok {
        return Err(envelope
            .error
            .unwrap_or_else(|| "Unknown API error".to_string()));
    }

    envelope
        .data
        .ok_or_else(|| "Empty response from Vela".to_string())
}

// ─── Commands ────────────────────────────────────────────────────────────

fn run_status(
    client: &reqwest::blocking::Client,
    base_url: &str,
    api_key: &str,
) -> Result<(), String> {
    let status: StatusResponse = get_json(client, base_url, api_key, "/api/v1/status", None)?;

    println!(
        "{} healthy  {} degraded  {} failed  {} unknown  ({} total)",
        status.healthy_count.to_string().green(),
        status.degraded_count.to_string().yellow(),
        status.failed_count.to_string().red(),
        status.unknown_count.to_string().bright_black(),
        status.total_services
    );
    println!("{}", "─".repeat(88));
    println!(
        "{:<24} {:<14} {:<10} {:<10} LAST CHECK",
        "SERVICE", "STATUS", "FAILURES", "RESTARTS"
    );

    let mut services = status.services;
    services.sort_by(|a, b| a.service_id.cmp(&b.service_id));

    for s in &services {
        println!(
            "{:<24} {} {:<10} {:<10} {}",
            truncate(&s.service_id, 24),
            status_display_padded(&s.status, 14),
            s.consecutive_failures,
            s.restart_count,
            s.last_checked_at
                .map(fmt_time)
                .unwrap_or_else(|| "never".to_string())
        );
    }

    Ok(())
}

fn run_service_detail(
    client: &reqwest::blocking::Client,
    base_url: &str,
    api_key: &str,
    id: &str,
) -> Result<(), String> {
    let path = format!("/api/v1/services/{}", id);
    let svc: ServiceSummary = get_json(client, base_url, api_key, &path, Some(id))?;

    println!("{}", svc.service_name.bold());
    println!("{}", "─".repeat(50));
    println!("{:<22} {}", "Service ID:", svc.service_id);
    println!("{:<22} {}", "Status:", status_display(&svc.status));
    println!(
        "{:<22} {}",
        "Consecutive failures:", svc.consecutive_failures
    );
    println!("{:<22} {}", "Restart count:", svc.restart_count);
    println!(
        "{:<22} {}",
        "Last checked:",
        svc.last_checked_at
            .map(fmt_time)
            .unwrap_or_else(|| "never".to_string())
    );
    println!(
        "{:<22} {}",
        "Last success:",
        svc.last_ok_at
            .map(fmt_time)
            .unwrap_or_else(|| "never".to_string())
    );

    Ok(())
}

fn run_checks(
    client: &reqwest::blocking::Client,
    base_url: &str,
    api_key: &str,
    id: &str,
) -> Result<(), String> {
    let path = format!("/api/v1/services/{}/checks", id);
    let records: Vec<HealthCheckRecord> = get_json(client, base_url, api_key, &path, Some(id))?;

    if records.is_empty() {
        println!("No health check records for '{}'.", id);
        return Ok(());
    }

    println!(
        "{:<22} {:<12} {:<10} ERROR",
        "TIMESTAMP", "RESULT", "LATENCY"
    );
    println!("{}", "─".repeat(88));

    for r in records.iter().rev().take(15) {
        let result = if r.success {
            pad_then_color("✓ Success", 12, |s| s.green())
        } else {
            pad_then_color("✗ Failed", 12, |s| s.red())
        };
        let latency = if r.success {
            format!("{}ms", r.latency_ms)
        } else {
            "—".to_string()
        };
        println!(
            "{:<22} {} {:<10} {}",
            fmt_time(r.checked_at),
            result,
            latency,
            r.error.as_deref().unwrap_or("—")
        );
    }

    Ok(())
}

fn run_alerts(
    client: &reqwest::blocking::Client,
    base_url: &str,
    api_key: &str,
    id: &str,
) -> Result<(), String> {
    let path = format!("/api/v1/services/{}/alerts", id);
    let records: Vec<AlertRecord> = get_json(client, base_url, api_key, &path, Some(id))?;

    if records.is_empty() {
        println!("No alert records for '{}'.", id);
        return Ok(());
    }

    println!(
        "{:<22} {:<20} {:<10} STATUS",
        "TIMESTAMP", "TRANSITION", "KIND"
    );
    println!("{}", "─".repeat(88));

    for a in records.iter().rev().take(10) {
        let status_str = if a.delivered {
            pad_then_color("Delivered", 10, |s| s.green())
        } else {
            pad_then_color("Failed", 10, |s| s.red())
        };
        println!(
            "{:<22} {:<20} {:<10} {}",
            fmt_time(a.sent_at),
            a.trigger,
            a.kind,
            status_str
        );
    }

    Ok(())
}

fn run_restarts(
    client: &reqwest::blocking::Client,
    base_url: &str,
    api_key: &str,
    id: &str,
) -> Result<(), String> {
    let path = format!("/api/v1/services/{}/restarts", id);
    let records: Vec<RestartRecord> = get_json(client, base_url, api_key, &path, Some(id))?;

    if records.is_empty() {
        println!("No restart records for '{}'.", id);
        return Ok(());
    }

    println!("{:<22} {:<10} OUTCOME", "TIMESTAMP", "ATTEMPT #");
    println!("{}", "─".repeat(60));

    for r in records.iter().rev().take(10) {
        let outcome = if r.succeeded {
            pad_then_color("Success", 10, |s| s.green())
        } else {
            pad_then_color("Failed", 10, |s| s.red())
        };
        println!(
            "{:<22} {:<10} {}",
            fmt_time(r.attempted_at),
            r.attempt_number,
            outcome
        );
    }

    Ok(())
}

// ─── Display helpers ─────────────────────────────────────────────────────

/// Pads plain text to `width` visible characters BEFORE applying color.
/// Colorizing first and padding second would count the ANSI escape bytes
/// as part of the width, breaking column alignment in the terminal.
fn pad_then_color(text: &str, width: usize, color_fn: impl Fn(&str) -> ColoredString) -> String {
    let padded = format!("{:<width$}", text, width = width);
    color_fn(&padded).to_string()
}

fn status_symbol_label(status: &str) -> (&'static str, &'static str) {
    match status {
        "Healthy" => ("●", "Healthy"),
        "Degraded" => ("◐", "Degraded"),
        "Failed" => ("✗", "Failed"),
        _ => ("○", "Unknown"),
    }
}

fn colorize(status: &str, text: &str) -> ColoredString {
    match status {
        "Healthy" => text.green(),
        "Degraded" => text.yellow(),
        "Failed" => text.red(),
        _ => text.bright_black(),
    }
}

fn status_display(status: &str) -> ColoredString {
    let (sym, label) = status_symbol_label(status);
    colorize(status, &format!("{} {}", sym, label))
}

fn status_display_padded(status: &str, width: usize) -> String {
    let (sym, label) = status_symbol_label(status);
    let plain = format!("{} {}", sym, label);
    pad_then_color(&plain, width, |s| colorize(status, s))
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

fn fmt_time(t: DateTime<Utc>) -> String {
    t.with_timezone(&Local)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}
