/**
 * Runtime tag registries for the three internally-tagged unions a client
 * actually receives off the socket, plus the `#[serde(other)] Unknown` fold
 * that Rust performs for free.
 *
 * Rust's `#[serde(tag = "type")] … #[serde(other)] Unknown` means a peer
 * running a NEWER protocol build can send a variant this build has never heard
 * of, and the receiving side must degrade it to `Unknown` rather than fail the
 * frame. TypeScript has no deserializer to hang that behaviour off, so it is
 * provided explicitly here: {@link foldUnknownPayload} and friends turn an
 * unrecognized tag into `{ type: "Unknown" }`, exactly as serde would.
 *
 * Each `*_TAGS` array is checked against its union at COMPILE time in both
 * directions, so adding a variant to the union without adding its tag here (or
 * vice versa) is a type error rather than a silent runtime gap.
 */

import type { CommandBody, CommandBodyTag } from "./commands.js";
import type { Payload, PayloadTag } from "./envelope.js";
import type { EventBody, EventBodyTag } from "./events.js";

/** Compile-time assertion that `T` is empty. */
type AssertNever<T extends never> = T;

export const PAYLOAD_TAGS = [
  "WorkflowCatalog",
  "KnowledgePage",
  "Ping",
  "Pong",
  "DaemonStatusRequest",
  "DaemonStatusResponse",
  "Shutdown",
  "ShutdownAck",
  "ShutdownIfIdle",
  "ShutdownRefused",
  "Error",
  "ClientHello",
  "ServerHello",
  "Command",
  "CommandAccepted",
  "CommandRejected",
  "DocumentLeaseGranted",
  "DocumentCreated",
  "DocumentSync",
  "DocumentPublishRequested",
  "DocsCheckCompleted",
  "SessionForked",
  "SessionList",
  "SessionSearchResults",
  "SessionHistory",
  "SessionLifecycleApplied",
  "SessionDeleted",
  "SessionExported",
  "EditorActionAccepted",
  "InboxPage",
  "InboxEntryApplied",
  "AnalyticsResults",
  "AnalyticsExported",
  "AutomationBindingResult",
  "AutomationBindingPage",
  "AutomationBindingDeleted",
  "AnalyticsBudgetResult",
  "AnalyticsBudgetPage",
  "AnalyticsBudgetDeleted",
  "BundleExported",
  "BundleImported",
  "FileSearchResults",
  "SessionEventsPage",
  "Event",
  "Catchup",
  "WorkflowRunStarted",
  "WorkflowRunSnapshot",
  "WorkflowEvent",
  "PromotionProposed",
  "UiPluginLifecycle",
  "ArtifactStored",
  "ArtifactChunk",
  "Memory",
  "MemoryForgotten",
  "MemoryEvidence",
  "BlackboardItems",
  "BlackboardPosted",
  "BlackboardItemApplied",
  "ModelProbes",
  "CodeGraphBuilt",
  "CodeGraphStatus",
  "CodeGraphPage",
  "RemoteUi",
  "MarketplaceSearchResults",
  "MarketplaceLifecycle",
  "SecretReferenceApplied",
  "SecretLeaseBound",
  "SecretReferenceList",
  "SecretReferenceRevoked",
  "FederatedIdentityEstablished",
  "PublicationPolicy",
  "GraphFactsPublished",
  "GraphTombstoned",
  "FederatedGraphResult",
  "BlastRadiusResult",
  "MigrationPlanResult",
  "ReviewerSuggestionsResult",
  "CampaignCreated",
  "CampaignDetail",
  "CampaignList",
  "CampaignExecuted",
  "CampaignCancelled",
  "Unknown",
] as const satisfies readonly PayloadTag[];

export const EVENT_BODY_TAGS = [
  "SessionCreated",
  "NoteAppended",
  "SessionClosed",
  "RunStarted",
  "RunStateChanged",
  "ModelStreamDelta",
  "ModelRetrying",
  "ToolProposed",
  "ToolDenied",
  "ToolStarted",
  "ToolUpdated",
  "ToolCompleted",
  "PatchProposed",
  "ApprovalRequested",
  "ApprovalResolved",
  "SteeringQueued",
  "SteeringApplied",
  "BudgetWarning",
  "ContextUsage",
  "RunCompleted",
  "RunUsage",
  "LearningsCaptured",
  "ClientPresenceChanged",
  "QuestionAsked",
  "QuestionResolved",
  "CheckpointRecorded",
  "CheckpointRestored",
  "SessionForked",
  "PendingPromptsChanged",
  "Unknown",
] as const satisfies readonly EventBodyTag[];

