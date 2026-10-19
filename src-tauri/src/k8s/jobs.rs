//! The cron job behind each Job, so a cron job's pods can be hidden as one
//! workload.
//!
//! A cron job makes a new Job, with a new name, for every run, and its pods
//! only reference that Job. The Job's own owner reference names the
//! cron job, so each Job is fetched once and the answer kept for as long
//! as it has pods (an owner reference does not change). Only final
//! answers are kept: a lookup that failed or ran out of time is retried
//! on the next poll.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::time::Duration;

use k8s_openapi::api::batch::v1::Job;
use kube::Client;
use kube::api::Api;
use tokio::task::JoinSet;
use tokio::time::Instant;

/// The `Job` kind, as it appears in a pod's controller reference.
pub(super) const JOB: &str = "Job";
pub(super) const CRON_JOB: &str = "CronJob";

/// Time allowed for one poll's Job lookups. They only refine how pods are
/// grouped, so a slow lookup must not push the whole cluster poll past its
/// timeout.
const JOB_LOOKUP_BUDGET: Duration = Duration::from_secs(5);

/// The name of the cron job owning each Job, `None` for a Job without
/// one. Keyed by Job name, within the target's namespace.
pub(super) type JobOwners = HashMap<String, Option<String>>;

/// What one poll's lookups found.
#[derive(Debug, Default, PartialEq)]
pub(super) struct Lookups {
    /// Final answers, to be kept.
    pub(super) owners: JobOwners,
    /// The API refused to let us read Jobs; asking again will not help.
    pub(super) forbidden: bool,
}

/// The outcome of looking up one Job.
#[derive(Debug, PartialEq)]
enum Lookup {
    /// The Job's cron job (`None`: it has none, or the Job is gone).
    Owner(Option<String>),
    Forbidden,
    /// A transient failure, already logged.
    Retry,
}

/// Forget Jobs that no longer have pods and return those in `wanted`
/// that are not known yet.
pub(super) fn jobs_to_fetch(owners: &mut JobOwners, wanted: &HashSet<&str>) -> Vec<String> {
    owners.retain(|name, _| wanted.contains(name.as_str()));
    wanted
        .iter()
        .filter(|name| !owners.contains_key(**name))
        .map(|name| (*name).to_string())
        .collect()
}

/// Look up the cron job of each Job in `names`, concurrently, within
/// [`JOB_LOOKUP_BUDGET`].
pub(super) async fn fetch_job_owners(
    client: &Client,
    namespace: &str,
    names: Vec<String>,
) -> Lookups {
    let api: Api<Job> = Api::namespaced(client.clone(), namespace);
    let lookup = |name: String| {
        let api = api.clone();
        let namespace = namespace.to_string();
        async move { classify(api.get(&name).await, &namespace, &name) }
    };
    lookup_all(names, lookup, Instant::now() + JOB_LOOKUP_BUDGET, namespace).await
}

/// Run `lookup` for every name until `deadline`. Lookups still running
/// then are dropped (which aborts them) and left out, to retry next poll.
async fn lookup_all<F, Fut>(
    names: Vec<String>,
    lookup: F,
    deadline: Instant,
    namespace: &str,
) -> Lookups
where
    F: Fn(String) -> Fut,
    Fut: Future<Output = Lookup> + Send + 'static,
{
    let mut tasks = JoinSet::new();
    for name in names {
        let found = lookup(name.clone());
        tasks.spawn(async move { (name, found.await) });
    }

    let mut lookups = Lookups::default();
    loop {
        let joined = match tokio::time::timeout_at(deadline, tasks.join_next()).await {
            Ok(Some(joined)) => joined,
            Ok(None) => break,
            Err(_) => {
                tracing::warn!("job lookups in {namespace} ran out of time; retrying next poll");
                break;
            }
        };
        match joined {
            Ok((name, Lookup::Owner(owner))) => {
                lookups.owners.insert(name, owner);
            }
            Ok((_, Lookup::Forbidden)) => lookups.forbidden = true,
            Ok((_, Lookup::Retry)) => {}
            Err(e) => tracing::warn!("job lookup task in {namespace} failed: {e}"),
        }
    }
    lookups
}

