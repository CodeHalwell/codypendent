//! Client capability advertisement.
//!
//! Each client tells the daemon what it can render and do (Chapter 02). The
//! daemon uses these flags to choose suitable projections and interaction
//! requests — for example, whether to offer a diff view or embed an image
//! rather than a link. Capabilities are advertised once, in the `ClientHello`.

use serde::{Deserialize, Serialize};

/// What a connected client can render and accept.
///
/// All flags default to `false`: a client only gets richer projections after it
/// explicitly opts in, so an unknown or minimal client is always served the
/// safe, plain-text baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ClientCapabilities {
    /// Renders styled text (bold, colour spans, links).
    pub rich_text: bool,
    /// Can display raster/vector images inline.
    pub image_display: bool,
    /// Can capture microphone audio.
    pub audio_capture: bool,
    /// Owns an editor buffer the daemon may mutate semantically.
    pub editor_mutations: bool,
    /// Can render a side-by-side or unified diff.
    pub diff_view: bool,
    /// Reports mouse input (every mouse affordance also has a keyboard path).
    pub mouse: bool,
    /// Terminal/display handles Unicode beyond ASCII.
    pub unicode: bool,
    /// Terminal/display supports 24-bit colour.
    pub true_color: bool,
}
