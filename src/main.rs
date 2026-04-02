use dioxus::prelude::*;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::fs;

fn main() {
    let llm_status = check_llm_connectivity();
    dioxus::launch_with_props(App, AppProps { llm_status });
}

#[derive(Props, Clone, PartialEq)]
struct AppProps {
    llm_status: String,
}

#[component]
fn App(props: AppProps) -> Element {
    let mut selected_mode = use_signal(|| "未选择模式".to_string());

    rsx! {
        div {
            style: "padding: 24px; font-family: Segoe UI, sans-serif;",
            h1 { "洛克王国世界-AI" }
            p { "先完成基础入口：选择对战模式" }
            p {
                style: "margin-top: 8px;",
                "模型连通性：{props.llm_status}"
            }

            div {
                style: "display: flex; gap: 12px; margin-top: 16px;",
                button {
                    onclick: move |_| {
                        selected_mode.set("AI对战（基础 demo）".to_string());
                    },
                    "AI对战"
                }

                button {
                    onclick: move |_| {
                        selected_mode.set("人机对战（基础 demo）".to_string());
                    },
                    "人机对战"
                }
            }

            p {
                style: "margin-top: 16px;",
                "当前模式：{selected_mode}"
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct ChatCompletionsResponse {
    choices: Vec<Choice>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: AssistantMessage,
}

#[derive(Debug, Deserialize)]
struct AssistantMessage {
    content: Option<String>,
}

fn check_llm_connectivity() -> String {
    match load_env_file(".env.local").and_then(verify_llm_api) {
        Ok(msg) => format!("连接成功 - {}", msg),
        Err(err) => format!("连接失败 - {}", err),
    }
}

fn load_env_file(path: &str) -> Result<HashMap<String, String>, String> {
    let content = fs::read_to_string(path).map_err(|e| format!("读取 {} 失败: {}", path, e))?;
    let mut map = HashMap::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = trimmed.split_once('=') {
            map.insert(k.trim().to_string(), v.trim().to_string());
        }
    }

    Ok(map)
}

fn verify_llm_api(env: HashMap<String, String>) -> Result<String, String> {
    let api_key = env
        .get("OPENAI_API_KEY")
        .ok_or("缺少 OPENAI_API_KEY".to_string())?;
    let base_url = env
        .get("OPENAI_BASE_URL")
        .ok_or("缺少 OPENAI_BASE_URL".to_string())?;
    let model = env
        .get("LLM_MODEL")
        .ok_or("缺少 LLM_MODEL".to_string())?;

    let endpoint = format!(
        "{}/openai/v1/chat/completions",
        base_url.trim_end_matches('/')
    );
    let auth_header = format!("Bearer {}", api_key);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("创建运行时失败: {}", e))?;

    rt.block_on(async move {
        let client = reqwest::Client::new();
        let payload = json!({
            "model": model,
            "messages": [{"role": "user", "content": "Hello!"}],
            "max_tokens": 64
        });

        let response = client
            .post(&endpoint)
            .header(AUTHORIZATION, auth_header)
            .header(CONTENT_TYPE, "application/json")
            .json(&payload)
            .send()
            .await
            .map_err(|e| format!("请求失败: {}", e))?;

        let status = response.status();
        let body_text = response
            .text()
            .await
            .map_err(|e| format!("读取响应失败: {}", e))?;

        if !status.is_success() {
            let brief = body_text.chars().take(120).collect::<String>();
            return Err(format!("HTTP {}: {}", status, brief));
        }

        let parsed: ChatCompletionsResponse = serde_json::from_str(&body_text)
            .map_err(|e| format!("解析响应失败: {}", e))?;

        let content = parsed
            .choices
            .first()
            .and_then(|c| c.message.content.clone())
            .unwrap_or_else(|| "模型已响应（空内容）".to_string());

        Ok(content.chars().take(80).collect::<String>())
    })
}
