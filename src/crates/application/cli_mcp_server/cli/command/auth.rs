//! `kaladoc auth login|logout|status|whoami|session list|session revoke`
//!
//! Login validates credentials against `users_tbl` (Argon2id) and stores
//! the issued bearer token in the OS config dir via [`credentials`]. The
//! token is never printed, logged, or placed in command arguments.

use serde::Serialize;
use sqlx::SqlitePool;

use crate::crates::application::Organization::auth_user;
use crate::crates::application::Organization::init_db;
use crate::crates::application::Organization::session::{self, Session};

use super::super::args::{AuthArgs, SessionArgs};
use super::super::credentials::{self, Credentials};
use super::{CliError, Context};

/// An authenticated request context: open pool, session config, stored
/// credentials, and the validated session.
struct AuthCtx {
    pool: SqlitePool,
    creds: Credentials,
    session: Session,
}

#[derive(Debug, Serialize)]
struct LoginReport {
    user_name: String,
    session_id: i64,
    expires_at: i64,
}

#[derive(Debug, Serialize)]
struct StatusReport {
    authenticated: bool,
    user_name: Option<String>,
    session_id: Option<i64>,
    expires_at: Option<i64>,
}

#[derive(Debug, Serialize)]
struct WhoamiReport {
    id: i64,
    user_name: String,
    email: String,
    first_name: String,
    last_name: String,
}

#[derive(Debug, Serialize)]
struct SessionView {
    id: i64,
    created_at: i64,
    expires_at: i64,
    last_seen_at: i64,
    current: bool,
}

pub async fn run(ctx: &Context, args: &AuthArgs) -> Result<i32, CliError> {
    match args {
        AuthArgs::Login { username } => login(ctx, username.as_deref()).await,
        AuthArgs::Logout => logout(ctx).await,
        AuthArgs::Status => status(ctx).await,
        AuthArgs::Whoami => whoami(ctx).await,
        AuthArgs::Session(sub) => match sub {
            SessionArgs::List => session_list(ctx).await,
            SessionArgs::Revoke { id } => session_revoke(ctx, *id).await,
        },
    }
}

// ---------------------------------------------------------------------------
// login / logout
// ---------------------------------------------------------------------------

async fn login(ctx: &Context, username_arg: Option<&str>) -> Result<i32, CliError> {
    let username = match username_arg {
        Some(u) => u.trim().to_string(),
        None => prompt_username()?,
    };
    if username.is_empty() {
        return Err(CliError::Usage("user name must not be empty".to_string()));
    }

    // Password is read without echo on a TTY; piped stdin reads one line.
    // It never appears in argv or logs.
    let password = prompt_password()?;
    if password.is_empty() {
        return Err(CliError::Usage("password must not be empty".to_string()));
    }

    let db = ctx.db_config()?;
    let pool = init_db::init_db_with(&db).await?;

    let Some(user) = auth_user::validate_user(&pool, &username, &password).await? else {
        pool.close().await;
        return Err(CliError::AuthFailed);
    };

    let (sess, token) = session::create_session(&pool, user.id, &db.session).await?;
    credentials::store(&Credentials::new(token, user.user_name.clone()))?;

    let report = LoginReport {
        user_name: user.user_name.clone(),
        session_id: sess.id,
        expires_at: sess.expires_at,
    };

    if ctx.json() {
        ctx.emit_json(&report)?;
    } else {
        println!("logged in as {}", report.user_name);
        println!("session {} expires at {}", report.session_id, report.expires_at);
    }

    pool.close().await;
    Ok(0)
}

