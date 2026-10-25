//! Session-scoped trust broker for Remote UI renderers and verified workers.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use codypendent_protocol::{
    ClientCapabilities, ClientId, ClientRole, MessageId, SessionId, UiActionCancellation,
    UiActionInvocation, UiActionResult, UiCapabilities, UiCapability, UiCapabilitySelection,
    UiClientKind, UiColorDepth, UiContributionPoint, UiDocumentId, UiEventId, UiHardLimits,
    UiMediaCapability, UiPrimitive, UiProjectionSubscription, UiProjectionUnsubscription,
    UiProjectionUpdate, UiProtocolVersion, UiRemoteError, UiSlotDefinition, UiSlotId, UiSnapshot,
    UiTheme, UiViewport, UiWireMessage,
};
use codypendent_ui_host::{
    DocumentStore, RegistrationTrust, UiHostSession, UiSessionUpdate, UiWorkerLaunch,
    VerifiedUiContribution,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::broadcast;
use uuid::Uuid;

const FANOUT_CAPACITY: usize = 256;
const QUARANTINE_AFTER: u8 = 3;
const REPLAY_ID_LIMIT: usize = 4_096;
const RENDERER_RATE_BURST: usize = 256;
const RENDERER_RATE_WINDOW: Duration = Duration::from_secs(1);
const MAX_SUBSCRIPTIONS_PER_PRODUCER: usize = 32;
pub const PUBLIC_POINTS: &[&str] = &[
    "sidebar",
    "panel",
    "status-item",
    "command",
    "command-palette",
    "composer-accessory",
    "message-renderer",
    "tool-renderer",
    "artifact-renderer",
    "workflow-inspector",
    "blackboard-renderer",
    "document-block",
    "code-graph-node",
    "settings-section",
    "setup-step",
    "form",
    "wizard",
    "dashboard-card",
    "trace-span-renderer",
    "context-menu",
    "quick-pick",
    "notification",
];

const CORE_POINTS: &[&str] = &[
    "approval-frame",
    "approval-actions",
    "secret-entry",
    "policy-state",
    "terminal-lifecycle",
];

/// Opaque authority minted only from a verified and enabled worker launch.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct UiProducerHandle {
    id: Uuid,
    plugin_id: String,
}

impl UiProducerHandle {
    #[must_use]
    pub fn plugin_id(&self) -> &str {
        &self.plugin_id
    }

    #[must_use]
    pub(crate) fn instance_id(&self) -> Uuid {
        self.id
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiBrokerTarget {
    AllRenderers,
    Renderer(ClientId),
    Producer(UiProducerHandle),
}

#[derive(Debug, Clone)]
pub struct UiBrokerFrame {
    pub target: UiBrokerTarget,
    pub message: UiWireMessage,
}

#[derive(Debug)]
pub struct UiBrokerSubscription {
    pub receiver: broadcast::Receiver<UiBrokerFrame>,
}

/// Renderer cardinality after a detach, including the concrete targets whose
/// final compatible renderer just left. `Shared` workers are session-wide and
/// should be stopped only when `remaining_total` reaches zero.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RendererDisconnect {
    pub remaining_total: usize,
    pub remaining_terminal: usize,
    pub remaining_web: usize,
    pub departed_targets: Vec<codypendent_sandbox::UiTarget>,
}

#[derive(Debug, Clone)]
pub struct UiMediatedAction {
    pub producer: UiProducerHandle,
    pub invocation: UiActionInvocation,
    pub requester: Option<(ClientId, ClientRole)>,
}

#[derive(Debug, Clone)]
pub struct UiMediatedSubscription {
    pub producer: UiProducerHandle,
    pub request: UiProjectionSubscription,
}

#[derive(Debug, Clone)]
pub struct UiMediatedUnsubscription {
    pub producer: UiProducerHandle,
    pub request: UiProjectionUnsubscription,
}

#[derive(Debug, Clone)]
pub struct UiMediatedCancellation {
    pub producer: UiProducerHandle,
    pub cancellation: UiActionCancellation,
}

#[derive(Debug, Default)]
pub struct UiBrokerDispatch {
    pub direct: Vec<UiWireMessage>,
    pub actions: Vec<UiMediatedAction>,
    pub subscriptions: Vec<UiMediatedSubscription>,
    pub unsubscriptions: Vec<UiMediatedUnsubscription>,
    pub cancellations: Vec<UiMediatedCancellation>,
    /// True only for the first accepted capabilities offer from a renderer.
    pub renderer_negotiated: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum UiBrokerError {
    #[error("remote UI renderer must attach to this session first")]
    RendererNotAttached,
    #[error("remote UI worker is not registered for this session")]
    UnknownProducer,
    #[error("remote UI producer is quarantined after repeated protocol violations")]
    Quarantined,
    #[error("remote UI renderer exceeded its bounded message rate")]
    RateLimited,
    #[error("remote UI producer exceeded aggregate quota: {0}")]
    Quota(String),
    #[error("remote UI extension attempted to use host-reserved decision/secret primitive {0}")]
    ReservedPrimitive(String),
    #[error("remote UI invocation id was replayed or collided: {0}")]
    InvocationCollision(String),
    #[error("client role {role:?} cannot send remote UI renderer message {kind}")]
    RendererDirection { role: ClientRole, kind: String },
    #[error("verified worker cannot send remote UI message {0} in this direction")]
    ProducerDirection(String),
    #[error("remote UI message does not own document or invocation {0}")]
    Ownership(String),
    #[error("remote UI action {0} was not declared by the verified worker")]
    UndeclaredAction(String),
    #[error("remote UI contribution {0} was not declared by the verified worker")]
    UndeclaredContribution(String),
    #[error("remote UI projection subscription is not authorized: {0}")]
    UnauthorizedSubscription(String),
    #[error("remote UI host rejected the message: {0}")]
    Host(String),
}

#[derive(Clone)]
pub struct RemoteUiBroker {
    sessions: Arc<Mutex<HashMap<SessionId, SessionBroker>>>,
    offer: UiCapabilities,
    limits: UiHardLimits,
}

struct SessionBroker {
    documents: DocumentStore,
    document_owners: HashMap<UiDocumentId, Uuid>,
    document_targets: HashMap<UiDocumentId, String>,
    renderers: HashMap<ClientId, RendererState>,
    producers: HashMap<Uuid, ProducerState>,
    sender: broadcast::Sender<UiBrokerFrame>,
    latest_theme: Option<UiWireMessage>,
    latest_contributions: HashMap<(Uuid, String), UiWireMessage>,
}

struct RendererState {
    host: UiHostSession,
    client_kind: Option<String>,
    role: Option<ClientRole>,
    viewport: Option<UiViewport>,
    capabilities_receipt: Option<(String, String)>,
    seen: HashSet<String>,
    seen_order: VecDeque<String>,
    recent_messages: VecDeque<Instant>,
}

struct ProducerState {
    handle: UiProducerHandle,
    host: UiHostSession,
    replacement_scope: String,
    target: String,
    publisher: String,
    trust_label: String,
    declared_capabilities: HashSet<String>,
    verified_contributions: HashMap<String, VerifiedUiContribution>,
    violations: u8,
    in_flight: HashSet<UiEventId>,
    subscriptions: HashMap<String, UiProjectionSubscription>,
    approved_commands: HashSet<String>,
    local_to_global_documents: HashMap<UiDocumentId, UiDocumentId>,
    global_to_local_documents: HashMap<UiDocumentId, UiDocumentId>,
    contribution_names:
        HashMap<codypendent_protocol::UiContributionId, codypendent_protocol::UiContributionId>,
    invocation_digests: HashMap<UiEventId, String>,
    invocation_order: VecDeque<UiEventId>,
    interaction_grants: HashMap<String, InteractionGrant>,
    projections: HashMap<String, UiWireMessage>,
}

struct InteractionGrant {
    requester: (ClientId, ClientRole),
    document_id: UiDocumentId,
    revision: codypendent_protocol::UiRevision,
    source_node_id: codypendent_protocol::UiNodeId,
    event_type: codypendent_protocol::UiEventType,
    permitted_actions: HashSet<String>,
    expires_at: Instant,
}

impl std::fmt::Debug for RemoteUiBroker {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RemoteUiBroker")
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl Default for RemoteUiBroker {
    fn default() -> Self {
        Self::new(daemon_offer(), UiHardLimits::default())
    }
}

impl RemoteUiBroker {
    #[must_use]
    pub fn new(offer: UiCapabilities, limits: UiHardLimits) -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            offer,
            limits,
        }
    }

    /// Canonical producer-facing offer used by the worker supervisor's single
    /// handshake. The broker is then seeded from the resulting selection.
    #[must_use]
    pub fn producer_offer(&self) -> UiCapabilities {
        self.offer.clone()
    }

    /// Attach one external renderer. Capability negotiation and message-id
    /// dedupe are private to this renderer and can never tighten another
    /// client's limits.
    pub fn subscribe_renderer(
        &self,
        session_id: SessionId,
        client_id: ClientId,
    ) -> Result<UiBrokerSubscription, UiBrokerError> {
        let mut sessions = self.sessions.lock().expect("remote UI broker poisoned");
        let session = self.session(&mut sessions, session_id)?;
        if let std::collections::hash_map::Entry::Vacant(renderer) =
            session.renderers.entry(client_id)
        {
            let host = new_host(&self.offer, self.limits)?;
            renderer.insert(RendererState {
                host,
                client_kind: None,
                role: None,
                viewport: None,
                capabilities_receipt: None,
                seen: HashSet::new(),
                seen_order: VecDeque::new(),
                recent_messages: VecDeque::new(),
            });
        }
        Ok(UiBrokerSubscription {
            receiver: session.sender.subscribe(),
        })
    }

    /// Return a point-in-time authoritative baseline for an attached renderer.
    /// This is also the broadcast-lag recovery path: the socket forwarder sends
    /// these messages directly, outside the lossy fan-out, so a slow renderer
    /// converges without having to guess which document lost a patch.
    pub fn renderer_baseline(
        &self,
        session_id: SessionId,
        client_id: ClientId,
    ) -> Result<Vec<UiWireMessage>, UiBrokerError> {
        let sessions = self.sessions.lock().expect("remote UI broker poisoned");
        let session = sessions
            .get(&session_id)
            .ok_or(UiBrokerError::RendererNotAttached)?;
        renderer_baseline_messages(session, client_id)
    }

    /// Register a component producer only after the sandbox layer constructed a
    /// verified UiWorkerLaunch. Socket roles cannot mint this handle.
    pub fn register_verified_producer(
        &self,
        session_id: SessionId,
        launch: &UiWorkerLaunch,
        selection: &UiCapabilitySelection,
    ) -> Result<UiProducerHandle, UiBrokerError> {
        let mut sessions = self.sessions.lock().expect("remote UI broker poisoned");
        let session = self.session(&mut sessions, session_id)?;
        let handle = UiProducerHandle {
            id: Uuid::now_v7(),
            plugin_id: launch.plugin_id().to_owned(),
        };
        let mut host = new_host(&self.offer, self.limits)?;
        host.handle(
            ui_message(
                "capabilitySelection",
                format!("producer-selection:{}", handle.id),
                |wire| wire.selection = Some(selection.clone()),
            ),
            RegistrationTrust::Extension,
        )
        .map_err(host_error)?;
        session.producers.insert(
            handle.id,
            ProducerState {
                handle: handle.clone(),
                host,
                replacement_scope: format!("ui-producer:{}", handle.id),
                target: format!("{:?}", launch.target()).to_ascii_lowercase(),
                publisher: launch.publisher().to_owned(),
                trust_label: if launch.is_signed() {
                    "signed".to_owned()
                } else {
                    "approved unsigned".to_owned()
                },
                declared_capabilities: launch.declared_capabilities().clone(),
                verified_contributions: launch.verified_contributions().clone(),
                violations: 0,
                in_flight: HashSet::new(),
                subscriptions: HashMap::new(),
                approved_commands: HashSet::new(),
                local_to_global_documents: HashMap::new(),
                global_to_local_documents: HashMap::new(),
                contribution_names: HashMap::new(),
                invocation_digests: HashMap::new(),
                invocation_order: VecDeque::new(),
                interaction_grants: HashMap::new(),
                projections: HashMap::new(),
            },
        );
        Ok(handle)
    }

    pub fn subscribe_producer(
        &self,
        session_id: SessionId,
        producer: &UiProducerHandle,
    ) -> Result<UiBrokerSubscription, UiBrokerError> {
        let mut sessions = self.sessions.lock().expect("remote UI broker poisoned");
        let session = self.session(&mut sessions, session_id)?;
        let Some(state) = session.producers.get(&producer.id) else {
            return Err(UiBrokerError::UnknownProducer);
        };
        let target = state.target.clone();
        let receiver = session.sender.subscribe();
        if let Some(viewport) = aggregate_viewport(session, &target) {
            publish(
                session,
                UiBrokerTarget::Producer(producer.clone()),
                viewport_message(viewport),
            );
        }
        if let Some(theme) = &session.latest_theme {
            publish(
                session,
                UiBrokerTarget::Producer(producer.clone()),
                theme.clone(),
            );
        }
        Ok(UiBrokerSubscription { receiver })
    }

