//! Представление ответа API в результате инструмента.

use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::json;
use sourcecraft_api::executor::{ApiResponse, ResponseBody};

use crate::error::ToolError;

/// Текст успешного ответа: компактный JSON, текст как есть, для файла — `{saved_to, bytes}`,
/// для пустого тела — `{status}`. К обрезанному ответу добавляется пояснение.
pub fn response_text(response: &ApiResponse) -> String {
    let mut text = match &response.body {
        ResponseBody::Json(value) => value.to_string(),
        ResponseBody::Text(text) => text.clone(),
        ResponseBody::SavedToFile { path, bytes } => {
            json!({"saved_to": path.display().to_string(), "bytes": bytes}).to_string()
        }
        ResponseBody::Empty => json!({"status": response.status}).to_string(),
    };
    if response.truncated {
        text.push_str(
            "\n\n[The response was truncated because it exceeds the server's size limit. \
             Request less data: use a smaller page_size, filters or a more specific tool.]",
        );
    }
    text
}

/// Успешный результат инструмента.
pub fn success(text: String) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(text)])
}

/// Результат инструмента с `isError: true`.
pub fn failure(err: &ToolError) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(err.message())])
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn response(body: ResponseBody, truncated: bool) -> ApiResponse {
        ApiResponse {
            status: 204,
            body,
            truncated,
        }
    }

    #[test]
    fn formats_bodies() {
        assert_eq!(
            response_text(&response(
                ResponseBody::Json(json!({"a": [1, 2], "b": "c"})),
                false
            )),
            r#"{"a":[1,2],"b":"c"}"#
        );
        assert_eq!(
            response_text(&response(ResponseBody::Text("plain".into()), false)),
            "plain"
        );
        assert_eq!(
            response_text(&response(ResponseBody::Empty, false)),
            r#"{"status":204}"#
        );
        let saved = response_text(&response(
            ResponseBody::SavedToFile {
                path: PathBuf::from("/tmp/x.bin"),
                bytes: 42,
            },
            false,
        ));
        assert_eq!(saved, r#"{"bytes":42,"saved_to":"/tmp/x.bin"}"#);
    }

    #[test]
    fn notes_truncation() {
        let text = response_text(&response(ResponseBody::Text("{\"a\":".into()), true));
        assert!(text.starts_with("{\"a\":"));
        assert!(text.contains("truncated"));
    }

    #[test]
    fn error_result_is_flagged() {
        let result = failure(&ToolError::new("boom"));
        assert_eq!(result.is_error, Some(true));
        assert_eq!(success("ok".into()).is_error, Some(false));
    }
}
