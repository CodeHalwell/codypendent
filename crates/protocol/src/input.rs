//! Multimodal input (Phase 6, STEP 6.5): the [`InputEnvelope`]/[`InputBlock`]
//! model from [Chapter 10](../../docs/docs/10-ide-github-and-inputs.md).
//!
//! Every input a client sends — a typed message, a pasted screenshot, a voice
//! note, a code-symbol reference, a linked PR — is normalized into one
//! [`InputEnvelope`]: a source, an ordered list of typed [`InputBlock`]s, the
//! scope it applies at, and any attached artifacts. The model is uniform so the
//! agent runtime consumes text, image, and audio through one path.
//!
//! Two invariants are load-bearing (exit criterion 3):
//!
//! * **The original is never replaced by a summary.** An [`ImageArtifact`] keeps
//!   the original image *and* its extracted text *and* model observations *and*
//!   crop/coordinate references as distinct linked artifacts; an
//!   [`AudioArtifact`] keeps the original audio linked to its transcript. A
//!   downstream summary is an addition, never a substitution.
//! * **Data classification gates off-device processing.** Image/audio default to
//!   [`DataClassification::Confidential`]; remote transcription of an artifact is
//!   permitted only when policy allows that classification to leave the device
//!   ([`transcription_allowed`]).

use serde::{Deserialize, Serialize};

use crate::artifact::{ArtifactRef, DataClassification};
use crate::ide::EditorSelection;
use crate::ids::{ArtifactId, ModelId};

/// The default data classification for captured media (image/audio). Media is
/// treated as `Confidential` unless a policy reclassifies it, so it does not
/// leave the device by accident (Chapter 10 / STEP 6.5).
pub const DEFAULT_MEDIA_CLASSIFICATION: DataClassification = DataClassification::Confidential;

/// A normalized unit of user input: where it came from, the typed blocks it
/// carries, the scope it applies at, and any bulk attachments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputEnvelope {
    pub source: InputSource,
    pub blocks: Vec<InputBlock>,
    pub scope: ScopeLevel,
    /// Bulk artifacts referenced by the blocks (or attached alongside them).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<ArtifactRef>,
}

impl InputEnvelope {
    /// A plain-text envelope — the common case (a typed message).
    #[must_use]
    pub fn text(source: InputSource, scope: ScopeLevel, text: impl Into<String>) -> Self {
        Self {
            source,
            blocks: vec![InputBlock::Text { text: text.into() }],
            scope,
            attachments: Vec::new(),
        }
    }

    /// Every artifact id linked from this envelope — the original media, its
    /// derived artifacts (extracted text), and any attachments. Proves the chain
    /// that keeps an original reachable after downstream interpretation.
    #[must_use]
    pub fn linked_artifacts(&self) -> Vec<ArtifactId> {
        let mut ids = Vec::new();
        for block in &self.blocks {
            match block {
                InputBlock::Image(img) => {
                    ids.push(img.original.id);
                    if let Some(text) = &img.extracted_text {
                        ids.push(text.id);
                    }
                }
                InputBlock::Audio(audio) => {
                    ids.push(audio.original.id);
                }
                InputBlock::File(file) => ids.push(file.id),
                _ => {}
            }
        }
        for a in &self.attachments {
            ids.push(a.id);
        }
        ids
    }
}

/// Where an input originated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
#[non_exhaustive]
pub enum InputSource {
    /// The Ratatui TUI.
    Tui,
    /// An IDE extension (VS Code / Cursor / Zed).
    Ide,
    /// The headless CLI / JSONL client.
    Cli,
    /// A web client.
    Web,
    /// Voice capture (push-to-talk), before/with transcription.
    Voice,
    /// A source a newer peer defined that this build does not know.
    #[serde(other)]
    Unknown,
}

