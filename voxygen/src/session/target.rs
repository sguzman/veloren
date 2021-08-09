use specs::{Join, WorldExt};
use vek::*;

use client::{self, Client};
use common::{
    comp,
    consts::MAX_PICKUP_RANGE,
    terrain::{Block, TerrainChunk},
    util::find_dist::{Cylinder, FindDist},
    vol::ReadVol,
    volumes::vol_grid_2d::{VolGrid2dError},
};
use common_base::span;

#[derive(Clone, Copy, Debug)]
pub enum Target {
    Build(Vec3<f32>, Vec3<f32>, f32), // (solid_pos, build_pos, dist)
    Collectable(Vec3<f32>, f32), // (pos, dist)
    Entity(specs::Entity, Vec3<f32>, f32), // (e, pos, dist) 
    Mine(Vec3<f32>, f32), // (pos, dist)
}

impl Target {
    pub fn entity(self) -> Option<specs::Entity> {
        match self {
            Self::Entity(e, _, _) => Some(e),
            _ => None,
        }
    }

    pub fn distance(self) -> f32 {
        match self {
            Self::Collectable(_, d)
            | Self::Entity(_, _, d)
            | Self::Mine(_, d)
            | Self::Build(_, _, d) => d,
        }
    }

    pub fn position(self) -> Vec3<f32> {
        match self {
            Self::Collectable(sp, _)
            | Self::Entity(_, sp, _)
            | Self::Mine(sp, _)
            | Self::Build(sp, _, _) => sp,
        }
    }

    pub fn position_int(self) -> Vec3<i32> {
        self.position().map(|p| p.floor() as i32)
    }
}

