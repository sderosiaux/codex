use crate::JsonSchema;
use crate::TS;
use codex_protocol::models::ImageDetail;
use codex_protocol::models::ImageReference as CoreImageReference;
use codex_protocol::plan_tool::PlanItemArg as CorePlanItemArg;
use codex_protocol::plan_tool::StepStatus as CorePlanStepStatus;
use codex_protocol::turn_input::CyberAccessProgram as CoreCyberAccessProgram;
use codex_protocol::user_input::ByteRange as CoreByteRange;
use codex_protocol::user_input::TextElement as CoreTextElement;
use codex_protocol::user_input::UserInput as CoreUserInput;
use serde::Deserialize;
use serde::Serialize;
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum TurnStatus {
    Completed,
    Interrupted,
    Failed,
    InProgress,
}

// Turn APIs

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "lowercase")]
#[ts(rename_all = "lowercase")]
#[ts(export_to = "v2/")]
pub enum AdditionalContextKind {
    Untrusted,
    Application,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AdditionalContextEntry {
    pub value: String,
    pub kind: AdditionalContextKind,
}

/// Requested cyber treatment for an OpenAI model turn.
/// Authorization and model-tier restrictions remain server-owned.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum CyberAccessProgram {
    Standard,
    DaybreakBlue,
    DaybreakRed,
}

impl From<CyberAccessProgram> for CoreCyberAccessProgram {
    fn from(value: CyberAccessProgram) -> Self {
        match value {
            CyberAccessProgram::Standard => Self::Standard,
            CyberAccessProgram::DaybreakBlue => Self::DaybreakBlue,
            CyberAccessProgram::DaybreakRed => Self::DaybreakRed,
        }
    }
}

impl From<CoreCyberAccessProgram> for CyberAccessProgram {
    fn from(value: CoreCyberAccessProgram) -> Self {
        match value {
            CoreCyberAccessProgram::Standard => Self::Standard,
            CoreCyberAccessProgram::DaybreakBlue => Self::DaybreakBlue,
            CoreCyberAccessProgram::DaybreakRed => Self::DaybreakRed,
        }
    }
}

// User input types
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ByteRange {
    pub start: usize,
    pub end: usize,
}

impl From<CoreByteRange> for ByteRange {
    fn from(value: CoreByteRange) -> Self {
        Self {
            start: value.start,
            end: value.end,
        }
    }
}

