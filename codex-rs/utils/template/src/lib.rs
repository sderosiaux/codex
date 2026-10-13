//! Minimal strict templating for prompt and text assets.
//!
//! Supported syntax:
//! - `{{ name }}` placeholder interpolation
//! - `{{{{` for a literal `{{`
//! - `}}}}` for a literal `}}`

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplateParseError {
    EmptyPlaceholder { start: usize },
    NestedPlaceholder { start: usize },
    UnmatchedClosingDelimiter { start: usize },
    UnterminatedPlaceholder { start: usize },
}

impl fmt::Display for TemplateParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyPlaceholder { start } => {
                write!(f, "template placeholder at byte {start} is empty")
            }
            Self::NestedPlaceholder { start } => {
                write!(
                    f,
                    "template placeholder starting at byte {start} contains a nested `{{`"
                )
            }
            Self::UnmatchedClosingDelimiter { start } => {
                write!(f, "template contains an unmatched `}}` at byte {start}")
            }
            Self::UnterminatedPlaceholder { start } => {
                write!(
                    f,
                    "template placeholder starting at byte {start} is missing `}}`"
                )
            }
        }
    }
}

impl Error for TemplateParseError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplateRenderError {
    DuplicateValue { name: String },
    ExtraValue { name: String },
    MissingValue { name: String },
}

impl fmt::Display for TemplateRenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateValue { name } => {
                write!(f, "template value `{name}` was provided more than once")
            }
            Self::ExtraValue { name } => {
                write!(f, "template value `{name}` is not used by this template")
            }
            Self::MissingValue { name } => {
                write!(f, "template placeholder `{name}` is missing a value")
            }
        }
    }
}

impl Error for TemplateRenderError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplateError {
    Parse(TemplateParseError),
    Render(TemplateRenderError),
}

impl fmt::Display for TemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(err) => err.fmt(f),
            Self::Render(err) => err.fmt(f),
        }
    }
}

impl Error for TemplateError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Parse(err) => Some(err),
            Self::Render(err) => Some(err),
        }
    }
}

impl From<TemplateParseError> for TemplateError {
    fn from(value: TemplateParseError) -> Self {
        Self::Parse(value)
    }
}

impl From<TemplateRenderError> for TemplateError {
    fn from(value: TemplateRenderError) -> Self {
        Self::Render(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    Literal(String),
    Placeholder(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    placeholders: BTreeSet<String>,
    segments: Vec<Segment>,
}

impl Template {
    pub fn parse(source: &str) -> Result<Self, TemplateParseError> {
        let mut placeholders = BTreeSet::new();
        let mut segments = Vec::new();
        let mut literal_start = 0usize;
        let mut cursor = 0usize;

        while cursor < source.len() {
            let rest = &source[cursor..];
            if rest.starts_with("{{{{") {
                push_literal(&mut segments, &source[literal_start..cursor]);
                push_literal(&mut segments, "{{");
                cursor += "{{{{".len();
                literal_start = cursor;
                continue;
            }
            if rest.starts_with("}}}}") {
                push_literal(&mut segments, &source[literal_start..cursor]);
                push_literal(&mut segments, "}}");
                cursor += "}}}}".len();
                literal_start = cursor;
                continue;
            }
            if rest.starts_with("{{") {
                push_literal(&mut segments, &source[literal_start..cursor]);
                let (placeholder, next_cursor) = parse_placeholder(source, cursor)?;
                placeholders.insert(placeholder.clone());
                segments.push(Segment::Placeholder(placeholder));
                cursor = next_cursor;
                literal_start = cursor;
                continue;
            }
            if rest.starts_with("}}") {
                return Err(TemplateParseError::UnmatchedClosingDelimiter { start: cursor });
            }

            let Some(ch) = rest.chars().next() else {
                break;
            };
            cursor += ch.len_utf8();
        }

        push_literal(&mut segments, &source[literal_start..]);
        Ok(Self {
            placeholders,
            segments,
        })
    }

    pub fn placeholders(&self) -> impl ExactSizeIterator<Item = &str> {
        self.placeholders.iter().map(String::as_str)
    }

    pub fn render<I, K, V>(&self, variables: I) -> Result<String, TemplateRenderError>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let variables = build_variable_map(variables)?;

        for placeholder in &self.placeholders {
            if !variables.contains_key(placeholder.as_str()) {
                return Err(TemplateRenderError::MissingValue {
                    name: placeholder.clone(),
                });
            }
        }

        for name in variables.keys() {
            if !self.placeholders.contains(name.as_str()) {
                return Err(TemplateRenderError::ExtraValue { name: name.clone() });
            }
        }

        let mut rendered = String::new();
        for segment in &self.segments {
            match segment {
                Segment::Literal(literal) => rendered.push_str(literal),
                Segment::Placeholder(name) => {
                    let Some(value) = variables.get(name.as_str()) else {
                        return Err(TemplateRenderError::MissingValue { name: name.clone() });
                    };
                    rendered.push_str(value);
                }
            }
        }
        Ok(rendered)
    }
}

pub fn render<I, K, V>(template: &str, variables: I) -> Result<String, TemplateError>
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: AsRef<str>,
{
    Template::parse(template)?
        .render(variables)
        .map_err(Into::into)
}

fn push_literal(segments: &mut Vec<Segment>, literal: &str) {
    if literal.is_empty() {
        return;
    }

    if let Some(Segment::Literal(existing)) = segments.last_mut() {
        existing.push_str(literal);
    } else {
        segments.push(Segment::Literal(literal.to_string()));
    }
}

fn parse_placeholder(source: &str, start: usize) -> Result<(String, usize), TemplateParseError> {
    let placeholder_start = start + "{{".len();
    let mut cursor = placeholder_start;

    while cursor < source.len() {
        let rest = &source[cursor..];
        if rest.starts_with("{{") {
            return Err(TemplateParseError::NestedPlaceholder { start });
        }
        if rest.starts_with("}}") {
            let placeholder = source[placeholder_start..cursor].trim();
            if placeholder.is_empty() {
                return Err(TemplateParseError::EmptyPlaceholder { start });
            }
            return Ok((placeholder.to_string(), cursor + "}}".len()));
        }

        let Some(ch) = rest.chars().next() else {
            break;
        };
        cursor += ch.len_utf8();
    }

    Err(TemplateParseError::UnterminatedPlaceholder { start })
}

fn build_variable_map<I, K, V>(
    variables: I,
) -> Result<BTreeMap<String, String>, TemplateRenderError>
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: AsRef<str>,
{
    let mut map = BTreeMap::new();
    for (name, value) in variables {
        let name = name.as_ref().to_string();
        if map
            .insert(name.clone(), value.as_ref().to_string())
            .is_some()
        {
            return Err(TemplateRenderError::DuplicateValue { name });
        }
    }
    Ok(map)
}
