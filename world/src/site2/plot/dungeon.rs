use super::*;
use crate::{
    site::namegen::NameGen,
    site2::{aabr_with_z, gen::PrimitiveTransform, Fill, Structure as SiteStructure},
    util::{attempt, Grid, RandomField, Sampler, CARDINALS, DIRS},
    Land,
};

use common::{
    assets::{self, AssetExt, AssetHandle},
    astar::Astar,
    generation::{ChunkSupplement, EntityInfo},
    store::{Id, Store},
    terrain::{
        BiomeKind, Block, BlockKind, SpriteKind, Structure, StructuresGroup, TerrainChunkSize,
    },
    vol::RectVolSize,
};
use core::{f32, hash::BuildHasherDefault};
use fxhash::FxHasher64;
use lazy_static::lazy_static;
use rand::{prelude::*, seq::SliceRandom};
use serde::Deserialize;
use vek::*;

pub struct Dungeon {
    name: String,
    origin: Vec2<i32>,
    alt: i32,
    seed: u32,
    #[allow(dead_code)]
    noise: RandomField,
    floors: Vec<Floor>,
    difficulty: u32,
}

pub struct GenCtx<'a, R: Rng> {
    land: &'a Land<'a>,
    rng: &'a mut R,
}

#[derive(Deserialize)]
pub struct Colors {
    pub stone: (u8, u8, u8),
}

const ALT_OFFSET: i32 = -2;

#[derive(Deserialize)]
struct DungeonDistribution(Vec<(u32, f32)>);
impl assets::Asset for DungeonDistribution {
    type Loader = assets::RonLoader;

    const EXTENSION: &'static str = "ron";
}

lazy_static! {
    static ref DUNGEON_DISTRIBUTION: Vec<(u32, f32)> =
        DungeonDistribution::load_expect("world.dungeon.difficulty_distribution")
            .read()
            .0
            .clone();
}

fn floor_amount(difficulty: u32) -> u32 { 3 + difficulty / 2 }

impl Dungeon {
    pub fn generate(wpos: Vec2<i32>, land: &Land, rng: &mut impl Rng) -> Self {
        let mut ctx = GenCtx { land, rng };
        let difficulty = DUNGEON_DISTRIBUTION
            .choose_weighted(&mut ctx.rng, |pair| pair.1)
            .map(|(difficulty, _)| *difficulty)
            .unwrap_or_else(|err| {
                panic!(
                    "Failed to choose difficulty (check instruction in config). Error: {}",
                    err
                )
            });
        let floors = floor_amount(difficulty);

        Self {
            name: {
                let name = NameGen::location(ctx.rng).generate();
                match ctx.rng.gen_range(0..5) {
                    0 => format!("{} Dungeon", name),
                    1 => format!("{} Lair", name),
                    2 => format!("{} Crib", name),
                    3 => format!("{} Catacombs", name),
                    _ => format!("{} Pit", name),
                }
            },
            origin: wpos - TILE_SIZE / 2,
            alt: ctx.land.get_alt_approx(wpos) as i32 + 6,
            seed: ctx.rng.gen(),
            noise: RandomField::new(ctx.rng.gen()),
            floors: (0..floors)
                .scan(Vec2::zero(), |stair_tile, level| {
                    let (floor, st) =
                        Floor::generate(&mut ctx, *stair_tile, level as i32, difficulty);
                    *stair_tile = st;
                    Some(floor)
                })
                .collect(),
            difficulty,
        }
    }

    pub fn name(&self) -> &str { &self.name }

    pub fn get_origin(&self) -> Vec2<i32> { self.origin }

    pub fn radius(&self) -> f32 {
        self.floors
            .iter()
            .map(|floor| (TILE_SIZE * floor.tiles.size()).magnitude_squared())
            .max()
            .map(|d| (d as f32).sqrt() / 2.0)
            .unwrap_or(200.0)
    }

    pub fn spawn_rules(&self, wpos: Vec2<i32>) -> SpawnRules {
        SpawnRules {
            trees: wpos.distance_squared(self.origin) > 64i32.pow(2),
            ..SpawnRules::default()
        }
    }

    pub fn difficulty(&self) -> u32 { self.difficulty }

    pub fn apply_supplement<'a>(
        &'a self,
        // NOTE: Used only for dynamic elements like chests and entities!
        dynamic_rng: &mut impl Rng,
        wpos2d: Vec2<i32>,
        supplement: &mut ChunkSupplement,
    ) {
        let rpos = wpos2d - self.origin;
        let area = Aabr {
            min: rpos,
            max: rpos + TerrainChunkSize::RECT_SIZE.map(|e| e as i32),
        };

        // Add waypoint
        let pos = self.origin.map2(FLOOR_SIZE, |e, sz| e + sz as i32 / 2);
        if area.contains_point(pos - self.origin) {
            supplement.add_entity(
                EntityInfo::at(Vec3::new(pos.x as f32, pos.y as f32, self.alt as f32) + 5.0)
                    .into_waypoint(),
            );
        }

        let mut z = self.alt + ALT_OFFSET;
        for floor in &self.floors {
            z -= floor.total_depth();
            let origin = Vec3::new(self.origin.x, self.origin.y, z);
            floor.apply_supplement(dynamic_rng, area, origin, supplement);
        }
    }
}

const TILE_SIZE: i32 = 13;

#[derive(Clone)]
pub enum StairsKind {
    Spiral,
    WallSpiral,
}

#[derive(Clone)]
pub enum Tile {
    UpStair(Id<Room>, StairsKind),
    DownStair(Id<Room>),
    Room(Id<Room>),
    Tunnel,
    Solid,
}

