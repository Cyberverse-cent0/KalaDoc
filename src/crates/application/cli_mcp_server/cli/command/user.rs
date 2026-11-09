//! `kaladoc user create|list|delete`
//!
//! Account management over `auth_user` service functions. Passwords are
//! prompted without echo (or read as a single stdin line with
//! `--password-stdin`) and are hashed with Argon2id before storage —
//! plaintext never touches the database or the command line.

use serde::Serialize;

use crate::crates::application::Organization::auth_user::{self, NewUser};
use crate::crates::application::Organization::init_db;

use super::super::args::UserArgs;
use super::{CliError, Context};

pub async fn run(ctx: &Context, args: &UserArgs) -> Result<i32, CliError> {
    match args {
        UserArgs::Create {
            username,
            email,
            first_name,
            last_name,
            password_stdin,
        } => {
            create(
                ctx,
                username,
                email,
                first_name,
                last_name,
                *password_stdin,
            )
            .await
        }
        UserArgs::List => list(ctx).await,
        UserArgs::Delete { username } => delete(ctx, username).await,
    }
}

// ---------------------------------------------------------------------------
// create
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct CreateReport {
    user_id: i64,
    user_name: String,
    email: String,
}

async fn create(
    ctx: &Context,
    username: &str,
    email: &str,
    first_name: &str,
    last_name: &str,
    password_stdin: bool,
) -> Result<i32, CliError> {
    let password = if password_stdin {
        read_password_line("Password: ")?
    } else {
        prompt_password("Password: ")?
    };
    if password.is_empty() {
        return Err(CliError::Usage("password must not be empty".to_string()));
    }

    let db = ctx.db_config()?;
    let pool = init_db::init_db_with(&db).await?;

    let new = NewUser {
        first_name: first_name.to_string(),
        last_name: last_name.to_string(),
        user_name: username.to_string(),
        email: email.to_string(),
        password,
    };
    let user_id = auth_user::create_user(&pool, &new).await?;
    pool.close().await;

    let report = CreateReport {
        user_id,
        user_name: username.to_string(),
        email: email.to_string(),
    };

    if ctx.emit_structured(&report)? {
        return Ok(0);
    }
    println!("created user '{}' (id {})", report.user_name, report.user_id);
    Ok(0)
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

async fn list(ctx: &Context) -> Result<i32, CliError> {
    let db = ctx.db_config()?;
    let pool = init_db::init_db_with(&db).await?;
    let users = auth_user::list_users(&pool).await?;
    pool.close().await;

    if ctx.json() {
        ctx.emit_json(&users)?;
    } else {
        println!(
            "{:<4} {:<20} {:<28} {:<20} name",
            "id", "user_name", "email", "first/last"
        );
        for u in &users {
            println!(
                "{:<4} {:<20} {:<28} {} {}",
                u.id, u.user_name, u.email, u.first_name, u.last_name
            );
        }
        if users.is_empty() {
            println!("(no users)");
        }
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// delete
// ---------------------------------------------------------------------------

async fn delete(ctx: &Context, username: &str) -> Result<i32, CliError> {
    let db = ctx.db_config()?;
    let pool = init_db::init_db_with(&db).await?;
    let removed = auth_user::remove_user(&pool, username).await?;
    pool.close().await;

    if !removed {
        return Err(CliError::NotFound(format!("user {username:?}")));
    }

    if ctx.json() {
        ctx.emit_json(&serde_json::json!({ "deleted": username }))?;
    } else {
        println!("deleted user '{username}'");
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

/// Password prompt: no echo on a TTY; a single stdin line otherwise
/// (same convention as `auth login`).
fn prompt_password(prompt: &str) -> Result<String, CliError> {
    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() {
        return Ok(rpassword::prompt_password(prompt)?);
    }
    read_password_line(prompt)
}

/// Reads one line from stdin for `--password-stdin` and non-TTY prompts.
fn read_password_line(prompt: &str) -> Result<String, CliError> {
    eprint!("{prompt}");
    use std::io::Write;
    std::io::stderr().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim_end_matches(['\n', '\r']).to_string())
}
