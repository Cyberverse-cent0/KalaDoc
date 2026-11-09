//! `kaladoc doctor` — environment, configuration, database, and storage
//! health checks. Read-only: it never migrates schema or writes data.

use serde::Serialize;

use crate::crates::application::Organization::init_db;
use crate::crates::shared_public::platform::platform;
use crate::deployments::require;

use super::{CliError, Context};

#[derive(Debug, Serialize)]
struct Check {
    name: &'static str,
    status: &'static str, // "ok" | "warn" | "fail"
    detail: String,
}

#[derive(Debug, Serialize)]
struct DoctorReport {
    overall: &'static str,
    checks: Vec<Check>,
}

pub async fn run(ctx: &Context) -> Result<i32, CliError> {
    let mut checks = Vec::new();

    checks.push(check_platform());
    checks.push(check_requirements());
    checks.push(check_config(ctx));
    checks.push(check_secrets(ctx));
    checks.push(check_database(ctx).await);
    checks.push(check_storage(ctx));

    let overall = if checks.iter().any(|c| c.status == "fail") {
        "fail"
    } else if checks.iter().any(|c| c.status == "warn") {
        "warn"
    } else {
        "ok"
    };

    let report = DoctorReport { overall, checks };

    if ctx.json() {
        ctx.emit_json(&report)?;
    } else {
        println!("kaladoc doctor — overall: {}", report.overall);
        for c in &report.checks {
            let mark = match c.status {
                "ok" => "ok  ",
                "warn" => "warn",
                _ => "FAIL",
            };
            println!("{mark} {:<14} {}", c.name, c.detail);
        }
    }

    // Exit non-zero when anything failed so scripts can gate on it.
    Ok(if overall == "fail" { 1 } else { 0 })
}

fn check_platform() -> Check {
    Check {
        name: "platform",
        status: "ok",
        detail: platform().describe(),
    }
}

fn check_requirements() -> Check {
    let report = require::check_requirements();
    if report.missing.is_empty() {
        Check {
            name: "dependencies",
            status: "ok",
            detail: format!("{} checked, all present", report.dependencies.len()),
        }
    } else {
        Check {
            name: "dependencies",
            status: "fail",
            detail: format!("missing: {}", report.missing.join(", ")),
        }
    }
}

fn check_config(ctx: &Context) -> Check {
    // Lenient: secrets may be absent; we only care the file/env parse.
    match ctx.config_lenient() {
        Ok(cfg) => {
            let source = ctx
                .globals
                .config
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "defaults + env".to_string());
            Check {
                name: "config",
                status: "ok",
                detail: format!("loaded ({source}); db path {}", cfg.database.path.display()),
            }
        }
        Err(e) => Check {
            name: "config",
            status: "fail",
            detail: e.to_string(),
        },
    }
}

fn check_secrets(ctx: &Context) -> Check {
    match ctx.config_lenient() {
        Ok(cfg) => {
            let mut missing = Vec::new();
            if cfg.secret_key.is_empty() {
                missing.push("APP_SECRET_KEY");
            }
            if cfg.db_user.is_empty() {
                missing.push("APP_DB_USER");
            }
            if cfg.db_password.is_empty() {
                missing.push("APP_DB_PASSWORD");
            }
            if missing.is_empty() {
                Check {
                    name: "secrets",
                    status: "ok",
                    detail: "required env vars present".to_string(),
                }
            } else {
                Check {
                    name: "secrets",
                    status: "fail",
                    detail: format!("missing: {}", missing.join(", ")),
                }
            }
        }
        Err(e) => Check {
            name: "secrets",
            status: "fail",
            detail: e.to_string(),
        },
    }
}

async fn check_database(ctx: &Context) -> Check {
    // Strict load: doctor reports the real error when secrets are absent.
    let db = match ctx.db_config() {
        Ok(db) => db,
        Err(e) => {
            return Check {
                name: "database",
                status: "fail",
                detail: e.to_string(),
            };
        }
    };

    // open_db only — doctor must not create/migrate schema as a side effect
    // beyond what a normal connect does.
    match init_db::open_db(&db).await {
        Ok(pool) => {
            let tables_ok = sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type='table' AND name IN ('users_tbl','sessions_tbl')",
            )
            .fetch_one(&pool)
            .await
            .map(|n| n == 2)
            .unwrap_or(false);
            pool.close().await;

            if tables_ok {
                Check {
                    name: "database",
                    status: "ok",
                    detail: format!("{} reachable, schema present", db.db_path.display()),
                }
            } else {
                Check {
                    name: "database",
                    status: "warn",
                    detail: format!(
                        "{} reachable but users_tbl/sessions_tbl missing — run `kaladoc init`",
                        db.db_path.display()
                    ),
                }
            }
        }
        Err(e) => Check {
            name: "database",
            status: "fail",
            detail: format!("{}: {e}", db.db_path.display()),
        },
    }
}

fn check_storage(ctx: &Context) -> Check {
    let cfg = match ctx.config_lenient() {
        Ok(c) => c.to_storage_config(),
        Err(e) => {
            return Check {
                name: "storage",
                status: "fail",
                detail: e.to_string(),
            };
        }
    };

    let root = &cfg.root_dir;
    if let Err(e) = std::fs::create_dir_all(root) {
        return Check {
            name: "storage",
            status: "fail",
            detail: format!("cannot create {}: {e}", root.display()),
        };
    }

    // Write-probe: create and remove a dotfile inside the root.
    let probe = root.join(".kaladoc_doctor_probe");
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            Check {
                name: "storage",
                status: "ok",
                detail: format!("{} writable", root.display()),
            }
        }
        Err(e) => Check {
            name: "storage",
            status: "fail",
            detail: format!("{} not writable: {e}", root.display()),
        },
    }
}
