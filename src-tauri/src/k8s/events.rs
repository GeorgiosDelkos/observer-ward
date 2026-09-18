//! Pod event listing (best-effort last-event bound).

use std::collections::HashMap;

use k8s_openapi::api::core::v1::Event;
use kube::Client;
use kube::api::{Api, ListParams};

use super::error::K8sError;

const EVENT_LIST_LIMIT: u32 = 400;

pub(super) fn event_list_params() -> ListParams {
    ListParams::default()
        .fields("involvedObject.kind=Pod")
        .limit(EVENT_LIST_LIMIT)
        .timeout(15)
}

/// Fetch recent K8s events for pods in a namespace.
/// Returns a map from pod name to the most recent event
/// formatted as `"{type}: {reason}"`.
pub(super) async fn fetch_pod_events(
    client: &Client,
    namespace: &str,
) -> Result<HashMap<String, String>, K8sError> {
    let events_api: Api<Event> = Api::namespaced(client.clone(), namespace);
    let events = events_api
        .list(&event_list_params())
        .await
        .map_err(|source| K8sError::ListEvents {
            namespace: namespace.to_string(),
            source: Box::new(source),
        })?;

    let mut latest: HashMap<
        String,
        (
            String,
            Option<k8s_openapi::apimachinery::pkg::apis::meta::v1::Time>,
        ),
    > = HashMap::new();

    for event in &events {
        let is_pod = event.involved_object.kind.as_deref() == Some("Pod");
        if !is_pod {
            continue;
        }

        let Some(pod_name) = &event.involved_object.name else {
            continue;
        };

        let event_type = event.type_.as_deref().unwrap_or("Normal");
        let reason = event.reason.as_deref().unwrap_or("Unknown");
        let formatted = format!("{event_type}: {reason}");

        let is_newer = match latest.get(pod_name.as_str()) {
            None => true,
            Some((_, prev_ts)) => match (&event.last_timestamp, prev_ts) {
                (Some(new), Some(old)) => new.0 >= old.0,
                (Some(_), None) => true,
                (None, Some(_) | None) => false,
            },
        };

        if is_newer {
            latest.insert(pod_name.clone(), (formatted, event.last_timestamp.clone()));
        }
    }

    Ok(latest.into_iter().map(|(k, (v, _))| (k, v)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_list_params_are_bounded() {
        let params = event_list_params();
        assert_eq!(
            params.field_selector.as_deref(),
            Some("involvedObject.kind=Pod")
        );
        assert_eq!(params.limit, Some(EVENT_LIST_LIMIT));
    }
}
