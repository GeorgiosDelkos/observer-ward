//! Latest event per pod, shown on each pod card.

use std::collections::HashMap;
use std::future::Future;
use std::time::Duration;

use k8s_openapi::api::core::v1::Event;
use k8s_openapi::jiff::Timestamp;
use kube::Client;
use kube::api::{Api, ListParams};
use tokio::time::Instant;

use super::error::K8sError;

/// Page size for the event list.
const EVENT_PAGE_SIZE: u32 = 500;

/// Pages read per poll before giving up on the rest. Events expire after
/// an hour by default, so this only bounds a namespace in an event storm.
const MAX_EVENT_PAGES: usize = 10;

/// Time allowed for all pages together. Events are optional decoration;
/// a slow listing must not push the whole cluster poll past its timeout
/// and mark the cluster offline.
const EVENT_BUDGET: Duration = Duration::from_secs(10);

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

/// One page of the event list and the token for the next, if any.
struct EventPage {
    items: Vec<Event>,
    next: Option<String>,
}

/// Fetch recent events for pods in `namespace`, keyed by pod name, each
/// formatted as `"{type}: {reason}"`.
///
/// The list comes back in name order, not time order, so every page is
/// read (within [`MAX_EVENT_PAGES`] and [`EVENT_BUDGET`]) before the
/// newest per pod is known.
pub(super) async fn fetch_pod_events(
    client: &Client,
    namespace: &str,
) -> Result<HashMap<String, String>, K8sError> {
    let api: Api<Event> = Api::namespaced(client.clone(), namespace);
    let fetch = |token: Option<String>| {
        let api = api.clone();
        async move {
            let page = api
                .list(&event_list_params(token.as_deref()))
                .await
                .map_err(|source| K8sError::ListEvents {
                    namespace: namespace.to_string(),
                    source: Box::new(source),
                })?;
            Ok(EventPage {
                items: page.items,
                next: page.metadata.continue_,
            })
        }
    };

    let latest = collect_pages(fetch, Instant::now() + EVENT_BUDGET, namespace).await?;
    Ok(latest.into_map())
}

/// Read pages through `fetch` until there is no continue token, folding
/// each into the newest-per-pod map as it arrives. Stops early, keeping
/// what it has, at [`MAX_EVENT_PAGES`] or at `deadline`.
async fn collect_pages<F, Fut>(
    mut fetch: F,
    deadline: Instant,
    namespace: &str,
) -> Result<LatestEvents, K8sError>
where
    F: FnMut(Option<String>) -> Fut,
    Fut: Future<Output = Result<EventPage, K8sError>>,
{
    let mut latest = LatestEvents::default();
    let mut token = None;

    for _ in 0..MAX_EVENT_PAGES {
        let Ok(page) = tokio::time::timeout_at(deadline, fetch(token)).await else {
            tracing::warn!("listing pod events in {namespace} ran out of time; showing partial");
            return Ok(latest);
        };
        let page = page?;
        latest.add(&page.items);

        token = page.next.filter(|t| !t.is_empty());
        if token.is_none() {
            return Ok(latest);
        }
    }

    tracing::warn!(
        "namespace {namespace} has more than {} pod events; showing the newest of those read",
        EVENT_PAGE_SIZE as usize * MAX_EVENT_PAGES
    );
    Ok(latest)
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

/// The newest event seen so far for each pod, as its description and
/// timestamp.
#[derive(Default)]
struct LatestEvents(HashMap<String, (String, Option<Timestamp>)>);

impl LatestEvents {
    /// Fold in a batch of events. An event with no timestamp at all only
    /// wins when the pod has nothing better.
    fn add(&mut self, events: &[Event]) {
        for event in events {
            if event.involved_object.kind.as_deref() != Some("Pod") {
                continue;
            }
            let Some(pod_name) = event.involved_object.name.as_deref() else {
                continue;
            };

            let at = observed_at(event);
            let is_newer = self
                .0
                .get(pod_name)
                .is_none_or(|&(_, prev_at)| at >= prev_at);
            if is_newer {
                self.0.insert(pod_name.to_string(), (describe(event), at));
            }
        }
    }

    fn into_map(self) -> HashMap<String, String> {
        self.0
            .into_iter()
            .map(|(pod, (description, _))| (pod, description))
            .collect()
    }
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

    fn latest_event_per_pod(events: &[Event]) -> HashMap<String, String> {
        let mut latest = LatestEvents::default();
        latest.add(events);
        latest.into_map()
    }

    /// Serves `pages` in order, recording the continue token of each call.
    fn paged<'a>(
        pages: Vec<(Vec<Event>, Option<&'static str>)>,
        tokens: &'a std::sync::Mutex<Vec<Option<String>>>,
    ) -> impl FnMut(Option<String>) -> std::future::Ready<Result<EventPage, K8sError>> + 'a {
        let mut pages = pages.into_iter();
        move |token| {
            tokens.lock().expect("lock").push(token);
            let (items, next) = pages.next().expect("no request past the last page");
            std::future::ready(Ok(EventPage {
                items,
                next: next.map(str::to_string),
            }))
        }
    }

    fn far_deadline() -> Instant {
        Instant::now() + Duration::from_secs(60)
    }

    #[tokio::test]
    async fn pages_are_followed_until_the_token_runs_out() {
        let tokens = std::sync::Mutex::new(Vec::new());
        let fetch = paged(
            vec![
                (
                    vec![at_last_timestamp(pod_event("web", "Old"), 100)],
                    Some("p2"),
                ),
                (
                    vec![at_last_timestamp(pod_event("web", "New"), 200)],
                    Some(""),
                ),
            ],
            &tokens,
        );

        let latest = collect_pages(fetch, far_deadline(), "ns")
            .await
            .expect("pages");

        assert_eq!(latest.into_map()["web"], "Warning: New");
        assert_eq!(
            *tokens.lock().expect("lock"),
            [None, Some("p2".to_string())]
        );
    }

    #[tokio::test]
    async fn paging_stops_at_the_page_cap() {
        let tokens = std::sync::Mutex::new(Vec::new());
        let pages = (0..MAX_EVENT_PAGES + 5)
            .map(|_| (vec![pod_event("web", "Spam")], Some("more")))
            .collect();

        let latest = collect_pages(paged(pages, &tokens), far_deadline(), "ns")
            .await
            .expect("pages");

        assert_eq!(tokens.lock().expect("lock").len(), MAX_EVENT_PAGES);
        assert_eq!(latest.into_map().len(), 1);
    }

    #[tokio::test]
    async fn a_slow_page_keeps_what_was_read() {
        let mut calls = 0;
        let fetch = move |_token: Option<String>| {
            calls += 1;
            let first = calls == 1;
            async move {
                if !first {
                    std::future::pending::<()>().await;
                }
                Ok(EventPage {
                    items: vec![pod_event("web", "Seen")],
                    next: Some("more".to_string()),
                })
            }
        };

        let deadline = Instant::now() + Duration::from_millis(50);
        let latest = collect_pages(fetch, deadline, "ns").await.expect("partial");

        assert_eq!(latest.into_map()["web"], "Warning: Seen");
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
