//! The cron job behind each Job, so a cron job's pods can be hidden as one
//! workload.
//!
//! A cron job makes a new Job, with a new name, for every run, and its pods
//! only reference that Job. The Job's own owner reference names the
//! cron job, so each Job is fetched once and the answer kept for as long
//! as it has pods (an owner reference does not change).

use std::collections::HashMap;

use k8s_openapi::api::batch::v1::Job;
use kube::Client;
use kube::api::Api;
use tokio::task::JoinSet;

use crate::config::WorkloadRef;

/// Kind of the controller that owns a Job's pods.
pub(super) const JOB: &str = "Job";
const CRON_JOB: &str = "CronJob";

/// The cron job owning each Job, `None` for a Job without one. Keyed by Job
/// name, within the target's namespace.
pub(super) type JobOwners = HashMap<String, Option<WorkloadRef>>;

/// Look up the owner of each Job in `names`, concurrently. A Job that
/// cannot be read (deleted, or no permission to get Jobs) is logged and
/// treated as having no cron job, so its pods are shown as the Job itself.
pub(super) async fn fetch_job_owners(
    client: &Client,
    namespace: &str,
    names: Vec<String>,
) -> JobOwners {
    let api: Api<Job> = Api::namespaced(client.clone(), namespace);
    let mut tasks = JoinSet::new();
    for name in names {
        let api = api.clone();
        tasks.spawn(async move {
            let job = api.get(&name).await;
            (name, job)
        });
    }

    let mut owners = JobOwners::new();
    while let Some(joined) = tasks.join_next().await {
        let Ok((name, job)) = joined else {
            tracing::warn!("job lookup task in {namespace} failed");
            continue;
        };
        let owner = match job {
            Ok(job) => cron_job_of(&job),
            Err(e) => {
                tracing::warn!(
                    "cannot read job {namespace}/{name}, showing it as a job: {}",
                    crate::error::error_chain(&e)
                );
                None
            }
        };
        owners.insert(name, owner);
    }
    owners
}

/// The cron job that controls `job`, if any.
fn cron_job_of(job: &Job) -> Option<WorkloadRef> {
    let owner = job
        .metadata
        .owner_references
        .as_deref()?
        .iter()
        .find(|o| o.controller == Some(true))?;
    (owner.kind == CRON_JOB).then(|| WorkloadRef {
        kind: CRON_JOB.to_string(),
        name: owner.name.clone(),
    })
}

#[cfg(test)]
mod tests {
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
    use kube::api::ObjectMeta;

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

    #[test]
    fn cron_job_is_the_controlling_owner() {
        assert_eq!(
            cron_job_of(&job_owned_by(CRON_JOB, true)),
            Some(WorkloadRef {
                kind: CRON_JOB.to_string(),
                name: "backup".to_string(),
            })
        );
    }

    #[test]
    fn other_or_non_controlling_owners_are_not_cron_jobs() {
        assert_eq!(cron_job_of(&job_owned_by(CRON_JOB, false)), None);
        assert_eq!(cron_job_of(&job_owned_by("Workflow", true)), None);
        assert_eq!(cron_job_of(&Job::default()), None);
    }
}
