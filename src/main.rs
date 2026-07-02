#[tokio::main]
async fn main() -> anyhow::Result<()> {
    openai_codex_proxy::run().await
}
