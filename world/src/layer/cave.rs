use super::scatter::close;
use crate::{
    util::{sampler::Sampler, RandomField, LOCALITY},
    Canvas, ColumnSample, Land,
};
use common::{
    terrain::{
        quadratic_nearest_point, river_spline_coeffs, Block, BlockKind, SpriteKind,
        TerrainChunkSize,
    },
    vol::RectVolSize,
};
use noise::{Fbm, NoiseFn};
use rand::prelude::*;
use std::{
    cmp::Ordering,
    f64::consts::PI,
    ops::{Add, Mul, Range, Sub},
};
use vek::*;

const CELL_SIZE: i32 = 1024;

#[derive(Copy, Clone)]
pub struct Node {
    pub wpos: Vec3<i32>,
}

fn to_cell(wpos: Vec2<i32>, level: u32) -> Vec2<i32> {
    (wpos + (level & 1) as i32 * CELL_SIZE / 2).map(|e| e.div_euclid(CELL_SIZE))
}
fn to_wpos(cell: Vec2<i32>, level: u32) -> Vec2<i32> {
    (cell * CELL_SIZE) - (level & 1) as i32 * CELL_SIZE / 2
}

const AVG_LEVEL_DEPTH: i32 = 120;
const LAYERS: u32 = 4;

fn node_at(cell: Vec2<i32>, level: u32, land: &Land) -> Option<Node> {
    let rand = RandomField::new(37 + level);

    if rand.chance(cell.with_z(0), 0.85) || level == 0 {
        let dx = RandomField::new(38 + level);
        let dy = RandomField::new(39 + level);
        let wpos = to_wpos(cell, level)
            + CELL_SIZE as i32 / 4
            + (Vec2::new(dx.get(cell.with_z(0)), dy.get(cell.with_z(0))) % CELL_SIZE as u32 / 2)
                .map(|e| e as i32);
        land.get_chunk_wpos(wpos).and_then(|chunk| {
            let alt = chunk.alt as i32 + 8 - AVG_LEVEL_DEPTH * level as i32;

            if level > 0
                || (!chunk.near_cliffs()
                    && !chunk.river.near_water()
                    && chunk.sites.is_empty()
                    && land.get_gradient_approx(wpos) < 0.75)
            {
                Some(Node {
                    wpos: wpos.with_z(alt),
                })
            } else {
                None
            }
        })
    } else {
        None
    }
}

pub fn surface_entrances<'a>(land: &'a Land) -> impl Iterator<Item = Vec2<i32>> + 'a {
    let sz_cells = to_cell(
        land.size()
            .map2(TerrainChunkSize::RECT_SIZE, |e, sz| (e * sz) as i32),
        0,
    );
    (0..sz_cells.x + 1)
        .flat_map(move |x| (0..sz_cells.y + 1).map(move |y| Vec2::new(x, y)))
        .filter_map(|cell| {
            let tunnel = tunnel_below_from_cell(cell, 0, land)?;
            // Hacky, moves the entrance position closer to the actual entrance
            Some(Lerp::lerp(tunnel.a.wpos.xy(), tunnel.b.wpos.xy(), 0.125))
        })
}

struct Tunnel {
    a: Node,
    b: Node,
    curve: f32,
}

fn tunnels_at<'a>(
    wpos: Vec2<i32>,
    level: u32,
    land: &'a Land,
) -> impl Iterator<Item = Tunnel> + 'a {
    let rand = RandomField::new(37 + level);
    let col_cell = to_cell(wpos, level);
    LOCALITY
        .into_iter()
        .filter_map(move |rpos| {
            let current_cell_pos = col_cell + rpos;
            Some(current_cell_pos).zip(node_at(current_cell_pos, level, land))
        })
        .flat_map(move |(current_cell_pos, current_cell)| {
            [Vec2::new(1, 1), Vec2::new(1, -1)]
                .into_iter()
                .filter(move |rpos| {
                    let mid = (current_cell_pos * 2 + rpos) / 2;
                    rand.chance(mid.with_z(0), 0.5) ^ (rpos.y == -1)
                })
                .chain([Vec2::new(1, 0), Vec2::new(0, 1)])
                .filter_map(move |rpos| {
                    let other_cell_pos = current_cell_pos + rpos;
                    Some(other_cell_pos).zip(node_at(other_cell_pos, level, land))
                })
                .filter(move |(other_cell_pos, _)| {
                    rand.chance((current_cell_pos + other_cell_pos).with_z(7), 0.3)
                })
                .map(move |(other_cell_pos, other_cell)| Tunnel {
                    a: current_cell,
                    b: other_cell,
                    curve: RandomField::new(13)
                        .get_f32(current_cell.wpos.xy().with_z(0))
                        .powf(0.25) as f32,
                })
        })
}

