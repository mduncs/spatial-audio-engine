use std::ffi::c_void;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use super::{SONG_SAMPLE_RATE_HZ, SongProgram};

#[repr(C)]
#[derive(Default)]
struct AudioStreamBasicDescription {
    sample_rate: f64,
    format_id: u32,
    format_flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels_per_frame: u32,
    bits_per_channel: u32,
    reserved: u32,
}

#[repr(C)]
struct AudioBuffer {
    channels: u32,
    byte_size: u32,
    data: *mut c_void,
}

#[repr(C)]
struct AudioBufferList {
    count: u32,
    buffer: AudioBuffer,
}

#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn ExtAudioFileOpenURL(url: *const c_void, file: *mut *mut c_void) -> i32;
    fn ExtAudioFileDispose(file: *mut c_void) -> i32;
    fn ExtAudioFileGetProperty(
        file: *mut c_void,
        property: u32,
        size: *mut u32,
        data: *mut c_void,
    ) -> i32;
    fn ExtAudioFileSetProperty(
        file: *mut c_void,
        property: u32,
        size: u32,
        data: *const c_void,
    ) -> i32;
    fn ExtAudioFileRead(file: *mut c_void, frames: *mut u32, data: *mut AudioBufferList) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFURLCreateFromFileSystemRepresentation(
        allocator: *const c_void,
        bytes: *const u8,
        length: isize,
        is_directory: u8,
    ) -> *const c_void;
    fn CFRelease(value: *const c_void);
}

struct AudioFile(*mut c_void);

impl Drop for AudioFile {
    fn drop(&mut self) {
        // This owner never crosses into the realtime renderer.
        unsafe { ExtAudioFileDispose(self.0) };
    }
}

fn check(status: i32, operation: &str, path: &Path) -> Result<(), String> {
    if status == 0 {
        Ok(())
    } else {
        Err(format!(
            "cannot {operation} song {}: AudioToolbox OSStatus {status}",
            path.display()
        ))
    }
}

pub(super) fn decode(path: &Path) -> Result<SongProgram, String> {
    let path_bytes = path.as_os_str().as_bytes();
    let path_length = isize::try_from(path_bytes.len()).map_err(|_| "song path is too long")?;
    let url = unsafe {
        CFURLCreateFromFileSystemRepresentation(
            std::ptr::null(),
            path_bytes.as_ptr(),
            path_length,
            0,
        )
    };
    if url.is_null() {
        return Err(format!("cannot create song URL for {}", path.display()));
    }
    let mut file = std::ptr::null_mut();
    let status = unsafe { ExtAudioFileOpenURL(url, &mut file) };
    unsafe { CFRelease(url) };
    check(status, "open", path)?;
    if file.is_null() {
        return Err("AudioToolbox returned an empty song handle".into());
    }
    let file = AudioFile(file);
    let mut source = AudioStreamBasicDescription::default();
    let mut size = std::mem::size_of::<AudioStreamBasicDescription>() as u32;
    check(
        unsafe {
            ExtAudioFileGetProperty(
                file.0,
                u32::from_be_bytes(*b"ffmt"),
                &mut size,
                (&mut source as *mut AudioStreamBasicDescription).cast(),
            )
        },
        "read format of",
        path,
    )?;
    if !(1..=2).contains(&source.channels_per_frame) {
        return Err(format!(
            "song must have one or two channels, found {}",
            source.channels_per_frame
        ));
    }
    let channels = source.channels_per_frame;
    // Native, packed, interleaved float32. Apple's converter also resamples here.
    let client = AudioStreamBasicDescription {
        sample_rate: f64::from(SONG_SAMPLE_RATE_HZ),
        format_id: u32::from_be_bytes(*b"lpcm"),
        format_flags: 1 | 8,
        bytes_per_packet: 4 * channels,
        frames_per_packet: 1,
        bytes_per_frame: 4 * channels,
        channels_per_frame: channels,
        bits_per_channel: 32,
        reserved: 0,
    };
    check(
        unsafe {
            ExtAudioFileSetProperty(
                file.0,
                u32::from_be_bytes(*b"cfmt"),
                std::mem::size_of::<AudioStreamBasicDescription>() as u32,
                (&client as *const AudioStreamBasicDescription).cast(),
            )
        },
        "set 48 kHz float format for",
        path,
    )?;

    const CHUNK_FRAMES: usize = 4_096;
    let mut scratch = [0.0_f32; CHUNK_FRAMES * 2];
    let mut frames = Vec::new();
    loop {
        let mut frame_count = CHUNK_FRAMES as u32;
        let mut buffers = AudioBufferList {
            count: 1,
            buffer: AudioBuffer {
                channels,
                byte_size: (CHUNK_FRAMES * channels as usize * 4) as u32,
                data: scratch.as_mut_ptr().cast(),
            },
        };
        check(
            unsafe { ExtAudioFileRead(file.0, &mut frame_count, &mut buffers) },
            "decode",
            path,
        )?;
        if frame_count == 0 {
            break;
        }
        if frame_count as usize > CHUNK_FRAMES {
            return Err("AudioToolbox returned too many song frames".into());
        }
        frames
            .try_reserve(frame_count as usize)
            .map_err(|_| "song is too large to decode")?;
        for frame in
            scratch[..frame_count as usize * channels as usize].chunks_exact(channels as usize)
        {
            let left = frame[0];
            let right = if channels == 2 { frame[1] } else { left };
            if !left.is_finite() || !right.is_finite() {
                return Err("non-finite decoded song sample".into());
            }
            frames.push([left, right]);
        }
    }
    if frames.is_empty() {
        return Err("song file is empty".into());
    }
    Ok(SongProgram {
        frames,
        channels: channels as u16,
    })
}
