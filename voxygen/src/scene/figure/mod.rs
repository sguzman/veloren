mod cache;
pub mod load;

pub use cache::FigureModelCache;
pub use load::load_mesh; // TODO: Don't make this public.

use crate::{
    ecs::comp::Interpolated,
    render::{
        pipelines, ColLightInfo, Consts, FigureBoneData, FigureLocals, FigureModel, GlobalModel,
        Mesh, RenderError, Renderer, TerrainVertex, Texture,
    },
    scene::{
        camera::{Camera, CameraMode, Dependents},
        math,
        terrain::Terrain,
        LodData, SceneData,
    },
};
use anim::{
    biped_large::BipedLargeSkeleton, biped_small::BipedSmallSkeleton,
    bird_large::BirdLargeSkeleton, bird_medium::BirdMediumSkeleton, character::CharacterSkeleton,
    dragon::DragonSkeleton, fish_medium::FishMediumSkeleton, fish_small::FishSmallSkeleton,
    golem::GolemSkeleton, object::ObjectSkeleton, quadruped_low::QuadrupedLowSkeleton,
    quadruped_medium::QuadrupedMediumSkeleton, quadruped_small::QuadrupedSmallSkeleton,
    ship::ShipSkeleton, theropod::TheropodSkeleton, Animation, Skeleton,
};
use common::{
    comp::{
        inventory::slot::EquipSlot,
        item::{Hands, ItemKind, ToolKind},
        Body, CharacterState, Controller, Health, Inventory, Item, Last, LightAnimation,
        LightEmitter, Ori, PhysicsState, PoiseState, Pos, Scale, Vel,
    },
    resources::DeltaTime,
    states::utils::StageSection,
    terrain::TerrainChunk,
    util::Dir,
    vol::RectRasterableVol,
};
use common_base::span;
use common_state::State;
use core::{
    borrow::Borrow,
    convert::TryFrom,
    hash::Hash,
    ops::{Deref, DerefMut, Range},
};
use guillotiere::AtlasAllocator;
use hashbrown::HashMap;
use specs::{Entity as EcsEntity, Join, LazyUpdate, WorldExt};
use treeculler::{BVol, BoundingSphere};
use vek::*;

const DAMAGE_FADE_COEFFICIENT: f64 = 15.0;
const MOVING_THRESHOLD: f32 = 0.2;
const MOVING_THRESHOLD_SQR: f32 = MOVING_THRESHOLD * MOVING_THRESHOLD;

/// camera data, figure LOD render distance.
pub type CameraData<'a> = (&'a Camera, f32);

/// Enough data to render a figure model.
pub type FigureModelRef<'a> = (
    &'a Consts<FigureLocals>,
    &'a Consts<FigureBoneData>,
    &'a FigureModel,
    &'a Texture, /* <ColLightFmt> */
);

/// An entry holding enough information to draw or destroy a figure in a
/// particular cache.
pub struct FigureModelEntry<const N: usize> {
    /// The estimated bounds of this figure, in voxels.  This may not be very
    /// useful yet.
    _bounds: math::Aabb<f32>,
    /// Hypothetical texture atlas allocation data for the current figure.
    /// Will be useful if we decide to use a packed texture atlas for figures
    /// like we do for terrain.
    allocation: guillotiere::Allocation,
    /// Texture used to store color/light information for this figure entry.
    /* TODO: Consider using mipmaps instead of storing multiple texture atlases for different
     * LOD levels. */
    col_lights: Texture, /* <ColLightFmt> */
    /// Models stored in this figure entry; there may be several for one figure,
    /// because of LOD models.
    pub models: [FigureModel; N],
}

struct FigureMgrStates {
    character_states: HashMap<EcsEntity, FigureState<CharacterSkeleton>>,
    quadruped_small_states: HashMap<EcsEntity, FigureState<QuadrupedSmallSkeleton>>,
    quadruped_medium_states: HashMap<EcsEntity, FigureState<QuadrupedMediumSkeleton>>,
    quadruped_low_states: HashMap<EcsEntity, FigureState<QuadrupedLowSkeleton>>,
    bird_medium_states: HashMap<EcsEntity, FigureState<BirdMediumSkeleton>>,
    fish_medium_states: HashMap<EcsEntity, FigureState<FishMediumSkeleton>>,
    theropod_states: HashMap<EcsEntity, FigureState<TheropodSkeleton>>,
    dragon_states: HashMap<EcsEntity, FigureState<DragonSkeleton>>,
    bird_large_states: HashMap<EcsEntity, FigureState<BirdLargeSkeleton>>,
    fish_small_states: HashMap<EcsEntity, FigureState<FishSmallSkeleton>>,
    biped_large_states: HashMap<EcsEntity, FigureState<BipedLargeSkeleton>>,
    biped_small_states: HashMap<EcsEntity, FigureState<BipedSmallSkeleton>>,
    golem_states: HashMap<EcsEntity, FigureState<GolemSkeleton>>,
    object_states: HashMap<EcsEntity, FigureState<ObjectSkeleton>>,
    ship_states: HashMap<EcsEntity, FigureState<ShipSkeleton>>,
}

impl FigureMgrStates {
    pub fn default() -> Self {
        Self {
            character_states: HashMap::new(),
            quadruped_small_states: HashMap::new(),
            quadruped_medium_states: HashMap::new(),
            quadruped_low_states: HashMap::new(),
            bird_medium_states: HashMap::new(),
            fish_medium_states: HashMap::new(),
            theropod_states: HashMap::new(),
            dragon_states: HashMap::new(),
            bird_large_states: HashMap::new(),
            fish_small_states: HashMap::new(),
            biped_large_states: HashMap::new(),
            biped_small_states: HashMap::new(),
            golem_states: HashMap::new(),
            object_states: HashMap::new(),
            ship_states: HashMap::new(),
        }
    }

    fn get_mut<'a, Q: ?Sized>(
        &'a mut self,
        body: &Body,
        entity: &Q,
    ) -> Option<&'a mut FigureStateMeta>
    where
        EcsEntity: Borrow<Q>,
        Q: Hash + Eq,
    {
        match body {
            Body::Humanoid(_) => self
                .character_states
                .get_mut(&entity)
                .map(DerefMut::deref_mut),
            Body::QuadrupedSmall(_) => self
                .quadruped_small_states
                .get_mut(&entity)
                .map(DerefMut::deref_mut),
            Body::QuadrupedMedium(_) => self
                .quadruped_medium_states
                .get_mut(&entity)
                .map(DerefMut::deref_mut),
            Body::QuadrupedLow(_) => self
                .quadruped_low_states
                .get_mut(&entity)
                .map(DerefMut::deref_mut),
            Body::BirdMedium(_) => self
                .bird_medium_states
                .get_mut(&entity)
                .map(DerefMut::deref_mut),
            Body::FishMedium(_) => self
                .fish_medium_states
                .get_mut(&entity)
                .map(DerefMut::deref_mut),
            Body::Theropod(_) => self
                .theropod_states
                .get_mut(&entity)
                .map(DerefMut::deref_mut),
            Body::Dragon(_) => self.dragon_states.get_mut(&entity).map(DerefMut::deref_mut),
            Body::BirdLarge(_) => self
                .bird_large_states
                .get_mut(&entity)
                .map(DerefMut::deref_mut),
            Body::FishSmall(_) => self
                .fish_small_states
                .get_mut(&entity)
                .map(DerefMut::deref_mut),
            Body::BipedLarge(_) => self
                .biped_large_states
                .get_mut(&entity)
                .map(DerefMut::deref_mut),
            Body::BipedSmall(_) => self
                .biped_small_states
                .get_mut(&entity)
                .map(DerefMut::deref_mut),
            Body::Golem(_) => self.golem_states.get_mut(&entity).map(DerefMut::deref_mut),
            Body::Object(_) => self.object_states.get_mut(&entity).map(DerefMut::deref_mut),
            Body::Ship(_) => self.ship_states.get_mut(&entity).map(DerefMut::deref_mut),
        }
    }

    fn remove<'a, Q: ?Sized>(&'a mut self, body: &Body, entity: &Q) -> Option<FigureStateMeta>
    where
        EcsEntity: Borrow<Q>,
        Q: Hash + Eq,
    {
        match body {
            Body::Humanoid(_) => self.character_states.remove(&entity).map(|e| e.meta),
            Body::QuadrupedSmall(_) => self.quadruped_small_states.remove(&entity).map(|e| e.meta),
            Body::QuadrupedMedium(_) => {
                self.quadruped_medium_states.remove(&entity).map(|e| e.meta)
            },
            Body::QuadrupedLow(_) => self.quadruped_low_states.remove(&entity).map(|e| e.meta),
            Body::BirdMedium(_) => self.bird_medium_states.remove(&entity).map(|e| e.meta),
            Body::FishMedium(_) => self.fish_medium_states.remove(&entity).map(|e| e.meta),
            Body::Theropod(_) => self.theropod_states.remove(&entity).map(|e| e.meta),
            Body::Dragon(_) => self.dragon_states.remove(&entity).map(|e| e.meta),
            Body::BirdLarge(_) => self.bird_large_states.remove(&entity).map(|e| e.meta),
            Body::FishSmall(_) => self.fish_small_states.remove(&entity).map(|e| e.meta),
            Body::BipedLarge(_) => self.biped_large_states.remove(&entity).map(|e| e.meta),
            Body::BipedSmall(_) => self.biped_small_states.remove(&entity).map(|e| e.meta),
            Body::Golem(_) => self.golem_states.remove(&entity).map(|e| e.meta),
            Body::Object(_) => self.object_states.remove(&entity).map(|e| e.meta),
            Body::Ship(_) => self.ship_states.remove(&entity).map(|e| e.meta),
        }
    }

    fn retain(&mut self, mut f: impl FnMut(&EcsEntity, &mut FigureStateMeta) -> bool) {
        span!(_guard, "retain", "FigureManagerStates::retain");
        self.character_states.retain(|k, v| f(k, &mut *v));
        self.quadruped_small_states.retain(|k, v| f(k, &mut *v));
        self.quadruped_medium_states.retain(|k, v| f(k, &mut *v));
        self.quadruped_low_states.retain(|k, v| f(k, &mut *v));
        self.bird_medium_states.retain(|k, v| f(k, &mut *v));
        self.fish_medium_states.retain(|k, v| f(k, &mut *v));
        self.theropod_states.retain(|k, v| f(k, &mut *v));
        self.dragon_states.retain(|k, v| f(k, &mut *v));
        self.bird_large_states.retain(|k, v| f(k, &mut *v));
        self.fish_small_states.retain(|k, v| f(k, &mut *v));
        self.biped_large_states.retain(|k, v| f(k, &mut *v));
        self.biped_small_states.retain(|k, v| f(k, &mut *v));
        self.golem_states.retain(|k, v| f(k, &mut *v));
        self.object_states.retain(|k, v| f(k, &mut *v));
        self.ship_states.retain(|k, v| f(k, &mut *v));
    }

    fn count(&self) -> usize {
        self.character_states.len()
            + self.quadruped_small_states.len()
            + self.character_states.len()
            + self.quadruped_medium_states.len()
            + self.quadruped_low_states.len()
            + self.bird_medium_states.len()
            + self.fish_medium_states.len()
            + self.theropod_states.len()
            + self.dragon_states.len()
            + self.bird_large_states.len()
            + self.fish_small_states.len()
            + self.biped_large_states.len()
            + self.biped_small_states.len()
            + self.golem_states.len()
            + self.object_states.len()
            + self.ship_states.len()
    }

    fn count_visible(&self) -> usize {
        self.character_states
            .iter()
            .filter(|(_, c)| c.visible())
            .count()
            + self
                .quadruped_small_states
                .iter()
                .filter(|(_, c)| c.visible())
                .count()
            + self
                .quadruped_medium_states
                .iter()
                .filter(|(_, c)| c.visible())
                .count()
            + self
                .quadruped_low_states
                .iter()
                .filter(|(_, c)| c.visible())
                .count()
            + self
                .bird_medium_states
                .iter()
                .filter(|(_, c)| c.visible())
                .count()
            + self
                .theropod_states
                .iter()
                .filter(|(_, c)| c.visible())
                .count()
            + self
                .dragon_states
                .iter()
                .filter(|(_, c)| c.visible())
                .count()
            + self
                .fish_medium_states
                .iter()
                .filter(|(_, c)| c.visible())
                .count()
            + self
                .bird_large_states
                .iter()
                .filter(|(_, c)| c.visible())
                .count()
            + self
                .fish_small_states
                .iter()
                .filter(|(_, c)| c.visible())
                .count()
            + self
                .biped_large_states
                .iter()
                .filter(|(_, c)| c.visible())
                .count()
            + self
                .biped_small_states
                .iter()
                .filter(|(_, c)| c.visible())
                .count()
            + self
                .golem_states
                .iter()
                .filter(|(_, c)| c.visible())
                .count()
            + self
                .object_states
                .iter()
                .filter(|(_, c)| c.visible())
                .count()
            + self.ship_states.iter().filter(|(_, c)| c.visible()).count()
    }
}

pub struct FigureMgr {
    col_lights: FigureColLights,
    model_cache: FigureModelCache,
    theropod_model_cache: FigureModelCache<TheropodSkeleton>,
    quadruped_small_model_cache: FigureModelCache<QuadrupedSmallSkeleton>,
    quadruped_medium_model_cache: FigureModelCache<QuadrupedMediumSkeleton>,
    quadruped_low_model_cache: FigureModelCache<QuadrupedLowSkeleton>,
    bird_medium_model_cache: FigureModelCache<BirdMediumSkeleton>,
    bird_large_model_cache: FigureModelCache<BirdLargeSkeleton>,
    dragon_model_cache: FigureModelCache<DragonSkeleton>,
    fish_medium_model_cache: FigureModelCache<FishMediumSkeleton>,
    fish_small_model_cache: FigureModelCache<FishSmallSkeleton>,
    biped_large_model_cache: FigureModelCache<BipedLargeSkeleton>,
    biped_small_model_cache: FigureModelCache<BipedSmallSkeleton>,
    object_model_cache: FigureModelCache<ObjectSkeleton>,
    ship_model_cache: FigureModelCache<ShipSkeleton>,
    golem_model_cache: FigureModelCache<GolemSkeleton>,
    states: FigureMgrStates,
}

impl FigureMgr {
    pub fn new(renderer: &mut Renderer) -> Self {
        Self {
            col_lights: FigureColLights::new(renderer),
            model_cache: FigureModelCache::new(),
            theropod_model_cache: FigureModelCache::new(),
            quadruped_small_model_cache: FigureModelCache::new(),
            quadruped_medium_model_cache: FigureModelCache::new(),
            quadruped_low_model_cache: FigureModelCache::new(),
            bird_medium_model_cache: FigureModelCache::new(),
            bird_large_model_cache: FigureModelCache::new(),
            dragon_model_cache: FigureModelCache::new(),
            fish_medium_model_cache: FigureModelCache::new(),
            fish_small_model_cache: FigureModelCache::new(),
            biped_large_model_cache: FigureModelCache::new(),
            biped_small_model_cache: FigureModelCache::new(),
            object_model_cache: FigureModelCache::new(),
            ship_model_cache: FigureModelCache::new(),
            golem_model_cache: FigureModelCache::new(),
            states: FigureMgrStates::default(),
        }
    }

    pub fn col_lights(&self) -> &FigureColLights { &self.col_lights }

    pub fn clean(&mut self, tick: u64) {
        span!(_guard, "clean", "FigureManager::clean");
        self.model_cache.clean(&mut self.col_lights, tick);
        self.theropod_model_cache.clean(&mut self.col_lights, tick);
        self.quadruped_small_model_cache
            .clean(&mut self.col_lights, tick);
        self.quadruped_medium_model_cache
            .clean(&mut self.col_lights, tick);
        self.quadruped_low_model_cache
            .clean(&mut self.col_lights, tick);
        self.bird_medium_model_cache
            .clean(&mut self.col_lights, tick);
        self.bird_large_model_cache
            .clean(&mut self.col_lights, tick);
        self.dragon_model_cache.clean(&mut self.col_lights, tick);
        self.fish_medium_model_cache
            .clean(&mut self.col_lights, tick);
        self.fish_small_model_cache
            .clean(&mut self.col_lights, tick);
        self.biped_large_model_cache
            .clean(&mut self.col_lights, tick);
        self.biped_small_model_cache
            .clean(&mut self.col_lights, tick);
        self.object_model_cache.clean(&mut self.col_lights, tick);
        self.ship_model_cache.clean(&mut self.col_lights, tick);
        self.golem_model_cache.clean(&mut self.col_lights, tick);
    }

    #[allow(clippy::redundant_pattern_matching)]
    // TODO: Pending review in #587
    pub fn update_lighting(&mut self, scene_data: &SceneData) {
        span!(_guard, "update_lighting", "FigureManager::update_lighting");
        let ecs = scene_data.state.ecs();
        for (entity, body, light_emitter) in (
            &ecs.entities(),
            ecs.read_storage::<common::comp::Body>().maybe(),
            &ecs.read_storage::<LightEmitter>(),
        )
            .join()
        {
            // Add LightAnimation for objects with a LightEmitter
            let mut anim_storage = ecs.write_storage::<LightAnimation>();
            if let None = anim_storage.get_mut(entity) {
                let anim = LightAnimation {
                    offset: body
                        .map(|b| b.default_light_offset())
                        .unwrap_or_else(Vec3::zero),
                    col: light_emitter.col,
                    strength: 0.0,
                };
                let _ = anim_storage.insert(entity, anim);
            }
        }
        let dt = ecs.fetch::<DeltaTime>().0;
        let updater = ecs.read_resource::<LazyUpdate>();
        for (entity, light_emitter_opt, body, mut light_anim) in (
            &ecs.entities(),
            ecs.read_storage::<LightEmitter>().maybe(),
            ecs.read_storage::<Body>().maybe(),
            &mut ecs.write_storage::<LightAnimation>(),
        )
            .join()
        {
            let (target_col, target_strength, flicker, animated) =
                if let Some(emitter) = light_emitter_opt {
                    (
                        emitter.col,
                        if emitter.strength.is_normal() {
                            emitter.strength
                        } else {
                            0.0
                        },
                        emitter.flicker,
                        emitter.animated,
                    )
                } else {
                    (vek::Rgb::zero(), 0.0, 0.0, true)
                };
            if let Some(state) = body.and_then(|body| self.states.get_mut(body, &entity)) {
                light_anim.offset = vek::Vec3::from(state.lantern_offset);
            }
            if !light_anim.strength.is_normal() {
                light_anim.strength = 0.0;
            }
            if animated {
                let flicker = (rand::random::<f32>() - 0.5) * flicker / dt.sqrt();
                // Close gap between current and target strength by 95% per second
                let delta = 0.05_f32.powf(dt);
                light_anim.strength =
                    light_anim.strength * delta + (target_strength + flicker) * (1.0 - delta);
                light_anim.col = light_anim.col * delta + target_col * (1.0 - delta)
            } else {
                light_anim.strength = target_strength;
                light_anim.col = target_col;
            }
            // NOTE: We add `LIGHT_EPSILON` because if we wait for numbers to become
            // equal to target (or even within a subnormal), it will take a minimum
            // of 30 seconds for a light to fully turn off (for initial
            // strength ≥ 1), which prevents optimizations (particularly those that
            // can kick in with zero lights).
            const LIGHT_EPSILON: f32 = 0.0001;
            if (light_anim.strength - target_strength).abs() < LIGHT_EPSILON {
                light_anim.strength = target_strength;
                if light_anim.strength == 0.0 {
                    updater.remove::<LightAnimation>(entity);
                }
            }
        }
    }

