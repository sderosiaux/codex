//! Bridges Apps SDK-style `openai/fileParams` metadata into Codex's MCP flow.
//!
//! Strategy:
//! - Inspect `_meta["openai/fileParams"]` to discover which tool arguments are
//!   file inputs.
//! - At tool execution time, read those files from the primary environment,
//!   upload them to OpenAI file storage,
//!   and rewrite only the declared arguments into the provided-file payload
//!   shape expected by the downstream Apps tool.
//!
//! The model-facing local-path schema is owned by `codex-mcp` alongside MCP tool inventory, so this
//! module only handles uploading the files and rewriting the execution-time arguments.

use crate::session::session::Session;
use crate::session::step_context::StepContext;
use codex_api::HostedFileUploadContext;
use codex_api::OPENAI_FILE_UPLOAD_LIMIT_BYTES;
use codex_api::upload_openai_file;
use codex_exec_server::GetMetadataOptions;
use codex_login::CodexAuth;
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::sync::Arc;

struct FileArgumentLocation<'a> {
    field_name: &'a str,
    index: Option<usize>,
}

pub(crate) async fn rewrite_mcp_tool_arguments_for_openai_files(
    sess: &Session,
    step_context: &StepContext,
    arguments_value: Option<JsonValue>,
    openai_file_input_optional_fields: Option<&HashMap<String, Vec<String>>>,
    hosted_upload: Option<&HostedFileUploadContext>,
) -> Result<Option<JsonValue>, String> {
    let Some(openai_file_input_optional_fields) = openai_file_input_optional_fields else {
        return Ok(arguments_value);
    };

    let Some(arguments_value) = arguments_value else {
        return Ok(None);
    };
    let Some(arguments) = arguments_value.as_object() else {
        return Ok(Some(arguments_value));
    };
    let auth = sess.services.auth_manager.auth().await;
    let mut rewritten_arguments = arguments.clone();

    for (field_name, optional_fields) in openai_file_input_optional_fields {
        let Some(value) = arguments.get(field_name) else {
            continue;
        };
        let Some(uploaded_value) = rewrite_argument_value_for_openai_files(
            sess,
            step_context,
            auth.as_ref(),
            field_name,
            optional_fields,
            value,
            hosted_upload,
        )
        .await?
        else {
            continue;
        };
        rewritten_arguments.insert(field_name.clone(), uploaded_value);
    }

    if rewritten_arguments == *arguments {
        return Ok(Some(arguments_value));
    }

    Ok(Some(JsonValue::Object(rewritten_arguments)))
}

async fn rewrite_argument_value_for_openai_files(
    sess: &Session,
    step_context: &StepContext,
    auth: Option<&CodexAuth>,
    field_name: &str,
    optional_fields: &[String],
    value: &JsonValue,
    hosted_upload: Option<&HostedFileUploadContext>,
) -> Result<Option<JsonValue>, String> {
    match value {
        JsonValue::String(file_path) => {
            let rewritten = build_uploaded_argument_value(
                sess,
                step_context,
                auth,
                FileArgumentLocation {
                    field_name,
                    index: None,
                },
                optional_fields,
                file_path,
                hosted_upload,
            )
            .await?;
            Ok(Some(rewritten))
        }
        JsonValue::Array(values) => {
            let mut rewritten_values = Vec::with_capacity(values.len());
            for (index, item) in values.iter().enumerate() {
                let Some(file_path) = item.as_str() else {
                    return Ok(None);
                };
                let rewritten = build_uploaded_argument_value(
                    sess,
                    step_context,
                    auth,
                    FileArgumentLocation {
                        field_name,
                        index: Some(index),
                    },
                    optional_fields,
                    file_path,
                    hosted_upload,
                )
                .await?;
                rewritten_values.push(rewritten);
            }
            Ok(Some(JsonValue::Array(rewritten_values)))
        }
        _ => Ok(None),
    }
}

