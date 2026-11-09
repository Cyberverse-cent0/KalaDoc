//! `kaladoc init` (and the hidden `--init-db` alias)
//!
//! Resolves configuration, opens/migrates the database, runs one
//! session-cleanup pass, and reports. Long-running services should call
//! `session::spawn_cleanup_task` instead.

use serde::Serialize;

use crate::crates::application::Organization::init_db;
use crate::crates::application::Organization::session;

use super::{CliError, Context};

#[derive(Debug, Serialize)]
struct InitReport {
    database: String,
    max_connections: u32,
    expired_sessions_removed: u64,
}

pub async fn run(ctx: &Context) -> Result<i32, CliError> {
    let db_config = ctx.db_config()?;
    let pool = init_db::init_db_with(&db_config).await?;

    let removed = session::cleanup_expired_sessions(&pool).await?;

    let report = InitReport {
        database: db_config.db_path.display().to_string(),
        max_connections: db_config.max_connections,
        expired_sessions_removed: removed,
    };

    if ctx.json() {
        ctx.emit_json(&report)?;
    } else {
        println!(
            "database ready: {} ({} max connections)",
            report.database, report.max_connections
        );
        if removed > 0 {
            println!("removed {removed} expired session(s)");
        }
    }

    pool.close().await;
    Ok(0)
}
