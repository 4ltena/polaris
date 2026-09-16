//! MCP client notification handling and unsupported server request rejection.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use rmcp::{
    ClientHandler, ErrorData, RoleClient,
    model::*,
    service::{NotificationContext, RequestContext},
};

#[derive(Clone, Default)]
pub(super) struct Handler {
    pub epoch: Arc<AtomicU64>,
}

impl Handler {
    fn invalidate(&self) {
        // Exhaustion stays invalid instead of wrapping to a previous snapshot.
        let _ = self
            .epoch
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |epoch| {
                epoch.checked_add(1)
            });
    }
}

fn unsupported() -> ErrorData {
    ErrorData::new(ErrorCode::METHOD_NOT_FOUND, "unsupported_capability", None)
}

// Sampling and roots are intentionally unsupported, including legacy profiles.
#[allow(deprecated)]
impl ClientHandler for Handler {
    async fn create_message(
        &self,
        _: CreateMessageRequestParams,
        _: RequestContext<RoleClient>,
    ) -> Result<CreateMessageResult, ErrorData> {
        Err(unsupported())
    }
    async fn list_roots(
        &self,
        _: RequestContext<RoleClient>,
    ) -> Result<ListRootsResult, ErrorData> {
        Err(unsupported())
    }
    async fn create_elicitation(
        &self,
        _: ElicitRequestParams,
        _: RequestContext<RoleClient>,
    ) -> Result<ElicitResult, ErrorData> {
        Err(unsupported())
    }
    async fn on_custom_request(
        &self,
        _: CustomRequest,
        _: RequestContext<RoleClient>,
    ) -> Result<CustomResult, ErrorData> {
        Err(unsupported())
    }
    async fn on_tool_list_changed(&self, _: NotificationContext<RoleClient>) {
        self.invalidate();
    }
    async fn on_resource_list_changed(&self, _: NotificationContext<RoleClient>) {
        self.invalidate();
    }
    async fn on_prompt_list_changed(&self, _: NotificationContext<RoleClient>) {
        self.invalidate();
    }
    async fn on_resource_updated(
        &self,
        _: ResourceUpdatedNotificationParam,
        _: NotificationContext<RoleClient>,
    ) {
        self.invalidate();
    }
    async fn on_custom_notification(
        &self,
        _: CustomNotification,
        _: NotificationContext<RoleClient>,
    ) {
        self.invalidate();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    #[tokio::test]
    async fn notifications_only_invalidate_and_requests_receive_a_closed_refusal() {
        let handler = Handler::default();
        let observer = handler.clone();
        let (client, fixture) = tokio::io::duplex(32 * 1024);
        let mut service = rmcp::service::serve_directly(handler, client, None);
        let (read, mut write) = tokio::io::split(fixture);
        let mut read = BufReader::new(read);
        for method in [
            "notifications/tools/list_changed",
            "notifications/resources/list_changed",
            "notifications/prompts/list_changed",
        ] {
            let notification = serde_json::json!({"jsonrpc":"2.0", "method":method});
            write
                .write_all(format!("{notification}\n").as_bytes())
                .await
                .unwrap();
        }
        let requests = [
            serde_json::json!({"method":"sampling/createMessage","params":{"messages":[],"maxTokens":1}}),
            serde_json::json!({"method":"elicitation/create","params":{"message":"synthetic-secret", "requestedSchema":{"type":"object","properties":{}}}}),
            serde_json::json!({"method":"tasks/get","params":{"taskId":"synthetic-secret"}}),
        ];
        for (id, mut request) in requests.into_iter().enumerate() {
            request["jsonrpc"] = serde_json::json!("2.0");
            request["id"] = serde_json::json!(id);
            write
                .write_all(format!("{request}\n").as_bytes())
                .await
                .unwrap();
            let mut line = String::new();
            tokio::time::timeout(std::time::Duration::from_secs(2), read.read_line(&mut line))
                .await
                .unwrap()
                .unwrap();
            let response: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(response["id"], id);
            assert_eq!(response["error"]["message"], "unsupported_capability");
            assert!(!line.contains("synthetic-secret"));
        }
        assert_eq!(observer.epoch.load(Ordering::Acquire), 3);
        // No extra model request, prompt get, tool call or server registration
        // is emitted while handling these unsolicited messages.
        let mut extra = String::new();
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(30),
                read.read_line(&mut extra)
            )
            .await
            .is_err()
        );
        service.close().await.unwrap();
    }
}
