use std::fs;
use std::path::PathBuf;

fn core_path(rel: &str) -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for part in rel.split('/') {
        path.push(part);
    }
    path
}

fn read_core(rel: &str) -> String {
    let path = core_path(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("failed to read {:?}: {e}", path))
}

fn assert_contains(haystack: &str, needle: &str) {
    assert!(haystack.contains(needle), "expected to find {needle:?}");
}

fn assert_not_contains(haystack: &str, needle: &str) {
    assert!(
        !haystack.contains(needle),
        "unexpected stale symbol/text {needle:?}"
    );
}

#[test]
fn protocol_exposes_app_monitor_names_without_old_wire_symbols() {
    let protocol = read_core("src/ipc/protocol.rs");

    for needle in [
        "OP_REGISTER_APP",
        "OP_REGISTER_MONITOR",
        "OP_ATTACH_MONITOR",
        "RegisterApp",
        "RegisterMonitor",
        "AttachMonitor",
        "monitor_id",
    ] {
        assert_contains(&protocol, needle);
    }

    for needle in [
        "OP_REGISTER_PRODUCER",
        "OP_REGISTER_CONSUMER",
        "OP_ATTACH_CONSUMER",
        "RegisterProducer",
        "RegisterConsumer",
        "AttachConsumer",
        "consumer_id",
    ] {
        assert_not_contains(&protocol, needle);
    }
}

#[test]
fn server_state_uses_app_monitor_method_and_resource_names() {
    let server = read_core("src/ipc/server.rs");

    for needle in [
        "pub struct App",
        "pub struct Monitor",
        "pub apps:",
        "pub monitors:",
        "per_monitor_surfaces",
        "register_app",
        "register_monitor",
        "remove_app",
        "remove_monitor",
        "attach_monitor",
        "PerMonitorResources",
    ] {
        assert_contains(&server, needle);
    }

    for needle in [
        "register_consumer",
        "attach_consumer",
        "remove_producer",
        "remove_consumer",
        "PerConsumerResources",
        "per_consumer_surfaces",
        "consumer_id",
    ] {
        assert_not_contains(&server, needle);
    }
}

#[test]
fn dcomp_resource_names_are_monitor_based() {
    let dcomp = read_core("src/renderer/dcomp.rs");

    for needle in [
        "PerMonitorResources",
        "PER_MONITOR_MAX_DIM",
        "PER_MONITOR_MIN_DIM",
    ] {
        assert_contains(&dcomp, needle);
    }

    for needle in [
        "PerConsumerResources",
        "PER_CONSUMER_MAX_DIM",
        "PER_CONSUMER_MIN_DIM",
    ] {
        assert_not_contains(&dcomp, needle);
    }
}

#[test]
fn server_task_runtime_logs_use_app_monitor_terms() {
    let server_task = read_core("src/server_task.rs");

    for needle in [
        "Registered App with ID: {} (PID: {})",
        "Registered Monitor with ID: {} (PID: {})",
        "CreateCanvas created ID {} for App {}",
        "CreateCanvas received but client is not a registered app",
        "AttachMonitor error: {}",
        "Attached Canvas {} to Monitor {}",
        "AttachMonitor received but client is not a registered app",
        "Cleaning up App {}",
        "Cleaning up Monitor {}",
        "monitor={} MonitorLocal",
    ] {
        assert_contains(&server_task, needle);
    }

    for needle in [
        "Registered Producer",
        "Registered Consumer",
        "CreateCanvas created ID {} for Producer {}",
        "registered producer",
        "AttachConsumer",
        "Attached Canvas {} to Consumer {}",
        "Cleaning up Producer",
        "Cleaning up Consumer",
        "consumer={} MonitorLocal",
    ] {
        assert_not_contains(&server_task, needle);
    }
}
