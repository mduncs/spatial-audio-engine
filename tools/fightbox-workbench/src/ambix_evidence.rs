#[test]
#[ignore = "device-free evidence decoder; requires AMBIX_INPUT and AMBIX_DECODED"]
fn decode_export_with_steam_hrtf() {
    let input = std::env::var("AMBIX_INPUT").expect("AMBIX_INPUT");
    let output = std::env::var("AMBIX_DECODED").expect("AMBIX_DECODED");
    let bytes = std::fs::read(input).unwrap();
    assert_eq!(&bytes[..4], b"RIFF");
    let mut position = 12;
    let mut data = None;
    while position + 8 <= bytes.len() {
        let size =
            u32::from_le_bytes(bytes[position + 4..position + 8].try_into().unwrap()) as usize;
        let chunk = &bytes[position + 8..position + 8 + size];
        match &bytes[position..position + 4] {
            b"fmt " => {
                assert_eq!(u16::from_le_bytes(chunk[..2].try_into().unwrap()), 0xfffe);
                assert_eq!(u16::from_le_bytes(chunk[2..4].try_into().unwrap()), 9);
                assert_eq!(u32::from_le_bytes(chunk[4..8].try_into().unwrap()), 48_000);
                assert_eq!(u16::from_le_bytes(chunk[14..16].try_into().unwrap()), 32);
            }
            b"data" => data = Some(chunk),
            _ => {}
        }
        position += 8 + size + size % 2;
    }
    let pcm = data
        .unwrap()
        .chunks_exact(4)
        .map(|sample| f32::from_le_bytes(sample.try_into().unwrap()))
        .collect::<Vec<_>>();
    let decoded = fightbox_steam_audio::decode_ambix_binaural(
        &pcm,
        fightbox_steam_audio::AudioConfig {
            sample_rate_hz: 48_000,
            frame_size: 128,
        },
    )
    .unwrap();
    assert_eq!(decoded.len(), pcm.len() / 9 * 2);
    let wav = fightbox_evidence::write_wav(
        fightbox_evidence::WavSpec {
            sample_rate_hz: 48_000,
            channels: 2,
        },
        &decoded,
    )
    .unwrap();
    std::fs::write(output, wav).unwrap();
}