fn tunnel_below_from_cell(cell: Vec2<i32>, level: u32, land: &Land) -> Option<Tunnel> {
    let wpos = to_wpos(cell, level);
    Some(Tunnel {
        a: node_at(to_cell(wpos, level), level, land)?,
        b: node_at(
            to_cell(wpos + CELL_SIZE as i32 / 2, level + 1),
            level + 1,
            land,
        )?,
        curve: 0.0,
    })
}

fn tunnels_down_from<'a>(
    wpos: Vec2<i32>,
    level: u32,
    land: &'a Land,
) -> impl Iterator<Item = Tunnel> + 'a {
    let col_cell = to_cell(wpos, level);
    LOCALITY
        .into_iter()
        .filter_map(move |rpos| tunnel_below_from_cell(col_cell + rpos, level, land))
}

pub fn apply_caves_to(canvas: &mut Canvas, rng: &mut impl Rng) {
    let nz = Fbm::new();
    let info = canvas.info();
    canvas.foreach_col(|canvas, wpos2d, col| {
        let wposf = wpos2d.map(|e| e as f64 + 0.5);
        let land = info.land();

        for level in 1..LAYERS + 1 {
            let rand = RandomField::new(37 + level);
            let tunnel_bounds = tunnels_at(wpos2d, level, &land)
                .chain(tunnels_down_from(wpos2d, level - 1, &land))
                .filter_map(|tunnel| {
                    let start = tunnel.a.wpos.xy().map(|e| e as f64 + 0.5);
                    let end = tunnel.b.wpos.xy().map(|e| e as f64 + 0.5);
                    let dist = LineSegment2 { start, end }
                        .distance_to_point(wpos2d.map(|e| e as f64 + 0.5));

                    let curve_dir =
                        (RandomField::new(14).get_f32(tunnel.a.wpos.xy().with_z(0)) as f64 - 0.5)
                            .signum();

                    if let Some((t, closest, _)) = quadratic_nearest_point(
                        &river_spline_coeffs(
                            start,
                            ((end - start) * 0.5
                                + ((end - start) * 0.5).rotated_z(PI / 2.0)
                                    * 6.0
                                    * tunnel.curve as f64
                                    * curve_dir)
                                .map(|e| e as f32),
                            end,
                        ),
                        wposf,
                        Vec2::new(start, end),
                    ) {
                        let dist = closest.distance(wposf);
                        if dist < 64.0 {
                            let tunnel_len = tunnel
                                .a
                                .wpos
                                .map(|e| e as f64)
                                .distance(tunnel.b.wpos.map(|e| e as f64));
                            let radius = Lerp::lerp(
                                6.0,
                                32.0,
                                (nz.get((wposf / 200.0).into_array()) * 2.0 * 0.5 + 0.5)
                                    .clamped(0.0, 1.0),
                            ); // Lerp::lerp(8.0, 24.0, (t * 0.075 * tunnel_len).sin() * 0.5 + 0.5);
                            let height_here = (1.0 - dist / radius).max(0.0).powf(0.3) * radius;
                            if height_here > 0.0 {
                                let z_offs = nz.get((wposf / 512.0).into_array())
                                    * 48.0
                                    * ((1.0 - (t - 0.5).abs() * 2.0) * 8.0).min(1.0);
                                let depth =
                                    Lerp::lerp(tunnel.a.wpos.z as f64, tunnel.b.wpos.z as f64, t)
                                        + z_offs;
                                Some((
                                    (depth - height_here * 0.3) as i32,
                                    (depth + height_here * 1.35) as i32,
                                    z_offs,
                                ))
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                });

            for (min, max, z_offs) in tunnel_bounds {
                // Avoid cave entrances intersecting water
                let z_range = Lerp::lerp(
                    max,
                    min,
                    1.0 - (1.0 - ((col.alt - col.water_level) / 4.0).clamped(0.0, 1.0))
                        * (1.0 - ((col.alt - max as f32) / 8.0).clamped(0.0, 1.0)),
                )..max;
                write_column(canvas, col, level, wpos2d, z_range, z_offs, rng);
            }
        }
    });
}

struct Biome {
    humidity: f32,
    temp: f32,
    mineral: f32,
}

fn write_column(
    canvas: &mut Canvas,
    col: &ColumnSample,
    level: u32,
    wpos2d: Vec2<i32>,
    z_range: Range<i32>,
    z_offs: f64,
    rng: &mut impl Rng,
) {
    let info = canvas.info();

    // Exposed to the sky
    let exposed = z_range.end as f32 > col.alt;
    // Below the ground
    let below = ((col.alt - z_range.start as f32) / 50.0).clamped(0.0, 1.0);

    let biome = Biome {
        humidity: Lerp::lerp(
            col.humidity,
            info.index()
                .noise
                .cave_nz
                .get(wpos2d.map(|e| e as f64 / 1024.0).into_array()) as f32,
            below,
        ),
        temp: Lerp::lerp(
            col.temp,
            info.index()
                .noise
                .cave_nz
                .get(wpos2d.map(|e| e as f64 / 2048.0).into_array())
                .add(
                    ((col.alt as f64 - z_range.start as f64)
                        / (AVG_LEVEL_DEPTH as f64 * LAYERS as f64 * 0.8))
                        .clamped(0.0, 2.0),
                ) as f32,
            below,
        ),
        mineral: info
            .index()
            .noise
            .cave_nz
            .get(wpos2d.map(|e| e as f64 / 256.0).into_array())
            .mul(0.5)
            .add(0.5) as f32,
    };

    let underground = ((col.alt as f32 - z_range.end as f32) / 80.0).clamped(0.0, 1.0);

    let [_, biome_mushroom, biome_fire, biome_leafy, biome_dusty] = {
        let barren = 0.01;
        let mushroom =
            underground * close(biome.humidity, 1.0, 0.75) * close(biome.temp, 0.25, 1.2);
        let fire = underground * close(biome.humidity, 0.0, 0.75) * close(biome.temp, 2.0, 0.65);
        let leafy = underground * close(biome.humidity, 1.0, 0.75) * close(biome.temp, -0.1, 0.75);
        let dusty = underground * close(biome.humidity, 0.0, 0.5) * close(biome.temp, -0.3, 0.65);

        let biomes = [barren, mushroom, fire, leafy, dusty];
        let max = biomes
            .into_iter()
            .max_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal))
            .unwrap();
        biomes.map(|e| (e / max).powf(4.0))
    };

    let stalactite = {
        let cavern_height = (z_range.end - z_range.start) as f64;
        info
            .index()
            .noise
            .cave_nz
            .get(wpos2d.map(|e| e as f64 / 16.0).into_array())
            .sub(0.5)
            .max(0.0)
            .mul(2.0)
            // No stalactites near entrances
            .mul(((col.alt as f64 - z_range.end as f64) / 8.0).clamped(0.0, 1.0))
            .mul(8.0 + cavern_height * 0.4)
    };

    let lava = {
        info.index()
            .noise
            .cave_nz
            .get(wpos2d.map(|e| e as f64 / 64.0).into_array())
            .sub(0.5)
            .abs()
            .sub(0.2)
            .min(0.0)
            // .mul((biome.temp as f64 - 1.5).mul(30.0).clamped(0.0, 1.0))
            .mul((biome_fire as f64 - 0.5).mul(30.0).clamped(0.0, 1.0))
            .mul(64.0)
            .max(-32.0)
    };

    let rand = RandomField::new(37 + level);

    let dirt = if exposed { 0 } else { 1 };
    let bedrock = z_range.start + lava as i32;
    let base = bedrock + (stalactite * 0.4) as i32;
    let floor = base + dirt;
    let ceiling = z_range.end - stalactite as i32;
    for z in bedrock..z_range.end {
        let wpos = wpos2d.with_z(z);
        canvas.map(wpos, |block| {
            if !block.is_filled() {
                block.into_vacant()
            } else if z < z_range.start - 4 {
                Block::new(BlockKind::Lava, Rgb::new(255, 65, 0))
            } else if z < base || z >= ceiling {
                let stalactite: Rgb<i16> =
                    Lerp::lerp(Rgb::new(80, 100, 150), Rgb::new(0, 75, 200), biome_mushroom);
                Block::new(
                    if rand.chance(wpos, biome_mushroom * biome.mineral) {
                        BlockKind::GlowingWeakRock
                    } else {
                        BlockKind::WeakRock
                    },
                    stalactite.map(|e| e as u8),
                )
            } else if z >= base && z < floor {
                let dry_mud =
                    Lerp::lerp(Rgb::new(40, 20, 0), Rgb::new(80, 80, 30), col.marble_small);
                let mycelium =
                    Lerp::lerp(Rgb::new(20, 65, 175), Rgb::new(20, 100, 80), col.marble_mid);
                let fire_rock =
                    Lerp::lerp(Rgb::new(120, 50, 20), Rgb::new(50, 5, 40), col.marble_small);
                let grassy = Lerp::lerp(
                    Rgb::new(0, 100, 50),
                    Rgb::new(80, 100, 20),
                    col.marble_small,
                );
                let dusty = Lerp::lerp(Rgb::new(50, 50, 75), Rgb::new(75, 75, 50), col.marble_mid);
                let surf_color: Rgb<i16> = Lerp::lerp(
                    Lerp::lerp(
                        Lerp::lerp(
                            Lerp::lerp(dry_mud, dusty, biome_dusty),
                            mycelium,
                            biome_mushroom,
                        ),
                        grassy,
                        biome_leafy,
                    ),
                    fire_rock,
                    biome_fire,
                );

                Block::new(BlockKind::Sand, surf_color.map(|e| e as u8))
            } else if let Some(sprite) = (z == floor && !exposed)
                .then(|| {
                    if rand.chance(wpos2d.with_z(1), biome_mushroom * 0.1) {
                        Some(
                            [
                                (SpriteKind::CaveMushroom, 0.3),
                                (SpriteKind::Mushroom, 0.3),
                                (SpriteKind::GrassBlue, 1.0),
                                (SpriteKind::CavernGrassBlueShort, 1.0),
                                (SpriteKind::CavernGrassBlueMedium, 1.0),
                                (SpriteKind::CavernGrassBlueLong, 1.0),
                            ]
                            .choose_weighted(rng, |(_, w)| *w)
                            .unwrap()
                            .0,
                        )
                    } else if rand.chance(wpos2d.with_z(1), biome_leafy * 0.25) {
                        Some(
                            [
                                (SpriteKind::LongGrass, 1.0),
                                (SpriteKind::MediumGrass, 2.0),
                                (SpriteKind::ShortGrass, 2.0),
                                (SpriteKind::JungleFern, 0.5),
                                (SpriteKind::JungleLeafyPlant, 0.5),
                                (SpriteKind::JungleRedGrass, 0.35),
                                (SpriteKind::Mushroom, 0.3),
                                (SpriteKind::EnsnaringVines, 0.2),
                                (SpriteKind::Fern, 0.75),
                                (SpriteKind::LeafyPlant, 0.8),
                            ]
                            .choose_weighted(rng, |(_, w)| *w)
                            .unwrap()
                            .0,
                        )
                    } else if rand.chance(wpos2d.with_z(2), biome_dusty * 0.01) {
                        Some(
                            [
                                (SpriteKind::Bones, 0.5),
                                (SpriteKind::Stones, 1.5),
                                (SpriteKind::DeadBush, 1.0),
                                (SpriteKind::EnsnaringWeb, 0.5),
                                (SpriteKind::Mud, 0.025),
                            ]
                            .choose_weighted(rng, |(_, w)| *w)
                            .unwrap()
                            .0,
                        )
                    } else if rand.chance(
                        wpos2d.with_z(3),
                        close(biome.humidity, 0.0, 0.5) * biome.mineral * 0.005,
                    ) {
                        Some(SpriteKind::CrystalLow)
                    } else if rand.chance(wpos2d.with_z(4), biome_fire * 0.0003) {
                        Some(SpriteKind::Pyrebloom)
                    } else if rand.chance(wpos2d.with_z(5), close(biome.mineral, 1.0, 0.5) * 0.001)
                    {
                        Some(
                            *[
                                SpriteKind::Velorite,
                                SpriteKind::VeloriteFrag,
                                SpriteKind::AmethystSmall,
                                SpriteKind::TopazSmall,
                                SpriteKind::DiamondSmall,
                                SpriteKind::RubySmall,
                                SpriteKind::EmeraldSmall,
                                SpriteKind::SapphireSmall,
                            ]
                            .choose(rng)
                            .unwrap(),
                        )
                    } else if rand.chance(wpos2d.with_z(6), 0.0002) {
                        [
                            (Some(SpriteKind::DungeonChest0), 1.0),
                            (Some(SpriteKind::DungeonChest1), 0.3),
                            (Some(SpriteKind::DungeonChest2), 0.1),
                            (Some(SpriteKind::DungeonChest3), 0.03),
                            (Some(SpriteKind::DungeonChest4), 0.01),
                            (Some(SpriteKind::DungeonChest5), 0.003),
                            (None, 1.0),
                        ]
                        .choose_weighted(rng, |(_, w)| *w)
                        .unwrap()
                        .0
                    } else {
                        None
                    }
                })
                .flatten()
            {
                Block::air(sprite)
            } else if let Some(sprite) = (z == ceiling - 1)
                .then(|| {
                    if rand.chance(wpos2d.with_z(3), biome_mushroom * 0.02) {
                        Some(
                            *[
                                SpriteKind::CavernMycelBlue,
                                SpriteKind::CeilingMushroom,
                                SpriteKind::Orb,
                            ]
                            .choose(rng)
                            .unwrap(),
                        )
                    } else if rand.chance(wpos2d.with_z(4), 0.0075) {
                        Some(
                            *[SpriteKind::CrystalHigh, SpriteKind::Liana]
                                .choose(rng)
                                .unwrap(),
                        )
                    } else {
                        None
                    }
                })
                .flatten()
            {
                Block::air(sprite)
            } else {
                Block::air(SpriteKind::Empty)
            }
        });
    }
}
