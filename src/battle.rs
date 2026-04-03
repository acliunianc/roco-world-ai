use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde::de::{self, Visitor};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::error::Error as StdError;
use std::fs;
use std::fmt;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::pet_battle_patch::{ElementSkillBanks, PetBattlePatchEntry, PetBattlePatchTable};
use crate::replay::{
    events_to_frames, write_battle_report, BattleReport, InitialState, PetInitialState,
    PetStatusSnapshot, RoundReport, SkillView, StatusEffectView,
};

const PVP_DEFAULT_LEVEL: i32 = 60;
const PVP_DEFAULT_INDIVIDUAL_VALUE: i32 = 0;
const PVP_DEFAULT_MAGNIFICATION: f32 = 1.0;
const PVP_EFFORT_BASE_MIN: i32 = 7;
const PVP_EFFORT_BASE_MAX: i32 = 10;
const PVP_EFFORT_MULTIPLIER: i32 = 4;
const LLM_MIN_INTERVAL_MS: u128 = 900;
const CHARGE_ENERGY_GAIN: i32 = 5;
const MAX_ENERGY: i32 = 10;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AiBattleSetupMode {
    Random,
    Fixed,
}

#[derive(Debug, Clone, Deserialize)]
struct ChatCompletionsResponse {
    choices: Vec<Choice>,
}

#[derive(Debug, Clone, Deserialize)]
struct Choice {
    message: AssistantMessage,
}

#[derive(Debug, Clone, Deserialize)]
struct AssistantMessage {
    content: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct PetJsonLite {
    name: String,
    element: String,
    #[serde(default)]
    element2: Option<String>,
    #[serde(default)]
    ability: String,
    #[serde(default, rename = "abilityDesc")]
    ability_desc: String,
    #[serde(default, deserialize_with = "de_i32_or_default")]
    hp: i32,
    #[serde(default, deserialize_with = "de_i32_or_default")]
    speed: i32,
    #[serde(default, rename = "physicalAttack", deserialize_with = "de_i32_or_default")]
    physical_attack: i32,
    #[serde(default, rename = "physicalDefense", deserialize_with = "de_i32_or_default")]
    physical_defense: i32,
    #[serde(default, rename = "magicAttack", deserialize_with = "de_i32_or_default")]
    magic_attack: i32,
    #[serde(default, rename = "magicDefense", deserialize_with = "de_i32_or_default")]
    magic_defense: i32,
    skills: Vec<String>,
    #[serde(default, rename = "skillDetails")]
    skill_details: Vec<SkillDetailLite>,
}

fn de_i32_or_default<'de, D>(deserializer: D) -> Result<i32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct I32OrDefaultVisitor;

    impl<'de> Visitor<'de> for I32OrDefaultVisitor {
        type Value = i32;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("an integer, numeric string, or null")
        }

        fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(value as i32)
        }

        fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(value as i32)
        }

        fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(value.round() as i32)
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(value.trim().parse::<i32>().unwrap_or(0))
        }

        fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            self.visit_str(&value)
        }

        fn visit_none<E>(self) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(0)
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(0)
        }
    }

    deserializer.deserialize_any(I32OrDefaultVisitor)
}

#[derive(Debug, Clone, Deserialize)]
struct SkillDetailLite {
    name: String,
    element: String,
    category: String,
    cost: String,
    power: String,
    #[serde(default)]
    effect: String,
}

#[derive(Clone)]
struct BattleSkill {
    name: String,
    element: String,
    category: String,
    cost: i32,
    power: i32,
    effect: String,
}

#[derive(Clone)]
struct BattlePet {
    name: String,
    level: i32,
    nature: String,
    ability: String,
    ability_desc: String,
    element1: String,
    element2: Option<String>,
    hp: i32,
    cur_hp: i32,
    speed: i32,
    patk: i32,
    pdef: i32,
    matk: i32,
    mdef: i32,
    energy: i32,
    /// 能量上限（常规为 MAX_ENERGY；补丁可提高到 99 等）
    energy_cap: i32,
    poison_layers: i32,
    burn_layers: i32,
    freeze_layers: i32,
    parasitic_layers: i32,
    frozen_hp_locked: i32,
    charging_skill: Option<String>,
    entry_bonus_power: i32,
    entry_leave_immune: bool,
    morph_stage: u8,
    effort: EffortValues,
    skills: Vec<BattleSkill>,
}

#[derive(Clone, Copy, Default)]
struct EffortValues {
    hp: i32,
    patk: i32,
    matk: i32,
    pdef: i32,
    mdef: i32,
    speed: i32,
}

#[derive(Clone)]
enum Decision {
    UseSkill(String),
    Switch(String),
    Charge,
}