impl From<ByteRange> for CoreByteRange {
    fn from(value: ByteRange) -> Self {
        Self {
            start: value.start,
            end: value.end,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TextElement {
    /// Byte range in the parent `text` buffer that this element occupies.
    pub byte_range: ByteRange,
    /// Optional human-readable placeholder for the element, displayed in the UI.
    placeholder: Option<String>,
}

impl TextElement {
    pub fn new(byte_range: ByteRange, placeholder: Option<String>) -> Self {
        Self {
            byte_range,
            placeholder,
        }
    }

    pub fn set_placeholder(&mut self, placeholder: Option<String>) {
        self.placeholder = placeholder;
    }

    pub fn placeholder(&self) -> Option<&str> {
        self.placeholder.as_deref()
    }
}

impl From<CoreTextElement> for TextElement {
    fn from(value: CoreTextElement) -> Self {
        Self::new(
            value.byte_range.into(),
            value._placeholder_for_conversion_only().map(str::to_string),
        )
    }
}

impl From<TextElement> for CoreTextElement {
    fn from(value: TextElement) -> Self {
        Self::new(value.byte_range.into(), value.placeholder)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(untagged)]
#[ts(untagged)]
#[ts(export_to = "v2/")]
pub enum ImageReference {
    Inline {
        url: String,
    },
    File {
        #[serde(rename = "fileId")]
        #[ts(rename = "fileId")]
        file_id: String,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "camelCase")]
#[ts(tag = "type")]
#[ts(export_to = "v2/")]
pub enum UserInput {
    Text {
        text: String,
        /// UI-defined spans within `text` used to render or persist special elements.
        #[serde(default)]
        text_elements: Vec<TextElement>,
    },
    Image {
        #[serde(flatten)]
        image: ImageReference,
        #[serde(default)]
        #[ts(optional)]
        detail: Option<ImageDetail>,
    },
    LocalImage {
        #[serde(default)]
        #[ts(optional)]
        detail: Option<ImageDetail>,
        path: PathBuf,
    },
    Audio {
        url: String,
    },
    LocalAudio {
        path: PathBuf,
    },
    Skill {
        name: String,
        path: PathBuf,
    },
    Mention {
        name: String,
        path: String,
    },
}

impl UserInput {
    pub fn into_core(self) -> CoreUserInput {
        match self {
            UserInput::Text {
                text,
                text_elements,
            } => CoreUserInput::Text {
                text,
                text_elements: text_elements.into_iter().map(Into::into).collect(),
            },
            UserInput::Image { image, detail } => CoreUserInput::Image {
                image: match image {
                    ImageReference::Inline { url } => CoreImageReference::Inline { image_url: url },
                    ImageReference::File { file_id } => CoreImageReference::File { file_id },
                },
                detail,
            },
            UserInput::LocalImage { path, detail } => CoreUserInput::LocalImage { path, detail },
            UserInput::Audio { url } => CoreUserInput::Audio { audio_url: url },
            UserInput::LocalAudio { path } => CoreUserInput::LocalAudio { path },
            UserInput::Skill { name, path } => CoreUserInput::Skill { name, path },
            UserInput::Mention { name, path } => CoreUserInput::Mention { name, path },
        }
    }
}

impl From<CoreUserInput> for UserInput {
    fn from(value: CoreUserInput) -> Self {
        match value {
            CoreUserInput::Text {
                text,
                text_elements,
            } => UserInput::Text {
                text,
                text_elements: text_elements.into_iter().map(Into::into).collect(),
            },
            CoreUserInput::Image { image, detail } => UserInput::Image {
                image: match image {
                    CoreImageReference::Inline { image_url } => {
                        ImageReference::Inline { url: image_url }
                    }
                    CoreImageReference::File { file_id } => ImageReference::File { file_id },
                },
                detail,
            },
            CoreUserInput::LocalImage { path, detail } => UserInput::LocalImage { path, detail },
            CoreUserInput::Audio { audio_url } => UserInput::Audio { url: audio_url },
            CoreUserInput::LocalAudio { path } => UserInput::LocalAudio { path },
            CoreUserInput::Skill { name, path } => UserInput::Skill { name, path },
            CoreUserInput::Mention { name, path } => UserInput::Mention { name, path },
            _ => unreachable!("unsupported user input variant"),
        }
    }
}

impl UserInput {
    pub fn text_char_count(&self) -> usize {
        match self {
            UserInput::Text { text, .. } => text.chars().count(),
            UserInput::Image { .. }
            | UserInput::LocalImage { .. }
            | UserInput::Audio { .. }
            | UserInput::LocalAudio { .. }
            | UserInput::Skill { .. }
            | UserInput::Mention { .. } => 0,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct Usage {
    pub input_tokens: i32,
    pub cached_input_tokens: i32,
    pub output_tokens: i32,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnPlanStep {
    pub step: String,
    pub status: TurnPlanStepStatus,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum TurnPlanStepStatus {
    Pending,
    InProgress,
    Completed,
}

impl From<CorePlanItemArg> for TurnPlanStep {
    fn from(value: CorePlanItemArg) -> Self {
        Self {
            step: value.step,
            status: value.status.into(),
        }
    }
}

impl From<CorePlanStepStatus> for TurnPlanStepStatus {
    fn from(value: CorePlanStepStatus) -> Self {
        match value {
            CorePlanStepStatus::Pending => Self::Pending,
            CorePlanStepStatus::InProgress => Self::InProgress,
            CorePlanStepStatus::Completed => Self::Completed,
        }
    }
}