    pub fn handle_renderer(
        &self,
        session_id: SessionId,
        origin: ClientId,
        role: ClientRole,
        message: UiWireMessage,
    ) -> Result<UiBrokerDispatch, UiBrokerError> {
        message.validate(&self.limits).map_err(host_error)?;
        if !renderer_direction_allowed(role, &message.kind) {
            return Err(UiBrokerError::RendererDirection {
                role,
                kind: message.kind,
            });
        }
        let mut sessions = self.sessions.lock().expect("remote UI broker poisoned");
        let session = self.session(&mut sessions, session_id)?;
        let renderer = session
            .renderers
            .get_mut(&origin)
            .ok_or(UiBrokerError::RendererNotAttached)?;
        renderer.role = Some(role);
        enforce_renderer_rate(renderer)?;
        let incoming = message.clone();
        if incoming.kind == "capabilities" {
            let digest = capabilities_digest(
                incoming
                    .capabilities
                    .as_ref()
                    .expect("validated capabilities message"),
            );
            if let Some((accepted_id, accepted_digest)) = &renderer.capabilities_receipt {
                if accepted_id == &incoming.message_id && accepted_digest != &digest {
                    return Err(UiBrokerError::RendererDirection {
                        role,
                        kind: "capabilities-collision".to_owned(),
                    });
                }
                let accepted = renderer
                    .host
                    .client_capabilities()
                    .ok_or(UiBrokerError::RendererNotAttached)?;
                let mut contract = incoming
                    .capabilities
                    .as_ref()
                    .expect("validated capabilities")
                    .clone();
                contract.viewport = accepted.viewport;
                if capabilities_digest(&contract) != *accepted_digest {
                    return Err(UiBrokerError::RendererDirection {
                        role,
                        kind: "capabilities-renegotiation".to_owned(),
                    });
                }
                let selection = renderer
                    .host
                    .selection()
                    .cloned()
                    .ok_or(UiBrokerError::RendererNotAttached)?;
                renderer.viewport = incoming
                    .capabilities
                    .as_ref()
                    .map(|capabilities| capabilities.viewport);
                let mut dispatch = UiBrokerDispatch::default();
                dispatch.direct.push(ui_message(
                    "capabilitySelection",
                    format!("selection:{}", incoming.message_id),
                    |wire| wire.selection = Some(selection),
                ));
                append_renderer_baseline(session, origin, &mut dispatch.direct)?;
                publish_aggregate_viewports(session);
                return Ok(dispatch);
            }
            renderer.capabilities_receipt = Some((incoming.message_id.clone(), digest));
            renderer.client_kind = incoming
                .capabilities
                .as_ref()
                .map(|capabilities| capabilities.client.as_str().to_owned());
            renderer.viewport = incoming
                .capabilities
                .as_ref()
                .map(|capabilities| capabilities.viewport);
        } else if incoming.kind == "viewport" {
            renderer.viewport = incoming.viewport;
        }
        if incoming.kind == "event" {
            if !remember_renderer_message(renderer, incoming.message_id.clone()) {
                return Ok(UiBrokerDispatch::default());
            }
            let event = incoming.event.as_ref().expect("validated event message");
            let invocation = renderer
                .host
                .documents()
                .validate_event(event)
                .map_err(host_error)?;
            let producer_id = session
                .document_owners
                .get(&event.document_id)
                .copied()
                .ok_or_else(|| UiBrokerError::Ownership(event.document_id.to_string()))?;
            let producer = session
                .producers
                .get_mut(&producer_id)
                .ok_or(UiBrokerError::UnknownProducer)?;
            let producer_handle = producer.handle.clone();
            let local_document_id = producer
                .global_to_local_documents
                .get(&event.document_id)
                .cloned()
                .ok_or_else(|| UiBrokerError::Ownership(event.document_id.to_string()))?;
            let mut owner_message = incoming.clone();
            owner_message
                .event
                .as_mut()
                .expect("validated event")
                .document_id = local_document_id.clone();
            // Renderer input can never supply or replay host authority.
            owner_message
                .event
                .as_mut()
                .expect("validated event")
                .interaction_token = None;
            let mut dispatch = UiBrokerDispatch::default();
            if let Some(invocation) = invocation {
                authorize_command_invocation(producer, &invocation)?;
                if !remember_invocation(producer, &invocation)? {
                    return Ok(UiBrokerDispatch::default());
                }
                enforce_in_flight_quota(producer, self.limits)?;
                if !producer.in_flight.insert(invocation.invocation_id.clone()) {
                    return Err(UiBrokerError::Ownership(
                        invocation.invocation_id.to_string(),
                    ));
                }
                dispatch.actions.push(UiMediatedAction {
                    producer: producer_handle.clone(),
                    invocation,
                    requester: Some((origin, role)),
                });
            } else if interaction_event_can_authorize_command(event.event_type.as_str()) {
                let token = Uuid::now_v7().to_string();
                producer.interaction_grants.insert(
                    token.clone(),
                    InteractionGrant {
                        requester: (origin, role),
                        document_id: local_document_id,
                        revision: event.revision,
                        source_node_id: event.target_id.clone(),
                        event_type: event.event_type.clone(),
                        permitted_actions: producer.approved_commands.clone(),
                        expires_at: Instant::now() + Duration::from_secs(5),
                    },
                );
                owner_message
                    .event
                    .as_mut()
                    .expect("validated event")
                    .interaction_token = Some(token);
                trim_interaction_grants(&mut producer.interaction_grants);
            }
            publish(
                session,
                UiBrokerTarget::Producer(producer_handle),
                owner_message,
            );
            return Ok(dispatch);
        }
        let update = renderer
            .host
            .handle(message, RegistrationTrust::Extension)
            .map_err(host_error)?;

        let mut dispatch = UiBrokerDispatch::default();
        match update {
            UiSessionUpdate::Negotiated(selection) => {
                dispatch.renderer_negotiated = true;
                dispatch.direct.push(ui_message(
                    "capabilitySelection",
                    format!("selection:{}", incoming.message_id),
                    |wire| wire.selection = Some(selection),
                ));
                append_renderer_baseline(session, origin, &mut dispatch.direct)?;
            }
            UiSessionUpdate::ResyncRequested(request) => {
                let renderer = session
                    .renderers
                    .get(&origin)
                    .ok_or(UiBrokerError::RendererNotAttached)?;
                let registered = renderer
                    .host
                    .registry()
                    .mounted_document_ids()
                    .contains(request.document_id.as_str());
                let target_visible = session
                    .document_targets
                    .get(&request.document_id)
                    .is_some_and(|target| target_matches(target, renderer.client_kind.as_deref()));
                if registered && target_visible {
                    if let Some(document) = session.documents.document(&request.document_id) {
                        dispatch
                            .direct
                            .push(snapshot_message(document.clone(), "resync"));
                    }
                } else {
                    let revision = request
                        .known_revision
                        .or_else(|| {
                            session
                                .documents
                                .document(&request.document_id)
                                .map(|document| document.revision)
                        })
                        .unwrap_or(codypendent_protocol::UiRevision(0));
                    dispatch.direct.push(ui_message(
                        "dispose",
                        format!("hidden-resync:{}:{}", request.document_id, revision.0),
                        |wire| {
                            wire.dispose = Some(codypendent_protocol::UiDispose {
                                document_id: request.document_id.clone(),
                                revision,
                            });
                        },
                    ));
                }
            }
            UiSessionUpdate::ViewportChanged(_)
            | UiSessionUpdate::DuplicateIgnored { .. }
            | UiSessionUpdate::RemoteError(_) => {}
            _ => {
                return Err(UiBrokerError::RendererDirection {
                    role,
                    kind: incoming.kind,
                });
            }
        }
        if matches!(incoming.kind.as_str(), "capabilities" | "viewport") {
            publish_aggregate_viewports(session);
        }
        Ok(dispatch)
    }

