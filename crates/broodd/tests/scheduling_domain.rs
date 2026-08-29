use std::time::Duration;

use broodd::scheduling_domain::{
    ActorProgressMetrics, BACKEND_ACTOR_EVENTS_PER_TURN, BACKEND_LIFECYCLE_CAPACITY,
    BACKEND_LIFECYCLE_START_LIMIT, BoundedWorkMetrics, CallbackMetrics, LifecycleAdmission,
    SMITHAY_CALLBACK_BUDGET, SMITHAY_CALLBACK_SAMPLE_CAPACITY, backend_worker_partition,
    bounded_round_robin_plan, try_admit_lifecycle_request_id,
};

#[test]
fn runtime_partition_reserves_each_domain_without_small_host_oversubscription() {
    let one = backend_worker_partition(1);
    assert_eq!(one.actors, 1);
    assert_eq!(one.general_io, 1);

    let two = backend_worker_partition(2);
    assert_eq!(two.actors, 1);
    assert_eq!(two.general_io, 1);

    let twelve = backend_worker_partition(12);
    assert_eq!(twelve.actors, 4);
    assert_eq!(twelve.general_io, 8);
}

#[test]
fn lifecycle_admission_reserves_stop_and_cancellation_capacity() {
    let mut admission = LifecycleAdmission::default();
    for _ in 0..BACKEND_LIFECYCLE_START_LIMIT {
        assert!(admission.try_admit_start());
    }
    assert!(!admission.try_admit_start());
    for _ in BACKEND_LIFECYCLE_START_LIMIT..BACKEND_LIFECYCLE_CAPACITY {
        assert!(admission.try_admit_reserved());
    }
    assert!(!admission.try_admit_reserved());
    admission.complete();
    assert!(admission.try_admit_reserved());
}

