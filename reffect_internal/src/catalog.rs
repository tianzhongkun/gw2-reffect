use enumflags2::BitFlags;
use nexus::texture::get_texture_or_create_from_url;
use reffect_core::{
    Texture,
    context::{Category, ItemInfo, SkillInfo, Stacking, Visibility},
    error::{Error, Result},
};
use serde::Deserialize;
use std::{
    collections::{HashMap, HashSet},
    sync::{Mutex, MutexGuard, OnceLock},
    thread::{self, JoinHandle},
    time::Duration,
};

const API_URL: &str = "https://api.guildwars2.com/v2";
const USER_AGENT: &str = "gw2-reffect-open-internal/0.1";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

static CATALOG: OnceLock<Mutex<Catalog>> = OnceLock::new();

type CatalogWorker = fn(u64);

#[derive(Debug, Default)]
struct Catalog {
    active: bool,
    generation: u64,
    workers: Vec<JoinHandle<()>>,
    skills: HashMap<u32, CachedSkill>,
    runtime_skills: HashMap<u32, SkillInfo>,
    /// Skills/effects classified from the game's runtime content tables (the
    /// same tables the original reads).  This covers internal effect ids the
    /// public API does not expose, so `get_skill_info` can label them (e.g. a
    /// generic "Effect" category) instead of failing.  Kept separate from
    /// `runtime_skills` so the buff publish/filter decisions in the state
    /// reader keep using the definition-pointer test and do not over-publish
    /// equipment/sigil/relic permanent effects.
    content_skills: HashMap<u32, SkillInfo>,
    items: HashMap<u32, ItemInfo>,
    items_by_buff: HashMap<u32, u32>,
    missing_skills: HashSet<u32>,
    missing_items: HashSet<u32>,
    /// Ids whose on-demand fetch is already in flight on a detached worker, so
    /// the render thread never issues a second request or blocks on the first.
    pending_skills: HashSet<u32>,
    pending_items: HashSet<u32>,
}

#[derive(Debug, Clone)]
struct CachedSkill {
    info: SkillInfo,
    icon: String,
}

#[derive(Debug, Deserialize)]
struct ApiSkill {
    id: u32,
    #[serde(rename = "type")]
    skill_type: Option<String>,
    #[serde(default)]
    flags: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_or_default")]
    facts: Vec<ApiFact>,
    #[serde(default)]
    icon: String,
}

#[derive(Debug, Deserialize)]
struct ApiFact {
    #[serde(rename = "type", default, deserialize_with = "deserialize_or_default")]
    fact_type: String,
    #[serde(default, deserialize_with = "deserialize_or_default")]
    text: String,
}

#[derive(Debug, Deserialize)]
struct ApiItem {
    id: u32,
    #[serde(rename = "type")]
    item_type: String,
    details: Option<ApiItemDetails>,
}

#[derive(Debug, Deserialize)]
struct ApiItemDetails {
    #[serde(rename = "type")]
    upgrade_type: String,
    infix_upgrade: Option<ApiInfixUpgrade>,
}

#[derive(Debug, Deserialize)]
struct ApiInfixUpgrade {
    buff: Option<ApiItemBuff>,
}

#[derive(Debug, Deserialize)]
struct ApiItemBuff {
    skill_id: u32,
}

fn deserialize_or_default<'de, D, T>(deserializer: D) -> std::result::Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Option::<T>::deserialize(deserializer).map(Option::unwrap_or_default)
}

impl From<ApiSkill> for CachedSkill {
    fn from(skill: ApiSkill) -> Self {
        let is_instant = skill.flags.iter().any(|flag| flag == "InstantCast");
        let is_ground_targeted = skill.flags.iter().any(|flag| flag == "GroundTargeted");
        let is_ammo = skill.facts.iter().any(|fact| fact.text == "Maximum Count");
        let is_stunbreak = skill.facts.iter().any(|fact| {
            fact.fact_type == "StunBreak"
                || matches!(fact.text.as_str(), "Break Stun" | "Breaks Stun")
        });

        // The API omits `type` for a number of valid skills (for example
        // mechanist commands).  A returned skill is still an ability; game
        // effects use a different id space and are handled by `known_buff`.
        let _skill_type = skill.skill_type;
        Self {
            info: SkillInfo::Ability {
                is_instant,
                is_ammo,
                is_ground_targeted,
                is_stunbreak,
            },
            icon: skill.icon,
        }
    }
}

