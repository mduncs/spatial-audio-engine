use crate::SteamVector3;

const FLATBUFFER_UOFFSET_BYTES: usize = 4;
const SERIALIZED_SPHERE_BYTES: usize = 4 * size_of::<f32>();

/// A validated view of the probe-influence spheres inside Steam Audio's
/// serialized `ProbeBatch` FlatBuffer.
///
/// Steam Audio 4.8.1 exposes the probe count but no C API for retrieving
/// individual probes from a loaded batch. Keeping this byte range instead of a
/// decoded copy avoids duplicating a potentially large probe array.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SerializedProbeInfluences {
    spheres_offset: usize,
    sphere_count: usize,
}

impl SerializedProbeInfluences {
    pub(crate) fn probe_count(self) -> usize {
        self.sphere_count
    }

    /// Every probe as `(center, radius)` in Steam Audio coordinates, decoded on
    /// demand so no copy of the probe array is ever materialized.
    pub(crate) fn spheres(self, bytes: &[u8]) -> impl Iterator<Item = (SteamVector3, f32)> + '_ {
        self.sphere_slice(bytes)
            .unwrap_or_default()
            .chunks_exact(SERIALIZED_SPHERE_BYTES)
            .map(|sphere| {
                (
                    SteamVector3::new(
                        read_chunk_f32(sphere, 0),
                        read_chunk_f32(sphere, 4),
                        read_chunk_f32(sphere, 8),
                    ),
                    read_chunk_f32(sphere, 12),
                )
            })
    }

    fn sphere_slice(self, bytes: &[u8]) -> Option<&[u8]> {
        let sphere_bytes = self.sphere_count.checked_mul(SERIALIZED_SPHERE_BYTES)?;
        let spheres_end = self.spheres_offset.checked_add(sphere_bytes)?;
        bytes.get(self.spheres_offset..spheres_end)
    }

    pub(crate) fn parse(bytes: &[u8], expected_count: u32) -> Result<Self, &'static str> {
        let table = read_u32(bytes, 0).ok_or("serialized probe batch has no root table")? as usize;
        let vtable_distance =
            read_i32(bytes, table).ok_or("serialized probe batch root table is truncated")?;
        let vtable_distance = usize::try_from(vtable_distance)
            .map_err(|_| "serialized probe batch root vtable offset is negative")?;
        let vtable = table
            .checked_sub(vtable_distance)
            .ok_or("serialized probe batch root vtable offset is invalid")?;
        let vtable_size = read_u16(bytes, vtable)
            .ok_or("serialized probe batch root vtable is truncated")?
            as usize;
        if vtable_size < 6 {
            return Err("serialized probe batch has no probe vector field");
        }
        let field_offset = read_u16(bytes, vtable + 4)
            .ok_or("serialized probe batch probe-vector field is truncated")?
            as usize;
        if field_offset == 0 {
            return Err("serialized probe batch probe vector is absent");
        }
        let vector_reference = table
            .checked_add(field_offset)
            .ok_or("serialized probe batch probe-vector offset overflowed")?;
        let vector_offset = read_u32(bytes, vector_reference)
            .ok_or("serialized probe batch probe-vector reference is truncated")?
            as usize;
        let vector = vector_reference
            .checked_add(vector_offset)
            .ok_or("serialized probe batch probe-vector reference overflowed")?;
        let sphere_count = read_u32(bytes, vector)
            .ok_or("serialized probe batch probe-vector header is truncated")?
            as usize;
        if sphere_count != expected_count as usize {
            return Err("serialized probe count does not match metadata");
        }
        let spheres_offset = vector
            .checked_add(FLATBUFFER_UOFFSET_BYTES)
            .ok_or("serialized probe batch sphere offset overflowed")?;
        let sphere_bytes = sphere_count
            .checked_mul(SERIALIZED_SPHERE_BYTES)
            .ok_or("serialized probe batch sphere byte count overflowed")?;
        let spheres_end = spheres_offset
            .checked_add(sphere_bytes)
            .ok_or("serialized probe batch sphere range overflowed")?;
        let spheres = bytes
            .get(spheres_offset..spheres_end)
            .ok_or("serialized probe batch probe-vector payload is truncated")?;
        for sphere in spheres.chunks_exact(SERIALIZED_SPHERE_BYTES) {
            let center = [
                read_chunk_f32(sphere, 0),
                read_chunk_f32(sphere, 4),
                read_chunk_f32(sphere, 8),
            ];
            let radius = read_chunk_f32(sphere, 12);
            if !center.into_iter().all(f32::is_finite) || !radius.is_finite() || radius < 0.0 {
                return Err("serialized probe batch contains an invalid influence sphere");
            }
        }
        Ok(Self {
            spheres_offset,
            sphere_count,
        })
    }

    pub(crate) fn contains(self, bytes: &[u8], point: SteamVector3) -> bool {
        let Some(spheres) = self.sphere_slice(bytes) else {
            return false;
        };
        spheres.chunks_exact(SERIALIZED_SPHERE_BYTES).any(|sphere| {
            let x = read_chunk_f32(sphere, 0) - point.x;
            let y = read_chunk_f32(sphere, 4) - point.y;
            let z = read_chunk_f32(sphere, 8) - point.z;
            let radius = read_chunk_f32(sphere, 12);
            x * x + y * y + z * z <= radius * radius
        })
    }
}

fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(offset..offset.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn read_i32(bytes: &[u8], offset: usize) -> Option<i32> {
    Some(i32::from_le_bytes(
        bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn read_chunk_f32(chunk: &[u8], offset: usize) -> f32 {
    f32::from_le_bytes(
        chunk[offset..offset + size_of::<f32>()]
            .try_into()
            .expect("validated fixed-size sphere chunk"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one_sphere_flatbuffer(center: [f32; 3], radius: f32) -> Vec<u8> {
        let mut bytes = vec![0_u8; 44];
        bytes[0..4].copy_from_slice(&12_u32.to_le_bytes());
        bytes[4..6].copy_from_slice(&6_u16.to_le_bytes());
        bytes[6..8].copy_from_slice(&8_u16.to_le_bytes());
        bytes[8..10].copy_from_slice(&4_u16.to_le_bytes());
        bytes[12..16].copy_from_slice(&8_i32.to_le_bytes());
        bytes[16..20].copy_from_slice(&8_u32.to_le_bytes());
        bytes[24..28].copy_from_slice(&1_u32.to_le_bytes());
        for (index, value) in center.into_iter().chain([radius]).enumerate() {
            let offset = 28 + index * 4;
            bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn parses_probe_spheres_and_tests_influence_without_decoding_a_copy() {
        let bytes = one_sphere_flatbuffer([2.0, 3.0, 4.0], 2.0);
        let probes = SerializedProbeInfluences::parse(&bytes, 1).unwrap();

        assert!(probes.contains(&bytes, SteamVector3::new(2.0, 3.0, 6.0)));
        assert!(!probes.contains(&bytes, SteamVector3::new(2.0, 3.0, 6.01)));
    }

    #[test]
    fn rejects_probe_count_mismatch_and_nonfinite_spheres() {
        let bytes = one_sphere_flatbuffer([2.0, 3.0, 4.0], 2.0);
        assert_eq!(
            SerializedProbeInfluences::parse(&bytes, 2),
            Err("serialized probe count does not match metadata")
        );

        let bytes = one_sphere_flatbuffer([f32::NAN, 3.0, 4.0], 2.0);
        assert_eq!(
            SerializedProbeInfluences::parse(&bytes, 1),
            Err("serialized probe batch contains an invalid influence sphere")
        );
    }
}
