use crate::{
    event::{metric::MetricValue, Event, Metric, MetricKind},
    Result,
};
use bytes::Bytes;
use chrono::{TimeZone, Utc};
use prost::Message;
use std::collections::BTreeMap;
use std::sync::Arc;
use vector_core::config::log_schema;
use vector_core::metrics::AgentDDSketch;

mod dd_proto {
    include!(concat!(env!("OUT_DIR"), "/datadog.agentpayload.rs"));
}

use dd_proto::SketchPayload;

pub(crate) fn decode_ddsketch(frame: Bytes, api_key: Option<Arc<str>>) -> Result<Vec<Event>> {
    let payload = SketchPayload::decode(frame)?;
    // payload.metadata is always empty for payload coming from dd agents
    Ok(payload
        .sketches
        .iter()
        .flat_map(|sketch_series| {
            // sketch_series.distributions is also always empty from payload coming from dd agents
            let mut tags = BTreeMap::<String, String>::new();
            for tag in &sketch_series.tags {
                let kv = tag.split_once(":").unwrap_or((tag, ""));
                tags.insert(kv.0.trim().into(), kv.1.trim().into());
            }
            tags.insert(
                log_schema().host_key().to_string(),
                sketch_series.host.clone(),
            );
            sketch_series
                .dogsketches
                .iter()
                .map(|sketch| {
                    let k: Vec<i16> = sketch.k.iter().map(|k| *k as i16).collect();
                    let n: Vec<u16> = sketch.n.iter().map(|n| *n as u16).collect();
                    let val = MetricValue::from(
                        AgentDDSketch::from_raw(
                            sketch.cnt as u32,
                            sketch.min,
                            sketch.max,
                            sketch.sum,
                            sketch.avg,
                            &k,
                            &n,
                        )
                        .unwrap_or_else(AgentDDSketch::with_agent_defaults),
                    );
                    let mut metric =
                        Metric::new(sketch_series.metric.clone(), MetricKind::Absolute, val)
                            .with_tags(Some(tags.clone()))
                            .with_timestamp(Some(Utc.timestamp(sketch.ts, 0)));
                    if let Some(k) = &api_key {
                        metric
                            .metadata_mut()
                            .set_datadog_api_key(Some(Arc::clone(k)));
                    }
                    metric.into()
                })
                .collect::<Vec<Event>>()
        })
        .collect())
}