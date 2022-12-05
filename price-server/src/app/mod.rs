mod error;

use std::collections::HashMap;

use chrono::Duration;
use futures::stream::StreamExt;
use rust_decimal::Decimal;
use tracing::{info_span, instrument, Instrument};

use shared::{
    exchanges_config::ExchangeConfigAll,
    health::HealthCheckTrigger,
    payload::{
        KolliderBtcUsdSwapPricePayload, OkexBtcUsdSwapPricePayload, KOLLIDER_EXCHANGE_ID,
        OKEX_EXCHANGE_ID,
    },
    pubsub::*,
};

pub use crate::{currency::*, fee_calculator::*};
use crate::{
    exchange_tick_cache::ExchangeTickCache,
    price_mixer::{PriceMixer, PriceProvider},
};
pub use error::*;

const EXCHANGE_CACHE_STALE_AFTER: i64 = 30;

pub struct PriceApp {
    price_mixer: PriceMixer,
    fee_calculator: FeeCalculator,
}

impl PriceApp {
    pub async fn run(
        health_check_trigger: HealthCheckTrigger,
        fee_calc_cfg: FeeCalculatorConfig,
        pubsub_cfg: PubSubConfig,
        exchanges_cfg: ExchangeConfigAll,
    ) -> Result<Self, PriceAppError> {
        let mut providers: HashMap<String, (Box<dyn PriceProvider + Sync + Send>, Decimal)> =
            HashMap::new();
        let mut subscribers = vec![];

        if let Some(config) = exchanges_cfg.kollider.as_ref() {
            let kollider_price_cache =
                ExchangeTickCache::new(Duration::seconds(EXCHANGE_CACHE_STALE_AFTER));
            Self::subscribe_kollider(
                pubsub_cfg.clone(),
                kollider_price_cache.clone(),
                &mut subscribers,
            )
            .await?;
            providers.insert(
                KOLLIDER_EXCHANGE_ID.to_string(),
                (Box::new(kollider_price_cache), config.weight),
            );
        }

        if let Some(config) = exchanges_cfg.okex.as_ref() {
            let okex_price_cache =
                ExchangeTickCache::new(Duration::seconds(EXCHANGE_CACHE_STALE_AFTER));
            Self::subscribe_okex(pubsub_cfg.clone(), okex_price_cache.clone()).await?;
            providers.insert(
                OKEX_EXCHANGE_ID.to_string(),
                (Box::new(okex_price_cache), config.weight),
            );
        }
        Self::check_health(health_check_trigger, subscribers);

        let fee_calculator = FeeCalculator::new(fee_calc_cfg);
        let app = Self {
            price_mixer: PriceMixer::new(providers),
            fee_calculator,
        };

        Ok(app)
    }

    fn check_health(mut health_check_trigger: HealthCheckTrigger, subscribers: Vec<Subscriber>) {
        tokio::spawn(async move {
            while let Some(check) = health_check_trigger.next().await {
                let mut received_errors = vec![];
                for subscriber in subscribers.iter() {
                    let result = subscriber.healthy(Duration::seconds(20)).await;
                    if let Err(err) = result {
                        received_errors.push(err);
                    }
                }
                if !received_errors.is_empty() && received_errors.len() == subscribers.len() {
                    check
                        .send(Err(received_errors
                            .get(0)
                            .expect("Couldn't find error")
                            .clone()))
                        .expect("Couldn't send response");
                } else {
                    check.send(Ok(())).expect("Couldn't send response");
                }
            }
        });
    }

    async fn subscribe_okex(
        pubsub_cfg: PubSubConfig,
        price_cache: ExchangeTickCache,
    ) -> Result<(), PriceAppError> {
        let subscriber = Subscriber::new(pubsub_cfg).await?;
        let mut stream = subscriber.subscribe::<OkexBtcUsdSwapPricePayload>().await?;
        let _ = tokio::spawn(async move {
            while let Some(msg) = stream.next().await {
                let span = info_span!(
                    "price_tick_received",
                    message_type = %msg.payload_type,
                    correlation_id = %msg.meta.correlation_id
                );
                shared::tracing::inject_tracing_data(&span, &msg.meta.tracing_data);

                async {
                    price_cache
                        .apply_update(msg.payload.0, msg.meta.correlation_id)
                        .await;
                }
                .instrument(span)
                .await;
            }
        });

        Ok(())
    }

    async fn subscribe_kollider(
        pubsub_cfg: PubSubConfig,
        price_cache: ExchangeTickCache,
        subscribers: &mut Vec<Subscriber>,
    ) -> Result<(), PriceAppError> {
        let subscriber = Subscriber::new(pubsub_cfg).await?;
        let mut stream = subscriber
            .subscribe::<KolliderBtcUsdSwapPricePayload>()
            .await?;
        subscribers.push(subscriber);

        let _ = tokio::spawn(async move {
            while let Some(msg) = stream.next().await {
                let span = info_span!(
                    "price_tick_received",
                    message_type = %msg.payload_type,
                    correlation_id = %msg.meta.correlation_id
                );
                shared::tracing::inject_tracing_data(&span, &msg.meta.tracing_data);

                async {
                    price_cache
                        .apply_update(msg.payload.0, msg.meta.correlation_id)
                        .await;
                }
                .instrument(span)
                .await;
            }
        });

        Ok(())
    }