impl Tile {
    fn is_passable(&self) -> bool {
        matches!(
            self,
            Tile::UpStair(_, _) | Tile::DownStair(_) | Tile::Room(_) | Tile::Tunnel
        )
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum RoomKind {
    Peaceful,
    Fight,
    Boss,
    Miniboss,
    #[allow(dead_code)]
    LavaPlatforming,
}

pub struct Room {
    seed: u32,
    loot_density: f32,
    kind: RoomKind,
    area: Rect<i32, i32>,
    height: i32,
    pillars: Option<i32>, // Pillars with the given separation
    pits: Option<i32>,    // Pits filled with lava
    difficulty: u32,
}

impl Room {
    fn fill_fight_cell(
        &self,
        supplement: &mut ChunkSupplement,
        dynamic_rng: &mut impl Rng,
        tile_wcenter: Vec3<i32>,
        wpos2d: Vec2<i32>,
        tile_pos: Vec2<i32>,
    ) {
        let enemy_spawn_tile = self.area.center();
        // Don't spawn enemies in a pillar
        let enemy_tile_is_pillar = self.pillars.map_or(false, |pillar_space| {
            enemy_spawn_tile
                .map(|e| e.rem_euclid(pillar_space) == 0)
                .reduce_and()
        });
        let enemy_spawn_tile = enemy_spawn_tile + if enemy_tile_is_pillar { 1 } else { 0 };

        // Toss mobs in the center of the room
        if tile_pos == enemy_spawn_tile && wpos2d == tile_wcenter.xy() {
            let entities = match self.difficulty {
                1 => enemy_1(dynamic_rng, tile_wcenter),
                2 => enemy_2(dynamic_rng, tile_wcenter),
                3 => enemy_3(dynamic_rng, tile_wcenter),
                4 => enemy_4(dynamic_rng, tile_wcenter),
                5 => enemy_5(dynamic_rng, tile_wcenter),
                _ => enemy_fallback(dynamic_rng, tile_wcenter),
            };

            for entity in entities {
                supplement.add_entity(entity);
            }
        } else {
            // Turrets
            // Turret has 1/5000 chance to spawn per voxel in fight room
            if dynamic_rng.gen_range(0..5000) == 0 {
                let pos = tile_wcenter.map(|e| e as f32)
                    + Vec3::<u32>::iota()
                        .map(|e| {
                            (RandomField::new(self.seed.wrapping_add(10 + e))
                                .get(Vec3::from(tile_pos))
                                % 32) as i32
                                - 16
                        })
                        .map(|e| e as f32 / 16.0);
                match self.difficulty {
                    3 => {
                        let turret = turret_3(dynamic_rng, pos);
                        supplement.add_entity(turret);
                    },
                    5 => {
                        let turret = turret_5(dynamic_rng, pos);
                        supplement.add_entity(turret);
                    },
                    _ => {},
                };
            }
        }
    }

    fn fill_miniboss_cell(
        &self,
        supplement: &mut ChunkSupplement,
        dynamic_rng: &mut impl Rng,
        tile_wcenter: Vec3<i32>,
        wpos2d: Vec2<i32>,
        tile_pos: Vec2<i32>,
    ) {
        let miniboss_spawn_tile = self.area.center();
        // Don't spawn the miniboss in a pillar
        let miniboss_tile_is_pillar = self.pillars.map_or(false, |pillar_space| {
            miniboss_spawn_tile
                .map(|e| e.rem_euclid(pillar_space) == 0)
                .reduce_and()
        });
        let miniboss_spawn_tile = miniboss_spawn_tile + if miniboss_tile_is_pillar { 1 } else { 0 };

        if tile_pos == miniboss_spawn_tile && tile_wcenter.xy() == wpos2d {
            let entities = match self.difficulty {
                1 => mini_boss_1(dynamic_rng, tile_wcenter),
                2 => mini_boss_2(dynamic_rng, tile_wcenter),
                3 => mini_boss_3(dynamic_rng, tile_wcenter),
                4 => mini_boss_4(dynamic_rng, tile_wcenter),
                5 => mini_boss_5(dynamic_rng, tile_wcenter),
                _ => mini_boss_fallback(dynamic_rng, tile_wcenter),
            };

            for entity in entities {
                supplement.add_entity(entity);
            }
        }
    }

    fn fill_boss_cell(
        &self,
        supplement: &mut ChunkSupplement,
        dynamic_rng: &mut impl Rng,
        tile_wcenter: Vec3<i32>,
        wpos2d: Vec2<i32>,
        tile_pos: Vec2<i32>,
    ) {
        let boss_spawn_tile = self.area.center();
        // Don't spawn the boss in a pillar
        let boss_tile_is_pillar = self.pillars.map_or(false, |pillar_space| {
            boss_spawn_tile
                .map(|e| e.rem_euclid(pillar_space) == 0)
                .reduce_and()
        });
        let boss_spawn_tile = boss_spawn_tile + if boss_tile_is_pillar { 1 } else { 0 };

        if tile_pos == boss_spawn_tile && wpos2d == tile_wcenter.xy() {
            let entities = match self.difficulty {
                1 => boss_1(dynamic_rng, tile_wcenter),
                2 => boss_2(dynamic_rng, tile_wcenter),
                3 => boss_3(dynamic_rng, tile_wcenter),
                4 => boss_4(dynamic_rng, tile_wcenter),
                5 => boss_5(dynamic_rng, tile_wcenter),
                _ => boss_fallback(dynamic_rng, tile_wcenter),
            };

            for entity in entities {
                supplement.add_entity(entity);
            }
        }
    }
}

struct Floor {
    tile_offset: Vec2<i32>,
    tiles: Grid<Tile>,
    rooms: Store<Room>,
    solid_depth: i32,
    hollow_depth: i32,
    #[allow(dead_code)]
    stair_tile: Vec2<i32>,
    final_level: bool,
    difficulty: u32,
}

const FLOOR_SIZE: Vec2<i32> = Vec2::new(18, 18);

impl Floor {
    fn generate(
        ctx: &mut GenCtx<impl Rng>,
        stair_tile: Vec2<i32>,
        level: i32,
        difficulty: u32,
    ) -> (Self, Vec2<i32>) {
        const MAX_WIDTH: u32 = 4;
        let floors = floor_amount(difficulty);
        let final_level = level == floors as i32 - 1;
        let width = (2 + difficulty / 2).min(MAX_WIDTH);
        let height = (15 + difficulty * 3).min(30);

        let new_stair_tile = if final_level {
            Vec2::zero()
        } else {
            std::iter::from_fn(|| {
                Some(FLOOR_SIZE.map(|sz| ctx.rng.gen_range(-sz / 2 + 2..sz / 2 - 1)))
            })
            .filter(|pos| *pos != stair_tile)
            .take(8)
            .max_by_key(|pos| (*pos - stair_tile).map(|e| e.abs()).sum())
            .unwrap()
        };

        let tile_offset = -FLOOR_SIZE / 2;
        let mut this = Floor {
            tile_offset,
            tiles: Grid::new(FLOOR_SIZE, Tile::Solid),
            rooms: Store::default(),
            solid_depth: if level == 0 { 80 } else { 32 },
            hollow_depth: 30,
            stair_tile: new_stair_tile - tile_offset,
            final_level,
            difficulty,
        };

        const STAIR_ROOM_HEIGHT: i32 = 13;
        // Create rooms for entrance and exit
        let upstair_room = this.create_room(Room {
            seed: ctx.rng.gen(),
            loot_density: 0.0,
            kind: RoomKind::Peaceful,
            area: Rect::from((stair_tile - tile_offset - 1, Extent2::broadcast(3))),
            height: STAIR_ROOM_HEIGHT,
            pillars: None,
            pits: None,
            difficulty,
        });
        if final_level {
            // Boss room
            this.create_room(Room {
                seed: ctx.rng.gen(),
                loot_density: 0.0,
                kind: RoomKind::Boss,
                area: Rect::from((
                    new_stair_tile - tile_offset - MAX_WIDTH as i32 - 1,
                    Extent2::broadcast(width as i32 * 2 + 1),
                )),
                height: height as i32,
                pillars: Some(2),
                pits: None,
                difficulty,
            });
        } else {
            // Create downstairs room
            let downstair_room = this.create_room(Room {
                seed: ctx.rng.gen(),
                loot_density: 0.0,
                kind: RoomKind::Peaceful,
                area: Rect::from((new_stair_tile - tile_offset - 1, Extent2::broadcast(3))),
                height: STAIR_ROOM_HEIGHT,
                pillars: None,
                pits: None,
                difficulty,
            });
            this.tiles.set(
                new_stair_tile - tile_offset,
                Tile::DownStair(downstair_room),
            );
        }
        let stair_kind = if ctx.rng.gen::<f32>() < 0.3 {
            StairsKind::Spiral
        } else {
            StairsKind::WallSpiral
        };

        this.tiles.set(
            stair_tile - tile_offset,
            Tile::UpStair(upstair_room, stair_kind),
        );

        this.create_rooms(ctx, level, 7);
        // Create routes between all rooms
        let room_areas = this.rooms.values().map(|r| r.area).collect::<Vec<_>>();
        for a in room_areas.iter() {
            for b in room_areas.iter() {
                this.create_route(ctx, a.center(), b.center());
            }
        }

        (this, new_stair_tile)
    }

    fn create_room(&mut self, room: Room) -> Id<Room> {
        let area = room.area;
        let id = self.rooms.insert(room);
        for x in 0..area.extent().w {
            for y in 0..area.extent().h {
                self.tiles
                    .set(area.position() + Vec2::new(x, y), Tile::Room(id));
            }
        }
        id
    }

    fn create_rooms(&mut self, ctx: &mut GenCtx<impl Rng>, level: i32, n: usize) {
        let dim_limits = (3, 6);

        for _ in 0..n {
            let area = match attempt(64, || {
                let sz = Vec2::<i32>::zero().map(|_| ctx.rng.gen_range(dim_limits.0..dim_limits.1));
                let pos = FLOOR_SIZE.map2(sz, |floor_sz, room_sz| {
                    ctx.rng.gen_range(0..floor_sz + 1 - room_sz)
                });
                let area = Rect::from((pos, Extent2::from(sz)));
                // The room, but with some personal space
                let area_border = Rect::from((pos - 1, Extent2::from(sz) + 2));

                // Ensure no overlap
                if self
                    .rooms
                    .values()
                    .any(|r| r.area.collides_with_rect(area_border))
                {
                    return None;
                }

                Some(area)
            }) {
                Some(area) => area,
                None => return,
            };

            let loot_density = |difficulty, level| {
                let max_floor = floor_amount(difficulty);
                // We count floors from 0, don't divide by zero
                let current_floor = level + 1;
                let ratio = f64::from(current_floor) / f64::from(max_floor);
                // filter starting floors
                let ratio = 0.0_f64.max(ratio - 0.55);
                0.00175 * ratio as f32
            };
            match ctx.rng.gen_range(0..5) {
                // Miniboss room
                0 => self.create_room(Room {
                    seed: ctx.rng.gen(),
                    loot_density: loot_density(self.difficulty, level),
                    kind: RoomKind::Miniboss,
                    area,
                    height: ctx.rng.gen_range(15..20),
                    pillars: Some(ctx.rng.gen_range(2..=4)),
                    pits: None,
                    difficulty: self.difficulty,
                }),
                //// Lava platforming room
                //1 => self.create_room(Room {
                //    seed: ctx.rng.gen(),
                //    loot_density: 0.0,
                //    kind: RoomKind::LavaPlatforming,
                //    area,
                //    height: ctx.rng.gen_range(10..15),
                //    pillars: None,
                //    pits: Some(1),
                //    difficulty: self.difficulty,
                //}),
                // Fight room with enemies in it
                _ => self.create_room(Room {
                    seed: ctx.rng.gen(),
                    loot_density: loot_density(self.difficulty, level),
                    kind: RoomKind::Fight,
                    area,
                    height: ctx.rng.gen_range(10..15),
                    pillars: if ctx.rng.gen_range(0..4) == 0 {
                        Some(ctx.rng.gen_range(2..=4))
                    } else {
                        None
                    },
                    pits: None,
                    difficulty: self.difficulty,
                }),
            };
        }
    }

    fn create_route(&mut self, _ctx: &mut GenCtx<impl Rng>, a: Vec2<i32>, b: Vec2<i32>) {
        let heuristic = move |l: &Vec2<i32>| (l - b).map(|e| e.abs()).reduce_max() as f32;
        let neighbors = |l: &Vec2<i32>| {
            let l = *l;
            CARDINALS
                .iter()
                .map(move |dir| l + dir)
                .filter(|pos| self.tiles.get(*pos).is_some())
        };
        let transition = |_a: &Vec2<i32>, b: &Vec2<i32>| match self.tiles.get(*b) {
            Some(Tile::Room(_)) | Some(Tile::Tunnel) => 1.0,
            Some(Tile::Solid) => 25.0,
            Some(Tile::UpStair(_, _)) | Some(Tile::DownStair(_)) => 0.0,
            _ => 100000.0,
        };
        let satisfied = |l: &Vec2<i32>| *l == b;
        // We use this hasher (FxHasher64) because
        // (1) we don't care about DDOS attacks (ruling out SipHash);
        // (2) we don't care about determinism across computers (we could use AAHash);
        // (3) we have 8-byte keys (for which FxHash is fastest).
        let mut astar = Astar::new(
            20000,
            a,
            heuristic,
            BuildHasherDefault::<FxHasher64>::default(),
        );
        let path = astar
            .poll(
                FLOOR_SIZE.product() as usize + 1,
                heuristic,
                neighbors,
                transition,
                satisfied,
            )
            .into_path()
            .expect("No route between locations - this shouldn't be able to happen");

        for pos in path.iter() {
            if let Some(tile @ Tile::Solid) = self.tiles.get_mut(*pos) {
                *tile = Tile::Tunnel;
            }
        }
    }

    fn apply_supplement(
        &self,
        // NOTE: Used only for dynamic elements like chests and entities!
        dynamic_rng: &mut impl Rng,
        area: Aabr<i32>,
        origin: Vec3<i32>,
        supplement: &mut ChunkSupplement,
    ) {
        /*
        // Add stair waypoint
        let stair_rcenter =
            Vec3::from((self.stair_tile + self.tile_offset).map(|e| e * TILE_SIZE + TILE_SIZE / 2));

        if area.contains_point(stair_rcenter.xy()) {
            let offs = Vec2::new(
                dynamic_rng.gen_range(-1.0..1.0),
                dynamic_rng.gen_range(-1.0..1.0),
            )
            .try_normalized()
            .unwrap_or_else(Vec2::unit_y)
                * (TILE_SIZE as f32 / 2.0 - 4.0);
            if !self.final_level {
                supplement.add_entity(
                    EntityInfo::at((origin + stair_rcenter).map(|e| e as f32)
            + Vec3::from(offs))             .into_waypoint(),
                );
            }
        }
        */

        for x in area.min.x..area.max.x {
            for y in area.min.y..area.max.y {
                let tile_pos = Vec2::new(x, y).map(|e| e.div_euclid(TILE_SIZE)) - self.tile_offset;
                let wpos2d = origin.xy() + Vec2::new(x, y);
                if let Some(Tile::Room(room)) = self.tiles.get(tile_pos) {
                    let room = &self.rooms[*room];

                    let tile_wcenter = origin
                        + Vec3::from(
                            Vec2::new(x, y)
                                .map(|e| e.div_euclid(TILE_SIZE) * TILE_SIZE + TILE_SIZE / 2),
                        );

                    match room.kind {
                        RoomKind::Fight => room.fill_fight_cell(
                            supplement,
                            dynamic_rng,
                            tile_wcenter,
                            wpos2d,
                            tile_pos,
                        ),
                        RoomKind::Miniboss => room.fill_miniboss_cell(
                            supplement,
                            dynamic_rng,
                            tile_wcenter,
                            wpos2d,
                            tile_pos,
                        ),
                        RoomKind::Boss => room.fill_boss_cell(
                            supplement,
                            dynamic_rng,
                            tile_wcenter,
                            wpos2d,
                            tile_pos,
                        ),
                        RoomKind::Peaceful | RoomKind::LavaPlatforming => {},
                    }
                }
            }
        }
    }

    fn total_depth(&self) -> i32 { self.solid_depth + self.hollow_depth }

    // Find orientation of a position relative to another position
    #[allow(clippy::collapsible_else_if)]
    fn relative_ori(pos1: Vec2<i32>, pos2: Vec2<i32>) -> u8 {
        if (pos1.x - pos2.x).abs() < (pos1.y - pos2.y).abs() {
            if pos1.y > pos2.y { 4 } else { 8 }
        } else {
            if pos1.x > pos2.x { 2 } else { 6 }
        }
    }
}

fn enemy_1(dynamic_rng: &mut impl Rng, tile_wcenter: Vec3<i32>) -> Vec<EntityInfo> {
    let number = dynamic_rng.gen_range(2..=4);
    let mut entities = Vec::new();
    entities.resize_with(number, || {
        // TODO: give enemies health skills?
        let entity = EntityInfo::at(tile_wcenter.map(|e| e as f32));
        match dynamic_rng.gen_range(0..=4) {
            0 => entity.with_asset_expect("common.entity.dungeon.tier-1.tracker", dynamic_rng),
            1 => entity.with_asset_expect("common.entity.dungeon.tier-1.icepicker", dynamic_rng),
            _ => entity.with_asset_expect("common.entity.dungeon.tier-1.hunter", dynamic_rng),
        }
    });

    entities
}

fn enemy_2(dynamic_rng: &mut impl Rng, tile_wcenter: Vec3<i32>) -> Vec<EntityInfo> {
    let number = dynamic_rng.gen_range(2..=4);
    let mut entities = Vec::new();
    entities.resize_with(number, || {
        // TODO: give enemies health skills?
        let entity = EntityInfo::at(tile_wcenter.map(|e| e as f32));
        match dynamic_rng.gen_range(0..=4) {
            0 => entity.with_asset_expect("common.entity.dungeon.tier-2.sniper", dynamic_rng),
            1 => entity.with_asset_expect("common.entity.dungeon.tier-2.sorcerer", dynamic_rng),
            _ => entity.with_asset_expect("common.entity.dungeon.tier-2.spearman", dynamic_rng),
        }
    });

    entities
}

fn enemy_3(dynamic_rng: &mut impl Rng, tile_wcenter: Vec3<i32>) -> Vec<EntityInfo> {
    let number = dynamic_rng.gen_range(2..=4);
    let mut entities = Vec::new();
    entities.resize_with(number, || {
        // TODO: give enemies health skills?
        let entity = EntityInfo::at(tile_wcenter.map(|e| e as f32));
        match dynamic_rng.gen_range(0..=4) {
            0 => entity.with_asset_expect("common.entity.dungeon.tier-3.archer", dynamic_rng),
            1 => entity.with_asset_expect("common.entity.dungeon.tier-3.soldier", dynamic_rng),
            _ => entity.with_asset_expect("common.entity.dungeon.tier-3.guard", dynamic_rng),
        }
    });

    entities
}

fn enemy_4(dynamic_rng: &mut impl Rng, tile_wcenter: Vec3<i32>) -> Vec<EntityInfo> {
    let number = dynamic_rng.gen_range(2..=4);
    let mut entities = Vec::new();
    entities.resize_with(number, || {
        // TODO: give enemies health skills?
        let entity = EntityInfo::at(tile_wcenter.map(|e| e as f32));
        match dynamic_rng.gen_range(0..=4) {
            0 => entity.with_asset_expect("common.entity.dungeon.tier-4.marksman", dynamic_rng),
            1 => entity.with_asset_expect("common.entity.dungeon.tier-4.strategian", dynamic_rng),
            _ => entity.with_asset_expect("common.entity.dungeon.tier-4.hoplite", dynamic_rng),
        }
    });

    entities
}

fn enemy_5(dynamic_rng: &mut impl Rng, tile_wcenter: Vec3<i32>) -> Vec<EntityInfo> {
    let number = dynamic_rng.gen_range(1..=3);
    let mut entities = Vec::new();
    entities.resize_with(number, || {
        // TODO: give enemies health skills?
        let entity = EntityInfo::at(tile_wcenter.map(|e| e as f32));
        match dynamic_rng.gen_range(0..=4) {
            0 => entity.with_asset_expect("common.entity.dungeon.tier-5.warlock", dynamic_rng),
            1 => entity.with_asset_expect("common.entity.dungeon.tier-5.warlord", dynamic_rng),
            _ => entity.with_asset_expect("common.entity.dungeon.tier-5.cultist", dynamic_rng),
        }
    });

    entities
}

fn enemy_fallback(dynamic_rng: &mut impl Rng, tile_wcenter: Vec3<i32>) -> Vec<EntityInfo> {
    let number = dynamic_rng.gen_range(2..=4);
    let mut entities = Vec::new();
    entities.resize_with(number, || {
        let entity = EntityInfo::at(tile_wcenter.map(|e| e as f32));
        entity.with_asset_expect("common.entity.dungeon.fallback.enemy", dynamic_rng)
    });

    entities
}

fn turret_3(dynamic_rng: &mut impl Rng, pos: Vec3<f32>) -> EntityInfo {
    EntityInfo::at(pos).with_asset_expect("common.entity.dungeon.tier-3.sentry", dynamic_rng)
}

fn turret_5(dynamic_rng: &mut impl Rng, pos: Vec3<f32>) -> EntityInfo {
    EntityInfo::at(pos).with_asset_expect("common.entity.dungeon.tier-5.turret", dynamic_rng)
}

fn boss_1(dynamic_rng: &mut impl Rng, tile_wcenter: Vec3<i32>) -> Vec<EntityInfo> {
    vec![
        EntityInfo::at(tile_wcenter.map(|e| e as f32))
            .with_asset_expect("common.entity.dungeon.tier-1.boss", dynamic_rng),
    ]
}

fn boss_2(dynamic_rng: &mut impl Rng, tile_wcenter: Vec3<i32>) -> Vec<EntityInfo> {
    vec![
        EntityInfo::at(tile_wcenter.map(|e| e as f32))
            .with_asset_expect("common.entity.dungeon.tier-2.boss", dynamic_rng),
    ]
}
fn boss_3(dynamic_rng: &mut impl Rng, tile_wcenter: Vec3<i32>) -> Vec<EntityInfo> {
    let mut entities = Vec::new();
    entities.resize_with(2, || {
        EntityInfo::at(tile_wcenter.map(|e| e as f32))
            .with_asset_expect("common.entity.dungeon.tier-3.boss", dynamic_rng)
    });

    entities
}

fn boss_4(dynamic_rng: &mut impl Rng, tile_wcenter: Vec3<i32>) -> Vec<EntityInfo> {
    vec![
        EntityInfo::at(tile_wcenter.map(|e| e as f32))
            .with_asset_expect("common.entity.dungeon.tier-4.boss", dynamic_rng),
    ]
}

fn boss_5(dynamic_rng: &mut impl Rng, tile_wcenter: Vec3<i32>) -> Vec<EntityInfo> {
    vec![
        EntityInfo::at(tile_wcenter.map(|e| e as f32))
            .with_asset_expect("common.entity.dungeon.tier-5.boss", dynamic_rng),
    ]
}

fn boss_fallback(dynamic_rng: &mut impl Rng, tile_wcenter: Vec3<i32>) -> Vec<EntityInfo> {
    vec![
        EntityInfo::at(tile_wcenter.map(|e| e as f32))
            .with_asset_expect("common.entity.dungeon.fallback.boss", dynamic_rng),
    ]
}

fn mini_boss_1(dynamic_rng: &mut impl Rng, tile_wcenter: Vec3<i32>) -> Vec<EntityInfo> {
    let mut entities = Vec::new();
    entities.resize_with(8, || {
        EntityInfo::at(tile_wcenter.map(|e| e as f32))
            .with_asset_expect("common.entity.dungeon.tier-1.rat", dynamic_rng)
    });
    entities
}

fn mini_boss_2(dynamic_rng: &mut impl Rng, tile_wcenter: Vec3<i32>) -> Vec<EntityInfo> {
    let mut entities = Vec::new();
    entities.resize_with(6, || {
        EntityInfo::at(tile_wcenter.map(|e| e as f32))
            .with_asset_expect("common.entity.dungeon.tier-2.hakulaq", dynamic_rng)
    });
    entities
}

fn mini_boss_3(dynamic_rng: &mut impl Rng, tile_wcenter: Vec3<i32>) -> Vec<EntityInfo> {
    let mut entities = Vec::new();
    entities.resize_with(3, || {
        EntityInfo::at(tile_wcenter.map(|e| e as f32))
            .with_asset_expect("common.entity.dungeon.tier-3.bonerattler", dynamic_rng)
    });
    entities
}

fn mini_boss_4(dynamic_rng: &mut impl Rng, tile_wcenter: Vec3<i32>) -> Vec<EntityInfo> {
    vec![
        EntityInfo::at(tile_wcenter.map(|e| e as f32))
            .with_asset_expect("common.entity.dungeon.tier-4.miniboss", dynamic_rng),
    ]
}

fn mini_boss_5(dynamic_rng: &mut impl Rng, tile_wcenter: Vec3<i32>) -> Vec<EntityInfo> {
    let mut entities = Vec::new();
    match dynamic_rng.gen_range(0..=2) {
        0 => {
            entities.push(
                EntityInfo::at(tile_wcenter.map(|e| e as f32))
                    .with_asset_expect("common.entity.dungeon.tier-5.beastmaster", dynamic_rng),
            );
            entities.resize_with(entities.len() + 4, || {
                EntityInfo::at(tile_wcenter.map(|e| e as f32))
                    .with_asset_expect("common.entity.dungeon.tier-5.hound", dynamic_rng)
            });
        },
        1 => {
            entities.resize_with(2, || {
                EntityInfo::at(tile_wcenter.map(|e| e as f32))
                    .with_asset_expect("common.entity.dungeon.tier-5.husk_brute", dynamic_rng)
            });
        },
        _ => {
            entities.resize_with(10, || {
                EntityInfo::at(tile_wcenter.map(|e| e as f32))
                    .with_asset_expect("common.entity.dungeon.tier-5.husk", dynamic_rng)
            });
        },
    }
    entities
}

fn mini_boss_fallback(dynamic_rng: &mut impl Rng, tile_wcenter: Vec3<i32>) -> Vec<EntityInfo> {
    vec![
        EntityInfo::at(tile_wcenter.map(|e| e as f32))
            .with_asset_expect("common.entity.dungeon.fallback.miniboss", dynamic_rng),
    ]
}

pub fn tilegrid_nearest_wall(tiles: &Grid<Tile>, rpos: Vec2<i32>) -> Option<Vec2<i32>> {
    let tile_pos = rpos.map(|e| e.div_euclid(TILE_SIZE));

    DIRS.iter()
        .map(|dir| tile_pos + *dir)
        .filter(|other_tile_pos| {
            tiles
                .get(*other_tile_pos)
                .filter(|tile| tile.is_passable())
                .is_none()
        })
        .map(|other_tile_pos| {
            rpos.clamped(
                other_tile_pos * TILE_SIZE,
                (other_tile_pos + 1) * TILE_SIZE - 1,
            )
        })
        .min_by_key(|nearest| rpos.distance_squared(*nearest))
}

pub fn spiral_staircase(
    origin: Vec3<i32>,
    radius: f32,
    inner_radius: f32,
    stretch: f32,
) -> impl Fn(Vec3<i32>) -> bool + Copy {
    move |pos: Vec3<i32>| {
        let pos = pos - origin;
        if (pos.xy().magnitude_squared() as f32) < inner_radius.powi(2) {
            true
        } else if (pos.xy().magnitude_squared() as f32) < radius.powi(2) {
            ((pos.x as f32).atan2(pos.y as f32) / (f32::consts::PI * 2.0) * stretch + pos.z as f32)
                .rem_euclid(stretch)
                < 1.5
        } else {
            false
        }
    }
}

pub fn wall_staircase(
    origin: Vec3<i32>,
    radius: f32,
    stretch: f32,
) -> impl Fn(Vec3<i32>) -> bool + Copy {
    move |pos: Vec3<i32>| {
        let pos = pos - origin;
        if (pos.x.abs().max(pos.y.abs())) as f32 > 0.6 * radius {
            ((pos.x as f32).atan2(pos.y as f32) / (f32::consts::PI * 2.0) * stretch + pos.z as f32)
                .rem_euclid(stretch)
                < 1.0
        } else {
            false
        }
    }
}

pub fn inscribed_polystar(
    origin: Vec2<i32>,
    radius: f32,
    sides: usize,
) -> impl Fn(Vec3<i32>) -> bool + Copy {
    move |pos| {
        use std::f32::consts::TAU;
        let rpos: Vec2<f32> = pos.xy().as_() - origin.as_();
        let is_border = rpos.magnitude_squared() > (radius - 2.0).powi(2);
        let is_line = (0..sides).into_iter().any(|i| {
            let f = |j: f32| {
                let t = j * TAU / sides as f32;
                radius * Vec2::new(t.cos(), t.sin())
            };
            let line = LineSegment2 {
                start: f(i as f32),
                end: f((i + 2) as f32),
            };
            line.distance_to_point(rpos) <= 1.0
        });
        is_border || is_line
    }
}

pub fn make_wall_contours<'a>(
    tiles: &'a Grid<Tile>,
    tile_corner: Vec2<i32>,
    tile_pos: Vec2<i32>,
    floor_z: i32,
    wall_thickness: f32,
    tunnel_height: f32,
) -> impl Fn(Vec3<i32>) -> bool + Copy + 'a {
    let mut wall_fn = |dir: Vec2<i32>| /* |dirs: &[Vec2<i32>]| dirs.iter()
        .map(|dir| */{
            tiles
                .get(tile_pos + dir)
                .filter(|tile| !tile.is_passable())
                .and(Some(Aabr {
                    min: dir * TILE_SIZE,
                    max: (dir + 1) * TILE_SIZE - 1,
                }))
        }/*)*/;

