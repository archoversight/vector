use crate::{
    config::{DataType, GlobalOptions, SourceConfig, SourceDescription},
    metrics::Controller,
    metrics::{capture_metrics, get_controller},
    shutdown::ShutdownSignal,
    Pipeline,
};
use futures::{stream, SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::time;

#[derive(Deserialize, Serialize, Debug, Clone, Derivative)]
#[derivative(Default)]
#[serde(deny_unknown_fields, default)]
pub struct InternalMetricsConfig {
    #[derivative(Default(value = "2.0"))]
    scrape_interval_secs: f64,
}

inventory::submit! {
    SourceDescription::new::<InternalMetricsConfig>("internal_metrics")
}

impl_generate_config_from_default!(InternalMetricsConfig);

#[async_trait::async_trait]
#[typetag::serde(name = "internal_metrics")]
impl SourceConfig for InternalMetricsConfig {
    async fn build(
        &self,
        _name: &str,
        _globals: &GlobalOptions,
        shutdown: ShutdownSignal,
        out: Pipeline,
    ) -> crate::Result<super::Source> {
        let interval = time::Duration::from_secs_f64(self.scrape_interval_secs);
        if interval < time::Duration::from_millis(500) {
            return Err(format!(
                "interval set too low ({} secs), use interval >= 0.5 secs",
                interval.as_secs_f64()
            )
            .into());
        }

        Ok(Box::pin(run(get_controller()?, interval, out, shutdown)))
    }

    fn output_type(&self) -> DataType {
        DataType::Metric
    }

    fn source_type(&self) -> &'static str {
        "internal_metrics"
    }
}

async fn run(
    controller: &Controller,
    interval: time::Duration,
    out: Pipeline,
    shutdown: ShutdownSignal,
) -> Result<(), ()> {
    let mut out =
        out.sink_map_err(|error| error!(message = "Error sending internal metrics.", %error));

    let mut interval = time::interval(interval).take_until(shutdown);
    while interval.next().await.is_some() {
        let metrics = capture_metrics(controller);
        out.send_all(&mut stream::iter(metrics).map(Ok)).await?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::event::metric::{Metric, MetricValue, StatisticKind};
    use crate::metrics::{capture_metrics, get_controller};
    use metrics::{counter, gauge, histogram};
    use std::collections::BTreeMap;

    #[test]
    fn generate_config() {
        crate::test_util::test_generate_config::<super::InternalMetricsConfig>();
    }

    #[test]
    fn captures_internal_metrics() {
        let _ = crate::metrics::init();

        // There *seems* to be a race condition here (CI was flaky), so add a slight delay.
        std::thread::sleep(std::time::Duration::from_millis(300));

        gauge!("foo", 1.0);
        gauge!("foo", 2.0);
        counter!("bar", 3);
        counter!("bar", 4);
        histogram!("baz", 5);
        histogram!("baz", 6);
        histogram!("quux", 7, "host" => "foo");
        histogram!("quux", 8, "host" => "foo");

        let controller = get_controller().expect("no controller");

        // There *seems* to be a race condition here (CI was flaky), so add a slight delay.
        std::thread::sleep(std::time::Duration::from_millis(300));

        let output = capture_metrics(&controller)
            .map(|event| {
                let m = event.into_metric();
                (m.name.clone(), m)
            })
            .collect::<BTreeMap<String, Metric>>();

        assert_eq!(MetricValue::Gauge { value: 2.0 }, output["foo"].value);
        assert_eq!(MetricValue::Counter { value: 7.0 }, output["bar"].value);
        assert_eq!(
            MetricValue::Distribution {
                values: vec![5.0, 6.0],
                sample_rates: vec![1, 1],
                statistic: StatisticKind::Histogram
            },
            output["baz"].value
        );
        assert_eq!(
            MetricValue::Distribution {
                values: vec![7.0, 8.0],
                sample_rates: vec![1, 1],
                statistic: StatisticKind::Histogram
            },
            output["quux"].value
        );

        let mut labels = BTreeMap::new();
        labels.insert(String::from("host"), String::from("foo"));
        assert_eq!(Some(labels), output["quux"].tags);
    }
}
