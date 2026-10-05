import Foundation

struct MotionQuaternion {
    let w: Float
    let x: Float
    let y: Float
    let z: Float

    static let identity = MotionQuaternion(w: 1, x: 0, y: 0, z: 0)

    static func normalized(_ raw: [Double]) -> MotionQuaternion? {
        guard raw.count == 4 else { return nil }
        let norm = raw.reduce(0.0) { $0 + $1 * $1 }.squareRoot()
        guard norm.isFinite && (0.5...1.5).contains(norm) else { return nil }
        return MotionQuaternion(w: Float(raw[0] / norm), x: Float(raw[1] / norm),
                                y: Float(raw[2] / norm), z: Float(raw[3] / norm))
    }

    func inverse() -> MotionQuaternion {
        MotionQuaternion(w: w, x: -x, y: -y, z: -z)
    }

    var rotationRadians: Float { 2 * acos(min(1, abs(w))) }

    func compose(_ other: MotionQuaternion) -> MotionQuaternion {
        let (v, a, b, c) = (other.w, other.x, other.y, other.z)
        return MotionQuaternion(
            w: w * v - x * a - y * b - z * c,
            x: w * a + x * v + y * c - z * b,
            y: w * b - x * c + y * v + z * a,
            z: w * c + x * b - y * a + z * v)
    }

    func rotate(_ vector: EnuForward) -> EnuForward {
        let rotated = compose(MotionQuaternion(w: 0, x: vector.east, y: vector.north, z: vector.up))
            .compose(inverse())
        return EnuForward(east: rotated.x, north: rotated.y, up: rotated.z)
    }
}

struct EnuForward {
    let east: Float
    let north: Float
    let up: Float
    static let north = EnuForward(east: 0, north: 1, up: 0)
}

func coreMotionToEnu(reference: MotionQuaternion, current: MotionQuaternion) -> MotionQuaternion {
    // AirPods probe 2026-10-01: active head attitude (+x right, +y forward, +z up)
    // in a gravity-vertical frame; the reference nose heading becomes north.
    // This Float port matches Rust core_motion_to_enu; mapping-cases.tsv pins both.
    let heading = referenceHeading(reference)
    return heading.compose(current.compose(reference.inverse())).compose(heading.inverse())
}

func referenceHeading(_ reference: MotionQuaternion) -> MotionQuaternion {
    let forward = reference.rotate(.north)
    var (east, north) = (forward.east, forward.north)
    if hypot(east, north) < 0.2 {
        let right = reference.rotate(EnuForward(east: 1, north: 0, up: 0))
        (east, north) = (-right.north, right.east)
    }
    let halfTurn = 0.5 * (Float.pi / 2 - atan2(north, east))
    return MotionQuaternion(w: cos(halfTurn), x: 0, y: 0, z: sin(halfTurn))
}
