use anyhow::Result;
use native_tls::TlsConnector;
use postgres_native_tls::MakeTlsConnector;
use tokio_postgres::Client;

pub fn make_postgres_tls_connector() -> Result<MakeTlsConnector> {
    let connector = TlsConnector::builder().build()?;
    Ok(MakeTlsConnector::new(connector))
}

pub async fn connect_postgres_client(
    database_url: &str,
    background_error_message: &'static str,
) -> Result<Client> {
    let connector = make_postgres_tls_connector()?;
    let (client, connection) = tokio_postgres::connect(database_url, connector).await?;

    tokio::spawn(async move {
        if let Err(err) = connection.await {
            tracing::error!(error = ?err, "{background_error_message}");
        }
    });

    Ok(client)
}