async fn build_uploaded_argument_value(
    sess: &Session,
    step_context: &StepContext,
    auth: Option<&CodexAuth>,
    argument: FileArgumentLocation<'_>,
    optional_fields: &[String],
    file_path: &str,
    hosted_upload: Option<&HostedFileUploadContext>,
) -> Result<JsonValue, String> {
    let FileArgumentLocation { field_name, index } = argument;
    let contextualize_error = |error: String| match index {
        Some(index) => {
            format!("failed to upload `{file_path}` for `{field_name}[{index}]`: {error}")
        }
        None => format!("failed to upload `{file_path}` for `{field_name}`: {error}"),
    };
    let Some(auth) = auth else {
        return Err("ChatGPT auth is required to upload files for Codex Apps tools".to_string());
    };
    if !auth.uses_codex_backend() {
        return Err("ChatGPT auth is required to upload files for Codex Apps tools".to_string());
    }
    let turn_context = &step_context.turn;
    let Some(turn_environment) = step_context.environments.primary() else {
        return Err(contextualize_error(
            "no primary turn environment is available".to_string(),
        ));
    };
    let path_uri = turn_environment
        .cwd()
        .join(file_path)
        .map_err(|error| contextualize_error(error.to_string()))?;
    let fs = turn_environment.environment.get_filesystem();
    let metadata = fs
        .get_metadata(&path_uri, GetMetadataOptions::default())
        .await
        .map_err(|error| contextualize_error(error.to_string()))?;
    if !metadata.is_file {
        return Err(contextualize_error(format!(
            "path `{}` is not a file",
            path_uri.inferred_native_path_string()
        )));
    }
    if metadata.size > OPENAI_FILE_UPLOAD_LIMIT_BYTES {
        return Err(contextualize_error(format!(
            "file `{}` is too large: {} bytes exceeds the limit of {} bytes",
            path_uri.inferred_native_path_string(),
            metadata.size,
            OPENAI_FILE_UPLOAD_LIMIT_BYTES,
        )));
    }
    // Open the file before allocating a remote file record.
    let mut first_contents = Some(
        fs.read_file_stream(&path_uri)
            .await
            .map_err(|error| contextualize_error(error.to_string()))?,
    );
    let file_name = path_uri
        .basename()
        .or_else(|| {
            path_uri.infer_path_convention().and_then(|convention| {
                convention
                    .path_segments(file_path)
                    .rfind(|segment| !segment.is_empty())
                    .map(str::to_string)
            })
        })
        .unwrap_or_else(|| "file".to_string());
    let open_contents = move || {
        let first_contents = first_contents.take();
        let fs = Arc::clone(&fs);
        let path_uri = path_uri.clone();
        async move {
            match first_contents {
                Some(contents) => Ok(contents),
                None => fs.read_file_stream(&path_uri).await,
            }
        }
    };
    let upload_auth = codex_model_provider::auth_provider_from_auth(auth);
    let uploaded = upload_openai_file(
        turn_context.config.chatgpt_base_url.trim_end_matches('/'),
        upload_auth.as_ref(),
        &sess.services.openai_file_upload_client_pool,
        file_name,
        metadata.size,
        open_contents,
        hosted_upload,
    )
    .await
    .map_err(|error| contextualize_error(error.to_string()))?;
    let mut payload = serde_json::Map::new();
    payload.insert(
        "download_url".to_string(),
        JsonValue::String(uploaded.download_url),
    );
    payload.insert("file_id".to_string(), JsonValue::String(uploaded.file_id));
    if optional_fields
        .iter()
        .any(|optional_field| optional_field == "mime_type")
        && let Some(mime_type) = uploaded.mime_type
    {
        payload.insert("mime_type".to_string(), JsonValue::String(mime_type));
    }
    if optional_fields
        .iter()
        .any(|optional_field| optional_field == "file_name")
    {
        payload.insert(
            "file_name".to_string(),
            JsonValue::String(uploaded.file_name),
        );
    }
    Ok(JsonValue::Object(payload))
}
