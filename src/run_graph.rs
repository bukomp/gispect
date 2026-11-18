//! The job graph of a workflow run: which column each job sits in and
//! which jobs it waited for, like the graph on a run's GitHub page.
//!
//! The REST API doesn't say which jobs a job `needs`, and reading them
//! from the workflow file breaks down with reusable workflows and matrix
//! names. Timing gives the same picture: GitHub creates (queues) a job
//! only once every job it needs has finished, so a job follows each job
//! of the same attempt that finished by the time it was created.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::gh::Job;
use crate::pr::parse_utc;

pub struct Graph {
    /// Job indices per column, top to bottom.
    pub columns: Vec<Vec<usize>>,
    /// Per job: the jobs of the previous column it waited for.
    pub needs: Vec<Vec<usize>>,
}

impl Graph {
    /// Column and row of job `idx`.
    pub fn position(&self, idx: usize) -> Option<(usize, usize)> {
        self.columns
            .iter()
            .enumerate()
            .find_map(|(c, col)| Some((c, col.iter().position(|&j| j == idx)?)))
    }
}

/// Whether `job` was created after `before` finished, so it may need it.
fn follows(job: &Job, before: &Job) -> bool {
    let (Some(created), Some(prev_created)) =
        (parse_utc(&job.created_at), parse_utc(&before.created_at))
    else {
        return false;
    };
    let Some(done) = before.completed_at.as_deref().and_then(parse_utc) else {
        return false;
    };
    job.run_attempt == before.run_attempt && prev_created < created && done <= created
}

pub fn layout(jobs: &[Job]) -> Graph {
    // Columns from timing, in creation order so each job's predecessors
    // already have a column.
    let mut order: Vec<usize> = (0..jobs.len()).collect();
    order.sort_by_key(|&i| {
        (
            parse_utc(&jobs[i].created_at).unwrap_or(i64::MAX),
            jobs[i].id,
        )
    });
    let mut column = vec![0usize; jobs.len()];
    for (pos, &j) in order.iter().enumerate() {
        column[j] = order[..pos]
            .iter()
            .filter(|&&k| follows(&jobs[j], &jobs[k]))
            .map(|&k| column[k] + 1)
            .max()
            .unwrap_or(0);
    }
    let needs: Vec<Vec<usize>> = (0..jobs.len())
        .map(|j| {
            (0..jobs.len())
                .filter(|&k| {
                    column[j] > 0 && column[k] + 1 == column[j] && follows(&jobs[j], &jobs[k])
                })
                .collect()
        })
        .collect();

    let width = column.iter().map(|c| c + 1).max().unwrap_or(0);
    let mut columns: Vec<Vec<usize>> = vec![Vec::new(); width];
    for (j, &c) in column.iter().enumerate() {
        columns[c].push(j);
    }
    let name = |j: usize| jobs[j].name.to_lowercase();
    if let Some(first) = columns.first_mut() {
        first.sort_by_cached_key(|&j| (name(j), jobs[j].id));
    }
    // Later columns sit next to the jobs they wait for (fewer crossing
    // lines), then by name so matrix jobs stay together.
    for c in 1..width {
        let (done, rest) = columns.split_at_mut(c);
        let prev = &done[c - 1];
        let row = |j: usize| prev.iter().position(|&k| k == j).unwrap_or(0);
        rest[0].sort_by_cached_key(|&j| {
            let rows: Vec<usize> = needs[j].iter().map(|&k| row(k)).collect();
            let center = if rows.is_empty() {
                usize::MAX
            } else {
                rows.iter().sum::<usize>() * 100 / rows.len()
            };
            (center, name(j), jobs[j].id)
        });
    }
    Graph { columns, needs }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Seconds from `start` to `end`, or to now while not ended.
pub fn elapsed(start: Option<&str>, end: Option<&str>) -> Option<i64> {
    let start = parse_utc(start?)?;
    let end = match end {
        Some(end) => parse_utc(end)?,
        None => now(),
    };
    Some((end - start).max(0))
}

/// `42s`, `3m 05s`, `1h 12m`.
pub fn format_duration(secs: i64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m {:02}s", s / 60, s % 60),
        s => format!("{}h {:02}m", s / 3600, s % 3600 / 60),
    }
}

/// Run time of a job: start to finish, or so far.
pub fn job_duration(job: &Job) -> Option<i64> {
    match job.status.as_str() {
        "completed" | "in_progress" => {
            elapsed(job.started_at.as_deref(), job.completed_at.as_deref())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(id: u64, name: &str, created: &str, done: Option<&str>) -> Job {
        let ts = |hms: &str| format!("2026-11-18T{hms}Z");
        Job {
            id,
            name: name.into(),
            status: if done.is_some() {
                "completed"
            } else {
                "in_progress"
            }
            .into(),
            created_at: ts(created),
            started_at: Some(ts(created)),
            completed_at: done.map(ts),
            run_attempt: 1,
            ..Default::default()
        }
    }

    #[test]
    fn columns_follow_timing() {
        // lint and test start together; build waits for both; deploy
        // waits for build. A fast job that finishes in the second the
        // others are created doesn't push them right.
        let jobs = vec![
            job(1, "test", "08:00:00", Some("08:03:00")),
            job(2, "lint", "08:00:00", Some("08:01:00")),
            job(3, "fast", "08:00:00", Some("08:00:00")),
            job(4, "build", "08:03:01", Some("08:05:00")),
            job(5, "deploy", "08:05:00", None),
        ];
        let g = layout(&jobs);
        assert_eq!(g.columns, vec![vec![2, 1, 0], vec![3], vec![4]]);
        assert_eq!(g.needs[3], vec![0, 1, 2]);
        assert_eq!(g.needs[4], vec![3]);
        assert!(g.needs[0].is_empty());
        assert_eq!(g.position(4), Some((2, 0)));
    }

    #[test]
    fn reruns_only_follow_their_own_attempt() {
        let mut rerun = job(2, "b", "09:00:00", None);
        rerun.run_attempt = 2;
        let g = layout(&[job(1, "a", "08:00:00", Some("08:01:00")), rerun]);
        assert_eq!(g.columns, vec![vec![0, 1]]);
    }

    #[test]
    fn durations() {
        assert_eq!(format_duration(42), "42s");
        assert_eq!(format_duration(185), "3m 05s");
        assert_eq!(format_duration(4320), "1h 12m");
        assert_eq!(
            elapsed(Some("2026-11-18T08:00:00Z"), Some("2026-11-18T08:02:30Z")),
            Some(150)
        );
    }
}
