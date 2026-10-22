use crate::process_scope::ProcessScope;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;
type Result<T> = std::result::Result<T, String>;
type Pending = HashMap<u64, mpsc::Sender<Result<Value>>>;
pub(crate) type RequestHandler = Arc<dyn Fn(&str, &Value) -> Option<Result<Value>> + Send + Sync>;
type MessageListener = Arc<dyn Fn(Value) + Send + Sync>;

fn native_error_summary(error: &Value) -> String {
    // Use only fixed categories. Native messages/data may contain credentials or configuration.
    let text = error.to_string().to_lowercase();
    let contains = |terms: &[&str]| terms.iter().any(|term| text.contains(term));
    let category = if contains(&[
        "context length",
        "context window",
        "contextwindowexceeded",
        "maximum context",
        "context_length_exceeded",
        "context_window_exceeded",
    ]) {
        "模型上下文容量不足，已保留现有记录"
    } else if contains(&[
        "429",
        "rate limit",
        "rate-limit",
        "rate_limit",
        "too many requests",
    ]) {
        "模型服务限流，请稍后继续当前任务"
    } else if contains(&[
        "402",
        "insufficient",
        "quota exceeded",
        "billing",
        "usage limit",
    ]) {
        "模型服务额度不足，请检查当前会话选定的提供商"
    } else if contains(&["401", "unauthorized", "invalid api key", "authentication"]) {
        "模型服务认证失败，请检查当前提供商的登录或认证"
    } else if contains(&[
        "fetch failed",
        "econnreset",
        "etimedout",
        "connection refused",
        "network error",
    ]) {
        "原生成员与模型服务的连接失败，可保留原方案后重试"
    } else if contains(&["unknown tool", "tool not found", "tool is not registered"]) {
        "原生成员调用了不可用的工具，已保留现有执行记录"
    } else if contains(&[
        "context length",
        "maximum context",
        "max_tokens",
        "invalid request",
        "400",
    ]) {
        "模型服务拒绝了请求参数，请检查当前模型与上下文设置"
    } else {
        "原生成员执行失败，已保留现有记录，可继续当前协作"
    };
    // 限流/额度类失败只额外带出重置时间（不带原文），协调器据此等到重置再继续。
    let reset = if category.starts_with("模型服务限流") || category.starts_with("模型服务额度") {
        let (offset, now) = crate::quota_reset::local_offset();
        crate::quota_reset::reset_at(&text, now, offset).map(crate::quota_reset::marker)
    } else {
        None
    };
    format!(
        "{category}（协议代码 {}）{}",
        error["code"].as_i64().unwrap_or(-1),
        reset.unwrap_or_default()
    )
}

pub struct Client {
    child: Mutex<Child>,
    writer: Mutex<Option<ChildStdin>>,
    pending: Mutex<Pending>,
    next_id: AtomicU64,
    request_handler: Mutex<Option<RequestHandler>>,
    listeners: Mutex<HashMap<u64, MessageListener>>,
    _scope: ProcessScope,
}

impl Client {
    pub fn spawn(
        executable: &PathBuf,
        on_message: impl Fn(Value) + Send + 'static,
        on_exit: impl Fn() + Send + 'static,
    ) -> Result<Arc<Self>> {
        let mut command = Command::new(executable);
        command.args(["app-server", "--listen", "stdio://"]);
        Self::spawn_command(command, on_message, on_exit)
    }

