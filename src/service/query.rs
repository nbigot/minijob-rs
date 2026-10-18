//! `GET /api/v1/jobs/`: filters, sorting and pagination (`GetJobs` in `service/service.go`).
//!
//! The sort keys are read once per job ([`select`]), so sorting and paginating
//! ([`paginate`]) touch no job; only the jobs of the requested page are copied (as `Arc`).
//! The service runs both under its read lock.

use crate::model::{Job, JobState};
use std::cmp::Ordering;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Clone, Debug, Default)]
pub struct GetJobsRequest {
    /// 1-based.
    pub page: u64,
    /// At least 1 (checked by the handler).
    pub limit: u64,
    pub status: String,
    pub topic: String,
    pub sort: String,
    pub search: String,
}

#[derive(Clone, Debug, Default)]
pub struct GetJobsResponse {
    pub jobs: Vec<Arc<Job>>,
    pub total: u64,
    pub page: u64,
    pub limit: u64,
    pub total_pages: u64,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Criterion {
    CreatedAsc,
    CreatedDesc,
    PriorityAsc,
    PriorityDesc,
    UpdatedAsc,
    UpdatedDesc,
    TopicAsc,
    TopicDesc,
}

/// Comma-separated criteria, default `created_desc`; unknown ones are ignored.
fn criteria(sort: &str) -> Vec<Criterion> {
    let sort = if sort.is_empty() {
        "created_desc"
    } else {
        sort
    };
    sort.split(',')
        .filter_map(|c| match c.trim() {
            "created_asc" => Some(Criterion::CreatedAsc),
            "created_desc" => Some(Criterion::CreatedDesc),
            "priority_asc" => Some(Criterion::PriorityAsc),
            "priority_desc" => Some(Criterion::PriorityDesc),
            "updated_asc" => Some(Criterion::UpdatedAsc),
            "updated_desc" => Some(Criterion::UpdatedDesc),
            "topic_asc" => Some(Criterion::TopicAsc),
            "topic_desc" => Some(Criterion::TopicDesc),
            _ => None,
        })
        .collect()
}

/// A matching job with its sort keys.
pub struct Entry<'a> {
    created: i64,
    updated: i64,
    priority: i8,
    id: Uuid,
    job: &'a Arc<Job>,
}

fn matches(job: &Job, req: &GetJobsRequest, status: Option<JobState>, search: &str) -> bool {
    if let Some(s) = status
        && (s == JobState::NoState || job.state() != s)
    {
        return false;
    }
    if !req.topic.is_empty() && job.topic != req.topic {
        return false;
    }
    // Case-insensitive substring of the id (Go's helper failed when lengths were equal).
    if search.is_empty() {
        return true;
    }
    let mut buf = Uuid::encode_buffer();
    job.id.hyphenated().encode_lower(&mut buf).contains(search)
}

fn compare(a: &Entry<'_>, b: &Entry<'_>, criteria: &[Criterion]) -> Ordering {
    for c in criteria {
        let o = match c {
            Criterion::CreatedAsc => a.created.cmp(&b.created),
            Criterion::CreatedDesc => b.created.cmp(&a.created),
            Criterion::PriorityAsc => a.priority.cmp(&b.priority),
            Criterion::PriorityDesc => b.priority.cmp(&a.priority),
            Criterion::UpdatedAsc => a.updated.cmp(&b.updated),
            Criterion::UpdatedDesc => b.updated.cmp(&a.updated),
            Criterion::TopicAsc => a.job.topic.cmp(&b.job.topic),
            Criterion::TopicDesc => b.job.topic.cmp(&a.job.topic),
        };
        if o != Ordering::Equal {
            return o;
        }
    }
    a.created.cmp(&b.created).then_with(|| a.id.cmp(&b.id))
}

/// The jobs matching the filters, with their sort keys (one pass).
pub fn select<'a>(
    jobs: impl Iterator<Item = &'a Arc<Job>>,
    req: &GetJobsRequest,
) -> Vec<Entry<'a>> {
    let status = (!req.status.is_empty()).then(|| JobState::from_filter(&req.status));
    let search = req.search.to_lowercase();
    jobs.filter(|j| matches(j, req, status, &search))
        .map(|j| Entry {
            created: j.creation_timestamp(),
            updated: j.last_update(),
            priority: j.priority,
            id: j.id,
            job: j,
        })
        .collect()
}

/// Sorts and paginates a selection. The order is total (creation time then id last), so
/// only the requested page has to be fully sorted.
pub fn paginate(mut entries: Vec<Entry<'_>>, req: &GetJobsRequest) -> GetJobsResponse {
    let criteria = criteria(&req.sort);
    let cmp = |a: &Entry<'_>, b: &Entry<'_>| compare(a, b, &criteria);
    let total = entries.len();
    let limit = req.limit.max(1);
    let total_pages = (total as u64).div_ceil(limit);
    let start = req.page.saturating_sub(1).saturating_mul(limit);
    let mut jobs = Vec::new();
    if start < total as u64 {
        let start = start as usize;
        let end = (start + limit as usize).min(total);
        if end < total {
            entries.select_nth_unstable_by(end, cmp);
        }
        let head = &mut entries[..end];
        if start > 0 {
            head.select_nth_unstable_by(start, cmp);
        }
        let page = &mut head[start..];
        page.sort_unstable_by(cmp);
        jobs = page.iter().map(|e| Arc::clone(e.job)).collect();
    }
    GetJobsResponse {
        jobs,
        total: total as u64,
        page: req.page,
        limit: req.limit,
        total_pages,
    }
}

