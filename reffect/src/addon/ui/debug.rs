use super::Addon;
use crate::{
    clipboard::Clipboard,
    colors::{self, Colored},
    context::{
        Ability, AbilityInfo, BuffMap, Build, CombatantResources, Context, Defiance, Gear,
        PlayerHealth, PlayerResources, SkillId, SkillInfo, Skillbar, Slot,
    },
    error::Error,
    internal::{Interface, Internal},
    named::Named,
};
use nexus::imgui::{StyleColor, TreeNode, TreeNodeFlags, Ui, Window};
use std::{
    cmp::Ordering,
    fmt::{self, Write},
    sync::atomic::{self, AtomicU32, AtomicUsize},
};
use strum::IntoEnumIterator;

/// Categories offered by the target classification probe, matching the sampling
/// checklist in `docs/superpowers/plans/2026-10-08-target-classification-recovery.md`.
const PROBE_LABELS: [&str; 13] = [
    "player",
    "npc",
    "normal",
    "veteran",
    "elite",
    "champion",
    "legendary",
    "epic",
    "neutral",
    "ore",
    "wood",
    "plant",
    "chest",
];

/// Sequence of the target that was last tagged from the debug window.
static PROBE_LAST_TAG_SEQUENCE: AtomicU32 = AtomicU32::new(0);
/// Index into [`PROBE_LABELS`] plus one, or 0 when nothing was tagged yet.
static PROBE_LAST_TAG_LABEL: AtomicUsize = AtomicUsize::new(0);

/// Returns the last tag applied from the debug window as `(sequence, label)`.
fn probe_last_tag() -> Option<(u32, &'static str)> {
    let index = PROBE_LAST_TAG_LABEL.load(atomic::Ordering::Relaxed);
    let label = PROBE_LABELS.get(index.checked_sub(1)?)?;
    Some((
        PROBE_LAST_TAG_SEQUENCE.load(atomic::Ordering::Relaxed),
        label,
    ))
}

