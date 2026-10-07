//! fabro-mini shape: a conditional arm versus an UNCONDITIONAL exit-kind
//! arm on the same node must stay tiered (conditional first) — the mini
//! line's soft exit is a fallback, not a rival of the success route.

mod support;

use support::*;

#[test]
fn an_unconditional_exit_kind_edge_stays_the_fallback_tier() {
    let text = dot(
        "evidence [script=\"true\"]\n  reviewer [script=\"true\"]\n  start -> evidence\n  \
         evidence -> reviewer [label=\"Evidence ready\", condition=\"outcome=succeeded\"]\n  \
         evidence -> exit [x.kind=\"soft\", label=\"Capture failed\"]\n  reviewer -> exit",
    );
    let graph = lower_ok(&text);
    let tiers = tiers(&graph, "evidence");
    assert_eq!(tiers[0].1.len(), 1);
    assert_eq!(tiers[0].1[0].0, "reviewer", "the conditional arm is tier 0");
    assert_eq!(tiers[1].1.len(), 1);
    assert_eq!(
        tiers[1].1[0].0, "exit",
        "the unconditional arm is the fallback"
    );
}