    #[allow(clippy::or_fun_call)]
    // TODO: Pending review in #587
    pub fn maintain(
        &mut self,
        renderer: &mut Renderer,
        scene_data: &SceneData,
        // Visible chunk data.
        visible_psr_bounds: math::Aabr<f32>,
        camera: &Camera,
        terrain: Option<&Terrain>,
    ) -> anim::vek::Aabb<f32> {
        span!(_guard, "maintain", "FigureManager::maintain");
        let state = scene_data.state;
        let time = state.get_time() as f32;
        let tick = scene_data.tick;
        let ecs = state.ecs();
        let view_distance = scene_data.view_distance;
        let dt = state.get_delta_time();
        let dt_lerp = (15.0 * dt).min(1.0);
        let frustum = camera.frustum();

        // Sun shadows--find the bounding box of the shadow map plane (i.e. the bounds
        // of the image rendered from the light).  If the position projected
        // with the ray_mat matrix is valid, and shadows are otherwise enabled,
        // we mark can_shadow.
        let can_shadow_sun = {
            let ray_direction = scene_data.get_sun_dir();
            let is_daylight = ray_direction.z < 0.0/*0.6*/;
            // Are shadows enabled at all?
            let can_shadow_sun = renderer.render_mode().shadow.is_map() && is_daylight;
            let Dependents {
                proj_mat: _,
                view_mat: _,
                cam_pos,
                ..
            } = camera.dependents();
            let cam_pos = math::Vec3::from(cam_pos);
            let ray_direction = math::Vec3::from(ray_direction);

            // Transform (semi) world space to light space.
            let ray_mat: math::Mat4<f32> =
                math::Mat4::look_at_rh(cam_pos, cam_pos + ray_direction, math::Vec3::unit_y());
            let focus_off = math::Vec3::from(camera.get_focus_pos().map(f32::trunc));
            let ray_mat = ray_mat * math::Mat4::translation_3d(-focus_off);

            let collides_with_aabr = |a: math::Aabr<f32>, b: math::Aabr<f32>| {
                let min = math::Vec4::new(a.min.x, a.min.y, b.min.x, b.min.y);
                let max = math::Vec4::new(b.max.x, b.max.y, a.max.x, a.max.y);
                #[cfg(feature = "simd")]
                return min.partial_cmple_simd(max).reduce_and();
                #[cfg(not(feature = "simd"))]
                return min.partial_cmple(&max).reduce_and();
            };
            move |pos: (anim::vek::Vec3<f32>,), radius: f32| {
                // Short circuit when there are no shadows to cast.
                if !can_shadow_sun {
                    return false;
                }
                // First project center onto shadow map.
                let center = (ray_mat * math::Vec4::new(pos.0.x, pos.0.y, pos.0.z, 1.0)).xy();
                // Then, create an approximate bounding box (± radius).
                let figure_box = math::Aabr {
                    min: center - radius,
                    max: center + radius,
                };
                // Quick intersection test for membership in the PSC (potential shader caster)
                // list.
                collides_with_aabr(figure_box, visible_psr_bounds)
            }
        };

        // Get player position.
        let player_pos = ecs
            .read_storage::<Pos>()
            .get(scene_data.player_entity)
            .map_or(anim::vek::Vec3::zero(), |pos| anim::vek::Vec3::from(pos.0));
        let visible_aabb = anim::vek::Aabb {
            min: player_pos - 2.0,
            max: player_pos + 2.0,
        };
        let camera_mode = camera.get_mode();
        let character_state_storage = state.read_storage::<common::comp::CharacterState>();
        let slow_jobs = state.slow_job_pool();
        let character_state = character_state_storage.get(scene_data.player_entity);

        let focus_pos = anim::vek::Vec3::<f32>::from(camera.get_focus_pos());

        let mut update_buf = [Default::default(); anim::MAX_BONE_COUNT];

        for (
            i,
            (
                entity,
                pos,
                controller,
                interpolated,
                vel,
                scale,
                body,
                character,
                last_character,
                physics,
                health,
                inventory,
                item,
                light_emitter,
            ),
        ) in (
            &ecs.entities(),
            &ecs.read_storage::<Pos>(),
            ecs.read_storage::<Controller>().maybe(),
            ecs.read_storage::<Interpolated>().maybe(),
            &ecs.read_storage::<Vel>(),
            ecs.read_storage::<Scale>().maybe(),
            &ecs.read_storage::<Body>(),
            ecs.read_storage::<CharacterState>().maybe(),
            ecs.read_storage::<Last<CharacterState>>().maybe(),
            &ecs.read_storage::<PhysicsState>(),
            ecs.read_storage::<Health>().maybe(),
            ecs.read_storage::<Inventory>().maybe(),
            ecs.read_storage::<Item>().maybe(),
            ecs.read_storage::<LightEmitter>().maybe(),
        )
            .join()
            .enumerate()
        {
            // Velocity relative to the current ground
            let rel_vel = anim::vek::Vec3::<f32>::from(vel.0 - physics.ground_vel);

            let look_dir = controller
                .map(|c| c.inputs.look_dir)
                .unwrap_or(Dir::default());
            let is_player = scene_data.player_entity == entity;
            let player_camera_mode = if is_player {
                camera_mode
            } else {
                CameraMode::default()
            };
            let player_character_state = if is_player { character_state } else { None };

            let (pos, ori) = interpolated
                .map(|i| {
                    (
                        (anim::vek::Vec3::from(i.pos),),
                        anim::vek::Quaternion::<f32>::from(i.ori),
                    )
                })
                .unwrap_or((
                    (anim::vek::Vec3::<f32>::from(pos.0),),
                    anim::vek::Quaternion::<f32>::default(),
                ));

            // Maintaining figure data and sending new figure data to the GPU turns out to
            // be a very expensive operation. We want to avoid doing it as much
            // as possible, so we make the assumption that players don't care so
            // much about the update *rate* for far away things. As the entity
            // goes further and further away, we start to 'skip' update ticks.
            // TODO: Investigate passing the velocity into the shader so we can at least
            // interpolate motion
            const MIN_PERFECT_RATE_DIST: f32 = 100.0;

            if (i as u64 + tick)
                % (1 + ((pos.0.distance_squared(focus_pos).powf(0.25)
                    - MIN_PERFECT_RATE_DIST.sqrt())
                .max(0.0)
                    / 3.0) as u64)
                != 0
            {
                continue;
            }

            // Check whether we could have been shadowing last frame.
            let mut state = self.states.get_mut(body, &entity);
            let can_shadow_prev = state
                .as_mut()
                .map(|state| state.can_shadow_sun())
                .unwrap_or(false);

            // Don't process figures outside the vd
            let vd_frac = anim::vek::Vec2::from(pos.0 - player_pos)
                .map2(
                    anim::vek::Vec2::<u32>::from(TerrainChunk::RECT_SIZE),
                    |d: f32, sz| d.abs() as f32 / sz as f32,
                )
                .magnitude()
                / view_distance as f32;

            // Keep from re-adding/removing entities on the border of the vd
            if vd_frac > 1.2 {
                self.states.remove(body, &entity);
                continue;
            } else if vd_frac > 1.0 {
                state.as_mut().map(|state| state.visible = false);
                // Keep processing if this might be a shadow caster.
                if !can_shadow_prev {
                    continue;
                }
            }

            // Don't display figures outside the frustum spectrum (this is important to do
            // for any figure that potentially casts a shadow, since we use this
            // to estimate bounds for shadow maps).  Currently, we don't do this before the
            // update cull, so it's possible that faraway figures will not
            // shadow correctly until their next update.  For now, we treat this
            // as an acceptable tradeoff.
            let radius = scale.unwrap_or(&Scale(1.0)).0 * 2.0;
            let (in_frustum, lpindex) = if let Some(mut meta) = state {
                let (in_frustum, lpindex) = BoundingSphere::new(pos.0.into_array(), radius)
                    .coherent_test_against_frustum(frustum, meta.lpindex);
                let in_frustum = in_frustum || matches!(body, Body::Ship(_));
                meta.visible = in_frustum;
                meta.lpindex = lpindex;
                if in_frustum {
                    /* // Update visible bounds.
                    visible_aabb.expand_to_contain(Aabb {
                        min: pos.0 - radius,
                        max: pos.0 + radius,
                    }); */
                } else {
                    // Check whether we can shadow.
                    meta.can_shadow_sun = can_shadow_sun(pos, radius);
                }
                (in_frustum, lpindex)
            } else {
                (true, 0)
            };

            // Change in health as color!
            let col = health
                .map(|h| {
                    vek::Rgba::broadcast(1.0)
                        + vek::Rgba::new(10.0, 10.0, 10.0, 0.0).map(|c| {
                            (c / (1.0 + DAMAGE_FADE_COEFFICIENT * h.last_change.0)) as f32
                        })
                })
                .unwrap_or(vek::Rgba::broadcast(1.0))
            // Highlight targeted collectible entities
            * if item.is_some() && scene_data.target_entity.map_or(false, |e| e == entity) {
                vek::Rgba::new(5.0, 5.0, 5.0, 1.0)
            } else {
                vek::Rgba::one()
            };

            let scale = scale.map(|s| s.0).unwrap_or(1.0);

            let mut state_animation_rate = 1.0;

            let tool_info = |equip_slot| {
                inventory
                    .and_then(|i| i.equipped(equip_slot))
                    .map(|i| {
                        if let ItemKind::Tool(tool) = i.kind() {
                            (Some(tool.kind), Some(tool.hands), i.ability_spec())
                        } else {
                            (None, None, None)
                        }
                    })
                    .unwrap_or((None, None, None))
            };

            let (active_tool_kind, active_tool_hand, active_tool_spec) =
                tool_info(EquipSlot::ActiveMainhand);
            let (second_tool_kind, second_tool_hand, second_tool_spec) =
                tool_info(EquipSlot::ActiveOffhand);

            let hands = (active_tool_hand, second_tool_hand);

            match body {
                Body::Humanoid(body) => {
                    let (model, skeleton_attr) = self.model_cache.get_or_create_model(
                        renderer,
                        &mut self.col_lights,
                        *body,
                        inventory,
                        tick,
                        player_camera_mode,
                        player_character_state,
                        &slow_jobs,
                    );

                    let holding_lantern = inventory
                        .map_or(false, |i| i.equipped(EquipSlot::Lantern).is_some())
                        && light_emitter.is_some()
                        && !((matches!(second_tool_hand, Some(_))
                            || matches!(active_tool_hand, Some(Hands::Two)))
                            && character.map_or(false, |c| c.is_wield()))
                        && !character.map_or(false, |c| c.is_using_hands())
                        && physics.in_liquid().is_none();

                    let state = self
                        .states
                        .character_states
                        .entry(entity)
                        .or_insert_with(|| {
                            FigureState::new(renderer, CharacterSkeleton::new(holding_lantern))
                        });

                    // Average velocity relative to the current ground
                    let rel_avg_vel = state.avg_vel - physics.ground_vel;

                    let (character, last_character) = match (character, last_character) {
                        (Some(c), Some(l)) => (c, l),
                        _ => continue,
                    };

                    if !character.same_variant(&last_character.0) {
                        state.state_time = 0.0;
                    }

                    let target_base = match (
                        physics.on_ground,
                        rel_vel.magnitude_squared() > MOVING_THRESHOLD_SQR, // Moving
                        physics.in_liquid().is_some(),                      // In water
                    ) {
                        // Standing
                        (true, false, false) => anim::character::StandAnimation::update_skeleton(
                            &CharacterSkeleton::new(holding_lantern),
                            (active_tool_kind, second_tool_kind, hands, time, rel_avg_vel),
                            state.state_time,
                            &mut state_animation_rate,
                            skeleton_attr,
                        ),
                        // Running
                        (true, true, false) => anim::character::RunAnimation::update_skeleton(
                            &CharacterSkeleton::new(holding_lantern),
                            (
                                active_tool_kind,
                                second_tool_kind,
                                hands,
                                rel_vel,
                                // TODO: Update to use the quaternion.
                                ori * anim::vek::Vec3::<f32>::unit_y(),
                                state.last_ori * anim::vek::Vec3::<f32>::unit_y(),
                                time,
                                rel_avg_vel,
                                state.acc_vel,
                            ),
                            state.state_time,
                            &mut state_animation_rate,
                            skeleton_attr,
                        ),
                        // In air
                        (false, _, false) => anim::character::JumpAnimation::update_skeleton(
                            &CharacterSkeleton::new(holding_lantern),
                            (
                                active_tool_kind,
                                second_tool_kind,
                                hands,
                                rel_vel,
                                // TODO: Update to use the quaternion.
                                ori * anim::vek::Vec3::<f32>::unit_y(),
                                state.last_ori * anim::vek::Vec3::<f32>::unit_y(),
                                time,
                            ),
                            state.state_time,
                            &mut state_animation_rate,
                            skeleton_attr,
                        ),
                        // Swim
                        (_, _, true) => anim::character::SwimAnimation::update_skeleton(
                            &CharacterSkeleton::new(holding_lantern),
                            (
                                active_tool_kind,
                                second_tool_kind,
                                hands,
                                rel_vel,
                                // TODO: Update to use the quaternion.
                                ori * anim::vek::Vec3::<f32>::unit_y(),
                                state.last_ori * anim::vek::Vec3::<f32>::unit_y(),
                                time,
                                rel_avg_vel,
                            ),
                            state.state_time,
                            &mut state_animation_rate,
                            skeleton_attr,
                        ),
                    };
                    let target_bones = match &character {
                        CharacterState::Roll(s) => {
                            let stage_time = s.timer.as_secs_f32();

                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time / s.static_data.buildup_duration.as_secs_f32()
                                },
                                StageSection::Movement => {
                                    stage_time / s.static_data.movement_duration.as_secs_f32()
                                },
                                StageSection::Recover => {
                                    stage_time / s.static_data.recover_duration.as_secs_f32()
                                },
                                _ => 0.0,
                            };
                            anim::character::RollAnimation::update_skeleton(
                                &target_base,
                                (
                                    active_tool_kind,
                                    second_tool_kind,
                                    // TODO: Update to use the quaternion.
                                    ori * anim::vek::Vec3::<f32>::unit_y(),
                                    state.last_ori * anim::vek::Vec3::<f32>::unit_y(),
                                    time,
                                    Some(s.stage_section),
                                ),
                                stage_progress,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        CharacterState::BasicMelee(s) => {
                            let stage_time = s.timer.as_secs_f32();
                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time / s.static_data.buildup_duration.as_secs_f32()
                                },
                                StageSection::Swing => {
                                    stage_time / s.static_data.swing_duration.as_secs_f32()
                                },
                                StageSection::Recover => {
                                    stage_time / s.static_data.recover_duration.as_secs_f32()
                                },
                                _ => 0.0,
                            };
                            anim::character::AlphaAnimation::update_skeleton(
                                &target_base,
                                (
                                    hands,
                                    Some(s.stage_section),
                                    Some(s.static_data.ability_info),
                                ),
                                stage_progress,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        CharacterState::ChargedRanged(s) => {
                            let stage_time = s.timer.as_secs_f32();

                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time / s.static_data.buildup_duration.as_secs_f32()
                                },
                                StageSection::Recover => {
                                    stage_time / s.static_data.recover_duration.as_secs_f32()
                                },

                                _ => 0.0,
                            };

                            anim::character::ShootAnimation::update_skeleton(
                                &target_base,
                                (
                                    Some(s.static_data.ability_info),
                                    hands,
                                    rel_vel.magnitude(),
                                    // TODO: Update to use the quaternion.
                                    ori * anim::vek::Vec3::<f32>::unit_y(),
                                    state.last_ori * anim::vek::Vec3::<f32>::unit_y(),
                                    look_dir,
                                    time,
                                    Some(s.stage_section),
                                ),
                                stage_progress,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        CharacterState::BasicRanged(s) => {
                            let stage_time = s.timer.as_secs_f32();

                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time / s.static_data.buildup_duration.as_secs_f32()
                                },
                                StageSection::Recover => {
                                    stage_time / s.static_data.recover_duration.as_secs_f32()
                                },

                                _ => 0.0,
                            };

                            anim::character::ShootAnimation::update_skeleton(
                                &target_base,
                                (
                                    Some(s.static_data.ability_info),
                                    hands,
                                    rel_vel.magnitude(),
                                    // TODO: Update to use the quaternion.
                                    ori * anim::vek::Vec3::<f32>::unit_y(),
                                    state.last_ori * anim::vek::Vec3::<f32>::unit_y(),
                                    look_dir,
                                    time,
                                    Some(s.stage_section),
                                ),
                                stage_progress,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        CharacterState::ChargedMelee(s) => {
                            let stage_time = s.timer.as_secs_f32();

                            let stage_progress = match s.stage_section {
                                StageSection::Charge => {
                                    stage_time / s.static_data.charge_duration.as_secs_f32()
                                },
                                StageSection::Swing => {
                                    stage_time / s.static_data.swing_duration.as_secs_f32()
                                },
                                StageSection::Recover => {
                                    stage_time / s.static_data.recover_duration.as_secs_f32()
                                },
                                _ => 0.0,
                            };

                            anim::character::ChargeswingAnimation::update_skeleton(
                                &target_base,
                                (
                                    hands,
                                    rel_vel,
                                    time,
                                    Some(s.stage_section),
                                    Some(s.static_data.ability_info),
                                ),
                                stage_progress,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        CharacterState::RepeaterRanged(s) => {
                            let stage_time = s.timer.as_secs_f32();

                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time / s.static_data.buildup_duration.as_secs_f32()
                                },
                                StageSection::Shoot => {
                                    stage_time / s.static_data.shoot_duration.as_secs_f32()
                                },
                                StageSection::Recover => {
                                    stage_time / s.static_data.recover_duration.as_secs_f32()
                                },
                                _ => 0.0,
                            };

                            anim::character::RepeaterAnimation::update_skeleton(
                                &target_base,
                                (
                                    Some(s.static_data.ability_info),
                                    hands,
                                    rel_vel,
                                    time,
                                    Some(s.stage_section),
                                ),
                                stage_progress,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        CharacterState::Sneak { .. } => {
                            anim::character::SneakAnimation::update_skeleton(
                                &target_base,
                                (
                                    active_tool_kind,
                                    rel_vel,
                                    // TODO: Update to use the quaternion.
                                    ori * anim::vek::Vec3::<f32>::unit_y(),
                                    state.last_ori * anim::vek::Vec3::<f32>::unit_y(),
                                    time,
                                ),
                                state.state_time,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        CharacterState::Boost(_) => {
                            anim::character::AlphaAnimation::update_skeleton(
                                &target_base,
                                (hands, None, None),
                                state.state_time,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        CharacterState::DashMelee(s) => {
                            let stage_time = s.timer.as_secs_f32();
                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time / s.static_data.buildup_duration.as_secs_f32()
                                },
                                StageSection::Charge => {
                                    stage_time / s.static_data.charge_duration.as_secs_f32()
                                },
                                StageSection::Swing => {
                                    stage_time / s.static_data.swing_duration.as_secs_f32()
                                },
                                StageSection::Recover => {
                                    stage_time / s.static_data.recover_duration.as_secs_f32()
                                },
                                _ => 0.0,
                            };
                            anim::character::DashAnimation::update_skeleton(
                                &target_base,
                                (
                                    hands,
                                    time,
                                    Some(s.stage_section),
                                    Some(s.static_data.ability_info),
                                ),
                                stage_progress,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        CharacterState::Shockwave(s) => {
                            let stage_time = s.timer.as_secs_f32();
                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time / s.static_data.buildup_duration.as_secs_f32()
                                },
                                StageSection::Swing => {
                                    stage_time / s.static_data.swing_duration.as_secs_f32()
                                },
                                StageSection::Recover => {
                                    stage_time / s.static_data.recover_duration.as_secs_f32()
                                },
                                _ => 0.0,
                            };
                            anim::character::ShockwaveAnimation::update_skeleton(
                                &target_base,
                                (
                                    Some(s.static_data.ability_info),
                                    hands,
                                    time,
                                    rel_vel.magnitude(),
                                    Some(s.stage_section),
                                ),
                                stage_progress,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        CharacterState::BasicAura(s) => {
                            let stage_time = s.timer.as_secs_f32();
                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time / s.static_data.buildup_duration.as_secs_f32()
                                },
                                StageSection::Cast => {
                                    stage_time / s.static_data.cast_duration.as_secs_f32()
                                },
                                StageSection::Recover => {
                                    stage_time / s.static_data.recover_duration.as_secs_f32()
                                },
                                _ => 0.0,
                            };
                            anim::character::ShockwaveAnimation::update_skeleton(
                                &target_base,
                                (
                                    Some(s.static_data.ability_info),
                                    hands,
                                    time,
                                    rel_vel.magnitude(),
                                    Some(s.stage_section),
                                ),
                                stage_progress,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        CharacterState::LeapMelee(s) => {
                            let stage_time = s.timer.as_secs_f32();
                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time / s.static_data.buildup_duration.as_secs_f32()
                                },
                                StageSection::Movement => {
                                    stage_time / s.static_data.movement_duration.as_secs_f32()
                                },
                                StageSection::Swing => {
                                    stage_time / s.static_data.swing_duration.as_secs_f32()
                                },
                                StageSection::Recover => {
                                    stage_time / s.static_data.recover_duration.as_secs_f32()
                                },
                                _ => 0.0,
                            };

                            anim::character::LeapAnimation::update_skeleton(
                                &target_base,
                                (
                                    hands,
                                    rel_vel,
                                    time,
                                    Some(s.stage_section),
                                    Some(s.static_data.ability_info),
                                ),
                                stage_progress,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        CharacterState::SpinMelee(s) => {
                            let stage_time = s.timer.as_secs_f32();
                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time / s.static_data.buildup_duration.as_secs_f32()
                                },
                                StageSection::Swing => {
                                    stage_time / s.static_data.swing_duration.as_secs_f32()
                                },
                                StageSection::Recover => {
                                    stage_time / s.static_data.recover_duration.as_secs_f32()
                                },
                                _ => 0.0,
                            };

                            anim::character::SpinMeleeAnimation::update_skeleton(
                                &target_base,
                                (
                                    hands,
                                    rel_vel,
                                    time,
                                    Some(s.stage_section),
                                    Some(s.static_data.ability_info),
                                ),
                                stage_progress,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        CharacterState::Stunned(s) => {
                            let stage_time = s.timer.as_secs_f32();
                            let wield_status = s.was_wielded;
                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time / s.static_data.buildup_duration.as_secs_f32()
                                },
                                StageSection::Recover => {
                                    stage_time / s.static_data.recover_duration.as_secs_f32()
                                },
                                _ => 0.0,
                            };
                            match s.static_data.poise_state {
                                PoiseState::Normal
                                | PoiseState::Stunned
                                | PoiseState::Interrupted => {
                                    anim::character::StunnedAnimation::update_skeleton(
                                        &target_base,
                                        (
                                            active_tool_kind,
                                            second_tool_kind,
                                            hands,
                                            rel_vel.magnitude(),
                                            time,
                                            Some(s.stage_section),
                                            state.state_time,
                                            wield_status,
                                        ),
                                        stage_progress,
                                        &mut state_animation_rate,
                                        skeleton_attr,
                                    )
                                },
                                PoiseState::Dazed | PoiseState::KnockedDown => {
                                    anim::character::StaggeredAnimation::update_skeleton(
                                        &target_base,
                                        (
                                            active_tool_kind,
                                            second_tool_kind,
                                            hands,
                                            rel_vel.magnitude(),
                                            time,
                                            Some(s.stage_section),
                                            state.state_time,
                                            wield_status,
                                        ),
                                        stage_progress,
                                        &mut state_animation_rate,
                                        skeleton_attr,
                                    )
                                },
                            }
                        },
                        CharacterState::BasicBeam(s) => {
                            let stage_time = s.timer.as_secs_f32();
                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time / s.static_data.buildup_duration.as_secs_f32()
                                },
                                StageSection::Cast => s.timer.as_secs_f32(),
                                StageSection::Recover => {
                                    stage_time / s.static_data.recover_duration.as_secs_f32()
                                },
                                _ => 0.0,
                            };
                            anim::character::BeamAnimation::update_skeleton(
                                &target_base,
                                (
                                    Some(s.static_data.ability_info),
                                    hands,
                                    time,
                                    rel_vel.magnitude(),
                                    Some(s.stage_section),
                                ),
                                stage_progress,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        CharacterState::HealingBeam(s) => {
                            let stage_time = s.timer.as_secs_f32();
                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time / s.static_data.buildup_duration.as_secs_f32()
                                },
                                StageSection::Cast => s.timer.as_secs_f32(),
                                StageSection::Recover => {
                                    stage_time / s.static_data.recover_duration.as_secs_f32()
                                },
                                _ => 0.0,
                            };
                            anim::character::BeamAnimation::update_skeleton(
                                &target_base,
                                (
                                    Some(s.static_data.ability_info),
                                    hands,
                                    time,
                                    rel_vel.magnitude(),
                                    Some(s.stage_section),
                                ),
                                stage_progress,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        CharacterState::ComboMelee(s) => {
                            let stage_index = (s.stage - 1) as usize;
                            let stage_time = s.timer.as_secs_f32();
                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time
                                        / s.static_data.stage_data[stage_index]
                                            .base_buildup_duration
                                            .as_secs_f32()
                                },
                                StageSection::Swing => {
                                    stage_time
                                        / s.static_data.stage_data[stage_index]
                                            .base_swing_duration
                                            .as_secs_f32()
                                },
                                StageSection::Recover => {
                                    stage_time
                                        / s.static_data.stage_data[stage_index]
                                            .base_recover_duration
                                            .as_secs_f32()
                                },
                                _ => 0.0,
                            };
                            match s.stage {
                                1 => anim::character::AlphaAnimation::update_skeleton(
                                    &target_base,
                                    (
                                        hands,
                                        Some(s.stage_section),
                                        Some(s.static_data.ability_info),
                                    ),
                                    stage_progress,
                                    &mut state_animation_rate,
                                    skeleton_attr,
                                ),
                                2 => anim::character::SpinAnimation::update_skeleton(
                                    &target_base,
                                    (
                                        hands,
                                        rel_vel,
                                        time,
                                        Some(s.stage_section),
                                        Some(s.static_data.ability_info),
                                    ),
                                    stage_progress,
                                    &mut state_animation_rate,
                                    skeleton_attr,
                                ),
                                _ => anim::character::BetaAnimation::update_skeleton(
                                    &target_base,
                                    (
                                        hands,
                                        rel_vel.magnitude(),
                                        time,
                                        Some(s.stage_section),
                                        Some(s.static_data.ability_info),
                                    ),
                                    stage_progress,
                                    &mut state_animation_rate,
                                    skeleton_attr,
                                ),
                            }
                        },
                        CharacterState::BasicBlock(s) => {
                            let stage_time = s.timer.as_secs_f32();
                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time / s.static_data.buildup_duration.as_secs_f32()
                                },
                                StageSection::Swing => stage_time,
                                StageSection::Recover => {
                                    stage_time / s.static_data.recover_duration.as_secs_f32()
                                },
                                _ => 0.0,
                            };
                            anim::character::BlockAnimation::update_skeleton(
                                &target_base,
                                (
                                    hands,
                                    active_tool_kind,
                                    second_tool_kind,
                                    rel_vel,
                                    time,
                                    Some(s.stage_section),
                                ),
                                stage_progress,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        CharacterState::Equipping { .. } => {
                            anim::character::EquipAnimation::update_skeleton(
                                &target_base,
                                (
                                    active_tool_kind,
                                    second_tool_kind,
                                    rel_vel.magnitude(),
                                    time,
                                ),
                                state.state_time,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        CharacterState::Talk => anim::character::TalkAnimation::update_skeleton(
                            &target_base,
                            (
                                active_tool_kind,
                                second_tool_kind,
                                rel_vel.magnitude(),
                                time,
                                look_dir,
                            ),
                            state.state_time,
                            &mut state_animation_rate,
                            skeleton_attr,
                        ),
                        CharacterState::Wielding { .. } => {
                            if physics.in_liquid().is_some() {
                                anim::character::SwimWieldAnimation::update_skeleton(
                                    &target_base,
                                    (
                                        active_tool_kind,
                                        second_tool_kind,
                                        hands,
                                        rel_vel.magnitude(),
                                        time,
                                    ),
                                    state.state_time,
                                    &mut state_animation_rate,
                                    skeleton_attr,
                                )
                            } else {
                                anim::character::WieldAnimation::update_skeleton(
                                    &target_base,
                                    (
                                        active_tool_kind,
                                        second_tool_kind,
                                        hands,
                                        // TODO: Update to use the quaternion.
                                        ori * anim::vek::Vec3::<f32>::unit_y(),
                                        state.last_ori * anim::vek::Vec3::<f32>::unit_y(),
                                        look_dir,
                                        rel_vel,
                                        time,
                                    ),
                                    state.state_time,
                                    &mut state_animation_rate,
                                    skeleton_attr,
                                )
                            }
                        },
                        CharacterState::Glide(data) => {
                            anim::character::GlidingAnimation::update_skeleton(
                                &target_base,
                                (rel_vel, ori, data.ori.into(), time, state.acc_vel),
                                state.state_time,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        CharacterState::Climb { .. } => {
                            anim::character::ClimbAnimation::update_skeleton(
                                &target_base,
                                (
                                    active_tool_kind,
                                    second_tool_kind,
                                    rel_vel,
                                    // TODO: Update to use the quaternion.
                                    ori * anim::vek::Vec3::<f32>::unit_y(),
                                    time,
                                ),
                                state.state_time,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        CharacterState::Sit { .. } => {
                            anim::character::SitAnimation::update_skeleton(
                                &target_base,
                                (active_tool_kind, second_tool_kind, time),
                                state.state_time,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        CharacterState::GlideWield { .. } => {
                            anim::character::GlideWieldAnimation::update_skeleton(
                                &target_base,
                                (),
                                state.state_time,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        CharacterState::Dance { .. } => {
                            anim::character::DanceAnimation::update_skeleton(
                                &target_base,
                                (active_tool_kind, second_tool_kind, time),
                                state.state_time,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        _ => target_base,
                    };

                    state.skeleton = anim::vek::Lerp::lerp(&state.skeleton, &target_bones, dt_lerp);
                    state.update(
                        renderer,
                        pos.0,
                        ori,
                        scale,
                        col,
                        dt,
                        state_animation_rate,
                        model,
                        lpindex,
                        in_frustum,
                        is_player,
                        camera,
                        &mut update_buf,
                        terrain,
                        physics.ground_vel,
                    );
                },
                Body::QuadrupedSmall(body) => {
                    let (model, skeleton_attr) =
                        self.quadruped_small_model_cache.get_or_create_model(
                            renderer,
                            &mut self.col_lights,
                            *body,
                            inventory,
                            tick,
                            player_camera_mode,
                            player_character_state,
                            &slow_jobs,
                        );

                    let state = self
                        .states
                        .quadruped_small_states
                        .entry(entity)
                        .or_insert_with(|| {
                            FigureState::new(renderer, QuadrupedSmallSkeleton::default())
                        });

                    // Average velocity relative to the current ground
                    let rel_avg_vel = state.avg_vel - physics.ground_vel;

                    let (character, last_character) = match (character, last_character) {
                        (Some(c), Some(l)) => (c, l),
                        _ => continue,
                    };

                    if !character.same_variant(&last_character.0) {
                        state.state_time = 0.0;
                    }

                    let target_base = match (
                        physics.on_ground,
                        rel_vel.magnitude_squared() > MOVING_THRESHOLD_SQR, // Moving
                        physics.in_liquid().is_some(),                      // In water
                    ) {
                        // Standing
                        (true, false, false) => {
                            anim::quadruped_small::IdleAnimation::update_skeleton(
                                &QuadrupedSmallSkeleton::default(),
                                time,
                                state.state_time,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        // Running
                        (true, true, false) => {
                            anim::quadruped_small::RunAnimation::update_skeleton(
                                &QuadrupedSmallSkeleton::default(),
                                (
                                    rel_vel.magnitude(),
                                    // TODO: Update to use the quaternion.
                                    ori * anim::vek::Vec3::<f32>::unit_y(),
                                    state.last_ori * anim::vek::Vec3::<f32>::unit_y(),
                                    time,
                                    rel_avg_vel,
                                    state.acc_vel,
                                ),
                                state.state_time,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        // Running
                        (false, _, true) => anim::quadruped_small::RunAnimation::update_skeleton(
                            &QuadrupedSmallSkeleton::default(),
                            (
                                rel_vel.magnitude(),
                                // TODO: Update to use the quaternion.
                                ori * anim::vek::Vec3::<f32>::unit_y(),
                                state.last_ori * anim::vek::Vec3::<f32>::unit_y(),
                                time,
                                rel_avg_vel,
                                state.acc_vel,
                            ),
                            state.state_time,
                            &mut state_animation_rate,
                            skeleton_attr,
                        ),
                        // In air
                        (false, _, false) => anim::quadruped_small::JumpAnimation::update_skeleton(
                            &QuadrupedSmallSkeleton::default(),
                            (rel_vel.magnitude(), time),
                            state.state_time,
                            &mut state_animation_rate,
                            skeleton_attr,
                        ),
                        _ => anim::quadruped_small::IdleAnimation::update_skeleton(
                            &QuadrupedSmallSkeleton::default(),
                            time,
                            state.state_time,
                            &mut state_animation_rate,
                            skeleton_attr,
                        ),
                    };
                    let target_bones = match &character {
                        CharacterState::ComboMelee(s) => {
                            let stage_index = (s.stage - 1) as usize;
                            let stage_time = s.timer.as_secs_f32();
                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time
                                        / s.static_data.stage_data[stage_index]
                                            .base_buildup_duration
                                            .as_secs_f32()
                                },
                                StageSection::Swing => {
                                    stage_time
                                        / s.static_data.stage_data[stage_index]
                                            .base_swing_duration
                                            .as_secs_f32()
                                },
                                StageSection::Recover => {
                                    stage_time
                                        / s.static_data.stage_data[stage_index]
                                            .base_recover_duration
                                            .as_secs_f32()
                                },
                                _ => 0.0,
                            };
                            {
                                anim::quadruped_small::AlphaAnimation::update_skeleton(
                                    &target_base,
                                    (
                                        rel_vel.magnitude(),
                                        time,
                                        Some(s.stage_section),
                                        state.state_time,
                                    ),
                                    stage_progress,
                                    &mut state_animation_rate,
                                    skeleton_attr,
                                )
                            }
                        },
                        CharacterState::Stunned(s) => {
                            let stage_time = s.timer.as_secs_f32();
                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time / s.static_data.buildup_duration.as_secs_f32()
                                },
                                StageSection::Recover => {
                                    stage_time / s.static_data.recover_duration.as_secs_f32()
                                },
                                _ => 0.0,
                            };
                            match s.static_data.poise_state {
                                PoiseState::Normal
                                | PoiseState::Interrupted
                                | PoiseState::Stunned
                                | PoiseState::Dazed
                                | PoiseState::KnockedDown => {
                                    anim::quadruped_small::StunnedAnimation::update_skeleton(
                                        &target_base,
                                        (
                                            rel_vel.magnitude(),
                                            time,
                                            Some(s.stage_section),
                                            state.state_time,
                                        ),
                                        stage_progress,
                                        &mut state_animation_rate,
                                        skeleton_attr,
                                    )
                                },
                            }
                        },
                        CharacterState::Sit { .. } => {
                            anim::quadruped_small::FeedAnimation::update_skeleton(
                                &target_base,
                                time,
                                state.state_time,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        // TODO!
                        _ => target_base,
                    };

                    state.skeleton = anim::vek::Lerp::lerp(&state.skeleton, &target_bones, dt_lerp);
                    state.update(
                        renderer,
                        pos.0,
                        ori,
                        scale,
                        col,
                        dt,
                        state_animation_rate,
                        model,
                        lpindex,
                        in_frustum,
                        is_player,
                        camera,
                        &mut update_buf,
                        terrain,
                        physics.ground_vel,
                    );
                },
                Body::QuadrupedMedium(body) => {
                    let (model, skeleton_attr) =
                        self.quadruped_medium_model_cache.get_or_create_model(
                            renderer,
                            &mut self.col_lights,
                            *body,
                            inventory,
                            tick,
                            player_camera_mode,
                            player_character_state,
                            &slow_jobs,
                        );

                    let state = self
                        .states
                        .quadruped_medium_states
                        .entry(entity)
                        .or_insert_with(|| {
                            FigureState::new(renderer, QuadrupedMediumSkeleton::default())
                        });

                    // Average velocity relative to the current ground
                    let rel_avg_vel = state.avg_vel - physics.ground_vel;

                    let (character, last_character) = match (character, last_character) {
                        (Some(c), Some(l)) => (c, l),
                        _ => continue,
                    };

                    if !character.same_variant(&last_character.0) {
                        state.state_time = 0.0;
                    }

                    let target_base = match (
                        physics.on_ground,
                        rel_vel.magnitude_squared() > 0.25, // Moving
                        physics.in_liquid().is_some(),      // In water
                    ) {
                        // Standing
                        (true, false, false) => {
                            anim::quadruped_medium::IdleAnimation::update_skeleton(
                                &QuadrupedMediumSkeleton::default(),
                                time,
                                state.state_time,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        // Running
                        (true, true, false) => {
                            anim::quadruped_medium::RunAnimation::update_skeleton(
                                &QuadrupedMediumSkeleton::default(),
                                (
                                    rel_vel.magnitude(),
                                    // TODO: Update to use the quaternion.
                                    ori * anim::vek::Vec3::<f32>::unit_y(),
                                    state.last_ori * anim::vek::Vec3::<f32>::unit_y(),
                                    time,
                                    rel_avg_vel,
                                    state.acc_vel,
                                ),
                                state.state_time,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        //Swimming
                        (false, _, true) => anim::quadruped_medium::RunAnimation::update_skeleton(
                            &QuadrupedMediumSkeleton::default(),
                            (
                                rel_vel.magnitude(),
                                // TODO: Update to use the quaternion.
                                ori * anim::vek::Vec3::<f32>::unit_y(),
                                state.last_ori * anim::vek::Vec3::<f32>::unit_y(),
                                time,
                                rel_avg_vel,
                                state.acc_vel,
                            ),
                            state.state_time,
                            &mut state_animation_rate,
                            skeleton_attr,
                        ),
                        // In air
                        (false, _, false) => {
                            anim::quadruped_medium::JumpAnimation::update_skeleton(
                                &QuadrupedMediumSkeleton::default(),
                                time,
                                state.state_time,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        _ => anim::quadruped_medium::IdleAnimation::update_skeleton(
                            &QuadrupedMediumSkeleton::default(),
                            time,
                            state.state_time,
                            &mut state_animation_rate,
                            skeleton_attr,
                        ),
                    };
                    let target_bones = match &character {
                        CharacterState::BasicMelee(s) => {
                            let stage_time = s.timer.as_secs_f32();

                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time / s.static_data.buildup_duration.as_secs_f32()
                                },
                                StageSection::Swing => {
                                    stage_time / s.static_data.swing_duration.as_secs_f32()
                                },
                                StageSection::Recover => {
                                    stage_time / s.static_data.recover_duration.as_secs_f32()
                                },

                                _ => 0.0,
                            };
                            anim::quadruped_medium::HoofAnimation::update_skeleton(
                                &target_base,
                                (
                                    rel_vel.magnitude(),
                                    time,
                                    Some(s.stage_section),
                                    state.state_time,
                                ),
                                stage_progress,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        CharacterState::DashMelee(s) => {
                            let stage_time = s.timer.as_secs_f32();
                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time / s.static_data.buildup_duration.as_secs_f32()
                                },
                                StageSection::Charge => stage_time,
                                StageSection::Swing => {
                                    stage_time / s.static_data.swing_duration.as_secs_f32()
                                },
                                StageSection::Recover => {
                                    stage_time / s.static_data.recover_duration.as_secs_f32()
                                },
                                _ => 0.0,
                            };
                            anim::quadruped_medium::DashAnimation::update_skeleton(
                                &target_base,
                                (
                                    rel_vel.magnitude(),
                                    time,
                                    Some(s.stage_section),
                                    state.state_time,
                                ),
                                stage_progress,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        CharacterState::LeapMelee(s) => {
                            let stage_time = s.timer.as_secs_f32();
                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time / s.static_data.buildup_duration.as_secs_f32()
                                },
                                StageSection::Movement => {
                                    stage_time / s.static_data.movement_duration.as_secs_f32()
                                },
                                StageSection::Swing => {
                                    stage_time / s.static_data.swing_duration.as_secs_f32()
                                },
                                StageSection::Recover => {
                                    stage_time / s.static_data.recover_duration.as_secs_f32()
                                },
                                _ => 0.0,
                            };
                            anim::quadruped_medium::LeapMeleeAnimation::update_skeleton(
                                &target_base,
                                (
                                    rel_vel.magnitude(),
                                    time,
                                    Some(s.stage_section),
                                    state.state_time,
                                ),
                                stage_progress,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        CharacterState::ComboMelee(s) => {
                            let stage_index = (s.stage - 1) as usize;
                            let stage_time = s.timer.as_secs_f32();
                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time
                                        / s.static_data.stage_data[stage_index]
                                            .base_buildup_duration
                                            .as_secs_f32()
                                },
                                StageSection::Swing => {
                                    stage_time
                                        / s.static_data.stage_data[stage_index]
                                            .base_swing_duration
                                            .as_secs_f32()
                                },
                                StageSection::Recover => {
                                    stage_time
                                        / s.static_data.stage_data[stage_index]
                                            .base_recover_duration
                                            .as_secs_f32()
                                },
                                _ => 0.0,
                            };
                            match s.stage {
                                1 => anim::quadruped_medium::AlphaAnimation::update_skeleton(
                                    &target_base,
                                    (
                                        rel_vel.magnitude(),
                                        time,
                                        Some(s.stage_section),
                                        state.state_time,
                                    ),
                                    stage_progress,
                                    &mut state_animation_rate,
                                    skeleton_attr,
                                ),
                                2 => anim::quadruped_medium::BetaAnimation::update_skeleton(
                                    &target_base,
                                    (
                                        rel_vel.magnitude(),
                                        time,
                                        Some(s.stage_section),
                                        state.state_time,
                                    ),
                                    stage_progress,
                                    &mut state_animation_rate,
                                    skeleton_attr,
                                ),
                                _ => anim::quadruped_medium::AlphaAnimation::update_skeleton(
                                    &target_base,
                                    (
                                        rel_vel.magnitude(),
                                        time,
                                        Some(s.stage_section),
                                        state.state_time,
                                    ),
                                    stage_progress,
                                    &mut state_animation_rate,
                                    skeleton_attr,
                                ),
                            }
                        },
                        CharacterState::Stunned(s) => {
                            let stage_time = s.timer.as_secs_f32();
                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time / s.static_data.buildup_duration.as_secs_f32()
                                },
                                StageSection::Recover => {
                                    stage_time / s.static_data.recover_duration.as_secs_f32()
                                },
                                _ => 0.0,
                            };
                            match s.static_data.poise_state {
                                PoiseState::Normal
                                | PoiseState::Stunned
                                | PoiseState::Interrupted => {
                                    anim::quadruped_medium::StunnedAnimation::update_skeleton(
                                        &target_base,
                                        (
                                            rel_vel.magnitude(),
                                            time,
                                            Some(s.stage_section),
                                            state.state_time,
                                        ),
                                        stage_progress,
                                        &mut state_animation_rate,
                                        skeleton_attr,
                                    )
                                },
                                PoiseState::Dazed | PoiseState::KnockedDown => {
                                    anim::quadruped_medium::StunnedAnimation::update_skeleton(
                                        &target_base,
                                        (
                                            rel_vel.magnitude(),
                                            time,
                                            Some(s.stage_section),
                                            state.state_time,
                                        ),
                                        stage_progress,
                                        &mut state_animation_rate,
                                        skeleton_attr,
                                    )
                                },
                            }
                        },
                        CharacterState::Sit { .. } => {
                            anim::quadruped_medium::FeedAnimation::update_skeleton(
                                &target_base,
                                time,
                                state.state_time,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        // TODO!
                        _ => target_base,
                    };

                    state.skeleton = anim::vek::Lerp::lerp(&state.skeleton, &target_bones, dt_lerp);
                    state.update(
                        renderer,
                        pos.0,
                        ori,
                        scale,
                        col,
                        dt,
                        state_animation_rate,
                        model,
                        lpindex,
                        in_frustum,
                        is_player,
                        camera,
                        &mut update_buf,
                        terrain,
                        physics.ground_vel,
                    );
                },
                Body::QuadrupedLow(body) => {
                    let (model, skeleton_attr) =
                        self.quadruped_low_model_cache.get_or_create_model(
                            renderer,
                            &mut self.col_lights,
                            *body,
                            inventory,
                            tick,
                            player_camera_mode,
                            player_character_state,
                            &slow_jobs,
                        );

                    let state = self
                        .states
                        .quadruped_low_states
                        .entry(entity)
                        .or_insert_with(|| {
                            FigureState::new(renderer, QuadrupedLowSkeleton::default())
                        });

                    // Average velocity relative to the current ground
                    let rel_avg_vel = state.avg_vel - physics.ground_vel;

                    let (character, last_character) = match (character, last_character) {
                        (Some(c), Some(l)) => (c, l),
                        _ => continue,
                    };

                    if !character.same_variant(&last_character.0) {
                        state.state_time = 0.0;
                    }

                    let target_base = match (
                        physics.on_ground,
                        rel_vel.magnitude_squared() > MOVING_THRESHOLD_SQR, // Moving
                        physics.in_liquid().is_some(),                      // In water
                    ) {
                        // Standing
                        (true, false, false) => {
                            anim::quadruped_low::IdleAnimation::update_skeleton(
                                &QuadrupedLowSkeleton::default(),
                                time,
                                state.state_time,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        // Running
                        (true, true, false) => anim::quadruped_low::RunAnimation::update_skeleton(
                            &QuadrupedLowSkeleton::default(),
                            (
                                rel_vel.magnitude(),
                                // TODO: Update to use the quaternion.
                                ori * anim::vek::Vec3::<f32>::unit_y(),
                                state.last_ori * anim::vek::Vec3::<f32>::unit_y(),
                                time,
                                rel_avg_vel,
                                state.acc_vel,
                            ),
                            state.state_time,
                            &mut state_animation_rate,
                            skeleton_attr,
                        ),
                        // Swimming
                        (false, _, true) => anim::quadruped_low::RunAnimation::update_skeleton(
                            &QuadrupedLowSkeleton::default(),
                            (
                                rel_vel.magnitude(),
                                // TODO: Update to use the quaternion.
                                ori * anim::vek::Vec3::<f32>::unit_y(),
                                state.last_ori * anim::vek::Vec3::<f32>::unit_y(),
                                time,
                                rel_avg_vel,
                                state.acc_vel,
                            ),
                            state.state_time,
                            &mut state_animation_rate,
                            skeleton_attr,
                        ),
                        // In air
                        (false, _, false) => anim::quadruped_low::JumpAnimation::update_skeleton(
                            &QuadrupedLowSkeleton::default(),
                            (rel_vel.magnitude(), time),
                            state.state_time,
                            &mut state_animation_rate,
                            skeleton_attr,
                        ),
                        _ => anim::quadruped_low::IdleAnimation::update_skeleton(
                            &QuadrupedLowSkeleton::default(),
                            time,
                            state.state_time,
                            &mut state_animation_rate,
                            skeleton_attr,
                        ),
                    };
                    let target_bones = match &character {
                        CharacterState::BasicRanged(s) => {
                            let stage_time = s.timer.as_secs_f32();

                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time / s.static_data.buildup_duration.as_secs_f32()
                                },
                                StageSection::Recover => {
                                    stage_time / s.static_data.recover_duration.as_secs_f32()
                                },

                                _ => 0.0,
                            };
                            anim::quadruped_low::ShootAnimation::update_skeleton(
                                &target_base,
                                (rel_vel.magnitude(), time, Some(s.stage_section)),
                                stage_progress,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        CharacterState::BasicMelee(s) => {
                            let stage_time = s.timer.as_secs_f32();

                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time / s.static_data.buildup_duration.as_secs_f32()
                                },
                                StageSection::Swing => {
                                    stage_time / s.static_data.swing_duration.as_secs_f32()
                                },
                                StageSection::Recover => {
                                    stage_time / s.static_data.recover_duration.as_secs_f32()
                                },

                                _ => 0.0,
                            };
                            anim::quadruped_low::BetaAnimation::update_skeleton(
                                &target_base,
                                (
                                    rel_vel.magnitude(),
                                    time,
                                    Some(s.stage_section),
                                    state.state_time,
                                ),
                                stage_progress,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },

                        CharacterState::ChargedMelee(s) => {
                            let stage_time = s.timer.as_secs_f32();

                            let stage_progress = match s.stage_section {
                                StageSection::Charge => {
                                    stage_time / s.static_data.charge_duration.as_secs_f32()
                                },
                                StageSection::Swing => {
                                    stage_time / s.static_data.swing_duration.as_secs_f32()
                                },
                                StageSection::Recover => {
                                    stage_time / s.static_data.recover_duration.as_secs_f32()
                                },

                                _ => 0.0,
                            };
                            anim::quadruped_low::TailwhipAnimation::update_skeleton(
                                &target_base,
                                (
                                    rel_vel.magnitude(),
                                    time,
                                    Some(s.stage_section),
                                    state.state_time,
                                ),
                                stage_progress,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        CharacterState::Stunned(s) => {
                            let stage_time = s.timer.as_secs_f32();
                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time / s.static_data.buildup_duration.as_secs_f32()
                                },
                                StageSection::Recover => {
                                    stage_time / s.static_data.recover_duration.as_secs_f32()
                                },
                                _ => 0.0,
                            };
                            match s.static_data.poise_state {
                                PoiseState::Normal
                                | PoiseState::Stunned
                                | PoiseState::Interrupted => {
                                    anim::quadruped_low::StunnedAnimation::update_skeleton(
                                        &target_base,
                                        (
                                            rel_vel.magnitude(),
                                            time,
                                            Some(s.stage_section),
                                            state.state_time,
                                        ),
                                        stage_progress,
                                        &mut state_animation_rate,
                                        skeleton_attr,
                                    )
                                },
                                PoiseState::Dazed | PoiseState::KnockedDown => {
                                    anim::quadruped_low::StunnedAnimation::update_skeleton(
                                        &target_base,
                                        (
                                            rel_vel.magnitude(),
                                            time,
                                            Some(s.stage_section),
                                            state.state_time,
                                        ),
                                        stage_progress,
                                        &mut state_animation_rate,
                                        skeleton_attr,
                                    )
                                },
                            }
                        },
                        CharacterState::ComboMelee(s) => {
                            let stage_index = (s.stage - 1) as usize;
                            let stage_time = s.timer.as_secs_f32();
                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time
                                        / s.static_data.stage_data[stage_index]
                                            .base_buildup_duration
                                            .as_secs_f32()
                                },
                                StageSection::Swing => {
                                    stage_time
                                        / s.static_data.stage_data[stage_index]
                                            .base_swing_duration
                                            .as_secs_f32()
                                },
                                StageSection::Recover => {
                                    stage_time
                                        / s.static_data.stage_data[stage_index]
                                            .base_recover_duration
                                            .as_secs_f32()
                                },
                                _ => 0.0,
                            };
                            match s.stage {
                                1 => anim::quadruped_low::AlphaAnimation::update_skeleton(
                                    &target_base,
                                    (
                                        rel_vel.magnitude(),
                                        time,
                                        Some(s.stage_section),
                                        state.state_time,
                                    ),
                                    stage_progress,
                                    &mut state_animation_rate,
                                    skeleton_attr,
                                ),
                                2 => anim::quadruped_low::BetaAnimation::update_skeleton(
                                    &target_base,
                                    (
                                        rel_vel.magnitude(),
                                        time,
                                        Some(s.stage_section),
                                        state.state_time,
                                    ),
                                    stage_progress,
                                    &mut state_animation_rate,
                                    skeleton_attr,
                                ),
                                _ => anim::quadruped_low::AlphaAnimation::update_skeleton(
                                    &target_base,
                                    (
                                        rel_vel.magnitude(),
                                        time,
                                        Some(s.stage_section),
                                        state.state_time,
                                    ),
                                    stage_progress,
                                    &mut state_animation_rate,
                                    skeleton_attr,
                                ),
                            }
                        },
                        CharacterState::BasicBeam(s) => {
                            let stage_time = s.timer.as_secs_f32();
                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time / s.static_data.buildup_duration.as_secs_f32()
                                },
                                StageSection::Cast => s.timer.as_secs_f32(),
                                StageSection::Recover => {
                                    stage_time / s.static_data.recover_duration.as_secs_f32()
                                },
                                _ => 0.0,
                            };
                            anim::quadruped_low::BreatheAnimation::update_skeleton(
                                &target_base,
                                (
                                    rel_vel.magnitude(),
                                    time,
                                    Some(s.stage_section),
                                    state.state_time,
                                ),
                                stage_progress,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        CharacterState::DashMelee(s) => {
                            let stage_time = s.timer.as_secs_f32();
                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time / s.static_data.buildup_duration.as_secs_f32()
                                },
                                StageSection::Charge => stage_time,
                                StageSection::Swing => {
                                    stage_time / s.static_data.swing_duration.as_secs_f32()
                                },
                                StageSection::Recover => {
                                    stage_time / s.static_data.recover_duration.as_secs_f32()
                                },
                                _ => 0.0,
                            };
                            anim::quadruped_low::DashAnimation::update_skeleton(
                                &target_base,
                                (
                                    rel_vel.magnitude(),
                                    time,
                                    Some(s.stage_section),
                                    state.state_time,
                                ),
                                stage_progress,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        // TODO!
                        _ => target_base,
                    };

                    state.skeleton = anim::vek::Lerp::lerp(&state.skeleton, &target_bones, dt_lerp);
                    state.update(
                        renderer,
                        pos.0,
                        ori,
                        scale,
                        col,
                        dt,
                        state_animation_rate,
                        model,
                        lpindex,
                        in_frustum,
                        is_player,
                        camera,
                        &mut update_buf,
                        terrain,
                        physics.ground_vel,
                    );
                },
                Body::BirdMedium(body) => {
                    let (model, skeleton_attr) = self.bird_medium_model_cache.get_or_create_model(
                        renderer,
                        &mut self.col_lights,
                        *body,
                        inventory,
                        tick,
                        player_camera_mode,
                        player_character_state,
                        &slow_jobs,
                    );

                    let state = self
                        .states
                        .bird_medium_states
                        .entry(entity)
                        .or_insert_with(|| {
                            FigureState::new(renderer, BirdMediumSkeleton::default())
                        });

                    // Average velocity relative to the current ground
                    let _rel_avg_vel = state.avg_vel - physics.ground_vel;

                    let (character, last_character) = match (character, last_character) {
                        (Some(c), Some(l)) => (c, l),
                        _ => continue,
                    };

                    if !character.same_variant(&last_character.0) {
                        state.state_time = 0.0;
                    }

                    let target_base = match (
                        physics.on_ground,
                        rel_vel.magnitude_squared() > MOVING_THRESHOLD_SQR, // Moving
                        physics.in_liquid().is_some(),                      // In water
                    ) {
                        // Standing
                        (true, false, false) => anim::bird_medium::IdleAnimation::update_skeleton(
                            &BirdMediumSkeleton::default(),
                            time,
                            state.state_time,
                            &mut state_animation_rate,
                            skeleton_attr,
                        ),
                        // Running
                        (true, true, false) => anim::bird_medium::RunAnimation::update_skeleton(
                            &BirdMediumSkeleton::default(),
                            (rel_vel.magnitude(), time),
                            state.state_time,
                            &mut state_animation_rate,
                            skeleton_attr,
                        ),
                        // Running
                        (false, _, true) => anim::bird_medium::RunAnimation::update_skeleton(
                            &BirdMediumSkeleton::default(),
                            (rel_vel.magnitude(), time),
                            state.state_time,
                            &mut state_animation_rate,
                            skeleton_attr,
                        ),
                        // In air
                        (false, _, false) => anim::bird_medium::FlyAnimation::update_skeleton(
                            &BirdMediumSkeleton::default(),
                            (rel_vel.magnitude(), time),
                            state.state_time,
                            &mut state_animation_rate,
                            skeleton_attr,
                        ),
                        _ => anim::bird_medium::IdleAnimation::update_skeleton(
                            &BirdMediumSkeleton::default(),
                            time,
                            state.state_time,
                            &mut state_animation_rate,
                            skeleton_attr,
                        ),
                    };
                    let target_bones = match &character {
                        CharacterState::Sit { .. } => {
                            anim::bird_medium::FeedAnimation::update_skeleton(
                                &target_base,
                                time,
                                state.state_time,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        // TODO!
                        _ => target_base,
                    };

                    state.skeleton = anim::vek::Lerp::lerp(&state.skeleton, &target_bones, dt_lerp);
                    state.update(
                        renderer,
                        pos.0,
                        ori,
                        scale,
                        col,
                        dt,
                        state_animation_rate,
                        model,
                        lpindex,
                        in_frustum,
                        is_player,
                        camera,
                        &mut update_buf,
                        terrain,
                        physics.ground_vel,
                    );
                },
                Body::FishMedium(body) => {
                    let (model, skeleton_attr) = self.fish_medium_model_cache.get_or_create_model(
                        renderer,
                        &mut self.col_lights,
                        *body,
                        inventory,
                        tick,
                        player_camera_mode,
                        player_character_state,
                        &slow_jobs,
                    );

                    let state = self
                        .states
                        .fish_medium_states
                        .entry(entity)
                        .or_insert_with(|| {
                            FigureState::new(renderer, FishMediumSkeleton::default())
                        });

                    // Average velocity relative to the current ground
                    let rel_avg_vel = state.avg_vel - physics.ground_vel;

                    let (character, last_character) = match (character, last_character) {
                        (Some(c), Some(l)) => (c, l),
                        _ => continue,
                    };

                    if !character.same_variant(&last_character.0) {
                        state.state_time = 0.0;
                    }

                    let target_base = match (
                        physics.on_ground,
                        rel_vel.magnitude_squared() > MOVING_THRESHOLD_SQR, // Moving
                        physics.in_liquid().is_some(),                      // In water
                    ) {
                        // Idle
                        (_, false, _) => anim::fish_medium::IdleAnimation::update_skeleton(
                            &FishMediumSkeleton::default(),
                            (
                                rel_vel,
                                // TODO: Update to use the quaternion.
                                ori * anim::vek::Vec3::<f32>::unit_y(),
                                state.last_ori * anim::vek::Vec3::<f32>::unit_y(),
                                time,
                                rel_avg_vel,
                            ),
                            state.state_time,
                            &mut state_animation_rate,
                            skeleton_attr,
                        ),
                        // Swim
                        (_, true, _) => anim::fish_medium::SwimAnimation::update_skeleton(
                            &FishMediumSkeleton::default(),
                            (
                                rel_vel,
                                // TODO: Update to use the quaternion.
                                ori * anim::vek::Vec3::<f32>::unit_y(),
                                state.last_ori * anim::vek::Vec3::<f32>::unit_y(),
                                time,
                                rel_avg_vel,
                                state.acc_vel,
                            ),
                            state.state_time,
                            &mut state_animation_rate,
                            skeleton_attr,
                        ),
                    };

                    state.skeleton = anim::vek::Lerp::lerp(&state.skeleton, &target_base, dt_lerp);
                    state.update(
                        renderer,
                        pos.0,
                        ori,
                        scale,
                        col,
                        dt,
                        state_animation_rate,
                        model,
                        lpindex,
                        in_frustum,
                        is_player,
                        camera,
                        &mut update_buf,
                        terrain,
                        physics.ground_vel,
                    );
                },
                Body::BipedSmall(body) => {
                    let (model, skeleton_attr) = self.biped_small_model_cache.get_or_create_model(
                        renderer,
                        &mut self.col_lights,
                        *body,
                        inventory,
                        tick,
                        player_camera_mode,
                        player_character_state,
                        &slow_jobs,
                    );

                    let state = self
                        .states
                        .biped_small_states
                        .entry(entity)
                        .or_insert_with(|| {
                            FigureState::new(renderer, BipedSmallSkeleton::default())
                        });

                    // Average velocity relative to the current ground
                    let rel_avg_vel = state.avg_vel - physics.ground_vel;

                    let (character, last_character) = match (character, last_character) {
                        (Some(c), Some(l)) => (c, l),
                        _ => continue,
                    };

                    if !character.same_variant(&last_character.0) {
                        state.state_time = 0.0;
                    }

                    let target_base = match (
                        physics.on_ground,
                        rel_vel.magnitude_squared() > MOVING_THRESHOLD_SQR, // Moving
                        physics.in_liquid().is_some(),                      // In water
                    ) {
                        // Idle
                        (true, false, false) => anim::biped_small::IdleAnimation::update_skeleton(
                            &BipedSmallSkeleton::default(),
                            (
                                rel_vel,
                                ori * anim::vek::Vec3::<f32>::unit_y(),
                                state.last_ori * anim::vek::Vec3::<f32>::unit_y(),
                                time,
                                rel_avg_vel,
                            ),
                            state.state_time,
                            &mut state_animation_rate,
                            skeleton_attr,
                        ),
                        // Run
                        (true, true, _) => anim::biped_small::RunAnimation::update_skeleton(
                            &BipedSmallSkeleton::default(),
                            (
                                rel_vel,
                                ori * anim::vek::Vec3::<f32>::unit_y(),
                                state.last_ori * anim::vek::Vec3::<f32>::unit_y(),
                                time,
                                rel_avg_vel,
                                state.acc_vel,
                            ),
                            state.state_time,
                            &mut state_animation_rate,
                            skeleton_attr,
                        ),
                        // Jump
                        (false, _, false) => anim::biped_small::RunAnimation::update_skeleton(
                            &BipedSmallSkeleton::default(),
                            (
                                rel_vel,
                                ori * anim::vek::Vec3::<f32>::unit_y(),
                                state.last_ori * anim::vek::Vec3::<f32>::unit_y(),
                                time,
                                rel_avg_vel,
                                state.acc_vel,
                            ),
                            state.state_time,
                            &mut state_animation_rate,
                            skeleton_attr,
                        ),
                        // Swim
                        (false, _, true) => anim::biped_small::RunAnimation::update_skeleton(
                            &BipedSmallSkeleton::default(),
                            (
                                rel_vel,
                                ori * anim::vek::Vec3::<f32>::unit_y(),
                                state.last_ori * anim::vek::Vec3::<f32>::unit_y(),
                                time,
                                rel_avg_vel,
                                state.acc_vel,
                            ),
                            state.state_time,
                            &mut state_animation_rate,
                            skeleton_attr,
                        ),
                        _ => anim::biped_small::IdleAnimation::update_skeleton(
                            &BipedSmallSkeleton::default(),
                            (
                                rel_vel,
                                ori * anim::vek::Vec3::<f32>::unit_y(),
                                state.last_ori * anim::vek::Vec3::<f32>::unit_y(),
                                time,
                                rel_avg_vel,
                            ),
                            state.state_time,
                            &mut state_animation_rate,
                            skeleton_attr,
                        ),
                    };

                    let target_bones = match &character {
                        CharacterState::Wielding { .. } => {
                            anim::biped_small::WieldAnimation::update_skeleton(
                                &target_base,
                                (
                                    active_tool_kind,
                                    rel_vel,
                                    ori * anim::vek::Vec3::<f32>::unit_y(),
                                    state.last_ori * anim::vek::Vec3::<f32>::unit_y(),
                                    time,
                                    rel_avg_vel,
                                    state.acc_vel,
                                ),
                                state.state_time,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        CharacterState::DashMelee(s) => {
                            let stage_time = s.timer.as_secs_f32();
                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time / s.static_data.buildup_duration.as_secs_f32()
                                },
                                StageSection::Charge => {
                                    stage_time / s.static_data.charge_duration.as_secs_f32()
                                },
                                StageSection::Swing => {
                                    stage_time / s.static_data.swing_duration.as_secs_f32()
                                },
                                StageSection::Recover => {
                                    stage_time / s.static_data.recover_duration.as_secs_f32()
                                },
                                _ => 0.0,
                            };
                            anim::biped_small::DashAnimation::update_skeleton(
                                &target_base,
                                (
                                    rel_vel,
                                    ori * anim::vek::Vec3::<f32>::unit_y(),
                                    state.last_ori * anim::vek::Vec3::<f32>::unit_y(),
                                    time,
                                    rel_avg_vel,
                                    state.acc_vel,
                                    Some(s.stage_section),
                                    state.state_time,
                                ),
                                stage_progress,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        CharacterState::Stunned(s) => {
                            let stage_time = s.timer.as_secs_f32();
                            let wield_status = s.was_wielded;
                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time / s.static_data.buildup_duration.as_secs_f32()
                                },
                                StageSection::Recover => {
                                    stage_time / s.static_data.recover_duration.as_secs_f32()
                                },
                                _ => 0.0,
                            };
                            match s.static_data.poise_state {
                                PoiseState::Normal
                                | PoiseState::Interrupted
                                | PoiseState::Stunned
                                | PoiseState::Dazed
                                | PoiseState::KnockedDown => {
                                    anim::biped_small::StunnedAnimation::update_skeleton(
                                        &target_base,
                                        (
                                            active_tool_kind,
                                            rel_vel,
                                            ori * anim::vek::Vec3::<f32>::unit_y(),
                                            state.last_ori * anim::vek::Vec3::<f32>::unit_y(),
                                            time,
                                            state.avg_vel,
                                            state.acc_vel,
                                            wield_status,
                                            Some(s.stage_section),
                                            state.state_time,
                                        ),
                                        stage_progress,
                                        &mut state_animation_rate,
                                        skeleton_attr,
                                    )
                                },
                            }
                        },
                        CharacterState::ChargedRanged(s) => {
                            let stage_time = s.timer.as_secs_f32();

                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time / s.static_data.buildup_duration.as_secs_f32()
                                },
                                StageSection::Recover => {
                                    stage_time / s.static_data.recover_duration.as_secs_f32()
                                },

                                _ => 0.0,
                            };
                            anim::biped_small::ShootAnimation::update_skeleton(
                                &target_base,
                                (
                                    active_tool_kind,
                                    rel_vel,
                                    ori * anim::vek::Vec3::<f32>::unit_y(),
                                    state.last_ori * anim::vek::Vec3::<f32>::unit_y(),
                                    time,
                                    rel_avg_vel,
                                    state.acc_vel,
                                    Some(s.stage_section),
                                    state.state_time,
                                ),
                                stage_progress,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        CharacterState::BasicRanged(s) => {
                            let stage_time = s.timer.as_secs_f32();

                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time / s.static_data.buildup_duration.as_secs_f32()
                                },
                                StageSection::Recover => {
                                    stage_time / s.static_data.recover_duration.as_secs_f32()
                                },

                                _ => 0.0,
                            };
                            anim::biped_small::ShootAnimation::update_skeleton(
                                &target_base,
                                (
                                    active_tool_kind,
                                    rel_vel,
                                    ori * anim::vek::Vec3::<f32>::unit_y(),
                                    state.last_ori * anim::vek::Vec3::<f32>::unit_y(),
                                    time,
                                    rel_avg_vel,
                                    state.acc_vel,
                                    Some(s.stage_section),
                                    state.state_time,
                                ),
                                stage_progress,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        CharacterState::ComboMelee(s) => {
                            let stage_index = (s.stage - 1) as usize;
                            let stage_time = s.timer.as_secs_f32();
                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time
                                        / s.static_data.stage_data[stage_index]
                                            .base_buildup_duration
                                            .as_secs_f32()
                                },
                                StageSection::Swing => {
                                    stage_time
                                        / s.static_data.stage_data[stage_index]
                                            .base_swing_duration
                                            .as_secs_f32()
                                },
                                StageSection::Recover => {
                                    stage_time
                                        / s.static_data.stage_data[stage_index]
                                            .base_recover_duration
                                            .as_secs_f32()
                                },
                                _ => 0.0,
                            };
                            match s.stage {
                                1 => anim::biped_small::AlphaAnimation::update_skeleton(
                                    &target_base,
                                    (
                                        active_tool_kind,
                                        rel_vel,
                                        ori * anim::vek::Vec3::<f32>::unit_y(),
                                        state.last_ori * anim::vek::Vec3::<f32>::unit_y(),
                                        time,
                                        rel_avg_vel,
                                        state.acc_vel,
                                        Some(s.stage_section),
                                        state.state_time,
                                    ),
                                    stage_progress,
                                    &mut state_animation_rate,
                                    skeleton_attr,
                                ),
                                _ => anim::biped_small::AlphaAnimation::update_skeleton(
                                    &target_base,
                                    (
                                        active_tool_kind,
                                        rel_vel,
                                        ori * anim::vek::Vec3::<f32>::unit_y(),
                                        state.last_ori * anim::vek::Vec3::<f32>::unit_y(),
                                        time,
                                        rel_avg_vel,
                                        state.acc_vel,
                                        Some(s.stage_section),
                                        state.state_time,
                                    ),
                                    stage_progress,
                                    &mut state_animation_rate,
                                    skeleton_attr,
                                ),
                            }
                        },
                        // TODO!
                        _ => target_base,
                    };

                    state.skeleton = anim::vek::Lerp::lerp(&state.skeleton, &target_bones, dt_lerp);
                    state.update(
                        renderer,
                        pos.0,
                        ori,
                        scale,
                        col,
                        dt,
                        state_animation_rate,
                        model,
                        lpindex,
                        in_frustum,
                        is_player,
                        camera,
                        &mut update_buf,
                        terrain,
                        physics.ground_vel,
                    );
                },
                Body::Dragon(body) => {
                    let (model, skeleton_attr) = self.dragon_model_cache.get_or_create_model(
                        renderer,
                        &mut self.col_lights,
                        *body,
                        inventory,
                        tick,
                        player_camera_mode,
                        player_character_state,
                        &slow_jobs,
                    );

                    let state =
                        self.states.dragon_states.entry(entity).or_insert_with(|| {
                            FigureState::new(renderer, DragonSkeleton::default())
                        });

                    // Average velocity relative to the current ground
                    let rel_avg_vel = state.avg_vel - physics.ground_vel;

                    let (character, last_character) = match (character, last_character) {
                        (Some(c), Some(l)) => (c, l),
                        _ => continue,
                    };

                    if !character.same_variant(&last_character.0) {
                        state.state_time = 0.0;
                    }

                    let target_base = match (
                        physics.on_ground,
                        rel_vel.magnitude_squared() > MOVING_THRESHOLD_SQR, // Moving
                        physics.in_liquid().is_some(),                      // In water
                    ) {
                        // Standing
                        (true, false, false) => anim::dragon::IdleAnimation::update_skeleton(
                            &DragonSkeleton::default(),
                            time,
                            state.state_time,
                            &mut state_animation_rate,
                            skeleton_attr,
                        ),
                        // Running
                        (true, true, false) => anim::dragon::RunAnimation::update_skeleton(
                            &DragonSkeleton::default(),
                            (
                                rel_vel.magnitude(),
                                // TODO: Update to use the quaternion.
                                ori * anim::vek::Vec3::<f32>::unit_y(),
                                state.last_ori * anim::vek::Vec3::<f32>::unit_y(),
                                time,
                                rel_avg_vel,
                            ),
                            state.state_time,
                            &mut state_animation_rate,
                            skeleton_attr,
                        ),
                        // In air
                        (false, _, false) => anim::dragon::FlyAnimation::update_skeleton(
                            &DragonSkeleton::default(),
                            (rel_vel.magnitude(), time),
                            state.state_time,
                            &mut state_animation_rate,
                            skeleton_attr,
                        ),
                        // TODO!
                        _ => anim::dragon::IdleAnimation::update_skeleton(
                            &DragonSkeleton::default(),
                            time,
                            state.state_time,
                            &mut state_animation_rate,
                            skeleton_attr,
                        ),
                    };

                    state.skeleton = anim::vek::Lerp::lerp(&state.skeleton, &target_base, dt_lerp);
                    state.update(
                        renderer,
                        pos.0,
                        ori,
                        scale,
                        col,
                        dt,
                        state_animation_rate,
                        model,
                        lpindex,
                        in_frustum,
                        is_player,
                        camera,
                        &mut update_buf,
                        terrain,
                        physics.ground_vel,
                    );
                },
                Body::Theropod(body) => {
                    let (model, skeleton_attr) = self.theropod_model_cache.get_or_create_model(
                        renderer,
                        &mut self.col_lights,
                        *body,
                        inventory,
                        tick,
                        player_camera_mode,
                        player_character_state,
                        &slow_jobs,
                    );

                    let state = self
                        .states
                        .theropod_states
                        .entry(entity)
                        .or_insert_with(|| FigureState::new(renderer, TheropodSkeleton::default()));

                    // Average velocity relative to the current ground
                    let rel_avg_vel = state.avg_vel - physics.ground_vel;

                    let (character, last_character) = match (character, last_character) {
                        (Some(c), Some(l)) => (c, l),
                        _ => continue,
                    };

                    if !character.same_variant(&last_character.0) {
                        state.state_time = 0.0;
                    }

                    let target_base = match (
                        physics.on_ground,
                        rel_vel.magnitude_squared() > MOVING_THRESHOLD_SQR, // Moving
                        physics.in_liquid().is_some(),                      // In water
                    ) {
                        // Standing
                        (true, false, false) => anim::theropod::IdleAnimation::update_skeleton(
                            &TheropodSkeleton::default(),
                            time,
                            state.state_time,
                            &mut state_animation_rate,
                            skeleton_attr,
                        ),
                        // Running
                        (true, true, false) => anim::theropod::RunAnimation::update_skeleton(
                            &TheropodSkeleton::default(),
                            (
                                rel_vel,
                                // TODO: Update to use the quaternion.
                                ori * anim::vek::Vec3::<f32>::unit_y(),
                                state.last_ori * anim::vek::Vec3::<f32>::unit_y(),
                                time,
                                rel_avg_vel,
                                state.acc_vel,
                            ),
                            state.state_time,
                            &mut state_animation_rate,
                            skeleton_attr,
                        ),
                        // In air
                        (false, _, false) => anim::theropod::JumpAnimation::update_skeleton(
                            &TheropodSkeleton::default(),
                            (
                                rel_vel.magnitude(),
                                // TODO: Update to use the quaternion.
                                ori * anim::vek::Vec3::<f32>::unit_y(),
                                state.last_ori * anim::vek::Vec3::<f32>::unit_y(),
                                time,
                                rel_avg_vel,
                            ),
                            state.state_time,
                            &mut state_animation_rate,
                            skeleton_attr,
                        ),
                        _ => anim::theropod::IdleAnimation::update_skeleton(
                            &TheropodSkeleton::default(),
                            time,
                            state.state_time,
                            &mut state_animation_rate,
                            skeleton_attr,
                        ),
                    };
                    let target_bones = match &character {
                        CharacterState::ComboMelee(s) => {
                            let stage_index = (s.stage - 1) as usize;
                            let stage_time = s.timer.as_secs_f32();
                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time
                                        / s.static_data.stage_data[stage_index]
                                            .base_buildup_duration
                                            .as_secs_f32()
                                },
                                StageSection::Swing => {
                                    stage_time
                                        / s.static_data.stage_data[stage_index]
                                            .base_swing_duration
                                            .as_secs_f32()
                                },
                                StageSection::Recover => {
                                    stage_time
                                        / s.static_data.stage_data[stage_index]
                                            .base_recover_duration
                                            .as_secs_f32()
                                },
                                _ => 0.0,
                            };
                            match s.stage {
                                1 => anim::theropod::AlphaAnimation::update_skeleton(
                                    &target_base,
                                    (
                                        rel_vel.magnitude(),
                                        time,
                                        Some(s.stage_section),
                                        state.state_time,
                                    ),
                                    stage_progress,
                                    &mut state_animation_rate,
                                    skeleton_attr,
                                ),
                                _ => anim::theropod::BetaAnimation::update_skeleton(
                                    &target_base,
                                    (
                                        rel_vel.magnitude(),
                                        time,
                                        Some(s.stage_section),
                                        state.state_time,
                                    ),
                                    stage_progress,
                                    &mut state_animation_rate,
                                    skeleton_attr,
                                ),
                            }
                        },
                        CharacterState::DashMelee(s) => {
                            let stage_time = s.timer.as_secs_f32();
                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time / s.static_data.buildup_duration.as_secs_f32()
                                },
                                StageSection::Charge => {
                                    stage_time / s.static_data.charge_duration.as_secs_f32()
                                },
                                StageSection::Swing => {
                                    stage_time / s.static_data.swing_duration.as_secs_f32()
                                },
                                StageSection::Recover => {
                                    stage_time / s.static_data.recover_duration.as_secs_f32()
                                },
                                _ => 0.0,
                            };
                            anim::theropod::DashAnimation::update_skeleton(
                                &target_base,
                                (
                                    rel_vel.magnitude(),
                                    time,
                                    Some(s.stage_section),
                                    state.state_time,
                                ),
                                stage_progress,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        // TODO!
                        _ => target_base,
                    };

                    state.skeleton = anim::vek::Lerp::lerp(&state.skeleton, &target_bones, dt_lerp);
                    state.update(
                        renderer,
                        pos.0,
                        ori,
                        scale,
                        col,
                        dt,
                        state_animation_rate,
                        model,
                        lpindex,
                        in_frustum,
                        is_player,
                        camera,
                        &mut update_buf,
                        terrain,
                        physics.ground_vel,
                    );
                },
                Body::BirdLarge(body) => {
                    let (model, skeleton_attr) = self.bird_large_model_cache.get_or_create_model(
                        renderer,
                        &mut self.col_lights,
                        *body,
                        inventory,
                        tick,
                        player_camera_mode,
                        player_character_state,
                        &slow_jobs,
                    );

                    let state = self
                        .states
                        .bird_large_states
                        .entry(entity)
                        .or_insert_with(|| {
                            FigureState::new(renderer, BirdLargeSkeleton::default())
                        });

                    let (character, last_character) = match (character, last_character) {
                        (Some(c), Some(l)) => (c, l),
                        _ => continue,
                    };

                    if !character.same_variant(&last_character.0) {
                        state.state_time = 0.0;
                    }

                    let target_base = match (
                        physics.on_ground,
                        rel_vel.magnitude_squared() > MOVING_THRESHOLD_SQR, // Moving
                        physics.in_liquid().is_some(),                      // In water
                    ) {
                        // Standing
                        (true, false, false) => anim::bird_large::IdleAnimation::update_skeleton(
                            &BirdLargeSkeleton::default(),
                            time,
                            state.state_time,
                            &mut state_animation_rate,
                            skeleton_attr,
                        ),
                        // Running
                        (true, true, false) => anim::bird_large::RunAnimation::update_skeleton(
                            &BirdLargeSkeleton::default(),
                            (
                                rel_vel,
                                // TODO: Update to use the quaternion.
                                ori * anim::vek::Vec3::<f32>::unit_y(),
                                state.last_ori * anim::vek::Vec3::<f32>::unit_y(),
                                state.acc_vel,
                            ),
                            state.state_time,
                            &mut state_animation_rate,
                            skeleton_attr,
                        ),
                        // In air
                        (false, _, false) => anim::bird_large::FlyAnimation::update_skeleton(
                            &BirdLargeSkeleton::default(),
                            (
                                rel_vel,
                                // TODO: Update to use the quaternion.
                                ori * anim::vek::Vec3::<f32>::unit_y(),
                                state.last_ori * anim::vek::Vec3::<f32>::unit_y(),
                            ),
                            state.state_time,
                            &mut state_animation_rate,
                            skeleton_attr,
                        ),
                        // Swim
                        (_, true, _) => anim::bird_large::SwimAnimation::update_skeleton(
                            &BirdLargeSkeleton::default(),
                            time,
                            state.state_time,
                            &mut state_animation_rate,
                            skeleton_attr,
                        ),
                        // TODO!
                        _ => anim::bird_large::IdleAnimation::update_skeleton(
                            &BirdLargeSkeleton::default(),
                            time,
                            state.state_time,
                            &mut state_animation_rate,
                            skeleton_attr,
                        ),
                    };
                    let target_bones = match &character {
                        CharacterState::Sit { .. } => {
                            anim::bird_large::FeedAnimation::update_skeleton(
                                &target_base,
                                time,
                                state.state_time,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        CharacterState::BasicBeam(s) => {
                            let stage_time = s.timer.as_secs_f32();
                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time / s.static_data.buildup_duration.as_secs_f32()
                                },
                                StageSection::Cast => s.timer.as_secs_f32(),
                                StageSection::Recover => {
                                    stage_time / s.static_data.recover_duration.as_secs_f32()
                                },
                                _ => 0.0,
                            };
                            anim::bird_large::BreatheAnimation::update_skeleton(
                                &target_base,
                                (
                                    time,
                                    ori * anim::vek::Vec3::<f32>::unit_y(),
                                    state.last_ori * anim::vek::Vec3::<f32>::unit_y(),
                                    Some(s.stage_section),
                                    state.state_time,
                                    look_dir,
                                    physics.on_ground,
                                ),
                                stage_progress,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        CharacterState::ComboMelee(s) => {
                            let stage_index = (s.stage - 1) as usize;
                            let stage_time = s.timer.as_secs_f32();
                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time
                                        / s.static_data.stage_data[stage_index]
                                            .base_buildup_duration
                                            .as_secs_f32()
                                },
                                StageSection::Swing => {
                                    stage_time
                                        / s.static_data.stage_data[stage_index]
                                            .base_swing_duration
                                            .as_secs_f32()
                                },
                                StageSection::Recover => {
                                    stage_time
                                        / s.static_data.stage_data[stage_index]
                                            .base_recover_duration
                                            .as_secs_f32()
                                },
                                _ => 0.0,
                            };

                            anim::bird_large::AlphaAnimation::update_skeleton(
                                &target_base,
                                (
                                    Some(s.stage_section),
                                    ori * anim::vek::Vec3::<f32>::unit_y(),
                                    state.last_ori * anim::vek::Vec3::<f32>::unit_y(),
                                    physics.on_ground,
                                ),
                                stage_progress,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        CharacterState::BasicRanged(s) => {
                            let stage_time = s.timer.as_secs_f32();

                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time / s.static_data.buildup_duration.as_secs_f32()
                                },
                                StageSection::Recover => {
                                    stage_time / s.static_data.recover_duration.as_secs_f32()
                                },

                                _ => 0.0,
                            };
                            anim::bird_large::ShootAnimation::update_skeleton(
                                &target_base,
                                (
                                    time,
                                    Some(s.stage_section),
                                    state.state_time,
                                    look_dir,
                                    physics.on_ground,
                                ),
                                stage_progress,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        CharacterState::Shockwave(s) => {
                            let stage_time = s.timer.as_secs_f32();
                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time / s.static_data.buildup_duration.as_secs_f32()
                                },
                                StageSection::Swing => {
                                    stage_time / s.static_data.swing_duration.as_secs_f32()
                                },
                                StageSection::Recover => {
                                    stage_time / s.static_data.recover_duration.as_secs_f32()
                                },
                                _ => 0.0,
                            };
                            anim::bird_large::ShockwaveAnimation::update_skeleton(
                                &target_base,
                                (Some(s.stage_section), physics.on_ground),
                                stage_progress,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        CharacterState::Stunned(s) => {
                            let stage_time = s.timer.as_secs_f32();
                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time / s.static_data.buildup_duration.as_secs_f32()
                                },
                                StageSection::Recover => {
                                    stage_time / s.static_data.recover_duration.as_secs_f32()
                                },
                                _ => 0.0,
                            };
                            match s.static_data.poise_state {
                                PoiseState::Normal
                                | PoiseState::Interrupted
                                | PoiseState::Stunned
                                | PoiseState::Dazed
                                | PoiseState::KnockedDown => {
                                    anim::bird_large::StunnedAnimation::update_skeleton(
                                        &target_base,
                                        (time, Some(s.stage_section), state.state_time),
                                        stage_progress,
                                        &mut state_animation_rate,
                                        skeleton_attr,
                                    )
                                },
                            }
                        },
                        // TODO!
                        _ => target_base,
                    };

                    state.skeleton = anim::vek::Lerp::lerp(&state.skeleton, &target_bones, dt_lerp);
                    state.update(
                        renderer,
                        pos.0,
                        ori,
                        scale,
                        col,
                        dt,
                        state_animation_rate,
                        model,
                        lpindex,
                        in_frustum,
                        is_player,
                        camera,
                        &mut update_buf,
                        terrain,
                        physics.ground_vel,
                    );
                },
                Body::FishSmall(body) => {
                    let (model, skeleton_attr) = self.fish_small_model_cache.get_or_create_model(
                        renderer,
                        &mut self.col_lights,
                        *body,
                        inventory,
                        tick,
                        player_camera_mode,
                        player_character_state,
                        &slow_jobs,
                    );

                    let state = self
                        .states
                        .fish_small_states
                        .entry(entity)
                        .or_insert_with(|| {
                            FigureState::new(renderer, FishSmallSkeleton::default())
                        });

                    // Average velocity relative to the current ground
                    let rel_avg_vel = state.avg_vel - physics.ground_vel;

                    let (character, last_character) = match (character, last_character) {
                        (Some(c), Some(l)) => (c, l),
                        _ => continue,
                    };

                    if !character.same_variant(&last_character.0) {
                        state.state_time = 0.0;
                    }

                    let target_base = match (
                        physics.on_ground,
                        rel_vel.magnitude_squared() > MOVING_THRESHOLD_SQR, // Moving
                        physics.in_liquid().is_some(),                      // In water
                    ) {
                        // Idle
                        (_, false, _) => anim::fish_small::IdleAnimation::update_skeleton(
                            &FishSmallSkeleton::default(),
                            (
                                rel_vel,
                                // TODO: Update to use the quaternion.
                                ori * anim::vek::Vec3::<f32>::unit_y(),
                                state.last_ori * anim::vek::Vec3::<f32>::unit_y(),
                                time,
                                rel_avg_vel,
                            ),
                            state.state_time,
                            &mut state_animation_rate,
                            skeleton_attr,
                        ),
                        // Swim
                        (_, true, _) => anim::fish_small::SwimAnimation::update_skeleton(
                            &FishSmallSkeleton::default(),
                            (
                                rel_vel,
                                // TODO: Update to use the quaternion.
                                ori * anim::vek::Vec3::<f32>::unit_y(),
                                state.last_ori * anim::vek::Vec3::<f32>::unit_y(),
                                time,
                                rel_avg_vel,
                                state.acc_vel,
                            ),
                            state.state_time,
                            &mut state_animation_rate,
                            skeleton_attr,
                        ),
                    };

                    state.skeleton = anim::vek::Lerp::lerp(&state.skeleton, &target_base, dt_lerp);
                    state.update(
                        renderer,
                        pos.0,
                        ori,
                        scale,
                        col,
                        dt,
                        state_animation_rate,
                        model,
                        lpindex,
                        in_frustum,
                        is_player,
                        camera,
                        &mut update_buf,
                        terrain,
                        physics.ground_vel,
                    );
                },
                Body::BipedLarge(body) => {
                    let (model, skeleton_attr) = self.biped_large_model_cache.get_or_create_model(
                        renderer,
                        &mut self.col_lights,
                        *body,
                        inventory,
                        tick,
                        player_camera_mode,
                        player_character_state,
                        &slow_jobs,
                    );

                    let state = self
                        .states
                        .biped_large_states
                        .entry(entity)
                        .or_insert_with(|| {
                            FigureState::new(renderer, BipedLargeSkeleton::default())
                        });

                    // Average velocity relative to the current ground
                    let rel_avg_vel = state.avg_vel - physics.ground_vel;

                    let (character, last_character) = match (character, last_character) {
                        (Some(c), Some(l)) => (c, l),
                        _ => continue,
                    };

                    if !character.same_variant(&last_character.0) {
                        state.state_time = 0.0;
                    }

                    let target_base = match (
                        physics.on_ground,
                        rel_vel.magnitude_squared() > MOVING_THRESHOLD_SQR, // Moving
                        physics.in_liquid().is_some(),                      // In water
                    ) {
                        // Running
                        (true, true, false) => anim::biped_large::RunAnimation::update_skeleton(
                            &BipedLargeSkeleton::default(),
                            (
                                active_tool_kind,
                                second_tool_kind,
                                rel_vel,
                                // TODO: Update to use the quaternion.
                                ori * anim::vek::Vec3::<f32>::unit_y(),
                                state.last_ori * anim::vek::Vec3::<f32>::unit_y(),
                                time,
                                rel_avg_vel,
                                state.acc_vel,
                            ),
                            state.state_time,
                            &mut state_animation_rate,
                            skeleton_attr,
                        ),
                        // In air
                        (false, _, false) => anim::biped_large::JumpAnimation::update_skeleton(
                            &BipedLargeSkeleton::default(),
                            (active_tool_kind, second_tool_kind, time),
                            state.state_time,
                            &mut state_animation_rate,
                            skeleton_attr,
                        ),
                        _ => anim::biped_large::IdleAnimation::update_skeleton(
                            &BipedLargeSkeleton::default(),
                            (active_tool_kind, second_tool_kind, time),
                            state.state_time,
                            &mut state_animation_rate,
                            skeleton_attr,
                        ),
                    };
                    let target_bones = match &character {
                        CharacterState::Equipping { .. } => {
                            anim::biped_large::EquipAnimation::update_skeleton(
                                &target_base,
                                (
                                    active_tool_kind,
                                    second_tool_kind,
                                    rel_vel.magnitude(),
                                    time,
                                ),
                                state.state_time,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        CharacterState::Wielding { .. } => {
                            anim::biped_large::WieldAnimation::update_skeleton(
                                &target_base,
                                (
                                    (active_tool_kind, active_tool_spec),
                                    (second_tool_kind, second_tool_spec),
                                    rel_vel,
                                    time,
                                    state.acc_vel,
                                ),
                                state.state_time,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        CharacterState::ChargedMelee(s) => {
                            let stage_time = s.timer.as_secs_f32();

                            let stage_progress = match s.stage_section {
                                StageSection::Charge => {
                                    stage_time / s.static_data.charge_duration.as_secs_f32()
                                },
                                StageSection::Swing => {
                                    stage_time / s.static_data.swing_duration.as_secs_f32()
                                },
                                StageSection::Recover => {
                                    stage_time / s.static_data.recover_duration.as_secs_f32()
                                },
                                _ => 0.0,
                            };

                            anim::biped_large::ChargeMeleeAnimation::update_skeleton(
                                &target_base,
                                (
                                    (active_tool_kind, active_tool_spec),
                                    (second_tool_kind, second_tool_spec),
                                    rel_vel,
                                    time,
                                    Some(s.stage_section),
                                    state.acc_vel,
                                ),
                                stage_progress,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        CharacterState::SelfBuff(s) => {
                            let stage_time = s.timer.as_secs_f32();

                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time / s.static_data.buildup_duration.as_secs_f32()
                                },
                                StageSection::Cast => {
                                    stage_time / s.static_data.cast_duration.as_secs_f32()
                                },
                                StageSection::Recover => {
                                    stage_time / s.static_data.recover_duration.as_secs_f32()
                                },
                                _ => 0.0,
                            };

                            anim::biped_large::SelfBuffAnimation::update_skeleton(
                                &target_base,
                                (
                                    (active_tool_kind, active_tool_spec),
                                    (second_tool_kind, second_tool_spec),
                                    rel_vel,
                                    time,
                                    Some(s.stage_section),
                                    state.acc_vel,
                                ),
                                stage_progress,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        CharacterState::BasicMelee(s) => {
                            let stage_time = s.timer.as_secs_f32();

                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time / s.static_data.buildup_duration.as_secs_f32()
                                },
                                StageSection::Swing => {
                                    stage_time / s.static_data.swing_duration.as_secs_f32()
                                },
                                StageSection::Recover => {
                                    stage_time / s.static_data.recover_duration.as_secs_f32()
                                },
                                _ => 0.0,
                            };

                            anim::biped_large::AlphaAnimation::update_skeleton(
                                &target_base,
                                (
                                    (active_tool_kind, active_tool_spec),
                                    (second_tool_kind, second_tool_spec),
                                    rel_vel,
                                    time,
                                    Some(s.stage_section),
                                    state.acc_vel,
                                ),
                                stage_progress,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        CharacterState::BasicRanged(s) => {
                            let stage_time = s.timer.as_secs_f32();

                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time / s.static_data.buildup_duration.as_secs_f32()
                                },
                                StageSection::Recover => {
                                    stage_time / s.static_data.recover_duration.as_secs_f32()
                                },

                                _ => 0.0,
                            };

                            anim::biped_large::ShootAnimation::update_skeleton(
                                &target_base,
                                (
                                    (active_tool_kind, active_tool_spec),
                                    (second_tool_kind, second_tool_spec),
                                    rel_vel,
                                    // TODO: Update to use the quaternion.
                                    ori * anim::vek::Vec3::<f32>::unit_y(),
                                    state.last_ori * anim::vek::Vec3::<f32>::unit_y(),
                                    time,
                                    Some(s.stage_section),
                                    state.acc_vel,
                                ),
                                stage_progress,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        CharacterState::Stunned(s) => {
                            let stage_time = s.timer.as_secs_f32();
                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time / s.static_data.buildup_duration.as_secs_f32()
                                },
                                StageSection::Recover => {
                                    stage_time / s.static_data.recover_duration.as_secs_f32()
                                },
                                _ => 0.0,
                            };
                            match s.static_data.poise_state {
                                PoiseState::Normal
                                | PoiseState::Interrupted
                                | PoiseState::Stunned
                                | PoiseState::Dazed
                                | PoiseState::KnockedDown => {
                                    anim::biped_large::StunnedAnimation::update_skeleton(
                                        &target_base,
                                        (
                                            (active_tool_kind, active_tool_spec),
                                            rel_vel,
                                            state.acc_vel,
                                            Some(s.stage_section),
                                        ),
                                        stage_progress,
                                        &mut state_animation_rate,
                                        skeleton_attr,
                                    )
                                },
                            }
                        },
                        CharacterState::Blink(s) => {
                            let stage_time = s.timer.as_secs_f32();

                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time / s.static_data.buildup_duration.as_secs_f32()
                                },
                                StageSection::Recover => {
                                    stage_time / s.static_data.recover_duration.as_secs_f32()
                                },

                                _ => 0.0,
                            };

                            anim::biped_large::BlinkAnimation::update_skeleton(
                                &target_base,
                                (
                                    active_tool_kind,
                                    second_tool_kind,
                                    rel_vel,
                                    time,
                                    Some(s.stage_section),
                                    state.acc_vel,
                                ),
                                stage_progress,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },

                        CharacterState::ChargedRanged(s) => {
                            let stage_time = s.timer.as_secs_f32();

                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time / s.static_data.buildup_duration.as_secs_f32()
                                },
                                StageSection::Recover => {
                                    stage_time / s.static_data.recover_duration.as_secs_f32()
                                },

                                _ => 0.0,
                            };

                            anim::biped_large::ShootAnimation::update_skeleton(
                                &target_base,
                                (
                                    (active_tool_kind, active_tool_spec),
                                    (second_tool_kind, second_tool_spec),
                                    rel_vel,
                                    // TODO: Update to use the quaternion.
                                    ori * anim::vek::Vec3::<f32>::unit_y(),
                                    state.last_ori * anim::vek::Vec3::<f32>::unit_y(),
                                    time,
                                    Some(s.stage_section),
                                    state.acc_vel,
                                ),
                                stage_progress,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        CharacterState::DashMelee(s) => {
                            let stage_time = s.timer.as_secs_f32();
                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time / s.static_data.buildup_duration.as_secs_f32()
                                },
                                StageSection::Charge => {
                                    stage_time / s.static_data.charge_duration.as_secs_f32()
                                },
                                StageSection::Swing => {
                                    stage_time / s.static_data.swing_duration.as_secs_f32()
                                },
                                StageSection::Recover => {
                                    stage_time / s.static_data.recover_duration.as_secs_f32()
                                },
                                _ => 0.0,
                            };
                            anim::biped_large::DashAnimation::update_skeleton(
                                &target_base,
                                (
                                    (active_tool_kind, active_tool_spec),
                                    (second_tool_kind, second_tool_spec),
                                    rel_vel,
                                    time,
                                    Some(s.stage_section),
                                    state.acc_vel,
                                ),
                                stage_progress,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        CharacterState::ComboMelee(s) => {
                            let stage_index = (s.stage - 1) as usize;
                            let stage_time = s.timer.as_secs_f32();
                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time
                                        / s.static_data.stage_data[stage_index]
                                            .base_buildup_duration
                                            .as_secs_f32()
                                },
                                StageSection::Swing => {
                                    stage_time
                                        / s.static_data.stage_data[stage_index]
                                            .base_swing_duration
                                            .as_secs_f32()
                                },
                                StageSection::Recover => {
                                    stage_time
                                        / s.static_data.stage_data[stage_index]
                                            .base_recover_duration
                                            .as_secs_f32()
                                },
                                _ => 0.0,
                            };
                            match s.stage {
                                1 => anim::biped_large::AlphaAnimation::update_skeleton(
                                    &target_base,
                                    (
                                        (active_tool_kind, active_tool_spec),
                                        (second_tool_kind, second_tool_spec),
                                        rel_vel,
                                        time,
                                        Some(s.stage_section),
                                        state.acc_vel,
                                    ),
                                    stage_progress,
                                    &mut state_animation_rate,
                                    skeleton_attr,
                                ),
                                2 => anim::biped_large::BetaAnimation::update_skeleton(
                                    &target_base,
                                    (
                                        (active_tool_kind, active_tool_spec),
                                        (second_tool_kind, second_tool_spec),
                                        rel_vel,
                                        time,
                                        Some(s.stage_section),
                                        state.acc_vel,
                                    ),
                                    stage_progress,
                                    &mut state_animation_rate,
                                    skeleton_attr,
                                ),
                                _ => anim::biped_large::BetaAnimation::update_skeleton(
                                    &target_base,
                                    (
                                        (active_tool_kind, active_tool_spec),
                                        (second_tool_kind, second_tool_spec),
                                        rel_vel,
                                        time,
                                        Some(s.stage_section),
                                        state.acc_vel,
                                    ),
                                    stage_progress,
                                    &mut state_animation_rate,
                                    skeleton_attr,
                                ),
                            }
                        },
                        CharacterState::SpinMelee(s) => {
                            let stage_time = s.timer.as_secs_f32();
                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time / s.static_data.buildup_duration.as_secs_f32()
                                },

                                StageSection::Swing => {
                                    stage_time / s.static_data.swing_duration.as_secs_f32()
                                },
                                StageSection::Recover => {
                                    stage_time / s.static_data.recover_duration.as_secs_f32()
                                },
                                _ => 0.0,
                            };

                            anim::biped_large::SpinMeleeAnimation::update_skeleton(
                                &target_base,
                                (
                                    active_tool_kind,
                                    second_tool_kind,
                                    rel_vel,
                                    time,
                                    Some(s.stage_section),
                                    state.acc_vel,
                                ),
                                stage_progress,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        CharacterState::BasicSummon(s) => {
                            let stage_time = s.timer.as_secs_f32();
                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time / s.static_data.buildup_duration.as_secs_f32()
                                },

                                StageSection::Cast => {
                                    stage_time / s.static_data.cast_duration.as_secs_f32()
                                },
                                StageSection::Recover => {
                                    stage_time / s.static_data.recover_duration.as_secs_f32()
                                },
                                _ => 0.0,
                            };

                            anim::biped_large::SummonAnimation::update_skeleton(
                                &target_base,
                                (
                                    active_tool_kind,
                                    second_tool_kind,
                                    rel_vel,
                                    time,
                                    Some(s.stage_section),
                                    state.acc_vel,
                                ),
                                stage_progress,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        CharacterState::LeapMelee(s) => {
                            let stage_progress = match active_tool_kind {
                                Some(ToolKind::Axe | ToolKind::Hammer) => {
                                    let stage_time = s.timer.as_secs_f32();
                                    match s.stage_section {
                                        StageSection::Buildup => {
                                            stage_time
                                                / s.static_data.buildup_duration.as_secs_f32()
                                        },
                                        StageSection::Movement => {
                                            stage_time
                                                / s.static_data.movement_duration.as_secs_f32()
                                        },
                                        StageSection::Swing => {
                                            stage_time / s.static_data.swing_duration.as_secs_f32()
                                        },
                                        StageSection::Recover => {
                                            stage_time
                                                / s.static_data.recover_duration.as_secs_f32()
                                        },
                                        _ => 0.0,
                                    }
                                },
                                _ => state.state_time,
                            };

                            anim::biped_large::LeapAnimation::update_skeleton(
                                &target_base,
                                (
                                    active_tool_kind,
                                    second_tool_kind,
                                    rel_vel,
                                    time,
                                    Some(s.stage_section),
                                ),
                                stage_progress,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        CharacterState::Shockwave(s) => {
                            let stage_time = s.timer.as_secs_f32();
                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time / s.static_data.buildup_duration.as_secs_f32()
                                },
                                StageSection::Swing => {
                                    stage_time / s.static_data.swing_duration.as_secs_f32()
                                },
                                StageSection::Recover => {
                                    stage_time / s.static_data.recover_duration.as_secs_f32()
                                },
                                _ => 0.0,
                            };
                            anim::biped_large::ShockwaveAnimation::update_skeleton(
                                &target_base,
                                (
                                    active_tool_kind,
                                    second_tool_kind,
                                    time,
                                    rel_vel.magnitude(),
                                    Some(s.stage_section),
                                ),
                                stage_progress,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        CharacterState::BasicBeam(s) => {
                            let stage_time = s.timer.as_secs_f32();
                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time / s.static_data.buildup_duration.as_secs_f32()
                                },
                                StageSection::Cast => s.timer.as_secs_f32(),
                                StageSection::Recover => {
                                    stage_time / s.static_data.recover_duration.as_secs_f32()
                                },
                                _ => 0.0,
                            };
                            anim::biped_large::BeamAnimation::update_skeleton(
                                &target_base,
                                (
                                    active_tool_kind,
                                    second_tool_kind,
                                    time,
                                    rel_vel,
                                    Some(s.stage_section),
                                    state.acc_vel,
                                ),
                                stage_progress,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        // TODO!
                        _ => target_base,
                    };

                    state.skeleton = anim::vek::Lerp::lerp(&state.skeleton, &target_bones, dt_lerp);
                    state.update(
                        renderer,
                        pos.0,
                        ori,
                        scale,
                        col,
                        dt,
                        state_animation_rate,
                        model,
                        lpindex,
                        in_frustum,
                        is_player,
                        camera,
                        &mut update_buf,
                        terrain,
                        physics.ground_vel,
                    );
                },
                Body::Golem(body) => {
                    let (model, skeleton_attr) = self.golem_model_cache.get_or_create_model(
                        renderer,
                        &mut self.col_lights,
                        *body,
                        inventory,
                        tick,
                        player_camera_mode,
                        player_character_state,
                        &slow_jobs,
                    );

                    let state =
                        self.states.golem_states.entry(entity).or_insert_with(|| {
                            FigureState::new(renderer, GolemSkeleton::default())
                        });

                    // Average velocity relative to the current ground
                    let _rel_avg_vel = state.avg_vel - physics.ground_vel;

                    let (character, last_character) = match (character, last_character) {
                        (Some(c), Some(l)) => (c, l),
                        _ => continue,
                    };

                    if !character.same_variant(&last_character.0) {
                        state.state_time = 0.0;
                    }

                    let target_base = match (
                        physics.on_ground,
                        rel_vel.magnitude_squared() > MOVING_THRESHOLD_SQR, // Moving
                        physics.in_liquid().is_some(),                      // In water
                    ) {
                        // Standing
                        (true, false, false) => anim::golem::IdleAnimation::update_skeleton(
                            &GolemSkeleton::default(),
                            time,
                            state.state_time,
                            &mut state_animation_rate,
                            skeleton_attr,
                        ),
                        // Running
                        (true, true, false) => anim::golem::RunAnimation::update_skeleton(
                            &GolemSkeleton::default(),
                            (
                                rel_vel,
                                // TODO: Update to use the quaternion.
                                ori * anim::vek::Vec3::<f32>::unit_y(),
                                state.last_ori * anim::vek::Vec3::<f32>::unit_y(),
                                time,
                                state.acc_vel,
                            ),
                            state.state_time,
                            &mut state_animation_rate,
                            skeleton_attr,
                        ),
                        // In air
                        (false, _, false) => anim::golem::RunAnimation::update_skeleton(
                            &GolemSkeleton::default(),
                            (
                                rel_vel,
                                ori * anim::vek::Vec3::<f32>::unit_y(),
                                state.last_ori * anim::vek::Vec3::<f32>::unit_y(),
                                time,
                                state.acc_vel,
                            ),
                            state.state_time,
                            &mut state_animation_rate,
                            skeleton_attr,
                        ),

                        _ => anim::golem::IdleAnimation::update_skeleton(
                            &GolemSkeleton::default(),
                            time,
                            state.state_time,
                            &mut state_animation_rate,
                            skeleton_attr,
                        ),
                    };
                    let target_bones = match &character {
                        CharacterState::ComboMelee(s) => {
                            let stage_index = (s.stage - 1) as usize;
                            let stage_time = s.timer.as_secs_f32();
                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time
                                        / s.static_data.stage_data[stage_index]
                                            .base_buildup_duration
                                            .as_secs_f32()
                                },
                                StageSection::Swing => {
                                    stage_time
                                        / s.static_data.stage_data[stage_index]
                                            .base_swing_duration
                                            .as_secs_f32()
                                },
                                StageSection::Recover => {
                                    stage_time
                                        / s.static_data.stage_data[stage_index]
                                            .base_recover_duration
                                            .as_secs_f32()
                                },
                                _ => 0.0,
                            };

                            anim::golem::AlphaAnimation::update_skeleton(
                                &target_base,
                                (Some(s.stage_section), time, state.state_time),
                                stage_progress,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        CharacterState::BasicRanged(s) => {
                            let stage_time = s.timer.as_secs_f32();

                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time / s.static_data.buildup_duration.as_secs_f32()
                                },
                                StageSection::Recover => {
                                    stage_time / s.static_data.recover_duration.as_secs_f32()
                                },

                                _ => 0.0,
                            };

                            anim::golem::ShootAnimation::update_skeleton(
                                &target_base,
                                (Some(s.stage_section), time, state.state_time, look_dir),
                                stage_progress,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        CharacterState::BasicBeam(s) => {
                            let stage_time = s.timer.as_secs_f32();

                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time / s.static_data.buildup_duration.as_secs_f32()
                                },
                                StageSection::Cast => s.timer.as_secs_f32(),
                                StageSection::Recover => {
                                    stage_time / s.static_data.recover_duration.as_secs_f32()
                                },

                                _ => 0.0,
                            };

                            anim::golem::BeamAnimation::update_skeleton(
                                &target_base,
                                (Some(s.stage_section), time, state.state_time, look_dir),
                                stage_progress,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        CharacterState::BasicMelee(s) => {
                            let stage_time = s.timer.as_secs_f32();
                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time / s.static_data.buildup_duration.as_secs_f32()
                                },
                                StageSection::Swing => {
                                    stage_time / s.static_data.swing_duration.as_secs_f32()
                                },
                                StageSection::Recover => {
                                    stage_time / s.static_data.recover_duration.as_secs_f32()
                                },
                                _ => 0.0,
                            };

                            anim::golem::AlphaAnimation::update_skeleton(
                                &target_base,
                                (Some(s.stage_section), time, state.state_time),
                                stage_progress,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        CharacterState::Shockwave(s) => {
                            let stage_time = s.timer.as_secs_f32();
                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time / s.static_data.buildup_duration.as_secs_f32()
                                },
                                StageSection::Swing => {
                                    stage_time / s.static_data.swing_duration.as_secs_f32()
                                },
                                StageSection::Recover => {
                                    stage_time / s.static_data.recover_duration.as_secs_f32()
                                },
                                _ => 0.0,
                            };
                            anim::golem::ShockwaveAnimation::update_skeleton(
                                &target_base,
                                (Some(s.stage_section), rel_vel.magnitude(), time),
                                stage_progress,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        CharacterState::SpinMelee(s) => {
                            let stage_progress = {
                                let stage_time = s.timer.as_secs_f32();
                                match s.stage_section {
                                    StageSection::Buildup => {
                                        stage_time / s.static_data.buildup_duration.as_secs_f32()
                                    },
                                    StageSection::Swing => {
                                        stage_time / s.static_data.swing_duration.as_secs_f32()
                                    },
                                    StageSection::Recover => {
                                        stage_time / s.static_data.recover_duration.as_secs_f32()
                                    },
                                    _ => 0.0,
                                }
                            };

                            anim::golem::SpinMeleeAnimation::update_skeleton(
                                &target_base,
                                Some(s.stage_section),
                                stage_progress,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        // TODO!
                        _ => target_base,
                    };

                    state.skeleton = anim::vek::Lerp::lerp(&state.skeleton, &target_bones, dt_lerp);
                    state.update(
                        renderer,
                        pos.0,
                        ori,
                        scale,
                        col,
                        dt,
                        state_animation_rate,
                        model,
                        lpindex,
                        in_frustum,
                        is_player,
                        camera,
                        &mut update_buf,
                        terrain,
                        physics.ground_vel,
                    );
                },
                Body::Object(body) => {
                    let (model, skeleton_attr) = self.object_model_cache.get_or_create_model(
                        renderer,
                        &mut self.col_lights,
                        *body,
                        inventory,
                        tick,
                        player_camera_mode,
                        player_character_state,
                        &slow_jobs,
                    );

                    let state =
                        self.states.object_states.entry(entity).or_insert_with(|| {
                            FigureState::new(renderer, ObjectSkeleton::default())
                        });

                    // Average velocity relative to the current ground
                    let _rel_avg_vel = state.avg_vel - physics.ground_vel;

                    let (character, last_character) = match (character, last_character) {
                        (Some(c), Some(l)) => (c, l),
                        _ => (&CharacterState::Idle, &Last {
                            0: CharacterState::Idle,
                        }),
                    };

                    if !character.same_variant(&last_character.0) {
                        state.state_time = 0.0;
                    }

                    let target_base = match (
                        physics.on_ground,
                        rel_vel.magnitude_squared() > MOVING_THRESHOLD_SQR, // Moving
                        physics.in_liquid().is_some(),                      // In water
                    ) {
                        // Standing
                        (true, false, false) => anim::object::IdleAnimation::update_skeleton(
                            &ObjectSkeleton::default(),
                            (active_tool_kind, second_tool_kind, time),
                            state.state_time,
                            &mut state_animation_rate,
                            skeleton_attr,
                        ),
                        _ => anim::object::IdleAnimation::update_skeleton(
                            &ObjectSkeleton::default(),
                            (active_tool_kind, second_tool_kind, time),
                            state.state_time,
                            &mut state_animation_rate,
                            skeleton_attr,
                        ),
                    };

                    let target_bones = match &character {
                        CharacterState::BasicRanged(s) => {
                            let stage_time = s.timer.as_secs_f32();

                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time / s.static_data.buildup_duration.as_secs_f32()
                                },
                                StageSection::Recover => {
                                    stage_time / s.static_data.recover_duration.as_secs_f32()
                                },

                                _ => 0.0,
                            };
                            anim::object::ShootAnimation::update_skeleton(
                                &target_base,
                                (
                                    active_tool_kind,
                                    second_tool_kind,
                                    Some(s.stage_section),
                                    *body,
                                ),
                                stage_progress,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        CharacterState::BasicBeam(s) => {
                            let stage_time = s.timer.as_secs_f32();
                            let stage_progress = match s.stage_section {
                                StageSection::Buildup => {
                                    stage_time / s.static_data.buildup_duration.as_secs_f32()
                                },
                                StageSection::Cast => s.timer.as_secs_f32(),
                                StageSection::Recover => {
                                    stage_time / s.static_data.recover_duration.as_secs_f32()
                                },
                                _ => 0.0,
                            };
                            anim::object::BeamAnimation::update_skeleton(
                                &target_base,
                                (
                                    active_tool_kind,
                                    second_tool_kind,
                                    Some(s.stage_section),
                                    *body,
                                ),
                                stage_progress,
                                &mut state_animation_rate,
                                skeleton_attr,
                            )
                        },
                        // TODO!
                        _ => target_base,
                    };

                    state.skeleton = anim::vek::Lerp::lerp(&state.skeleton, &target_bones, dt_lerp);
                    state.update(
                        renderer,
                        pos.0,
                        ori,
                        scale,
                        col,
                        dt,
                        state_animation_rate,
                        model,
                        lpindex,
                        true,
                        is_player,
                        camera,
                        &mut update_buf,
                        terrain,
                        physics.ground_vel,
                    );
                },
                Body::Ship(body) => {
                    let (model, skeleton_attr) = self.ship_model_cache.get_or_create_model(
                        renderer,
                        &mut self.col_lights,
                        *body,
                        inventory,
                        tick,
                        player_camera_mode,
                        player_character_state,
                        &slow_jobs,
                    );

                    let state = self
                        .states
                        .ship_states
                        .entry(entity)
                        .or_insert_with(|| FigureState::new(renderer, ShipSkeleton::default()));

                    // Average velocity relative to the current ground
                    let _rel_avg_vel = state.avg_vel - physics.ground_vel;

                    let (character, last_character) = match (character, last_character) {
                        (Some(c), Some(l)) => (c, l),
                        _ => (&CharacterState::Idle, &Last {
                            0: CharacterState::Idle,
                        }),
                    };

                    if !character.same_variant(&last_character.0) {
                        state.state_time = 0.0;
                    }

                    let target_base = match (
                        physics.on_ground,
                        rel_vel.magnitude_squared() > MOVING_THRESHOLD_SQR, // Moving
                        physics.in_liquid().is_some(),                      // In water
                    ) {
                        // Standing
                        (true, false, false) => anim::ship::IdleAnimation::update_skeleton(
                            &ShipSkeleton::default(),
                            (
                                active_tool_kind,
                                second_tool_kind,
                                time,
                                state.acc_vel,
                                ori * anim::vek::Vec3::<f32>::unit_y(),
                                state.last_ori * anim::vek::Vec3::<f32>::unit_y(),
                            ),
                            state.state_time,
                            &mut state_animation_rate,
                            skeleton_attr,
                        ),
                        _ => anim::ship::IdleAnimation::update_skeleton(
                            &ShipSkeleton::default(),
                            (
                                active_tool_kind,
                                second_tool_kind,
                                time,
                                state.acc_vel,
                                ori * anim::vek::Vec3::<f32>::unit_y(),
                                state.last_ori * anim::vek::Vec3::<f32>::unit_y(),
                            ),
                            state.state_time,
                            &mut state_animation_rate,
                            skeleton_attr,
                        ),
                    };

                    #[allow(clippy::match_single_binding)]
                    let target_bones = match &character {
                        // TODO!
                        _ => target_base,
                    };

                    state.skeleton = anim::vek::Lerp::lerp(&state.skeleton, &target_bones, dt_lerp);
                    state.update(
                        renderer,
                        pos.0,
                        ori,
                        scale,
                        col,
                        dt,
                        state_animation_rate,
                        model,
                        lpindex,
                        true,
                        is_player,
                        camera,
                        &mut update_buf,
                        terrain,
                        physics.ground_vel,
                    );
                },
            }
        }

        // Update lighting (lanterns) for figures
        self.update_lighting(scene_data);

        // Clear states that have deleted entities.
        self.states
            .retain(|entity, _| ecs.entities().is_alive(*entity));

        visible_aabb
    }

    pub fn render_shadows(
        &self,
        renderer: &mut Renderer,
        state: &State,
        tick: u64,
        global: &GlobalModel,
        (is_daylight, _light_data): super::LightData,
        (camera, figure_lod_render_distance): CameraData,
    ) {
        span!(_guard, "render_shadows", "FigureManager::render_shadows");
        let ecs = state.ecs();

        if is_daylight && renderer.render_mode().shadow.is_map() {
            (
                &ecs.entities(),
                &ecs.read_storage::<Pos>(),
                ecs.read_storage::<Ori>().maybe(),
                &ecs.read_storage::<Body>(),
                ecs.read_storage::<Health>().maybe(),
                ecs.read_storage::<Inventory>().maybe(),
                ecs.read_storage::<Scale>().maybe(),
            )
            .join()
            // Don't render dead entities
            .filter(|(_, _, _, _, health, _, _)| health.map_or(true, |h| !h.is_dead))
            .for_each(|(entity, pos, _, body, _, inventory, scale)| {
                if let Some((locals, bone_consts, model, _)) = self.get_model_for_render(
                    tick,
                    camera,
                    None,
                    entity,
                    body,
                    inventory,
                    false,
                    pos.0,
                    figure_lod_render_distance * scale.map_or(1.0, |s| s.0),
                    |state| state.can_shadow_sun(),
                ) {
                    renderer.render_figure_shadow_directed(
                        model,
                        global,
                        locals,
                        bone_consts,
                        &global.shadow_mats,
                    );
                }
            });
        }
    }

    #[allow(clippy::too_many_arguments)] // TODO: Pending review in #587
    pub fn render(
        &self,
        renderer: &mut Renderer,
        state: &State,
        player_entity: EcsEntity,
        tick: u64,
        global: &GlobalModel,
        lod: &LodData,
        (camera, figure_lod_render_distance): CameraData,
    ) {
        span!(_guard, "render", "FigureManager::render");
        let ecs = state.ecs();

        let character_state_storage = state.read_storage::<common::comp::CharacterState>();
        let character_state = character_state_storage.get(player_entity);

        for (entity, pos, _, body, _, inventory, scale) in (
            &ecs.entities(),
            &ecs.read_storage::<Pos>(),
            ecs.read_storage::<Ori>().maybe(),
            &ecs.read_storage::<Body>(),
            ecs.read_storage::<Health>().maybe(),
            ecs.read_storage::<Inventory>().maybe(),
            ecs.read_storage::<Scale>().maybe(),
        )
            .join()
        // Don't render dead entities
        .filter(|(_, _, _, _, health, _, _)| health.map_or(true, |h| !h.is_dead))
        {
            let is_player = entity == player_entity;

            if !is_player {
                if let Some((locals, bone_consts, model, col_lights)) = self.get_model_for_render(
                    tick,
                    camera,
                    character_state,
                    entity,
                    body,
                    inventory,
                    false,
                    pos.0,
                    figure_lod_render_distance * scale.map_or(1.0, |s| s.0),
                    |state| state.visible(),
                ) {
                    renderer.render_figure(model, &col_lights, global, locals, bone_consts, lod);
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)] // TODO: Pending review in #587
    pub fn render_player(
        &self,
        renderer: &mut Renderer,
        state: &State,
        player_entity: EcsEntity,
        tick: u64,
        global: &GlobalModel,
        lod: &LodData,
        (camera, figure_lod_render_distance): CameraData,
    ) {
        span!(_guard, "render_player", "FigureManager::render_player");
        let ecs = state.ecs();

        let character_state_storage = state.read_storage::<common::comp::CharacterState>();
        let character_state = character_state_storage.get(player_entity);

        if let (Some(pos), Some(body)) = (
            ecs.read_storage::<Pos>().get(player_entity),
            ecs.read_storage::<Body>().get(player_entity),
        ) {
            let healths = state.read_storage::<Health>();
            let health = healths.get(player_entity);
            if health.map_or(false, |h| h.is_dead) {
                return;
            }

            let inventory_storage = ecs.read_storage::<Inventory>();
            let inventory = inventory_storage.get(player_entity);

            if let Some((locals, bone_consts, model, col_lights)) = self.get_model_for_render(
                tick,
                camera,
                character_state,
                player_entity,
                body,
                inventory,
                true,
                pos.0,
                figure_lod_render_distance,
                |state| state.visible(),
            ) {
                renderer.render_player(model, &col_lights, global, locals, bone_consts, lod);
                renderer.render_player_shadow(
                    model,
                    &col_lights,
                    global,
                    bone_consts,
                    lod,
                    &global.shadow_mats,
                );
            }
        }
    }

    #[allow(clippy::too_many_arguments)] // TODO: Pending review in #587
    fn get_model_for_render(
        &self,
        tick: u64,
        camera: &Camera,
        character_state: Option<&CharacterState>,
        entity: EcsEntity,
        body: &Body,
        inventory: Option<&Inventory>,
        is_player: bool,
        pos: vek::Vec3<f32>,
        figure_lod_render_distance: f32,
        filter_state: impl Fn(&FigureStateMeta) -> bool,
    ) -> Option<FigureModelRef> {
        let player_camera_mode = if is_player {
            camera.get_mode()
        } else {
            CameraMode::default()
        };
        let focus_pos = camera.get_focus_pos();
        let cam_pos = camera.dependents().cam_pos + focus_pos.map(|e| e.trunc());
        let character_state = if is_player { character_state } else { None };

        let FigureMgr {
            col_lights: ref col_lights_,
            model_cache,
            theropod_model_cache,
            quadruped_small_model_cache,
            quadruped_medium_model_cache,
            quadruped_low_model_cache,
            bird_medium_model_cache,
            bird_large_model_cache,
            dragon_model_cache,
            fish_medium_model_cache,
            fish_small_model_cache,
            biped_large_model_cache,
            biped_small_model_cache,
            object_model_cache,
            ship_model_cache,
            golem_model_cache,
            states:
                FigureMgrStates {
                    character_states,
                    quadruped_small_states,
                    quadruped_medium_states,
                    quadruped_low_states,
                    bird_medium_states,
                    fish_medium_states,
                    theropod_states,
                    dragon_states,
                    bird_large_states,
                    fish_small_states,
                    biped_large_states,
                    biped_small_states,
                    golem_states,
                    object_states,
                    ship_states,
                },
        } = self;
        let col_lights = &*col_lights_;
        if let Some((locals, bone_consts, model_entry)) = match body {
            Body::Humanoid(body) => character_states
                .get(&entity)
                .filter(|state| filter_state(&*state))
                .map(move |state| {
                    (
                        state.locals(),
                        state.bone_consts(),
                        model_cache.get_model(
                            col_lights,
                            *body,
                            inventory,
                            tick,
                            player_camera_mode,
                            character_state,
                        ),
                    )
                }),
            Body::QuadrupedSmall(body) => quadruped_small_states
                .get(&entity)
                .filter(|state| filter_state(&*state))
                .map(move |state| {
                    (
                        state.locals(),
                        state.bone_consts(),
                        quadruped_small_model_cache.get_model(
                            col_lights,
                            *body,
                            inventory,
                            tick,
                            player_camera_mode,
                            character_state,
                        ),
                    )
                }),
            Body::QuadrupedMedium(body) => quadruped_medium_states
                .get(&entity)
                .filter(|state| filter_state(&*state))
                .map(move |state| {
                    (
                        state.locals(),
                        state.bone_consts(),
                        quadruped_medium_model_cache.get_model(
                            col_lights,
                            *body,
                            inventory,
                            tick,
                            player_camera_mode,
                            character_state,
                        ),
                    )
                }),
            Body::QuadrupedLow(body) => quadruped_low_states
                .get(&entity)
                .filter(|state| filter_state(&*state))
                .map(move |state| {
                    (
                        state.locals(),
                        state.bone_consts(),
                        quadruped_low_model_cache.get_model(
                            col_lights,
                            *body,
                            inventory,
                            tick,
                            player_camera_mode,
                            character_state,
                        ),
                    )
                }),
            Body::BirdMedium(body) => bird_medium_states
                .get(&entity)
                .filter(|state| filter_state(&*state))
                .map(move |state| {
                    (
                        state.locals(),
                        state.bone_consts(),
                        bird_medium_model_cache.get_model(
                            col_lights,
                            *body,
                            inventory,
                            tick,
                            player_camera_mode,
                            character_state,
                        ),
                    )
                }),
            Body::FishMedium(body) => fish_medium_states
                .get(&entity)
                .filter(|state| filter_state(&*state))
                .map(move |state| {
                    (
                        state.locals(),
                        state.bone_consts(),
                        fish_medium_model_cache.get_model(
                            col_lights,
                            *body,
                            inventory,
                            tick,
                            player_camera_mode,
                            character_state,
                        ),
                    )
                }),
            Body::Theropod(body) => theropod_states
                .get(&entity)
                .filter(|state| filter_state(&*state))
                .map(move |state| {
                    (
                        state.locals(),
                        state.bone_consts(),
                        theropod_model_cache.get_model(
                            col_lights,
                            *body,
                            inventory,
                            tick,
                            player_camera_mode,
                            character_state,
                        ),
                    )
                }),
            Body::Dragon(body) => dragon_states
                .get(&entity)
                .filter(|state| filter_state(&*state))
                .map(move |state| {
                    (
                        state.locals(),
                        state.bone_consts(),
                        dragon_model_cache.get_model(
                            col_lights,
                            *body,
                            inventory,
                            tick,
                            player_camera_mode,
                            character_state,
                        ),
                    )
                }),
            Body::BirdLarge(body) => bird_large_states
                .get(&entity)
                .filter(|state| filter_state(&*state))
                .map(move |state| {
                    (
                        state.locals(),
                        state.bone_consts(),
                        bird_large_model_cache.get_model(
                            col_lights,
                            *body,
                            inventory,
                            tick,
                            player_camera_mode,
                            character_state,
                        ),
                    )
                }),
            Body::FishSmall(body) => fish_small_states
                .get(&entity)
                .filter(|state| filter_state(&*state))
                .map(move |state| {
                    (
                        state.locals(),
                        state.bone_consts(),
                        fish_small_model_cache.get_model(
                            col_lights,
                            *body,
                            inventory,
                            tick,
                            player_camera_mode,
                            character_state,
                        ),
                    )
                }),
            Body::BipedLarge(body) => biped_large_states
                .get(&entity)
                .filter(|state| filter_state(&*state))
                .map(move |state| {
                    (
                        state.locals(),
                        state.bone_consts(),
                        biped_large_model_cache.get_model(
                            col_lights,
                            *body,
                            inventory,
                            tick,
                            player_camera_mode,
                            character_state,
                        ),
                    )
                }),
            Body::BipedSmall(body) => biped_small_states
                .get(&entity)
                .filter(|state| filter_state(&*state))
                .map(move |state| {
                    (
                        state.locals(),
                        state.bone_consts(),
                        biped_small_model_cache.get_model(
                            col_lights,
                            *body,
                            inventory,
                            tick,
                            player_camera_mode,
                            character_state,
                        ),
                    )
                }),
            Body::Golem(body) => golem_states
                .get(&entity)
                .filter(|state| filter_state(&*state))
                .map(move |state| {
                    (
                        state.locals(),
                        state.bone_consts(),
                        golem_model_cache.get_model(
                            col_lights,
                            *body,
                            inventory,
                            tick,
                            player_camera_mode,
                            character_state,
                        ),
                    )
                }),
            Body::Object(body) => object_states
                .get(&entity)
                .filter(|state| filter_state(&*state))
                .map(move |state| {
                    (
                        state.locals(),
                        state.bone_consts(),
                        object_model_cache.get_model(
                            col_lights,
                            *body,
                            inventory,
                            tick,
                            player_camera_mode,
                            character_state,
                        ),
                    )
                }),
            Body::Ship(body) => ship_states
                .get(&entity)
                .filter(|state| filter_state(&*state))
                .map(move |state| {
                    (
                        state.locals(),
                        state.bone_consts(),
                        ship_model_cache.get_model(
                            col_lights,
                            *body,
                            inventory,
                            tick,
                            player_camera_mode,
                            character_state,
                        ),
                    )
                }),
        } {
            let model_entry = model_entry?;

            let figure_low_detail_distance = figure_lod_render_distance * 0.75;
            let figure_mid_detail_distance = figure_lod_render_distance * 0.5;

            let model = if pos.distance_squared(cam_pos) > figure_low_detail_distance.powi(2) {
                &model_entry.models[2]
            } else if pos.distance_squared(cam_pos) > figure_mid_detail_distance.powi(2) {
                &model_entry.models[1]
            } else {
                &model_entry.models[0]
            };

            Some((locals, bone_consts, model, col_lights_.texture(model_entry)))
        } else {
            // trace!("Body has no saved figure");
            None
        }
    }

    pub fn figure_count(&self) -> usize { self.states.count() }

    pub fn figure_count_visible(&self) -> usize { self.states.count_visible() }
}

pub struct FigureColLights {
    atlas: AtlasAllocator,
    // col_lights: Texture<ColLightFmt>,
}

impl FigureColLights {
    pub fn new(renderer: &mut Renderer) -> Self {
        let atlas = Self::make_atlas(renderer).expect("Failed to create texture atlas for figures");
        Self {
            atlas, /* col_lights, */
        }
    }

    /// Find the correct texture for this model entry.
    pub fn texture<'a, const N: usize>(&'a self, model: &'a FigureModelEntry<N>) -> &'a Texture /* <ColLightFmt> */
    {
        /* &self.col_lights */
        &model.col_lights
    }

    /// NOTE: Panics if the opaque model's length does not fit in a u32.
    /// This is part of the function contract.
    ///
    /// NOTE: Panics if the vertex range bounds are not in range of the opaque
    /// model stored in the BoneMeshes parameter.  This is part of the
    /// function contract.
    pub fn create_figure<const N: usize>(
        &mut self,
        renderer: &mut Renderer,
        (tex, tex_size): ColLightInfo,
        (opaque, bounds): (Mesh<TerrainVertex>, math::Aabb<f32>),
        vertex_range: [Range<u32>; N],
    ) -> Result<FigureModelEntry<N>, RenderError> {
        span!(_guard, "create_figure", "FigureColLights::create_figure");
        let atlas = &mut self.atlas;
        let allocation = atlas
            .allocate(guillotiere::Size::new(tex_size.x as i32, tex_size.y as i32))
            .expect("Not yet implemented: allocate new atlas on allocation failure.");
        let col_lights = pipelines::shadow::create_col_lights(renderer, &(tex, tex_size));
        let model_len = u32::try_from(opaque.vertices().len())
            .expect("The model size for this figure does not fit in a u32!");
        let model = renderer.create_model(&opaque)?;

        Ok(FigureModelEntry {
            _bounds: bounds,
            models: vertex_range.map(|range| {
                assert!(
                    range.start <= range.end && range.end <= model_len,
                    "The provided vertex range for figure mesh {:?} does not fit in the model, \
                     which is of size {:?}!",
                    range,
                    model_len
                );
                FigureModel {
                    opaque: model.submodel(range),
                }
            }),
            col_lights,
            allocation,
        })
    }

    #[allow(clippy::unnecessary_wraps)]
    fn make_atlas(renderer: &mut Renderer) -> Result<AtlasAllocator, RenderError> {
        let max_texture_size = renderer.max_texture_size();
        let atlas_size = guillotiere::Size::new(max_texture_size as i32, max_texture_size as i32);
        let atlas = AtlasAllocator::with_options(atlas_size, &guillotiere::AllocatorOptions {
            // TODO: Verify some good empirical constants.
            small_size_threshold: 32,
            large_size_threshold: 256,
            ..guillotiere::AllocatorOptions::default()
        });
        // TODO: Consider using a single texture atlas to store all figures, much like
        // we do for terrain chunks.  We previously avoided this due to
        // perceived performance degradation for the figure use case, but with a
        // smaller atlas size this may be less likely.
        /* let texture = renderer.create_texture_raw(
            gfx::texture::Kind::D2(
                max_texture_size,
                max_texture_size,
                gfx::texture::AaMode::Single,
            ),
            1 as gfx::texture::Level,
            gfx::memory::Bind::SHADER_RESOURCE,
            gfx::memory::Usage::Dynamic,
            (0, 0),
            gfx::format::Swizzle::new(),
            gfx::texture::SamplerInfo::new(
                gfx::texture::FilterMetho>:Bilinear,
                gfx::texture::WrapMode::Clamp,
            ),
        )?;
        Ok((atlas, texture)) */
        Ok(atlas)
    }
}

pub struct FigureStateMeta {
    bone_consts: Consts<FigureBoneData>,
    locals: Consts<FigureLocals>,
    lantern_offset: anim::vek::Vec3<f32>,
    state_time: f32,
    last_ori: anim::vek::Quaternion<f32>,
    lpindex: u8,
    can_shadow_sun: bool,
    visible: bool,
    last_pos: Option<anim::vek::Vec3<f32>>,
    avg_vel: anim::vek::Vec3<f32>,
    last_light: f32,
    last_glow: (Vec3<f32>, f32),
    acc_vel: f32,
}

impl FigureStateMeta {
    pub fn visible(&self) -> bool { self.visible }

    pub fn can_shadow_sun(&self) -> bool {
        // Either visible, or explicitly a shadow caster.
        self.visible || self.can_shadow_sun
    }
}

pub struct FigureState<S> {
    meta: FigureStateMeta,
    skeleton: S,
}

impl<S> Deref for FigureState<S> {
    type Target = FigureStateMeta;

    fn deref(&self) -> &Self::Target { &self.meta }
}

impl<S> DerefMut for FigureState<S> {
    fn deref_mut(&mut self) -> &mut Self::Target { &mut self.meta }
}

impl<S: Skeleton> FigureState<S> {
    pub fn new(renderer: &mut Renderer, skeleton: S) -> Self {
        let mut buf = [Default::default(); anim::MAX_BONE_COUNT];
        let lantern_offset =
            anim::compute_matrices(&skeleton, anim::vek::Mat4::identity(), &mut buf);
        let bone_consts = figure_bone_data_from_anim(&buf);
        Self {
            meta: FigureStateMeta {
                bone_consts: renderer.create_consts(bone_consts).unwrap(),
                locals: renderer.create_consts(&[FigureLocals::default()]).unwrap(),
                lantern_offset,
                state_time: 0.0,
                last_ori: Ori::default().into(),
                lpindex: 0,
                visible: false,
                can_shadow_sun: false,
                last_pos: None,
                avg_vel: anim::vek::Vec3::zero(),
                last_light: 1.0,
                last_glow: (Vec3::zero(), 0.0),
                acc_vel: 0.0,
            },
            skeleton,
        }
    }

    #[allow(clippy::too_many_arguments)] // TODO: Pending review in #587
    pub fn update<const N: usize>(
        &mut self,
        renderer: &mut Renderer,
        pos: anim::vek::Vec3<f32>,
        ori: anim::vek::Quaternion<f32>,
        scale: f32,
        col: vek::Rgba<f32>,
        dt: f32,
        state_animation_rate: f32,
        model: Option<&FigureModelEntry<N>>,
        _lpindex: u8,
        _visible: bool,
        is_player: bool,
        _camera: &Camera,
        buf: &mut [anim::FigureBoneData; anim::MAX_BONE_COUNT],
        terrain: Option<&Terrain>,
        ground_vel: Vec3<f32>,
    ) {
        // NOTE: As long as update() always gets called after get_or_create_model(), and
        // visibility is not set again until after the model is rendered, we
        // know we don't pair the character model with invalid model state.
        //
        // Currently, the only exception to this during normal gameplay is in the very
        // first tick after a model is created (so there's no `last_character`
        // state).  So in theory, we could have incorrect model data during this
        // tick.  It is possible to resolve this in a few ways, but since
        // currently we don't actually use the model state for anything, we
        // currently ignore this potential issue.
        //
        // FIXME: Address the above at some point.
        let model = if let Some(model) = model {
            model
        } else {
            self.visible = false;
            return;
        };

        // Approximate as a sphere with radius equal to the
        // largest dimension (if we were exact, it should just be half the largest
        // dimension, but we're not, so we double it and use size() instead of
        // half_size()).
        /* let radius = vek::Extent3::<f32>::from(model.bounds.half_size()).reduce_partial_max();
        let _bounds = BoundingSphere::new(pos.into_array(), scale * 0.8 * radius); */

        self.last_ori = vek::Lerp::lerp(self.last_ori, ori, 15.0 * dt).normalized();

        self.state_time += dt * state_animation_rate;

        let mat = {
            anim::vek::Mat4::from(ori) * anim::vek::Mat4::scaling_3d(anim::vek::Vec3::from(scale))
        };

        let atlas_offs = model.allocation.rectangle.min;

        let (light, glow) = terrain
            .map(|t| {
                // Sample the location a little above to avoid clipping into terrain
                // TODO: Try to make this faster? It might be fine though
                let wpos = Vec3::from(pos.into_array()) + Vec3::unit_z();

                let wposi = wpos.map(|e: f32| e.floor() as i32);

                // TODO: Fix this up enough to make it work
                /*
                let sample = |off| {
                    let off = off * wpos.map(|e| (e.fract() - 0.5).signum() as i32);
                    Vec2::new(t.light_at_wpos(wposi + off), t.glow_at_wpos(wposi + off))
                };

                let s_000 = sample(Vec3::new(0, 0, 0));
                let s_100 = sample(Vec3::new(1, 0, 0));
                let s_010 = sample(Vec3::new(0, 1, 0));
                let s_110 = sample(Vec3::new(1, 1, 0));
                let s_001 = sample(Vec3::new(0, 0, 1));
                let s_101 = sample(Vec3::new(1, 0, 1));
                let s_011 = sample(Vec3::new(0, 1, 1));
                let s_111 = sample(Vec3::new(1, 1, 1));
                let s_00 = Lerp::lerp(s_000, s_001, (wpos.z.fract() - 0.5).abs() * 2.0);
                let s_10 = Lerp::lerp(s_100, s_101, (wpos.z.fract() - 0.5).abs() * 2.0);
                let s_01 = Lerp::lerp(s_010, s_011, (wpos.z.fract() - 0.5).abs() * 2.0);
                let s_11 = Lerp::lerp(s_110, s_111, (wpos.z.fract() - 0.5).abs() * 2.0);
                let s_0 = Lerp::lerp(s_00, s_01, (wpos.y.fract() - 0.5).abs() * 2.0);
                let s_1 = Lerp::lerp(s_10, s_11, (wpos.y.fract() - 0.5).abs() * 2.0);
                let s = Lerp::lerp(s_10, s_11, (wpos.x.fract() - 0.5).abs() * 2.0);
                */

                (t.light_at_wpos(wposi), t.glow_normal_at_wpos(wpos))
            })
            .unwrap_or((1.0, (Vec3::zero(), 0.0)));
        // Fade between light and glow levels
        // TODO: Making this temporal rather than spatial is a bit dumb but it's a very
        // subtle difference
        self.last_light = vek::Lerp::lerp(self.last_light, light, 16.0 * dt);
        self.last_glow.0 = vek::Lerp::lerp(self.last_glow.0, glow.0, 16.0 * dt);
        self.last_glow.1 = vek::Lerp::lerp(self.last_glow.1, glow.1, 16.0 * dt);

        let locals = FigureLocals::new(
            mat,
            col.rgb(),
            pos,
            vek::Vec2::new(atlas_offs.x, atlas_offs.y),
            is_player,
            self.last_light,
            self.last_glow,
        );
        renderer.update_consts(&mut self.locals, &[locals]);

        let lantern_offset = anim::compute_matrices(&self.skeleton, mat, buf);

        let new_bone_consts = figure_bone_data_from_anim(buf);

        renderer.update_consts(
            &mut self.meta.bone_consts,
            &new_bone_consts[0..S::BONE_COUNT],
        );
        self.lantern_offset = lantern_offset;

        let smoothing = (5.0 * dt).min(1.0);
        if let Some(last_pos) = self.last_pos {
            self.avg_vel = (1.0 - smoothing) * self.avg_vel + smoothing * (pos - last_pos) / dt;
        }
        self.last_pos = Some(pos);

        // Can potentially overflow
        if self.avg_vel.magnitude_squared() != 0.0 {
            self.acc_vel += (self.avg_vel - ground_vel).magnitude() * dt;
        } else {
            self.acc_vel = 0.0;
        }
    }

    pub fn locals(&self) -> &Consts<FigureLocals> { &self.locals }

    pub fn bone_consts(&self) -> &Consts<FigureBoneData> { &self.bone_consts }

    pub fn skeleton_mut(&mut self) -> &mut S { &mut self.skeleton }
}

fn figure_bone_data_from_anim(
    mats: &[anim::FigureBoneData; anim::MAX_BONE_COUNT],
) -> &[FigureBoneData] {
    bytemuck::cast_slice(mats)
}