    // NOTE: There can never be more than 4 meaningful closest walls.
    //
    // * If a horizontal *or* vertical wall is present at a corner, we will always
    //   be closer to one of those walls than the corner (because we form a right triangle
    //   with the distance from horizontal, distance from vertical, and distance from corner).
    // * Otherwise, we may use the corner wall, if present, in exchange for one of the two slots.
    // * Since at most 2 corners are adjacent to a vertical or horizontal slot, and we can only use
    //   a slot if *both* adjacent edges are missing, we will never use up more slots than edges,
    //   even if all 4 corners are taken.
    let x1 = wall_fn(Vec2::new(1, 0));
    let y1 = wall_fn(Vec2::new(0, 1));
    let x0 = wall_fn(Vec2::new(-1, 0));
    let y0 = wall_fn(Vec2::new(0, -1));

    let walls = [
        x1.or(y1).or_else(|| wall_fn(Vec2::new(1, 1))),
        y0.or(x0).or_else(|| wall_fn(Vec2::new(-1, -1))),
        // if x1 is true it was already looked at
        // if y1 is true it was already looked at
        // if (x1,y1) is relevant it was already looked at
        //
        // Remaining options are (x1,y0) and (x0,y1).
        // Since if x0 & y0, neither of these will be relevant, it doesn't matter which we pick
        // unless this isn't the case, and we pick both.
        x1.and(y1).or_else(|| wall_fn(Vec2::new(1, -1))),
        y0.and(x0).or_else(|| wall_fn(Vec2::new(-1, 1))),
    ];
    /* let walls = [
        walls_iter.next().map(Some).unwrap_or(None),
        walls_iter.next().map(Some).unwrap_or(None),
        walls_iter.next().map(Some).unwrap_or(None),
        walls_iter.next().map(Some).unwrap_or(None),
    ]; */

