use serde::Serialize;
use std::fs;

#[derive(Serialize)]
pub struct BattleReport {
    pub schema_version: String,
    pub started_at_ms: u128,
    pub setup_mode: String,
    pub winner: String,
    pub final_life_a: i32,
    pub final_life_b: i32,
    pub initial_state: InitialState,
    pub rounds: Vec<RoundReport>,
}

#[derive(Serialize)]
pub struct RoundReport {
    pub round: i32,
    pub round_label: String,
    pub acting_order: String,
    pub decision_a: String,
    pub decision_b: String,
    pub active_a: String,
    pub active_b: String,
    pub hp_a_max: i32,
    pub hp_b_max: i32,
    pub energy_a_max: i32,
    pub energy_b_max: i32,
    pub hp_a_before: i32,
    pub hp_b_before: i32,
    pub hp_a_after: i32,
    pub hp_b_after: i32,
    pub energy_a_before: i32,
    pub energy_b_before: i32,
    pub energy_a_after: i32,
    pub energy_b_after: i32,
    pub life_a_after: i32,
    pub life_b_after: i32,
    pub alive_a: Vec<String>,
    pub alive_b: Vec<String>,
    pub status_effects: Vec<PetStatusSnapshot>,
    pub events: Vec<EventFrame>,
}

#[derive(Serialize)]
pub struct InitialState {
    pub team_a: Vec<PetInitialState>,
    pub team_b: Vec<PetInitialState>,
}

#[derive(Serialize)]
pub struct PetInitialState {
    pub name: String,
    pub level: i32,
    pub element: String,
    pub element2: Option<String>,
    pub ability: String,
    pub hp_max: i32,
    pub energy_max: i32,
    pub speed: i32,
    pub patk: i32,
    pub pdef: i32,
    pub matk: i32,
    pub mdef: i32,
    pub skills: Vec<SkillView>,
}

#[derive(Serialize)]
pub struct SkillView {
    pub name: String,
    pub element: String,
    pub category: String,
    pub cost: i32,
    pub power: i32,
}

#[derive(Serialize)]
pub struct PetStatusSnapshot {
    pub side: String,
    pub pet: String,
    pub effects: Vec<StatusEffectView>,
}

#[derive(Serialize)]
pub struct StatusEffectView {
    pub name: String,
    pub layers: i32,
}

#[derive(Serialize)]
pub struct EventFrame {
    pub event_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_side: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skill: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anim_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amount: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub multiplier: Option<f32>,
    pub text: String,
}

pub fn events_to_frames(events: &[String]) -> Vec<EventFrame> {
    events.iter().map(|line| classify_event_frame(line)).collect::<Vec<_>>()
}

pub fn write_battle_report(report: &BattleReport) -> Result<String, String> {
    let dir = "data/battle-reports";
    fs::create_dir_all(dir).map_err(|e| format!("创建战报目录失败: {}", e))?;
    let filename = format!("{}/battle-report-{}.json", dir, report.started_at_ms);
    let content = serde_json::to_string_pretty(report).map_err(|e| format!("序列化战报失败: {}", e))?;
    fs::write(&filename, content).map_err(|e| format!("写入战报失败: {}", e))?;
    Ok(filename)
}

fn classify_event_frame(line: &str) -> EventFrame {
    let event_type = if line.contains("进入蓄力状态") {
        "CHARGE_START"
    } else if line.contains("使用 [回能]") || line.contains("回复") && line.contains("点能量") {
        "ENERGY_CHANGE"
    } else if line.contains("造成 ") && line.contains("伤害") {
        "DAMAGE"
    } else if line.contains("施加状态/效果") {
        "APPLY_EFFECT"
    } else if line.contains("获得 ") && line.contains("层") {
        "STATUS_APPLY"
    } else if line.contains("结算伤害") {
        "STATUS_TICK"
    } else if line.contains("倒下") {
        "FAINT"
    } else if line.contains("选择换宠") || line.contains("上场") {
        "SWITCH"
    } else {
        "TEXT"
    };
    let source = parse_side(line);
    let target_side = parse_target_side(line, source.as_deref());
    let skill = parse_bracket_name(line);
    EventFrame {
        event_type: event_type.to_string(),
        source,
        target_side,
        target: parse_target_name(line),
        anim_id: infer_anim_id(event_type, skill.as_deref()),
        skill,
        amount: parse_damage_amount(line),
        multiplier: parse_multiplier(line),
        text: line.to_string(),
    }
}

fn parse_side(text: &str) -> Option<String> {
    if text.contains("A方") {
        Some("A".to_string())
    } else if text.contains("B方") {
        Some("B".to_string())
    } else {
        None
    }
}

fn parse_target_side(text: &str, source: Option<&str>) -> Option<String> {
    if text.contains("A方") && !text.contains("B方") && source == Some("B") {
        return Some("A".to_string());
    }
    if text.contains("B方") && !text.contains("A方") && source == Some("A") {
        return Some("B".to_string());
    }
    match source {
        Some("A") => Some("B".to_string()),
        Some("B") => Some("A".to_string()),
        _ => None,
    }
}

fn infer_anim_id(event_type: &str, skill: Option<&str>) -> Option<String> {
    match (event_type, skill) {
        ("DAMAGE", Some(s)) | ("APPLY_EFFECT", Some(s)) | ("CHARGE_START", Some(s)) => {
            Some(format!("skill:{}", s))
        }
        ("SWITCH", _) => Some("switch:default".to_string()),
        ("FAINT", _) => Some("faint:default".to_string()),
        _ => None,
    }
}

fn parse_bracket_name(text: &str) -> Option<String> {
    let start = text.find('[')?;
    let rest = &text[start + 1..];
    let end = rest.find(']')?;
    Some(rest[..end].to_string())
}

fn parse_damage_amount(text: &str) -> Option<i32> {
    let marker = "造成 ";
    let pos = text.find(marker)?;
    let rest = &text[pos + marker.len()..];
    let digits = rest
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>();
    if digits.is_empty() {
        None
    } else {
        digits.parse::<i32>().ok()
    }
}

fn parse_multiplier(text: &str) -> Option<f32> {
    let start_marker = "克制x";
    let start = text.find(start_marker)?;
    let rest = &text[start + start_marker.len()..];
    let number = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect::<String>();
    if number.is_empty() {
        None
    } else {
        number.parse::<f32>().ok()
    }
}

fn parse_target_name(text: &str) -> Option<String> {
    let hp_marker = " HP: ";
    let idx = text.find(hp_marker)?;
    let prefix = &text[..idx];
    let split = prefix.rfind("。")?;
    let candidate = prefix[split + "。".len()..].trim();
    if candidate.is_empty() {
        None
    } else {
        Some(candidate.to_string())
    }
}