impl Addon {
    pub fn render_debug(&mut self, ui: &Ui, ctx: &Context) {
        Window::new("Reffect Open 调试##reffect-open-debug")
            .collapsible(false)
            .always_auto_resize(true)
            .opened(&mut self.debug)
            .build(ui, || {
                ui.text(format!("Show elements: {}", ctx.ui.should_show()));

                ui.text(format!("Combat: {}", ctx.ui.combat));

                ui.text("Profession:");
                ui.same_line();
                name_or_unknown_id_colored(ui, ctx.player.prof);

                ui.text("Specialization:");
                ui.same_line();
                name_or_unknown_id_colored(ui, ctx.player.spec);

                ui.text("Race:");
                ui.same_line();
                ui.text(match ctx.player.race {
                    Ok(value) => value.to_string(),
                    Err(id) => format!("Unknown ({id})"),
                });

                ui.text("Mount:");
                ui.same_line();
                name_or_unknown_id_colored(ui, ctx.player.mount);

                ui.text("Target affinity:");
                ui.same_line();
                match &ctx.target.affinity {
                    Ok(affinity) => {
                        let _color = affinity.push_text_color(ui);
                        ui.text(affinity)
                    }
                    Err(err) => ui.text_colored(colors::RED, err.to_string()),
                }

                ui.text("Target category:");
                ui.same_line();
                match &ctx.target.category {
                    Ok(category) if category.is_empty() => ui.text("-"),
                    Ok(category) => {
                        for (i, entry) in category.iter().enumerate() {
                            if i != 0 {
                                ui.same_line();
                            }
                            let _color = entry.push_text_color(ui);
                            ui.text(entry);
                        }
                    }
                    Err(err) => ui.text_colored(colors::RED, err.to_string()),
                }

                render_target_probe(ui);

                ui.text(format!("Map id: {}", ctx.map.id));
                ui.text(format!("Map category: {}", ctx.map.category));

                debug_result_tree(ui, "plgear", "Player gear", &ctx.player.gear, |gear| {
                    let Gear {
                        weapons,
                        sigils,
                        relic,
                    } = gear;

                    ui.text("Sigils:");
                    for sigil in sigils {
                        ui.same_line();
                        ui.text(format!("{sigil: >5}"));
                    }
                    ui.text(format!("Relic: {relic: >5}"));

                    ui.text("Weapons:");
                    ui.indent();
                    for weapon in weapons.iter() {
                        ui.text(weapon);
                    }
                    ui.unindent();
                });

                debug_result_tree(ui, "plbuild", "Player build", &ctx.player.build, |build| {
                    let Build {
                        specs,
                        traits,
                        skill_selections,
                        prof_selections,
                        pets,
                    } = build;

                    let [spec1, spec2, spec3] = specs;
                    ui.text(format!("Specs: {spec1: >2} {spec2: >2} {spec3: >2}"));

                    ui.text("Traits:");
                    ui.indent();
                    for [adept, master, grandmaster] in [
                        [traits[0], traits[1], traits[2]],
                        [traits[3], traits[4], traits[5]],
                        [traits[6], traits[7], traits[8]],
                    ] {
                        ui.text(format!("{adept: >4} {master: >4} {grandmaster: >4}"));
                    }
                    ui.unindent();

                    ui.text("Skill selections:");
                    if !skill_selections.is_empty() {
                        ui.indent();
                        let mut text = String::new();
                        for (i, skill) in skill_selections.iter().enumerate() {
                            if i != 0 && i.is_multiple_of(3) {
                                text.push('\n');
                            }
                            let _ = write!(&mut text, "{skill: >5} ");
                        }
                        ui.text(text);
                        ui.unindent();
                    }

                    ui.text("Profession selections:");
                    ui.indent();
                    for info in prof_selections.iter() {
                        let _color = info.push_text_color(ui);
                        ui.text(info);
                        ui.same_line();
                    }
                    ui.new_line();
                    ui.unindent();

                    let [pet1, pet2] = pets;
                    ui.text(format!("Pets: {pet1} {pet2}"));
                });

                debug_result_tree(ui, "plhp", "Player health", &ctx.player.health, |health| {
                    let PlayerHealth {
                        combatant,
                        health_reduction,
                    } = health;

                    debug_combatant_resources(ui, combatant, false);
                    ui.text(format!("Health reduction: {health_reduction}"));
                });

                debug_result_tree(
                    ui,
                    "plres",
                    "Player resources",
                    &ctx.player.resources,
                    |resources| {
                        let PlayerResources {
                            pet: _,
                            endurance,
                            primary,
                            secondary,
                            rate,
                        } = resources;

                        ui.text(format!("Endurance: {endurance}",));
                        ui.text(format!("Primary: {primary}"));
                        ui.text(format!("Secondary: {secondary}"));
                        ui.text(format!("Rate: {rate}"));
                    },
                );

                debug_result_tree(
                    ui,
                    "plbuffs",
                    "Player buffs",
                    &ctx.player.buff_info,
                    |buff_info| {
                        ui.text(format!(
                            "Last screen border: {}",
                            buff_info.last_screen_border
                        ));
                        ui.text(format!(
                            "Last squad highlight: {}",
                            buff_info.last_squad_highlight
                        ));

                        ui.spacing();
                        debug_buffs(ui, ctx, &buff_info.buffs)
                    },
                );

                debug_result_tree(
                    ui,
                    "plskills",
                    "Player skillbar",
                    &ctx.player.skillbar,
                    |skillbar| debug_skillbar(ui, ctx, skillbar),
                );

                debug_result_tree(
                    ui,
                    "plcd",
                    "Off-bar cooldowns",
                    &ctx.player.active_cooldowns,
                    |cooldowns| {
                        let offbar: Vec<_> =
                            cooldowns.iter().filter(|cd| cd.slot.is_none()).collect();
                        if offbar.is_empty() {
                            ui.text("(none)");
                            return;
                        }
                        for cd in offbar {
                            let label = match cd.skill {
                                SkillId::Id(id) => format!("{id: >5}"),
                                other => other.to_string(),
                            };
                            // Prefer the single recharge; ammo-only skills keep
                            // their cooldown on the ammo fields.
                            let (remaining, total) = if cd.recharge > 0 {
                                (cd.recharge_remaining(ctx.now), cd.recharge)
                            } else if cd.ammo_recharge > 0 {
                                (cd.ammo_recharge_remaining(ctx.now), cd.ammo_recharge)
                            } else {
                                (0, 0)
                            };
                            if total > 0 {
                                let percent =
                                    (100.0 * remaining as f32 / total as f32).clamp(0.0, 100.0);
                                ui.text(format!(
                                    "{label} {:.1}/{:.1}s {:.1}%",
                                    to_secs(remaining),
                                    to_secs(total),
                                    percent,
                                ));
                            } else {
                                ui.text(label);
                            }
                        }
                    },
                );
                debug_result_tree(
                    ui,
                    "ptres",
                    "Pet resources",
                    &ctx.player.resources,
                    |resources| {
                        if let Some(pet) = &resources.pet {
                            debug_combatant_resources(ui, pet, false);
                        } else {
                            ui.text("No pet");
                        }
                    },
                );

                debug_result_tree(
                    ui,
                    "tgres",
                    "Target resources",
                    &ctx.target.resources,
                    |resources| debug_combatant_resources(ui, resources, true),
                );

                debug_result_tree(ui, "tgbuff", "Target buffs", &ctx.target.buffs, |buffs| {
                    debug_buffs(ui, ctx, buffs)
                });

                debug_result_tree(ui, "grp", "Group", &ctx.group, |group| {
                    ui.text(format!("Group Type: {}", group.group_type));

                    for (i, member) in group.members.iter().enumerate() {
                        let label = format!(
                            "Member {}: {}",
                            i + 1,
                            if let Some(acc) = &member.account {
                                acc.strip_prefix(':').unwrap_or(acc)
                            } else {
                                "-"
                            }
                        );
                        TreeNode::new(i.to_string())
                            .label::<String, _>(label)
                            .flags(TreeNodeFlags::SPAN_AVAIL_WIDTH)
                            .build(ui, || {
                                debug_result_tree(
                                    ui,
                                    "res",
                                    "Resources",
                                    &member.resources,
                                    |resources| debug_combatant_resources(ui, resources, true),
                                );
                                debug_result_tree(ui, "buffs", "Buffs", &member.buffs, |buffs| {
                                    debug_buffs(ui, ctx, buffs)
                                });
                            });
                    }
                });

                ui.spacing();
                ui.separator();
                ui.spacing();

                ctx.edit.debug(ui);
                Clipboard::debug(ui);
            });
    }
}

