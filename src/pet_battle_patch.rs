use serde::Deserialize;
use std::collections::HashMap;
use std::fs;

const DEFAULT_PATCH_PATH: &str = "data/pet-battle-patch.json";

#[derive(Debug, Deserialize)]
struct PetBattlePatchFile {
    #[serde(default)]
    version: u32,
    entries: Vec<PetBattlePatchEntry>,
}

/// 己方「某属性技能使用次数」共享计数（按阵营分冰/火/地），入场带该规则的精灵结算后清零对应槽位。
#[derive(Debug, Default, Clone)]
pub struct ElementSkillBanks {
    pub ice: i32,
    pub fire: i32,
    pub earth: i32,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ElementSkillBankRule {
    pub element: String,
    #[serde(default = "default_energy_per_element_use")]
    pub energy_per_use: i32,
    /// true：不统计「接收方自己」释放的该属性技能（迷嶂布莱克：仅其他精灵的地系技能）
    #[serde(default)]
    pub exclude_self_as_attacker: bool,
}

fn default_energy_per_element_use() -> i32 {
    3
}

#[derive(Debug, Deserialize, Clone)]
pub struct PetBattlePatchEntry {
    pub pet_name: String,
    #[serde(default)]
    pub initial_energy: Option<i32>,
    /// 为 true 时可超过常规 10 点；`energy_cap` 为软上限（缺省 99）。
    #[serde(default)]
    pub allow_over_max_energy: bool,
    #[serde(default)]
    pub energy_cap: Option<i32>,
    /// 入场时：当前能量 += 该值 × 己方「盟友应对成功」计数，然后计数清零。
    #[serde(default)]
    pub on_entry_energy_per_prior_ally_counter: Option<i32>,
    /// 入场时：当前能量 += energy_per_use × 己方对应属性技能计数，然后该属性计数清零。
    #[serde(default)]
    pub on_entry_per_element_skill_bank: Option<ElementSkillBankRule>,
}

#[derive(Debug, Clone)]
pub struct PetBattlePatchTable {
    by_name: HashMap<String, PetBattlePatchEntry>,
}

impl PetBattlePatchTable {
    pub fn load_or_empty() -> Self {
        Self::load_from(DEFAULT_PATCH_PATH).unwrap_or_else(|_| Self { by_name: HashMap::new() })
    }

    pub fn load_from(path: &str) -> Result<Self, String> {
        let raw = fs::read_to_string(path).map_err(|e| format!("读取精灵战斗补丁 {} 失败: {}", path, e))?;
        let file: PetBattlePatchFile =
            serde_json::from_str(&raw).map_err(|e| format!("解析精灵战斗补丁失败: {}", e))?;
        let mut by_name = HashMap::new();
        for e in file.entries {
            by_name.insert(e.pet_name.clone(), e);
        }
        Ok(Self { by_name })
    }

    pub fn get(&self, pet_name: &str) -> Option<&PetBattlePatchEntry> {
        self.by_name.get(pet_name)
    }

    /// 入场时按「盟友应对计数」结算能量的接收方；此类精灵在己方先手且仅因应对成功时，不增加计数。
    pub fn is_ally_counter_bank_receiver(&self, pet_name: &str) -> bool {
        self.get(pet_name)
            .and_then(|e| e.on_entry_energy_per_prior_ally_counter)
            .is_some()
    }
}