/// Filters, sorts and paginates `jobs`.
pub fn get_jobs(jobs: &[Arc<Job>], req: &GetJobsRequest) -> GetJobsResponse {
    paginate(select(jobs.iter(), req), req)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::events;
    use uuid::Uuid;

    fn job(n: u128, topic: &str, prio: i8, created: i64, last: Option<&str>) -> Arc<Job> {
        let mut j = Job {
            topic: topic.into(),
            priority: prio,
            ..Default::default()
        };
        j.init(Uuid::from_u128(n), created);
        if let Some(e) = last {
            j.add_event(e, created + 10);
        }
        Arc::new(j)
    }

    fn ids(r: &GetJobsResponse) -> Vec<u128> {
        r.jobs.iter().map(|j| j.id.as_u128()).collect()
    }

    #[test]
    fn filter_sort_paginate() {
        let jobs = vec![
            job(1, "a", 0, 100, Some(events::PENDING)),
            job(2, "b", 5, 200, Some(events::PENDING)),
            job(3, "a", 5, 300, None),
            job(0xABC, "a", 1, 300, None),
        ];
        let req =
            |page, limit, status: &str, topic: &str, sort: &str, search: &str| GetJobsRequest {
                page,
                limit,
                status: status.into(),
                topic: topic.into(),
                sort: sort.into(),
                search: search.into(),
            };
        let r = get_jobs(&jobs, &req(1, 10, "", "", "", ""));
        assert_eq!(ids(&r), [3, 0xABC, 2, 1]);
        assert_eq!((r.total, r.total_pages), (4, 1));
        let r = get_jobs(&jobs, &req(1, 10, "PENDING", "", "priority_desc", ""));
        assert_eq!(ids(&r), [2, 1]);
        let r = get_jobs(&jobs, &req(1, 10, "nope", "", "", ""));
        assert_eq!(r.total, 0);
        let r = get_jobs(&jobs, &req(2, 3, "", "", "created_asc", ""));
        assert_eq!(ids(&r), [0xABC]);
        assert_eq!(r.total_pages, 2);
        let r = get_jobs(&jobs, &req(3, 3, "", "", "", ""));
        assert!(r.jobs.is_empty());
        let r = get_jobs(
            &jobs,
            &req(1, 10, "", "a", "topic_desc, priority_asc, bogus", ""),
        );
        assert_eq!(ids(&r), [1, 0xABC, 3]);
        let r = get_jobs(&jobs, &req(1, 10, "", "", "", "0000000ABC"));
        assert_eq!(ids(&r), [0xABC]);
        let full = Uuid::from_u128(0xABC).to_string().to_uppercase();
        let r = get_jobs(&jobs, &req(1, 10, "", "", "", &full));
        assert_eq!(ids(&r), [0xABC]);
    }
}

#[cfg(test)]
mod paging_tests {
    use super::*;
    use crate::model::events;
    use uuid::Uuid;

    /// Every page of the partial sort matches the same slice of a full sort.
    #[test]
    fn pages_match_a_full_sort() {
        let mut seed = 7u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let jobs: Vec<Arc<Job>> = (0..500u128)
            .map(|i| {
                let mut j = Job {
                    topic: format!("t{}", next() % 4),
                    priority: (next() % 5) as i8,
                    ..Default::default()
                };
                j.init(Uuid::from_u128(next() as u128), (next() % 50) as i64);
                if i % 3 == 0 {
                    j.add_event(events::PENDING, 60 + (next() % 50) as i64);
                }
                Arc::new(j)
            })
            .collect();
        for sort in ["", "priority_desc", "topic_asc,updated_desc", "created_asc"] {
            let all = GetJobsRequest {
                page: 1,
                limit: 1000,
                sort: sort.into(),
                ..Default::default()
            };
            let full: Vec<Uuid> = get_jobs(&jobs, &all).jobs.iter().map(|j| j.id).collect();
            assert_eq!(full.len(), 500);
            for (page, limit) in [
                (1, 7),
                (2, 7),
                (13, 7),
                (72, 7),
                (73, 7),
                (5, 100),
                (6, 100),
            ] {
                let r = get_jobs(
                    &jobs,
                    &GetJobsRequest {
                        page,
                        limit,
                        sort: sort.into(),
                        ..Default::default()
                    },
                );
                let start = ((page - 1) * limit) as usize;
                let expected = &full[start.min(500)..(start + limit as usize).min(500)];
                let got: Vec<Uuid> = r.jobs.iter().map(|j| j.id).collect();
                assert_eq!(got, expected, "sort {sort:?} page {page} limit {limit}");
            }
        }
    }
}