    let size_recip = (TILE_SIZE as f32 - wall_thickness).recip();
    let size_recip_2 = size_recip * size_recip;
    let wall_thickness_2 = wall_thickness * wall_thickness;
    const TILE_SIZE_2: i32 = TILE_SIZE * TILE_SIZE;
    let size_offset = 1.0 + wall_thickness_2 * size_recip_2;
    // let size_offset = 1.0 + wall_thickness * size_recip;
    move |pos| {
        let rpos = pos.xy() - /*floor_corner*/tile_corner;
        let dist_to_wall =
            walls
            .into_iter()
            .filter_map(|x| x)
            .map(|x| x.projected_point(rpos).distance_squared(rpos))
            .min()
            .unwrap_or(TILE_SIZE_2) as f32;

        /* let dist_to_wall = tilegrid_nearest_wall(&tiles, rpos)
            .map(|nearest| (nearest.distance_squared(rpos) as f32)/*.sqrt()*/)
            .unwrap_or(TILE_SIZE as f32); */
        /* let tunnel_dist = dist_to_wall.mul_add(-size_recip_2, size_offset);
        let tunnel_dist_2 = tunnel_dist.mul_add(-tunnel_dist, 1.0);
        let tunnel_dist_3 = tunnel_dist_2.mul_add(-tunnel_height, (pos.z - floor_z) as f32); */
        let tunnel_dist = dist_to_wall.sqrt().mul_add(-size_recip, size_offset);
        let tunnel_dist_2 = tunnel_dist * tunnel_dist;
        let tunnel_dist_2 = tunnel_dist_2.mul_add(-tunnel_dist_2, 1.0);
        let tunnel_dist_3 = tunnel_dist_2.mul_add(-tunnel_height, (pos.z - floor_z) as f32);
        dist_to_wall < wall_thickness_2 || tunnel_dist_3 >= 0.0
        /* dist_to_wall < wall_thickness ||
                // 1.0 - (dist_to_wall - wall_thickness) * size_recip;
                ((pos.z - floor_z) as f32) >= tunnel_height * (1.0 - tunnel_dist.powi(4)) */
        /* false */
    }
}

