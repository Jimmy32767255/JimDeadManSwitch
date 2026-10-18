//! 客户端与守护进程之间的 Unix Socket 协议。

use serde::{Deserialize, Serialize};

/// 客户端发给守护进程的请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
}

impl Request {
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            rule: None,
        }
    }

    pub fn with_rule(command: impl Into<String>, rule: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            rule: Some(rule.into()),
        }
    }
}

/// 守护进程返回的响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub msg: Option<String>,
}

impl Response {
    pub fn ok() -> Self {
        Self {
            status: "ok".to_string(),
            msg: None,
        }
    }

    pub fn error(msg: impl Into<String>) -> Self {
        Self {
            status: "error".to_string(),
            msg: Some(msg.into()),
        }
    }

    pub fn is_ok(&self) -> bool {
        self.status == "ok"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_without_rule_has_no_rule_field() {
        let json = serde_json::to_value(Request::new("reload")).unwrap();
        assert_eq!(json, serde_json::json!({ "command": "reload" }));
    }

    #[test]
    fn request_with_rule_round_trips() {
        let request = Request::with_rule("trigger", "rule1");
        let json = serde_json::to_string(&request).unwrap();
        let back: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(back.command, "trigger");
        assert_eq!(back.rule.as_deref(), Some("rule1"));
    }

    #[test]
    fn response_shapes() {
        assert_eq!(
            serde_json::to_value(Response::ok()).unwrap(),
            serde_json::json!({ "status": "ok" })
        );
        assert_eq!(
            serde_json::to_value(Response::error("boom")).unwrap(),
            serde_json::json!({ "status": "error", "msg": "boom" })
        );
    }
}
