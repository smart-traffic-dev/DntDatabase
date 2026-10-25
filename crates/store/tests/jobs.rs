//! Migration 7: persisted background jobs.

use dnt_core::config::ConnectionConfig;
use dnt_core::secret::MemorySecretStore;
use dnt_core::test_util::sample_config;
use dnt_store::db::open_in_memory;
use dnt_store::jobs::JobRecord;
use dnt_store::repo::{ConnectionRepo, NewSecrets};
use std::sync::Arc;
use uuid::Uuid;

fn repo() -> ConnectionRepo {
    ConnectionRepo::new(
        open_in_memory().unwrap(),
        Arc::new(MemorySecretStore::default()),
    )
}

fn connection(repo: &ConnectionRepo) -> Uuid {
    let cfg = ConnectionConfig {
        id: Uuid::nil(),
        name: "a".into(),
        ..sample_config()
    };
    repo.create(
        cfg,
        NewSecrets {
            db_password: None,
            ssh_password: None,
            ssh_key_passphrase: None,
        },
    )
    .unwrap()
    .id
}

fn job(state: &str, created_at: &str) -> JobRecord {
    JobRecord {
        id: Uuid::new_v4(),
        kind: "import".into(),
        connection_id: None,
        title: format!("Job {created_at}"),
        params_json: None,
        rerunnable: false,
        state: state.into(),
        created_at: created_at.into(),
        started_at: None,
        finished_at: None,
        result_path: None,
        summary: None,
        error_json: None,
        outcome_unknown: false,
        log_json: "[]".into(),
        dropped_lines: 0,
    }
}

fn ts(i: u32) -> String {
    format!("2026-10-25T10:00:{i:02}.000Z")
}

fn titles(repo: &ConnectionRepo) -> Vec<String> {
    repo.job_list(100)
        .unwrap()
        .into_iter()
        .map(|j| j.title)
        .collect()
}

#[test]
fn insert_get_update_round_trip() {
    let repo = repo();
    let conn = connection(&repo);
    let mut j = JobRecord {
        connection_id: Some(conn),
        params_json: Some(r#"{"file":"a.csv"}"#.into()),
        rerunnable: true,
        ..job("queued", &ts(1))
    };
    repo.job_insert(&j).unwrap();
    assert_eq!(repo.job_get(j.id).unwrap(), Some(j.clone()));
    assert_eq!(repo.job_get(Uuid::new_v4()).unwrap(), None);

    j.state = "failed".into();
    j.started_at = Some(ts(2));
    j.finished_at = Some(ts(3));
    j.result_path = Some("C:/out/a.sql".into());
    j.summary = Some("3 rows".into());
    j.error_json = Some(r#"{"kind":"server"}"#.into());
    j.outcome_unknown = true;
    j.log_json = r#"[{"level":"info","message":"x"}]"#.into();
    j.dropped_lines = 12;
    j.title = "Renamed".into();
    j.params_json = None;
    j.rerunnable = false;
    repo.job_update(&j).unwrap();
    assert_eq!(repo.job_get(j.id).unwrap(), Some(j));
}

#[test]
fn invalid_kind_or_state_is_refused() {
    let repo = repo();
    let bad_kind = JobRecord {
        kind: "x".into(),
        ..job("queued", &ts(1))
    };
    assert!(repo.job_insert(&bad_kind).is_err());
    assert!(repo.job_insert(&job("y", &ts(1))).is_err());
}

#[test]
fn list_is_newest_first_and_limited() {
    let repo = repo();
    for i in [2, 1, 3] {
        repo.job_insert(&job("done", &ts(i))).unwrap();
    }
    assert_eq!(
        titles(&repo),
        vec![
            format!("Job {}", ts(3)),
            format!("Job {}", ts(2)),
            format!("Job {}", ts(1))
        ]
    );
    let two = repo.job_list(2).unwrap();
    assert_eq!(two.len(), 2);
    assert_eq!(two[0].created_at, ts(3));
}

#[test]
fn mark_interrupted_only_touches_live_jobs() {
    let repo = repo();
    let queued = job("queued", &ts(1));
    let running = job("running", &ts(2));
    let done = job("done", &ts(3));
    let cancelled = job("cancelled", &ts(4));
    for j in [&queued, &running, &done, &cancelled] {
        repo.job_insert(j).unwrap();
    }
    let n = repo
        .job_mark_interrupted("The app closed while the job ran.", &ts(9))
        .unwrap();
    assert_eq!(n, 2);
    for j in [&queued, &running] {
        let got = repo.job_get(j.id).unwrap().unwrap();
        assert_eq!(got.state, "failed");
        assert!(got.outcome_unknown);
        assert_eq!(got.finished_at.as_deref(), Some(ts(9).as_str()));
        assert_eq!(
            got.summary.as_deref(),
            Some("The app closed while the job ran.")
        );
    }
    for j in [&done, &cancelled] {
        assert_eq!(repo.job_get(j.id).unwrap().as_ref(), Some(j));
    }
    assert_eq!(repo.job_mark_interrupted("again", &ts(10)).unwrap(), 0);
}

#[test]
fn clear_finished_keeps_live_jobs() {
    let repo = repo();
    let states = ["queued", "running", "done", "failed", "cancelled"];
    for (i, s) in states.iter().enumerate() {
        repo.job_insert(&job(s, &ts(i as u32))).unwrap();
    }
    assert_eq!(repo.job_clear_finished().unwrap(), 3);
    let mut left: Vec<String> = repo
        .job_list(100)
        .unwrap()
        .into_iter()
        .map(|j| j.state)
        .collect();
    left.sort();
    assert_eq!(left, vec!["queued", "running"]);
}

#[test]
fn prune_keeps_newest_finished_and_every_live_job() {
    let repo = repo();
    repo.job_insert(&job("running", &ts(0))).unwrap();
    for i in 1..=4 {
        repo.job_insert(&job("done", &ts(i))).unwrap();
    }
    repo.job_insert(&job("queued", &ts(5))).unwrap();
    repo.job_prune(2).unwrap();
    assert_eq!(
        titles(&repo),
        vec![
            format!("Job {}", ts(5)),
            format!("Job {}", ts(4)),
            format!("Job {}", ts(3)),
            format!("Job {}", ts(0))
        ]
    );
}

#[test]
fn deleting_the_connection_detaches_its_jobs() {
    let repo = repo();
    let conn = connection(&repo);
    let j = JobRecord {
        connection_id: Some(conn),
        ..job("done", &ts(1))
    };
    repo.job_insert(&j).unwrap();
    repo.delete(conn).unwrap();
    let got = repo.job_get(j.id).unwrap().unwrap();
    assert_eq!(got.connection_id, None);
}