#[test]
fn lifecycle_request_id_admission_is_transactional() {
    let mut admission = LifecycleAdmission::default();
    let mut next = 1;
    assert_eq!(
        try_admit_lifecycle_request_id(&mut next, &mut admission),
        Ok(Some(1))
    );
    assert_eq!(next, 2);
    assert_eq!(admission.active(), 1);

    let mut exhausted_admission = LifecycleAdmission::default();
    let mut exhausted = u64::MAX;
    assert!(try_admit_lifecycle_request_id(&mut exhausted, &mut exhausted_admission).is_err());
    assert_eq!(exhausted, u64::MAX);
    assert_eq!(exhausted_admission.active(), 0);

    let mut full_admission = LifecycleAdmission::default();
    while full_admission.try_admit_start() {}
    let active = full_admission.active();
    let mut unconsumed = 7;
    assert_eq!(
        try_admit_lifecycle_request_id(&mut unconsumed, &mut full_admission),
        Ok(None)
    );
    assert_eq!(unconsumed, 7);
    assert_eq!(full_admission.active(), active);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lifecycle_callback_schedules_without_waiting_for_completion() {
    let (completion, mut completions) = tokio::sync::mpsc::channel(1);
    let release = std::sync::Arc::new(tokio::sync::Notify::new());
    let started = std::time::Instant::now();
    let task_release = std::sync::Arc::clone(&release);
    let task = tokio::spawn(async move {
        task_release.notified().await;
        completion.send(7_u8).await
    });

    assert!(started.elapsed() < Duration::from_millis(10));
    assert!(completions.try_recv().is_err());
    release.notify_one();
    let received = tokio::time::timeout(Duration::from_secs(1), completions.recv()).await;
    assert!(matches!(received, Ok(Some(7))));
    let joined = task.await;
    assert!(matches!(joined, Ok(Ok(()))));
}

#[test]
fn actor_drain_plan_is_bounded_and_rotates_past_the_serviced_set() {
    let first = bounded_round_robin_plan(12, 0, 64, 8);
    assert_eq!(first.actors.len(), 8);
    assert_eq!(
        first.actors.iter().map(|(_, budget)| budget).sum::<usize>(),
        64
    );
    assert_eq!(first.next_cursor, 8);
    assert_eq!(first.unvisited_actors, 4);

    let second = bounded_round_robin_plan(12, first.next_cursor, 64, 8);
    assert_eq!(second.actors.first(), Some(&(8, 8)));
    assert!(second.actors.iter().any(|(actor, _)| *actor == 0));
}

#[test]
fn callback_metrics_retain_a_fixed_window_and_report_tail_latency() {
    let mut metrics = CallbackMetrics::default();
    for micros in 1..=SMITHAY_CALLBACK_SAMPLE_CAPACITY + 10 {
        metrics.record(Duration::from_micros(micros as u64));
    }

    let snapshot = metrics.snapshot();
    assert_eq!(
        snapshot.callbacks,
        u64::try_from(SMITHAY_CALLBACK_SAMPLE_CAPACITY + 10).unwrap_or(u64::MAX)
    );
    assert_eq!(snapshot.retained_samples, SMITHAY_CALLBACK_SAMPLE_CAPACITY);
    assert_eq!(snapshot.sample_capacity, SMITHAY_CALLBACK_SAMPLE_CAPACITY);
    assert_eq!(
        snapshot.maximum_micros,
        u64::try_from(SMITHAY_CALLBACK_SAMPLE_CAPACITY + 10).unwrap_or(u64::MAX)
    );
    let p99_rank = SMITHAY_CALLBACK_SAMPLE_CAPACITY
        .saturating_mul(99)
        .div_ceil(100)
        .saturating_sub(1);
    assert_eq!(
        snapshot.p99_micros,
        u64::try_from(11 + p99_rank).unwrap_or(u64::MAX)
    );
    let maximum = u64::try_from(SMITHAY_CALLBACK_SAMPLE_CAPACITY + 10).unwrap_or(u64::MAX);
    let budget = u64::try_from(SMITHAY_CALLBACK_BUDGET.as_micros()).unwrap_or(u64::MAX);
    assert_eq!(snapshot.over_budget, maximum.saturating_sub(budget));
}

#[test]
fn callback_budget_violations_are_counted_without_dropping_samples() {
    let mut metrics = CallbackMetrics::default();
    metrics.record(SMITHAY_CALLBACK_BUDGET);
    metrics.record(SMITHAY_CALLBACK_BUDGET + Duration::from_micros(1));

    let snapshot = metrics.snapshot();
    assert_eq!(snapshot.callbacks, 2);
    assert_eq!(snapshot.over_budget, 1);
    assert_eq!(snapshot.retained_samples, 2);
}

#[test]
fn callback_window_max_excludes_rolled_out_startup_spikes() {
    let mut metrics = CallbackMetrics::default();
    metrics.record(Duration::from_millis(50));
    for _ in 0..SMITHAY_CALLBACK_SAMPLE_CAPACITY {
        metrics.record(Duration::from_micros(100));
    }

    let snapshot = metrics.snapshot();
    assert_eq!(snapshot.maximum_micros, 100);
    assert_eq!(snapshot.lifetime_maximum_micros, 50_000);
    assert_eq!(snapshot.over_budget, 0);
    assert_eq!(snapshot.lifetime_over_budget, 1);
}

#[test]
fn bounded_work_reports_progress_and_deferral_separately_from_drops() {
    let mut metrics = BoundedWorkMetrics::default();
    metrics.record_batch(BACKEND_ACTOR_EVENTS_PER_TURN, 1);
    metrics.record_batch(7, 0);

    assert_eq!(metrics.admitted, 71);
    assert_eq!(metrics.completed, 71);
    assert_eq!(metrics.deferred_turns, 1);
    assert_eq!(metrics.dropped, 0);
    assert_eq!(metrics.last_batch, 7);
    assert_eq!(metrics.maximum_batch, BACKEND_ACTOR_EVENTS_PER_TURN);
    assert_eq!(metrics.maximum_backlog, 1);
}

#[test]
fn actor_progress_is_generation_scoped_and_filters_retired_actors() {
    let mut progress = ActorProgressMetrics::default();
    progress.record("a", 1);
    progress.record("a", 1);
    progress.record("b", 7);
    assert_eq!(progress.snapshot(|_, _| true).get("a"), Some(&2));

    progress.record("a", 2);
    assert_eq!(progress.snapshot(|_, _| true).get("a"), Some(&1));
    progress.retain_current(|actor, generation| actor == "a" && generation == 2);
    let snapshot = progress.snapshot(|_, _| true);
    assert_eq!(snapshot.len(), 1);
    assert_eq!(snapshot.get("a"), Some(&1));
}
