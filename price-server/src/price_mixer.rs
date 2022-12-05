use async_trait::async_trait;
use rust_decimal::Decimal;
use thiserror::Error;
use tracing::Span;

use crate::currency::VolumePicker;
use shared::time::*;
use std::{collections::HashMap, vec};

use super::currency::*;

pub trait SidePicker {
    fn buy_usd<'a>(&'a self) -> Box<dyn VolumePicker + 'a>;
    fn sell_usd<'a>(&'a self) -> Box<dyn VolumePicker + 'a>;
    fn mid_price_of_one_sat(&self) -> UsdCents;
}

#[async_trait]
pub trait PriceProvider {
    async fn latest(&self) -> Result<Box<dyn SidePicker>, ExchangePriceCacheError>;
}

pub struct PriceMixer {
    providers: HashMap<String, (Box<dyn PriceProvider + Sync + Send>, Decimal)>,
}

impl PriceMixer {
    pub fn new(
        providers: HashMap<String, (Box<dyn PriceProvider + Sync + Send>, Decimal)>,
    ) -> Self {
        Self { providers }
    }

    pub async fn apply(
        &self,
        f: impl Fn(&Box<dyn SidePicker>) -> Decimal,
    ) -> Result<Decimal, ExchangePriceCacheError> {
        let mut total = Decimal::ZERO;
        let mut total_weights = Decimal::ZERO;
        let mut errors = vec![];
        for (provider, weight) in self.providers.values() {
            let side_picker = match provider.latest().await {
                Ok(side_picker) => side_picker,
                Err(err) => {
                    log_error(&err);
                    errors.push(err);
                    continue;
                }
            };
            total_weights += weight;
            total += f(&side_picker) * weight;
        }

        if errors.len() == self.providers.values().len() {
            Err(errors.pop().expect("Could not find error"))
        } else {
            Ok(total / total_weights)
        }
    }
}

fn log_error(error: &ExchangePriceCacheError) {
    Span::current().record("error", &tracing::field::display("true"));
    Span::current().record("error.message", &tracing::field::display(error));
    Span::current().record(
        "error.level",
        &tracing::field::display(tracing::Level::WARN),
    );
}

#[derive(Error, Debug)]
pub enum ExchangePriceCacheError {
    #[error("StalePrice: last update was at {0}")]
    StalePrice(TimeStamp),
    #[error("No price data available")]
    NoPriceAvailable,
}

#[cfg(test)]
mod tests {
    pub use std::collections::HashMap;

    pub use chrono::Duration;
    pub use rust_decimal::Decimal;
    use shared::payload::PriceMessagePayload;
    use shared::pubsub::CorrelationId;
    use shared::time::TimeStamp;

    pub use super::PriceMixer;
    pub use super::PriceProvider;
    pub use crate::currency::UsdCents;
    pub use crate::{
        currency::{Sats, VolumePicker},
        exchange_tick_cache::ExchangeTickCache,
    };
    pub use serde_json::*;

    #[tokio::test]
    async fn test_price_mixer() -> anyhow::Result<(), Error> {
        let mut providers: HashMap<String, (Box<dyn PriceProvider + Sync + Send>, Decimal)> =
            HashMap::new();
        let cache = Box::new(ExchangeTickCache::new(Duration::seconds(3000)));
        providers.insert("okex".to_string(), (cache.clone(), Decimal::from(1)));
        let price_mixer = PriceMixer::new(providers);

        cache
            .apply_update(get_payload(), CorrelationId::new())
            .await;

        let price = price_mixer
            .apply(|p| {
                *p.sell_usd()
                    .sats_from_cents(UsdCents::from_decimal(Decimal::ONE))
                    .amount()
            })
            .await
            .expect("Price should be available");
        assert_ne!(Decimal::ZERO, price);
        Ok(())
    }

    fn get_payload() -> PriceMessagePayload {
        let raw = r#"{
            "exchange": "okex",
            "instrumentId": "BTC-USD-SWAP",
            "timestamp": 1,
            "bidPrice": {
                "numeratorUnit": "USD_CENT",
                "denominatorUnit": "BTC_SAT",
                "offset": 12,
                "base": "1000000000"
            },
            "askPrice": {
                "numeratorUnit": "USD_CENT",
                "denominatorUnit": "BTC_SAT",
                "offset": 12,
                "base": "10000000000"
            }
            }"#;
        let mut price_message_payload =
            serde_json::from_str::<PriceMessagePayload>(raw).expect("Could not parse payload");
        price_message_payload.timestamp = TimeStamp::now();
        price_message_payload
    }
}