fn classify(job: Result<Job, kube::Error>, namespace: &str, name: &str) -> Lookup {
    match job {
        Ok(job) => Lookup::Owner(cron_job_of(&job)),
        Err(kube::Error::Api(status)) if status.is_not_found() => Lookup::Owner(None),
        Err(kube::Error::Api(status)) if status.is_forbidden() => Lookup::Forbidden,
        Err(e) => {
            tracing::warn!(
                "cannot read job {namespace}/{name}, retrying next poll: {}",
                crate::error::error_chain(&e)
            );
            Lookup::Retry
        }
    }
}

/// The name of the cron job that controls `job`, if any.
fn cron_job_of(job: &Job) -> Option<String> {
    let owner = job
        .metadata
        .owner_references
        .as_deref()?
        .iter()
        .find(|o| o.controller == Some(true))?;
    (owner.kind == CRON_JOB).then(|| owner.name.clone())
}

#[cfg(test)]
mod tests {
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
    use kube::api::ObjectMeta;
    use kube::core::Status;

    use super::*;

    fn job_owned_by(kind: &str, controller: bool) -> Job {
        Job {
            metadata: ObjectMeta {
                name: Some("backup-29000000".to_string()),
                owner_references: Some(vec![OwnerReference {
                    kind: kind.to_string(),
                    name: "backup".to_string(),
                    controller: Some(controller),
                    ..OwnerReference::default()
                }]),
                ..ObjectMeta::default()
            },
            ..Job::default()
        }
    }

    fn api_error(code: u16, reason: &str) -> kube::Error {
        kube::Error::Api(Box::new(Status {
            code,
            reason: reason.to_string(),
            ..Status::default()
        }))
    }

    #[test]
    fn cron_job_is_the_controlling_owner() {
        assert_eq!(
            cron_job_of(&job_owned_by(CRON_JOB, true)).as_deref(),
            Some("backup")
        );
        assert_eq!(cron_job_of(&job_owned_by(CRON_JOB, false)), None);
        assert_eq!(cron_job_of(&job_owned_by("Workflow", true)), None);
        assert_eq!(cron_job_of(&Job::default()), None);
    }

    #[test]
    fn only_final_answers_are_owners() {
        let found = classify(Ok(job_owned_by(CRON_JOB, true)), "ns", "j");
        assert_eq!(found, Lookup::Owner(Some("backup".to_string())));
        assert_eq!(
            classify(Err(api_error(404, "NotFound")), "ns", "j"),
            Lookup::Owner(None)
        );
        assert_eq!(
            classify(Err(api_error(403, "Forbidden")), "ns", "j"),
            Lookup::Forbidden
        );
        assert_eq!(
            classify(Err(api_error(503, "ServiceUnavailable")), "ns", "j"),
            Lookup::Retry
        );
    }

    #[test]
    fn known_jobs_are_not_refetched_and_gone_ones_are_forgotten() {
        let mut owners = JobOwners::from([
            ("kept".to_string(), Some("backup".to_string())),
            ("gone".to_string(), None),
        ]);

        let fetch = jobs_to_fetch(&mut owners, &HashSet::from(["kept", "new"]));

        assert_eq!(fetch, ["new"]);
        assert_eq!(owners.keys().collect::<Vec<_>>(), ["kept"]);
    }

    #[tokio::test]
    async fn lookups_keep_answers_and_flag_forbidden() {
        let lookup = |name: String| async move {
            match name.as_str() {
                "a" => Lookup::Owner(Some("cron".to_string())),
                "b" => Lookup::Forbidden,
                _ => Lookup::Retry,
            }
        };
        let names = ["a", "b", "c"].map(str::to_string).to_vec();

        let found = lookup_all(names, lookup, Instant::now() + JOB_LOOKUP_BUDGET, "ns").await;

        assert_eq!(
            found,
            Lookups {
                owners: JobOwners::from([("a".to_string(), Some("cron".to_string()))]),
                forbidden: true,
            }
        );
    }

    #[tokio::test]
    async fn lookups_past_the_deadline_are_left_to_retry() {
        let lookup = |name: String| async move {
            if name == "slow" {
                std::future::pending::<()>().await;
            }
            Lookup::Owner(None)
        };
        let names = ["fast", "slow"].map(str::to_string).to_vec();
        let deadline = Instant::now() + Duration::from_millis(50);

        let found = lookup_all(names, lookup, deadline, "ns").await;

        assert_eq!(found.owners, JobOwners::from([("fast".to_string(), None)]));
    }
}