impl ApiItem {
    fn into_info(self) -> Option<(u32, ItemInfo)> {
        let details = self.details?;
        let buff = details.infix_upgrade?.buff?.skill_id;
        let info = match (self.item_type.as_str(), details.upgrade_type.as_str()) {
            ("UpgradeComponent", "Sigil") => ItemInfo::Sigil { buff },
            // Older API responses represented relics as upgrade components;
            // keep this branch for archived API data as well as current data.
            ("UpgradeComponent", "Relic") | ("Relic", "Relic") => ItemInfo::Relic { buff },
            _ => return None,
        };
        Some((self.id, info))
    }
}

pub(super) fn init() {
    let mut catalog = lock_catalog();
    if catalog.active {
        return;
    }

    catalog.active = true;
    catalog.generation = catalog.generation.wrapping_add(1);
    let generation = catalog.generation;

    for (name, task) in startup_workers() {
        match thread::Builder::new()
            .name(name.into())
            .spawn(move || task(generation))
        {
            Ok(worker) => catalog.workers.push(worker),
            Err(error) => log::warn!("Failed to create {name} worker: {error}"),
        }
    }
}

/// Metadata workers must remain independent of game-owned thread-affine
/// objects.  Runtime content-table vtables are not safe to call from here.
fn startup_workers() -> [(&'static str, CatalogWorker); 1] {
    [("reffect-gw2-api", load_skill_catalog)]
}

pub(super) fn deinit() {
    let workers = {
        let mut catalog = lock_catalog();
        catalog.active = false;
        catalog.generation = catalog.generation.wrapping_add(1);
        catalog.skills.clear();
        catalog.items.clear();
        catalog.items_by_buff.clear();
        catalog.missing_skills.clear();
        catalog.missing_items.clear();
        catalog.runtime_skills.clear();
        catalog.content_skills.clear();
        catalog.pending_skills.clear();
        catalog.pending_items.clear();
        std::mem::take(&mut catalog.workers)
    };

    for worker in workers {
        if worker.join().is_err() {
            log::warn!("Reffect internal metadata worker panicked");
        }
    }
}

pub(super) fn get_item_info(id: u32) -> Result<ItemInfo> {
    let mut catalog = lock_catalog();
    ensure_active(&catalog)?;
    if let Some(info) = catalog.items.get(&id) {
        return Ok(info.clone());
    }
    if catalog.missing_items.contains(&id) {
        return Err(Error::Item);
    }

    // Item validation happens only when configuration changes, so an on-demand
    // request avoids scanning tens of thousands of unrelated item records. The
    // fetch runs on a detached worker: this function is called every frame from
    // trigger evaluation on the render thread, so it must never block on HTTP.
    if catalog.pending_items.insert(id) {
        let generation = catalog.generation;
        thread::Builder::new()
            .name("reffect-gw2-item".into())
            .spawn(move || fetch_item(id, generation))
            .ok();
    }
    Err(Error::Item)
}

/// Fetches a single item on a detached worker and folds the result back into
/// the catalog, discarding it if the catalog has been reset in the meantime.
fn fetch_item(id: u32, generation: u64) {
    let item = request_one::<ApiItem>(&format!("items/{id}")).and_then(ApiItem::into_info);
    let mut catalog = lock_catalog();
    if catalog.generation != generation {
        return;
    }
    catalog.pending_items.remove(&id);
    match item {
        Some((item_id, info)) => {
            let buff = info.buff().expect("supported items always expose a buff");
            catalog.items_by_buff.insert(buff, item_id);
            catalog.items.insert(item_id, info);
        }
        None => {
            catalog.missing_items.insert(id);
        }
    }
}

pub(super) fn get_item_from_buff(id: u32) -> Result<u32> {
    let catalog = lock_catalog();
    ensure_active(&catalog)?;
    catalog.items_by_buff.get(&id).copied().ok_or(Error::Item)
}

/// Returns whether a known runtime effect id is a buff without requesting data.
pub(super) fn cached_is_buff(id: u32) -> bool {
    if known_buff(id).is_some() {
        return true;
    }
    matches!(
        lock_catalog().runtime_skills.get(&id),
        Some(SkillInfo::Buff { .. })
    )
}

/// Returns whether a known runtime effect is visible as a group boon or
/// condition. Generic effects are deliberately excluded: the game only shows
/// them in party UI and not consistently for squads.
pub(super) fn cached_is_group_buff(id: u32) -> bool {
    if let Some((SkillInfo::Buff { visibility, .. }, _)) = known_buff(id) {
        return visibility.contains(Visibility::Group);
    }
    matches!(
        lock_catalog().runtime_skills.get(&id),
        Some(SkillInfo::Buff {
            category: Category::Boon | Category::Condition,
            visibility,
            ..
        }) if visibility.contains(Visibility::Group)
    )
}

pub(super) fn get_skill_info(id: u32) -> Result<SkillInfo> {
    let mut catalog = lock_catalog();
    ensure_active(&catalog)?;
    if let Some(info) = known_buff(id).map(|(info, _)| info) {
        return Ok(info);
    }
    if let Some(info) = catalog.runtime_skills.get(&id) {
        return Ok(info.clone());
    }
    if let Some(skill) = catalog.skills.get(&id) {
        return Ok(skill.info.clone());
    }
    // Internal content-table effects come after the public API so that known
    // abilities keep their API-derived `SkillInfo::Ability`; the content scan
    // only fills in otherwise-unknown ids (mostly buffs/effects) for labels.
    if let Some(info) = catalog.content_skills.get(&id) {
        return Ok(info.clone());
    }
    if catalog.missing_skills.contains(&id) {
        return Err(Error::Skill);
    }

    // The bulk worker normally fills this cache before the editor is opened.
    // This fallback also makes the interface usable when the worker could not
    // be created or an id was introduced after its catalogue was downloaded.
    // It runs on a detached worker rather than inline: this function is called
    // every frame from trigger evaluation on the render thread, and a blocking
    // HTTP request there froze the game whenever a freshly-cast skill's id was
    // seen for the first time.
    if catalog.pending_skills.insert(id) {
        let generation = catalog.generation;
        thread::Builder::new()
            .name("reffect-gw2-skill".into())
            .spawn(move || fetch_skill(id, generation))
            .ok();
    }
    Err(Error::Skill)
}

/// Fetches a single skill on a detached worker and folds the result back into
/// the catalog, discarding it if the catalog has been reset in the meantime.
fn fetch_skill(id: u32, generation: u64) {
    let skill = request_one::<Vec<ApiSkill>>(&format!("skills?ids={id}"))
        .and_then(|mut skills| skills.pop())
        .map(CachedSkill::from);
    let mut catalog = lock_catalog();
    if catalog.generation != generation {
        return;
    }
    catalog.pending_skills.remove(&id);
    match skill {
        Some(skill) => {
            catalog.skills.insert(id, skill);
        }
        None => {
            catalog.missing_skills.insert(id);
        }
    }
}

/// Folds the game's runtime content-table skill catalogue into
/// [`Catalog::content_skills`].  The scan needs a valid game-thread capture
/// (`content::scan_skills` fails closed without one), so this is called from
/// the state worker once a snapshot is available — never from the startup API
/// worker, which has no capture and must stay independent of thread-affine
/// game objects.  Returns the number of entries folded in.
pub(super) fn ingest_content_skills() -> usize {
    let Ok(scan) = crate::game::content::scan_skills() else {
        return 0;
    };
    let mut catalog = lock_catalog();
    if !catalog.active {
        return 0;
    }
    let before = catalog.content_skills.len();
    catalog.content_skills.extend(scan);
    catalog.content_skills.len().saturating_sub(before)
}

pub(super) fn get_skill_icon(id: u32) -> Option<Texture> {
    let icon = {
        let catalog = lock_catalog();
        if !catalog.active {
            return None;
        }
        known_buff(id)
            .map(|(_, icon)| icon.to_owned())
            .or_else(|| catalog.skills.get(&id).map(|skill| skill.icon.clone()))
    }?;

    let (remote, endpoint) = split_remote_url(&icon)?;
    get_texture_or_create_from_url(format!("REFFECT_SKILL_ICON_{id}"), remote, endpoint)
        .map(|texture| texture.resource)
}

fn load_skill_catalog(generation: u64) {
    let Some(skills) = request_one::<Vec<ApiSkill>>("skills?ids=all") else {
        log::warn!("Failed to download Guild Wars 2 skill catalogue");
        return;
    };

    let skills = skills
        .into_iter()
        .map(|skill| (skill.id, CachedSkill::from(skill)))
        .collect::<HashMap<_, _>>();
    let count = skills.len();

    let mut catalog = lock_catalog();
    if catalog.active && catalog.generation == generation {
        catalog.skills.extend(skills);
        log::debug!("Loaded {count} Guild Wars 2 skill records");
    }
}

fn request_one<T>(endpoint: &str) -> Option<T>
where
    T: serde::de::DeserializeOwned,
{
    ureq::AgentBuilder::new()
        .timeout_connect(REQUEST_TIMEOUT)
        .timeout_read(REQUEST_TIMEOUT)
        .timeout_write(REQUEST_TIMEOUT)
        .build()
        .get(&format!("{API_URL}/{endpoint}"))
        .set("User-Agent", USER_AGENT)
        .call()
        .ok()?
        .into_json()
        .ok()
}

fn ensure_active(catalog: &Catalog) -> Result<()> {
    if catalog.active {
        Ok(())
    } else {
        Err(Error::NotInitialized)
    }
}

fn lock_catalog() -> MutexGuard<'static, Catalog> {
    CATALOG
        .get_or_init(|| Mutex::new(Catalog::default()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn split_remote_url(url: &str) -> Option<(&str, &str)> {
    let (scheme, remainder) = url.split_once("://")?;
    if !matches!(scheme, "http" | "https") {
        return None;
    }
    let endpoint_index = remainder.find('/')?;
    Some((
        &url[..scheme.len() + 3 + endpoint_index],
        &remainder[endpoint_index..],
    ))
}

fn visible_everywhere() -> BitFlags<Visibility> {
    BitFlags::from_flag(Visibility::Player)
        | BitFlags::from_flag(Visibility::TargetNonHostile)
        | BitFlags::from_flag(Visibility::TargetHostile)
        | BitFlags::from_flag(Visibility::Group)
}

/// Metadata for universal effects absent from `/v2/skills`.
///
/// The identifiers and icons are stable game data.  They make the common
/// player, target and group triggers useful without the private memory reader.
fn known_buff(id: u32) -> Option<(SkillInfo, &'static str)> {
    const MIGHT: &str =
        "https://render.guildwars2.com/file/2FA9DF9D6BC17839BBEA14723F1C53D645DDB5E1/102852.png";
    const FURY: &str =
        "https://render.guildwars2.com/file/96D90DF84CAFE008233DD1C2606A12C1A0E68048/102842.png";
    const SWIFTNESS: &str =
        "https://render.guildwars2.com/file/20CFC14967E67F7A3FD4A4B8722B4CF5B8565E11/102836.png";
    const VIGOR: &str =
        "https://render.guildwars2.com/file/58E92EBAF0DB4DA7C4AC04D9B22BCA5ECF0100DE/102843.png";
    const REGENERATION: &str =
        "https://render.guildwars2.com/file/F69996772B9E18FD18AD0AABAB25D7E3FC42F261/102835.png";
    const PROTECTION: &str =
        "https://render.guildwars2.com/file/CD77D1FAB7B270223538A8F8ECDA1CFB044D65F4/102834.png";
    const QUICKNESS: &str =
        "https://render.guildwars2.com/file/D4AB6401A6D6917C3D4F230764452BCCE1035B0D/1012835.png";
    const ALACRITY: &str =
        "https://render.guildwars2.com/file/4FDAC2113B500104121753EF7E026E45C141E94D/1938787.png";
    const RESISTANCE: &str =
        "https://render.guildwars2.com/file/50BAC1B8E10CFAB9E749A5D910D4A9DCF29EBB7C/961398.png";
    const RESOLUTION: &str =
        "https://render.guildwars2.com/file/D104A6B9344A2E2096424A3C300E46BC2926E4D7/2440718.png";
    const STABILITY: &str =
        "https://render.guildwars2.com/file/3D3A1C2D6D791C05179AB871902D28782C65C244/415959.png";
    const AEGIS: &str =
        "https://render.guildwars2.com/file/DFB4D1B50AE4D6A275B349E15B179261EE3EB0AF/102854.png";
    const BLEEDING: &str =
        "https://render.guildwars2.com/file/79FF0046A5F9ADA3B4C4EC19ADB4CB124D5F0021/102848.png";
    const BURNING: &str =
        "https://render.guildwars2.com/file/B47BF5803FED2718D7474EAF9617629AD068EE10/102849.png";
    const CHILLED: &str =
        "https://render.guildwars2.com/file/28C4EC547A3516AF0242E826772DA43A5EAC3DF3/102839.png";
    const CONFUSION: &str =
        "https://render.guildwars2.com/file/289AA0A4644F0E044DED3D3F39CED958E1DDFF53/102880.png";
    const CRIPPLED: &str =
        "https://render.guildwars2.com/file/070325E519C178D502A8160523766070D30C0C19/102838.png";
    const FEAR: &str =
        "https://render.guildwars2.com/file/30307A6E766D74B6EB09EDA12A4A2DE50E4D76F4/102869.png";
    const IMMOBILIZED: &str =
        "https://render.guildwars2.com/file/397A613651BFCA2832B6469CE34735580A2C120E/102844.png";
    const POISONED: &str =
        "https://render.guildwars2.com/file/559B0AF9FB5E1243D2649FAAE660CCB338AACC19/102840.png";
    const SLOW: &str =
        "https://render.guildwars2.com/file/F60D1EF5271D7B9319610855676D320CD25F01C6/961397.png";
    const TAUNT: &str =
        "https://render.guildwars2.com/file/02EED459AD65FAF7DF32A260E479C625070841B9/1228472.png";
    const TORMENT: &str =
        "https://render.guildwars2.com/file/10BABF2708CA3575730AC662A2E72EC292565B08/598887.png";
    const VULNERABILITY: &str =
        "https://render.guildwars2.com/file/3A394C1A0A3257EB27A44842DDEEF0DF000E1241/102850.png";
    const WEAKNESS: &str =
        "https://render.guildwars2.com/file/6CB0E64AF9AA292E332A38C1770CE577E2CDE0E8/102853.png";
    const BLINDED: &str =
        "https://render.guildwars2.com/file/09770136BB76FD0DBE1CC4267DEED54774CB20F6/102837.png";

    let (category, stacking, icon) = match id {
        740 => (Category::Boon, Stacking::Intensity, MIGHT),
        717 => (Category::Boon, Stacking::Duration, PROTECTION),
        718 => (Category::Boon, Stacking::Duration, REGENERATION),
        719 => (Category::Boon, Stacking::Duration, SWIFTNESS),
        725 => (Category::Boon, Stacking::Duration, FURY),
        726 => (Category::Boon, Stacking::Duration, VIGOR),
        743 => (Category::Boon, Stacking::Duration, AEGIS),
        873 => (Category::Boon, Stacking::Duration, RESOLUTION),
        1122 => (Category::Boon, Stacking::Duration, STABILITY),
        1187 => (Category::Boon, Stacking::Duration, QUICKNESS),
        26980 => (Category::Boon, Stacking::Duration, RESISTANCE),
        30328 => (Category::Boon, Stacking::Duration, ALACRITY),
        720 => (Category::Condition, Stacking::Duration, BLINDED),
        721 => (Category::Condition, Stacking::Duration, CRIPPLED),
        722 => (Category::Condition, Stacking::Duration, CHILLED),
        723 => (Category::Condition, Stacking::Intensity, POISONED),
        727 => (Category::Condition, Stacking::Duration, IMMOBILIZED),
        736 => (Category::Condition, Stacking::Intensity, BLEEDING),
        737 => (Category::Condition, Stacking::Intensity, BURNING),
        738 => (Category::Condition, Stacking::Intensity, VULNERABILITY),
        742 => (Category::Condition, Stacking::Duration, WEAKNESS),
        791 => (Category::Condition, Stacking::Duration, FEAR),
        861 => (Category::Condition, Stacking::Intensity, CONFUSION),
        19426 => (Category::Condition, Stacking::Intensity, TORMENT),
        26766 => (Category::Condition, Stacking::Duration, SLOW),
        27705 => (Category::Condition, Stacking::Duration, TAUNT),
        // These effect ids are client-internal and intentionally absent from
        // the public API.  They were observed in the original reader's live
        // Player Buffs output for this client build.
        27_890 | 33_613 | 35_126 | 48_894 => (Category::Effect, Stacking::Other, ""),
        _ => return None,
    };

    Some((
        SkillInfo::Buff {
            category,
            stacking,
            visibility: visible_everywhere(),
        },
        icon,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sigil_item() {
        let item: ApiItem = serde_json::from_str(
            r#"{
                "id": 24615,
                "type": "UpgradeComponent",
                "details": {
                    "type": "Sigil",
                    "infix_upgrade": { "buff": { "skill_id": 9322 } }
                }
            }"#,
        )
        .unwrap();

        let (id, info) = item.into_info().unwrap();
        assert_eq!(id, 24615);
        assert!(matches!(info, ItemInfo::Sigil { buff: 9322 }));
    }

    #[test]
    fn rejects_relic_without_private_buff_data() {
        let item: ApiItem =
            serde_json::from_str(r#"{ "id": 100916, "type": "Relic", "details": null }"#).unwrap();

        assert!(item.into_info().is_none());
    }

    #[test]
    fn reads_skill_metadata_from_api_shape() {
        let skill: ApiSkill = serde_json::from_str(
            r#"{
                "id": 10238,
                "type": "Utility",
                "flags": ["GroundTargeted", "InstantCast"],
                "facts": [
                    { "type": "StunBreak", "text": "StunBreak" },
                    { "type": "Number", "text": "Maximum Count" }
                ],
                "icon": "https://render.guildwars2.com/file/hash/10238.png"
            }"#,
        )
        .unwrap();

        let skill = CachedSkill::from(skill);
        assert!(matches!(
            skill.info,
            SkillInfo::Ability {
                is_instant: true,
                is_ammo: true,
                is_ground_targeted: true,
                is_stunbreak: true,
            }
        ));
    }

    #[test]
    fn accepts_null_facts_from_the_bulk_skill_catalogue() {
        let skill: ApiSkill = serde_json::from_str(
            r#"{
                "id": 5492,
                "type": "Utility",
                "flags": [],
                "facts": null,
                "icon": ""
            }"#,
        )
        .unwrap();

        let skill = CachedSkill::from(skill);
        assert!(matches!(
            skill.info,
            SkillInfo::Ability {
                is_instant: false,
                is_ammo: false,
                is_ground_targeted: false,
                is_stunbreak: false,
            }
        ));
    }

    #[test]
    fn accepts_null_fields_inside_bulk_skill_facts() {
        let skill: ApiSkill = serde_json::from_str(
            r#"{
                "id": 5493,
                "type": null,
                "flags": [],
                "facts": [{ "type": null, "text": null }],
                "icon": ""
            }"#,
        )
        .unwrap();

        let skill = CachedSkill::from(skill);
        assert!(matches!(
            skill.info,
            SkillInfo::Ability {
                is_instant: false,
                is_ammo: false,
                is_ground_targeted: false,
                is_stunbreak: false,
            }
        ));
    }

    #[test]
    fn startup_workers_never_start_a_runtime_content_reader() {
        let workers = startup_workers();

        assert_eq!(
            workers
                .into_iter()
                .map(|(name, _)| name)
                .collect::<Vec<_>>(),
            vec!["reffect-gw2-api"]
        );
    }

    #[test]
    fn knows_standard_boons_and_conditions() {
        let (might, _) = known_buff(740).unwrap();
        let (burning, _) = known_buff(737).unwrap();
        assert!(matches!(
            might,
            SkillInfo::Buff {
                category: Category::Boon,
                stacking: Stacking::Intensity,
                ..
            }
        ));
        assert!(matches!(
            burning,
            SkillInfo::Buff {
                category: Category::Condition,
                stacking: Stacking::Intensity,
                ..
            }
        ));
    }

    #[test]
    fn knows_observed_internal_player_effects_without_runtime_vtable_metadata() {
        for id in [27_890, 33_613, 35_126, 48_894] {
            let (info, _) = known_buff(id).expect("observed player effect must stay visible");
            assert!(matches!(
                info,
                SkillInfo::Buff {
                    category: Category::Effect,
                    stacking: Stacking::Other,
                    ..
                }
            ));
            assert!(cached_is_buff(id));
        }
    }

    #[test]
    fn splits_render_url() {
        assert_eq!(
            split_remote_url("https://render.guildwars2.com/file/hash/icon.png"),
            Some(("https://render.guildwars2.com", "/file/hash/icon.png"))
        );
        assert_eq!(split_remote_url("file:///not-a-remote"), None);
    }
}