#[derive(Clone)]
struct WeatherState {
    kind: WeatherKind,
    remain_turns: i32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WeatherKind {
    Rain,
    Sandstorm,
    Blizzard,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NegativeImprintKind {
    Thorn,
    Descend,
}

#[derive(Clone, Copy)]
struct NegativeImprint {
    kind: NegativeImprintKind,
    layers: i32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PositiveImprintKind {
    Charge,
    Photosynthesis,
}

#[derive(Clone, Copy)]
struct PositiveImprint {
    kind: PositiveImprintKind,
    layers: i32,
}

#[derive(Clone, Copy, Default)]
struct DedicationBuff {
    power_bonus_times: i32,
    poison_bonus_times: i32,
    lifesteal_bonus_times: i32,
    combo_bonus_times: i32,
    cost_reduction_times: i32,
}

pub async fn run_ai_battle_stream<F>(
    setup_mode: AiBattleSetupMode,
    fixed_team_a_raw: String,
    fixed_team_b_raw: String,
    mut on_delta: F,
) -> Result<(), String>
where
    F: FnMut(&str),
{
    let env = load_env_file(".env.local")?;
    let base_url = env.get("OPENAI_BASE_URL").ok_or("缺少 OPENAI_BASE_URL")?.to_string();
    let api_key = env.get("OPENAI_API_KEY").ok_or("缺少 OPENAI_API_KEY")?.to_string();
    let model = env.get("LLM_MODEL").ok_or("缺少 LLM_MODEL")?.to_string();

    let pet_pool = load_pet_pool("data/pets")?;
    let pet_index = build_pet_index(&pet_pool);
    let (team_a_raw, team_b_raw) = match setup_mode {
        AiBattleSetupMode::Random => generate_random_teams(&pet_pool)?,
        AiBattleSetupMode::Fixed => (parse_fixed_team(&fixed_team_a_raw, "AI A")?, parse_fixed_team(&fixed_team_b_raw, "AI B")?),
    };
    let pet_patches = PetBattlePatchTable::load_or_empty();
    let mut team_a = build_battle_team(&team_a_raw, &pet_index, "A", &pet_patches)?;
    let mut team_b = build_battle_team(&team_b_raw, &pet_index, "B", &pet_patches)?;
    let initial_state = InitialState {
        team_a: build_initial_team_state(&team_a),
        team_b: build_initial_team_state(&team_b),
    };
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(8))
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| format!("创建 HTTP 客户端失败: {}", e))?;

    on_delta("=== 对战开始（程序结算，AI仅决策）===\n");
    on_delta("规则载入完成：AI 已在决策前获得完整对战规则摘要。\n");
    on_delta(&format!("PVP等级已统一：所有精灵等级设为 {} 级。\n", PVP_DEFAULT_LEVEL));
    let mut a_active = 0usize;
    let mut b_active = 0usize;
    let mut history = String::new();
    let mut a_life = 4;
    let mut b_life = 4;
    let mut weather: Option<WeatherState> = None;
    let mut a_negative_imprint: Option<NegativeImprint> = None;
    let mut b_negative_imprint: Option<NegativeImprint> = None;
    let mut a_positive_imprint: Option<PositiveImprint> = None;
    let mut b_positive_imprint: Option<PositiveImprint> = None;
    let mut a_dedication = DedicationBuff::default();
    let mut b_dedication = DedicationBuff::default();
    let mut a_need_system_prompt = true;
    let mut b_need_system_prompt = true;
    let mut report_rounds: Vec<RoundReport> = Vec::new();
    let report_started_at_ms = current_millis();
    let mut a_ally_counter_bank: i32 = 0;
    let mut b_ally_counter_bank: i32 = 0;
    let mut a_element_banks = ElementSkillBanks::default();
    let mut b_element_banks = ElementSkillBanks::default();

    apply_entry_effects(
        "A",
        &mut team_a[a_active],
        a_negative_imprint,
        a_positive_imprint,
        &pet_patches,
        &mut a_ally_counter_bank,
        &mut a_element_banks,
        &mut history,
        &mut on_delta,
    );
    apply_entry_effects(
        "B",
        &mut team_b[b_active],
        b_negative_imprint,
        b_positive_imprint,
        &pet_patches,
        &mut b_ally_counter_bank,
        &mut b_element_banks,
        &mut history,
        &mut on_delta,
    );

    for round in 1..=200 {
        if a_life <= 0 || b_life <= 0 {
            break;
        }
        if a_active >= team_a.len() || team_a[a_active].cur_hp <= 0 {
            if let Some(idx) = first_alive_index(&team_a) {
                a_active = idx;
            } else {
                a_active = team_a.len();
            }
        }
        if b_active >= team_b.len() || team_b[b_active].cur_hp <= 0 {
            if let Some(idx) = first_alive_index(&team_b) {
                b_active = idx;
            } else {
                b_active = team_b.len();
            }
        }
        if a_active >= team_a.len() || b_active >= team_b.len() {
            break;
        }

        let round_header = format!("\n【回合 {}】\n", round);
        history.push_str(&round_header);
        on_delta(&round_header);
        let round_log_start_len = history.len();
        let a_snapshot = team_a[a_active].clone();
        let b_snapshot = team_b[b_active].clone();
        let hp_a_before = a_snapshot.cur_hp;
        let hp_b_before = b_snapshot.cur_hp;
        let energy_a_before = a_snapshot.energy;
        let energy_b_before = b_snapshot.energy;

        let mut a_decision = llm_choose_action(
            &client,
            &base_url,
            &api_key,
            &model,
            "A",
            a_need_system_prompt,
            &a_snapshot,
            &b_snapshot,
            &team_a,
            a_active,
            weather.as_ref(),
            a_negative_imprint,
            a_positive_imprint,
            b_negative_imprint,
            b_positive_imprint,
            &history,
            round,
        )
        .await
        .unwrap_or_else(|_| Decision::UseSkill(a_snapshot.skills[0].name.clone()));
        a_need_system_prompt = false;
        let mut b_decision = llm_choose_action(
            &client,
            &base_url,
            &api_key,
            &model,
            "B",
            b_need_system_prompt,
            &b_snapshot,
            &a_snapshot,
            &team_b,
            b_active,
            weather.as_ref(),
            b_negative_imprint,
            b_positive_imprint,
            a_negative_imprint,
            a_positive_imprint,
            &history,
            round,
        )
        .await
        .unwrap_or_else(|_| Decision::UseSkill(b_snapshot.skills[0].name.clone()));
        b_need_system_prompt = false;

        enforce_charging_constraint(&team_a[a_active], &mut a_decision);
        enforce_charging_constraint(&team_b[b_active], &mut b_decision);

        let a_priority = match &a_decision {
            Decision::UseSkill(skill) => skill_priority(&a_snapshot, skill),
            Decision::Switch(_) => 99,
            Decision::Charge => 0,
        };
        let b_priority = match &b_decision {
            Decision::UseSkill(skill) => skill_priority(&b_snapshot, skill),
            Decision::Switch(_) => 99,
            Decision::Charge => 0,
        };
        let a_counter = counter_triggered(&a_snapshot, &a_decision, &b_decision, &b_snapshot);
        let b_counter = counter_triggered(&b_snapshot, &b_decision, &a_decision, &a_snapshot);
        let a_counter_priority = counter_grants_act_priority(&a_snapshot, &a_decision, &b_decision, &b_snapshot);
        let b_counter_priority = counter_grants_act_priority(&b_snapshot, &b_decision, &a_decision, &a_snapshot);
        let a_first = if a_counter_priority && !b_counter_priority {
            true
        } else if b_counter_priority && !a_counter_priority {
            false
        } else if a_priority == b_priority {
            effective_speed(&a_snapshot) >= effective_speed(&b_snapshot)
        } else {
            a_priority > b_priority
        };
        let acting_order = if a_first { "A_then_B" } else { "B_then_A" }.to_string();

        if a_counter && !b_counter {
            let nm = team_a[a_active].name.as_str();
            if !pet_patches.is_ally_counter_bank_receiver(nm) {
                a_ally_counter_bank = a_ally_counter_bank.saturating_add(1);
            }
        }
        if b_counter && !a_counter {
            let nm = team_b[b_active].name.as_str();
            if !pet_patches.is_ally_counter_bank_receiver(nm) {
                b_ally_counter_bank = b_ally_counter_bank.saturating_add(1);
            }
        }

        if a_first {
            a_decision = redecide_if_energy_insufficient(
                &client,
                &base_url,
                &api_key,
                &model,
                "A",
                false,
                a_decision,
                &team_a,
                a_active,
                &team_b[b_active],
                weather.as_ref(),
                a_negative_imprint,
                a_positive_imprint,
                b_negative_imprint,
                b_positive_imprint,
                &mut history,
                round,
            )
            .await;
            execute_decision(
                "A",
                &a_decision,
                &mut team_a,
                &mut a_active,
                &mut team_b[b_active],
                &mut weather,
                &mut a_negative_imprint,
                &mut b_negative_imprint,
                &mut a_positive_imprint,
                &mut b_positive_imprint,
                &mut a_dedication,
                &pet_patches,
                &mut a_ally_counter_bank,
                &mut b_ally_counter_bank,
                &mut a_element_banks,
                &mut b_element_banks,
                &mut history,
                &mut on_delta,
            );
            if team_b[b_active].cur_hp > 0 && a_active < team_a.len() && team_a[a_active].cur_hp > 0 {
                b_decision = redecide_if_energy_insufficient(
                    &client,
                    &base_url,
                    &api_key,
                    &model,
                    "B",
                    false,
                    b_decision,
                    &team_b,
                    b_active,
                    &team_a[a_active],
                    weather.as_ref(),
                    b_negative_imprint,
                    b_positive_imprint,
                    a_negative_imprint,
                    a_positive_imprint,
                    &mut history,
                    round,
                )
                .await;
                execute_decision(
                    "B",
                    &b_decision,
                    &mut team_b,
                    &mut b_active,
                    &mut team_a[a_active],
                    &mut weather,
                    &mut b_negative_imprint,
                    &mut a_negative_imprint,
                    &mut b_positive_imprint,
                    &mut a_positive_imprint,
                    &mut b_dedication,
                    &pet_patches,
                    &mut a_ally_counter_bank,
                    &mut b_ally_counter_bank,
                    &mut a_element_banks,
                    &mut b_element_banks,
                    &mut history,
                    &mut on_delta,
                );
            }
        } else {
            b_decision = redecide_if_energy_insufficient(
                &client,
                &base_url,
                &api_key,
                &model,
                "B",
                false,
                b_decision,
                &team_b,
                b_active,
                &team_a[a_active],
                weather.as_ref(),
                b_negative_imprint,
                b_positive_imprint,
                a_negative_imprint,
                a_positive_imprint,
                &mut history,
                round,
            )
            .await;
            execute_decision(
                "B",
                &b_decision,
                &mut team_b,
                &mut b_active,
                &mut team_a[a_active],
                &mut weather,
                &mut b_negative_imprint,
                &mut a_negative_imprint,
                &mut b_positive_imprint,
                &mut a_positive_imprint,
                &mut b_dedication,
                &pet_patches,
                &mut a_ally_counter_bank,
                &mut b_ally_counter_bank,
                &mut a_element_banks,
                &mut b_element_banks,
                &mut history,
                &mut on_delta,
            );
            if team_a[a_active].cur_hp > 0 && b_active < team_b.len() && team_b[b_active].cur_hp > 0 {
                a_decision = redecide_if_energy_insufficient(
                    &client,
                    &base_url,
                    &api_key,
                    &model,
                    "A",
                    false,
                    a_decision,
                    &team_a,
                    a_active,
                    &team_b[b_active],
                    weather.as_ref(),
                    a_negative_imprint,
                    a_positive_imprint,
                    b_negative_imprint,
                    b_positive_imprint,
                    &mut history,
                    round,
                )
                .await;
                execute_decision(
                    "A",
                    &a_decision,
                    &mut team_a,
                    &mut a_active,
                    &mut team_b[b_active],
                    &mut weather,
                    &mut a_negative_imprint,
                    &mut b_negative_imprint,
                    &mut a_positive_imprint,
                    &mut b_positive_imprint,
                    &mut a_dedication,
                    &pet_patches,
                    &mut a_ally_counter_bank,
                    &mut b_ally_counter_bank,
                    &mut a_element_banks,
                    &mut b_element_banks,
                    &mut history,
                    &mut on_delta,
                );
            }
        }

        apply_weather_end_turn(&mut weather, &mut team_a[a_active], &mut team_b[b_active], &mut history, &mut on_delta);
        apply_end_turn_status(&mut team_a[a_active], &mut team_b[b_active], &mut history, &mut on_delta);
        apply_end_turn_status(&mut team_b[b_active], &mut team_a[a_active], &mut history, &mut on_delta);
        apply_positive_imprint_end_turn("A", &mut team_a[a_active], a_positive_imprint, &mut history, &mut on_delta);
        apply_positive_imprint_end_turn("B", &mut team_b[b_active], b_positive_imprint, &mut history, &mut on_delta);

        if team_a[a_active].cur_hp <= 0 {
            a_life -= 1;
            on_delta(&format!("A方损失1点生命值，剩余 {}\n", a_life));
        }
        if team_b[b_active].cur_hp <= 0 {
            b_life -= 1;
            on_delta(&format!("B方损失1点生命值，剩余 {}\n", b_life));
        }
        let round_body = if history.len() > round_log_start_len {
            history[round_log_start_len..].to_string()
        } else {
            String::new()
        };
        let event_lines = round_body
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        report_rounds.push(RoundReport {
            round,
            round_label: format!("回合{}", round),
            acting_order,
            decision_a: decision_to_text(&a_decision),
            decision_b: decision_to_text(&b_decision),
            active_a: team_a[a_active].name.clone(),
            active_b: team_b[b_active].name.clone(),
            hp_a_max: team_a[a_active].hp,
            hp_b_max: team_b[b_active].hp,
            energy_a_max: team_a[a_active].energy_cap,
            energy_b_max: team_b[b_active].energy_cap,
            hp_a_before,
            hp_b_before,
            hp_a_after: team_a[a_active].cur_hp,
            hp_b_after: team_b[b_active].cur_hp,
            energy_a_before,
            energy_b_before,
            energy_a_after: team_a[a_active].energy,
            energy_b_after: team_b[b_active].energy,
            life_a_after: a_life,
            life_b_after: b_life,
            alive_a: alive_pet_names(&team_a),
            alive_b: alive_pet_names(&team_b),
            status_effects: snapshot_status_effects(&team_a[a_active], &team_b[b_active]),
            events: events_to_frames(&event_lines),
        });
    }

    let winner = resolve_winner_no_draw(a_life, b_life, &team_a, &team_b);
    match winner.as_str() {
        "A" => on_delta("\n=== 对战结束：A 方获胜 ===\n"),
        "B" => on_delta("\n=== 对战结束：B 方获胜 ===\n"),
        _ => on_delta("\n=== 对战结束：A 方获胜（决胜规则） ===\n"),
    }
    let report = BattleReport {
        schema_version: "2.0".to_string(),
        started_at_ms: report_started_at_ms,
        setup_mode: setup_mode_text(setup_mode).to_string(),
        winner,
        final_life_a: a_life,
        final_life_b: b_life,
        initial_state,
        rounds: report_rounds,
    };
    let path = write_battle_report(&report)?;
    on_delta(&format!("结构化战报已保存：{}\n", path));
    Ok(())
}

fn team_should_tally_element_skill_use(
    pet_patches: &PetBattlePatchTable,
    team: &[BattlePet],
    attacker_name: &str,
    skill_element: &str,
) -> bool {
    if skill_element != "冰" && skill_element != "火" && skill_element != "地" {
        return false;
    }
    team.iter().filter(|p| p.cur_hp > 0).any(|receiver| {
        let Some(e) = pet_patches.get(&receiver.name) else {
            return false;
        };
        let Some(rule) = &e.on_entry_per_element_skill_bank else {
            return false;
        };
        if rule.element != skill_element {
            return false;
        }
        !rule.exclude_self_as_attacker || receiver.name != attacker_name
    })
}

fn tally_element_skill_use_for_side(
    own_team: &[BattlePet],
    attacker_name: &str,
    skill_element: &str,
    banks: &mut ElementSkillBanks,
    pet_patches: &PetBattlePatchTable,
) {
    if !team_should_tally_element_skill_use(pet_patches, own_team, attacker_name, skill_element) {
        return;
    }
    match skill_element {
        "冰" => banks.ice = banks.ice.saturating_add(1),
        "火" => banks.fire = banks.fire.saturating_add(1),
        "地" => banks.earth = banks.earth.saturating_add(1),
        _ => {}
    }
}

/// 若本回合成功出手（含蓄力出手），返回 `(出手精灵名, 技能属性)` 供冰/火/地入场补丁计数。
fn resolve_one_attack<F: FnMut(&str)>(
    side: &str,
    chosen_skill: &str,
    attacker: &mut BattlePet,
    defender: &mut BattlePet,
    dedication: &DedicationBuff,
    weather: &mut Option<WeatherState>,
    history: &mut String,
    on_delta: &mut F,
) -> Option<(String, String)> {
    let skill = attacker
        .skills
        .iter()
        .find(|s| s.name == chosen_skill)
        .cloned()
        .unwrap_or_else(|| attacker.skills[0].clone());
    let dedication_applied = dedication_applies_to_skill(&skill.name);
    let base_cost = calc_skill_cost(&skill, weather.as_ref());
    let dedication_cost_down = if dedication_applied { dedication.cost_reduction_times * 2 } else { 0 };
    let actual_cost = (base_cost - dedication_cost_down).max(0);
    if attacker.energy < actual_cost {
        let msg = format!("{}方 {} 试图使用 [{}]，能量不足，行动失败。\n", side, attacker.name, skill.name);
        history.push_str(&msg);
        on_delta(&msg);
        return None;
    }
    attacker.energy -= actual_cost;

    let is_releasing_charged_skill = attacker.charging_skill.as_deref() == Some(skill.name.as_str());
    if skill.effect.contains("蓄力") && !is_releasing_charged_skill {
        attacker.charging_skill = Some(skill.name.clone());
        let msg = format!("{}方 {} 使用 [{}] 进入蓄力状态，本回合不造成伤害。\n", side, attacker.name, skill.name);
        history.push_str(&msg);
        on_delta(&msg);
        apply_weather_from_skill(weather, &skill, history, on_delta);
        return Some((attacker.name.clone(), skill.element.clone()));
    }

    let type_mul = type_multiplier(&skill.element, &defender.element1, defender.element2.as_deref());
    let stab = if skill.element == attacker.element1 || attacker.element2.as_deref() == Some(skill.element.as_str()) {
        1.5
    } else {
        1.0
    };
    let attacker_stat_mul = morph_stat_multiplier(attacker);
    let defender_stat_mul = morph_stat_multiplier(defender);
    let (atk, def) = if skill.category == "魔攻" {
        (
            (
                attacker.matk.max(1) as f32
                    * attacker_stat_mul
                    * nature_multiplier(&attacker.nature, "特攻")
            )
            .max(1.0),
            (
                defender.mdef.max(1) as f32
                    * defender_stat_mul
                    * nature_multiplier(&defender.nature, "特防")
            )
            .max(1.0),
        )
    } else {
        (
            (
                attacker.patk.max(1) as f32
                    * attacker_stat_mul
                    * nature_multiplier(&attacker.nature, "攻击")
            )
            .max(1.0),
            (
                defender.pdef.max(1) as f32
                    * defender_stat_mul
                    * nature_multiplier(&defender.nature, "防御")
            )
            .max(1.0),
        )
    };
    let weather_mul = weather_damage_multiplier(weather.as_ref(), &skill);
    let dedication_power_bonus = if dedication_applied { dedication.power_bonus_times * 20 } else { 0 };
    let power = skill.power.max(0) + attacker.entry_bonus_power + dedication_power_bonus;
    let is_damage_skill = (skill.category == "物攻" || skill.category == "魔攻") && power > 0;
    let dedication_combo_bonus = if dedication_applied { dedication.combo_bonus_times } else { 0 };
    let hit_count = (extract_multi_hit(&skill.effect) + dedication_combo_bonus).max(1);
    let dmg = if is_damage_skill {
        (((atk / def) * 0.9 * (power as f32) * type_mul * stab * weather_mul).floor() as i32 * hit_count).max(0)
    } else {
        0
    };
    defender.cur_hp = (defender.cur_hp - dmg).max(0);
    attacker.entry_bonus_power = 0;

    let msg = if is_damage_skill {
        if (type_mul - 1.0).abs() < 0.001 {
            format!(
                "{}方 {} 使用 [{}]，消耗{}能量，造成 {} 伤害。{} HP: {}/{}\n",
                side, attacker.name, skill.name, actual_cost, dmg, defender.name, defender.cur_hp, defender.hp
            )
        } else {
            format!(
                "{}方 {} 使用 [{}]，消耗{}能量，造成 {} 伤害(克制x{:.2})。{} HP: {}/{}\n",
                side, attacker.name, skill.name, actual_cost, dmg, type_mul, defender.name, defender.cur_hp, defender.hp
            )
        }
    } else {
        format!(
            "{}方 {} 使用 [{}]，消耗{}能量，施加状态/效果。{} HP: {}/{}\n",
            side, attacker.name, skill.name, actual_cost, defender.name, defender.cur_hp, defender.hp
        )
    };
    history.push_str(&msg);
    on_delta(&msg);
    if defender.cur_hp <= 0 {
        let faint = format!("{} 倒下。\n", defender.name);
        history.push_str(&faint);
        on_delta(&faint);
    }

    apply_status_from_skill(defender, &skill, history, on_delta);
    if dedication_applied && dedication.poison_bonus_times > 0 && !is_immune_to_poison(defender) {
        let poison_layers = dedication.poison_bonus_times * 2;
        defender.poison_layers += poison_layers;
        let msg = format!("奉献强化触发：{} 额外获得 {} 层中毒。\n", defender.name, poison_layers);
        history.push_str(&msg);
        on_delta(&msg);
    }
    if dedication_applied && dedication.lifesteal_bonus_times > 0 && attacker.cur_hp > 0 {
        let mut ratio = 0.2 * dedication.lifesteal_bonus_times as f32;
        if ratio > 1.0 {
            ratio = 1.0;
        }
        let heal = ((dmg as f32) * ratio).round() as i32;
        if heal > 0 {
            attacker.cur_hp = (attacker.cur_hp + heal).min(attacker.hp - attacker.frozen_hp_locked);
            let msg = format!("奉献强化触发：{} 吸血 {}，HP {}/{}。\n", attacker.name, heal, attacker.cur_hp, attacker.hp);
            history.push_str(&msg);
            on_delta(&msg);
        }
    }
    if skill.effect.contains("恢复") {
        let cap = attacker.energy_cap;
        attacker.energy = add_energy_clamped(attacker.energy, 5, cap);
        let msg = format!("{}方 {} 的恢复效果触发，回复 5 点能量（当前{}）。\n", side, attacker.name, attacker.energy);
        history.push_str(&msg);
        on_delta(&msg);
    }
    apply_weather_from_skill(weather, &skill, history, on_delta);
    if is_releasing_charged_skill {
        attacker.charging_skill = None;
    }
    Some((attacker.name.clone(), skill.element.clone()))
}

fn apply_end_turn_status<F: FnMut(&str)>(pet: &mut BattlePet, opp: &mut BattlePet, history: &mut String, on_delta: &mut F) {
    if pet.poison_layers > 0 && pet.cur_hp > 0 {
        let mul = type_multiplier("毒", &pet.element1, pet.element2.as_deref());
        let dmg = (((pet.hp as f32) * 0.03).floor() as f32 * pet.poison_layers.max(1) as f32 * mul).round() as i32;
        pet.cur_hp = (pet.cur_hp - dmg.max(1)).max(0);
        let msg = format!(
            "{} 受到中毒({}层)结算伤害 {}，剩余 HP {}/{}\n",
            pet.name, pet.poison_layers, dmg.max(1), pet.cur_hp, pet.hp
        );
        history.push_str(&msg);
        on_delta(&msg);
    }
    if pet.burn_layers > 0 && pet.cur_hp > 0 {
        let mul = type_multiplier("火", &pet.element1, pet.element2.as_deref());
        let dmg = (((pet.hp as f32) * 0.02).floor() as f32 * pet.burn_layers.max(1) as f32 * mul).round() as i32;
        pet.cur_hp = (pet.cur_hp - dmg.max(1)).max(0);
        let msg = format!(
            "{} 受到灼烧({}层)结算伤害 {}，剩余 HP {}/{}\n",
            pet.name, pet.burn_layers, dmg.max(1), pet.cur_hp, pet.hp
        );
        history.push_str(&msg);
        on_delta(&msg);
        // 文档：灼烧每回合衰减一半，向上取整，最少衰减1层
        let decay = (pet.burn_layers + 1) / 2;
        pet.burn_layers = (pet.burn_layers - decay.max(1)).max(0);
    }
    if pet.parasitic_layers > 0 && pet.cur_hp > 0 {
        let dmg = ((pet.hp as f32) * 0.06).floor() as i32 * pet.parasitic_layers.max(1);
        pet.cur_hp = (pet.cur_hp - dmg.max(1)).max(0);
        let msg = format!(
            "{} 受到寄生({}层)结算伤害 {}，剩余 HP {}/{}\n",
            pet.name, pet.parasitic_layers, dmg.max(1), pet.cur_hp, pet.hp
        );
        history.push_str(&msg);
        on_delta(&msg);
        if opp.cur_hp > 0 {
            opp.cur_hp = (opp.cur_hp + dmg.max(1)).min(opp.hp - opp.frozen_hp_locked).max(1);
        }
    }
}

fn execute_decision<F: FnMut(&str)>(
    side: &str,
    decision: &Decision,
    own_team: &mut [BattlePet],
    own_active: &mut usize,
    opp_active_pet: &mut BattlePet,
    weather: &mut Option<WeatherState>,
    own_negative_imprint: &mut Option<NegativeImprint>,
    opp_negative_imprint: &mut Option<NegativeImprint>,
    own_positive_imprint: &mut Option<PositiveImprint>,
    _opp_positive_imprint: &mut Option<PositiveImprint>,
    own_dedication: &mut DedicationBuff,
    pet_patches: &PetBattlePatchTable,
    a_ally_counter_bank: &mut i32,
    b_ally_counter_bank: &mut i32,
    a_element_banks: &mut ElementSkillBanks,
    b_element_banks: &mut ElementSkillBanks,
    history: &mut String,
    on_delta: &mut F,
) {
    match decision {
        Decision::UseSkill(skill) => {
            if *own_active < own_team.len() {
                let used_skill_name = skill.clone();
                let tally = resolve_one_attack(
                    side,
                    skill,
                    &mut own_team[*own_active],
                    opp_active_pet,
                    own_dedication,
                    weather,
                    history,
                    on_delta,
                );
                if let Some((an, el)) = tally {
                    tally_element_skill_use_for_side(
                        &*own_team,
                        &an,
                        &el,
                        if side == "A" {
                            a_element_banks
                        } else {
                            b_element_banks
                        },
                        pet_patches,
                    );
                }
                apply_field_imprint_from_skill(
                    &own_team[*own_active],
                    skill,
                    own_negative_imprint,
                    opp_negative_imprint,
                    own_positive_imprint,
                    history,
                    on_delta,
                );
                process_leave_keywords(
                    side,
                    &used_skill_name,
                    own_team,
                    own_active,
                    opp_active_pet,
                    weather,
                    *own_negative_imprint,
                    *own_positive_imprint,
                    own_dedication,
                    pet_patches,
                    a_ally_counter_bank,
                    b_ally_counter_bank,
                    a_element_banks,
                    b_element_banks,
                    history,
                    on_delta,
                );
                own_team[*own_active].entry_leave_immune = false;
                apply_dedication_gain_from_effect(
                    side,
                    &own_team[*own_active],
                    skill,
                    own_dedication,
                    history,
                    on_delta,
                );
            }
        }
        Decision::Switch(target_name) => {
            if let Some(idx) = find_switch_target(own_team, *own_active, target_name) {
                clear_switch_cleared_status(&mut own_team[*own_active]);
                *own_active = idx;
                own_team[*own_active].charging_skill = None;
                let msg = format!("{}方选择换宠，上场：{}。\n", side, own_team[*own_active].name);
                history.push_str(&msg);
                on_delta(&msg);
                let ally_bank = if side == "A" {
                    a_ally_counter_bank
                } else {
                    b_ally_counter_bank
                };
                apply_entry_effects(
                    side,
                    &mut own_team[*own_active],
                    *own_negative_imprint,
                    *own_positive_imprint,
                    pet_patches,
                    ally_bank,
                    if side == "A" {
                        a_element_banks
                    } else {
                        b_element_banks
                    },
                    history,
                    on_delta,
                );
                let swift_tally = trigger_swift_on_entry(
                    side,
                    &mut own_team[*own_active],
                    opp_active_pet,
                    own_dedication,
                    weather,
                    history,
                    on_delta,
                );
                if let Some((an, el)) = swift_tally {
                    let banks = if side == "A" {
                        a_element_banks
                    } else {
                        b_element_banks
                    };
                    tally_element_skill_use_for_side(&*own_team, &an, &el, banks, pet_patches);
                }
            } else {
                let msg = format!("{}方尝试换宠到 [{}] 失败（不存在或已倒下），本回合行动失败。\n", side, target_name);
                history.push_str(&msg);
                on_delta(&msg);
            }
        }
        Decision::Charge => {
            if *own_active < own_team.len() && own_team[*own_active].cur_hp > 0 {
                let cap = own_team[*own_active].energy_cap;
                own_team[*own_active].energy =
                    add_energy_clamped(own_team[*own_active].energy, CHARGE_ENERGY_GAIN, cap);
                let msg = format!(
                    "{}方 {} 使用 [回能]，回复 {} 点能量（当前{}）。\n",
                    side,
                    own_team[*own_active].name,
                    CHARGE_ENERGY_GAIN,
                    own_team[*own_active].energy
                );
                history.push_str(&msg);
                on_delta(&msg);
                own_team[*own_active].entry_leave_immune = false;
            }
        }
    }
}

fn find_switch_target(team: &[BattlePet], active: usize, target_name: &str) -> Option<usize> {
    team.iter()
        .enumerate()
        .find(|(i, p)| *i != active && p.cur_hp > 0 && p.name == target_name)
        .map(|(i, _)| i)
}

async fn llm_choose_action(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    side: &str,
    include_system_prompt: bool,
    own: &BattlePet,
    opp: &BattlePet,
    own_team: &[BattlePet],
    active_index: usize,
    weather: Option<&WeatherState>,
    own_negative_imprint: Option<NegativeImprint>,
    own_positive_imprint: Option<PositiveImprint>,
    enemy_negative_imprint: Option<NegativeImprint>,
    enemy_positive_imprint: Option<PositiveImprint>,
    history: &str,
    round: i32,
) -> Result<Decision, String> {
    throttle_llm_request().await;
    let endpoint = format!("{}/openai/v1/chat/completions", base_url.trim_end_matches('/'));
    let all_skills = own
        .skills
        .iter()
        .map(|s| {
            let actual_cost = calc_skill_cost(s, weather);
            (
                s.name.clone(),
                actual_cost,
                format!(
                    "- {}|属性:{}|分类:{}|能耗:{}|威力:{}|{}",
                    s.name, s.element, s.category, actual_cost, s.power, s.effect
                ),
            )
        })
        .collect::<Vec<_>>()
        ;
    let available_lines = all_skills
        .iter()
        .filter(|(_, cost, _)| own.energy >= *cost)
        .map(|(_, _, line)| line.clone())
        .collect::<Vec<_>>();
    let blocked_names = all_skills
        .iter()
        .filter(|(_, cost, _)| own.energy < *cost)
        .map(|(name, _, _)| name.clone())
        .collect::<Vec<_>>();
    let skills_text = if available_lines.is_empty() {
        "无（当前能量不足，建议换宠）".to_string()
    } else {
        available_lines.join("\n")
    };
    let blocked_text = if blocked_names.is_empty() {
        "无".to_string()
    } else {
        blocked_names.join("，")
    };
    let switchable = own_team
        .iter()
        .enumerate()
        .filter(|(i, p)| *i != active_index && p.cur_hp > 0)
        .map(|(_, p)| p.name.clone())
        .collect::<Vec<_>>();
    let switch_text = if switchable.is_empty() {
        "无".to_string()
    } else {
        switchable.join(", ")
    };
    let legal_actions = all_skills
        .iter()
        .filter(|(_, cost, _)| own.energy >= *cost)
        .map(|(name, _, _)| format!("SKILL:{}", name))
        .chain(switchable.iter().map(|name| format!("SWITCH:{}", name)))
        .chain(std::iter::once("CHARGE".to_string()))
        .collect::<Vec<_>>();
    let legal_actions_text = if legal_actions.is_empty() {
        "无".to_string()
    } else {
        legal_actions.join(" | ")
    };
    let own_team_full_text = own_team
        .iter()
        .enumerate()
        .map(|(idx, p)| {
            let skills = p
                .skills
                .iter()
                .map(|s| {
                    format!(
                        "{}({}/{}/耗{}{})",
                        s.name,
                        s.power,
                        s.category,
                        calc_skill_cost(s, weather),
                        if s.effect.trim().is_empty() {
                            "".to_string()
                        } else {
                            format!("/{}", s.effect.trim())
                        }
                    )
                })
                .collect::<Vec<_>>()
                .join("，");
            format!(
                "- {}{}: {} [{}] HP {}/{} EN{} SPD{} 特性:{}({}) 技能:{}",
                if idx == active_index { "*" } else { "" },
                idx + 1,
                p.name,
                pet_element_summary(p),
                p.cur_hp,
                p.hp,
                p.energy,
                p.speed,
                if p.ability.trim().is_empty() { "无" } else { p.ability.trim() },
                if p.ability_desc.trim().is_empty() { "无" } else { p.ability_desc.trim() },
                skills
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let own_reserve_compact_text = own_team
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != active_index)
        .enumerate()
        .map(|(idx, (_, p))| {
            let ability = if p.ability.trim().is_empty() {
                String::new()
            } else {
                format!(" [特性:{}]", p.ability.trim())
            };
            format!(
                "{}. {}({}) {}/{} EN:{} SPD:{}{}",
                idx + 1,
                p.name,
                pet_element_summary(p),
                p.cur_hp,
                p.hp,
                p.energy,
                p.speed,
                ability
            )
        })
        .collect::<Vec<_>>();
    let own_reserve_compact_text = if own_reserve_compact_text.is_empty() {
        "无".to_string()
    } else {
        own_reserve_compact_text.join("\n")
    };
    let own_team_context = if round <= 1 {
        format!("### 我方全队完整情报(仅R1提供) ###\n{}", own_team_full_text)
    } else {
        format!("### 后备选择 ###\n{}", own_reserve_compact_text)
    };

    let enemy_hp_pct = if opp.hp > 0 {
        ((opp.cur_hp as f32 / opp.hp as f32) * 100.0).round() as i32
    } else {
        0
    };
    let enemy_intel = observed_enemy_intel(side, history);
    let recent_dyn = compact_recent_dynamics(history, 5, 8);
    let user = [
        format!("你是{}方决策AI。每回合仅输出一行合法动作：SKILL:技能名 / SWITCH:精灵名 / CHARGE。", side),
        "### 状态快照 ###".to_string(),
        format!(
            "R{} | 天气:{}\n我方: {}({}) {}/{}({}%) EN:{} SPD:{} [特性:{}]\n敌方: {}({}) HP:{}% EN:{}",
            round,
            weather_summary(weather),
            own.name,
            pet_element_summary(own),
            own.cur_hp,
            own.hp,
            hp_percent(own),
            own.energy,
            own.speed,
            if own.ability.trim().is_empty() { "无" } else { own.ability.trim() },
            opp.name,
            pet_element_summary(opp),
            enemy_hp_pct,
            opp.energy
        ),
        "### 敌方情报摘要 ###".to_string(),
        format!(
            "- 我方印记: 负面={} 正面={}\n- 敌方印记(可见): 负面={} 正面={}",
            negative_imprint_summary(own_negative_imprint),
            positive_imprint_summary(own_positive_imprint),
            negative_imprint_summary(enemy_negative_imprint),
            positive_imprint_summary(enemy_positive_imprint)
        ),
        own_team_context,
        enemy_intel,
        "### 可用技能 ###".to_string(),
        skills_text,
        "### 不可用技能 ###".to_string(),
        blocked_text,
        "### 可切换精灵 ###".to_string(),
        switch_text,
        "### 最近动态 ###".to_string(),
        recent_dyn,
        "### 合法动作 ###".to_string(),
        legal_actions_text,
    ]
    .join("\n");
    let system_prompt = battle_rule_prompt();
    if include_system_prompt {
        println!(
            "\n===== LLM PROMPT [{}方 R{}] SYSTEM =====\n{}\n===== LLM PROMPT [{}方 R{}] USER =====\n{}\n===== END PROMPT =====\n",
            side, round, system_prompt, side, round, user
        );
    } else {
        println!(
            "\n===== LLM PROMPT [{}方 R{}] USER-ONLY =====\n{}\n===== END PROMPT =====\n",
            side, round, user
        );
    }
    let payload = if include_system_prompt {
        json!({
            "model": model,
            "messages": [
                {"role":"system","content": system_prompt},
                {"role":"user","content": user}
            ]
        })
    } else {
        json!({
            "model": model,
            "messages": [
                {"role":"user","content": user}
            ]
        })
    };
    let resp = client
        .post(&endpoint)
        .header(AUTHORIZATION, format!("Bearer {}", api_key))
        .header(CONTENT_TYPE, "application/json")
        .json(&payload)
        .send()
        .await
        .map_err(|e| format_reqwest_error("决策请求失败", &endpoint, &e))?;
    let status = resp.status();
    let body = resp.text().await.map_err(|e| format!("读取决策响应失败: {}", e))?;
    if !status.is_success() {
        return Err(format!("决策 HTTP {}: {}", status, body.chars().take(150).collect::<String>()));
    }
    let parsed: ChatCompletionsResponse =
        serde_json::from_str(&body).map_err(|e| format!("解析决策响应失败: {}", e))?;
    Ok(parse_decision(
        &parsed
            .choices
            .first()
            .and_then(|c| c.message.content.clone())
            .unwrap_or_default(),
        own,
    ))
}

async fn throttle_llm_request() {
    static LAST_LLM_CALL_MS: OnceLock<Mutex<u128>> = OnceLock::new();
    let lock = LAST_LLM_CALL_MS.get_or_init(|| Mutex::new(0));
    let now = current_millis();
    let mut last = if let Ok(guard) = lock.lock() { guard } else { return };
    let elapsed = now.saturating_sub(*last);
    if elapsed < LLM_MIN_INTERVAL_MS {
        let wait_ms = (LLM_MIN_INTERVAL_MS - elapsed) as u64;
        drop(last);
        tokio::time::sleep(Duration::from_millis(wait_ms)).await;
        if let Ok(mut guard) = lock.lock() {
            *guard = current_millis();
        }
    } else {
        *last = now;
    }
}

fn current_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn battle_rule_prompt() -> &'static str {
    "你是洛克王国PVP对战决策器。你必须严格遵守以下规则并只做一行动作决策：\n\
1) 只允许输出一行：SKILL:技能名 或 SWITCH:精灵名 或 CHARGE，禁止解释。\n\
2) 回合制；每回合只能行动一次；换宠也算行动。\n\
3) 先手规则：先比较先手+X，再比较速度；非防御类技能在应对成功时可抢先出手；防御类技能即使应对成功也不因此抢先，仍按先手+X与速度排序。\n\
4) 能量规则：技能需要足够能量才能释放；恢复类效果可回能；每只精灵默认满能量(10)；换宠不重置能量；CHARGE为每回合可用独立回能指令。\n\
5) 属性克制：单克制2.0，单抵抗0.5；双属性双克制3.0，双属性双抵抗0.25。\n\
6) 伤害核心：攻击/防御×0.9×威力×克制×本系加成，并受天气/词条影响。\n\
7) 状态与词条按战报与规则结算（中毒/灼烧/冻结/寄生/萌化/印记/天气等）。\n\
   - 换下场后会清除：中毒、灼烧、寄生。\n\
   - 换下场后不会清除：冻结、萌化。\n\
   - 场地印记默认不会因换宠自动清除；仅在技能效果明确写“清除印记/覆盖印记”时改变。\n\
8) 信息可见性限制：你不能直接知道敌方隐藏信息（性格、努力值、未登场精灵等），只能基于当前可见状态和历史观测推断。\n\
9) 若某技能当前能量不足则视为非法选择，不得输出该技能。\n\
10) 应对触发规则：\n\
   - 你的技能效果含“应对攻击”时，对手本回合若使用分类为“物攻”或“魔攻”的技能，则应对成功。\n\
   - 你的技能效果含“应对防御”时，对手本回合若使用分类为“防御”的技能，则应对成功。\n\
   - 你的技能效果含“应对状态”时，对手本回合若使用分类为“状态”的技能，则应对成功。\n\
11) 属性克制表（攻击属性 -> strong(克制), resist(被抵抗)）：\n\
   普通 -> strong:[] resist:[地,幽,机械]\n\
   草 -> strong:[水,光,地] resist:[火,龙,毒,虫,翼,机械]\n\
   火 -> strong:[草,冰,虫,机械] resist:[水,地,龙]\n\
   水 -> strong:[火,地,机械] resist:[草,冰,龙]\n\
   光 -> strong:[幽,恶] resist:[草,冰]\n\
   地 -> strong:[火,冰,电,毒] resist:[草,武]\n\
   冰 -> strong:[草,地,龙,翼] resist:[火,冰,机械]\n\
   龙 -> strong:[龙] resist:[机械]\n\
   电 -> strong:[水,翼] resist:[草,地,龙,电]\n\
   毒 -> strong:[草,萌] resist:[地,毒,幽,机械]\n\
   虫 -> strong:[草,恶,幻] resist:[火,毒,武,翼,萌,幽,机械]\n\
   武 -> strong:[普通,地,冰,恶,机械] resist:[毒,虫,翼,萌,幽,幻]\n\
   翼 -> strong:[草,虫,武] resist:[地,龙,电,机械]\n\
   萌 -> strong:[龙,武,恶] resist:[火,毒,机械]\n\
   幽 -> strong:[光,幽,幻] resist:[普通,恶]\n\
   恶 -> strong:[毒,萌,幽] resist:[光,武,恶]\n\
   机械 -> strong:[地,冰,萌] resist:[火,水,电,机械]\n\
   幻 -> strong:[毒,武] resist:[光,机械,幻]\n\
   无 -> strong:[] resist:[]\n\
12) 目标是最大化本方胜率。"
}

async fn redecide_if_energy_insufficient(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    side: &str,
    include_system_prompt: bool,
    decision: Decision,
    own_team: &[BattlePet],
    own_active: usize,
    opp: &BattlePet,
    weather: Option<&WeatherState>,
    own_negative_imprint: Option<NegativeImprint>,
    own_positive_imprint: Option<PositiveImprint>,
    enemy_negative_imprint: Option<NegativeImprint>,
    enemy_positive_imprint: Option<PositiveImprint>,
    history: &mut String,
    round: i32,
) -> Decision {
    let own = &own_team[own_active];
    let illegal_reason = decision_invalid_reason(own_team, own_active, &decision, weather);
    let Some(reason) = illegal_reason else {
        return decision;
    };
    history.push_str(&format!("{}方 {} 的行动非法（{}），触发重新决策。\n", side, own.name, reason));
    let mut new_decision = llm_choose_action(
        client,
        base_url,
        api_key,
        model,
        side,
        include_system_prompt,
        own,
        opp,
        own_team,
        own_active,
        weather,
        own_negative_imprint,
        own_positive_imprint,
        enemy_negative_imprint,
        enemy_positive_imprint,
        history,
        round,
    )
    .await
    .unwrap_or_else(|_| decision.clone());
    enforce_charging_constraint(own, &mut new_decision);
    if decision_invalid_reason(own_team, own_active, &new_decision, weather).is_none() {
        return new_decision;
    }
    if let Some(fallback_switch) = first_valid_switch_decision(own_team, own_active) {
        return fallback_switch;
    }
    first_valid_skill_decision(own, weather)
}

fn decision_invalid_reason(
    own_team: &[BattlePet],
    own_active: usize,
    decision: &Decision,
    weather: Option<&WeatherState>,
) -> Option<String> {
    match decision {
        Decision::Charge => None,
        Decision::Switch(target_name) => {
            if find_switch_target(own_team, own_active, target_name).is_none() {
                Some(format!("切换目标 [{}] 不存在或已倒下", target_name))
            } else {
                None
            }
        }
        Decision::UseSkill(skill_name) => {
            let own = &own_team[own_active];
            let Some(skill) = own.skills.iter().find(|s| s.name == *skill_name) else {
                return Some(format!("技能 [{}] 不存在", skill_name));
            };
            let cost = calc_skill_cost(skill, weather);
            if own.energy < cost {
                Some(format!("技能 [{}] 能量不足({}<{})", skill.name, own.energy, cost))
            } else {
                None
            }
        }
    }
}

fn first_valid_switch_decision(team: &[BattlePet], active: usize) -> Option<Decision> {
    team.iter()
        .enumerate()
        .find(|(i, p)| *i != active && p.cur_hp > 0)
        .map(|(_, p)| Decision::Switch(p.name.clone()))
}

fn first_valid_skill_decision(own: &BattlePet, weather: Option<&WeatherState>) -> Decision {
    if let Some(skill) = own
        .skills
        .iter()
        .find(|s| own.energy >= calc_skill_cost(s, weather))
    {
        return Decision::UseSkill(skill.name.clone());
    }
    Decision::Charge
}

fn observed_enemy_intel(side: &str, history: &str) -> String {
    let enemy_side = if side == "A" { "B方" } else { "A方" };
    let mut seen_pets: Vec<String> = Vec::new();
    let mut shown_skills: Vec<String> = Vec::new();
    for line in history.lines() {
        if !line.starts_with(enemy_side) {
            continue;
        }
        if let Some(pos) = line.find(" 上场：") {
            let name = line[pos + " 上场：".len()..].trim_end_matches('。').trim();
            if !name.is_empty() && !seen_pets.iter().any(|x| x == name) {
                seen_pets.push(name.to_string());
            }
        }
        if let Some(pos) = line.find(" 使用 [") {
            let rest = &line[pos + " 使用 [".len()..];
            if let Some(end) = rest.find(']') {
                let skill = rest[..end].trim();
                if !skill.is_empty() && !shown_skills.iter().any(|x| x == skill) {
                    shown_skills.push(skill.to_string());
                }
            }
        }
    }
    let pets_text = if seen_pets.is_empty() {
        "无".to_string()
    } else {
        seen_pets.join("，")
    };
    let skills_text = if shown_skills.is_empty() {
        "无".to_string()
    } else {
        shown_skills.join("，")
    };
    format!("- 敌方已出战精灵: {}\n- 敌方已展示技能: {}", pets_text, skills_text)
}

fn type_multiplier(attack_type: &str, def1: &str, def2: Option<&str>) -> f32 {
    let chart = type_chart();
    let eff = if let Some(e) = chart.get(attack_type) { e } else { return 1.0 };
    let defender_attr_count = if def2.is_some() { 2 } else { 1 };
    let mut strong_count = 0;
    let mut resist_count = 0;
    for d in [Some(def1), def2].into_iter().flatten() {
        if eff.strong.iter().any(|x| x == d) {
            strong_count += 1;
        }
        if eff.resist.iter().any(|x| x == d) {
            resist_count += 1;
        }
    }
    // 300%/25% 只在双属性目标且双命中(克制/抵抗)时触发
    if defender_attr_count == 2 && strong_count == 2 {
        3.0
    } else if defender_attr_count == 2 && resist_count == 2 {
        0.25
    } else if strong_count == 1 {
        2.0
    } else if resist_count == 1 {
        0.5
    } else {
        1.0
    }
}

fn skill_priority(pet: &BattlePet, skill_name: &str) -> i32 {
    let text = pet
        .skills
        .iter()
        .find(|s| s.name == skill_name)
        .map(|s| s.effect.as_str())
        .unwrap_or("");
    if let Some(pos) = text.find("先手+") {
        let num = text[pos + 7..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>();
        return num.parse::<i32>().unwrap_or(0);
    }
    0
}

fn normalize_skill_name(raw: &str) -> String {
    raw.lines().next().unwrap_or("").trim().trim_matches('"').trim_matches('`').to_string()
}

fn parse_decision(raw: &str, own: &BattlePet) -> Decision {
    let text = normalize_skill_name(raw);
    if text.trim() == "CHARGE" {
        return Decision::Charge;
    }
    if let Some(rest) = text.strip_prefix("SWITCH:") {
        return Decision::Switch(rest.trim().to_string());
    }
    if let Some(rest) = text.strip_prefix("SKILL:") {
        return Decision::UseSkill(rest.trim().to_string());
    }
    if own.skills.iter().any(|s| s.name == text) {
        return Decision::UseSkill(text);
    }
    Decision::UseSkill(own.skills[0].name.clone())
}

fn enforce_charging_constraint(pet: &BattlePet, decision: &mut Decision) {
    if let Some(skill) = &pet.charging_skill {
        match decision {
            Decision::Switch(_) => {}
            _ => {
                *decision = Decision::UseSkill(skill.clone());
            }
        }
    }
}

fn counter_triggered(self_pet: &BattlePet, self_decision: &Decision, opp_decision: &Decision, opp_pet: &BattlePet) -> bool {
    let skill_name = match self_decision {
        Decision::UseSkill(s) => s,
        Decision::Switch(_) => return false,
        Decision::Charge => return false,
    };
    let self_skill = self_pet.skills.iter().find(|x| x.name == *skill_name);
    let self_effect = if let Some(s) = self_skill { &s.effect } else { return false };
    if !self_effect.contains("应对") {
        return false;
    }
    match opp_decision {
        Decision::Switch(_) | Decision::Charge => false,
        Decision::UseSkill(opp_skill_name) => {
            let opp_cat = opp_pet
                .skills
                .iter()
                .find(|s| s.name == *opp_skill_name)
                .map(|s| s.category.as_str())
                .unwrap_or("物攻");
            if self_effect.contains("应对攻击") {
                opp_cat == "物攻" || opp_cat == "魔攻"
            } else if self_effect.contains("应对防御") {
                opp_cat == "防御"
            } else if self_effect.contains("应对状态") {
                opp_cat == "状态"
            } else {
                false
            }
        }
    }
}

/// 应对成功时是否因此获得当回合「抢先出手」；防御类技能不参与此项（仍按先手+X 与速度排序）。
fn counter_grants_act_priority(self_pet: &BattlePet, self_decision: &Decision, opp_decision: &Decision, opp_pet: &BattlePet) -> bool {
    if !counter_triggered(self_pet, self_decision, opp_decision, opp_pet) {
        return false;
    }
    let Decision::UseSkill(skill_name) = self_decision else {
        return false;
    };
    let Some(sk) = self_pet.skills.iter().find(|x| x.name == *skill_name) else {
        return false;
    };
    sk.category != "防御"
}

fn build_pet_index(pets: &[PetJsonLite]) -> HashMap<String, PetJsonLite> {
    let mut map = HashMap::new();
    for p in pets {
        map.insert(p.name.clone(), p.clone());
    }
    map
}

fn apply_pet_battle_patch(pet: &mut BattlePet, patch: Option<&PetBattlePatchEntry>) {
    let Some(p) = patch else {
        return;
    };
    if let Some(ie) = p.initial_energy {
        pet.energy = ie;
    }
    if p.allow_over_max_energy {
        pet.energy_cap = p.energy_cap.unwrap_or(99).max(1);
    } else if let Some(cap) = p.energy_cap {
        pet.energy_cap = cap.max(1);
    }
    pet.energy = pet.energy.clamp(0, pet.energy_cap);
}

fn build_battle_team(
    raw_team: &[(String, String, Vec<String>)],
    pet_index: &HashMap<String, PetJsonLite>,
    side: &str,
    pet_patches: &PetBattlePatchTable,
) -> Result<Vec<BattlePet>, String> {
    let mut out = Vec::new();
    let mut seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(42)
        ^ side.bytes().map(|b| b as u64).sum::<u64>();
    for (pet_name, nature_name, skill_names) in raw_team {
        let pet = pet_index
            .get(pet_name)
            .ok_or_else(|| format!("{} 方精灵不存在: {}", side, pet_name))?;
        let mut skills = Vec::new();
        for name in skill_names {
            let d = pet.skill_details.iter().find(|x| x.name == *name).cloned();
            let (element, category, cost, power, effect) = if let Some(v) = d.clone() {
                (v.element, v.category, parse_cost(&v.cost), parse_power(&v.power), v.effect)
            } else {
                ("普通".to_string(), "物攻".to_string(), 2, 60, String::new())
            };
            skills.push(BattleSkill {
                name: name.clone(),
                element,
                category,
                cost,
                power,
                effect,
            });
        }
        let effort = random_effort_values(&mut seed);
        let hp = calculate_hp(pet.hp.max(1), PVP_DEFAULT_INDIVIDUAL_VALUE, PVP_DEFAULT_MAGNIFICATION) + effort.hp;
        let patk = calculate_stat(
            pet.physical_attack.max(1),
            PVP_DEFAULT_INDIVIDUAL_VALUE,
            PVP_DEFAULT_MAGNIFICATION,
        ) + effort.patk;
        let pdef = calculate_stat(
            pet.physical_defense.max(1),
            PVP_DEFAULT_INDIVIDUAL_VALUE,
            PVP_DEFAULT_MAGNIFICATION,
        ) + effort.pdef;
        let matk = calculate_stat(
            pet.magic_attack.max(1),
            PVP_DEFAULT_INDIVIDUAL_VALUE,
            PVP_DEFAULT_MAGNIFICATION,
        ) + effort.matk;
        let mdef = calculate_stat(
            pet.magic_defense.max(1),
            PVP_DEFAULT_INDIVIDUAL_VALUE,
            PVP_DEFAULT_MAGNIFICATION,
        ) + effort.mdef;
        let speed = calculate_stat(
            pet.speed.max(1),
            PVP_DEFAULT_INDIVIDUAL_VALUE,
            PVP_DEFAULT_MAGNIFICATION,
        ) + effort.speed;
        let mut battle_pet = BattlePet {
            name: pet.name.clone(),
            level: PVP_DEFAULT_LEVEL,
            nature: normalize_nature_name(nature_name),
            ability: pet.ability.clone(),
            ability_desc: pet.ability_desc.clone(),
            element1: pet.element.clone(),
            element2: pet
                .element2
                .as_ref()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            hp,
            cur_hp: hp,
            speed,
            patk,
            pdef,
            matk,
            mdef,
            energy: MAX_ENERGY,
            energy_cap: MAX_ENERGY,
            poison_layers: 0,
            burn_layers: 0,
            freeze_layers: 0,
            parasitic_layers: 0,
            frozen_hp_locked: 0,
            charging_skill: None,
            entry_bonus_power: 0,
            entry_leave_immune: false,
            morph_stage: 0,
            effort,
            skills,
        };
        let patch_key = battle_pet.name.clone();
        apply_pet_battle_patch(&mut battle_pet, pet_patches.get(&patch_key));
        out.push(battle_pet);
    }
    Ok(out)
}

fn parse_fixed_team(raw: &str, side_name: &str) -> Result<Vec<(String, String, Vec<String>)>, String> {
    let lines = raw.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>();
    if lines.len() != 6 {
        return Err(format!("{} 阵容需要 6 行，当前 {}", side_name, lines.len()));
    }
    let mut out = Vec::new();
    for line in lines {
        let (pet, skill_raw) = line.split_once(':').ok_or_else(|| format!("{} 行格式错误：{}", side_name, line))?;
        let pet_token = pet.trim();
        let (pet_name, nature_name) = if let Some((name, nature)) = pet_token.split_once('@') {
            (name.trim().to_string(), normalize_nature_name(nature.trim()))
        } else {
            (pet_token.to_string(), "认真".to_string())
        };
        let skills = skill_raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        if skills.len() != 4 {
            return Err(format!("{} 的精灵 {} 需要 4 个技能", side_name, pet.trim()));
        }
        out.push((pet_name, nature_name, skills));
    }
    Ok(out)
}

fn generate_random_teams(
    pet_pool: &[PetJsonLite],
) -> Result<(Vec<(String, String, Vec<String>)>, Vec<(String, String, Vec<String>)>), String> {
    let mut seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(42);
    let mut mk = || {
        let mut t = Vec::new();
        for _ in 0..6 {
            let mut picked = None;
            for _ in 0..64 {
                let pet = &pet_pool[rand_index(&mut seed, pet_pool.len())];
                if pet.skills.len() < 4 {
                    continue;
                }
                let mut idxs = (0..pet.skills.len()).collect::<Vec<_>>();
                shuffle_indices(&mut idxs, &mut seed);
                let skills = idxs.into_iter().take(4).map(|i| pet.skills[i].clone()).collect::<Vec<_>>();
                picked = Some((pet.name.clone(), "认真".to_string(), skills));
                break;
            }
            if let Some(v) = picked {
                t.push(v);
            }
        }
        t
    };
    let a = mk();
    let b = mk();
    if a.len() != 6 || b.len() != 6 {
        return Err("随机组队失败，请检查 data/pets 是否存在足够数据".to_string());
    }
    Ok((a, b))
}

fn load_pet_pool(dir: &str) -> Result<Vec<PetJsonLite>, String> {
    let entries = fs::read_dir(dir).map_err(|e| format!("读取精灵目录失败 {}: {}", dir, e))?;
    let mut pool = Vec::new();
    for entry in entries {
        let path = entry.map_err(|e| format!("读取目录项失败: {}", e))?.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let content = fs::read_to_string(&path).map_err(|e| format!("读取精灵文件失败 {}: {}", path.display(), e))?;
        let pet: PetJsonLite =
            serde_json::from_str(&content).map_err(|e| format!("解析精灵文件失败 {}: {}", path.display(), e))?;
        if !pet.skills.is_empty() {
            pool.push(pet);
        }
    }
    if pool.is_empty() {
        return Err("data/pets 中没有可用精灵数据".to_string());
    }
    Ok(pool)
}

fn parse_power(s: &str) -> i32 {
    s.parse::<i32>().unwrap_or(60).max(0)
}

fn calculate_hp(base_hp: i32, individual_value: i32, magnification: f32) -> i32 {
    let mut hp = ((base_hp + individual_value * 3) as f32 * 1.7 + 70.5).floor() as i32;
    hp = (hp as f32 * magnification).floor() as i32 + 100;
    hp.max(1)
}

fn calculate_stat(base_stat: i32, individual_value: i32, magnification: f32) -> i32 {
    let mut stat = ((base_stat + individual_value * 3) as f32 * 1.1 + 10.5).floor() as i32;
    stat = (stat as f32 * magnification).floor() as i32 + 50;
    stat.max(1)
}

fn parse_cost(s: &str) -> i32 {
    s.parse::<i32>().unwrap_or(2).max(0)
}

fn rand_index(seed: &mut u64, len: usize) -> usize {
    *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
    ((*seed >> 32) as usize) % len
}

fn shuffle_indices(arr: &mut [usize], seed: &mut u64) {
    if arr.len() < 2 {
        return;
    }
    for i in (1..arr.len()).rev() {
        let j = rand_index(seed, i + 1);
        arr.swap(i, j);
    }
}

fn random_effort_values(seed: &mut u64) -> EffortValues {
    let stat_count = rand_index(seed, 3) + 1; // 1..=3
    let mut idxs = [0usize, 1, 2, 3, 4, 5];
    for i in (1..idxs.len()).rev() {
        let j = rand_index(seed, i + 1);
        idxs.swap(i, j);
    }
    let mut ev = EffortValues::default();
    for idx in idxs.iter().take(stat_count) {
        let base = PVP_EFFORT_BASE_MIN + rand_index(seed, (PVP_EFFORT_BASE_MAX - PVP_EFFORT_BASE_MIN + 1) as usize) as i32;
        let value = base * PVP_EFFORT_MULTIPLIER;
        match idx {
            0 => ev.hp = value,
            1 => ev.patk = value,
            2 => ev.matk = value,
            3 => ev.pdef = value,
            4 => ev.mdef = value,
            _ => ev.speed = value,
        }
    }
    ev
}

fn effort_summary(e: &EffortValues) -> String {
    let mut parts = Vec::new();
    if e.hp > 0 {
        parts.push(format!("生命+{}", e.hp));
    }
    if e.patk > 0 {
        parts.push(format!("物攻+{}", e.patk));
    }
    if e.matk > 0 {
        parts.push(format!("魔攻+{}", e.matk));
    }
    if e.pdef > 0 {
        parts.push(format!("物防+{}", e.pdef));
    }
    if e.mdef > 0 {
        parts.push(format!("魔防+{}", e.mdef));
    }
    if e.speed > 0 {
        parts.push(format!("速度+{}", e.speed));
    }
    if parts.is_empty() {
        "无".to_string()
    } else {
        parts.join("、")
    }
}

fn weather_summary(weather: Option<&WeatherState>) -> String {
    let Some(w) = weather else {
        return "无".to_string();
    };
    let kind = match w.kind {
        WeatherKind::Rain => "下雨",
        WeatherKind::Sandstorm => "沙暴",
        WeatherKind::Blizzard => "暴风雪",
    };
    format!("{}(剩余{}回合)", kind, w.remain_turns)
}

fn negative_imprint_summary(imprint: Option<NegativeImprint>) -> String {
    let Some(i) = imprint else {
        return "无".to_string();
    };
    let kind = match i.kind {
        NegativeImprintKind::Thorn => "棘刺",
        NegativeImprintKind::Descend => "降灵",
    };
    format!("{}x{}", kind, i.layers)
}

fn positive_imprint_summary(imprint: Option<PositiveImprint>) -> String {
    let Some(i) = imprint else {
        return "无".to_string();
    };
    let kind = match i.kind {
        PositiveImprintKind::Charge => "蓄电",
        PositiveImprintKind::Photosynthesis => "光合",
    };
    format!("{}x{}", kind, i.layers)
}

fn hp_percent(pet: &BattlePet) -> i32 {
    if pet.hp <= 0 {
        0
    } else {
        ((pet.cur_hp as f32 / pet.hp as f32) * 100.0).round() as i32
    }
}

fn pet_status_summary(pet: &BattlePet) -> String {
    let mut parts = Vec::new();
    if pet.poison_layers > 0 {
        parts.push(format!("中毒{}", pet.poison_layers));
    }
    if pet.burn_layers > 0 {
        parts.push(format!("灼烧{}", pet.burn_layers));
    }
    if pet.freeze_layers > 0 {
        parts.push(format!("冻结{}", pet.freeze_layers));
    }
    if pet.parasitic_layers > 0 {
        parts.push(format!("寄生{}", pet.parasitic_layers));
    }
    if pet.morph_stage > 0 {
        parts.push(format!("萌化阶段{}", pet.morph_stage));
    }
    if pet.frozen_hp_locked > 0 {
        parts.push(format!("冻结锁血{}", pet.frozen_hp_locked));
    }
    if parts.is_empty() {
        "无".to_string()
    } else {
        parts.join("、")
    }
}

fn pet_element_summary(pet: &BattlePet) -> String {
    if let Some(e2) = &pet.element2 {
        if e2.trim().is_empty() {
            pet.element1.clone()
        } else {
            format!("{}/{}", pet.element1, e2)
        }
    } else {
        pet.element1.clone()
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

fn format_reqwest_error(prefix: &str, endpoint: &str, err: &reqwest::Error) -> String {
    let kind = if err.is_timeout() {
        "请求超时"
    } else if err.is_connect() {
        "连接失败（DNS/TLS/网络）"
    } else {
        "请求异常"
    };
    let mut chain = Vec::new();
    let mut source = err.source();
    while let Some(s) = source {
        chain.push(s.to_string());
        source = s.source();
    }
    let source_text = if chain.is_empty() { String::new() } else { format!(" | 详细原因: {}", chain.join(" -> ")) };
    format!("{}: {} | url: {} | {}{}", prefix, err, endpoint, kind, source_text)
}

fn type_chart() -> HashMap<String, TypeEffect> {
    let mut m = HashMap::new();
    m.insert("普通".into(), TypeEffect::new(vec![], vec!["地", "幽", "机械"]));
    m.insert("草".into(), TypeEffect::new(vec!["水", "光", "地"], vec!["火", "龙", "毒", "虫", "翼", "机械"]));
    m.insert("火".into(), TypeEffect::new(vec!["草", "冰", "虫", "机械"], vec!["水", "地", "龙"]));
    m.insert("水".into(), TypeEffect::new(vec!["火", "地", "机械"], vec!["草", "冰", "龙"]));
    m.insert("光".into(), TypeEffect::new(vec!["幽", "恶"], vec!["草", "冰"]));
    m.insert("地".into(), TypeEffect::new(vec!["火", "冰", "电", "毒"], vec!["草", "武"]));
    m.insert("冰".into(), TypeEffect::new(vec!["草", "地", "龙", "翼"], vec!["火", "冰", "机械"]));
    m.insert("龙".into(), TypeEffect::new(vec!["龙"], vec!["机械"]));
    m.insert("电".into(), TypeEffect::new(vec!["水", "翼"], vec!["草", "地", "龙", "电"]));
    m.insert("毒".into(), TypeEffect::new(vec!["草", "萌"], vec!["地", "毒", "幽", "机械"]));
    m.insert("虫".into(), TypeEffect::new(vec!["草", "恶", "幻"], vec!["火", "毒", "武", "翼", "萌", "幽", "机械"]));
    m.insert("武".into(), TypeEffect::new(vec!["普通", "地", "冰", "恶", "机械"], vec!["毒", "虫", "翼", "萌", "幽", "幻"]));
    m.insert("翼".into(), TypeEffect::new(vec!["草", "虫", "武"], vec!["地", "龙", "电", "机械"]));
    m.insert("萌".into(), TypeEffect::new(vec!["龙", "武", "恶"], vec!["火", "毒", "机械"]));
    m.insert("幽".into(), TypeEffect::new(vec!["光", "幽", "幻"], vec!["普通", "恶"]));
    m.insert("恶".into(), TypeEffect::new(vec!["毒", "萌", "幽"], vec!["光", "武", "恶"]));
    m.insert("机械".into(), TypeEffect::new(vec!["地", "冰", "萌"], vec!["火", "水", "电", "机械"]));
    m.insert("幻".into(), TypeEffect::new(vec!["毒", "武"], vec!["光", "机械", "幻"]));
    m.insert("无".into(), TypeEffect::new(vec![], vec![]));
    m
}

struct TypeEffect {
    strong: Vec<String>,
    resist: Vec<String>,
}

impl TypeEffect {
    fn new(strong: Vec<&str>, resist: Vec<&str>) -> Self {
        Self {
            strong: strong.into_iter().map(|s| s.to_string()).collect(),
            resist: resist.into_iter().map(|s| s.to_string()).collect(),
        }
    }
}

fn apply_field_imprint_from_skill<F: FnMut(&str)>(
    attacker: &BattlePet,
    used_skill_name: &str,
    own_negative_imprint: &mut Option<NegativeImprint>,
    opp_negative_imprint: &mut Option<NegativeImprint>,
    own_positive_imprint: &mut Option<PositiveImprint>,
    history: &mut String,
    on_delta: &mut F,
) {
    let skill = attacker.skills.iter().find(|s| s.name == used_skill_name);
    let effect = if let Some(s) = skill { &s.effect } else { return };
    if effect.contains("棘刺") {
        let n = extract_layers(effect, "层棘刺").unwrap_or(1);
        set_negative_imprint(opp_negative_imprint, NegativeImprintKind::Thorn, n);
        let msg = format!("场地印记：对方获得 {} 层棘刺（入场受伤）。\n", n);
        history.push_str(&msg);
        on_delta(&msg);
    }
    if effect.contains("降灵") {
        let n = extract_layers(effect, "层降灵").unwrap_or(1);
        set_negative_imprint(opp_negative_imprint, NegativeImprintKind::Descend, n);
        let msg = format!("场地印记：对方获得 {} 层降灵（入场扣能）。\n", n);
        history.push_str(&msg);
        on_delta(&msg);
    }
    if effect.contains("蓄电") {
        let n = extract_layers(effect, "层蓄电").unwrap_or(1);
        set_positive_imprint(own_positive_imprint, PositiveImprintKind::Charge, n);
        let msg = format!("场地印记：本方获得 {} 层蓄电（入场首回合威力+10/层）。\n", n);
        history.push_str(&msg);
        on_delta(&msg);
    }
    if effect.contains("光合") {
        let n = extract_layers(effect, "层光合").unwrap_or(1);
        set_positive_imprint(own_positive_imprint, PositiveImprintKind::Photosynthesis, n);
        let msg = format!("场地印记：本方获得 {} 层光合（回合结束每层回复1点能量）。\n", n);
        history.push_str(&msg);
        on_delta(&msg);
    }
    let _ = own_negative_imprint;
}

fn apply_element_skill_bank_on_entry<F: FnMut(&str)>(
    side: &str,
    pet: &mut BattlePet,
    entry: &PetBattlePatchEntry,
    elem_banks: &mut ElementSkillBanks,
    history: &mut String,
    on_delta: &mut F,
) {
    let Some(rule) = &entry.on_entry_per_element_skill_bank else {
        return;
    };
    let count = match rule.element.as_str() {
        "冰" => elem_banks.ice,
        "火" => elem_banks.fire,
        "地" => elem_banks.earth,
        _ => 0,
    };
    if count <= 0 || rule.energy_per_use == 0 {
        return;
    }
    let gain = rule.energy_per_use.saturating_mul(count);
    pet.energy = (pet.energy + gain).clamp(0, pet.energy_cap);
    let msg = format!(
        "{}方 {} 入场：己方{}系技能累计 {} 次，按特性回复 {} 能量（当前{}）。\n",
        side, pet.name, rule.element, count, gain, pet.energy
    );
    history.push_str(&msg);
    on_delta(&msg);
    match rule.element.as_str() {
        "冰" => elem_banks.ice = 0,
        "火" => elem_banks.fire = 0,
        "地" => elem_banks.earth = 0,
        _ => {}
    }
}

fn apply_entry_effects<F: FnMut(&str)>(
    side: &str,
    pet: &mut BattlePet,
    negative_imprint: Option<NegativeImprint>,
    positive_imprint: Option<PositiveImprint>,
    pet_patches: &PetBattlePatchTable,
    ally_counter_bank: &mut i32,
    element_banks: &mut ElementSkillBanks,
    history: &mut String,
    on_delta: &mut F,
) {
    // 冻结会跨换场保留，入场时先校准可用生命值上限
    recompute_frozen_hp_lock(pet);
    if let Some(entry) = pet_patches.get(&pet.name) {
        if let Some(per) = entry.on_entry_energy_per_prior_ally_counter {
            if *ally_counter_bank > 0 && per != 0 {
                let gain = per.saturating_mul(*ally_counter_bank);
                pet.energy = (pet.energy + gain).clamp(0, pet.energy_cap);
                let msg = format!(
                    "{}方 {} 入场：己方应对成功累计 {} 次，按特性回复 {} 能量（当前{}）。\n",
                    side, pet.name, ally_counter_bank, gain, pet.energy
                );
                history.push_str(&msg);
                on_delta(&msg);
                *ally_counter_bank = 0;
            }
        }
        apply_element_skill_bank_on_entry(side, pet, entry, element_banks, history, on_delta);
    }
    pet.entry_leave_immune = true;
    if let Some(neg) = negative_imprint {
        match neg.kind {
            NegativeImprintKind::Thorn if neg.layers > 0 => {
                let dmg = ((pet.hp as f32) * 0.06).floor() as i32 * neg.layers;
                pet.cur_hp = (pet.cur_hp - dmg.max(1)).max(0);
                let msg = format!(
                    "{}方 {} 入场触发棘刺({}层)，受伤 {}，HP {}/{}\n",
                    side,
                    pet.name,
                    neg.layers,
                    dmg.max(1),
                    pet.cur_hp,
                    pet.hp
                );
                history.push_str(&msg);
                on_delta(&msg);
            }
            NegativeImprintKind::Descend if neg.layers > 0 => {
                pet.energy = (pet.energy - neg.layers).max(0);
                let msg = format!("{}方 {} 入场触发降灵({}层)，能量降至 {}\n", side, pet.name, neg.layers, pet.energy);
                history.push_str(&msg);
                on_delta(&msg);
            }
            _ => {}
        }
    }
    if let Some(pos) = positive_imprint {
        if pos.kind == PositiveImprintKind::Charge && pos.layers > 0 {
            pet.entry_bonus_power = 10 * pos.layers;
            let msg = format!(
                "{}方 {} 入场触发蓄电({}层)，本回合技能威力+{}\n",
                side, pet.name, pos.layers, pet.entry_bonus_power
            );
            history.push_str(&msg);
            on_delta(&msg);
        }
    }
}

/// 若触发迅捷并成功出手，返回与 `resolve_one_attack` 相同的计数信息。
fn trigger_swift_on_entry<F: FnMut(&str)>(
    side: &str,
    pet: &mut BattlePet,
    opp: &mut BattlePet,
    dedication: &DedicationBuff,
    weather: &mut Option<WeatherState>,
    history: &mut String,
    on_delta: &mut F,
) -> Option<(String, String)> {
    if opp.cur_hp <= 0 || pet.cur_hp <= 0 {
        return None;
    }
    let first_swift = pet
        .skills
        .iter()
        .find(|s| s.effect.contains("迅捷") && pet.energy >= calc_skill_cost(s, weather.as_ref()))
        .map(|s| s.name.clone());
    if let Some(skill_name) = first_swift {
        let msg = format!("{}方 {} 触发迅捷，自动使用 [{}]。\n", side, pet.name, skill_name);
        history.push_str(&msg);
        on_delta(&msg);
        return resolve_one_attack(
            side,
            &skill_name,
            pet,
            opp,
            dedication,
            weather,
            history,
            on_delta,
        );
    }
    None
}

fn calc_skill_cost(skill: &BattleSkill, weather: Option<&WeatherState>) -> i32 {
    if let Some(w) = weather {
        if w.kind == WeatherKind::Sandstorm && skill.element == "地" {
            return (skill.cost + 1) / 2;
        }
    }
    skill.cost
}

fn weather_damage_multiplier(weather: Option<&WeatherState>, skill: &BattleSkill) -> f32 {
    if let Some(w) = weather {
        if w.kind == WeatherKind::Rain && skill.element == "水" {
            return 1.5;
        }
    }
    1.0
}

fn apply_weather_from_skill<F: FnMut(&str)>(
    weather: &mut Option<WeatherState>,
    skill: &BattleSkill,
    history: &mut String,
    on_delta: &mut F,
) {
    let new_weather = if skill.effect.contains("下雨") {
        Some(WeatherKind::Rain)
    } else if skill.effect.contains("沙暴") {
        Some(WeatherKind::Sandstorm)
    } else if skill.effect.contains("暴风雪") {
        Some(WeatherKind::Blizzard)
    } else {
        None
    };
    if let Some(kind) = new_weather {
        *weather = Some(WeatherState { kind, remain_turns: 8 });
        let name = match kind {
            WeatherKind::Rain => "下雨",
            WeatherKind::Sandstorm => "沙暴",
            WeatherKind::Blizzard => "暴风雪",
        };
        let msg = format!("天气变为 [{}]，持续 8 回合。\n", name);
        history.push_str(&msg);
        on_delta(&msg);
    }
}

fn apply_weather_end_turn<F: FnMut(&str)>(
    weather: &mut Option<WeatherState>,
    a: &mut BattlePet,
    b: &mut BattlePet,
    history: &mut String,
    on_delta: &mut F,
) {
    if let Some(w) = weather.as_mut() {
        if w.kind == WeatherKind::Blizzard {
            let mut a_lock_changed = false;
            let mut b_lock_changed = false;
            if !is_immune_to_freeze(a) {
                a.freeze_layers += 2;
                let before = a.frozen_hp_locked;
                recompute_frozen_hp_lock(a);
                a_lock_changed = a.frozen_hp_locked != before;
            }
            if !is_immune_to_freeze(b) {
                b.freeze_layers += 2;
                let before = b.frozen_hp_locked;
                recompute_frozen_hp_lock(b);
                b_lock_changed = b.frozen_hp_locked != before;
            }
            let msg = "暴风雪：双方各获得2层冻结（冰系免疫）。\n".to_string();
            history.push_str(&msg);
            on_delta(&msg);
            if a_lock_changed {
                let msg = format!(
                    "{} 的冻结生命值提升至 {}（当前可用上限 {}/{}）。\n",
                    a.name,
                    a.frozen_hp_locked,
                    (a.hp - a.frozen_hp_locked).max(1),
                    a.hp
                );
                history.push_str(&msg);
                on_delta(&msg);
            }
            if b_lock_changed {
                let msg = format!(
                    "{} 的冻结生命值提升至 {}（当前可用上限 {}/{}）。\n",
                    b.name,
                    b.frozen_hp_locked,
                    (b.hp - b.frozen_hp_locked).max(1),
                    b.hp
                );
                history.push_str(&msg);
                on_delta(&msg);
            }
        }
        w.remain_turns -= 1;
        if w.remain_turns <= 0 {
            *weather = None;
            let msg = "天气效果结束。\n".to_string();
            history.push_str(&msg);
            on_delta(&msg);
        }
    }
}

fn apply_status_from_skill<F: FnMut(&str)>(
    defender: &mut BattlePet,
    skill: &BattleSkill,
    history: &mut String,
    on_delta: &mut F,
) {
    let effect = &skill.effect;
    if effect.contains("中毒") && !is_immune_to_poison(defender) {
        let n = extract_layers(effect, "层中毒").unwrap_or(1);
        defender.poison_layers += n;
        let msg = format!("{} 获得 {} 层中毒。\n", defender.name, n);
        history.push_str(&msg);
        on_delta(&msg);
    }
    if effect.contains("灼烧") && !is_immune_to_burn(defender) {
        let n = extract_layers(effect, "层灼烧").unwrap_or(1);
        defender.burn_layers += n;
        let msg = format!("{} 获得 {} 层灼烧。\n", defender.name, n);
        history.push_str(&msg);
        on_delta(&msg);
    }
    if effect.contains("冻结") && !is_immune_to_freeze(defender) {
        let n = extract_layers(effect, "层冻结").unwrap_or(1);
        defender.freeze_layers += n;
        recompute_frozen_hp_lock(defender);
        let msg = format!(
            "{} 获得 {} 层冻结，冻结生命值 {}（当前可用上限 {}/{}）。\n",
            defender.name,
            n,
            defender.frozen_hp_locked,
            (defender.hp - defender.frozen_hp_locked).max(1),
            defender.hp
        );
        history.push_str(&msg);
        on_delta(&msg);
    }
    if effect.contains("寄生") && !is_immune_to_parasite(defender) {
        let n = extract_layers(effect, "层寄生").unwrap_or(1);
        defender.parasitic_layers += n;
        let msg = format!("{} 获得 {} 层寄生。\n", defender.name, n);
        history.push_str(&msg);
        on_delta(&msg);
    } else if effect.contains("寄生") {
        let msg = format!("{} 免疫寄生（草系免疫）。\n", defender.name);
        history.push_str(&msg);
        on_delta(&msg);
    }
    if effect.contains("萌化") {
        if defender.morph_stage < 2 {
            defender.morph_stage += 1;
            let mul = morph_stat_multiplier(defender);
            let msg = format!(
                "{} 受到萌化，当前形态阶段 {}，种族值系数 {:.1}。\n",
                defender.name, defender.morph_stage, mul
            );
            history.push_str(&msg);
            on_delta(&msg);
        } else {
            let msg = format!("{} 已处于最低形态，萌化不再继续生效。\n", defender.name);
            history.push_str(&msg);
            on_delta(&msg);
        }
    }
}

fn extract_multi_hit(effect: &str) -> i32 {
    if !effect.contains("连击") {
        return 1;
    }
    if let Some(n) = extract_layers(effect, "次攻击") {
        return n.max(1);
    }
    if let Some(n) = extract_layers(effect, "连击") {
        return n.max(1);
    }
    2
}

fn recompute_frozen_hp_lock(pet: &mut BattlePet) {
    let lock = ((pet.hp as f32) * 0.05).floor() as i32 * pet.freeze_layers.max(0);
    pet.frozen_hp_locked = lock.clamp(0, pet.hp.saturating_sub(1));
    let max_usable = (pet.hp - pet.frozen_hp_locked).max(1);
    if pet.cur_hp > max_usable {
        pet.cur_hp = max_usable;
    }
}

fn clear_switch_cleared_status(pet: &mut BattlePet) {
    pet.poison_layers = 0;
    pet.burn_layers = 0;
    pet.parasitic_layers = 0;
}

fn is_immune_to_parasite(p: &BattlePet) -> bool {
    p.element1 == "草" || p.element2.as_deref() == Some("草")
}

fn process_leave_keywords<F: FnMut(&str)>(
    side: &str,
    used_skill_name: &str,
    own_team: &mut [BattlePet],
    own_active: &mut usize,
    opp_active_pet: &mut BattlePet,
    weather: &mut Option<WeatherState>,
    own_negative_imprint: Option<NegativeImprint>,
    own_positive_imprint: Option<PositiveImprint>,
    own_dedication: &DedicationBuff,
    pet_patches: &PetBattlePatchTable,
    a_ally_counter_bank: &mut i32,
    b_ally_counter_bank: &mut i32,
    a_element_banks: &mut ElementSkillBanks,
    b_element_banks: &mut ElementSkillBanks,
    history: &mut String,
    on_delta: &mut F,
) {
    let active_skill = own_team[*own_active].skills.iter().find(|s| s.name == used_skill_name);
    let Some(skill) = active_skill else {
        return;
    };
    let skill_name = skill.name.clone();
    let skill_effect = skill.effect.clone();
    if !(skill_effect.contains("折返")
        || skill_effect.contains("脱离")
        || skill_effect.contains("返场")
        || skill_effect.contains("紧急脱离"))
    {
        return;
    }
    if own_team[*own_active].entry_leave_immune {
        let msg = format!("{}方 {} 处于入场首回合，离场词条未触发。\n", side, own_team[*own_active].name);
        history.push_str(&msg);
        on_delta(&msg);
        return;
    }
    let mut candidates = own_team
        .iter()
        .enumerate()
        .filter(|(i, p)| *i != *own_active && p.cur_hp > 0)
        .map(|(i, _)| i)
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return;
    }
    let next_idx = if skill_effect.contains("紧急脱离") {
        let mut seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(42);
        candidates[rand_index(&mut seed, candidates.len())]
    } else {
        candidates.remove(0)
    };
    clear_switch_cleared_status(&mut own_team[*own_active]);
    *own_active = next_idx;
    own_team[*own_active].charging_skill = None;
    let msg = format!("{}方因 [{}] 触发离场，上场：{}。\n", side, skill_name, own_team[*own_active].name);
    history.push_str(&msg);
    on_delta(&msg);
    let ally_bank = if side == "A" {
        a_ally_counter_bank
    } else {
        b_ally_counter_bank
    };
    apply_entry_effects(
        side,
        &mut own_team[*own_active],
        own_negative_imprint,
        own_positive_imprint,
        pet_patches,
        ally_bank,
        if side == "A" {
            a_element_banks
        } else {
            b_element_banks
        },
        history,
        on_delta,
    );
    let swift_tally = trigger_swift_on_entry(
        side,
        &mut own_team[*own_active],
        opp_active_pet,
        own_dedication,
        weather,
        history,
        on_delta,
    );
    if let Some((an, el)) = swift_tally {
        let banks = if side == "A" {
            a_element_banks
        } else {
            b_element_banks
        };
        tally_element_skill_use_for_side(&*own_team, &an, &el, banks, pet_patches);
    }
}

fn extract_layers(text: &str, suffix: &str) -> Option<i32> {
    let idx = text.find(suffix)?;
    let prefix = &text[..idx];
    let num_rev = prefix
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>();
    if num_rev.is_empty() {
        return None;
    }
    let num = num_rev.chars().rev().collect::<String>();
    num.parse::<i32>().ok()
}

fn dedication_applies_to_skill(skill_name: &str) -> bool {
    skill_name == "啃咬" || skill_name == "虫群"
}

fn apply_dedication_gain_from_effect<F: FnMut(&str)>(
    side: &str,
    pet: &BattlePet,
    used_skill_name: &str,
    dedication: &mut DedicationBuff,
    history: &mut String,
    on_delta: &mut F,
) {
    let Some(skill) = pet.skills.iter().find(|s| s.name == used_skill_name) else {
        return;
    };
    let effect = skill.effect.as_str();
    if !effect.contains("奉献") {
        return;
    }
    if effect.contains("随机奉献") {
        let mut seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(42);
        let idx = rand_index(&mut seed, 5);
        add_dedication_by_index(dedication, idx);
        let msg = format!(
            "{}方 {} 触发随机奉献，获得 {}。\n",
            side,
            pet.name,
            dedication_kind_name(idx)
        );
        history.push_str(&msg);
        on_delta(&msg);
    }
    let mut gains = Vec::new();
    if effect.contains("威力+20") {
        dedication.power_bonus_times += 1;
        gains.push("威力+20");
    }
    if effect.contains("附加 2 层中毒") || effect.contains("获得2层中毒") {
        dedication.poison_bonus_times += 1;
        gains.push("附加2层中毒");
    }
    if effect.contains("10%吸血") || effect.contains("20%吸血") {
        dedication.lifesteal_bonus_times += 1;
        gains.push("20%吸血");
    }
    if effect.contains("连击数+1") {
        dedication.combo_bonus_times += 1;
        gains.push("连击数+1");
    }
    if effect.contains("能耗-2") {
        dedication.cost_reduction_times += 1;
        gains.push("能耗-2");
    }
    if !gains.is_empty() {
        let msg = format!("{}方 {} 获得奉献强化：{}。\n", side, pet.name, gains.join("、"));
        history.push_str(&msg);
        on_delta(&msg);
    }
}

fn add_dedication_by_index(d: &mut DedicationBuff, idx: usize) {
    match idx {
        0 => d.power_bonus_times += 1,
        1 => d.poison_bonus_times += 1,
        2 => d.lifesteal_bonus_times += 1,
        3 => d.combo_bonus_times += 1,
        _ => d.cost_reduction_times += 1,
    }
}

fn dedication_kind_name(idx: usize) -> &'static str {
    match idx {
        0 => "威力+20",
        1 => "附加2层中毒",
        2 => "20%吸血",
        3 => "连击数+1",
        _ => "能耗-2",
    }
}

fn normalize_nature_name(raw: &str) -> String {
    let name = raw.trim();
    match name {
        "胆小" | "急躁" | "天真" | "开朗" | "固执" | "勇敢" | "调皮" | "孤独" | "保守" | "冷静" | "马虎"
        | "稳重" | "淘气" | "大胆" | "悠闲" | "沉着" | "慎重" | "温顺" | "狂妄" | "认真" | "实干"
        | "坦率" | "害羞" | "浮躁" => name.to_string(),
        _ => "认真".to_string(),
    }
}

fn nature_multiplier(nature: &str, stat: &str) -> f32 {
    let (up, down): (Option<&str>, Option<&str>) = match nature {
        "胆小" => (Some("速度"), Some("攻击")),
        "急躁" => (Some("速度"), Some("防御")),
        "天真" => (Some("速度"), Some("特防")),
        "开朗" => (Some("速度"), Some("特攻")),
        "固执" => (Some("攻击"), Some("特攻")),
        "勇敢" => (Some("攻击"), Some("速度")),
        "调皮" => (Some("攻击"), Some("特防")),
        "孤独" => (Some("攻击"), Some("防御")),
        "保守" => (Some("特攻"), Some("攻击")),
        "冷静" => (Some("特攻"), Some("速度")),
        "马虎" => (Some("特攻"), Some("特防")),
        "稳重" => (Some("特攻"), Some("防御")),
        "淘气" => (Some("防御"), Some("特攻")),
        "大胆" => (Some("防御"), Some("攻击")),
        "悠闲" => (Some("防御"), Some("速度")),
        "沉着" => (Some("特防"), Some("攻击")),
        "慎重" => (Some("特防"), Some("特攻")),
        "温顺" => (Some("特防"), Some("防御")),
        "狂妄" => (Some("特防"), Some("速度")),
        _ => (None, None),
    };
    if up == Some(stat) {
        1.1
    } else if down == Some(stat) {
        0.9
    } else {
        1.0
    }
}

fn recent_round_history(history: &str, keep_rounds: usize) -> String {
    if keep_rounds == 0 || history.trim().is_empty() {
        return "无".to_string();
    }
    let marker = "\n【回合 ";
    let mut starts = history.match_indices(marker).map(|(idx, _)| idx).collect::<Vec<_>>();
    if history.starts_with("【回合 ") {
        starts.insert(0, 0);
    }
    if starts.is_empty() {
        return history.to_string();
    }
    let start_idx = if starts.len() > keep_rounds {
        starts[starts.len() - keep_rounds]
    } else {
        starts[0]
    };
    history[start_idx..].trim().to_string()
}

fn compact_recent_dynamics(history: &str, keep_rounds: usize, max_lines: usize) -> String {
    let recent = recent_round_history(history, keep_rounds);
    if recent == "无" {
        return recent;
    }
    let mut out: Vec<String> = Vec::new();
    let mut current_round = String::new();
    for line in recent.lines() {
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        if text.starts_with("【回合 ") && text.ends_with("】") {
            current_round = text
                .trim_start_matches("【回合 ")
                .trim_end_matches("】")
                .to_string();
            continue;
        }
        if current_round.is_empty() {
            out.push(text.to_string());
        } else {
            out.push(format!("R{}: {}", current_round, text));
        }
        if out.len() >= max_lines {
            break;
        }
    }
    if out.is_empty() {
        "无".to_string()
    } else {
        out.join("\n")
    }
}

fn add_energy_clamped(current: i32, delta: i32, cap: i32) -> i32 {
    (current + delta).clamp(0, cap)
}

fn decision_to_text(decision: &Decision) -> String {
    match decision {
        Decision::UseSkill(s) => format!("SKILL:{}", s),
        Decision::Switch(s) => format!("SWITCH:{}", s),
        Decision::Charge => "CHARGE".to_string(),
    }
}

fn setup_mode_text(mode: AiBattleSetupMode) -> &'static str {
    match mode {
        AiBattleSetupMode::Random => "Random",
        AiBattleSetupMode::Fixed => "Fixed",
    }
}

fn build_initial_team_state(team: &[BattlePet]) -> Vec<PetInitialState> {
    team.iter()
        .map(|p| PetInitialState {
            name: p.name.clone(),
            level: p.level,
            element: p.element1.clone(),
            element2: p.element2.clone(),
            ability: p.ability.clone(),
            hp_max: p.hp,
            energy_max: p.energy_cap,
            speed: p.speed,
            patk: p.patk,
            pdef: p.pdef,
            matk: p.matk,
            mdef: p.mdef,
            skills: p
                .skills
                .iter()
                .map(|s| SkillView {
                    name: s.name.clone(),
                    element: s.element.clone(),
                    category: s.category.clone(),
                    cost: s.cost,
                    power: s.power,
                })
                .collect::<Vec<_>>(),
        })
        .collect::<Vec<_>>()
}

fn snapshot_status_effects(a: &BattlePet, b: &BattlePet) -> Vec<PetStatusSnapshot> {
    vec![
        PetStatusSnapshot {
            side: "A".to_string(),
            pet: a.name.clone(),
            effects: collect_effects(a),
        },
        PetStatusSnapshot {
            side: "B".to_string(),
            pet: b.name.clone(),
            effects: collect_effects(b),
        },
    ]
}

fn collect_effects(pet: &BattlePet) -> Vec<StatusEffectView> {
    let mut out = Vec::new();
    if pet.poison_layers > 0 {
        out.push(StatusEffectView {
            name: "中毒".to_string(),
            layers: pet.poison_layers,
        });
    }
    if pet.burn_layers > 0 {
        out.push(StatusEffectView {
            name: "灼烧".to_string(),
            layers: pet.burn_layers,
        });
    }
    if pet.freeze_layers > 0 {
        out.push(StatusEffectView {
            name: "冻结".to_string(),
            layers: pet.freeze_layers,
        });
    }
    if pet.parasitic_layers > 0 {
        out.push(StatusEffectView {
            name: "寄生".to_string(),
            layers: pet.parasitic_layers,
        });
    }
    out
}

fn alive_pet_names(team: &[BattlePet]) -> Vec<String> {
    team.iter()
        .filter(|p| p.cur_hp > 0)
        .map(|p| p.name.clone())
        .collect::<Vec<_>>()
}

fn resolve_winner_no_draw(a_life: i32, b_life: i32, team_a: &[BattlePet], team_b: &[BattlePet]) -> String {
    if a_life != b_life {
        return if a_life > b_life { "A".to_string() } else { "B".to_string() };
    }
    let a_hp = team_total_hp(team_a);
    let b_hp = team_total_hp(team_b);
    if a_hp != b_hp {
        return if a_hp > b_hp { "A".to_string() } else { "B".to_string() };
    }
    let a_energy = team_total_energy(team_a);
    let b_energy = team_total_energy(team_b);
    if a_energy != b_energy {
        return if a_energy > b_energy { "A".to_string() } else { "B".to_string() };
    }
    "A".to_string()
}

fn team_total_hp(team: &[BattlePet]) -> i32 {
    team.iter().map(|p| p.cur_hp.max(0)).sum()
}

fn team_total_energy(team: &[BattlePet]) -> i32 {
    team.iter().map(|p| p.energy.max(0)).sum()
}

fn first_alive_index(team: &[BattlePet]) -> Option<usize> {
    team.iter().position(|p| p.cur_hp > 0)
}

fn is_immune_to_poison(p: &BattlePet) -> bool {
    p.element1 == "毒" || p.element2.as_deref() == Some("毒")
}

fn is_immune_to_burn(p: &BattlePet) -> bool {
    p.element1 == "火" || p.element2.as_deref() == Some("火")
}

fn is_immune_to_freeze(p: &BattlePet) -> bool {
    p.element1 == "冰" || p.element2.as_deref() == Some("冰")
}

fn set_negative_imprint(slot: &mut Option<NegativeImprint>, kind: NegativeImprintKind, layers: i32) {
    if layers <= 0 {
        return;
    }
    match slot {
        Some(imprint) if imprint.kind == kind => imprint.layers += layers,
        _ => {
            *slot = Some(NegativeImprint { kind, layers });
        }
    }
}

fn set_positive_imprint(slot: &mut Option<PositiveImprint>, kind: PositiveImprintKind, layers: i32) {
    if layers <= 0 {
        return;
    }
    match slot {
        Some(imprint) if imprint.kind == kind => imprint.layers += layers,
        _ => {
            *slot = Some(PositiveImprint { kind, layers });
        }
    }
}

fn apply_positive_imprint_end_turn<F: FnMut(&str)>(
    side: &str,
    pet: &mut BattlePet,
    imprint: Option<PositiveImprint>,
    history: &mut String,
    on_delta: &mut F,
) {
    let Some(imprint) = imprint else {
        return;
    };
    if imprint.kind == PositiveImprintKind::Photosynthesis && imprint.layers > 0 && pet.cur_hp > 0 {
        let cap = pet.energy_cap;
        pet.energy = add_energy_clamped(pet.energy, imprint.layers, cap);
        let msg = format!(
            "{}方 {} 触发光合({}层)，回复 {} 点能量（当前{}）。\n",
            side, pet.name, imprint.layers, imprint.layers, pet.energy
        );
        history.push_str(&msg);
        on_delta(&msg);
    }
}

fn morph_stat_multiplier(pet: &BattlePet) -> f32 {
    match pet.morph_stage {
        1 => 0.8,
        2 => 0.6,
        _ => 1.0,
    }
}

fn effective_speed(pet: &BattlePet) -> i32 {
    ((pet.speed.max(1) as f32) * morph_stat_multiplier(pet) * nature_multiplier(&pet.nature, "速度")).round() as i32
}

