use fightbox_api::{EnuVector3, ListenerState, Pose};
use fightbox_runtime::{SnapshotPublication, SnapshotReader, SnapshotWriter};

pub const WALK_SPEED_MPS: f32 = 1.4;
pub const SPRINT_SPEED_MPS: f32 = 20.0;
const EYE_HEIGHT_M: f32 = 1.5;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ListenerControl {
    pub position: EnuVector3,
    pub yaw_radians: f32,
}

impl ListenerControl {
    #[must_use]
    pub fn at(position: EnuVector3, forward: EnuVector3) -> Self {
        Self {
            position,
            yaw_radians: forward.east_m.atan2(forward.north_m),
        }
    }

    #[must_use]
    pub fn forward(self) -> EnuVector3 {
        EnuVector3::new(self.yaw_radians.sin(), self.yaw_radians.cos(), 0.0)
    }

    #[must_use]
    pub fn right(self) -> EnuVector3 {
        EnuVector3::new(self.yaw_radians.cos(), -self.yaw_radians.sin(), 0.0)
    }

    pub fn turn(&mut self, delta_radians: f32) {
        self.yaw_radians = (self.yaw_radians + delta_radians).rem_euclid(std::f32::consts::TAU);
    }

    /// Applies normalized ground-plane input and returns the resulting velocity.
    pub fn walk(
        &mut self,
        forward_input: f32,
        right_input: f32,
        sprinting: bool,
        delta_seconds: f32,
    ) -> EnuVector3 {
        let length = forward_input.hypot(right_input);
        if length == 0.0 || delta_seconds <= 0.0 {
            return EnuVector3::default();
        }
        let forward_input = forward_input / length.max(1.0);
        let right_input = right_input / length.max(1.0);
        let speed = if sprinting {
            SPRINT_SPEED_MPS
        } else {
            WALK_SPEED_MPS
        };
        let forward = self.forward();
        let right = self.right();
        let velocity = EnuVector3::new(
            (forward.east_m * forward_input + right.east_m * right_input) * speed,
            (forward.north_m * forward_input + right.north_m * right_input) * speed,
            0.0,
        );
        self.position.east_m += velocity.east_m * delta_seconds;
        self.position.north_m += velocity.north_m * delta_seconds;
        self.position.up_m = EYE_HEIGHT_M;
        velocity
    }

    #[must_use]
    pub fn listener_state(self, velocity: EnuVector3) -> ListenerState {
        ListenerState {
            pose: Pose {
                position: self.position,
                forward: self.forward(),
                up: EnuVector3::new(0.0, 0.0, 1.0),
            },
            linear_velocity_mps: velocity,
        }
    }
}

pub struct PoseMailbox {
    writer: SnapshotWriter<ListenerState>,
}

impl PoseMailbox {
    #[must_use]
    pub fn new(initial: ListenerState) -> (Self, SnapshotReader<ListenerState>) {
        let (writer, reader) = SnapshotPublication::new(initial);
        (Self { writer }, reader)
    }

    pub fn publish(&mut self, listener: ListenerState) {
        self.writer.publish(listener);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yaw_zero_faces_north_and_drag_turns_east() {
        let mut control = ListenerControl::at(
            EnuVector3::new(0.0, 0.0, 1.5),
            EnuVector3::new(0.0, 1.0, 0.0),
        );
        assert_eq!(control.forward(), EnuVector3::new(0.0, 1.0, 0.0));
        control.turn(std::f32::consts::FRAC_PI_2);
        let forward = control.forward();
        assert!((forward.east_m - 1.0).abs() < 1.0e-6);
        assert!(forward.north_m.abs() < 1.0e-6);
    }

    #[test]
    fn diagonal_walk_is_normalized_to_walk_speed() {
        let mut control = ListenerControl::at(
            EnuVector3::new(0.0, 0.0, 1.5),
            EnuVector3::new(0.0, 1.0, 0.0),
        );
        let velocity = control.walk(1.0, 1.0, false, 1.0);
        assert!((velocity.east_m.hypot(velocity.north_m) - WALK_SPEED_MPS).abs() < 1.0e-6);
        assert_eq!(control.position.up_m, EYE_HEIGHT_M);
    }

    #[test]
    fn mailbox_publishes_complete_listener_pose() {
        let initial = ListenerControl::at(
            EnuVector3::new(1.0, 2.0, 1.5),
            EnuVector3::new(0.0, 1.0, 0.0),
        )
        .listener_state(EnuVector3::default());
        let (mut mailbox, mut reader) = PoseMailbox::new(initial);
        let next = ListenerControl::at(
            EnuVector3::new(3.0, 4.0, 1.5),
            EnuVector3::new(1.0, 0.0, 0.0),
        )
        .listener_state(EnuVector3::new(1.0, 0.0, 0.0));
        mailbox.publish(next);
        assert_eq!(reader.read(), next);
    }
}
