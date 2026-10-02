//! Latest event per pod, shown on each pod card.

use std::collections::HashMap;

use k8s_openapi::api::core::v1::Event;
use k8s_openapi::jiff::Timestamp;
use kube::Client;
use kube::api::{Api, ListParams};

use super::error::K8sError;

/// Page size for the event list.
const EVENT_PAGE_SIZE: u32 = 500;

/// Pages read per poll before giving up on the rest. Events expire after
/// an hour by default, so this only bounds a namespace in an event storm.
const MAX_EVENT_PAGES: usize = 10;

fn event_list_params(continue_token: Option<&str>) -> ListParams {
    let params = ListParams::default()
        .fields("involvedObject.kind=Pod")
        .limit(EVENT_PAGE_SIZE)
        .timeout(15);
    match continue_token {
        Some(token) => params.continue_token(token),
        None => params,
    }
}

/// Fetch recent events for pods in `namespace`, keyed by pod name, each
/// formatted as `"{type}: {reason}"`.
///
/// The list comes back in name order, not time order, so every page is
/// read (up to [`MAX_EVENT_PAGES`]) before picking the newest per pod.
pub(super) async fn fetch_pod_events(
    client: &Client,
    namespace: &str,
) -> Result<HashMap<String, String>, K8sError> {
    let api: Api<Event> = Api::namespaced(client.clone(), namespace);
    let mut events = Vec::new();
    let mut continue_token: Option<String> = None;

    for _ in 0..MAX_EVENT_PAGES {
        let page = api
            .list(&event_list_params(continue_token.as_deref()))
            .await
            .map_err(|source| K8sError::ListEvents {
                namespace: namespace.to_string(),
                source: Box::new(source),
            })?;
        events.extend(page.items);

        continue_token = page.metadata.continue_.filter(|token| !token.is_empty());
        if continue_token.is_none() {
            return Ok(latest_event_per_pod(&events));
        }
    }

    tracing::warn!(
        "namespace {namespace} has more than {} pod events; using the first {}",
        EVENT_PAGE_SIZE as usize * MAX_EVENT_PAGES,
        events.len()
    );
    Ok(latest_event_per_pod(&events))
}

/// When an event last happened. Core/v1 events from newer components
/// leave `lastTimestamp` empty and fill `eventTime` or `series` instead,
/// so all of them are considered, newest first.
fn observed_at(event: &Event) -> Option<Timestamp> {
    let series = event
        .series
        .as_ref()
        .and_then(|s| s.last_observed_time.as_ref())
        .map(|t| t.0);
    let candidates = [
        series,
        event.last_timestamp.as_ref().map(|t| t.0),
        event.event_time.as_ref().map(|t| t.0),
        event.first_timestamp.as_ref().map(|t| t.0),
        event.metadata.creation_timestamp.as_ref().map(|t| t.0),
    ];
    candidates.into_iter().flatten().max()
}

/// Pick the newest event for each pod. An event with no timestamp at all
/// only wins when the pod has nothing better.
fn latest_event_per_pod(events: &[Event]) -> HashMap<String, String> {
    let mut latest: HashMap<&str, (&Event, Option<Timestamp>)> = HashMap::new();

    for event in events {
        if event.involved_object.kind.as_deref() != Some("Pod") {
            continue;
        }
        let Some(pod_name) = event.involved_object.name.as_deref() else {
            continue;
        };

        let at = observed_at(event);
        let is_newer = latest
            .get(pod_name)
            .is_none_or(|&(_, prev_at)| at >= prev_at);
        if is_newer {
            latest.insert(pod_name, (event, at));
        }
    }

    latest
        .into_iter()
        .map(|(pod, (event, _))| (pod.to_string(), describe(event)))
        .collect()
}

fn describe(event: &Event) -> String {
    let event_type = event.type_.as_deref().unwrap_or("Normal");
    let reason = event.reason.as_deref().unwrap_or("Unknown");
    format!("{event_type}: {reason}")
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "panicking on failure is standard in tests"
)]
mod tests {
    use super::*;

    use k8s_openapi::api::core::v1::{EventSeries, ObjectReference};
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{MicroTime, Time};

    fn ts(second: i64) -> Timestamp {
        Timestamp::from_second(second).expect("valid timestamp")
    }

    fn pod_event(pod: &str, reason: &str) -> Event {
        Event {
            involved_object: ObjectReference {
                kind: Some("Pod".to_string()),
                name: Some(pod.to_string()),
                ..ObjectReference::default()
            },
            type_: Some("Warning".to_string()),
            reason: Some(reason.to_string()),
            ..Event::default()
        }
    }

    fn at_last_timestamp(mut event: Event, second: i64) -> Event {
        event.last_timestamp = Some(Time(ts(second)));
        event
    }

    #[test]
    fn list_params_filter_to_pods_and_page() {
        let first = event_list_params(None);
        assert_eq!(
            first.field_selector.as_deref(),
            Some("involvedObject.kind=Pod")
        );
        assert_eq!(first.limit, Some(EVENT_PAGE_SIZE));
        assert_eq!(first.continue_token, None);

        let next = event_list_params(Some("abc"));
        assert_eq!(next.continue_token.as_deref(), Some("abc"));
    }

    #[test]
    fn newest_event_wins_regardless_of_list_order() {
        let events = [
            at_last_timestamp(pod_event("web", "BackOff"), 300),
            at_last_timestamp(pod_event("web", "Pulled"), 100),
        ];

        let latest = latest_event_per_pod(&events);

        assert_eq!(latest["web"], "Warning: BackOff");
    }

    #[test]
    fn event_time_and_series_count_when_last_timestamp_is_empty() {
        let mut series = pod_event("web", "Unhealthy");
        series.series = Some(EventSeries {
            last_observed_time: Some(MicroTime(ts(500))),
            ..EventSeries::default()
        });
        let mut event_time = pod_event("web", "Killing");
        event_time.event_time = Some(MicroTime(ts(400)));
        let older = at_last_timestamp(pod_event("web", "Pulled"), 100);

        let latest = latest_event_per_pod(&[older, series, event_time]);

        assert_eq!(latest["web"], "Warning: Unhealthy");
    }

    #[test]
    fn untimed_event_does_not_displace_a_timed_one() {
        let events = [
            at_last_timestamp(pod_event("web", "BackOff"), 300),
            pod_event("web", "Mystery"),
        ];

        assert_eq!(latest_event_per_pod(&events)["web"], "Warning: BackOff");
    }

    #[test]
    fn non_pod_and_unnamed_events_are_skipped() {
        let mut node = pod_event("node-1", "NodeNotReady");
        node.involved_object.kind = Some("Node".to_string());
        let mut unnamed = pod_event("x", "Odd");
        unnamed.involved_object.name = None;

        assert!(latest_event_per_pod(&[node, unnamed]).is_empty());
    }
}