impl Floor {
    fn render<'a, F: Filler>(&self, painter: &Painter<'a>, dungeon: &Dungeon, floor_z: i32, filler: &mut FillFn<'a, '_, F>) {
        // Calculate an AABB and corner for the AABB that covers the current floor.
        let floor_corner = dungeon.origin + TILE_SIZE * self.tile_offset;
        let floor_aabb = Aabb {
            min: floor_corner.with_z(floor_z),
            max: (floor_corner + TILE_SIZE * self.tiles.size())
                .with_z(floor_z + self.total_depth()),
        };
        let floor_prim = painter.aabb(floor_aabb);

        // This is copied from `src/layer/mod.rs`. It should be moved into
        // a util file somewhere
        let noisy_color = |color: Rgb<u8>, factor: u32| {
            let nz = RandomField::new(0).get(Vec3::new(floor_corner.x, floor_corner.y, floor_z));
            color.map(|e| {
                (e as u32 + nz % (factor * 2))
                    .saturating_sub(factor)
                    .min(255) as u8
            })
        };

        // Declare the various kinds of blocks that will be used as fills
        let vacant = Block::air(SpriteKind::Empty);
        // FIXME: Lava and stone color hardcoded here, it is available in colors.ron
        // but that file is not accessed from site2 yet
        let lava = Block::new(BlockKind::Lava, noisy_color(Rgb::new(184, 39, 0), 8));
        let stone = Block::new(BlockKind::Rock, Rgb::new(150, 150, 175));
        let stone_purple = Block::new(BlockKind::GlowingRock, Rgb::new(96, 0, 128));

        let wall_thickness = 3.0;
        let tunnel_height = if self.final_level { 16.0 } else { 8.0 };
        let pillar_thickness: i32 = 4;

        // Several primitives and fills use the tile information for finding the nearest
        // wall, a copy of the tilegrid for the floor is stored in an Arc to
        // avoid making a copy for each primitive
        let tiles = /*Arc::new(self.tiles.clone())*/&self.tiles;

        /* // The way the ceiling is curved around corners and near hallways is intricate
        // enough that it's easiest to do with a sampling primitive, this gets
        // masked per room so that it's more efficient to query
        let wall_contours = /* painter.sampling(floor_prim, /*{
            // let tiles = Arc::clone(&tiles);*/
            painter.arena.alloc_with(move || */make_wall_contours(tiles, floor_corner, floor_z, wall_thickness, tunnel_height)/* )
        /*}*/)*/;

        // The surface 1 unit thicker than the walls is used to place the torches onto
        let wall_contour_surface = /*painter.sampling(floor_prim, /*{
            // let tiles = Arc::clone(&tiles);*/
            painter.arena.alloc_with(move || */make_wall_contours(
                tiles,
                floor_corner,
                floor_z,
                wall_thickness + 1.0,
                tunnel_height - 1.0,
            )/*)
        /*}*/)*/;

        // Sprites are randomly positioned and have random kinds, this primitive
        // produces a box of dots that will later get truncated to just the
        // floor, and the corresponding fill places the random kinds where the
        // mask says to
        let floor_sprite = /*painter.sampling(
            floor_prim,
            &*/move |pos| RandomField::new(7331).chance(pos, 0.001) && !wall_contours(pos)
        /*)*/;

        let floor_sprite_fill = filler.sampling(
            painter.arena.alloc_with(move || move |pos| {
                floor_sprite(pos).then(|| Block::air(
                    match (RandomField::new(1337).get(pos) / 2) % 30 {
                        0 => SpriteKind::Apple,
                        1 => SpriteKind::VeloriteFrag,
                        2 => SpriteKind::Velorite,
                        3..=8 => SpriteKind::Mushroom,
                        9..=15 => SpriteKind::FireBowlGround,
                        _ => SpriteKind::ShortGrass,
                    },
                ))
            }),
        ); */

        // The sconces use a sampling-based fill to orient them properly relative to the
        // walls/staircases/pillars
        let light_offset: i32 = 7;
        /* let sconces_wall = /*filler.sampling(painter.arena.alloc_with(move || */move |pos: Vec3<i32>| {
            let rpos = pos.xy() - floor_corner;
            let nearest = tilegrid_nearest_wall(&tiles, rpos);
            let ori = Floor::relative_ori(rpos, nearest.unwrap_or_default());
            Block::air(SpriteKind::WallSconce).with_ori(ori)
        }/*))*/;*/
        // FIXME: Per tile.
        let sconces_inward = move |pos: Vec3<i32>| {
            let rpos = pos.xy() - floor_corner;
            let tile_pos = rpos.map(|e| e.div_euclid(TILE_SIZE));
            let tile_center = tile_pos * TILE_SIZE + TILE_SIZE / 2;
            let ori = Floor::relative_ori(rpos, tile_center);
            Block::air(SpriteKind::WallSconce).with_ori(ori)
        };
        let sconces_inward = filler.sampling(/*painter.arena.alloc_with(move || */&sconces_inward/*)*/);
        let sconces_outward = move |pos: Vec3<i32>| {
            let rpos = pos.xy() - floor_corner;
            let tile_pos = rpos.map(|e| e.div_euclid(TILE_SIZE));
            let tile_center = tile_pos * TILE_SIZE + TILE_SIZE / 2;
            let ori = Floor::relative_ori(tile_center, rpos);
            Block::air(SpriteKind::WallSconce).with_ori(ori)
        };
        let sconces_outward = filler.sampling(/*painter.arena.alloc_with(move || */&sconces_outward/*)*/);/*

        // NOTE: Might be easier to just draw stuff first, then draw the walls over them...
        let lava_within_walls = filler.sampling(painter.arena.alloc_with(move || move |pos: Vec3<i32>| {
            (!wall_contours(pos)).then_some(lava)
        }));
        let vacant_within_walls = filler.sampling(painter.arena.alloc_with(move || move |pos: Vec3<i32>| {
            (!wall_contours(pos)).then_some(vacant)
        }));
        let sconces_layer_fill = filler.sampling(painter.arena.alloc_with(move || move |pos| {
            // NOTE: This intersection feels a bit wasteful...
            (wall_contour_surface(pos) && !wall_contours(pos)).then(|| sconces_wall(pos)).flatten()
        })); */

        // The lighting mask is a grid of thin AABB planes with the same period as the
        // tile grid, but offset by `lighting_offset`, used to space the torches
        // on the walls/pillars/staircases
        let lighting_mask = {
            let floor_w = floor_aabb.max.x - floor_aabb.min.x;

            let lighting_mask_x = painter.union_all((0..floor_w / light_offset).map(|i| {
                let j = floor_corner.x + i * TILE_SIZE + light_offset;
                painter.aabb(Aabb {
                    min: floor_aabb.min.with_x(j - 1),
                    max: floor_aabb.max.with_x(j),
                }).into()
            }));
            let floor_h = floor_aabb.max.y - floor_aabb.min.y;
            let lighting_mask_y = painter.union_all((0..floor_h / light_offset).map(|i| {
                let j = floor_corner.y + i * TILE_SIZE + light_offset;
                painter.aabb(Aabb {
                    min: floor_aabb.min.with_y(j - 1),
                    max: floor_aabb.max.with_y(j),
                }).into()
            }));

            /*
            let mut lighting_mask_x = painter.empty();
            for i in 0..floor_w / light_offset {
                let j = floor_corner.x + i * TILE_SIZE + light_offset;
                let plane = painter.aabb(Aabb {
                    min: floor_aabb.min.with_x(j - 1),
                    max: floor_aabb.max.with_x(j),
                });
                lighting_mask_x = plane.union(lighting_mask_x);
            }
            let floor_h = floor_aabb.max.y - floor_aabb.min.y;
            let mut lighting_mask_y = painter.empty();
            for i in 0..floor_h / light_offset {
                let j = floor_corner.y + i * TILE_SIZE + light_offset;
                let plane = painter.aabb(Aabb {
                    min: floor_aabb.min.with_y(j - 1),
                    max: floor_aabb.max.with_y(j),
                });
                lighting_mask_y = plane.union(lighting_mask_y);
            } */

            lighting_mask_x
                .union(lighting_mask_y)
                .without(lighting_mask_x.intersect(lighting_mask_y))
        };

        // Declare collections of various disjoint primitives that need postprocessing
        // after handling all the local information per-tile
        let mut stairs = Vec::new();
        let mut pillars = Vec::new();
        let mut boss_room_center = None;
        let mut sprites = Vec::new();

        // This loop processes the tile grid, carving out rooms and tunnels and
        // collecting stair/pillar/sprite info to place afterwards
        for (tile_pos, tile) in self.tiles.iter() {
            let tile_corner = dungeon.origin + TILE_SIZE * (self.tile_offset + tile_pos);
            let tile_aabr = Aabr {
                min: tile_corner,
                max: tile_corner + Vec2::broadcast(TILE_SIZE),
            };
            let tile_center = tile_corner + Vec2::broadcast(TILE_SIZE / 2);
            let (mut height, room) = match tile {
                Tile::UpStair(room, _) => (self.hollow_depth, Some(room)),
                Tile::DownStair(room) => (self.hollow_depth, Some(room)),
                Tile::Room(room) => (self.hollow_depth, Some(room)),
                Tile::Tunnel => (tunnel_height as i32, None),
                Tile::Solid => continue,
            };

            // Sprites are contained to the level above the floor, and not within walls
            let sprite_layer = painter.aabb(aabr_with_z(
                tile_aabr,
                floor_z..floor_z + 1,
            ));
            // let sprite_layer_fill = move |pos: Vec3<i32>| !wall_contours(pos);
            /* let sprite_layer = sprite_layer.without(wall_contours); */

            // Lights are 2 units above the floor, and aligned with the `lighting_mask` grid
            let lighting_plane = painter.aabb(aabr_with_z(
                tile_aabr,
                floor_z + 1..floor_z + 2,
            ));
            let lighting_plane = lighting_plane.intersect(lighting_mask);

            let mut chests = None;

            // The way the ceiling is curved around corners and near hallways is intricate
            // enough that it's easiest to do with a sampling primitive, this gets
            // masked per room so that it's more efficient to query
            let wall_contours = /* painter.sampling(floor_prim, /*{
                // let tiles = Arc::clone(&tiles);*/
                painter.arena.alloc_with(move || */make_wall_contours(tiles, /*floor_corner*/tile_corner, tile_pos, floor_z, wall_thickness, tunnel_height)/* )
            /*}*/)*/;

            // The surface 1 unit thicker than the walls is used to place the torches onto
            let wall_contour_surface = /*painter.sampling(floor_prim, /*{
                // let tiles = Arc::clone(&tiles);*/
                painter.arena.alloc_with(move || */make_wall_contours(
                    tiles,
                    /*floor_corner*/tile_corner,
                    tile_pos,
                    floor_z,
                    wall_thickness + 1.0,
                    tunnel_height - 1.0,
                )/*)
            /*}*/)*/;

            // Sprites are randomly positioned and have random kinds, this primitive
            // produces a box of dots that will later get truncated to just the
            // floor, and the corresponding fill places the random kinds where the
            // mask says to
            let floor_sprite = /*painter.sampling(
                floor_prim,
                &*/move |pos| RandomField::new(7331).chance(pos, 0.001) && !wall_contours(pos)
            /*)*/;

            /* fn make_fill(fill: impl Fn(Vec3<i32>) -> Option<Block>) -> impl Fill {
                move |pos, _| fill(pos)
            } */
            let floor_sprite_fill = /*make_fill*/filler.sampling(/*filler.sampling(*/
                painter.arena.alloc_with(move || move |pos| {
                    floor_sprite(pos).then(|| Block::air(
                        match (RandomField::new(1337).get(pos) / 2) % 30 {
                            0 => SpriteKind::Apple,
                            1 => SpriteKind::VeloriteFrag,
                            2 => SpriteKind::Velorite,
                            3..=8 => SpriteKind::Mushroom,
                            9..=15 => SpriteKind::FireBowlGround,
                            _ => SpriteKind::ShortGrass,
                        },
                    ))
                }) as &dyn Fn(_) -> _/*,
            )*/);

            // The sconces use a sampling-based fill to orient them properly relative to the
            // walls/staircases/pillars
            let sconces_wall = /*filler.sampling(painter.arena.alloc_with(move || */move |pos: Vec3<i32>| {
                let rpos = pos.xy() - floor_corner;
                let nearest = tilegrid_nearest_wall(&tiles, rpos);
                let ori = Floor::relative_ori(rpos, nearest.unwrap_or_default());
                Block::air(SpriteKind::WallSconce).with_ori(ori)
            }/*))*/;
            /* let sconces_inward = filler.sampling(painter.arena.alloc_with(move || move |pos: Vec3<i32>| {
                let rpos = pos.xy() - floor_corner;
                let tile_pos = rpos.map(|e| e.div_euclid(TILE_SIZE));
                let tile_center = tile_pos * TILE_SIZE + TILE_SIZE / 2;
                let ori = Floor::relative_ori(rpos, tile_center);
                Block::air(SpriteKind::WallSconce).with_ori(ori)
            }));
            let sconces_outward = filler.sampling(painter.arena.alloc_with(move || move |pos: Vec3<i32>| {
                let rpos = pos.xy() - floor_corner;
                let tile_pos = rpos.map(|e| e.div_euclid(TILE_SIZE));
                let tile_center = tile_pos * TILE_SIZE + TILE_SIZE / 2;
                let ori = Floor::relative_ori(tile_center, rpos);
                Block::air(SpriteKind::WallSconce).with_ori(ori)
            })); */

            // NOTE: Might be easier to just draw stuff first, then draw the walls over them...
            let lava_within_walls = move |pos: Vec3<i32>| {
                (!wall_contours(pos)).then_some(lava)
            };
            let lava_within_walls = filler.sampling(/*painter.arena.alloc_with(move || */&lava_within_walls/*)*/);
            let vacant_within_walls = move |pos: Vec3<i32>| {
                (!wall_contours(pos)).then_some(vacant)
            };
            let vacant_within_walls = filler.sampling(/*painter.arena.alloc_with(move || */&vacant_within_walls/*)*/);
            let sconces_layer_fill = move |pos| {
                // NOTE: This intersection feels a bit wasteful...
                (!wall_contours(pos) && wall_contour_surface(pos)).then(|| sconces_wall(pos)).flatten()
            };
            let sconces_layer_fill = filler.sampling(/*painter.arena.alloc_with(move || */&sconces_layer_fill/*)*/);

            if let Some(room) = room.map(|i| self.rooms.get(*i)) {
                height = height.min(room.height);
                if let Tile::UpStair(_, kind) = tile {
                    // Construct the staircase that connects this tile to the matching DownStair
                    // tile on the floor above (or to the surface if this is the top floor), and a
                    // hollow bounding box to place air in
                    let center = tile_center.with_z(floor_z);
                    let radius = TILE_SIZE as f32 / 2.0;
                    let aabb = aabr_with_z(tile_aabr, floor_z..floor_z + self.total_depth());
                    let bb = match kind {
                        StairsKind::Spiral => painter.cylinder(aabb),
                        StairsKind::WallSpiral => painter.aabb(aabb).as_kind(),
                    };
                    let stair = /*filler.sampling/*painter.sampling*/(*//*bb, */filler.sampling(match kind {
                        StairsKind::Spiral => &*painter.arena.alloc_with(move || {
                            let f = spiral_staircase(center, radius, 0.5, 9.0);
                            move |pos| f(pos).then_some(stone)
                        }) as &dyn Fn(_) -> _,
                        StairsKind::WallSpiral => &*painter.arena.alloc_with(move || {
                            let f = wall_staircase(center, radius, 27.0);
                            move |pos| f(pos).then_some(stone)
                        }) as &dyn Fn(_) -> _,
                    }/*)*/);
                    // Construct the lights that go inside the staircase, starting above the
                    // ceiling to avoid placing them floating in mid-air
                    let mut lights = painter.empty();
                    for i in height..self.total_depth() {
                        if i % 9 == 0 {
                            let mut light = painter.aabb(Aabb {
                                min: aabb.min.with_z(floor_z + i),
                                max: aabb.max.with_z(floor_z + i + 1),
                            });
                            let inner = painter.aabb(Aabb {
                                min: (aabb.min + Vec3::new(1, 1, 0)).with_z(floor_z + i),
                                max: (aabb.max - Vec3::new(1, 1, 0)).with_z(floor_z + i + 1),
                            });

                            light = light.without(inner);
                            lights = light.union(lights);
                        }
                    }
                    lights = lights.intersect(lighting_mask);
                    stairs.push((bb, stair, lights));
                }

                if matches!(tile, Tile::Room(_) | Tile::DownStair(_)) {
                    let seed = room.seed;
                    let loot_density = room.loot_density;
                    let difficulty = room.difficulty;
                    // Place chests with a random distribution based on the
                    // room's loot density in valid sprite locations,
                    // filled based on the room's difficulty
                    let chest_sprite = /*painter.sampling(
                        sprite_layer,
                        painter.arena.alloc_with(move || */move |pos| RandomField::new(seed).chance(pos, loot_density * 0.5) && !wall_contours(pos)/*),
                    )*/;
                    let chest_sprite_fill = /*filler.block(*/Block::air(match difficulty {
                        0 => SpriteKind::DungeonChest0,
                        1 => SpriteKind::DungeonChest1,
                        2 => SpriteKind::DungeonChest2,
                        3 => SpriteKind::DungeonChest3,
                        4 => SpriteKind::DungeonChest4,
                        5 => SpriteKind::DungeonChest5,
                        _ => SpriteKind::Chest,
                    }/*)*/);
                    let chest_sprite_fill = /*make_fill*/filler.sampling(/*filler.sampling(*/
                        painter.arena.alloc_with(move || move |pos| chest_sprite(pos).then_some(chest_sprite_fill))/*,
                    )*/ as &dyn Fn(_) -> _);
                    chests = Some(/*(chest_sprite, */chest_sprite_fill/*)*/);

                    // If a room has pits, place them
                    if room.pits.is_some() {
                        // Make an air pit
                        let tile_pit = painter.aabb(aabr_with_z(
                            tile_aabr,
                            floor_z - 7..floor_z,
                        ));
                        /* let tile_pit_fill = filler.sampling(
                            painter.arena.alloc_with(move || move |pos| 
                        );
                        let tile_pit = tile_pit.without(wall_contours); */
                        painter.fill(tile_pit, /*filler.block(vacant)*/vacant_within_walls, filler);

                        // Fill with lava
                        let tile_lava = painter.aabb(aabr_with_z(
                            tile_aabr,
                            floor_z - 7..floor_z - 5,
                        ));
                        /* let tile_lava = tile_lava.without(wall_contours); */
                        //pits.push(tile_pit);
                        //pits.push(tile_lava);
                        painter.fill(tile_lava, /*filler.block(lava)*/lava_within_walls, filler);
                    }
                    if room
                        .pits
                        .map(|pit_space| {
                            tile_pos.map(|e| e.rem_euclid(pit_space) == 0).reduce_and()
                        })
                        .unwrap_or(false)
                    {
                        let platform = painter.aabb(Aabb {
                            min: (tile_center - Vec2::broadcast(pillar_thickness - 1))
                                .with_z(floor_z - 7),
                            max: (tile_center + Vec2::broadcast(pillar_thickness)).with_z(floor_z),
                        });
                        painter.fill(platform, filler.block(stone), filler);
                    }

                    // If a room has pillars, the current tile aligns with the pillar spacing, and
                    // we're not too close to a wall (i.e. the adjacent tiles are rooms and not
                    // hallways/solid), place a pillar
                    if room
                        .pillars
                        .map(|pillar_space| {
                            tile_pos
                                .map(|e| e.rem_euclid(pillar_space) == 0)
                                .reduce_and()
                        })
                        .unwrap_or(false)
                        && DIRS
                            .iter()
                            .map(|dir| tile_pos + *dir)
                            .all(|other_tile_pos| {
                                matches!(self.tiles.get(other_tile_pos), Some(Tile::Room(_)))
                            })
                    {
                        let mut pillar = painter.cylinder(Aabb {
                            min: (tile_center - Vec2::broadcast(pillar_thickness - 1))
                                .with_z(floor_z),
                            max: (tile_center + Vec2::broadcast(pillar_thickness))
                                .with_z(floor_z + height),
                        });
                        let base = painter.cylinder(Aabb {
                            min: (tile_center - Vec2::broadcast(1 + pillar_thickness - 1))
                                .with_z(floor_z),
                            max: (tile_center + Vec2::broadcast(1 + pillar_thickness))
                                .with_z(floor_z + 1),
                        });

                        /* let scale = (pillar_thickness + 2) as f32 / pillar_thickness as f32;
                        let mut lights = painter
                            .scale(pillar, Vec2::broadcast(scale).with_z(1.0)); */
                        let mut lights = painter.cylinder(Aabb {
                            min: (tile_center - Vec2::broadcast(2 + pillar_thickness - 1))
                                .with_z(floor_z),
                            max: (tile_center + Vec2::broadcast(2 + pillar_thickness))
                                .with_z(floor_z + height),
                        });
                        lights = lighting_plane.as_kind().intersect(lights);
                        // Only add the base (and shift the lights up)
                        // for boss-rooms pillars
                        if room.kind == RoomKind::Boss {
                            lights = lights.translate(3 * Vec3::unit_z());
                            pillar = pillar.union(base);
                        }
                        pillars.push((tile_center, pillar, lights));
                    }
                }

                // Keep track of the boss room to be able to add decorations later
                if room.kind == RoomKind::Boss {
                    boss_room_center =
                        Some(floor_corner + TILE_SIZE * room.area.center() + TILE_SIZE / 2);
                }
            }

            // Carve out the room's air inside the walls
            let tile_air = painter.aabb(aabr_with_z(
                tile_aabr,
                floor_z..floor_z + height,
            ));
            /* let tile_air_fill = filler.sampling(painter.arena.alloc_with(move || move |pos| {
                (!wall_contours(pos)).then_some(vacant)
            })); */
            // let tile_air = tile_air.without(wall_contours);
            painter.fill(tile_air, /*filler.block(vacant)*//*tile_air_fill*/vacant_within_walls, filler);

            // Place torches on the walls with the aforementioned spacing
            let sconces_layer = /*tile_air.intersect(*/lighting_plane/*)*/;
            /* let sconces_layer =
                sconces_layer.as_kind().intersect(wall_contour_surface); */
            /* let sconces_layer_fill = filler.sampling(painter.arena.alloc_with(move || move |pos| {
                // NOTE: This intersection feels a bit wasteful...
                (wall_contour_surface(pos) && !wall_contours(pos)).then(|| sconces_wall(pos))
            })); */
            painter.fill(sconces_layer, /*sconces_wall*/sconces_layer_fill, filler);

            // Defer chest/floor sprite placement
            if let Some(/*(chest_sprite, */chest_sprite_fill/*)*/) = chests {
                // let chest_sprite = chest_sprite.without(wall_contours);
                sprites.push((sprite_layer, chest_sprite_fill));
            }

            /* let floor_sprite = sprite_layer.as_kind().intersect(floor_sprite); */
            sprites.push((sprite_layer, floor_sprite_fill));
        }

        // Place a glowing purple heptagonal star inscribed in a circle in the boss room
        if let Some(boss_room_center) = boss_room_center {
            let magic_circle_bb = painter.cylinder(Aabb {
                min: (boss_room_center - 3 * Vec2::broadcast(TILE_SIZE) / 2).with_z(floor_z - 1),
                max: (boss_room_center + 3 * Vec2::broadcast(TILE_SIZE) / 2).with_z(floor_z),
            });
            let magic_circle = {
                    let f = inscribed_polystar(boss_room_center, 1.4 * TILE_SIZE as f32, 7);
                    move |pos| f(pos).then_some(stone_purple)
                };
            let magic_circle = /*painter.sampling*/filler.sampling(
                // magic_circle_bb,
                /* painter.arena.alloc_with(move || */&magic_circle/*),*/
            );
            painter.fill(/*magic_circle*/magic_circle_bb, /*filler.block(stone_purple)*/magic_circle, filler);
        }

        // Place pillars and pillar lights facing the pillars
        for (pos, pillar, lights) in pillars.iter() {
            // Avoid placing pillars that would cover the heptagonal star
            if let Some(boss_room_center) = boss_room_center {
                if pos.distance_squared(boss_room_center) < (3 * TILE_SIZE / 2).pow(2) {
                    continue;
                }
            }
            painter.fill(*lights, sconces_inward, filler);
            painter.fill(*pillar, filler.block(stone), filler);
        }
        // Carve out space for the stairs
        for (stair_bb, _, _) in stairs.iter() {
            painter.fill(*stair_bb, filler.block(vacant), filler);
            /* // Prevent sprites from floating above the stairs
            //
            // NOTE: This is *mostly* equivalent to a Z test, since sprites are always drawn in
            // single-voxel ground layers, except that cylindrical stairs can generate
            // non-convex differences with a tile.  For now we leave it as is, but it could
            // probably be made a lot cheaper (and we could likely avoid the difference entirely by
            // just preventing sprites from spawning in the tile around staircases, whether or not
            // they were a cylinder).
            let stair_bb_up = stair_bb.translate(Vec3::unit_z());
            for (sprite, _) in sprites.iter_mut() {
                /* *sprite = */sprite.without(stair_bb_up);
            } */
        }
        // Place the stairs themselves, and lights within the stairwells
        let stairs_bb_up = painter.union_all(stairs.into_iter().map(|(stair_bb, stair, lights)| {
            painter.fill(lights, sconces_outward, filler);
            painter.fill(stair_bb, stair, filler);
            stair_bb
        })).translate(Vec3::unit_z());
        // Place the sprites
        for (sprite, sprite_fill) in sprites.into_iter() {
            /* let sprite = */sprite.without(stairs_bb_up);
            painter.fill(sprite, sprite_fill, filler);
        }
    }
}