/// Renders the target classification probe controls (offset recovery only).
///
/// Shows the sequence of the current selection and tags it in the log, so a
/// capture labels itself while it is taken instead of relying on the order the
/// targets were selected in.
fn render_target_probe(ui: &Ui) {
    let Some(sequence) = crate::internal::game::state::target_probe_sequence() else {
        ui.text("Target probe:");
        ui.same_line();
        ui.text_colored(colors::GREY, "no target selected");
        return;
    };

    ui.text("Target probe:");
    ui.same_line();
    ui.text(format!("seq {sequence}"));
    ui.same_line();
    match probe_last_tag() {
        Some((tagged, label)) if tagged == sequence => {
            ui.text_colored(colors::GREEN, format!("tagged \"{label}\""))
        }
        _ => ui.text_colored(colors::YELLOW, "not tagged"),
    }
    ui.text("Tag the selected target, then select the next one:");

    for (index, label) in PROBE_LABELS.iter().enumerate() {
        if index % 7 != 0 {
            ui.same_line();
        }
        if ui.small_button(*label) {
            // Same prefix and level as the probe dumps, so the diff tool picks
            // the label up from the log without any manual transcription.
            log::warn!("[reffect-target-classify] label={label} seq={sequence}");
            PROBE_LAST_TAG_SEQUENCE.store(sequence, atomic::Ordering::Relaxed);
            PROBE_LAST_TAG_LABEL.store(index + 1, atomic::Ordering::Relaxed);
        }
    }
}

fn debug_combatant_resources(ui: &Ui, resources: &CombatantResources, normalized: bool) {
    let CombatantResources {
        health,
        barrier,
        defiance,
        health_state,
    } = resources;

    let precision = if normalized { 1 } else { 0 };
    ui.text(format!("State: {}", health_state.short_name()));
    ui.text(format!("Health: {health:.*}", precision));
    ui.text(format!("Barrier: {barrier:.*}", precision));

    ui.text("Defiance:");
    ui.same_line();
    match defiance {
        None => ui.text("-"),
        Some(Defiance::Immune) => ui.text("immune"),
        Some(Defiance::Active(percent)) => ui.text(format!("active {percent:.1}%")),
        Some(Defiance::Recover(percent)) => ui.text(format!("recover {percent:.1}%")),
    }
}