export const COMMAND_BODY_TAGS = [
  "ListWorkflows",
  "ReadKnowledge",
  "InstallUiPlugin",
  "SmokeTestUiPlugin",
  "EnableUiPlugin",
  "ListUiPlugins",
  "UpdateUiPlugin",
  "ApproveUiPluginUpdate",
  "RejectUiPluginUpdate",
  "RevokeUiPlugin",
  "RemoveTrustedUiPublisher",
  "ListSessions",
  "SearchSessions",
  "ReadSessionHistory",
  "MutateSessionLifecycle",
  "RunEditorAction",
  "ListInbox",
  "MutateInbox",
  "QueryAnalytics",
  "ExportAnalytics",
  "ManageAutomationBinding",
  "ManageAnalyticsBudget",
  "ExportBundle",
  "ImportBundle",
  "SearchWorkspaceFiles",
  "CreateSession",
  "CloseSession",
  "AttachSession",
  "SubmitUserInput",
  "StartRun",
  "ResolveApproval",
  "ResolveQuestion",
  "CancelRun",
  "PauseRun",
  "ResumeRun",
  "QueueSteering",
  "UpdateIdeContext",
  "CreateDocument",
  "CheckDocuments",
  "MutateDocument",
  "AcquireDocumentLease",
  "ReleaseDocumentLease",
  "PublishDocument",
  "StartWorkflow",
  "PauseWorkflow",
  "ResumeWorkflow",
  "RetryWorkflowNode",
  "CancelWorkflow",
  "ReadWorkflowRun",
  "ProposePromotion",
  "AdvancePromotion",
  "ApprovePromotion",
  "RollbackPromotion",
  "ReadBlackboard",
  "PostBlackboardItem",
  "UpdateBlackboardItem",
  "ReadSessionEvents",
  "InspectMemory",
  "CorrectMemory",
  "ForgetMemory",
  "ForgetMemoryScope",
  "OpenMemoryEvidence",
  "SubmitEvalEvidence",
  "PutArtifact",
  "ReadArtifact",
  "ProbeModel",
  "BuildCodeGraph",
  "ReadCodeGraphStatus",
  "ReadCodeGraph",
  "RestoreCheckpoint",
  "ForkSession",
  "QueuePrompt",
  "UpdateQueuedPrompt",
  "PromoteQueuedPrompt",
  "DeleteQueuedPrompt",
  "RunUserShell",
  "RememberMemory",
  "MarketplaceSearch",
  "MarketplaceInstall",
  "MarketplaceUpdate",
  "MarketplaceEnable",
  "MarketplaceDisable",
  "MarketplaceRevoke",
  "SecretDeclare",
  "SecretBind",
  "SecretList",
  "SecretRevoke",
  "EstablishFederatedIdentity",
  "GetPublicationPolicy",
  "SetPublicationPolicy",
  "PublishGraphFacts",
  "TombstoneGraphFacts",
  "QueryFederatedGraph",
  "QueryBlastRadius",
  "PlanMigration",
  "SuggestReviewers",
  "CreateCampaign",
  "GetCampaign",
  "ListCampaigns",
  "ExecuteCampaign",
  "CancelCampaign",
  "Unknown",
] as const satisfies readonly CommandBodyTag[];

// The other direction: a variant added to a union but not to the array above
// makes one of these three aliases a type error.
type _NoMissingPayloadTag = AssertNever<Exclude<PayloadTag, (typeof PAYLOAD_TAGS)[number]>>;
type _NoMissingEventBodyTag = AssertNever<Exclude<EventBodyTag, (typeof EVENT_BODY_TAGS)[number]>>;
type _NoMissingCommandBodyTag = AssertNever<Exclude<CommandBodyTag, (typeof COMMAND_BODY_TAGS)[number]>>;

const PAYLOAD_TAG_SET: ReadonlySet<string> = new Set<string>(PAYLOAD_TAGS);
const EVENT_BODY_TAG_SET: ReadonlySet<string> = new Set<string>(EVENT_BODY_TAGS);
const COMMAND_BODY_TAG_SET: ReadonlySet<string> = new Set<string>(COMMAND_BODY_TAGS);

export function isKnownPayloadTag(tag: string): tag is PayloadTag {
  return PAYLOAD_TAG_SET.has(tag);
}

export function isKnownEventBodyTag(tag: string): tag is EventBodyTag {
  return EVENT_BODY_TAG_SET.has(tag);
}

export function isKnownCommandBodyTag(tag: string): tag is CommandBodyTag {
  return COMMAND_BODY_TAG_SET.has(tag);
}

/**
 * Apply Rust's `#[serde(other)]` semantics to an already-parsed payload: a tag
 * this build does not know becomes `{ type: "Unknown" }`. Field shapes are NOT
 * validated — this only closes the tag over the known union, which is the one
 * thing a static type cannot do at runtime.
 */
export function foldUnknownPayload(value: { type: string }): Payload {
  return isKnownPayloadTag(value.type) ? (value as Payload) : { type: "Unknown" };
}

/** As {@link foldUnknownPayload}, for `EventBody`. */
export function foldUnknownEventBody(value: { type: string }): EventBody {
  return isKnownEventBodyTag(value.type) ? (value as EventBody) : { type: "Unknown" };
}

/** As {@link foldUnknownPayload}, for `CommandBody`. */
export function foldUnknownCommandBody(value: { type: string }): CommandBody {
  return isKnownCommandBodyTag(value.type) ? (value as CommandBody) : { type: "Unknown" };
}