    #[instrument(skip_all, fields(correlation_id, amount = %sats.amount()), ret, err)]
    pub async fn get_cents_from_sats_for_immediate_buy(
        &self,
        sats: Sats,
    ) -> Result<UsdCents, PriceAppError> {
        let cents = UsdCents::from_decimal(
            self.price_mixer
                .apply(|p| *p.buy_usd().cents_from_sats(sats.clone()).amount())
                .await?,
        );

        Ok(self.fee_calculator.decrease_by_immediate_fee(cents).floor())
    }

    #[instrument(skip_all, fields(correlation_id, amount = %sats.amount()), ret, err)]
    pub async fn get_cents_from_sats_for_immediate_sell(
        &self,
        sats: Sats,
    ) -> Result<UsdCents, PriceAppError> {
        let cents = UsdCents::from_decimal(
            self.price_mixer
                .apply(|p| *p.sell_usd().cents_from_sats(sats.clone()).amount())
                .await?,
        );
        Ok(self.fee_calculator.increase_by_immediate_fee(cents).ceil())
    }

    #[instrument(skip_all, fields(correlation_id, amount = %sats.amount()), ret, err)]
    pub async fn get_cents_from_sats_for_future_buy(
        &self,
        sats: Sats,
    ) -> Result<UsdCents, PriceAppError> {
        let cents = UsdCents::from_decimal(
            self.price_mixer
                .apply(|p| *p.buy_usd().cents_from_sats(sats.clone()).amount())
                .await?,
        );
        Ok(self.fee_calculator.decrease_by_delayed_fee(cents).floor())
    }

    #[instrument(skip_all, fields(correlation_id, amount = %sats.amount()), ret, err)]
    pub async fn get_cents_from_sats_for_future_sell(
        &self,
        sats: Sats,
    ) -> Result<UsdCents, PriceAppError> {
        let cents = UsdCents::from_decimal(
            self.price_mixer
                .apply(|p| *p.sell_usd().cents_from_sats(sats.clone()).amount())
                .await?,
        );
        Ok(self.fee_calculator.increase_by_delayed_fee(cents).ceil())
    }

    #[instrument(skip_all, fields(correlation_id, amount = %cents.amount()), ret, err)]
    pub async fn get_sats_from_cents_for_immediate_buy(
        &self,
        cents: UsdCents,
    ) -> Result<Sats, PriceAppError> {
        let sats = Sats::from_decimal(
            self.price_mixer
                .apply(|p| *p.buy_usd().sats_from_cents(cents.clone()).amount())
                .await?,
        );
        Ok(self.fee_calculator.increase_by_immediate_fee(sats).ceil())
    }

    #[instrument(skip_all, fields(correlation_id, amount = %cents.amount()), ret, err)]
    pub async fn get_sats_from_cents_for_immediate_sell(
        &self,
        cents: UsdCents,
    ) -> Result<Sats, PriceAppError> {
        let sats = Sats::from_decimal(
            self.price_mixer
                .apply(|p| *p.sell_usd().sats_from_cents(cents.clone()).amount())
                .await?,
        );

        Ok(self.fee_calculator.decrease_by_immediate_fee(sats).floor())
    }

    #[instrument(skip_all, fields(correlation_id, amount = %cents.amount()), ret, err)]
    pub async fn get_sats_from_cents_for_future_buy(
        &self,
        cents: UsdCents,
    ) -> Result<Sats, PriceAppError> {
        let sats = Sats::from_decimal(
            self.price_mixer
                .apply(|p| *p.buy_usd().sats_from_cents(cents.clone()).amount())
                .await?,
        );

        Ok(self.fee_calculator.increase_by_delayed_fee(sats).ceil())
    }

    #[instrument(skip_all, fields(correlation_id, amount = %cents.amount()), ret, err)]
    pub async fn get_sats_from_cents_for_future_sell(
        &self,
        cents: UsdCents,
    ) -> Result<Sats, PriceAppError> {
        let sats = Sats::from_decimal(
            self.price_mixer
                .apply(|p| *p.sell_usd().sats_from_cents(cents.clone()).amount())
                .await?,
        );
        Ok(self.fee_calculator.decrease_by_delayed_fee(sats).floor())
    }

    #[instrument(skip_all, fields(correlation_id), ret, err)]
    pub async fn get_cents_per_sat_exchange_mid_rate(&self) -> Result<f64, PriceAppError> {
        let cents_per_sat = self
            .price_mixer
            .apply(|p| *p.mid_price_of_one_sat().amount())
            .await?;
        Ok(f64::try_from(Sats::from_decimal(cents_per_sat))?)
    }
}