impl<F: Filler> SiteStructure<F> for Dungeon {
    fn render<'a>(&self, _site: &Site, land: Land, painter: &Painter<'a>, filler: &mut FillFn<'a, '_, F>) {
        let origin = (self.origin + Vec2::broadcast(TILE_SIZE / 2)).with_z(self.alt + ALT_OFFSET);

        lazy_static! {
            pub static ref JUNGLE: AssetHandle<StructuresGroup> =
                Structure::load_group("dungeon_entrances.jungle");
            pub static ref GRASSLAND: AssetHandle<StructuresGroup> =
                Structure::load_group("dungeon_entrances.grassland");
            pub static ref DESERT: AssetHandle<StructuresGroup> =
                Structure::load_group("dungeon_entrances.desert");
        }

        let biome = land
            .get_chunk_wpos(self.origin)
            .map_or(BiomeKind::Void, |c| c.get_biome());
        let entrances = match biome {
            BiomeKind::Jungle => *JUNGLE,
            BiomeKind::Desert => *DESERT,
            _ => *GRASSLAND,
        };
        let entrances = entrances.get();
        let entrance = &entrances[self.seed as usize % entrances.len()];

        let entrance_prim = painter.prefab(entrance);
        let entrance_prim = entrance_prim.translate(origin);
        painter.fill(
            entrance_prim,
            filler.prefab(entrance, origin, self.seed),
            filler,
        );

        let mut z = self.alt + ALT_OFFSET;
        for floor in &self.floors {
            z -= floor.total_depth();

            floor.render(painter, self, z, filler);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_creating_bosses() {
        let mut dynamic_rng = thread_rng();
        let tile_wcenter = Vec3::new(0, 0, 0);
        boss_1(&mut dynamic_rng, tile_wcenter);
        boss_2(&mut dynamic_rng, tile_wcenter);
        boss_3(&mut dynamic_rng, tile_wcenter);
        boss_4(&mut dynamic_rng, tile_wcenter);
        boss_5(&mut dynamic_rng, tile_wcenter);
        boss_fallback(&mut dynamic_rng, tile_wcenter);
    }

    #[test]
    // FIXME: Uses random, test may be not great
    fn test_creating_enemies() {
        let mut dynamic_rng = thread_rng();
        let random_position = Vec3::new(0, 0, 0);
        enemy_1(&mut dynamic_rng, random_position);
        enemy_2(&mut dynamic_rng, random_position);
        enemy_3(&mut dynamic_rng, random_position);
        enemy_4(&mut dynamic_rng, random_position);
        enemy_5(&mut dynamic_rng, random_position);
        enemy_fallback(&mut dynamic_rng, random_position);
    }

    #[test]
    // FIXME: Uses random, test may be not great
    fn test_creating_minibosses() {
        let mut dynamic_rng = thread_rng();
        let tile_wcenter = Vec3::new(0, 0, 0);
        mini_boss_1(&mut dynamic_rng, tile_wcenter);
        mini_boss_2(&mut dynamic_rng, tile_wcenter);
        mini_boss_3(&mut dynamic_rng, tile_wcenter);
        mini_boss_4(&mut dynamic_rng, tile_wcenter);
        mini_boss_5(&mut dynamic_rng, tile_wcenter);
        mini_boss_fallback(&mut dynamic_rng, tile_wcenter);
    }

    #[test]
    fn test_creating_turrets() {
        let mut dynamic_rng = thread_rng();
        let pos = Vec3::new(0.0, 0.0, 0.0);
        turret_3(&mut dynamic_rng, pos);
        turret_5(&mut dynamic_rng, pos);
    }
}
