use excalibur_browser::rpc::{MAX_MESSAGE, Protocol, Request};
use serde_json::{Value, json};
use std::{future::Future, path::PathBuf, pin::Pin, sync::Arc};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use zeroize::Zeroize;
pub trait BrowserBackend: Send + Sync {
    fn request<'a>(
        &'a self,
        method: &'a str,
        params: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send + 'a>>;
}
pub struct LocalBackend(pub Arc<Protocol>);
pub struct SocketBackend {
    pub socket: PathBuf,
}
fn erase(value: &mut Value) {
    match value {
        Value::String(s) => s.zeroize(),
        Value::Array(a) => {
            for v in a {
                erase(v)
            }
        }
        Value::Object(o) => {
            for v in o.values_mut() {
                erase(v)
            }
        }
        _ => {}
    }
}
fn response(v: Value) -> Result<Option<Value>, String> {
    if !v["error"].is_null() {
        return Err(format!(
            "{}:{}",
            v["error"]["message"].as_str().unwrap_or("browser error"),
            v["error"]["data"]["detail"].as_str().unwrap_or("")
        ));
    }
    if v["result"]["state"] == "working" {
        return Ok(None);
    }
    if v["result"]["state"] == "uncertain" {
        return Err("browser effect uncertain; reconciliation required".into());
    }
    Ok(Some(v["result"]["result"].clone()))
}
impl BrowserBackend for LocalBackend {
    fn request<'a>(
        &'a self,
        method: &'a str,
        mut params: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send + 'a>> {
        Box::pin(async move {
            loop {
                let r = Request {
                    jsonrpc: "2.0".into(),
                    id: json!(1),
                    method: method.into(),
                    params: params.clone(),
                };
                if method == "act.fill_secret" {
                    erase(&mut params)
                }
                let v = self.0.request(r).await;
                match response(v)? {
                    Some(v) => return Ok(v),
                    None => tokio::time::sleep(std::time::Duration::from_millis(100)).await,
                }
            }
        })
    }
}
impl BrowserBackend for SocketBackend {
    fn request<'a>(
        &'a self,
        method: &'a str,
        mut params: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send + 'a>> {
        Box::pin(async move {
            loop {
                let id = uuid::Uuid::new_v4().to_string();
                let mut encoded = zeroize::Zeroizing::new(
                    serde_json::to_vec(
                        &json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
                    )
                    .map_err(|e| e.to_string())?,
                );
                if method == "act.fill_secret" {
                    erase(&mut params)
                }
                if encoded.len() > MAX_MESSAGE {
                    return Err("browser request exceeds frame limit".into());
                }
                encoded.push(b'\n');
                let mut stream = tokio::net::UnixStream::connect(&self.socket)
                    .await
                    .map_err(|e| e.to_string())?;
                stream
                    .write_all(&encoded)
                    .await
                    .map_err(|_| "browser request interrupted; reconcile".to_owned())?;
                encoded.zeroize();
                let mut stream = BufReader::new(stream);
                let reply = loop {
                    let mut line = vec![];
                    let n = stream
                        .read_until(b'\n', &mut line)
                        .await
                        .map_err(|e| e.to_string())?;
                    if n == 0 {
                        return Err("browser closed connection; reconcile".into());
                    }
                    if n > MAX_MESSAGE {
                        return Err("browser response exceeds frame limit".into());
                    }
                    let v: Value =
                        serde_json::from_slice(&line).map_err(|_| "invalid browser response")?;
                    line.zeroize();
                    if v["id"] == id {
                        break v;
                    }
                };
                match response(reply)? {
                    Some(v) => return Ok(v),
                    None => tokio::time::sleep(std::time::Duration::from_millis(100)).await,
                }
            }
        })
    }
}