    pub fn handle_producer(
        &self,
        session_id: SessionId,
        producer: &UiProducerHandle,
        mut message: UiWireMessage,
    ) -> Result<UiBrokerDispatch, UiBrokerError> {
        let mut sessions = self.sessions.lock().expect("remote UI broker poisoned");
        let session = self.session(&mut sessions, session_id)?;
        let producer_state = session
            .producers
            .get_mut(&producer.id)
            .ok_or(UiBrokerError::UnknownProducer)?;
        if producer_state.violations >= QUARANTINE_AFTER {
            return Err(UiBrokerError::Quarantined);
        }
        if !producer_direction_allowed(&message.kind) {
            producer_state.violations = producer_state.violations.saturating_add(1);
            return Err(UiBrokerError::ProducerDirection(message.kind));
        }

        message.message_id = format!("producer:{}:{}", producer.id, message.message_id);
        if let Some(snapshot) = &message.snapshot {
            ensure_document_namespace(
                session
                    .producers
                    .get_mut(&producer.id)
                    .expect("producer checked"),
                &snapshot.document.document_id,
            );
        }
        validate_producer_message(session, producer, &message)?;
        filter_contributions_for_launch_target(
            session
                .producers
                .get(&producer.id)
                .expect("producer checked"),
            &mut message,
        );
        let incoming = {
            let state = session
                .producers
                .get_mut(&producer.id)
                .expect("producer checked");
            namespace_producer_message(state, &message)?
        };
        preflight_authoritative_state(session, producer, &incoming, self.limits)?;
        let empty_contribution_replacement =
            incoming.kind == "contributions" && incoming.contributions.is_empty();
        let mut next_producer_host = session
            .producers
            .get(&producer.id)
            .expect("producer checked")
            .host
            .clone();
        let update = if empty_contribution_replacement {
            next_producer_host
                .registry_mut()
                .unregister_extension(producer.plugin_id());
            UiSessionUpdate::ContributionsChanged(Vec::new())
        } else {
            next_producer_host
                .handle(message, RegistrationTrust::Extension)
                .map_err(host_error)?
        };

        let mut dispatch = UiBrokerDispatch::default();
        match update {
            UiSessionUpdate::Negotiated(selection) => {
                dispatch.direct.push(ui_message(
                    "capabilitySelection",
                    format!("selection:{}", incoming.message_id),
                    |wire| wire.selection = Some(selection),
                ));
            }
            UiSessionUpdate::SnapshotMounted { .. } => {
                session
                    .producers
                    .get_mut(&producer.id)
                    .expect("producer checked")
                    .interaction_grants
                    .clear();
                let document = incoming
                    .snapshot
                    .as_ref()
                    .expect("snapshot update")
                    .document
                    .clone();
                let visible = document_is_contributed(
                    session,
                    &incoming
                        .snapshot
                        .as_ref()
                        .expect("snapshot update")
                        .document
                        .document_id,
                );
                if visible {
                    apply_to_renderers(session, producer.id, &incoming)?;
                }
                session
                    .documents
                    .mount(document.clone())
                    .map_err(|error| UiBrokerError::Host(error.to_string()))?;
                session
                    .document_owners
                    .insert(document.document_id, producer.id);
                session.document_targets.insert(
                    incoming
                        .snapshot
                        .as_ref()
                        .expect("snapshot update")
                        .document
                        .document_id
                        .clone(),
                    session
                        .producers
                        .get(&producer.id)
                        .expect("producer checked")
                        .target
                        .clone(),
                );
                enforce_aggregate_document_quota(session, producer.id, self.limits)?;
                if visible {
                    publish_to_compatible_renderers(session, producer.id, incoming);
                }
            }
            UiSessionUpdate::PatchApplied { .. } => {
                session
                    .producers
                    .get_mut(&producer.id)
                    .expect("producer checked")
                    .interaction_grants
                    .clear();
                let visible = document_is_contributed(
                    session,
                    &incoming
                        .patch_batch
                        .as_ref()
                        .expect("patch update")
                        .document_id,
                );
                if visible {
                    apply_to_renderers(session, producer.id, &incoming)?;
                }
                session
                    .documents
                    .apply(incoming.patch_batch.as_ref().expect("patch update"))
                    .map_err(|error| UiBrokerError::Host(error.to_string()))?;
                enforce_aggregate_document_quota(session, producer.id, self.limits)?;
                if visible {
                    publish_to_compatible_renderers(session, producer.id, incoming);
                }
            }
            UiSessionUpdate::DocumentDisposed { .. } => {
                session
                    .producers
                    .get_mut(&producer.id)
                    .expect("producer checked")
                    .interaction_grants
                    .clear();
                let document_id = incoming
                    .dispose
                    .as_ref()
                    .expect("dispose update")
                    .document_id
                    .clone();
                apply_to_renderers(session, producer.id, &incoming)?;
                session.documents.remove(&document_id);
                session.document_owners.remove(&document_id);
                session.document_targets.remove(&document_id);
                if let Some(local) = session
                    .producers
                    .get_mut(&producer.id)
                    .expect("producer checked")
                    .global_to_local_documents
                    .remove(&document_id)
                {
                    session
                        .producers
                        .get_mut(&producer.id)
                        .expect("producer checked")
                        .local_to_global_documents
                        .remove(&local);
                }
                publish_to_compatible_renderers(session, producer.id, incoming);
            }
            UiSessionUpdate::ContributionsChanged(_) => {
                let producer_target = session
                    .producers
                    .get(&producer.id)
                    .expect("producer checked")
                    .target
                    .clone();
                let route = (producer.id, producer_target.clone());
                let previous_documents = session
                    .latest_contributions
                    .get(&route)
                    .map_or_else(HashSet::new, contribution_documents);
                let next_documents = contribution_documents(&incoming);
                // Registration is published before its snapshot. Renderers can
                // remember an attested placement without mounting anything;
                // the following snapshot then becomes visible in that slot.
                // This avoids ever displaying an orphan through a default slot.
                let mut renderer_updates = vec![incoming.clone()];
                let mut reveal: Vec<_> = next_documents
                    .difference(&previous_documents)
                    .filter_map(|document_id| session.documents.document(document_id).cloned())
                    .map(|document| snapshot_message(document, "contribution-reveal"))
                    .collect();
                reveal.sort_by(|left, right| left.message_id.cmp(&right.message_id));
                renderer_updates.extend(reveal);
                let mut hidden: Vec<_> = previous_documents
                    .difference(&next_documents)
                    .filter_map(|document_id| {
                        let document = session.documents.document(document_id)?;
                        Some(ui_message(
                            "dispose",
                            format!("contribution-hide:{}:{}", document_id, document.revision.0),
                            |wire| {
                                wire.dispose = Some(codypendent_protocol::UiDispose {
                                    document_id: document_id.clone(),
                                    revision: document.revision,
                                });
                            },
                        ))
                    })
                    .collect();
                hidden.sort_by(|left, right| left.message_id.cmp(&right.message_id));
                renderer_updates.extend(hidden);
                apply_messages_to_renderers_for_target(
                    session,
                    &producer_target,
                    &renderer_updates,
                )?;
                if incoming.contributions.is_empty() {
                    session.latest_contributions.remove(&route);
                } else {
                    session.latest_contributions.insert(route, incoming.clone());
                }
                for update in renderer_updates {
                    publish_for_target(session, &producer_target, update);
                }
            }
            UiSessionUpdate::ThemeChanged(_) => {
                apply_to_renderers(session, producer.id, &incoming)?;
                session.latest_theme = Some(incoming.clone());
                publish_to_compatible_renderers(session, producer.id, incoming);
            }
            UiSessionUpdate::Action(mut invocation) => {
                let state = session
                    .producers
                    .get_mut(&producer.id)
                    .expect("producer checked");
                // Consume before any semantic/action check so a failed probe
                // cannot reuse a real user gesture with another command.
                let requester =
                    match consume_interaction_grant(state, &mut invocation).and_then(|requester| {
                        authorize_command_invocation(state, &invocation).map(|()| requester)
                    }) {
                        Ok(requester) => requester,
                        Err(error) => {
                            state.host = next_producer_host;
                            state.violations = state.violations.saturating_add(1);
                            dispatch
                                .direct
                                .push(action_denied_message(&invocation, &error));
                            return Ok(dispatch);
                        }
                    };
                if !remember_invocation(state, &invocation)? {
                    state.host = next_producer_host;
                    state.violations = 0;
                    return Ok(dispatch);
                }
                enforce_in_flight_quota(state, self.limits)?;
                if !state.in_flight.insert(invocation.invocation_id.clone()) {
                    return Err(UiBrokerError::Ownership(
                        invocation.invocation_id.to_string(),
                    ));
                }
                dispatch.actions.push(UiMediatedAction {
                    producer: producer.clone(),
                    requester: Some(requester),
                    invocation,
                });
            }
            UiSessionUpdate::SubscriptionRequested(request) => {
                let state = session
                    .producers
                    .get_mut(&producer.id)
                    .expect("producer checked");
                authorize_subscription(state, &request)?;
                if state.subscriptions.contains_key(&request.subscription_id) {
                    return Err(UiBrokerError::UnauthorizedSubscription(format!(
                        "duplicate subscription id {}",
                        request.subscription_id
                    )));
                }
                let duplicate_semantics = state.subscriptions.values().any(|current| {
                    current.kind == request.kind
                        && current.resource_id == request.resource_id
                        && current.parameters == request.parameters
                });
                if duplicate_semantics {
                    return Err(UiBrokerError::UnauthorizedSubscription(
                        "duplicate projection subscription".to_owned(),
                    ));
                }
                if state.subscriptions.len() >= MAX_SUBSCRIPTIONS_PER_PRODUCER {
                    state.violations = QUARANTINE_AFTER;
                    return Err(UiBrokerError::Quota("projection subscriptions".to_owned()));
                }
                state
                    .subscriptions
                    .insert(request.subscription_id.clone(), request.clone());
                dispatch.subscriptions.push(UiMediatedSubscription {
                    producer: producer.clone(),
                    request,
                });
            }
            UiSessionUpdate::SubscriptionCancelled(request) => {
                let state = session
                    .producers
                    .get_mut(&producer.id)
                    .expect("producer checked");
                let subscription = state
                    .subscriptions
                    .remove(&request.subscription_id)
                    .ok_or_else(|| {
                        UiBrokerError::UnauthorizedSubscription(request.subscription_id.clone())
                    })?;
                state.projections.remove(&request.subscription_id);
                if subscription.kind == "command" {
                    if let Some(command_id) = subscription.resource_id {
                        state.approved_commands.remove(&command_id);
                    }
                }
                dispatch.unsubscriptions.push(UiMediatedUnsubscription {
                    producer: producer.clone(),
                    request,
                });
            }
            UiSessionUpdate::ActionCancelled(cancellation) => {
                let state = session
                    .producers
                    .get_mut(&producer.id)
                    .expect("producer checked");
                let active = if state.in_flight.remove(&cancellation.invocation_id) {
                    true
                } else if state
                    .invocation_digests
                    .contains_key(&cancellation.invocation_id)
                {
                    false
                } else {
                    return Err(UiBrokerError::Ownership(
                        cancellation.invocation_id.to_string(),
                    ));
                };
                if active {
                    dispatch.cancellations.push(UiMediatedCancellation {
                        producer: producer.clone(),
                        cancellation,
                    });
                }
            }
            UiSessionUpdate::HotReload(_) | UiSessionUpdate::RemoteError(_) => {}
            UiSessionUpdate::DuplicateIgnored { .. } => {}
            _ => return Err(UiBrokerError::ProducerDirection(incoming.kind)),
        }
        let state = session
            .producers
            .get_mut(&producer.id)
            .expect("producer checked");
        state.host = next_producer_host;
        state.violations = 0;
        Ok(dispatch)
    }

    /// Point-to-point projection delivery after the daemon authorized and read
    /// the requested projection through its existing stores.
    pub fn deliver_projection(
        &self,
        session_id: SessionId,
        producer: &UiProducerHandle,
        projection: UiProjectionUpdate,
    ) -> Result<(), UiBrokerError> {
        let mut sessions = self.sessions.lock().expect("remote UI broker poisoned");
        let session = self.session(&mut sessions, session_id)?;
        let state = session
            .producers
            .get_mut(&producer.id)
            .ok_or(UiBrokerError::UnknownProducer)?;
        let subscription = state
            .subscriptions
            .get(&projection.subscription_id)
            .ok_or_else(|| {
                UiBrokerError::UnauthorizedSubscription(projection.subscription_id.clone())
            })?;
        if subscription.kind == "command" {
            if let Some(command_id) = subscription.resource_id.as_deref() {
                let projected_id = projection
                    .value
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or(command_id);
                let enabled =
                    projection.value.get("enabled").and_then(Value::as_bool) == Some(true);
                if !projection.removed && enabled && projected_id == command_id {
                    state.approved_commands.insert(command_id.to_owned());
                } else {
                    state.approved_commands.remove(command_id);
                }
            }
        }
        let message = ui_message(
            "projection",
            format!("projection:{}:{}", producer.id, MessageId::new()),
            |wire| wire.projection = Some(projection.clone()),
        );
        state
            .projections
            .insert(projection.subscription_id.clone(), message.clone());
        publish(session, UiBrokerTarget::Producer(producer.clone()), message);
        Ok(())
    }

    pub fn settle_action(
        &self,
        session_id: SessionId,
        producer: &UiProducerHandle,
        result: UiActionResult,
    ) -> Result<(), UiBrokerError> {
        let mut sessions = self.sessions.lock().expect("remote UI broker poisoned");
        let session = self.session(&mut sessions, session_id)?;
        if !take_in_flight_or_known(session, producer, &result.invocation_id)? {
            return Ok(());
        }
        publish(
            session,
            UiBrokerTarget::Producer(producer.clone()),
            ui_message(
                "actionResult",
                format!("result:{}:{}", producer.id, MessageId::new()),
                |wire| wire.action_result = Some(result),
            ),
        );
        Ok(())
    }

    pub fn cancel_action(
        &self,
        session_id: SessionId,
        producer: &UiProducerHandle,
        cancellation: UiActionCancellation,
    ) -> Result<(), UiBrokerError> {
        let mut sessions = self.sessions.lock().expect("remote UI broker poisoned");
        let session = self.session(&mut sessions, session_id)?;
        if !take_in_flight_or_known(session, producer, &cancellation.invocation_id)? {
            return Ok(());
        }
        publish(
            session,
            UiBrokerTarget::Producer(producer.clone()),
            ui_message(
                "cancelAction",
                format!("cancel:{}:{}", producer.id, MessageId::new()),
                |wire| wire.cancellation = Some(cancellation),
            ),
        );
        Ok(())
    }

    /// Detach a renderer and return both session and concrete-target
    /// cardinality so lifecycle code can stop only the worker instances that
    /// no longer have a compatible renderer.
    pub fn disconnect_renderer(
        &self,
        session_id: SessionId,
        client_id: ClientId,
    ) -> RendererDisconnect {
        let mut sessions = self.sessions.lock().expect("remote UI broker poisoned");
        if let Some(session) = sessions.get_mut(&session_id) {
            let before = renderer_target_counts(session);
            session.renderers.remove(&client_id);
            for producer in session.producers.values_mut() {
                producer
                    .interaction_grants
                    .retain(|_, grant| grant.requester.0 != client_id);
            }
            publish_aggregate_viewports(session);
            let after = renderer_target_counts(session);
            let mut departed_targets = Vec::with_capacity(2);
            if before.0 > 0 && after.0 == 0 {
                departed_targets.push(codypendent_sandbox::UiTarget::Terminal);
            }
            if before.1 > 0 && after.1 == 0 {
                departed_targets.push(codypendent_sandbox::UiTarget::Web);
            }
            RendererDisconnect {
                remaining_total: session.renderers.len(),
                remaining_terminal: after.0,
                remaining_web: after.1,
                departed_targets,
            }
        } else {
            RendererDisconnect {
                remaining_total: 0,
                remaining_terminal: 0,
                remaining_web: 0,
                departed_targets: Vec::new(),
            }
        }
    }

    /// Install a trusted host theme and project it to workers. Extension
    /// producers cannot call this path or publish a session-global theme.
    pub fn set_host_theme(
        &self,
        session_id: SessionId,
        theme: UiTheme,
    ) -> Result<(), UiBrokerError> {
        let mut sessions = self.sessions.lock().expect("remote UI broker poisoned");
        let session = self.session(&mut sessions, session_id)?;
        let message = theme_message(theme);
        session.latest_theme = Some(message.clone());
        let producers: Vec<_> = session
            .producers
            .values()
            .map(|state| state.handle.clone())
            .collect();
        for producer in producers {
            publish(session, UiBrokerTarget::Producer(producer), message.clone());
        }
        Ok(())
    }

    #[must_use]
    pub fn renderer_count(&self, session_id: SessionId) -> usize {
        self.sessions
            .lock()
            .expect("remote UI broker poisoned")
            .get(&session_id)
            .map_or(0, |session| session.renderers.len())
    }

    /// Negotiated concrete renderer targets currently attached to a session.
    /// Unknown/pre-handshake renderers are omitted and the result is stable.
    #[must_use]
    pub fn renderer_targets(&self, session_id: SessionId) -> Vec<codypendent_sandbox::UiTarget> {
        let sessions = self.sessions.lock().expect("remote UI broker poisoned");
        let Some(session) = sessions.get(&session_id) else {
            return Vec::new();
        };
        let terminal = session
            .renderers
            .values()
            .any(|renderer| renderer.client_kind.as_deref() == Some("terminal"));
        let web = session.renderers.values().any(|renderer| {
            renderer
                .client_kind
                .as_deref()
                .is_some_and(|kind| kind != "terminal")
        });
        let mut targets = Vec::with_capacity(2);
        if terminal {
            targets.push(codypendent_sandbox::UiTarget::Terminal);
        }
        if web {
            targets.push(codypendent_sandbox::UiTarget::Web);
        }
        targets
    }