async fn logout(ctx: &Context) -> Result<i32, CliError> {
    let Some(creds) = credentials::load()? else {
        if ctx.json() {
            ctx.emit_json(&serde_json::json!({ "logged_out": false }))?;
        } else {
            println!("not logged in");
        }
        return Ok(0);
    };

    // Best-effort server-side revocation; local logout succeeds even when
    // the database is unreachable.
    if let Ok(db) = ctx.db_config()
        && let Ok(pool) = init_db::init_db_with(&db).await
    {
        let _ = session::revoke_session(&pool, &creds.token).await;
        pool.close().await;
    }

    let removed = credentials::clear()?;

    if ctx.json() {
        ctx.emit_json(&serde_json::json!({ "logged_out": removed }))?;
    } else {
        println!("logged out");
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// status / whoami
// ---------------------------------------------------------------------------

async fn status(ctx: &Context) -> Result<i32, CliError> {
    let report = match try_authenticate(ctx).await? {
        Some(auth) => StatusReport {
            authenticated: true,
            user_name: Some(auth.creds.user_name.clone()),
            session_id: Some(auth.session.id),
            expires_at: Some(auth.session.expires_at),
        },
        None => StatusReport {
            authenticated: false,
            user_name: None,
            session_id: None,
            expires_at: None,
        },
    };

    if ctx.json() {
        ctx.emit_json(&report)?;
    } else if report.authenticated {
        println!(
            "authenticated as {} (session {}, expires {})",
            report.user_name.as_deref().unwrap_or("?"),
            report.session_id.unwrap_or(0),
            report.expires_at.unwrap_or(0)
        );
    } else {
        println!("not authenticated");
    }
    Ok(0)
}

async fn whoami(ctx: &Context) -> Result<i32, CliError> {
    let auth = require_auth(ctx).await?;
    let user = auth_user::get_user_by_id(&auth.pool, auth.session.user_id)
        .await?
        .ok_or_else(|| CliError::NotFound("user no longer exists".to_string()))?;

    let report = WhoamiReport {
        id: user.id,
        user_name: user.user_name,
        email: user.email,
        first_name: user.first_name,
        last_name: user.last_name,
    };

    if ctx.json() {
        ctx.emit_json(&report)?;
    } else {
        println!("{} <{}>", report.user_name, report.email);
        println!("{} {}", report.first_name, report.last_name);
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// session list / revoke
// ---------------------------------------------------------------------------

async fn session_list(ctx: &Context) -> Result<i32, CliError> {
    let auth = require_auth(ctx).await?;
    let sessions = session::list_active_sessions(&auth.pool, auth.session.user_id).await?;

    let views: Vec<SessionView> = sessions
        .iter()
        .map(|s| SessionView {
            id: s.id,
            created_at: s.created_at,
            expires_at: s.expires_at,
            last_seen_at: s.last_seen_at,
            current: s.id == auth.session.id,
        })
        .collect();

    if ctx.json() {
        ctx.emit_json(&views)?;
    } else {
        for v in &views {
            let mark = if v.current { "*" } else { " " };
            println!(
                "{mark} {}  created={} expires={} last_seen={}",
                v.id, v.created_at, v.expires_at, v.last_seen_at
            );
        }
    }
    Ok(0)
}

async fn session_revoke(ctx: &Context, id: i64) -> Result<i32, CliError> {
    let auth = require_auth(ctx).await?;

    // Ownership check: only sessions belonging to the current user may be
    // revoked through this command.
    let owns = session::list_active_sessions(&auth.pool, auth.session.user_id)
        .await?
        .iter()
        .any(|s| s.id == id);
    if !owns {
        return Err(CliError::NotFound(format!("session {id}")));
    }

    let revoked = session::revoke_session_by_id(&auth.pool, id).await?;
    if !revoked {
        return Err(CliError::NotFound(format!("session {id}")));
    }

    if ctx.json() {
        ctx.emit_json(&serde_json::json!({ "revoked": id }))?;
    } else {
        println!("revoked session {id}");
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

/// Reads the password without echo when attached to a terminal; otherwise
/// consumes a single line from stdin (for `echo pass | kaladoc auth
/// login --username ...` style scripting).
fn prompt_password() -> Result<String, CliError> {
    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() {
        return Ok(rpassword::prompt_password("Password: ")?);
    }
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim_end_matches(['\n', '\r']).to_string())
}

fn prompt_username() -> Result<String, CliError> {
    eprint!("Username: ");
    use std::io::Write;
    std::io::stderr().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

/// Loads stored credentials, opens the database, and validates the
/// session. `Ok(None)` means "not authenticated" (no creds or the session
/// expired/revoked).
async fn try_authenticate(ctx: &Context) -> Result<Option<AuthCtx>, CliError> {
    let Some(creds) = credentials::load()? else {
        return Ok(None);
    };
    let db = ctx.db_config()?;
    let pool = init_db::init_db_with(&db).await?;
    let session_cfg = db.session.clone();

    match session::validate_session(&pool, &creds.token, &session_cfg).await? {
        Some(sess) => Ok(Some(AuthCtx {
            pool,
            creds,
            session: sess,
        })),
        None => {
            pool.close().await;
            Ok(None)
        }
    }
}

/// Like [`try_authenticate`] but fails with [`CliError::AuthRequired`]
/// when there is no valid session.
async fn require_auth(ctx: &Context) -> Result<AuthCtx, CliError> {
    match try_authenticate(ctx).await? {
        Some(auth) => Ok(auth),
        None => Err(CliError::AuthRequired),
    }
}
