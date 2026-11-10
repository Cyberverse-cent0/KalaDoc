//! The `kaladoc` command-line interface (spec §3–§9).
//!
//! [`run`] parses arguments, builds a [`Context`], dispatches to
//! [`command`] handlers, and maps [`CliError`] onto the stable exit
//! codes and JSON error shape from spec §5. Commands are adapters over
//! the `application`/`storage` services — no business logic lives here.

mod args;
mod command;
mod context;
mod credentials;
mod envfile;
mod error;
mod global;
mod output;

pub(crate) use context::Context;
pub use error::CliError;

use args::{Cli, Commands};
use clap::{CommandFactory, Parser};
use output::OutputFormat;

/// Parse arguments, execute the command, and return the process exit
/// code (never panics on user error).
pub async fn run() -> i32 {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        // clap prints help/usage errors and exits the process itself.
        Err(e) => e.exit(),
    };

    init_logging(cli.global.verbose, cli.global.quiet);
    let json_errors = cli.global.output == OutputFormat::Json;

    let result = match Context::new(&cli) {
        Ok(ctx) => dispatch(&cli, ctx).await,
        Err(e) => Err(e),
    };

    match result {
        Ok(code) => code,
        Err(e) => {
            report_error(&e, json_errors);
            e.exit_code()
        }
    }
}

async fn dispatch(cli: &Cli, ctx: Context) -> Result<i32, CliError> {
    if cli.init_db {
        return command::init::run(&ctx).await;
    }

    let Some(cmd) = &cli.command else {
        // Bare `kaladoc` → print help.
        Cli::command().print_help()?;
        println!();
        return Ok(0);
    };

    match cmd {
        Commands::Version => command::version::run(&ctx),
        Commands::Doctor => command::doctor::run(&ctx).await,
        Commands::Init => command::init::run(&ctx).await,
        Commands::Config(args) => command::config::run(&ctx, args),
        Commands::Auth(args) => command::auth::run(&ctx, args).await,
        Commands::User(args) => command::user::run(&ctx, args).await,
        Commands::Jobs(args) => command::jobs::run(&ctx, args).await,
        Commands::Storage(args) => command::storage::run(&ctx, args).await,
        Commands::Schedule(args) => command::schedule::run(&ctx, args).await,
        Commands::Media(args) => command::media::run(&ctx, args).await,
        Commands::Convert {
            input,
            from,
            to,
            options,
            output_file,
        } => {
            command::convert::run(
                &ctx,
                input,
                from,
                to,
                options.as_deref(),
                output_file.as_ref(),
            )
            .await
        }
        Commands::Serve { addr, worker } => {
            command::serve::run(&ctx, addr.as_deref(), *worker).await
        }
        Commands::Engine(args) => command::engine::run(&ctx, args).await,
        Commands::Doc(args) => command::doc::run(&ctx, args).await,
        Commands::Pdf(args) => command::pdf::run(&ctx, args).await,
        Commands::Image(args) => command::image::run(&ctx, args).await,
        Commands::Pipeline(args) => command::pipeline::run(&ctx, args).await,
        Commands::Mcp(args) => command::mcp::run(&ctx, args).await,
        Commands::Video(args) => command::video::run(&ctx, args),
        Commands::Mcp(args) => command::mcp::run(&ctx, args).await,
    }
}

/// Diagnostics go to stderr; in JSON mode the error object follows the
/// stable shape from spec §5.3.
fn report_error(e: &CliError, json: bool) {
    if json {
        let body = serde_json::json!({
            "error": {
                "code": e.code(),
                "message": e.to_string(),
                "request_id": null,
                "retryable": e.retryable(),
            }
        });
        eprintln!("{}", serde_json::to_string(&body).unwrap_or_default());
    } else {
        eprintln!("error: {e}");
    }
}

fn init_logging(verbose: bool, quiet: bool) {
    use tracing_subscriber::EnvFilter;

    let level = if verbose {
        "debug"
    } else if quiet {
        "error"
    } else {
        "warn"
    };
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(level))
        .unwrap_or_else(|_| EnvFilter::new("warn"));
    // try_init: the library may already have a subscriber in tests.
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}