    /// Concrete targets across every active renderer session, used to refresh
    /// a globally enabled verified plugin immediately.
    #[must_use]
    pub fn renderer_session_targets(&self) -> Vec<(SessionId, codypendent_sandbox::UiTarget)> {
        let sessions = self.sessions.lock().expect("remote UI broker poisoned");
        let mut targets = Vec::new();
        for (session_id, session) in sessions.iter() {
            let terminal = session
                .renderers
                .values()
                .any(|renderer| renderer.client_kind.as_deref() == Some("terminal"));
            let web = session.renderers.values().any(|renderer| {
                renderer
                    .client_kind
                    .as_deref()
                    .is_some_and(|kind| kind != "terminal")
            });
            if terminal {
                targets.push((*session_id, codypendent_sandbox::UiTarget::Terminal));
            }
            if web {
                targets.push((*session_id, codypendent_sandbox::UiTarget::Web));
            }
        }
        targets.sort_by(|left, right| {
            left.0
                .to_string()
                .cmp(&right.0.to_string())
                .then_with(|| format!("{:?}", left.1).cmp(&format!("{:?}", right.1)))
        });
        targets
    }

    pub fn dispose_producer(
        &self,
        session_id: SessionId,
        producer: &UiProducerHandle,
    ) -> Result<(), UiBrokerError> {
        let mut sessions = self.sessions.lock().expect("remote UI broker poisoned");
        let session = self.session(&mut sessions, session_id)?;
        dispose_producer_in_session(session, producer)
    }

    /// Remove every broker authority for `plugin_id` before asynchronous
    /// worker termination begins. Late frames are rejected as unknown even if
    /// an OS process is still draining its output pipe.
    pub fn revoke_plugin(&self, plugin_id: &str) -> Result<usize, UiBrokerError> {
        let mut sessions = self.sessions.lock().expect("remote UI broker poisoned");
        let mut revoked = 0usize;
        let mut first_error = None;
        for session in sessions.values_mut() {
            let producers: Vec<_> = session
                .producers
                .values()
                .filter(|producer| producer.handle.plugin_id() == plugin_id)
                .map(|producer| producer.handle.clone())
                .collect();
            for producer in producers {
                revoked = revoked.saturating_add(1);
                if let Err(error) = dispose_producer_in_session(session, &producer) {
                    first_error.get_or_insert(error);
                }
            }
        }
        first_error.map_or(Ok(revoked), Err)
    }

    fn session<'a>(
        &self,
        sessions: &'a mut HashMap<SessionId, SessionBroker>,
        session_id: SessionId,
    ) -> Result<&'a mut SessionBroker, UiBrokerError> {
        if let std::collections::hash_map::Entry::Vacant(entry) = sessions.entry(session_id) {
            let (sender, _) = broadcast::channel(FANOUT_CAPACITY);
            entry.insert(SessionBroker {
                documents: DocumentStore::new(self.limits),
                document_owners: HashMap::new(),
                document_targets: HashMap::new(),
                renderers: HashMap::new(),
                producers: HashMap::new(),
                sender,
                latest_theme: Some(theme_message(default_host_theme())),
                latest_contributions: HashMap::new(),
            });
        }
        Ok(sessions.get_mut(&session_id).expect("session inserted"))
    }
}

fn dispose_producer_in_session(
    session: &mut SessionBroker,
    producer: &UiProducerHandle,
) -> Result<(), UiBrokerError> {
    let removed = session
        .producers
        .remove(&producer.id)
        .ok_or(UiBrokerError::UnknownProducer)?;
    let visible_documents: HashSet<_> = session
        .latest_contributions
        .iter()
        .filter(|((owner, _), _)| *owner == producer.id)
        .flat_map(|(_, message)| contribution_documents(message))
        .collect();
    let mut owned_routes = session
        .latest_contributions
        .keys()
        .filter(|(owner, _)| *owner == producer.id)
        .map(|(_, target)| target.clone())
        .collect::<HashSet<_>>();
    owned_routes.extend(
        session
            .document_targets
            .iter()
            .filter(|(document_id, _)| {
                session.document_owners.get(*document_id) == Some(&producer.id)
            })
            .map(|(_, target)| target.clone()),
    );
    session
        .latest_contributions
        .retain(|(owner, _), _| *owner != producer.id);
    for route in &owned_routes {
        let mut unregister = ui_message(
            "contributions",
            format!("producer-unregister:{}:{}", producer.id, MessageId::new()),
            |_| {},
        );
        unregister.extensions.insert(
            "contributionOwner".to_owned(),
            Value::String(removed.replacement_scope.clone()),
        );
        apply_to_renderers_for_target(session, route, &unregister)?;
        publish_for_target(session, route, unregister);
    }
    let documents: Vec<_> = session
        .document_owners
        .iter()
        .filter(|(_, owner)| **owner == producer.id)
        .map(|(document_id, _)| {
            (
                document_id.clone(),
                session
                    .document_targets
                    .get(document_id)
                    .cloned()
                    .unwrap_or_else(|| removed.target.clone()),
            )
        })
        .collect();
    for (document_id, document_target) in documents {
        if let Some(document) = session.documents.remove(&document_id) {
            session.document_owners.remove(&document_id);
            session.document_targets.remove(&document_id);
            let dispose = ui_message(
                "dispose",
                format!("dispose:{}:{}", producer.id, document.revision.0),
                |wire| {
                    wire.dispose = Some(codypendent_protocol::UiDispose {
                        document_id: document_id.clone(),
                        revision: document.revision,
                    });
                },
            );
            if visible_documents.contains(&document_id) {
                apply_to_renderers_for_target(session, &document_target, &dispose)?;
                publish_for_target(session, &document_target, dispose);
            }
        }
    }
    Ok(())
}

fn renderer_direction_allowed(role: ClientRole, kind: &str) -> bool {
    if matches!(role, ClientRole::Unknown) {
        return false;
    }
    if kind == "event" && matches!(role, ClientRole::Observer) {
        return false;
    }
    matches!(
        kind,
        "capabilities" | "event" | "viewport" | "resync" | "error"
    )
}

fn producer_direction_allowed(kind: &str) -> bool {
    matches!(
        kind,
        "capabilities"
            | "snapshot"
            | "patchBatch"
            | "action"
            | "subscription"
            | "unsubscribe"
            | "cancelAction"
            | "dispose"
            | "hotReload"
            | "contributions"
            | "error"
    )
}

fn validate_producer_message(
    session: &mut SessionBroker,
    producer: &UiProducerHandle,
    message: &UiWireMessage,
) -> Result<(), UiBrokerError> {
    let state = session
        .producers
        .get(&producer.id)
        .ok_or(UiBrokerError::UnknownProducer)?;
    let owner = |document_id: &UiDocumentId| {
        state
            .local_to_global_documents
            .get(document_id)
            .and_then(|global| session.document_owners.get(global))
            .copied()
    };
    if let Some(snapshot) = &message.snapshot {
        if owner(&snapshot.document.document_id).is_some_and(|owner| owner != producer.id) {
            return Err(UiBrokerError::Ownership(
                snapshot.document.document_id.to_string(),
            ));
        }
    }
    if let Some(dispose) = &message.dispose {
        if let Some(global) = state.local_to_global_documents.get(&dispose.document_id) {
            if document_is_contributed(session, global) {
                return Err(UiBrokerError::Ownership(format!(
                    "document {} is still mounted by a contribution",
                    dispose.document_id
                )));
            }
        }
    }
    let document_id = message
        .patch_batch
        .as_ref()
        .map(|batch| &batch.document_id)
        .or_else(|| message.dispose.as_ref().map(|dispose| &dispose.document_id))
        .or_else(|| message.action.as_ref().map(|action| &action.document_id));
    if let Some(document_id) = document_id {
        if owner(document_id) != Some(producer.id) {
            return Err(UiBrokerError::Ownership(document_id.to_string()));
        }
    }
    if message.kind == "contributions" {
        let owner = message
            .extensions
            .get("contributionOwner")
            .and_then(Value::as_str);
        if owner != Some(producer.plugin_id()) {
            return Err(UiBrokerError::UndeclaredContribution(
                "unauthenticated contribution replacement owner".to_owned(),
            ));
        }
    }
    for contribution in &message.contributions {
        let expected = state
            .verified_contributions
            .get(contribution.id.as_str())
            .ok_or_else(|| UiBrokerError::UndeclaredContribution(contribution.id.to_string()))?;
        let renderer = contribution
            .metadata
            .get("renderer")
            .and_then(Value::as_str);
        if expected.point != contribution.point.as_str()
            || expected.applicable_slot != contribution.slot.as_str()
            || renderer != Some(expected.renderer.as_str())
            || contribution.extension_id.as_str() != producer.plugin_id
        {
            return Err(UiBrokerError::UndeclaredContribution(
                contribution.id.to_string(),
            ));
        }
        if owner(&contribution.document_id) != Some(producer.id) {
            return Err(UiBrokerError::Ownership(
                contribution.document_id.to_string(),
            ));
        }
        if let Some(wire_fallback) = contribution
            .metadata
            .get("fallbackRenderer")
            .and_then(Value::as_str)
        {
            if expected.fallback_renderer.as_deref() != Some(wire_fallback) {
                return Err(UiBrokerError::UndeclaredContribution(
                    contribution.id.to_string(),
                ));
            }
        }
    }
    Ok(())
}

/// A shared JS entrypoint can declare surfaces for more than one target. The
/// launch target is host-attested, while the worker registration list is not,
/// so validate every tuple above and then retain only registrations applicable
/// to this concrete worker instance. `Shared` declarations apply everywhere.
fn filter_contributions_for_launch_target(state: &ProducerState, message: &mut UiWireMessage) {
    if message.kind != "contributions" {
        return;
    }
    message.contributions.retain(|registration| {
        state
            .verified_contributions
            .get(registration.id.as_str())
            .is_some_and(|verified| {
                verified.targets.iter().any(|target| {
                    matches!(target, codypendent_sandbox::UiTarget::Shared)
                        || format!("{target:?}").eq_ignore_ascii_case(&state.target)
                })
            })
    });
}

fn authorize_command_invocation(
    producer: &ProducerState,
    action: &UiActionInvocation,
) -> Result<(), UiBrokerError> {
    let granted = producer.declared_capabilities.contains("command-invoke");
    let negotiated = producer.host.selection().is_some_and(|selection| {
        selection
            .capabilities
            .iter()
            .any(|capability| capability.as_str() == "command-invoke")
    });
    let projected_and_enabled = producer
        .approved_commands
        .contains(action.action_id.as_str());
    if granted && negotiated && projected_and_enabled {
        Ok(())
    } else {
        Err(UiBrokerError::UndeclaredAction(
            action.action_id.to_string(),
        ))
    }
}

fn action_denied_message(invocation: &UiActionInvocation, error: &UiBrokerError) -> UiWireMessage {
    ui_message(
        "actionResult",
        format!(
            "action-denied:{}:{}",
            invocation.invocation_id,
            MessageId::new()
        ),
        |wire| {
            wire.action_result = Some(UiActionResult {
                invocation_id: invocation.invocation_id.clone(),
                status: "failed".to_owned(),
                value: Value::Null,
                error: Some(UiRemoteError {
                    code: "ui.action.not-authorized".to_owned(),
                    message: error.to_string(),
                    recoverable: true,
                    document_id: Some(invocation.document_id.clone()),
                    node_id: Some(invocation.source_node_id.clone()),
                    patch_index: None,
                    recovery: None,
                    fallback: None,
                    details: Value::Null,
                }),
            });
        },
    )
}

fn ensure_document_namespace(state: &mut ProducerState, local: &UiDocumentId) -> UiDocumentId {
    if let Some(global) = state.local_to_global_documents.get(local) {
        return global.clone();
    }
    let global = UiDocumentId::from(format!("document-{}", Uuid::now_v7()));
    state
        .local_to_global_documents
        .insert(local.clone(), global.clone());
    state
        .global_to_local_documents
        .insert(global.clone(), local.clone());
    global
}

fn namespace_producer_message(
    state: &mut ProducerState,
    message: &UiWireMessage,
) -> Result<UiWireMessage, UiBrokerError> {
    let mut namespaced = message.clone();
    if namespaced.kind == "contributions" {
        namespaced.extensions.insert(
            "contributionOwner".to_owned(),
            Value::String(state.replacement_scope.clone()),
        );
    }
    if let Some(snapshot) = &mut namespaced.snapshot {
        snapshot.document.document_id =
            ensure_document_namespace(state, &snapshot.document.document_id.clone());
    }
    if let Some(batch) = &mut namespaced.patch_batch {
        batch.document_id = state
            .local_to_global_documents
            .get(&batch.document_id)
            .cloned()
            .ok_or_else(|| UiBrokerError::Ownership(batch.document_id.to_string()))?;
    }
    if let Some(dispose) = &mut namespaced.dispose {
        dispose.document_id = state
            .local_to_global_documents
            .get(&dispose.document_id)
            .cloned()
            .ok_or_else(|| UiBrokerError::Ownership(dispose.document_id.to_string()))?;
    }
    for contribution in &mut namespaced.contributions {
        contribution.document_id = state
            .local_to_global_documents
            .get(&contribution.document_id)
            .cloned()
            .ok_or_else(|| UiBrokerError::Ownership(contribution.document_id.to_string()))?;
        contribution.id = state
            .contribution_names
            .entry(contribution.id.clone())
            .or_insert_with(|| {
                codypendent_protocol::UiContributionId::from(format!(
                    "contribution-{}",
                    Uuid::now_v7()
                ))
            })
            .clone();
        contribution.extension_id =
            codypendent_protocol::UiExtensionId::from(state.replacement_scope.clone());
        // Immutable identity chrome is broker-attested from the verified
        // launch descriptor, never accepted from producer document props.
        contribution.metadata.insert(
            "hostExtensionId".to_owned(),
            Value::String(state.handle.plugin_id.clone()),
        );
        contribution.metadata.insert(
            "hostPublisher".to_owned(),
            Value::String(state.publisher.clone()),
        );
        contribution.metadata.insert(
            "hostTrust".to_owned(),
            Value::String(state.trust_label.clone()),
        );
    }
    Ok(namespaced)
}

