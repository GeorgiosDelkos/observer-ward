//! The cron job behind each Job, so a cron job's pods can be hidden as one
//! workload.
//!
//! A cron job makes a new Job, with a new name, for every run, and its pods
//! only reference that Job. The Job's own owner reference names the cron
//! job. One metadata-only list of the namespace's Jobs answers that for
//! every Job at once, and the answer is kept until a Job with pods turns up
//! that the last list did not have: once per cron job run, not per poll.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use k8s_openapi::api::batch::v1::Job;
use kube::Client;
use kube::api::{Api, ListParams, ObjectMeta};

/// The `Job` kind, as it appears in a pod's controller reference.
pub(super) const JOB: &str = "Job";
pub(super) const CRON_JOB: &str = "CronJob";

/// Time allowed for the Job list. It only refines how pods are grouped, so
/// a slow list must not push the whole cluster poll past its timeout.
const JOB_LIST_BUDGET: Duration = Duration::from_secs(5);

/// The name of the cron job owning each Job, `None` for a Job without one.
/// Keyed by Job name, within the target's namespace.
pub(super) type JobOwners = HashMap<String, Option<String>>;

/// The outcome of listing the namespace's Jobs.
#[derive(Debug, PartialEq)]
pub(super) enum JobList {
    /// Every Job in `wanted`, with its cron job. A wanted Job the list did
    /// not have (deleted meanwhile) maps to `None`, so it is not listed
    /// again for as long as its pods linger.
    Owners(JobOwners),
    /// Reading Jobs is not allowed; asking again will not help.
    Forbidden,
    /// A transient failure or timeout, already logged; retry next poll.
    Failed,
}

/// List the Jobs in `namespace` within [`JOB_LIST_BUDGET`] and map each to
/// its cron job.
pub(super) async fn list_job_owners(
    client: &Client,
    namespace: &str,
    wanted: &HashSet<&str>,
) -> JobList {
    let api: Api<Job> = Api::namespaced(client.clone(), namespace);
    let params = ListParams::default();
    let list = api.list_metadata(&params);
    let Ok(listed) = tokio::time::timeout(JOB_LIST_BUDGET, list).await else {
        tracing::warn!("listing jobs in {namespace} ran out of time; retrying next poll");
        return JobList::Failed;
    };
    match listed {
        Ok(list) => JobList::Owners(owners_of(list.items.iter().map(|j| &j.metadata), wanted)),
        Err(kube::Error::Api(status)) if status.is_forbidden() => JobList::Forbidden,
        Err(e) => {
            tracing::warn!(
                "cannot list jobs in {namespace}, retrying next poll: {}",
                crate::error::error_chain(&e)
            );
            JobList::Failed
        }
    }
}

fn owners_of<'a>(jobs: impl Iterator<Item = &'a ObjectMeta>, wanted: &HashSet<&str>) -> JobOwners {
    let mut owners: JobOwners = jobs
        .filter_map(|job| Some((job.name.clone()?, cron_job_of(job))))
        .collect();
    for name in wanted {
        owners.entry((*name).to_string()).or_insert(None);
    }
    owners
}

/// The name of the cron job that controls the Job with `meta`, if any.
fn cron_job_of(meta: &ObjectMeta) -> Option<String> {
    let owner = meta
        .owner_references
        .as_deref()?
        .iter()
        .find(|o| o.controller == Some(true))?;
    (owner.kind == CRON_JOB).then(|| owner.name.clone())
}

#[cfg(test)]
mod tests {
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;

    use super::*;

    fn job(name: &str, owner_kind: &str, controller: bool) -> ObjectMeta {
        ObjectMeta {
            name: Some(name.to_string()),
            owner_references: Some(vec![OwnerReference {
                kind: owner_kind.to_string(),
                name: "backup".to_string(),
                controller: Some(controller),
                ..OwnerReference::default()
            }]),
            ..ObjectMeta::default()
        }
    }

    #[test]
    fn cron_job_is_the_controlling_owner() {
        assert_eq!(
            cron_job_of(&job("j", CRON_JOB, true)).as_deref(),
            Some("backup")
        );
        assert_eq!(cron_job_of(&job("j", CRON_JOB, false)), None);
        assert_eq!(cron_job_of(&job("j", "Workflow", true)), None);
        assert_eq!(cron_job_of(&ObjectMeta::default()), None);
    }

    #[test]
    fn listed_jobs_map_to_their_cron_job_and_missing_wanted_ones_to_none() {
        let listed = [
            job("backup-1", CRON_JOB, true),
            job("manual", "Workflow", true),
        ];

        let owners = owners_of(listed.iter(), &HashSet::from(["backup-1", "deleted"]));

        assert_eq!(
            owners,
            JobOwners::from([
                ("backup-1".to_string(), Some("backup".to_string())),
                ("manual".to_string(), None),
                ("deleted".to_string(), None),
            ])
        );
    }
}
