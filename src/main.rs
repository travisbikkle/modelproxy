use clap::Parser;
use hyper::body::to_bytes;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Request, Response, Server};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use hyper::server::conn::AddrStream;

/// Anthropic 协议代理，将请求转发到实际后端
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// 监听地址，例如 127.0.0.1:8080 或 0.0.0.0:8080
    #[arg(short, long, default_value = "127.0.0.1:8080")]
    listen: SocketAddr,

    /// 后端基础 URL，例如 https://x.x.x.x:38455/apiaccess/modelrouter
    #[arg(short, long)]
    server: String,

    /// 强制使用的模型名称
    #[arg(short, long, default_value = "MiniMax-M2.7")]
    model: String,
}

async fn handle_request(
    req: Request<Body>,
    client: Arc<reqwest::Client>,
    target_base: Arc<str>,
    model: Arc<str>,
    remote_addr: SocketAddr,
) -> Result<Response<Body>, Infallible> {
    // HEAD 健康检查
    if req.method() == hyper::Method::HEAD {
        println!("\n[Health Check] HEAD {} -> 200 OK", req.uri());
        return Ok(Response::builder().status(200).body(Body::empty()).unwrap());
    }

    // 读取请求
    let (parts, body) = req.into_parts();
    let body_bytes = match to_bytes(body).await {
        Ok(b) => b,
        Err(e) => {
            eprintln!("读取请求体失败: {}", e);
            return Ok(Response::builder()
                .status(400)
                .body(Body::from("Failed to read body"))
                .unwrap());
        }
    };
    let body_str = String::from_utf8_lossy(&body_bytes);

    // 获取客户端 IP
    let client_ip = remote_addr.to_string();

    println!("\n==================== INCOMING ====================");
    println!("[Client IP] {}", client_ip);
    println!("[Method] {}", parts.method);
    println!("[URI]    {}", parts.uri);
    println!("[Headers]");
    for (k, v) in parts.headers.iter() {
        let key = k.as_str().to_lowercase();
        let value = if key == "authorization" || key == "x-api-key" {
            "***".to_string()
        } else {
            v.to_str().unwrap_or("<non-utf8>").to_string()
        };
        println!("  {}: {}", k, value);
    }
    // println!("[Body]   {}", body_str);
    println!("==================================================\n");

    // ----- 路径处理：将任何以 /v1/messages 开头的路径映射为 /v1/messages -----
    let path = parts.uri.path();
    let target_path = if path.starts_with("/v1/messages") {
        "/v1/messages".to_string()
    } else {
        path.to_string()
    };
    // 丢弃所有查询参数（包括 ?beta=true）
    let target_url = format!("{}{}", target_base, target_path);
    println!("[Forward] {} {}", parts.method, target_url);

    // ----- 头部处理 -----
    let mut new_headers = HeaderMap::new();
    let allowed_headers = [
        "content-type",
        "anthropic-version",
        "accept",
        "x-api-key",
    ];

    for (k, v) in parts.headers.iter() {
        let key = k.as_str().to_lowercase();

        // Authorization: Bearer -> x-api-key
        if key == "authorization" {
            if let Ok(val) = v.to_str() {
                if let Some(token) = val.strip_prefix("Bearer ") {
                    if let Ok(hv) = HeaderValue::from_str(token) {
                        new_headers.insert(
                            HeaderName::from_static("x-api-key"),
                            hv
                        );
                    }
                }
            }
            continue;
        }

        // 白名单保留
        if allowed_headers.contains(&key.as_str()) {
            if let Ok(hv) = HeaderValue::from_bytes(v.as_bytes()) {
                if let Ok(name) = HeaderName::from_bytes(k.as_str().as_bytes()) {
                    new_headers.insert(name, hv);
                }
            }
        }
    }

    // 如果还没有 x-api-key，从原始头取
    if !new_headers.contains_key("x-api-key") {
        if let Some(val) = parts.headers.get("x-api-key") {
            if let Ok(hv) = HeaderValue::from_bytes(val.as_bytes()) {
                new_headers.insert(
                    HeaderName::from_static("x-api-key"),
                    hv
                );
            }
        }
    }

    // ----- 强制模型名 -----
    let final_body = if let Ok(mut json) = serde_json::from_str::<serde_json::Value>(&body_str) {
        if let Some(obj) = json.as_object_mut() {
            obj.insert("model".to_string(), serde_json::Value::String(model.to_string()));
        }
        serde_json::to_string(&json).unwrap_or_else(|_| body_str.to_string())
    } else {
        body_str.to_string()
    };

    // 更新 Content-Length
    new_headers.insert(
        HeaderName::from_static("content-length"),
        HeaderValue::from_str(&final_body.len().to_string()).unwrap(),
    );

    // ----- 转发 -----
    let reqwest_req = client
        .request(
            reqwest::Method::from_bytes(parts.method.as_str().as_bytes()).unwrap(),
            &target_url,
        )
        .headers(new_headers)
        .body(final_body)
        .build()
        .unwrap();

    let resp = match client.execute(reqwest_req).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("转发请求失败: {:#?}", e);
            return Ok(Response::builder()
                .status(502)
                .body(Body::from(format!("Proxy error: {}", e)))
                .unwrap());
        }
    };

    let status = resp.status();
    let resp_headers = resp.headers().clone();

    println!("\n==================== RESPONSE ====================");
    println!("[Status] {}", status);
    println!("[Headers]");
    for (k, v) in resp_headers.iter() {
        println!("  {}: {}", k, v.to_str().unwrap_or("<non-utf8>"));
    }
    println!("[Streaming] Body will be streamed");
    println!("==================================================\n");

    // ----- 流式转发 SSE -----
    let stream = resp.bytes_stream();

    // 构建响应，保留所有头（除了 connection/transfer-encoding）
    let mut response_builder = Response::builder().status(status.as_u16());
    for (k, v) in resp_headers.iter() {
        let key = k.as_str().to_lowercase();
        if key != "connection" && key != "transfer-encoding" {
            response_builder = response_builder.header(k.as_str(), v.as_bytes());
        }
    }
    if !resp_headers.contains_key("content-type") {
        response_builder = response_builder.header("content-type", "text/event-stream; charset=utf-8");
    }

    let body = Body::wrap_stream(stream);
    return Ok(response_builder.body(body).unwrap());
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    let client = Arc::new(
        reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .no_proxy()
            .timeout(std::time::Duration::from_secs(600))
            .build()
            .expect("Failed to build reqwest client"),
    );

    println!("后端服务器: {}", args.server);
    let target_base: Arc<str> = args.server.into();
    let model: Arc<str> = args.model.into();

    let make_svc = make_service_fn(move |conn: &AddrStream| {
        let client = client.clone();
        let target_base = target_base.clone();
        let model = model.clone();
        let remote_addr = conn.remote_addr();
        async move {
            Ok::<_, Infallible>(service_fn(move |req| {
                let client = client.clone();
                let target_base = target_base.clone();
                let model = model.clone();
                let remote_addr = remote_addr;
                async move { handle_request(req, client, target_base, model, remote_addr).await }
            }))
        }
    });

    let server = Server::bind(&args.listen).serve(make_svc);
    println!("Author: The most handsome guy in CEM. If you don't agree, press Ctrl+C.");
    println!("🦀 proxy running at http://{}", args.listen);
    println!("Set ANTHROPIC_API_URL in ~/.claude/settings.json to http://{}", args.listen);
    println!("press Ctrl+C to stop");

    server.await?;
    Ok(())
}