fn authorize_subscription(
    producer: &ProducerState,
    request: &UiProjectionSubscription,
) -> Result<(), UiBrokerError> {
    let required = match request.kind.as_str() {
        "artifact" => "artifact-read",
        "context" | "session" => "context-read",
        "run" => "run-read",
        // A run's blackboard is part of that workflow run's observable state:
        // same resource id, same ownership join, same read-only authority.
        "workflow" | "blackboard" => "workflow-read",
        "command" => "command-invoke",
        other => return Err(UiBrokerError::UnauthorizedSubscription(other.to_owned())),
    };
    if producer.declared_capabilities.contains(required)
        || producer.declared_capabilities.contains("projections")
    {
        Ok(())
    } else {
        Err(UiBrokerError::UnauthorizedSubscription(required.to_owned()))
    }
}

fn enforce_renderer_rate(renderer: &mut RendererState) -> Result<(), UiBrokerError> {
    let now = Instant::now();
    while renderer
        .recent_messages
        .front()
        .is_some_and(|seen| now.duration_since(*seen) >= RENDERER_RATE_WINDOW)
    {
        renderer.recent_messages.pop_front();
    }
    if renderer.recent_messages.len() >= RENDERER_RATE_BURST {
        return Err(UiBrokerError::RateLimited);
    }
    renderer.recent_messages.push_back(now);
    Ok(())
}

fn remember_renderer_message(renderer: &mut RendererState, message_id: String) -> bool {
    if !renderer.seen.insert(message_id.clone()) {
        return false;
    }
    renderer.seen_order.push_back(message_id);
    while renderer.seen_order.len() > REPLAY_ID_LIMIT {
        if let Some(expired) = renderer.seen_order.pop_front() {
            renderer.seen.remove(&expired);
        }
    }
    true
}

fn capabilities_digest(capabilities: &UiCapabilities) -> String {
    hex::encode(Sha256::digest(
        serde_json::to_vec(capabilities).expect("UiCapabilities is serializable"),
    ))
}

fn interaction_event_can_authorize_command(event: &str) -> bool {
    matches!(event, "action" | "press" | "submit" | "select" | "navigate")
}

fn consume_interaction_grant(
    producer: &mut ProducerState,
    invocation: &mut UiActionInvocation,
) -> Result<(ClientId, ClientRole), UiBrokerError> {
    let token = invocation
        .interaction_token
        .take()
        .ok_or_else(|| UiBrokerError::UndeclaredAction(invocation.action_id.to_string()))?;
    // Remove before checking anything else: a stale, malformed, or
    // unauthorized first attempt burns the token and cannot probe/replay it.
    let grant = producer
        .interaction_grants
        .remove(&token)
        .ok_or_else(|| UiBrokerError::UndeclaredAction(invocation.action_id.to_string()))?;
    let matching_context = grant.expires_at >= Instant::now()
        && grant.document_id == invocation.document_id
        && grant.revision == invocation.revision
        && grant.source_node_id == invocation.source_node_id
        && invocation.interaction_event_type.as_ref() == Some(&grant.event_type);
    let action_permitted = grant
        .permitted_actions
        .contains(invocation.action_id.as_str())
        && producer
            .approved_commands
            .contains(invocation.action_id.as_str());
    invocation.interaction_event_type = None;
    if matching_context && action_permitted {
        Ok(grant.requester)
    } else {
        Err(UiBrokerError::UndeclaredAction(
            invocation.action_id.to_string(),
        ))
    }
}

fn trim_interaction_grants(grants: &mut HashMap<String, InteractionGrant>) {
    let now = Instant::now();
    grants.retain(|_, grant| grant.expires_at >= now);
    if grants.len() <= REPLAY_ID_LIMIT {
        return;
    }
    // These are one-shot gesture capabilities. Eviction fails a later command
    // closed and never expands authority.
    if let Some(key) = grants.keys().next().cloned() {
        grants.remove(&key);
    }
}

fn enforce_in_flight_quota(
    producer: &mut ProducerState,
    limits: UiHardLimits,
) -> Result<(), UiBrokerError> {
    let maximum = usize::from(limits.max_actions_per_node)
        .saturating_mul(16)
        .clamp(1, 4_096);
    if producer.in_flight.len() >= maximum {
        producer.violations = QUARANTINE_AFTER;
        Err(UiBrokerError::Quota(
            "pending action invocations".to_owned(),
        ))
    } else {
        Ok(())
    }
}

fn remember_invocation(
    producer: &mut ProducerState,
    invocation: &UiActionInvocation,
) -> Result<bool, UiBrokerError> {
    let encoded = serde_json::to_vec(invocation)
        .map_err(|error| UiBrokerError::Host(format!("cannot fingerprint action: {error}")))?;
    let digest = hex::encode(Sha256::digest(encoded));
    if let Some(previous) = producer.invocation_digests.get(&invocation.invocation_id) {
        if previous == &digest {
            return Ok(false);
        }
        producer.violations = QUARANTINE_AFTER;
        return Err(UiBrokerError::InvocationCollision(
            invocation.invocation_id.to_string(),
        ));
    }
    producer
        .invocation_digests
        .insert(invocation.invocation_id.clone(), digest);
    producer
        .invocation_order
        .push_back(invocation.invocation_id.clone());
    while producer.invocation_order.len() > REPLAY_ID_LIMIT {
        if let Some(expired) = producer.invocation_order.pop_front() {
            producer.invocation_digests.remove(&expired);
        }
    }
    Ok(true)
}

fn preflight_authoritative_state(
    session: &mut SessionBroker,
    producer: &UiProducerHandle,
    message: &UiWireMessage,
    limits: UiHardLimits,
) -> Result<(), UiBrokerError> {
    let mut documents = session.documents.clone();
    let mut owners = session.document_owners.clone();
    match message.kind.as_str() {
        "snapshot" => {
            let document = message
                .snapshot
                .as_ref()
                .expect("validated snapshot")
                .document
                .clone();
            documents.mount(document.clone()).map_err(host_error)?;
            owners.insert(document.document_id, producer.id);
        }
        "patchBatch" => {
            documents
                .apply(message.patch_batch.as_ref().expect("validated patch"))
                .map_err(host_error)?;
        }
        "dispose" => {
            let document_id = &message
                .dispose
                .as_ref()
                .expect("validated dispose")
                .document_id;
            documents.remove(document_id);
            owners.remove(document_id);
        }
        _ => return Ok(()),
    }
    enforce_document_quota_view(&documents, &owners, producer.id, limits).inspect_err(|_error| {
        if let Some(state) = session.producers.get_mut(&producer.id) {
            state.violations = QUARANTINE_AFTER;
        }
    })
}

fn enforce_aggregate_document_quota(
    session: &mut SessionBroker,
    producer_id: Uuid,
    limits: UiHardLimits,
) -> Result<(), UiBrokerError> {
    enforce_document_quota_view(
        &session.documents,
        &session.document_owners,
        producer_id,
        limits,
    )
}

fn enforce_document_quota_view(
    documents: &DocumentStore,
    owners: &HashMap<UiDocumentId, Uuid>,
    producer_id: Uuid,
    limits: UiHardLimits,
) -> Result<(), UiBrokerError> {
    let owned = documents
        .documents()
        .filter(|document| owners.get(&document.document_id) == Some(&producer_id));
    let mut count = 0_usize;
    let mut nodes = 0_u64;
    let mut text_bytes = 0_u64;
    let mut json_values = 0_u64;
    for document in owned {
        reject_reserved_extension_primitives(&document.root)?;
        count = count.saturating_add(1);
        let stats = document.validate(&limits).map_err(host_error)?;
        nodes = nodes.saturating_add(u64::from(stats.nodes));
        text_bytes = text_bytes.saturating_add(stats.text_bytes);
        json_values = json_values.saturating_add(u64::from(stats.json_values));
    }
    if count > limits.max_contributions as usize {
        return Err(UiBrokerError::Quota("document count".to_owned()));
    }
    if nodes > u64::from(limits.max_nodes) {
        return Err(UiBrokerError::Quota("aggregate node count".to_owned()));
    }
    if text_bytes > limits.max_text_bytes {
        return Err(UiBrokerError::Quota("aggregate text bytes".to_owned()));
    }
    if json_values > u64::from(limits.max_json_values) {
        return Err(UiBrokerError::Quota("aggregate JSON values".to_owned()));
    }
    Ok(())
}

fn reject_reserved_extension_primitives(
    node: &codypendent_protocol::UiNode,
) -> Result<(), UiBrokerError> {
    let primitive = node.node_type.as_ref().map(|value| value.as_str());
    if matches!(primitive, Some("ApprovalCard" | "PermissionDiff")) {
        return Err(UiBrokerError::ReservedPrimitive(
            primitive.expect("matched primitive").to_owned(),
        ));
    }
    let secret_input = node
        .props
        .input
        .as_ref()
        .and_then(|input| input.input_type.as_deref())
        .or_else(|| node.props.extension.get("type").and_then(Value::as_str))
        .is_some_and(|input_type| matches!(input_type, "password" | "secret"));
    if secret_input {
        return Err(UiBrokerError::ReservedPrimitive(
            primitive.unwrap_or("secret input").to_owned(),
        ));
    }
    for child in &node.children {
        reject_reserved_extension_primitives(child)?;
    }
    if let Some(fallback) = &node.fallback {
        reject_reserved_extension_primitives(fallback)?;
    }
    Ok(())
}

fn append_renderer_baseline(
    session: &mut SessionBroker,
    renderer_id: ClientId,
    direct: &mut Vec<UiWireMessage>,
) -> Result<(), UiBrokerError> {
    let baseline = renderer_baseline_messages(session, renderer_id)?;
    let renderer = session
        .renderers
        .get_mut(&renderer_id)
        .ok_or(UiBrokerError::RendererNotAttached)?;
    let mut next_host = renderer.host.clone();
    for message in &baseline {
        let message = renderer_message(message);
        let _ = next_host
            .handle(message, RegistrationTrust::Extension)
            .map_err(host_error)?;
    }
    renderer.host = next_host;
    direct.extend(baseline);
    Ok(())
}

fn renderer_baseline_messages(
    session: &SessionBroker,
    renderer_id: ClientId,
) -> Result<Vec<UiWireMessage>, UiBrokerError> {
    let client_kind = session
        .renderers
        .get(&renderer_id)
        .ok_or(UiBrokerError::RendererNotAttached)?
        .client_kind
        .clone();
    let mut snapshots: Vec<_> = session
        .documents
        .documents()
        .filter(|document| {
            document_is_contributed(session, &document.document_id)
                && session
                    .document_targets
                    .get(&document.document_id)
                    .is_some_and(|target| target_matches(target, client_kind.as_deref()))
        })
        .map(|document| snapshot_message(document.clone(), "reconnect"))
        .collect();
    snapshots.sort_by(|left, right| {
        left.snapshot
            .as_ref()
            .expect("snapshot")
            .document
            .document_id
            .cmp(
                &right
                    .snapshot
                    .as_ref()
                    .expect("snapshot")
                    .document
                    .document_id,
            )
    });
    let mut coalesced = Vec::new();
    if let Some(theme) = &session.latest_theme {
        coalesced.push(theme.clone());
    }
    let mut contributions: Vec<_> = session
        .latest_contributions
        .iter()
        .filter(|((_, target), _)| target_matches(target, client_kind.as_deref()))
        .map(|(_, message)| message.clone())
        .collect();
    contributions.sort_by(|left, right| left.message_id.cmp(&right.message_id));
    coalesced.extend(contributions);
    coalesced.extend(snapshots);
    Ok(coalesced)
}

fn apply_to_renderers(
    session: &mut SessionBroker,
    producer_id: Uuid,
    message: &UiWireMessage,
) -> Result<(), UiBrokerError> {
    let producer_target = session
        .producers
        .get(&producer_id)
        .ok_or(UiBrokerError::UnknownProducer)?
        .target
        .clone();
    apply_to_renderers_for_target(session, &producer_target, message)
}

fn apply_to_renderers_for_target(
    session: &mut SessionBroker,
    producer_target: &str,
    message: &UiWireMessage,
) -> Result<(), UiBrokerError> {
    apply_messages_to_renderers_for_target(session, producer_target, std::slice::from_ref(message))
}

