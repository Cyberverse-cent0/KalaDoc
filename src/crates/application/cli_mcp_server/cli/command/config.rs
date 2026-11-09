//! `kaladoc config show|validate|get`
//!
//! Secrets are never printed — `show` reports whether each is set, and
//! `get` refuses secret keys outright.

use serde::Serialize;

use crate::config::AppConfig;

use super::super::args::ConfigArgs;
use super::{CliError, Context};

#[derive(Debug, Serialize)]
struct ConfigReport {
    source: String,
    database: DatabaseView,
    storage: StorageView,
    runtime: RuntimeView,
    secrets: SecretsView,
}

#[derive(Debug, Serialize)]
struct DatabaseView {
    path: String,
    max_connections: u32,
    acquire_timeout_secs: u64,
    session: SessionView,
}

#[derive(Debug, Serialize)]
struct SessionView {
    ttl_secs: u64,
    absolute_ttl_secs: u64,
    renew_on_use: bool,
    touch_interval_secs: u64,
    cleanup_interval_secs: u64,
}

#[derive(Debug, Serialize)]
struct StorageView {
    root_dir: String,
    max_size_bytes: Option<u64>,
    filesystem: Option<String>,
}

#[derive(Debug, Serialize)]
struct RuntimeView {
    log_level: String,
}

#[derive(Debug, Serialize)]
struct SecretsView {
    app_secret_key: &'static str,
    app_db_user: &'static str,
    app_db_password: &'static str,
}

pub fn run(ctx: &Context, args: &ConfigArgs) -> Result<i32, CliError> {
    match args {
        ConfigArgs::Show => show(ctx),
        ConfigArgs::Validate => validate(ctx),
        ConfigArgs::Get { key } => get(ctx, key),
    }
}

fn source_label(ctx: &Context) -> String {
    ctx.globals
        .config
        .as_ref()
        .map(|p| format!("file:{}", p.display()))
        .or_else(|| std::env::var_os("KALADOC_CONFIG").map(|p| format!("file:{p:?}")))
        .unwrap_or_else(|| {
            if std::path::Path::new("kaladoc.toml").is_file() {
                "file:kaladoc.toml".to_string()
            } else {
                "defaults+env".to_string()
            }
        })
}

fn report(cfg: &AppConfig, source: String) -> ConfigReport {
    ConfigReport {
        source,
        database: DatabaseView {
            path: cfg.database.path.display().to_string(),
            max_connections: cfg.database.max_connections,
            acquire_timeout_secs: cfg.database.acquire_timeout_secs,
            session: SessionView {
                ttl_secs: cfg.database.session.ttl_secs,
                absolute_ttl_secs: cfg.database.session.absolute_ttl_secs,
                renew_on_use: cfg.database.session.renew_on_use,
                touch_interval_secs: cfg.database.session.touch_interval_secs,
                cleanup_interval_secs: cfg.database.session.cleanup_interval_secs,
            },
        },
        storage: StorageView {
            root_dir: cfg.storage.root_dir.display().to_string(),
            max_size_bytes: cfg.storage.max_size_bytes,
            filesystem: cfg.storage.filesystem.clone(),
        },
        runtime: RuntimeView {
            log_level: cfg.runtime.log_level.clone(),
        },
        secrets: SecretsView {
            app_secret_key: if cfg.secret_key.is_empty() {
                "unset"
            } else {
                "set (redacted)"
            },
            app_db_user: if cfg.db_user.is_empty() {
                "unset"
            } else {
                "set (redacted)"
            },
            app_db_password: if cfg.db_password.is_empty() {
                "unset"
            } else {
                "set (redacted)"
            },
        },
    }
}