    pub fn spawn_command(
        mut command: Command,
        on_message: impl Fn(Value) + Send + 'static,
        on_exit: impl Fn() + Send + 'static,
    ) -> Result<Arc<Self>> {
        // 原生子进程的 stderr 默认丢弃；排障时设 AGENT_HUB_NATIVE_STDERR=<文件> 抓下来。
        let stderr = std::env::var_os("AGENT_HUB_NATIVE_STDERR")
            .and_then(|path| std::fs::File::create(path).ok())
            .map(Stdio::from)
            .unwrap_or_else(Stdio::null);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000); // CREATE_NO_WINDOW for background harness
        }
        let mut child = command
            .spawn()
            .map_err(|_| "原生 agent 原生程序启动失败".to_string())?;
        let scope = match ProcessScope::attach(&child) {
            Ok(scope) => scope,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        let writer = child.stdin.take();
        let stdout = child.stdout.take().ok_or("原生 agent 输出管道不可用")?;
        let client = Arc::new(Self {
            child: Mutex::new(child),
            writer: Mutex::new(writer),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            request_handler: Mutex::new(None),
            listeners: Mutex::new(HashMap::new()),
            _scope: scope,
        });
        let reader_client = client.clone();
        std::thread::spawn(move || {
            // Logs/diagnostics from the harness are never forwarded as chat or saved.
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else {
                    break;
                };
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if value.get("method").is_some() {
                    if let Some(id) = value.get("id") {
                        let handler = reader_client.request_handler.lock().unwrap().clone();
                        if let Some(handler) = handler {
                            // The reader must remain free to receive nested RPC responses.
                            let responder = reader_client.clone();
                            let id = id.clone();
                            std::thread::spawn(move || {
                                let result = handler(
                                    value["method"].as_str().unwrap_or(""),
                                    &value["params"],
                                );
                                let response = match result {
                                    Some(Ok(result)) => json!({"id":id,"result":result}),
                                    Some(Err(_)) => {
                                        json!({"id":id,"error":{"code":-32603,"message":"Project request rejected"}})
                                    }
                                    None => {
                                        json!({"id":id,"error":{"code":-32601,"message":"Unsupported project request"}})
                                    }
                                };
                                let _ = responder.write(&response);
                            });
                            continue;
                        }
                        // Chat stage does not support interactive tool approvals/forms.
                        let result = match value["method"].as_str().unwrap_or("") {
                            "item/commandExecution/requestApproval"
                            | "item/fileChange/requestApproval" => json!({"decision":"decline"}),
                            "item/tool/requestUserInput" => json!({"answers":{}}),
                            "session/request_permission" => {
                                json!({"outcome":{"outcome":"cancelled"}})
                            }
                            _ => Value::Null,
                        };
                        let response = if result.is_null() {
                            json!({"id":id,"error":{"code":-32601,"message":"Unsupported client request in chat stage"}})
                        } else {
                            json!({"id":id,"result":result})
                        };
                        let _ = reader_client.write(&response);
                    } else {
                        let listeners = reader_client
                            .listeners
                            .lock()
                            .unwrap()
                            .values()
                            .cloned()
                            .collect::<Vec<_>>();
                        for listener in listeners {
                            listener(value.clone());
                        }
                        on_message(value);
                    }
                } else if let Some(id) = value["id"].as_u64() {
                    let sender = reader_client.pending.lock().unwrap().remove(&id);
                    if let Some(sender) = sender {
                        let result = if value.get("error").is_some() {
                            // Do not expose raw native errors: they may include configuration.
                            Err(native_error_summary(&value["error"]))
                        } else {
                            Ok(value["result"].clone())
                        };
                        let _ = sender.send(result);
                    }
                }
            }
            reader_client.fail_pending("原生 agent 连接已断开");
            let listeners = reader_client
                .listeners
                .lock()
                .unwrap()
                .values()
                .cloned()
                .collect::<Vec<_>>();
            for listener in listeners {
                listener(json!({"method":"agent_hub/transport_closed"}));
            }
            on_exit();
        });
        Ok(client)
    }

    pub fn write(&self, value: &Value) -> Result<()> {
        let mut guard = self.writer.lock().map_err(|_| "原生 agent 管道不可用")?;
        let writer = guard.as_mut().ok_or("原生 agent 已停止")?;
        let mut value = value.clone();
        value["jsonrpc"] = json!("2.0");
        serde_json::to_writer(&mut *writer, &value).map_err(|_| "原生 agent 请求序列化失败")?;
        writer
            .write_all(b"\n")
            .and_then(|_| writer.flush())
            .map_err(|_| "原生 agent 请求发送失败".into())
    }

    pub fn set_request_handler(&self, handler: RequestHandler) {
        *self.request_handler.lock().unwrap() = Some(handler);
    }

    pub fn rpc(&self, method: &str, params: Value) -> Result<Value> {
        self.rpc_timeout(method, params, Duration::from_secs(60))
    }

    pub fn rpc_timeout(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        self.rpc_timeout_with_sent(method, params, timeout, || {})
    }

    pub fn rpc_timeout_with_sent(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        on_sent: impl FnOnce(),
    ) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = mpsc::channel();
        self.pending.lock().unwrap().insert(id, sender);
        if let Err(error) = self.write(&json!({"id":id,"method":method,"params":params})) {
            self.pending.lock().unwrap().remove(&id);
            return Err(error);
        }
        on_sent();
        let result = receiver
            .recv_timeout(timeout)
            .map_err(|_| format!("原生 agent {method} 超时，请断开后重新连接"));
        self.pending.lock().unwrap().remove(&id);
        result?.map_err(|error| format!("原生 agent {method} 请求失败：{error}"))
    }

    fn fail_pending(&self, error: &str) {
        for (_, sender) in self.pending.lock().unwrap().drain() {
            let _ = sender.send(Err(error.into()));
        }
    }

    pub fn stop(&self) {
        self.writer.lock().unwrap().take();
        self.fail_pending("原生 agent 已停止");
        self._scope.terminate();
        let mut child = self.child.lock().unwrap();
        let _ = child.kill();
        let _ = child.wait();
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_errors_expose_only_a_safe_category_and_protocol_code() {
        let secret = "PRIVATE_NATIVE_TOKEN_DO_NOT_EXPOSE";
        let limited =
            native_error_summary(&json!({"code":-32603,"message":format!("HTTP 429 {secret}")}));
        assert!(limited.contains("限流"));
        assert!(!limited.contains(secret));
        let unknown = native_error_summary(&json!({"code":-32603,"message":secret}));
        assert!(unknown.contains("执行失败"));
        assert!(!unknown.contains("登录"));
        assert!(!unknown.contains(secret));
        assert!(
            native_error_summary(&json!({"code":-32603,"message":"fetch failed"}))
                .contains("连接失败")
        );
    }
    #[test]
    fn project_request_callback_can_use_nested_rpc_without_blocking_reader() {
        let mut command = Command::new("python");
        command.args([
            "-u",
            "-c",
            r#"
import json,sys
outer=None
def send(value): print(json.dumps(value),flush=True)
for line in sys.stdin:
    value=json.loads(line)
    if value.get('method')=='outer':
        outer=value['id']
        send({'id':'server-tool','method':'project/tool','params':{'fixture':True}})
    elif value.get('method')=='nested': send({'id':value['id'],'result':{'nested':True}})
    elif value.get('id')=='server-tool': send({'id':outer,'result':value.get('result')})
"#,
        ]);
        let client = Client::spawn_command(command, |_| {}, || {}).unwrap();
        let weak = Arc::downgrade(&client);
        client.set_request_handler(Arc::new(move |method, params| {
            if method != "project/tool" || params["fixture"] != true {
                return None;
            }
            Some(
                weak.upgrade()
                    .ok_or("client gone".into())
                    .and_then(|client| {
                        client.rpc_timeout("nested", json!({}), Duration::from_secs(3))
                    }),
            )
        }));
        let result = client.rpc_timeout("outer", json!({}), Duration::from_secs(5));
        client.stop();
        assert_eq!(result.unwrap(), json!({"nested":true}));
    }
}

#[cfg(test)]
mod reset_tests {
    use super::native_error_summary;
    use serde_json::json;

    #[test]
    fn rate_limit_summary_carries_only_the_reset_time() {
        let at = crate::quota_reset::now_secs() + 3600;
        let summary = native_error_summary(&json!({
            "code": -32603,
            "message": "Claude 返回额度或频率限制（HTTP 429 rate limit exceeded） token=secret",
            "data": {"resets_at": at}
        }));
        assert!(summary.starts_with("模型服务限流"), "{summary}");
        assert!(summary.contains(&format!("resets_at={at}")), "{summary}");
        assert!(!summary.contains("secret"), "{summary}");
        let plain = native_error_summary(&json!({"code": -32603, "message": "HTTP 429"}));
        assert!(!plain.contains("resets_at"), "{plain}");
    }
}
