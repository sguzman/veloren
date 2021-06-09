use super::{glide_wield, utils::handle_climb};
use crate::{
    comp::{inventory::slot::EquipSlot, CharacterState, ControllerInputs, Ori, StateUpdate, Vel},
    glider::Glider,
    states::behavior::{CharacterBehavior, JoinData},
    util::{Dir, Plane, Projection},
};
use inline_tweak::tweak;
use serde::{Deserialize, Serialize};
use std::f32::consts::PI;
use vek::*;

pub const BASE_PITCH: f32 = 0.06 * PI; // 10.8°

#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Data {
    pub glider: Glider,
    inputs_disabled: bool,
}

impl Data {
    pub fn new(glider: Glider) -> Self {
        Self {
            inputs_disabled: true,
            glider,
        }
    }

    fn pitch_input(&self, inputs: &ControllerInputs) -> Option<f32> {
        inputs
            .look_dir
            .xy()
            .try_normalized()
            .map(|look_dir2| -look_dir2.dot(inputs.move_dir))
            .map(|pitch| pitch * self.pitch_modifier())
            .filter(|pitch| pitch.abs() > std::f32::EPSILON)
    }

    fn roll_input(&self, inputs: &ControllerInputs) -> Option<f32> {
        Some(Ori::from(inputs.look_dir).right().xy().dot(inputs.move_dir))
            .map(|roll| roll * self.roll_modifier())
            .filter(|roll| roll.abs() > std::f32::EPSILON)
    }

    fn pitch_modifier(&self) -> f32 { if self.inputs_disabled { 0.0 } else { 1.0 } }

    fn roll_modifier(&self) -> f32 { if self.inputs_disabled { 0.0 } else { 1.0 } }

    fn tgt_dir(&self, base_pitch: f32, max_pitch: f32, data: &JoinData) -> Dir {
        let char_fw = data.ori.look_dir();
        if data.inputs.look_dir.dot(*char_fw) > max_pitch.cos() {
            Quaternion::rotation_3d(base_pitch, Ori::from(data.inputs.look_dir).right())
                * data.inputs.look_dir
        } else {
            char_fw
                .cross(*data.inputs.look_dir)
                .try_normalized()
                .map(|axis| Quaternion::rotation_3d(max_pitch, axis) * char_fw)
                .unwrap_or_else(|| data.ori.up())
        }
    }

    fn roll_dir(&self, max_roll: f32, tgt_dir: &Dir, flow_dir: &Dir, data: &JoinData) -> Dir {
        let char_up = data.ori.up();
        Dir::from_unnormalized(tgt_dir.to_vec() + flow_dir.to_vec())
            .map(|tgt_lift_dir| {
                // this rolls to decrease the surface exposed to unfavourable wind that is
                // causing us to drift off course
                tgt_lift_dir.slerped_to(
                    -*flow_dir,
                    Dir::from_unnormalized(data.vel.0)
                        .map_or(0.0, |moving_dir| moving_dir.dot(flow_dir.to_vec()).max(0.0)),
                )
            })
            .and_then(|dir| dir.projected(&Plane::from(data.ori.look_dir())))
            .and_then(|dir| {
                (dir.dot(*char_up) > max_roll.cos())
                    .then_some(dir)
                    .or_else(|| {
                        char_up
                            .cross(*dir)
                            .try_normalized()
                            .map(|axis| Quaternion::rotation_3d(max_roll, axis) * char_up)
                    })
            })
            .unwrap_or(char_up)
    }
}

impl CharacterBehavior for Data {
    fn behavior(&self, data: &JoinData) -> StateUpdate {
        let mut update = StateUpdate::from(data);

        // If player is on ground, end glide
        if data.physics.on_ground.is_some()
            && (data.vel.0 - data.physics.ground_vel).magnitude_squared() < 2_f32.powi(2)
        {
            update.character = CharacterState::GlideWield(glide_wield::Data(self.glider));
        } else if data.physics.in_liquid().is_some()
            || data.inventory.equipped(EquipSlot::Glider).is_none()
        {
            update.character = CharacterState::Idle;
        } else if !handle_climb(&data, &mut update) {
            // Tweaks
            let base_pitch = BASE_PITCH * tweak!(1.0);
            let max_pitch = tweak!(0.2) * PI;
            let max_roll = tweak!(0.5) * PI;
            let inputs_rate = tweak!(5.0);
            let look_pitch_rate = tweak!(5.0);
            let autoroll_rate = tweak!(5.0);
            let yaw_correction_rate = tweak!(1.0);
            let char_yaw_follow_rate = tweak!(2.0);
            // ----

            let air_flow = data
                .physics
                .in_fluid
                .map(|fluid| fluid.relative_flow(data.vel))
                .unwrap_or_else(|| Vel(Vec3::unit_z()));
            let flow_dir = Dir::from_unnormalized(air_flow.0).unwrap_or_else(Dir::up);
            let tgt_dir = self.tgt_dir(base_pitch, max_pitch, data);

            let ori = self.glider.ori;
            let mut glider = self.glider;

            {
                let char_fw = data.ori.look_dir();
                let char_up = data.ori.up();
                let glider_up = ori.up();
                let glider_right = ori.right();
                let mut next_up = glider_up;

                if let Some(roll_input) = self.roll_input(data.inputs) {
                    next_up = next_up.slerped_to(
                        Quaternion::rotation_3d(roll_input * max_roll, char_fw) * char_up,
                        data.dt.0 * inputs_rate,
                    );
                } else {
                    let roll_dir = self.roll_dir(max_roll, &tgt_dir, &flow_dir, data);
                    let s = 1.0 - roll_dir.dot(*glider_up).abs();
                    next_up = next_up.slerped_to(roll_dir, data.dt.0 * autoroll_rate * s);
                }

                if let Some(pitch_input) = self.pitch_input(data.inputs) {
                    next_up = next_up.slerped_to(
                        Quaternion::rotation_3d(pitch_input * max_pitch, glider_right) * char_up,
                        data.dt.0 * inputs_rate,
                    );
                } else {
                    // slerp un-pitching for more stable rolling
                    let tgt_fw = tgt_dir.slerped_to(char_fw, tgt_dir.dot(*glider_right).powi(2));
                    next_up = next_up.slerped_to(
                        data.ori.pitched_towards(tgt_fw).up(),
                        data.dt.0 * look_pitch_rate,
                    );
                }

                glider.ori = ori.prerotated(glider_up.rotation_between(next_up));
            }

            glider.slerp_yaw_towards(-flow_dir, data.dt.0 * yaw_correction_rate);
            update.ori = data.ori.slerped_towards(
                data.ori.yawed_towards(glider.ori.look_dir()),
                data.dt.0 * char_yaw_follow_rate,
            );

            update.character = CharacterState::Glide(Self {
                inputs_disabled: self.inputs_disabled && !data.inputs.move_dir.is_approx_zero(),
                glider,
            });
        }

        update
    }

    fn unwield(&self, data: &JoinData) -> StateUpdate {
        let mut update = StateUpdate::from(data);
        update.character = CharacterState::Idle;
        update
    }
}
