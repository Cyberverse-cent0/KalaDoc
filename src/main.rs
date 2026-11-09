use kaladoc::crates::application::cli_mcp_server::cli;

#[tokio::main]
async fn main() {
    std::process::exit(cli::run().await);
}
