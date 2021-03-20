use common::{
    combat::{AttackerInfo, TargetInfo},
    comp::{
        Body, CharacterState, Combo, Energy, Group, Health, Inventory, Melee, Ori, Pos, Scale,
        Stats,
    },
    event::{EventBus, ServerEvent},
    uid::Uid,
    util::Dir,
    GroupTarget,
};
use common_ecs::{Job, Origin, Phase, System};
use specs::{
    shred::ResourceId, Entities, Join, Read, ReadStorage, SystemData, World, WriteStorage,
};
use vek::*;

#[derive(SystemData)]
pub struct ReadData<'a> {
    entities: Entities<'a>,
    uids: ReadStorage<'a, Uid>,
    positions: ReadStorage<'a, Pos>,
    orientations: ReadStorage<'a, Ori>,
    scales: ReadStorage<'a, Scale>,
    bodies: ReadStorage<'a, Body>,
    healths: ReadStorage<'a, Health>,
    energies: ReadStorage<'a, Energy>,
    inventories: ReadStorage<'a, Inventory>,
    groups: ReadStorage<'a, Group>,
    char_states: ReadStorage<'a, CharacterState>,
    server_bus: Read<'a, EventBus<ServerEvent>>,
    stats: ReadStorage<'a, Stats>,
    combos: ReadStorage<'a, Combo>,
}

/// This system is responsible for handling accepted inputs like moving or
/// attacking
#[derive(Default)]
pub struct Sys;

impl<'a> System<'a> for Sys {
    type SystemData = (ReadData<'a>, WriteStorage<'a, Melee>);

    const NAME: &'static str = "melee";
    const ORIGIN: Origin = Origin::Common;
    const PHASE: Phase = Phase::Create;

    fn run(_job: &mut Job<Self>, (read_data, mut melee_attacks): Self::SystemData) {
        let mut server_emitter = read_data.server_bus.emitter();
        // Attacks
        for (attacker, uid, pos, ori, melee_attack, body) in (
            &read_data.entities,
            &read_data.uids,
            &read_data.positions,
            &read_data.orientations,
            &mut melee_attacks,
            &read_data.bodies,
        )
            .join()
        {
            if melee_attack.applied {
                continue;
            }
            melee_attack.applied = true;

            // Go through all other entities
            for (target, pos_b, health_b, body_b) in (
                &read_data.entities,
                &read_data.positions,
                &read_data.healths,
                &read_data.bodies,
            )
                .join()
            {
                let look_dir = *ori.look_dir();

                // 2D versions
                let pos2 = Vec2::from(pos.0);
                let pos_b2 = Vec2::<f32>::from(pos_b.0);
                let ori2 = Vec2::from(look_dir);

                // Scales
                let scale = read_data.scales.get(attacker).map_or(1.0, |s| s.0);
                let scale_b = read_data.scales.get(target).map_or(1.0, |s| s.0);
                let rad = body.radius() * scale;
                let rad_b = body_b.radius() * scale_b;

                // Check if entity is dodging
                let is_dodge = read_data
                    .char_states
                    .get(target)
                    .map_or(false, |c_s| c_s.is_melee_dodge());

                // Check if it is a hit
                if attacker != target
                    && !health_b.is_dead
                    // Spherical wedge shaped attack field
                    && pos.0.distance_squared(pos_b.0) < (rad + rad_b + scale * melee_attack.range).powi(2)
                    && ori2.angle_between(pos_b2 - pos2) < melee_attack.max_angle + (rad_b / pos2.distance(pos_b2)).atan()
                {
                    // See if entities are in the same group
                    let same_group = read_data
                        .groups
                        .get(attacker)
                        .map(|group_a| Some(group_a) == read_data.groups.get(target))
                        .unwrap_or(false);

                    let target_group = if same_group {
                        GroupTarget::InGroup
                    } else {
                        GroupTarget::OutOfGroup
                    };

                    let dir = Dir::new((pos_b.0 - pos.0).try_normalized().unwrap_or(look_dir));

                    let attacker_info = Some(AttackerInfo {
                        entity: attacker,
                        uid: *uid,
                        energy: read_data.energies.get(attacker),
                        combo: read_data.combos.get(attacker),
                    });

                    let target_info = TargetInfo {
                        entity: target,
                        inventory: read_data.inventories.get(target),
                        stats: read_data.stats.get(target),
                        health: read_data.healths.get(target),
                    };

                    melee_attack.attack.apply_attack(
                        target_group,
                        attacker_info,
                        target_info,
                        dir,
                        is_dodge,
                        1.0,
                        |e| server_emitter.emit(e),
                    );

                    melee_attack.hit_count += 1;
                }
            }
        }
    }
}