fn debug_buffs(ui: &Ui, ctx: &Context, buffs: &BuffMap) {
    for (id, buff) in buffs {
        let id = *id;
        ui.group(|| {
            let category = match Internal::get_skill_info(id) {
                Ok(SkillInfo::Buff { category, .. }) => category.to_string(),
                _ => "Invalid".into(),
            };

            ui.text(format!("{:>2}x {id:>5} {category}", buff.stacks));

            if !buff.is_infinite() {
                ui.same_line();
                ui.text(format!(
                    "{:.1}/{:.1}s {:.1}%",
                    to_secs(buff.remaining(ctx.now)),
                    to_secs(buff.duration()),
                    100.0 * buff.progress(ctx.now),
                ));
            }
        });

        if ui.is_item_hovered()
            && let Ok(SkillInfo::Buff {
                stacking,
                visibility,
                ..
            }) = Internal::get_skill_info(id)
        {
            ui.tooltip(|| {
                ui.text(format!("Stacking {stacking}"));
                ui.text(format!("Visible for {visibility}"));
            });
        }
    }
}

fn debug_skillbar(ui: &Ui, ctx: &Context, skillbar: &Skillbar) {
    for slot in Slot::iter() {
        ui.text(format!("{slot:<14} ="));
        if let Some(ability) = skillbar.slot(slot) {
            let color = if ability.info.contains(AbilityInfo::Pressed) {
                Some(ui.push_style_color(StyleColor::Text, colors::BLUE))
            } else if ability.info.contains(AbilityInfo::Pending) {
                Some(ui.push_style_color(StyleColor::Text, colors::GREEN))
            } else if ability.info.contains(AbilityInfo::NoResources) {
                Some(ui.push_style_color(StyleColor::Text, colors::RED))
            } else if ability.info.contains(AbilityInfo::NoRange) {
                Some(ui.push_style_color(StyleColor::Text, colors::YELLOW))
            } else {
                None
            };

            ui.same_line();
            ui.group(|| {
                ui.text(format!("{}x {}", ability.ammo, ability.id));
                drop(color);

                let _color = match ability.recharge_rate.total_cmp(&1.0) {
                    Ordering::Less => Some(ui.push_style_color(StyleColor::Text, colors::BLUE)),
                    Ordering::Equal => None,
                    Ordering::Greater => Some(ui.push_style_color(StyleColor::Text, colors::GREEN)),
                };
                let recharge = ability.recharge_remaining(ctx.now);
                if recharge > 0 {
                    ui.same_line();
                    ui.text(format!(
                        "{:.1}/{:.1}s {:.1}%",
                        to_secs(recharge),
                        to_secs(ability.recharge),
                        100.0 * ability.recharge_progress(ctx.now)
                    ));
                }

                let ammo_recharge = ability.ammo_recharge_remaining(ctx.now);
                if ammo_recharge > 0 {
                    ui.same_line();
                    ui.text(format!(
                        "Ammo {:.1}/{:.1}s {:.1}%",
                        to_secs(ammo_recharge),
                        to_secs(ability.ammo_recharge),
                        100.0 * ability.ammo_recharge_progress(ctx.now)
                    ));
                }
            });
            if ui.is_item_hovered() {
                ui.tooltip(|| {
                    let Ability {
                        recharge_rate,
                        info: state,
                        ..
                    } = ability;
                    ui.text(format!("Rate {recharge_rate}"));
                    ui.text(format!("State {state}"));

                    if let SkillId::Id(id) = ability.id
                        && let Ok(SkillInfo::Ability {
                            is_instant,
                            is_ammo,
                            is_ground_targeted,
                            is_stunbreak,
                        }) = Internal::get_skill_info(id)
                    {
                        ui.text(format!("Instant {is_instant}"));
                        ui.text(format!("Ammo {is_ammo}"));
                        ui.text(format!("Ground Targeted {is_ground_targeted}"));
                        ui.text(format!("Stunbreak {is_stunbreak}"));
                    }
                });
            }
        }
    }
}

fn debug_result_tree<T>(
    ui: &Ui,
    id: impl AsRef<str>,
    label: impl AsRef<str>,
    value: &Result<T, Error>,
    body: impl FnOnce(&T),
) {
    TreeNode::new(id)
        .label::<&str, _>(label.as_ref())
        .flags(TreeNodeFlags::SPAN_AVAIL_WIDTH)
        .build(ui, || match value {
            Ok(value) => {
                body(value);
            }
            Err(err) => ui.text_colored(colors::RED, format!("Error: {err}")),
        });
}

fn name_or_unknown_id_colored<T, N>(ui: &Ui, value: Result<T, N>)
where
    T: AsRef<str> + Colored,
    N: fmt::Display,
{
    match value {
        Ok(value) => {
            let _color = value.push_text_color(ui);
            ui.text(value);
        }
        Err(id) => ui.text(format!("Unknown ({id})")),
    }
}

fn to_secs(millisecs: u32) -> f32 {
    millisecs as f32 / 1000.0
}