/// Max distance an entity can be "targeted"
const MAX_TARGET_RANGE: f32 = 300.0;
/// Calculate what the cursor is pointing at within the 3d scene
#[allow(clippy::type_complexity)]
pub(super) fn targets_under_cursor(
    client: &Client,
    cam_pos: Vec3<f32>,
    cam_dir: Vec3<f32>,
    can_build: bool,
    is_mining: bool,
) -> (
    Option<Target>,
    Option<Target>,
    Option<Target>,
    Option<Target>,
    f32,
) {
    span!(_guard, "under_cursor");
    // Choose a spot above the player's head for item distance checks
    let player_entity = client.entity();
    let ecs = client.state().ecs();
    let positions = ecs.read_storage::<comp::Pos>();
    let player_pos = match positions.get(player_entity) {
        Some(pos) => pos.0,
        None => cam_pos, // Should never happen, but a safe fallback
    };
    let scales = ecs.read_storage();
    let colliders = ecs.read_storage();
    let char_states = ecs.read_storage();
    // Get the player's cylinder
    let player_cylinder = Cylinder::from_components(
        player_pos,
        scales.get(player_entity).copied(),
        colliders.get(player_entity),
        char_states.get(player_entity),
    );

    fn curry_find_pos <'a> (
        client: &'a Client, cam_pos: &'a Vec3<f32>, cam_dir: &'a Vec3<f32>, player_cylinder: &'a Cylinder
    ) -> impl FnMut(fn(Block)->bool) -> (Option<Vec3<f32>>, Option<Vec3<f32>>, (f32, Result<Option<Block>, VolGrid2dError<TerrainChunk>>)) + 'a {
        let terrain = client.state().terrain();

        move |hit: fn(Block)->bool| {
            let cam_ray = terrain
                .ray(*cam_pos, *cam_pos + *cam_dir * 100.0)
                .until(|block| hit(*block))
                .cast();
            let cam_ray = (cam_ray.0, cam_ray.1.map(|x| x.copied()));
            let cam_dist = cam_ray.0;
    
            if matches!(
                cam_ray.1,
                Ok(Some(_)) if player_cylinder.min_distance(*cam_pos + *cam_dir * (cam_dist + 0.01)) <= MAX_PICKUP_RANGE
            ) {
                (
                    Some(*cam_pos + *cam_dir * cam_dist),
                    Some(*cam_pos + *cam_dir * (cam_dist - 0.01)),
                    cam_ray
                )
            } else { (None, None, cam_ray) }
        }
    }

    let mut find_pos = curry_find_pos(&client, &cam_pos, &cam_dir, &player_cylinder);

    let (collect_pos, _, cam_ray_0) = find_pos(|b: Block| { b.is_collectible() });
    let (mine_pos, _, cam_ray_1) = find_pos(|b: Block| { b.mine_tool().is_some() });
    // FIXME: the `solid_pos` is used in the remove_block(). is this correct?
    let (solid_pos, build_pos, cam_ray_2) = find_pos(|b: Block| { b.is_solid() });

    // collectables can be in the Air. so using solely solid_pos is not correct.
    // so, use a minimum distance of all 3
    let mut cam_rays = vec![&cam_ray_0, &cam_ray_2];
    if is_mining { cam_rays.push(&cam_ray_1); }
    let cam_dist = cam_rays.iter().filter_map(|x| match **x {
        (d, Ok(Some(_))) => Some(d),
        _ => None,
    }).min_by(|d1, d2| d1.partial_cmp(d2).unwrap())
    .unwrap_or(MAX_PICKUP_RANGE);

    // See if ray hits entities
    // Currently treated as spheres
    // Don't cast through blocks
    // Could check for intersection with entity from last frame to narrow this down
    let cast_dist = cam_dist.min(MAX_TARGET_RANGE);

    // Need to raycast by distance to cam
    // But also filter out by distance to the player (but this only needs to be done
    // on final result)
    let mut nearby = (
        &ecs.entities(),
        &positions,
        scales.maybe(),
        &ecs.read_storage::<comp::Body>(),
        ecs.read_storage::<comp::Item>().maybe(),
    )
        .join()
        .filter(|(e, _, _, _, _)| *e != player_entity)
        .filter_map(|(e, p, s, b, i)| {
            const RADIUS_SCALE: f32 = 3.0;
            // TODO: use collider radius instead of body radius?
            let radius = s.map_or(1.0, |s| s.0) * b.max_radius() * RADIUS_SCALE;
            // Move position up from the feet
            let pos = Vec3::new(p.0.x, p.0.y, p.0.z + radius);
            // Distance squared from camera to the entity
            let dist_sqr = pos.distance_squared(cam_pos);
            // We only care about interacting with entities that contain items,
            // or are not inanimate (to trade with)
            if i.is_some() || !matches!(b, comp::Body::Object(_)) {
                Some((e, pos, radius, dist_sqr))
            } else {
                None
            }
        })
        // Roughly filter out entities farther than ray distance
        .filter(|(_, _, r, d_sqr)| *d_sqr <= cast_dist.powi(2) + 2.0 * cast_dist * r + r.powi(2))
        // Ignore entities intersecting the camera
        .filter(|(_, _, r, d_sqr)| *d_sqr > r.powi(2))
        // Substract sphere radius from distance to the camera
        .map(|(e, p, r, d_sqr)| (e, p, r, d_sqr.sqrt() - r))
        .collect::<Vec<_>>();
    // Sort by distance
    nearby.sort_unstable_by(|a, b| a.3.partial_cmp(&b.3).unwrap());

    let seg_ray = LineSegment3 {
        start: cam_pos,
        end: cam_pos + cam_dir * cam_dist,
    };
    // TODO: fuzzy borders
    let entity_target = nearby
        .iter()
        .map(|(e, p, r, _)| (e, *p, r))
        // Find first one that intersects the ray segment
        .find(|(_, p, r)| seg_ray.projected_point(*p).distance_squared(*p) < r.powi(2))
        .and_then(|(e, p, _)| {
            // Get the entity's cylinder
            let target_cylinder = Cylinder::from_components(
                p,
                scales.get(*e).copied(),
                colliders.get(*e),
                char_states.get(*e),
            );

            let dist_to_player = player_cylinder.min_distance(target_cylinder);
            (dist_to_player < MAX_TARGET_RANGE).then_some(Target::Entity(*e, p, dist_to_player))
        });

    let build_target = if can_build {
        solid_pos.map(|p| Target::Build(p, build_pos.unwrap(), cam_ray_2.0))
    } else { None };

    let mine_target = if is_mining {
        mine_pos.map(|p| Target::Mine(p, cam_ray_1.0))
    } else { None };

    let shortest_distance = cam_dist;

    // Return multiple possible targets
    // GameInput events determine which target to use.
    (
        build_target,
        collect_pos.map(|p| Target::Collectable(p, cam_ray_0.0)),
        entity_target,
        mine_target,
        shortest_distance
    )
}
