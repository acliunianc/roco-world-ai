mod battle;
mod pet_battle_patch;
mod replay;

use battle::AiBattleSetupMode;
use dioxus::prelude::*;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::error::Error as StdError;
use std::fs;
use std::time::Duration;

fn main() {
    dioxus::launch(App);
}

#[derive(Clone, Copy, PartialEq)]
enum MainMode {
    None,
    AiBattle,
    HumanVsAi,
}

#[component]
fn App() -> Element {
    let mut main_mode = use_signal(|| MainMode::None);
    let mut ai_setup_mode = use_signal(|| AiBattleSetupMode::Random);
    let mut llm_status = use_signal(|| "检测中...".to_string());
    let mut fixed_team_a = use_signal(String::new);
    let mut fixed_team_b = use_signal(String::new);
    let mut battle_status = use_signal(|| "未开始".to_string());
    let mut battle_log = use_signal(String::new);

    use_effect(move || {
        spawn(async move {
            llm_status.set(check_llm_connectivity().await);
        });
    });

    rsx! {
        div {
            style: "padding: 24px; font-family: Segoe UI, sans-serif;",
            h1 { "洛克王国世界-AI" }
            p { "先完成基础入口：选择对战模式" }
            p {
                style: "margin-top: 8px;",
                "模型连通性：{llm_status}"
            }

            div {
                style: "display: flex; gap: 12px; margin-top: 16px;",
                button {
                    onclick: move |_| {
                        main_mode.set(MainMode::AiBattle);
                    },
                    "AI对战"
                }

                button {
                    onclick: move |_| {
                        main_mode.set(MainMode::HumanVsAi);
                    },
                    "人机对战"
                }
            }

            div {
                style: "margin-top: 20px; padding: 12px; border: 1px solid #ddd; border-radius: 8px;",
                {
                    match main_mode() {
                        MainMode::None => rsx! {
                            p { "当前模式：未选择模式" }
                        },
                        MainMode::HumanVsAi => rsx! {
                            p { "当前模式：人机对战（下一步实现）" }
                        },
                        MainMode::AiBattle => rsx! {
                            div {
                                h3 { "AI对战配置" }
                                p { "请选择对战生成方式：" }

                                div {
                                    style: "display: flex; align-items: center; gap: 8px; margin-bottom: 12px;",
                                    label { "模式" }
                                    select {
                                        value: if ai_setup_mode() == AiBattleSetupMode::Random { "random" } else { "fixed" },
                                        onchange: move |evt| {
                                            if evt.value() == "fixed" {
                                                ai_setup_mode.set(AiBattleSetupMode::Fixed);
                                            } else {
                                                ai_setup_mode.set(AiBattleSetupMode::Random);
                                            }
                                        },
                                        option { value: "random", "随机精灵对战" }
                                        option { value: "fixed", "固定精灵对战" }
                                    }
                                }

                                {
                                    match ai_setup_mode() {
                                        AiBattleSetupMode::Random => rsx! {
                                            p { "规则：系统按精灵池分别为双方 AI 随机选择 6 只精灵，每只精灵随机分配 4 个技能；允许重复。" }
                                        },
                                        AiBattleSetupMode::Fixed => rsx! {
                                            div {
                                                p { "规则：由玩家分别为双方 AI 指定 6 只精灵及其技能（当前为文本录入 demo）。" }

                                                p { "AI A 阵容（示例格式：精灵名:技能1,技能2,技能3,技能4）" }
                                                textarea {
                                                    rows: "6",
                                                    style: "width: 100%;",
                                                    value: "{fixed_team_a}",
                                                    oninput: move |evt| fixed_team_a.set(evt.value()),
                                                }

                                                p { style: "margin-top: 10px;", "AI B 阵容（示例格式：精灵名:技能1,技能2,技能3,技能4）" }
                                                textarea {
                                                    rows: "6",
                                                    style: "width: 100%;",
                                                    value: "{fixed_team_b}",
                                                    oninput: move |evt| fixed_team_b.set(evt.value()),
                                                }
                                            }
                                        },
                                    }
                                }

                                div {
                                    style: "margin-top: 14px; display: flex; gap: 10px; align-items: center;",
                                    button {
                                        onclick: move |_| {
                                            let setup_mode = ai_setup_mode();
                                            let team_a = fixed_team_a();
                                            let team_b = fixed_team_b();
                                            let mut status = battle_status;
                                            let mut log = battle_log;
                                            spawn(async move {
                                                status.set("对战进行中...".to_string());
                                                log.set(String::new());
                                                match battle::run_ai_battle_stream(setup_mode, team_a, team_b, |delta| {
                                                    let mut text = log();
                                                    text.push_str(delta);
                                                    log.set(text);
                                                }).await {
                                                    Ok(()) => {
                                                        status.set("对战完成".to_string());
                                                        if log().is_empty() {
                                                            log.set("流式对战完成，但没有收到文本内容。".to_string());
                                                        }
                                                    }
                                                    Err(err) => {
                                                        status.set("对战失败".to_string());
                                                        log.set(err);
                                                    }
                                                }
                                            });
                                        },
                                        "开始 AI 对战"
                                    }
                                    span { "状态：{battle_status}" }
                                }

                                {
                                    if !battle_log().is_empty() {
                                        rsx! {
                                            div {
                                                style: "margin-top: 12px;",
                                                p { "对战日志（模型返回）" }
                                                textarea {
                                                    rows: "14",
                                                    style: "width: 100%;",
                                                    value: "{battle_log}",
                                                    readonly: true,
                                                }
                                            }
                                        }
                                    } else {
                                        rsx! {}
                                    }
                                }
                            }
                        },
                    }
                }
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

async fn check_llm_connectivity() -> String {
    match load_env_file(".env.local") {
        Ok(env) => match verify_llm_api(env).await {
            Ok(msg) => format!("连接成功 - {}", msg),
            Err(err) => format!("连接失败 - {}", err),
        },
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

async fn verify_llm_api(env: HashMap<String, String>) -> Result<String, String> {
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

    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(8))
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| format!("创建 HTTP 客户端失败: {}", e))?;
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
        .map_err(|e| format_reqwest_error("发送请求失败", &endpoint, &e))?;

    let status = response.status();
    let body_text = response
        .text()
        .await
        .map_err(|e| format!("读取响应失败: {}", e))?;

    if !status.is_success() {
        let brief = body_text.chars().take(120).collect::<String>();
        return Err(format!("HTTP {}: {}", status, brief));
    }

    let parsed: ChatCompletionsResponse =
        serde_json::from_str(&body_text).map_err(|e| format!("解析响应失败: {}", e))?;

    let content = parsed
        .choices
        .first()
        .and_then(|c| c.message.content.clone())
        .unwrap_or_else(|| "模型已响应（空内容）".to_string());

    Ok(content.chars().take(80).collect::<String>())
}

fn format_reqwest_error(prefix: &str, endpoint: &str, err: &reqwest::Error) -> String {
    let kind = if err.is_timeout() {
        "请求超时"
    } else if err.is_connect() {
        "连接失败（DNS/TLS/网络）"
    } else if err.is_request() {
        "请求构造失败"
    } else if err.is_decode() {
        "响应解析失败"
    } else {
        "请求异常"
    };

    let mut chain = Vec::new();
    let mut source = err.source();
    while let Some(s) = source {
        chain.push(s.to_string());
        source = s.source();
    }
    let source_text = if chain.is_empty() {
        String::new()
    } else {
        format!(" | 详细原因: {}", chain.join(" -> "))
    };

    format!("{}: {} | url: {} | {}{}", prefix, err, endpoint, kind, source_text)
}