fn apply_messages_to_renderers_for_target(
    session: &mut SessionBroker,
    producer_target: &str,
    messages: &[UiWireMessage],
) -> Result<(), UiBrokerError> {
    let mut next_hosts = Vec::with_capacity(session.renderers.len());
    for (client_id, renderer) in &session.renderers {
        if !target_matches(producer_target, renderer.client_kind.as_deref()) {
            continue;
        }
        let mut host = renderer.host.clone();
        if host.selection().is_none() {
            next_hosts.push((*client_id, host));
            continue;
        }
        for message in messages {
            if message.kind == "contributions" && message.contributions.is_empty() {
                if let Some(owner) = message
                    .extensions
                    .get("contributionOwner")
                    .and_then(Value::as_str)
                {
                    host.registry_mut().unregister_extension(owner);
                }
                continue;
            }
            host.handle(renderer_message(message), RegistrationTrust::Extension)
                .map_err(host_error)?;
        }
        next_hosts.push((*client_id, host));
    }
    for (client_id, host) in next_hosts {
        session
            .renderers
            .get_mut(&client_id)
            .expect("renderer was preflighted")
            .host = host;
    }
    Ok(())
}

fn contribution_documents(message: &UiWireMessage) -> HashSet<UiDocumentId> {
    message
        .contributions
        .iter()
        .map(|registration| registration.document_id.clone())
        .collect()
}

fn document_is_contributed(session: &SessionBroker, document_id: &UiDocumentId) -> bool {
    session
        .latest_contributions
        .values()
        .any(|message| contribution_documents(message).contains(document_id))
}

fn target_matches(producer_target: &str, renderer_kind: Option<&str>) -> bool {
    match producer_target {
        "shared" => renderer_kind.is_some(),
        "terminal" => renderer_kind == Some("terminal"),
        "web" => renderer_kind.is_some_and(|kind| kind != "terminal"),
        _ => false,
    }
}

fn renderer_target_counts(session: &SessionBroker) -> (usize, usize) {
    session
        .renderers
        .values()
        .fold((0, 0), |(terminal, web), renderer| {
            match renderer.client_kind.as_deref() {
                Some("terminal") => (terminal + 1, web),
                Some(_) => (terminal, web + 1),
                None => (terminal, web),
            }
        })
}

fn publish_to_compatible_renderers(
    session: &SessionBroker,
    producer_id: Uuid,
    message: UiWireMessage,
) {
    let Some(producer) = session.producers.get(&producer_id) else {
        return;
    };
    publish_for_target(session, &producer.target, message);
}

fn publish_for_target(session: &SessionBroker, target: &str, message: UiWireMessage) {
    for (client_id, renderer) in &session.renderers {
        if target_matches(target, renderer.client_kind.as_deref()) {
            publish(
                session,
                UiBrokerTarget::Renderer(*client_id),
                message.clone(),
            );
        }
    }
}

fn renderer_message(message: &UiWireMessage) -> UiWireMessage {
    let mut message = message.clone();
    if message.kind == "contributions" {
        // Manifest grants were already checked by the broker. A renderer's
        // display feature negotiation must not be confused with plugin service
        // authorization and re-decide those grants.
        for contribution in &mut message.contributions {
            contribution.requires.clear();
        }
    }
    message
}

fn take_in_flight_or_known(
    session: &mut SessionBroker,
    producer: &UiProducerHandle,
    invocation_id: &UiEventId,
) -> Result<bool, UiBrokerError> {
    let state = session
        .producers
        .get_mut(&producer.id)
        .ok_or(UiBrokerError::UnknownProducer)?;
    if state.in_flight.remove(invocation_id) {
        Ok(true)
    } else if state.invocation_digests.contains_key(invocation_id) {
        Ok(false)
    } else {
        Err(UiBrokerError::Ownership(invocation_id.to_string()))
    }
}

fn new_host(offer: &UiCapabilities, limits: UiHardLimits) -> Result<UiHostSession, UiBrokerError> {
    let mut host = UiHostSession::new(offer.clone(), limits).map_err(host_error)?;
    define_slots(&mut host)?;
    Ok(host)
}

fn define_slots(host: &mut UiHostSession) -> Result<(), UiBrokerError> {
    for (point, trusted_only) in PUBLIC_POINTS
        .iter()
        .map(|point| (*point, false))
        .chain(CORE_POINTS.iter().map(|point| (*point, true)))
    {
        host.registry_mut()
            .define_slot(UiSlotDefinition {
                id: UiSlotId::from(point),
                point: UiContributionPoint::from(point),
                accepts: Vec::new(),
                trusted_only,
                maximum_contributions: Some(32),
                fallback: None,
            })
            .map_err(|error| UiBrokerError::Host(error.to_string()))?;
    }
    Ok(())
}

fn publish(session: &SessionBroker, target: UiBrokerTarget, message: UiWireMessage) {
    let _ = session.sender.send(UiBrokerFrame { target, message });
}

fn aggregate_viewport(session: &SessionBroker, producer_target: &str) -> Option<UiViewport> {
    session
        .renderers
        .values()
        .filter(|renderer| {
            target_matches(producer_target, renderer.client_kind.as_deref())
                && renderer.role != Some(ClientRole::Observer)
        })
        .filter_map(|renderer| renderer.viewport)
        .reduce(|left, right| UiViewport {
            width: left.width.min(right.width),
            height: left.height.min(right.height),
            pixel_width: match (left.pixel_width, right.pixel_width) {
                (Some(left), Some(right)) => Some(left.min(right)),
                _ => None,
            },
            pixel_height: match (left.pixel_height, right.pixel_height) {
                (Some(left), Some(right)) => Some(left.min(right)),
                _ => None,
            },
            density: match (left.density, right.density) {
                (Some(left), Some(right)) => Some(left.min(right)),
                _ => None,
            },
        })
}

fn publish_aggregate_viewports(session: &SessionBroker) {
    let updates: Vec<_> = session
        .producers
        .values()
        .filter_map(|producer| {
            aggregate_viewport(session, &producer.target)
                .map(|viewport| (producer.handle.clone(), viewport))
        })
        .collect();
    for (producer, viewport) in updates {
        publish(
            session,
            UiBrokerTarget::Producer(producer),
            viewport_message(viewport),
        );
    }
}

fn viewport_message(viewport: UiViewport) -> UiWireMessage {
    ui_message(
        "viewport",
        format!("host-viewport:{}", MessageId::new()),
        |wire| wire.viewport = Some(viewport),
    )
}

fn theme_message(theme: UiTheme) -> UiWireMessage {
    ui_message(
        "theme",
        format!("host-theme:{}:{}", theme.id, theme.revision),
        |wire| wire.theme = Some(theme),
    )
}

fn default_host_theme() -> UiTheme {
    UiTheme {
        id: "host.default".to_owned(),
        name: "Host default".to_owned(),
        revision: 0,
        color_scheme: None,
        high_contrast: false,
        reduced_motion: false,
        tokens: BTreeMap::new(),
    }
}

fn snapshot_message(document: codypendent_protocol::UiDocument, reason: &str) -> UiWireMessage {
    ui_message(
        "snapshot",
        format!(
            "snapshot:{}:{}:{}",
            document.document_id, document.revision.0, reason
        ),
        |wire| {
            wire.snapshot = Some(UiSnapshot {
                document,
                reason: Some(reason.to_owned()),
            });
        },
    )
}

fn ui_message(
    kind: impl Into<String>,
    message_id: impl Into<String>,
    configure: impl FnOnce(&mut UiWireMessage),
) -> UiWireMessage {
    let mut message = UiWireMessage {
        kind: kind.into(),
        message_id: message_id.into(),
        snapshot: None,
        patch_batch: None,
        event: None,
        action: None,
        subscription: None,
        unsubscription: None,
        projection: None,
        action_result: None,
        cancellation: None,
        dispose: None,
        viewport: None,
        resync: None,
        hot_reload: None,
        capabilities: None,
        selection: None,
        contributions: Vec::new(),
        theme: None,
        error: None,
        extensions: BTreeMap::new(),
    };
    configure(&mut message);
    message
}

fn host_error(error: impl ToString) -> UiBrokerError {
    UiBrokerError::Host(error.to_string())
}

fn daemon_offer() -> UiCapabilities {
    UiCapabilities {
        client: UiClientKind::from("daemon"),
        protocol_versions: vec![UiProtocolVersion::V1],
        daemon: ClientCapabilities {
            rich_text: true,
            image_display: true,
            audio_capture: false,
            editor_mutations: false,
            diff_view: true,
            mouse: true,
            unicode: true,
            true_color: true,
            ..ClientCapabilities::default()
        },
        primitives: vec![UiPrimitive::from("*")],
        media: vec![UiMediaCapability::from("image")],
        color_depth: UiColorDepth::from("trueColor"),
        keyboard: true,
        screen_reader: true,
        reduced_motion: true,
        clipboard: true,
        terminal_graphics: vec!["kitty".to_owned(), "sixel".to_owned(), "iterm2".to_owned()],
        viewport: UiViewport {
            width: 65_535,
            height: 65_535,
            pixel_width: None,
            pixel_height: None,
            density: None,
        },
        capabilities: vec![
            UiCapability::from("artifact-read"),
            UiCapability::from("context-read"),
            UiCapability::from("run-read"),
            UiCapability::from("workflow-read"),
            UiCapability::from("command-invoke"),
        ],
        contribution_points: PUBLIC_POINTS
            .iter()
            .map(|point| UiContributionPoint::from(*point))
            .collect(),
        limits: UiHardLimits::default(),
    }
}

