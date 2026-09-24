use crate::attempt_gate::AttemptGate;

#[test]
fn a_fresh_gate_allows_a_commit() {
    let gate = AttemptGate::new();
    assert!(!gate.is_discarded(), "a fresh gate is active");
    assert!(gate.try_commit(), "nothing has claimed the attempt yet");
}

#[test]
fn a_gate_commits_only_once() {
    let gate = AttemptGate::new();
    assert!(gate.try_commit());
    assert!(!gate.try_commit(), "a second commit must not also win");
}

#[test]
fn a_discard_before_a_commit_blocks_the_commit() {
    // The window the whole design exists to close: a removal that lands
    // first must win outright, and the worker must then deliver nothing.
    let gate = AttemptGate::new();
    assert!(gate.discard(), "the first discard wins");
    assert!(!gate.discard(), "a second discard must not also win");
    assert!(!gate.try_commit(), "a discarded attempt must not deliver");
    assert!(gate.is_discarded());
}

#[test]
fn a_commit_before_a_discard_blocks_the_discard() {
    // The mirror, and the reason the manager needs `was_delivered`: the
    // commit wins, so the attempt may attempt delivery, and the finalizer
    // has to know whether it actually placed a file to remove it.
    let gate = AttemptGate::new();
    assert!(gate.try_commit());
    assert!(!gate.discard(), "the commit already had it");
    assert!(!gate.is_discarded());
}

#[test]
fn delivery_is_recorded_only_after_marking() {
    let gate = AttemptGate::new();
    assert!(!gate.was_delivered());
    gate.mark_delivered();
    assert!(gate.was_delivered());
}

#[test]
fn a_shared_gate_is_the_same_gate() {
    // The manager and the worker must arbitrate on one object, not copies.
    let gate = AttemptGate::new();
    let worker = std::sync::Arc::clone(&gate);
    assert!(worker.discard());
    assert!(
        !gate.try_commit(),
        "the other handle did not observe the discard"
    );
}