fn show(ctx: &Context) -> Result<i32, CliError> {
    // Lenient: `show` works before secrets are configured.
    let cfg = ctx.config_lenient()?;
    let report = report(&cfg, source_label(ctx));

    if ctx.json() {
        ctx.emit_json(&report)?;
    } else {
        println!("config source: {}", report.source);
        println!("database.path: {}", report.database.path);
        println!(
            "database.max_connections: {}",
            report.database.max_connections
        );
        println!(
            "database.acquire_timeout_secs: {}",
            report.database.acquire_timeout_secs
        );
        println!(
            "database.session.ttl_secs: {}",
            report.database.session.ttl_secs
        );
        println!(
            "database.session.absolute_ttl_secs: {}",
            report.database.session.absolute_ttl_secs
        );
        println!(
            "database.session.renew_on_use: {}",
            report.database.session.renew_on_use
        );
        println!(
            "database.session.touch_interval_secs: {}",
            report.database.session.touch_interval_secs
        );
        println!(
            "database.session.cleanup_interval_secs: {}",
            report.database.session.cleanup_interval_secs
        );
        println!("storage.root_dir: {}", report.storage.root_dir);
        match report.storage.max_size_bytes {
            Some(v) => println!("storage.max_size_bytes: {v}"),
            None => println!("storage.max_size_bytes: (unset)"),
        }
        match &report.storage.filesystem {
            Some(v) => println!("storage.filesystem: {v}"),
            None => println!("storage.filesystem: (unset)"),
        }
        println!("runtime.log_level: {}", report.runtime.log_level);
        println!("secrets.app_secret_key: {}", report.secrets.app_secret_key);
        println!("secrets.app_db_user: {}", report.secrets.app_db_user);
        println!(
            "secrets.app_db_password: {}",
            report.secrets.app_db_password
        );
    }
    Ok(0)
}

fn validate(ctx: &Context) -> Result<i32, CliError> {
    // Strict: every layer of validation runs inside load(), including the
    // required-secret check.
    let cfg = ctx.config()?;
    let report = report(&cfg, source_label(ctx));

    if ctx.json() {
        ctx.emit_json(&serde_json::json!({
            "valid": true,
            "source": report.source,
        }))?;
    } else {
        println!("configuration valid (source: {})", report.source);
    }
    Ok(0)
}

fn get(ctx: &Context, key: &str) -> Result<i32, CliError> {
    if is_secret_key(key) {
        return Err(CliError::Usage(format!(
            "{key} is a secret and cannot be displayed"
        )));
    }

    // Lenient so `get` works for non-secret keys pre-configuration.
    let cfg = ctx.config_lenient()?;
    let value = lookup(&cfg, key)
        .ok_or_else(|| CliError::NotFound(format!("unknown configuration key {key:?}")))?;

    if ctx.json() {
        ctx.emit_json(&serde_json::json!({ "key": key, "value": value }))?;
    } else {
        println!("{key} = {value}");
    }
    Ok(0)
}

fn is_secret_key(key: &str) -> bool {
    matches!(
        key,
        "secret_key"
            | "db_user"
            | "db_password"
            | "secrets.app_secret_key"
            | "secrets.app_db_user"
            | "secrets.app_db_password"
    )
}

fn lookup(cfg: &AppConfig, key: &str) -> Option<String> {
    let s = &cfg.database.session;
    Some(match key {
        "database.path" => cfg.database.path.display().to_string(),
        "database.max_connections" => cfg.database.max_connections.to_string(),
        "database.acquire_timeout_secs" => cfg.database.acquire_timeout_secs.to_string(),
        "database.session.ttl_secs" => s.ttl_secs.to_string(),
        "database.session.absolute_ttl_secs" => s.absolute_ttl_secs.to_string(),
        "database.session.renew_on_use" => s.renew_on_use.to_string(),
        "database.session.touch_interval_secs" => s.touch_interval_secs.to_string(),
        "database.session.cleanup_interval_secs" => s.cleanup_interval_secs.to_string(),
        "storage.root_dir" => cfg.storage.root_dir.display().to_string(),
        "storage.max_size_bytes" => cfg.storage.max_size_bytes?.to_string(),
        "storage.filesystem" => cfg.storage.filesystem.clone()?,
        "runtime.log_level" => cfg.runtime.log_level.clone(),
        _ => return None,
    })
}
