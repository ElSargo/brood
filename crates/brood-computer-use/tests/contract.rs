#![allow(
    clippy::expect_used,
    reason = "contract-test setup must stop immediately when a fixture or prerequisite is invalid"
)]

use std::{collections::BTreeSet, fs, path::PathBuf};

use brood_computer_use::{
    ApprovalKind, ComputerActionBatch, ComputerApprovalDecision, ComputerApprovalRequest,
    ComputerCancellation, ComputerScriptRequest, ComputerScriptResult, ComputerUseError,
    ComputerUseLease, ComputerUseLimits, ComputerUsePolicy, ComputerUseScope,
    ComputerUseSupervisor, ComputerUseTask, TaskId, TaskState,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
struct LifecycleFixture {
    task: ComputerUseTask,
    lease: ComputerUseLease,
    script_request: ComputerScriptRequest,
    script_result: ComputerScriptResult,
    approval_request: ComputerApprovalRequest,
    approval_decision: ComputerApprovalDecision,
    cancellation: ComputerCancellation,
    error: ComputerUseError,
}

fn fixture(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    fs::read_to_string(path).expect("read golden fixture")
}

fn running_supervisor() -> (ComputerUseSupervisor, TaskId, u64) {
    let mut supervisor = ComputerUseSupervisor::new();
    let task = TaskId("task-7".to_string());
    supervisor
        .create_task(
            &task,
            "test-adapter".to_string(),
            ComputerUseScope {
                session: 3,
                windows: BTreeSet::from([41]),
            },
            ComputerUsePolicy::default(),
            ComputerUseLimits::default(),
            1000,
        )
        .expect("create task");
    let generation = supervisor
        .acquire_lease(&task, 1010, 5000)
        .expect("acquire lease")
        .generation;
    (supervisor, task, generation)
}

#[test]
fn direct_action_batch_matches_golden_fixture() {
    let decoded: ComputerActionBatch =
        serde_json::from_str(&fixture("direct-action-batch-v1.json"))
            .expect("decode direct action fixture");
    let encoded = serde_json::to_value(decoded).expect("encode direct action fixture");
    let golden: serde_json::Value =
        serde_json::from_str(&fixture("direct-action-batch-v1.json")).expect("decode golden json");
    assert_eq!(encoded, golden);
}

#[test]
fn observation_matches_golden_fixture() {
    let golden = fixture("observation-v1.json");
    let decoded: brood_computer_use::ComputerObservation =
        serde_json::from_str(&golden).expect("decode observation fixture");
    assert_eq!(
        serde_json::to_value(decoded).expect("encode observation"),
        serde_json::from_str::<serde_json::Value>(&golden).expect("decode golden json")
    );
}

#[test]
fn lifecycle_messages_match_golden_fixture() {
    let golden = fixture("lifecycle-v1.json");
    let decoded: LifecycleFixture =
        serde_json::from_str(&golden).expect("decode lifecycle fixture");
    assert_eq!(
        serde_json::to_value(decoded).expect("encode lifecycle fixture"),
        serde_json::from_str::<serde_json::Value>(&golden).expect("decode golden json")
    );
}

#[test]
fn stale_lease_and_cross_session_are_rejected() {
    let (supervisor, _task, generation) = running_supervisor();
    let mut batch: ComputerActionBatch =
        serde_json::from_str(&fixture("direct-action-batch-v1.json")).expect("decode fixture");
    batch.lease_generation = generation + 1;
    assert_eq!(
        supervisor.validate_batch(&batch, 1020),
        Err(ComputerUseError::StaleLease)
    );
    batch.lease_generation = generation;
    batch.target_session = 4;
    assert_eq!(
        supervisor.validate_batch(&batch, 1020),
        Err(ComputerUseError::ScopeDenied)
    );
}

#[test]
fn lease_is_daemon_wide_and_generation_never_reused() {
    let (mut supervisor, first, first_generation) = running_supervisor();
    let second = TaskId("task-8".to_string());
    supervisor
        .create_task(
            &second,
            "other".to_string(),
            ComputerUseScope {
                session: 9,
                windows: BTreeSet::new(),
            },
            ComputerUsePolicy::default(),
            ComputerUseLimits::default(),
            1020,
        )
        .expect("create second task");
    assert_eq!(
        supervisor.acquire_lease(&second, 1030, 1000),
        Err(ComputerUseError::LeaseBusy)
    );
    supervisor.stop(&first, 1040).expect("stop first task");
    let second_generation = supervisor
        .acquire_lease(&second, 1050, 1000)
        .expect("acquire second lease")
        .generation;
    assert!(second_generation > first_generation);
}

#[test]
fn approval_proof_is_scoped_expiring_and_single_use() {
    let mut supervisor = ComputerUseSupervisor::new();
    let task = TaskId("task-7".to_string());
    supervisor
        .create_task(
            &task,
            "test-adapter".to_string(),
            ComputerUseScope {
                session: 3,
                windows: BTreeSet::from([41]),
            },
            ComputerUsePolicy {
                require_approval: BTreeSet::from([ApprovalKind::Pointer]),
                ..ComputerUsePolicy::default()
            },
            ComputerUseLimits::default(),
            1000,
        )
        .expect("create task");
    let generation = supervisor
        .acquire_lease(&task, 1010, 5000)
        .expect("acquire lease")
        .generation;
    let request = ComputerApprovalRequest {
        id: "approval-1".to_string(),
        task: task.clone(),
        sequence: 1,
        kind: ApprovalKind::Pointer,
        prompt: "allow pointer input".to_string(),
        created_at_ms: 1020,
        expires_at_ms: 2000,
    };
    supervisor
        .request_approval(request)
        .expect("request approval");
    assert_eq!(
        supervisor.task(&task).expect("task").state,
        TaskState::WaitingApproval
    );
    supervisor
        .decide_approval(&ComputerApprovalDecision {
            request_id: "approval-1".to_string(),
            approved: true,
            principal: "operator".to_string(),
            decided_at_ms: 1030,
        })
        .expect("approve request");

    let mut batch: ComputerActionBatch =
        serde_json::from_str(&fixture("direct-action-batch-v1.json")).expect("decode fixture");
    batch.lease_generation = generation;
    batch.actions.truncate(1);
    batch.approval_proof = Some("approval-1".to_string());
    supervisor
        .commit_batch(&batch, 1040)
        .expect("consume approved action");
    batch.expected_sequence = 2;
    assert_eq!(
        supervisor.validate_batch(&batch, 1050),
        Err(ComputerUseError::ApprovalRequired)
    );
}