/// One typed block within an [`InputEnvelope`]. Internally tagged on the wire
/// with a `block` discriminant (`{"block": "...", …fields}`) so the media
/// variants carry structured, artifact-linked payloads inline; an unrecognized
/// block decodes to [`InputBlock::Unknown`] for forward compatibility. The
/// discriminant is `block` (not `kind`) precisely because inner payloads such as
/// [`SymbolRef`]/[`GitHubReference`] carry their own `kind` field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "block", rename_all = "kebab-case")]
#[non_exhaustive]
pub enum InputBlock {
    /// Free text.
    Text { text: String },
    /// Captured audio and (once produced) its reviewed transcript.
    Audio(AudioArtifact),
    /// A pasted/attached image and its preserved derived artifacts.
    Image(ImageArtifact),
    /// An attached file.
    File(ArtifactRef),
    /// The IDE's current editor selection.
    EditorSelection(EditorSelection),
    /// A reference to a code symbol.
    CodeSymbol(SymbolRef),
    /// A reference to a GitHub entity (PR / issue / commit).
    #[serde(rename = "github-reference")]
    GitHubReference(GitHubReference),
    /// A block kind a newer peer defined that this build does not know.
    #[serde(other)]
    Unknown,
}

/// Captured audio with the **original preserved** and linked to its transcript.
/// The original artifact is never dropped in favor of the transcript (exit
/// criterion 3): the transcript is an added, attributed interpretation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioArtifact {
    /// The preserved original audio blob (kept where policy allows).
    pub original: ArtifactRef,
    /// The transcript, once produced and (for submission) reviewed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript: Option<Transcript>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_rate_hz: Option<u32>,
}

impl AudioArtifact {
    /// Whether this audio may be transcribed under `mode` given `policy` — a
    /// convenience over [`transcription_allowed`] reading the original's
    /// classification.
    pub fn may_transcribe(
        &self,
        mode: TranscriptionMode,
        policy: &OffDevicePolicy,
    ) -> Result<(), ClassificationError> {
        transcription_allowed(self.original.sensitivity, mode, policy)
    }
}

/// A transcript of an [`AudioArtifact`], linked back to its source audio.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transcript {
    pub text: String,
    /// Where the transcription ran (local vs. off-device).
    pub mode: TranscriptionMode,
    /// The transcription model, if a hosted/known one produced it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelId>,
    /// Whether the user reviewed/edited the transcript before submission
    /// (Chapter 10: "transcript review before submission").
    #[serde(default)]
    pub reviewed: bool,
    /// The audio artifact this transcript was produced from — the link that keeps
    /// the original reachable from the interpretation.
    pub source_audio: ArtifactId,
}

/// A pasted/attached image with all four Chapter 10 artifacts preserved: the
/// original, extracted text, model observations, and crop/coordinate references.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageArtifact {
    /// (1) The original image — never replaced by a summary.
    pub original: ArtifactRef,
    /// (2) Extracted text (OCR), when produced. A separate artifact, not a
    /// substitute for the image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extracted_text: Option<ArtifactRef>,
    /// (3) Model observations about the image.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observations: Vec<ModelObservation>,
    /// (4) Crop / coordinate references into the image.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub regions: Vec<ImageRegion>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
}

impl ImageArtifact {
    /// Whether all four Chapter 10 artifacts are present and linked (original +
    /// extracted text + at least one observation + at least one region). The
    /// original is mandatory; this reports the fully-enriched state.
    #[must_use]
    pub fn has_all_linked_artifacts(&self) -> bool {
        self.extracted_text.is_some() && !self.observations.is_empty() && !self.regions.is_empty()
    }
}

/// A model's textual observation about an image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelObservation {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelId>,
}

/// A rectangular region of an image (a crop or a coordinate reference).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageRegion {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// A reference to a code symbol in the workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolRef {
    pub path: String,
    /// The symbol name (e.g. `WorkflowDriver::advance`).
    pub symbol: String,
    /// The symbol kind (`function`, `struct`, …), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
}

/// A reference to a GitHub entity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubReference {
    pub owner: String,
    pub repo: String,
    pub kind: GitHubRefKind,
    /// The PR/issue number, or `None` for a commit/repo reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub number: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// The kind of GitHub entity a [`GitHubReference`] points to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
#[non_exhaustive]
pub enum GitHubRefKind {
    PullRequest,
    Issue,
    Commit,
    Comment,
    #[serde(other)]
    Unknown,
}

/// The scope hierarchy an input applies at (README: `System → Organisation →
/// User → Workspace → Repository → Branch → Session → Task`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
#[non_exhaustive]
pub enum ScopeLevel {
    System,
    Organization,
    User,
    Workspace,
    Repository,
    Branch,
    Session,
    Task,
    #[serde(other)]
    Unknown,
}

