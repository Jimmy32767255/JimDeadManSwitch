//! 客户端与守护进程之间的 Unix Socket 协议。

use serde::{Deserialize, Serialize};

/// 客户端发给守护进程的请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    /// 请求只模拟执行命令（守护进程侧不再二次执行）。
    #[serde(default, skip_serializing_if = "is_false")]
    pub dry_run: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl Request {
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            rule: None,
            dry_run: false,
        }
    }

    pub fn with_rule(command: impl Into<String>, rule: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            rule: Some(rule.into()),
            dry_run: false,
        }
    }

    /// 标记为干运行请求：守护进程只记录将要执行的命令。
    pub fn dry_run(mut self, dry_run: bool) -> Self {
        self.dry_run = dry_run;
        self
    }
}

/// 守护进程返回的响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub msg: Option<String>,
    /// 捕获到的按键名称，仅 `capture` 请求使用。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<String>,
}

impl Response {
    pub fn ok() -> Self {
        Self {
            status: "ok".to_string(),
            msg: None,
            keys: Vec::new(),
        }
    }

    pub fn ok_with_message(msg: impl Into<String>) -> Self {
        Self {
            status: "ok".to_string(),
            msg: Some(msg.into()),
            keys: Vec::new(),
        }
    }

    pub fn ok_with_keys(keys: Vec<String>) -> Self {
        Self {
            status: "ok".to_string(),
            msg: None,
            keys,
        }
    }

    pub fn error(msg: impl Into<String>) -> Self {
        Self {
            status: "error".to_string(),
            msg: Some(msg.into()),
            keys: Vec::new(),
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
        assert!(!back.dry_run);
    }

    #[test]
    fn dry_run_request_round_trips() {
        let request = Request::with_rule("trigger", "rule1").dry_run(true);
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["dry_run"], true);
        let back: Request = serde_json::from_value(json).unwrap();
        assert!(back.dry_run);
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

    #[test]
    fn response_with_keys_round_trips() {
        let response =
            Response::ok_with_keys(vec!["KEY_LEFTCTRL".to_string(), "KEY_X".to_string()]);
        let json = serde_json::to_string(&response).unwrap();
        let back: Response = serde_json::from_str(&json).unwrap();
        assert!(back.is_ok());
        assert_eq!(back.keys, vec!["KEY_LEFTCTRL", "KEY_X"]);
    }
}
