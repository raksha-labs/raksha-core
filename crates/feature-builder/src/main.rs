use anyhow::Result;
use feature_builder::StaticReferencePriceProvider;

#[tokio::main]
async fn main() -> Result<()> {
    common::init_logging("info");

    let _provider = StaticReferencePriceProvider;
    tracing::info!("feature-builder started");
    tracing::info!("TODO: subscribe to normalized events and publish feature events");

    Ok(())
}