/// Where a transcription (or any media interpretation) runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum TranscriptionMode {
    /// On-device (e.g. a local whisper-server). Always permitted.
    Local,
    /// Off-device (a hosted/cloud model). Gated by data classification.
    Remote,
}

/// A policy describing the most sensitive classification permitted to leave the
/// device. Anything more restrictive stays local.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct OffDevicePolicy {
    pub max_off_device: DataClassification,
}

impl OffDevicePolicy {
    /// A restrictive policy: nothing above `Internal` may leave the device — so
    /// `Confidential` media (the media default) cannot be transcribed remotely.
    #[must_use]
    pub fn restrictive() -> Self {
        Self {
            max_off_device: DataClassification::Internal,
        }
    }

    /// A permissive policy: up to `Confidential` may leave the device.
    #[must_use]
    pub fn permissive() -> Self {
        Self {
            max_off_device: DataClassification::Confidential,
        }
    }
}

/// Why a classification gate blocked an operation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ClassificationError {
    /// The data is too sensitive to leave the device under the active policy.
    #[error("data classified {classification:?} may not be processed off-device (policy allows up to {max_off_device:?})")]
    OffDeviceForbidden {
        classification: DataClassification,
        max_off_device: DataClassification,
    },
}

/// The classification gate (exit criterion 3): whether media at `classification`
/// may be transcribed under `mode` given `policy`. Local transcription is always
/// allowed; remote transcription requires the classification to be permitted
/// off-device.
pub fn transcription_allowed(
    classification: DataClassification,
    mode: TranscriptionMode,
    policy: &OffDevicePolicy,
) -> Result<(), ClassificationError> {
    match mode {
        TranscriptionMode::Local => Ok(()),
        TranscriptionMode::Remote => {
            if classification.allowed_off_device(policy.max_off_device) {
                Ok(())
            } else {
                Err(ClassificationError::OffDeviceForbidden {
                    classification,
                    max_off_device: policy.max_off_device,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn artifact(media_type: &str, sensitivity: DataClassification) -> ArtifactRef {
        ArtifactRef {
            id: ArtifactId::new(),
            media_type: media_type.to_string(),
            byte_length: 1024,
            sha256: "a".repeat(64),
            sensitivity,
        }
    }

    fn round_trip<T>(value: &T) -> T
    where
        T: Serialize + for<'de> Deserialize<'de>,
    {
        let json = serde_json::to_string(value).expect("serialize");
        serde_json::from_str(&json).expect("deserialize")
    }

    #[test]
    fn image_envelope_round_trips_with_all_four_linked_artifacts() {
        let original = artifact("image/png", DEFAULT_MEDIA_CLASSIFICATION);
        let extracted = artifact("text/plain", DEFAULT_MEDIA_CLASSIFICATION);
        let img = ImageArtifact {
            original: original.clone(),
            extracted_text: Some(extracted.clone()),
            observations: vec![ModelObservation {
                text: "A terminal showing a failing test.".into(),
                model: Some(ModelId("claude-sonnet-5".into())),
            }],
            regions: vec![ImageRegion {
                label: Some("error message".into()),
                x: 10,
                y: 20,
                width: 300,
                height: 40,
            }],
            width: Some(1280),
            height: Some(720),
        };
        assert!(img.has_all_linked_artifacts());
        let envelope = InputEnvelope {
            source: InputSource::Tui,
            blocks: vec![InputBlock::Image(img)],
            scope: ScopeLevel::Session,
            attachments: vec![],
        };
        let back = round_trip(&envelope);
        assert_eq!(envelope, back);
        // The original and the extracted text both remain reachable.
        let linked = back.linked_artifacts();
        assert!(linked.contains(&original.id), "original image retained");
        assert!(
            linked.contains(&extracted.id),
            "extracted text linked, not substituted"
        );
    }

    #[test]
    fn audio_artifact_retains_original_linked_to_transcript() {
        let original = artifact("audio/wav", DEFAULT_MEDIA_CLASSIFICATION);
        let audio = AudioArtifact {
            original: original.clone(),
            transcript: Some(Transcript {
                text: "approve the patch".into(),
                mode: TranscriptionMode::Local,
                model: None,
                reviewed: true,
                source_audio: original.id,
            }),
            duration_ms: Some(1500),
            sample_rate_hz: Some(16_000),
        };
        let envelope = InputEnvelope {
            source: InputSource::Voice,
            blocks: vec![InputBlock::Audio(audio)],
            scope: ScopeLevel::Session,
            attachments: vec![],
        };
        let back = round_trip(&envelope);
        assert_eq!(envelope, back);
        // The transcript points back at the retained original audio.
        if let InputBlock::Audio(a) = &back.blocks[0] {
            let t = a.transcript.as_ref().unwrap();
            assert_eq!(t.source_audio, original.id);
            assert!(back.linked_artifacts().contains(&original.id));
        } else {
            panic!("expected an audio block");
        }
    }

    #[test]
    fn restrictive_policy_blocks_remote_transcription_of_confidential_audio() {
        // The media default is Confidential; a restrictive policy allows only up
        // to Internal off-device ⇒ remote transcription is blocked, local is fine.
        let policy = OffDevicePolicy::restrictive();
        let err = transcription_allowed(
            DEFAULT_MEDIA_CLASSIFICATION,
            TranscriptionMode::Remote,
            &policy,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            ClassificationError::OffDeviceForbidden { .. }
        ));
        // Local transcription of the same audio is always permitted.
        assert!(transcription_allowed(
            DEFAULT_MEDIA_CLASSIFICATION,
            TranscriptionMode::Local,
            &policy
        )
        .is_ok());
    }

    #[test]
    fn permissive_policy_allows_remote_transcription_of_confidential_audio() {
        let policy = OffDevicePolicy::permissive();
        assert!(transcription_allowed(
            DataClassification::Confidential,
            TranscriptionMode::Remote,
            &policy
        )
        .is_ok());
        // But Secret still cannot leave, even under the permissive policy.
        assert!(transcription_allowed(
            DataClassification::Secret,
            TranscriptionMode::Remote,
            &policy
        )
        .is_err());
    }

    #[test]
    fn may_transcribe_reads_the_originals_classification() {
        let audio = AudioArtifact {
            original: artifact("audio/wav", DataClassification::Secret),
            transcript: None,
            duration_ms: None,
            sample_rate_hz: None,
        };
        assert!(audio
            .may_transcribe(TranscriptionMode::Remote, &OffDevicePolicy::permissive())
            .is_err());
        assert!(audio
            .may_transcribe(TranscriptionMode::Local, &OffDevicePolicy::restrictive())
            .is_ok());
    }

    #[test]
    fn unknown_block_kind_decodes_forward_compatibly() {
        // A block kind a newer peer sent that this build does not know.
        let json = r#"{"source":{"type":"web"},"scope":{"type":"session"},"blocks":[{"block":"hologram","depth":true}]}"#;
        let envelope: InputEnvelope = serde_json::from_str(json).expect("tolerant decode");
        assert_eq!(envelope.blocks, vec![InputBlock::Unknown]);
    }

    #[test]
    fn unknown_source_and_scope_decode_forward_compatibly() {
        let json = r#"{"source":{"type":"smart-fridge"},"scope":{"type":"galaxy"},"blocks":[]}"#;
        let envelope: InputEnvelope = serde_json::from_str(json).expect("tolerant decode");
        assert_eq!(envelope.source, InputSource::Unknown);
        assert_eq!(envelope.scope, ScopeLevel::Unknown);
    }

    #[test]
    fn text_helper_builds_a_single_text_block() {
        let e = InputEnvelope::text(InputSource::Cli, ScopeLevel::Repository, "hello");
        assert_eq!(
            e.blocks,
            vec![InputBlock::Text {
                text: "hello".into()
            }]
        );
        let back = round_trip(&e);
        assert_eq!(e, back);
    }

    #[test]
    fn code_symbol_and_github_reference_round_trip() {
        let envelope = InputEnvelope {
            source: InputSource::Ide,
            blocks: vec![
                InputBlock::CodeSymbol(SymbolRef {
                    path: "crates/workflow/src/drive.rs".into(),
                    symbol: "WorkflowDriver::advance".into(),
                    kind: Some("function".into()),
                    line: Some(42),
                }),
                InputBlock::GitHubReference(GitHubReference {
                    owner: "CodeHalwell".into(),
                    repo: "codypendent".into(),
                    kind: GitHubRefKind::PullRequest,
                    number: Some(14),
                    url: None,
                }),
            ],
            scope: ScopeLevel::Repository,
            attachments: vec![],
        };
        assert_eq!(envelope, round_trip(&envelope));
    }
}
