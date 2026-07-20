//! codypendent-routing — per-task-node model routing (Phase 7, STEP 7.2/7.3).
//!
//! Routing happens per **task node**, never per session, and follows the
//! [Chapter 09](../../docs/docs/09-model-routing-and-compaction.md) pipeline:
//! data-classification/policy and capability **hard filters** run first (security
//! before utility), then the cheapest model above a quality threshold is chosen,
//! then — on an objective validation failure — the router **escalates** along a
//! declared chain, preserving the node's artifacts. Every decision records the
//! classifier version and the routing-policy revision, so it is attributable.
//!
//! The pieces:
//!
//! * [`capability`] — [`ModelCapabilities`](capability::ModelCapabilities) and the
//!   [`RequiredCapabilities`](capability::RequiredCapabilities) hard filter.
//! * [`classify`] — the rule-based [`TaskClass`](classify::TaskClass) classifier,
//!   version-stamped.
//! * [`profile`] — [`ModelProfile`](profile::ModelProfile): capabilities +
//!   **measured** performance + execution profile + local bench.
//! * [`policy`] — the versioned [`RoutingPolicy`](policy::RoutingPolicy): λ
//!   weights, quality threshold, escalation chain, privacy ceiling.
//! * [`router`] — the [`Router`](router::Router): hard filters →
//!   cheapest-above-threshold → utility → cascading escalation.
//! * [`arms`] — the five [`RouteArm`](arms::RouteArm)s and the release-gate report
//!   (exit criterion 1).
//!
//! The crate is daemon-free and makes no network calls: measured numbers are fed
//! in as [`ModelProfile`](profile::ModelProfile)s (the daemon populates them from
//! eval + trace data and the bench harness), and the model-execution seam that
//! actually runs the chosen model is the daemon's to fill.

pub mod arms;
pub mod capability;
pub mod classify;
pub mod policy;
pub mod profile;
pub mod router;

pub use arms::{RouteArm, RouteArmResult, RouteEvalReport};
pub use capability::{
    ModelCapabilities, RequiredCapabilities, StructuredOutputSupport, ToolCallSupport,
};
pub use classify::{classify, Classification, TaskClass, TaskSignals, RULE_CLASSIFIER_VERSION};
pub use policy::{Lambdas, RoutingPolicy};
pub use profile::{
    EditProtocol, LocalBench, ModelExecutionProfile, ModelLocation, ModelPerformance, ModelProfile,
    SchemaRepairPolicy,
};
pub use router::{
    Router, RoutingDecision, RoutingError, RoutingTransition, SelectionReason, TaskNode,
};
