mod document;
mod refs;
mod server;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn main() -> Result<()> {
    let (connection, threads) = lsp_server::Connection::stdio();
    server::run(connection)?;
    threads.join()?;
    Ok(())
}
