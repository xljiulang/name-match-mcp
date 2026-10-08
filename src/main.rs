//! stdio entry point for the `name-match-mcp` server.

use rmcp::{ServiceExt, transport::stdio};

use name_match_mcp::server::NameMatchServer;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("name-match-mcp failed: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let service = NameMatchServer::new().serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
