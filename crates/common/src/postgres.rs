use std::{env, fs, path::Path};

use anyhow::{Context, Result};
use native_tls::{Certificate, TlsConnector};
use postgres_native_tls::MakeTlsConnector;
use tokio_postgres::Client;
use tracing::{info, warn};

const DEFAULT_RDS_CA_BUNDLE_PATH: &str = "/app/certs/rds-global-bundle.pem";

pub fn make_postgres_tls_connector() -> Result<MakeTlsConnector> {
    let mut builder = TlsConnector::builder();

    if let Some(bundle_path) = postgres_root_cert_bundle_path() {
        let certificates = load_pem_certificates(&bundle_path)?;
        info!(
            bundle_path = %bundle_path,
            certificate_count = certificates.len(),
            "loading postgres root certificates"
        );
        for certificate in certificates {
            builder.add_root_certificate(certificate);
        }
    } else {
        warn!("postgres root cert bundle not configured; relying on system trust store");
    }

    let connector = builder.build()?;
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
            crate::log_error!(error, err, background_error_message);
        }
    });

    Ok(client)
}

fn postgres_root_cert_bundle_path() -> Option<String> {
    env::var("POSTGRES_SSL_ROOT_CERT")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            Path::new(DEFAULT_RDS_CA_BUNDLE_PATH)
                .exists()
                .then(|| DEFAULT_RDS_CA_BUNDLE_PATH.to_string())
        })
}

fn load_pem_certificates(bundle_path: &str) -> Result<Vec<Certificate>> {
    let pem_bytes = fs::read(bundle_path)
        .with_context(|| format!("failed to read postgres root cert bundle at {bundle_path}"))?;
    let pem_text = String::from_utf8(pem_bytes).with_context(|| {
        format!("postgres root cert bundle at {bundle_path} is not valid UTF-8")
    })?;

    let mut certificates = Vec::new();
    for pem in split_pem_certificates(&pem_text) {
        let certificate = Certificate::from_pem(pem.as_bytes()).with_context(|| {
            format!("failed to parse postgres root certificate from {bundle_path}")
        })?;
        certificates.push(certificate);
    }

    if certificates.is_empty() {
        return Err(anyhow::anyhow!(
            "postgres root cert bundle at {bundle_path} did not contain any PEM certificates"
        ));
    }

    Ok(certificates)
}

fn split_pem_certificates(pem_bundle: &str) -> Vec<String> {
    let end_marker = "-----END CERTIFICATE-----";
    pem_bundle
        .split(end_marker)
        .filter_map(|chunk| {
            let trimmed = chunk.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(format!("{trimmed}\n{end_marker}\n"))
            }
        })
        .collect()
}
