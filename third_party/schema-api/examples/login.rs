use schema_api::{api, ApiClient, SessionRequest, SessionToken};
use std::{env, error::Error, time::Duration};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let client = ApiClient::builder(env::var("API_ENDPOINT")?)?
        .connect_timeout(Duration::from_secs(10))
        .request_timeout(Duration::from_secs(30))
        .connect()
        .await?;
    // Credential encoding is server-defined; this example passes the supplied values verbatim.
    let reply = client
        .login()
        .login(api::LoginData {
            email: env::var("API_EMAIL")?,
            password: env::var("API_PASSWORD")?,
            otp: env::var("API_OTP").unwrap_or_default(),
            device_id: env::var("API_DEVICE_ID").unwrap_or_default(),
            device_name: "Rust API client".into(),
            ..Default::default()
        })
        .await?
        .into_inner();
    let session = SessionToken::try_from(&reply)?;
    println!("Login succeeded");
    let status = client
        .login()
        .session(api::Session::default().with_session(&session))
        .await?
        .into_inner();
    schema_api::session::validate_session_status(&status)?;
    println!("Session is valid");
    Ok(())
}
