use schema_api::{
    tonic::metadata::{MetadataKey, MetadataMap, MetadataValue},
    ApiClient,
};
use std::{env, error::Error};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // The schemas do not specify push authentication. Supply your server's metadata contract.
    let mut headers = MetadataMap::new();
    if let Ok(name) = env::var("API_PUSH_HEADER") {
        let key: MetadataKey<_> = name.parse()?;
        let mut value: MetadataValue<_> = env::var("API_PUSH_VALUE")?.parse()?;
        value.set_sensitive(true);
        headers.insert(key, value);
    }
    let client = ApiClient::builder(env::var("API_ENDPOINT")?)?
        .metadata(headers)
        .connect()
        .await?;
    let mut stream = client.push().long_pull(()).await?.into_inner();
    while let Some(message) = stream.message().await? {
        println!("Received {} opaque payload bytes", message.payload.len());
        // If your server uses this envelope, explicitly decode it with:
        // let notification = schema_api::wire::decode_push_notification(&message)?;
    }
    Ok(())
}
