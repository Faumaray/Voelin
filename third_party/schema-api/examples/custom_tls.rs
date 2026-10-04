use schema_api::{
    tonic::transport::{Certificate, ClientTlsConfig, Identity},
    ApiClient,
};
use std::{env, error::Error, fs};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let mut tls = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(fs::read(env::var("API_CA_PEM")?)?));
    if let Ok(domain) = env::var("API_TLS_DOMAIN") {
        tls = tls.domain_name(domain);
    }
    if let Ok(cert_path) = env::var("API_CLIENT_CERT_PEM") {
        tls = tls.identity(Identity::from_pem(
            fs::read(cert_path)?,
            fs::read(env::var("API_CLIENT_KEY_PEM")?)?,
        ));
    }
    let _client = ApiClient::builder(env::var("API_ENDPOINT")?)?
        .tls_config(tls)?
        .connect()
        .await?;
    println!("Connected with custom TLS configuration");
    Ok(())
}
