use chrono::SecondsFormat;
use chrono::Utc;
use data_encoding::BASE64;
use reqwest::Client;

use reqwest::header::HeaderMap;
use reqwest::header::HeaderValue;
use reqwest::header::CONTENT_TYPE;
use ring::hmac;
use serde_derive::Deserialize;
use serde_derive::Serialize;

use self::error::KolliderClientError;
use self::primitives::UserBalances;

mod error;
mod primitives;

#[derive(Default, Debug, Clone, Serialize, Deserialize)]
pub struct KolliderClientConfig {
    pub url: String,
    pub api_key: String,
    pub passphrase: String,
    pub secret: String,
}

#[derive(Clone)]
pub struct KolliderClient {
    client: Client,
    config: KolliderClientConfig,
}

impl KolliderClient {
    pub fn new(cfg: KolliderClientConfig) -> KolliderClient {
        KolliderClient {
            client: reqwest::Client::new(),
            config: cfg,
        }
    }

    fn create_headers(
        &self,
        timestamp: &str,
        signature: &str,
    ) -> Result<HeaderMap, KolliderClientError> {
        let mut header = HeaderMap::new();
        header.append(CONTENT_TYPE, HeaderValue::from_str("application/json")?);
        header.append("k-signature", HeaderValue::from_str(signature)?);

        header.append("k-timestamp", HeaderValue::from_str(timestamp)?);
        header.append(
            "k-passphrase",
            HeaderValue::from_str(&self.config.passphrase)?,
        );
        header.append("k-api-key", HeaderValue::from_str(&self.config.api_key)?);
        Ok(header)
    }

    fn generate_signature(secretb64: &str, pre_hash: &str) -> Result<String, KolliderClientError> {
        let res = BASE64.decode(secretb64.as_bytes())?;
        let key = hmac::Key::new(hmac::HMAC_SHA256, &res);
        let signature = hmac::sign(&key, pre_hash.as_bytes());
        let sig_encoded = BASE64.encode(signature.as_ref());
        Ok(sig_encoded)
    }

    fn create_get_headers(&self, path: &str) -> Result<HeaderMap, KolliderClientError> {
        let timestamp = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        let pre_hash = format!("{}{}{}", timestamp, "GET", path);
        let sig = Self::generate_signature(&self.config.secret, &pre_hash)?;
        Self::create_headers(self, &timestamp, &sig)
    }

    pub async fn get_user_balances(&self) -> Result<UserBalances, KolliderClientError> {
        let path = "/user/balances";
        let res = self
            .client
            .get(format!("{}{}", &self.config.url, path))
            .headers(Self::create_get_headers(self, path)?)
            .send()
            .await?;
        Ok(res.json::<UserBalances>().await?)
    }

    pub async fn get_products(&self) -> Result<String, KolliderClientError> {
        let path = "/market/products";
        let res = self
            .client
            .get(format!("{}{}", &self.config.url, path))
            .send()
            .await?;

        Ok(res.text().await?)
    }
}