#[must_use]
pub fn broker_error(error: impl ToString) -> UiWireMessage {
    ui_message("error", format!("error:{}", MessageId::new()), |wire| {
        wire.error = Some(UiRemoteError {
            code: "ui.host.rejected".to_owned(),
            message: error.to_string(),
            recoverable: true,
            document_id: None,
            node_id: None,
            patch_index: None,
            recovery: Some("resync".to_owned()),
            fallback: None,
            details: Value::Null,
        });
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn renderer_state() -> RendererState {
        RendererState {
            host: new_host(&daemon_offer(), UiHardLimits::default()).expect("host"),
            client_kind: Some("terminal".to_owned()),
            role: Some(ClientRole::Contributor),
            viewport: Some(UiViewport {
                width: 80,
                height: 24,
                pixel_width: None,
                pixel_height: None,
                density: None,
            }),
            capabilities_receipt: None,
            seen: HashSet::new(),
            seen_order: VecDeque::new(),
            recent_messages: VecDeque::new(),
        }
    }

    #[test]
    fn blackboard_subscriptions_need_the_workflow_read_capability() {
        // A run's board is part of that workflow run's observable state, so it
        // is gated by the same declared capability, never by nothing.
        let mut producer = producer_state();
        let request = UiProjectionSubscription {
            subscription_id: "subscription-1".to_owned(),
            kind: "blackboard".to_owned(),
            resource_id: Some("workflow-run-1".to_owned()),
            parameters: Default::default(),
        };
        assert!(matches!(
            authorize_subscription(&producer, &request),
            Err(UiBrokerError::UnauthorizedSubscription(required)) if required == "workflow-read"
        ));
        producer
            .declared_capabilities
            .insert("workflow-read".to_owned());
        assert!(authorize_subscription(&producer, &request).is_ok());

        // An unlisted kind stays refused whatever the producer declares.
        let unknown = UiProjectionSubscription {
            kind: "raw-daemon-handle".to_owned(),
            ..request
        };
        assert!(matches!(
            authorize_subscription(&producer, &unknown),
            Err(UiBrokerError::UnauthorizedSubscription(_))
        ));
    }

    fn producer_state() -> ProducerState {
        let handle = UiProducerHandle {
            id: Uuid::now_v7(),
            plugin_id: "test.plugin".to_owned(),
        };
        ProducerState {
            handle,
            host: new_host(&daemon_offer(), UiHardLimits::default()).expect("host"),
            replacement_scope: "ui-producer:test".to_owned(),
            target: "terminal".to_owned(),
            publisher: "Test Publisher".to_owned(),
            trust_label: "signed".to_owned(),
            declared_capabilities: HashSet::new(),
            verified_contributions: HashMap::new(),
            violations: 0,
            in_flight: HashSet::new(),
            subscriptions: HashMap::new(),
            approved_commands: HashSet::from(["run.pause".to_owned()]),
            local_to_global_documents: HashMap::new(),
            global_to_local_documents: HashMap::new(),
            contribution_names: HashMap::new(),
            invocation_digests: HashMap::new(),
            invocation_order: VecDeque::new(),
            interaction_grants: HashMap::new(),
            projections: HashMap::new(),
        }
    }

    fn action_with_token(token: &str, action_id: &str) -> UiActionInvocation {
        UiActionInvocation {
            invocation_id: UiEventId::from(Uuid::now_v7().to_string()),
            document_id: UiDocumentId::from("document"),
            revision: codypendent_protocol::UiRevision(3),
            source_node_id: codypendent_protocol::UiNodeId::from("pause"),
            action_id: codypendent_protocol::UiActionId::from(action_id),
            payload: Value::Null,
            form_data: BTreeMap::new(),
            interaction_token: Some(token.to_owned()),
            interaction_event_type: Some(codypendent_protocol::UiEventType::from("press")),
        }
    }

    #[test]
    fn contribution_namespace_rewrites_owner_and_registration_together() {
        let mut producer = producer_state();
        producer.local_to_global_documents.insert(
            UiDocumentId::from("local-document"),
            UiDocumentId::from("global-document"),
        );
        let mut message = ui_message("contributions", "replace", |wire| {
            wire.contributions = vec![codypendent_protocol::UiContributionRegistration {
                id: codypendent_protocol::UiContributionId::from("local-contribution"),
                extension_id: codypendent_protocol::UiExtensionId::from("test.plugin"),
                point: UiContributionPoint::from("panel"),
                slot: UiSlotId::from("panel"),
                document_id: UiDocumentId::from("local-document"),
                priority: 0,
                when: None,
                requires: Vec::new(),
                metadata: BTreeMap::new(),
            }];
        });
        message.extensions.insert(
            "contributionOwner".to_owned(),
            Value::String("test.plugin".to_owned()),
        );

        let namespaced = namespace_producer_message(&mut producer, &message).expect("namespace");
        assert_eq!(
            namespaced
                .extensions
                .get("contributionOwner")
                .and_then(Value::as_str),
            Some("ui-producer:test")
        );
        let registration = namespaced.contributions.first().expect("registration");
        assert_eq!(registration.extension_id.as_str(), "ui-producer:test");
        assert_eq!(registration.document_id.as_str(), "global-document");
        assert_eq!(
            registration
                .metadata
                .get("hostExtensionId")
                .and_then(Value::as_str),
            Some("test.plugin")
        );
    }

    #[test]
    fn observer_events_and_non_gesture_contexts_fail_closed() {
        assert!(!renderer_direction_allowed(ClientRole::Observer, "event"));
        assert!(renderer_direction_allowed(ClientRole::Contributor, "event"));
        assert!(!interaction_event_can_authorize_command("focus"));
        assert!(!interaction_event_can_authorize_command("change"));
        assert!(interaction_event_can_authorize_command("action"));
        assert!(interaction_event_can_authorize_command("press"));
        assert!(interaction_event_can_authorize_command("submit"));
    }

    #[test]
    fn interaction_grants_are_exact_context_one_shot_authority() {
        let mut producer = producer_state();
        let client = ClientId::new();
        let grant = |permitted_actions| InteractionGrant {
            requester: (client, ClientRole::Contributor),
            document_id: UiDocumentId::from("document"),
            revision: codypendent_protocol::UiRevision(3),
            source_node_id: codypendent_protocol::UiNodeId::from("pause"),
            event_type: codypendent_protocol::UiEventType::from("press"),
            permitted_actions,
            expires_at: Instant::now() + Duration::from_secs(5),
        };
        producer.interaction_grants.insert(
            "valid-token".to_owned(),
            grant(HashSet::from(["run.pause".to_owned()])),
        );
        let mut invocation = action_with_token("valid-token", "run.pause");
        assert_eq!(
            consume_interaction_grant(&mut producer, &mut invocation).expect("valid gesture"),
            (client, ClientRole::Contributor)
        );
        assert!(invocation.interaction_token.is_none());
        assert!(consume_interaction_grant(
            &mut producer,
            &mut action_with_token("valid-token", "run.pause")
        )
        .is_err());

        producer.interaction_grants.insert(
            "wrong-action".to_owned(),
            grant(HashSet::from(["run.pause".to_owned()])),
        );
        assert!(consume_interaction_grant(
            &mut producer,
            &mut action_with_token("wrong-action", "run.cancel")
        )
        .is_err());
        assert!(!producer.interaction_grants.contains_key("wrong-action"));
    }

    #[test]
    fn command_disabled_after_gesture_returns_structured_recoverable_denial() {
        let mut producer = producer_state();
        let client = ClientId::new();
        producer.interaction_grants.insert(
            "raced-token".to_owned(),
            InteractionGrant {
                requester: (client, ClientRole::Contributor),
                document_id: UiDocumentId::from("document"),
                revision: codypendent_protocol::UiRevision(3),
                source_node_id: codypendent_protocol::UiNodeId::from("pause"),
                event_type: codypendent_protocol::UiEventType::from("press"),
                permitted_actions: HashSet::from(["run.pause".to_owned()]),
                expires_at: Instant::now() + Duration::from_secs(5),
            },
        );
        producer.approved_commands.remove("run.pause");
        let mut invocation = action_with_token("raced-token", "run.pause");
        let error = consume_interaction_grant(&mut producer, &mut invocation)
            .expect_err("projection was disabled before invocation");
        let message = action_denied_message(&invocation, &error);
        let result = message.action_result.expect("structured action result");
        assert_eq!(result.status, "failed");
        let remote = result.error.expect("denial details");
        assert_eq!(remote.code, "ui.action.not-authorized");
        assert!(remote.recoverable);
        assert!(!producer.interaction_grants.contains_key("raced-token"));
    }

    #[test]
    fn cancellation_is_active_once_then_idempotent_but_unknown_ids_fail() {
        let broker = RemoteUiBroker::default();
        let session_id = SessionId::new();
        let mut producer = producer_state();
        let handle = producer.handle.clone();
        let invocation_id = UiEventId::from("known-invocation");
        producer.in_flight.insert(invocation_id.clone());
        producer
            .invocation_digests
            .insert(invocation_id.clone(), "digest".to_owned());
        {
            let mut sessions = broker.sessions.lock().expect("broker lock");
            let session = broker.session(&mut sessions, session_id).expect("session");
            session.producers.insert(handle.id, producer);
        }
        {
            let mut sessions = broker.sessions.lock().expect("broker lock");
            let session = sessions.get_mut(&session_id).expect("session");
            assert!(take_in_flight_or_known(session, &handle, &invocation_id)
                .expect("active invocation"));
            assert!(
                !take_in_flight_or_known(session, &handle, &invocation_id).expect("settled replay")
            );
            assert!(matches!(
                take_in_flight_or_known(session, &handle, &UiEventId::from("guessed")),
                Err(UiBrokerError::Ownership(_))
            ));
        }
    }

    #[tokio::test]
    async fn settle_and_cancel_races_are_owner_bound_and_idempotent() {
        let broker = RemoteUiBroker::default();
        let session_id = SessionId::new();
        let mut producer = producer_state();
        let handle = producer.handle.clone();
        for invocation in ["settle-first", "cancel-first"] {
            let invocation = UiEventId::from(invocation);
            producer.in_flight.insert(invocation.clone());
            producer
                .invocation_digests
                .insert(invocation, "digest".to_owned());
        }
        {
            let mut sessions = broker.sessions.lock().expect("broker lock");
            let session = broker.session(&mut sessions, session_id).expect("session");
            session.producers.insert(handle.id, producer);
        }
        let mut subscription = broker
            .subscribe_producer(session_id, &handle)
            .expect("producer subscription");

        let result = UiActionResult {
            invocation_id: UiEventId::from("settle-first"),
            status: "succeeded".to_owned(),
            value: Value::Null,
            error: None,
        };
        broker
            .settle_action(session_id, &handle, result.clone())
            .expect("initial settlement");
        let settled = async {
            for _ in 0..4 {
                let frame = subscription.receiver.recv().await.expect("broker frame");
                if let Some(result) = frame.message.action_result {
                    return result;
                }
            }
            panic!("bounded producer baseline did not yield settlement")
        }
        .await;
        assert_eq!(settled, result);
        broker
            .cancel_action(
                session_id,
                &handle,
                UiActionCancellation {
                    invocation_id: UiEventId::from("settle-first"),
                },
            )
            .expect("late cancellation is an idempotent no-op");
        assert!(matches!(
            subscription.receiver.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));

        let cancellation = UiActionCancellation {
            invocation_id: UiEventId::from("cancel-first"),
        };
        broker
            .cancel_action(session_id, &handle, cancellation.clone())
            .expect("initial cancellation");
        let cancelled = async {
            for _ in 0..4 {
                let frame = subscription.receiver.recv().await.expect("broker frame");
                if let Some(cancellation) = frame.message.cancellation {
                    return cancellation;
                }
            }
            panic!("bounded producer baseline did not yield cancellation")
        }
        .await;
        assert_eq!(cancelled, cancellation);
        broker
            .settle_action(
                session_id,
                &handle,
                UiActionResult {
                    invocation_id: UiEventId::from("cancel-first"),
                    status: "succeeded".to_owned(),
                    value: Value::Null,
                    error: None,
                },
            )
            .expect("late settlement is an idempotent no-op");
        assert!(matches!(
            subscription.receiver.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));

        assert!(matches!(
            broker.cancel_action(
                session_id,
                &handle,
                UiActionCancellation {
                    invocation_id: UiEventId::from("guessed"),
                },
            ),
            Err(UiBrokerError::Ownership(_))
        ));
    }

    #[test]
    fn synchronous_plugin_revocation_removes_authority_before_late_output() {
        let broker = RemoteUiBroker::default();
        let session_id = SessionId::new();
        let mut producer = producer_state();
        let handle = producer.handle.clone();
        producer.in_flight.insert(UiEventId::from("ready-action"));
        producer.interaction_grants.insert(
            "ready-grant".to_owned(),
            InteractionGrant {
                requester: (ClientId::new(), ClientRole::Controller),
                document_id: UiDocumentId::from("document"),
                revision: codypendent_protocol::UiRevision(1),
                source_node_id: codypendent_protocol::UiNodeId::from("button"),
                event_type: codypendent_protocol::UiEventType::from("press"),
                permitted_actions: HashSet::from(["run.pause".to_owned()]),
                expires_at: Instant::now() + Duration::from_secs(5),
            },
        );
        {
            let mut sessions = broker.sessions.lock().expect("broker lock");
            broker
                .session(&mut sessions, session_id)
                .expect("session")
                .producers
                .insert(handle.id, producer);
        }

        assert_eq!(broker.revoke_plugin("test.plugin").expect("revoke"), 1);
        assert!(matches!(
            broker.handle_producer(
                session_id,
                &handle,
                ui_message("action", "late-ready-output", |_| {}),
            ),
            Err(UiBrokerError::UnknownProducer)
        ));
        let sessions = broker.sessions.lock().expect("broker lock");
        let session = sessions.get(&session_id).expect("session remains");
        assert!(session.producers.is_empty());
    }

    #[test]
    fn disabled_during_click_returns_a_structured_denial_not_a_bridge_error() {
        let mut producer = producer_state();
        producer
            .declared_capabilities
            .insert("command-invoke".to_owned());
        let client = ClientId::new();
        producer.interaction_grants.insert(
            "click-before-disable".to_owned(),
            InteractionGrant {
                requester: (client, ClientRole::Controller),
                document_id: UiDocumentId::from("document"),
                revision: codypendent_protocol::UiRevision(3),
                source_node_id: codypendent_protocol::UiNodeId::from("pause"),
                event_type: codypendent_protocol::UiEventType::from("press"),
                permitted_actions: HashSet::from(["run.pause".to_owned()]),
                expires_at: Instant::now() + Duration::from_secs(5),
            },
        );
        // A newer command projection disables the command after the renderer
        // click but before the worker submits its gesture-bound invocation.
        producer.approved_commands.remove("run.pause");
        let mut invocation = action_with_token("click-before-disable", "run.pause");
        let error = consume_interaction_grant(&mut producer, &mut invocation)
            .and_then(|_| authorize_command_invocation(&producer, &invocation))
            .expect_err("latest disabled projection wins the race");
        let denial = action_denied_message(&invocation, &error);
        assert_eq!(denial.kind, "actionResult");
        let result = denial.action_result.expect("structured action result");
        assert_eq!(result.status, "failed");
        assert_eq!(result.invocation_id, invocation.invocation_id);
        assert_eq!(
            result.error.expect("typed denial").code,
            "ui.action.not-authorized"
        );
        assert!(!producer
            .interaction_grants
            .contains_key("click-before-disable"));
    }

    #[test]
    fn renderer_replay_cache_and_rate_window_are_bounded() {
        let mut renderer = renderer_state();
        for index in 0..=REPLAY_ID_LIMIT {
            assert!(remember_renderer_message(
                &mut renderer,
                format!("message-{index}")
            ));
        }
        assert_eq!(renderer.seen.len(), REPLAY_ID_LIMIT);
        assert!(remember_renderer_message(
            &mut renderer,
            "message-0".to_owned()
        ));
        for _ in 0..RENDERER_RATE_BURST {
            enforce_renderer_rate(&mut renderer).expect("within burst");
        }
        assert!(matches!(
            enforce_renderer_rate(&mut renderer),
            Err(UiBrokerError::RateLimited)
        ));
    }

    #[test]
    fn producer_target_routing_never_crosses_terminal_and_web() {
        assert!(target_matches("shared", Some("terminal")));
        assert!(target_matches("shared", Some("vscode")));
        assert!(target_matches("terminal", Some("terminal")));
        assert!(!target_matches("terminal", Some("vscode")));
        assert!(target_matches("web", Some("vscode")));
        assert!(!target_matches("web", Some("terminal")));
        assert!(!target_matches("shared", None));
    }

    #[test]
    fn shared_entrypoint_registrations_are_filtered_by_attested_launch_target() {
        let mut producer = producer_state();
        producer.verified_contributions = HashMap::from([
            (
                "terminal-only".to_owned(),
                VerifiedUiContribution {
                    id: "terminal-only".to_owned(),
                    point: "panel".to_owned(),
                    renderer: "terminal.renderer".to_owned(),
                    targets: vec![codypendent_sandbox::UiTarget::Terminal],
                    fallback_renderer: None,
                    applicable_slot: "panel".to_owned(),
                },
            ),
            (
                "web-only".to_owned(),
                VerifiedUiContribution {
                    id: "web-only".to_owned(),
                    point: "panel".to_owned(),
                    renderer: "web.renderer".to_owned(),
                    targets: vec![codypendent_sandbox::UiTarget::Web],
                    fallback_renderer: Some("terminal.renderer".to_owned()),
                    applicable_slot: "panel".to_owned(),
                },
            ),
            (
                "shared".to_owned(),
                VerifiedUiContribution {
                    id: "shared".to_owned(),
                    point: "panel".to_owned(),
                    renderer: "shared.renderer".to_owned(),
                    targets: vec![codypendent_sandbox::UiTarget::Shared],
                    fallback_renderer: None,
                    applicable_slot: "panel".to_owned(),
                },
            ),
        ]);
        let registration = |id: &str| codypendent_protocol::UiContributionRegistration {
            id: codypendent_protocol::UiContributionId::from(id),
            extension_id: codypendent_protocol::UiExtensionId::from("test.plugin"),
            point: UiContributionPoint::from("panel"),
            slot: UiSlotId::from("panel"),
            document_id: UiDocumentId::from(format!("{id}-document")),
            priority: 0,
            when: None,
            requires: Vec::new(),
            metadata: BTreeMap::new(),
        };
        let registrations = || {
            vec![
                registration("terminal-only"),
                registration("web-only"),
                registration("shared"),
            ]
        };
        let mut message = ui_message("contributions", "mixed-targets", |wire| {
            wire.contributions = registrations();
        });
        filter_contributions_for_launch_target(&producer, &mut message);
        let ids: Vec<_> = message
            .contributions
            .iter()
            .map(|registration| registration.id.as_str())
            .collect();
        assert_eq!(ids, vec!["terminal-only", "shared"]);

        producer.target = "web".to_owned();
        let mut message = ui_message("contributions", "mixed-targets-web", |wire| {
            wire.contributions = registrations();
        });
        filter_contributions_for_launch_target(&producer, &mut message);
        let ids: Vec<_> = message
            .contributions
            .iter()
            .map(|registration| registration.id.as_str())
            .collect();
        assert_eq!(ids, vec!["web-only", "shared"]);
    }

    #[test]
    fn detach_reports_only_targets_whose_last_renderer_left() {
        let broker = RemoteUiBroker::default();
        let session_id = SessionId::new();
        let terminal = ClientId::new();
        let second_terminal = ClientId::new();
        let web = ClientId::new();
        for client in [terminal, second_terminal, web] {
            broker
                .subscribe_renderer(session_id, client)
                .expect("subscribe renderer");
        }
        let capabilities = |client: &str, message_id: &str| {
            let mut offer = daemon_offer();
            offer.client = UiClientKind::from(client);
            ui_message("capabilities", message_id, |wire| {
                wire.capabilities = Some(offer)
            })
        };
        broker
            .handle_renderer(
                session_id,
                terminal,
                ClientRole::Contributor,
                capabilities("terminal", "terminal-1"),
            )
            .expect("terminal capabilities");
        broker
            .handle_renderer(
                session_id,
                second_terminal,
                ClientRole::Contributor,
                capabilities("terminal", "terminal-2"),
            )
            .expect("second terminal capabilities");
        broker
            .handle_renderer(
                session_id,
                web,
                ClientRole::Contributor,
                capabilities("vscode", "web-1"),
            )
            .expect("web capabilities");

        let first = broker.disconnect_renderer(session_id, terminal);
        assert_eq!(first.remaining_total, 2);
        assert_eq!(first.remaining_terminal, 1);
        assert_eq!(first.remaining_web, 1);
        assert!(first.departed_targets.is_empty());

        let web_left = broker.disconnect_renderer(session_id, web);
        assert_eq!(web_left.remaining_total, 1);
        assert_eq!(
            web_left.departed_targets,
            vec![codypendent_sandbox::UiTarget::Web]
        );

        let final_renderer = broker.disconnect_renderer(session_id, second_terminal);
        assert_eq!(final_renderer.remaining_total, 0);
        assert_eq!(
            final_renderer.departed_targets,
            vec![codypendent_sandbox::UiTarget::Terminal]
        );
    }

    #[test]
    fn renderer_capabilities_replay_the_selection_without_changing_the_contract() {
        let broker = RemoteUiBroker::default();
        let session_id = SessionId::new();
        let client_id = ClientId::new();
        broker
            .subscribe_renderer(session_id, client_id)
            .expect("subscribe renderer");
        let mut offer = daemon_offer();
        offer.client = UiClientKind::from("terminal");
        let capabilities = ui_message("capabilities", "one-shot", |wire| {
            wire.capabilities = Some(offer.clone());
        });
        let first = broker
            .handle_renderer(
                session_id,
                client_id,
                ClientRole::Contributor,
                capabilities.clone(),
            )
            .expect("first negotiation");
        assert!(first.renderer_negotiated);
        let replay = broker
            .handle_renderer(session_id, client_id, ClientRole::Contributor, capabilities)
            .expect("exact replay deduped");
        assert!(!replay.renderer_negotiated);

        let changed_id = ui_message("capabilities", "renegotiate", |wire| {
            wire.capabilities = Some(offer.clone());
        });
        let remounted = broker
            .handle_renderer(session_id, client_id, ClientRole::Controller, changed_id)
            .expect("same renderer contract survives a view remount");
        assert!(!remounted.renderer_negotiated);
        assert!(remounted
            .direct
            .iter()
            .any(|message| message.kind == "capabilitySelection"));

        offer.viewport.width += 1;
        let collision = ui_message("capabilities", "one-shot", |wire| {
            wire.capabilities = Some(offer);
        });
        assert!(matches!(
            broker.handle_renderer(
                session_id,
                client_id,
                ClientRole::Contributor,
                collision,
            ),
            Err(UiBrokerError::RendererDirection { kind, .. }) if kind == "capabilities-collision"
        ));
    }

    #[test]
    fn renderer_remount_updates_viewport_but_cannot_expand_capabilities() {
        let broker = RemoteUiBroker::default();
        let session = SessionId::new();
        let client = ClientId::new();
        broker
            .subscribe_renderer(session, client)
            .expect("subscribe");
        let offer = daemon_offer();
        broker
            .handle_renderer(
                session,
                client,
                ClientRole::Controller,
                ui_message("capabilities", "initial", |wire| {
                    wire.capabilities = Some(offer.clone())
                }),
            )
            .expect("initial negotiation");
        let mut resized = offer.clone();
        resized.viewport.width += 100;
        let replay = broker
            .handle_renderer(
                session,
                client,
                ClientRole::Controller,
                ui_message("capabilities", "remounted", |wire| {
                    wire.capabilities = Some(resized)
                }),
            )
            .expect("remount after resize");
        assert!(!replay.renderer_negotiated);
        assert!(replay
            .direct
            .iter()
            .any(|message| message.kind == "capabilitySelection"));
        let mut changed = offer;
        changed.primitives.pop();
        assert!(
            matches!(broker.handle_renderer(session, client, ClientRole::Controller, ui_message("capabilities", "changed-contract", |wire| wire.capabilities = Some(changed))), Err(UiBrokerError::RendererDirection { kind, .. }) if kind == "capabilities-renegotiation")
        );
    }

    #[test]
    fn renderer_baseline_replays_coalesced_state_and_visible_snapshots() {
        let broker = RemoteUiBroker::default();
        let session_id = SessionId::new();
        let client_id = ClientId::new();
        broker
            .subscribe_renderer(session_id, client_id)
            .expect("subscribe renderer");

        let owner = Uuid::now_v7();
        let document_id = UiDocumentId::from("latest-document");
        let document = codypendent_protocol::UiDocument {
            protocol_version: UiProtocolVersion::V1,
            document_id: document_id.clone(),
            revision: codypendent_protocol::UiRevision(9),
            root: codypendent_protocol::UiNode::element("root", "Stack"),
            capabilities: None,
            metadata: BTreeMap::new(),
            compatibility: None,
        };
        let contribution = ui_message("contributions", "latest-contribution", |wire| {
            wire.contributions = vec![codypendent_protocol::UiContributionRegistration {
                id: codypendent_protocol::UiContributionId::from("latest"),
                extension_id: codypendent_protocol::UiExtensionId::from("ui-producer:test"),
                point: UiContributionPoint::from("panel"),
                slot: UiSlotId::from("panel"),
                document_id: document_id.clone(),
                priority: 0,
                when: None,
                requires: Vec::new(),
                metadata: BTreeMap::new(),
            }];
        });
        {
            let mut sessions = broker.sessions.lock().expect("broker lock");
            let session = sessions.get_mut(&session_id).expect("session");
            session
                .renderers
                .get_mut(&client_id)
                .expect("renderer")
                .client_kind = Some("terminal".to_owned());
            session.documents.mount(document).expect("mount document");
            session.document_owners.insert(document_id.clone(), owner);
            session
                .document_targets
                .insert(document_id, "terminal".to_owned());
            session
                .latest_contributions
                .insert((owner, "terminal".to_owned()), contribution);
        }

        let baseline = broker
            .renderer_baseline(session_id, client_id)
            .expect("authoritative baseline");
        assert_eq!(
            baseline
                .iter()
                .map(|message| message.kind.as_str())
                .collect::<Vec<_>>(),
            vec!["theme", "contributions", "snapshot"]
        );
        let snapshot = baseline[2].snapshot.as_ref().expect("snapshot payload");
        assert_eq!(snapshot.document.revision.0, 9);
        assert_eq!(snapshot.reason.as_deref(), Some("reconnect"));
    }

    #[test]
    fn producer_replacement_scopes_coexist_and_unregister_independently() {
        let offer = daemon_offer();
        let selection = offer.negotiate(&offer).expect("self negotiation");
        let mut host = new_host(&offer, UiHardLimits::default()).expect("host");
        host.handle(
            ui_message("capabilitySelection", "selection", |wire| {
                wire.selection = Some(selection);
            }),
            RegistrationTrust::Extension,
        )
        .expect("selection");
        let registration = |scope: &str, id: &str, document: &str| {
            codypendent_protocol::UiContributionRegistration {
                id: codypendent_protocol::UiContributionId::from(id),
                extension_id: codypendent_protocol::UiExtensionId::from(scope),
                point: UiContributionPoint::from("panel"),
                slot: UiSlotId::from("panel"),
                document_id: UiDocumentId::from(document),
                priority: 0,
                when: None,
                requires: Vec::new(),
                metadata: BTreeMap::from([(
                    "hostExtensionId".to_owned(),
                    Value::String("acme.plugin".to_owned()),
                )]),
            }
        };
        for (scope, id, document) in [
            ("ui-producer:shared", "shared", "shared-document"),
            ("ui-producer:terminal", "terminal", "terminal-document"),
        ] {
            let mut message = ui_message("contributions", format!("replace-{id}"), |wire| {
                wire.contributions = vec![registration(scope, id, document)];
            });
            message.extensions.insert(
                "contributionOwner".to_owned(),
                Value::String(scope.to_owned()),
            );
            host.handle(message, RegistrationTrust::Extension)
                .expect("scoped registration");
        }
        assert_eq!(host.registry().mounted_document_ids().len(), 2);

        let mut unregister = ui_message("contributions", "unregister-shared", |_| {});
        unregister.extensions.insert(
            "contributionOwner".to_owned(),
            Value::String("ui-producer:shared".to_owned()),
        );
        host.handle(unregister, RegistrationTrust::Extension)
            .expect("scoped empty replacement");
        assert_eq!(
            host.registry().mounted_document_ids(),
            HashSet::from(["terminal-document"])
        );
    }

    #[test]
    fn extension_decision_and_secret_primitives_are_reserved() {
        let approval = codypendent_protocol::UiNode::element("approval", "ApprovalCard");
        assert!(matches!(
            reject_reserved_extension_primitives(&approval),
            Err(UiBrokerError::ReservedPrimitive(_))
        ));
        let mut secret = codypendent_protocol::UiNode::element("secret", "TextInput");
        secret
            .props
            .extension
            .insert("type".to_owned(), Value::String("password".to_owned()));
        assert!(matches!(
            reject_reserved_extension_primitives(&secret),
            Err(UiBrokerError::ReservedPrimitive(_))
        ));
    }
}
