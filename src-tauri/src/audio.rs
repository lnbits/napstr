use lofty::{config::ParseOptions, file::TaggedFileExt, probe::Probe, tag::Accessor};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

const MAX_METADATA_BYTES: usize = 16 * 1024 * 1024;
const MAX_PUBLIC_METADATA_CHARS: usize = 256;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AudioMetadata {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub track_number: u32,
    pub disc_number: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioInfo {
    pub format: &'static str,
    pub mime: &'static str,
    pub metadata: AudioMetadata,
}

pub fn validate_audio(path: &Path) -> Result<AudioInfo, String> {
    let mut extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if extension == "part" {
        extension = path
            .file_stem()
            .and_then(|value| Path::new(value).extension())
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
    }
    let mut file = File::open(path).map_err(|error| error.to_string())?;
    let size = file.metadata().map_err(|error| error.to_string())?.len();
    if size < 12 {
        return Err("not a valid supported audio file".into());
    }
    let mut info = match extension.as_str() {
        "mp3" => validate_mp3(&mut file, size)?,
        "flac" => validate_flac(&mut file, size)?,
        "wav" => validate_wav(&mut file, size)?,
        "ogg" => validate_ogg(&mut file, size, false)?,
        "opus" => validate_ogg(&mut file, size, true)?,
        _ => {
            return Err(
                "only MP3, FLAC, WAV, Ogg Vorbis, and Opus audio files may be shared".into(),
            )
        }
    };
    info.metadata = read_metadata(path);
    Ok(info)
}

pub fn read_metadata(path: &Path) -> AudioMetadata {
    let options = ParseOptions::new()
        .read_properties(false)
        .read_cover_art(false);
    let Ok(probe) = Probe::open(path) else {
        return AudioMetadata::default();
    };
    let Ok(probe) = probe.options(options).guess_file_type() else {
        return AudioMetadata::default();
    };
    let Ok(tagged_file) = probe.read() else {
        return AudioMetadata::default();
    };
    AudioMetadata {
        title: first_safe_metadata_value(&tagged_file, |tag| tag.title()),
        artist: first_safe_metadata_value(&tagged_file, |tag| tag.artist()),
        album: first_safe_metadata_value(&tagged_file, |tag| tag.album()),
        track_number: tagged_file
            .tags()
            .iter()
            .find_map(|tag| tag.track())
            .unwrap_or_default(),
        disc_number: tagged_file
            .tags()
            .iter()
            .find_map(|tag| tag.disk())
            .unwrap_or_default(),
    }
}

fn first_safe_metadata_value<'a>(
    tagged_file: &'a lofty::file::TaggedFile,
    value: impl Fn(&'a lofty::tag::Tag) -> Option<std::borrow::Cow<'a, str>>,
) -> String {
    tagged_file
        .tags()
        .iter()
        .find_map(value)
        .map(|value| sanitise_public_text(&value))
        .unwrap_or_default()
}

pub(crate) fn sanitise_public_text(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control()
                || matches!(
                    character,
                    '\u{061c}'
                        | '\u{200e}'
                        | '\u{200f}'
                        | '\u{202a}'..='\u{202e}'
                        | '\u{2066}'..='\u{2069}'
                )
            {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(MAX_PUBLIC_METADATA_CHARS)
        .collect()
}

fn validate_mp3(file: &mut File, size: u64) -> Result<AudioInfo, String> {
    let mut first = [0u8; 10];
    file.read_exact(&mut first)
        .map_err(|error| error.to_string())?;
    let mut audio_offset = 0u64;
    if &first[..3] == b"ID3" {
        if first[3] < 2 || first[3] > 4 || first[6..10].iter().any(|byte| byte & 0x80 != 0) {
            return Err("malformed MP3 ID3 metadata".into());
        }
        let tag_size = synchsafe(&first[6..10]) as usize;
        if tag_size > MAX_METADATA_BYTES || tag_size as u64 + 10 >= size {
            return Err("MP3 metadata is invalid or too large".into());
        }
        file.seek(SeekFrom::Current(tag_size as i64))
            .map_err(|error| error.to_string())?;
        audio_offset = 10 + tag_size as u64 + if first[5] & 0x10 != 0 { 10 } else { 0 };
        if audio_offset >= size {
            return Err("MP3 metadata does not leave an audio stream".into());
        }
    }
    file.seek(SeekFrom::Start(audio_offset))
        .map_err(|error| error.to_string())?;
    let mut probe = vec![0u8; ((size - audio_offset).min(64 * 1024)) as usize];
    file.read_exact(&mut probe)
        .map_err(|error| error.to_string())?;
    let valid_stream = probe.windows(4).enumerate().any(|(index, header)| {
        let Some(frame_length) = mpeg_frame_length(header) else {
            return false;
        };
        probe
            .get(index + frame_length..index + frame_length + 4)
            .is_some_and(valid_mpeg_header)
    });
    if valid_stream {
        Ok(AudioInfo {
            format: "MP3",
            mime: "audio/mpeg",
            metadata: AudioMetadata::default(),
        })
    } else {
        Err("file does not contain a valid MPEG audio stream".into())
    }
}

fn mpeg_frame_length(header: &[u8]) -> Option<usize> {
    if !valid_mpeg_header(header) {
        return None;
    }
    let version = (header[1] >> 3) & 0x03;
    let layer = (header[1] >> 1) & 0x03;
    let bitrate_index = (header[2] >> 4) as usize;
    let sample_index = ((header[2] >> 2) & 0x03) as usize;
    let padding = ((header[2] >> 1) & 1) as usize;
    let mpeg1 = version == 3;
    let bitrates_mpeg1_l1 = [
        0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448,
    ];
    let bitrates_mpeg1_l2 = [
        0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384,
    ];
    let bitrates_mpeg1_l3 = [
        0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
    ];
    let bitrates_mpeg2_l1 = [
        0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256,
    ];
    let bitrates_mpeg2_l23 = [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];
    let bitrate = match (mpeg1, layer) {
        (true, 3) => bitrates_mpeg1_l1[bitrate_index],
        (true, 2) => bitrates_mpeg1_l2[bitrate_index],
        (true, 1) => bitrates_mpeg1_l3[bitrate_index],
        (false, 3) => bitrates_mpeg2_l1[bitrate_index],
        (false, 2 | 1) => bitrates_mpeg2_l23[bitrate_index],
        _ => 0,
    };
    let base_sample = [44_100usize, 48_000, 32_000][sample_index];
    let sample_rate = match version {
        3 => base_sample,
        2 => base_sample / 2,
        0 => base_sample / 4,
        _ => 0,
    };
    if bitrate == 0 || sample_rate == 0 {
        return None;
    }
    Some(if layer == 3 {
        ((12 * bitrate * 1000 / sample_rate) + padding) * 4
    } else if layer == 1 && !mpeg1 {
        72 * bitrate * 1000 / sample_rate + padding
    } else {
        144 * bitrate * 1000 / sample_rate + padding
    })
}

fn valid_mpeg_header(header: &[u8]) -> bool {
    header.len() == 4
        && header[0] == 0xff
        && header[1] & 0xe0 == 0xe0
        && header[1] & 0x18 != 0x08
        && header[1] & 0x06 != 0
        && header[2] & 0xf0 != 0
        && header[2] & 0xf0 != 0xf0
        && header[2] & 0x0c != 0x0c
}

fn validate_flac(file: &mut File, size: u64) -> Result<AudioInfo, String> {
    file.seek(SeekFrom::Start(0))
        .map_err(|error| error.to_string())?;
    let mut signature = [0u8; 4];
    file.read_exact(&mut signature)
        .map_err(|error| error.to_string())?;
    if &signature != b"fLaC" {
        return Err("FLAC extension does not match file content".into());
    }
    let mut offset = 4u64;
    let mut first = true;
    loop {
        let mut header = [0u8; 4];
        file.read_exact(&mut header)
            .map_err(|_| "truncated FLAC metadata".to_string())?;
        offset += 4;
        let final_block = header[0] & 0x80 != 0;
        let block_type = header[0] & 0x7f;
        let length = u32::from_be_bytes([0, header[1], header[2], header[3]]) as usize;
        if first && (block_type != 0 || length != 34) {
            return Err("FLAC STREAMINFO block is missing".into());
        }
        if block_type == 127 || length > MAX_METADATA_BYTES || offset + length as u64 >= size {
            return Err("invalid or excessive FLAC metadata".into());
        }
        file.seek(SeekFrom::Current(length as i64))
            .map_err(|error| error.to_string())?;
        offset += length as u64;
        first = false;
        if final_block {
            break;
        }
    }
    let mut sync = [0u8; 2];
    file.read_exact(&mut sync)
        .map_err(|_| "FLAC has no audio frames".to_string())?;
    if sync[0] != 0xff || sync[1] & 0xfc != 0xf8 {
        return Err("FLAC audio frame sync is invalid".into());
    }
    Ok(AudioInfo {
        format: "FLAC",
        mime: "audio/flac",
        metadata: AudioMetadata::default(),
    })
}

fn validate_wav(file: &mut File, size: u64) -> Result<AudioInfo, String> {
    file.seek(SeekFrom::Start(0))
        .map_err(|error| error.to_string())?;
    let mut header = [0u8; 12];
    file.read_exact(&mut header)
        .map_err(|error| error.to_string())?;
    if &header[..4] != b"RIFF" || &header[8..] != b"WAVE" {
        return Err("WAV extension does not match file content".into());
    }
    let declared = u32::from_le_bytes(header[4..8].try_into().unwrap()) as u64 + 8;
    if declared != size {
        return Err("WAV container length is invalid or has appended data".into());
    }
    let mut offset = 12u64;
    let mut valid_format = false;
    let mut has_audio = false;
    while offset + 8 <= size {
        let mut chunk = [0u8; 8];
        file.read_exact(&mut chunk)
            .map_err(|error| error.to_string())?;
        offset += 8;
        let length = u32::from_le_bytes(chunk[4..8].try_into().unwrap()) as u64;
        if offset + length > size {
            return Err("truncated WAV chunk".into());
        }
        match &chunk[..4] {
            b"fmt " => {
                if length < 16 || length > 4096 {
                    return Err("invalid WAV format block".into());
                }
                let mut format = vec![0u8; length as usize];
                file.read_exact(&mut format)
                    .map_err(|error| error.to_string())?;
                let codec = u16::from_le_bytes(format[..2].try_into().unwrap());
                let extensible_pcm = codec == 0xfffe
                    && length >= 40
                    && matches!(
                        u16::from_le_bytes(format[24..26].try_into().unwrap()),
                        1 | 3
                    )
                    && &format[26..40] == b"\0\0\x10\0\x80\0\0\xaa\0\x38\x9b\x71";
                if codec != 1 && codec != 3 && !extensible_pcm {
                    return Err("WAV codec is not PCM audio".into());
                }
                valid_format = true;
            }
            b"data" => {
                if length == 0 {
                    return Err("WAV has an empty audio stream".into());
                }
                has_audio = true;
                file.seek(SeekFrom::Current(length as i64))
                    .map_err(|error| error.to_string())?;
            }
            b"id3 " | b"ID3 " | b"fact" | b"LIST" | b"JUNK" | b"bext" | b"iXML" | b"cue "
            | b"smpl" | b"PAD " => {
                if length > MAX_METADATA_BYTES as u64 {
                    return Err("WAV metadata is too large".into());
                }
                file.seek(SeekFrom::Current(length as i64))
                    .map_err(|error| error.to_string())?;
            }
            _ => return Err("WAV contains a non-audio or unsupported payload chunk".into()),
        }
        offset += length;
        if length & 1 == 1 {
            if offset >= size {
                return Err("invalid WAV padding".into());
            }
            file.seek(SeekFrom::Current(1))
                .map_err(|error| error.to_string())?;
            offset += 1;
        }
    }
    if !valid_format || !has_audio || offset != size {
        return Err("WAV is missing a valid PCM format or audio data".into());
    }
    Ok(AudioInfo {
        format: "WAV",
        mime: "audio/wav",
        metadata: AudioMetadata::default(),
    })
}

fn validate_ogg(file: &mut File, size: u64, expect_opus: bool) -> Result<AudioInfo, String> {
    file.seek(SeekFrom::Start(0))
        .map_err(|error| error.to_string())?;
    let mut offset = 0u64;
    let mut serial = None;
    let mut packet = Vec::new();
    let mut packets = Vec::new();
    let mut expected_sequence = 0u32;
    let mut saw_end = false;
    while offset < size {
        let mut header = [0u8; 27];
        file.read_exact(&mut header)
            .map_err(|_| "truncated Ogg page".to_string())?;
        offset += 27;
        if &header[..4] != b"OggS" || header[4] != 0 {
            return Err("invalid Ogg container or appended data".into());
        }
        let sequence = u32::from_le_bytes(header[18..22].try_into().unwrap());
        if sequence != expected_sequence || (expected_sequence == 0 && header[5] & 0x02 == 0) {
            return Err("Ogg page sequence is malformed".into());
        }
        expected_sequence = expected_sequence
            .checked_add(1)
            .ok_or("too many Ogg pages")?;
        saw_end = header[5] & 0x04 != 0;
        let current_serial = u32::from_le_bytes(header[14..18].try_into().unwrap());
        if serial
            .replace(current_serial)
            .is_some_and(|existing| existing != current_serial)
        {
            return Err("Ogg files with multiple media streams are not allowed".into());
        }
        let segments = header[26] as usize;
        let mut lacing = vec![0u8; segments];
        file.read_exact(&mut lacing)
            .map_err(|_| "truncated Ogg segment table".to_string())?;
        offset += segments as u64;
        let body_size: usize = lacing.iter().map(|value| *value as usize).sum();
        if offset + body_size as u64 > size {
            return Err("truncated Ogg page body".into());
        }
        let mut body = vec![0u8; body_size];
        file.read_exact(&mut body)
            .map_err(|error| error.to_string())?;
        offset += body_size as u64;
        let expected_crc = u32::from_le_bytes(header[22..26].try_into().unwrap());
        let mut page = header.to_vec();
        page[22..26].fill(0);
        page.extend_from_slice(&lacing);
        page.extend_from_slice(&body);
        if ogg_crc(&page) != expected_crc {
            return Err("Ogg page checksum is invalid".into());
        }
        let mut cursor = 0;
        for segment in lacing {
            let next = cursor + segment as usize;
            if packet.len() + segment as usize > MAX_METADATA_BYTES {
                return Err("Ogg header metadata is too large".into());
            }
            if packets.len() < 3 {
                packet.extend_from_slice(&body[cursor..next]);
            }
            cursor = next;
            if segment < 255 && packets.len() < 3 {
                packets.push(std::mem::take(&mut packet));
            }
        }
    }
    if offset != size || packets.len() < 2 || !saw_end {
        return Err("Ogg file has incomplete codec headers".into());
    }
    let is_opus = packets[0].starts_with(b"OpusHead");
    let is_vorbis = packets[0].starts_with(b"\x01vorbis");
    if expect_opus && !is_opus || !expect_opus && !is_vorbis {
        return Err("Ogg filename extension does not match its audio codec".into());
    }
    Ok(if is_opus {
        AudioInfo {
            format: "OPUS",
            mime: "audio/ogg",
            metadata: AudioMetadata::default(),
        }
    } else {
        AudioInfo {
            format: "OGG",
            mime: "audio/ogg",
            metadata: AudioMetadata::default(),
        }
    })
}

fn ogg_crc(bytes: &[u8]) -> u32 {
    let mut crc = 0u32;
    for byte in bytes {
        crc ^= (*byte as u32) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04c1_1db7
            } else {
                crc << 1
            };
        }
    }
    crc
}

fn synchsafe(bytes: &[u8]) -> u32 {
    bytes
        .iter()
        .fold(0, |value, byte| (value << 7) | *byte as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn path(name: &str) -> std::path::PathBuf {
        let input = std::path::Path::new(name);
        let stem = input
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("audio");
        let extension = input
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("bin");
        std::env::temp_dir().join(format!(
            "napstr-audio-{stem}-{}.{}",
            std::process::id(),
            extension
        ))
    }

    fn wav(payload: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        let total = 4 + (8 + 16) + (8 + payload.len());
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(total as u32).to_le_bytes());
        bytes.extend_from_slice(
            b"WAVEfmt \x10\0\0\0\x01\0\x01\0\x44\xac\0\0\x88\x58\x01\0\x02\0\x10\0data",
        );
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(payload);
        bytes
    }

    fn ogg_page(packets: &[&[u8]]) -> Vec<u8> {
        assert!(packets.iter().all(|packet| packet.len() < 255));
        let mut header = vec![0u8; 27];
        header[..4].copy_from_slice(b"OggS");
        header[5] = 0x06;
        header[14..18].copy_from_slice(&7u32.to_le_bytes());
        header[26] = packets.len() as u8;
        let lacing = packets
            .iter()
            .map(|packet| packet.len() as u8)
            .collect::<Vec<_>>();
        let mut page = header;
        page.extend_from_slice(&lacing);
        for packet in packets {
            page.extend_from_slice(packet);
        }
        let checksum = ogg_crc(&page);
        page[22..26].copy_from_slice(&checksum.to_le_bytes());
        page
    }

    fn id3_text_frame(id: &[u8; 4], value: &[u8]) -> Vec<u8> {
        let mut frame = Vec::new();
        frame.extend_from_slice(id);
        frame.extend_from_slice(&((value.len() + 1) as u32).to_be_bytes());
        frame.extend_from_slice(&[0, 0, 0]);
        frame.extend_from_slice(value);
        frame
    }

    fn synchsafe_bytes(value: usize) -> [u8; 4] {
        [
            ((value >> 21) & 0x7f) as u8,
            ((value >> 14) & 0x7f) as u8,
            ((value >> 7) & 0x7f) as u8,
            (value & 0x7f) as u8,
        ]
    }

    #[test]
    fn validates_pcm_wav_by_content() {
        let file = path("valid.wav");
        fs::write(&file, wav(&[0, 1, 2, 3])).unwrap();
        assert_eq!(validate_audio(&file).unwrap().format, "WAV");
        let _ = fs::remove_file(file);
    }

    #[test]
    fn rejects_renamed_and_appended_payloads() {
        let renamed = path("renamed.mp3");
        fs::write(&renamed, b"MZ this is an executable, not audio").unwrap();
        assert!(validate_audio(&renamed).is_err());
        let appended = path("appended.wav");
        let mut bytes = wav(&[0, 1]);
        bytes.extend_from_slice(b"PK\x03\x04archive");
        fs::write(&appended, bytes).unwrap();
        assert!(validate_audio(&appended).is_err());
        let _ = fs::remove_file(renamed);
        let _ = fs::remove_file(appended);
    }

    #[test]
    fn validates_each_allowed_codec_container() {
        let mp3 = path("frames.mp3");
        let mut frame = vec![0u8; 417];
        frame[..4].copy_from_slice(&[0xff, 0xfb, 0x90, 0x00]);
        let mut mp3_bytes = frame.clone();
        mp3_bytes.extend_from_slice(&frame);
        fs::write(&mp3, mp3_bytes).unwrap();
        assert_eq!(validate_audio(&mp3).unwrap().format, "MP3");

        let flac = path("stream.flac");
        let mut flac_bytes = b"fLaC".to_vec();
        flac_bytes.extend_from_slice(&[0x80, 0, 0, 34]);
        flac_bytes.extend_from_slice(&[0; 34]);
        flac_bytes.extend_from_slice(&[0xff, 0xf8]);
        fs::write(&flac, flac_bytes).unwrap();
        assert_eq!(validate_audio(&flac).unwrap().format, "FLAC");

        let ogg = path("vorbis.ogg");
        fs::write(&ogg, ogg_page(&[b"\x01vorbis", b"\x03vorbiscomments"])).unwrap();
        assert_eq!(validate_audio(&ogg).unwrap().format, "OGG");

        let opus = path("voice.opus");
        fs::write(
            &opus,
            ogg_page(&[
                b"OpusHead\x01\x01\0\0\0\0\0\0\0\0\0",
                b"OpusTags\0\0\0\0\0\0\0\0",
            ]),
        )
        .unwrap();
        assert_eq!(validate_audio(&opus).unwrap().format, "OPUS");

        for file in [mp3, flac, ogg, opus] {
            let _ = fs::remove_file(file);
        }
    }

    #[test]
    fn extracts_bounded_safe_embedded_mp3_metadata() {
        let mp3 = std::env::temp_dir().join(format!(
            "napstr-audio-metadata-{}.mp3.part",
            std::process::id()
        ));
        let mut tag = Vec::new();
        tag.extend_from_slice(&id3_text_frame(b"TIT2", b"Enter\nSandman"));
        tag.extend_from_slice(&id3_text_frame(b"TPE1", b"Metallica"));
        tag.extend_from_slice(&id3_text_frame(b"TALB", b"Metallica (Black Album)"));
        let mut bytes = b"ID3\x03\0\0".to_vec();
        bytes.extend_from_slice(&synchsafe_bytes(tag.len()));
        bytes.extend_from_slice(&tag);
        let mut frame = vec![0u8; 417];
        frame[..4].copy_from_slice(&[0xff, 0xfb, 0x90, 0x00]);
        bytes.extend_from_slice(&frame);
        bytes.extend_from_slice(&frame);
        fs::write(&mp3, bytes).unwrap();

        let metadata = validate_audio(&mp3).unwrap().metadata;
        assert_eq!(metadata.title, "Enter Sandman");
        assert_eq!(metadata.artist, "Metallica");
        assert_eq!(metadata.album, "Metallica (Black Album)");
        let _ = fs::remove_file(mp3);
    }

    #[test]
    fn accepts_embedded_artwork_but_rejects_unknown_wav_chunks() {
        let mp3 = path("art.mp3");
        let mut frame = vec![0u8; 417];
        frame[..4].copy_from_slice(&[0xff, 0xfb, 0x90, 0x00]);
        let mut mp3_bytes = b"ID3\x04\0\0\0\0\0\x04APIC".to_vec();
        mp3_bytes.extend_from_slice(&frame);
        mp3_bytes.extend_from_slice(&frame);
        fs::write(&mp3, mp3_bytes).unwrap();
        assert_eq!(validate_audio(&mp3).unwrap().format, "MP3");

        let flac = path("art.flac");
        let mut flac_bytes = b"fLaC".to_vec();
        flac_bytes.extend_from_slice(&[0, 0, 0, 34]);
        flac_bytes.extend_from_slice(&[0; 34]);
        flac_bytes.extend_from_slice(&[0x86, 0, 0, 1, 0, 0xff, 0xf8]);
        fs::write(&flac, flac_bytes).unwrap();
        assert_eq!(validate_audio(&flac).unwrap().format, "FLAC");

        let ogg = path("art.ogg");
        fs::write(
            &ogg,
            ogg_page(&[b"\x01vorbis", b"\x03vorbisMETADATA_BLOCK_PICTURE"]),
        )
        .unwrap();
        assert_eq!(validate_audio(&ogg).unwrap().format, "OGG");

        let wav_art = path("art.wav");
        let mut wav_art_bytes = wav(&[0, 1, 2, 3]);
        wav_art_bytes[4..8].copy_from_slice(&52u32.to_le_bytes());
        wav_art_bytes.extend_from_slice(b"ID3 \x04\0\0\0APIC");
        fs::write(&wav_art, wav_art_bytes).unwrap();
        assert_eq!(validate_audio(&wav_art).unwrap().format, "WAV");

        let wav_file = path("extra.wav");
        let mut bytes = wav(&[0, 1, 2, 3]);
        bytes[4..8].copy_from_slice(&48u32.to_le_bytes());
        bytes.extend_from_slice(b"evil\0\0\0\0");
        fs::write(&wav_file, bytes).unwrap();
        assert!(validate_audio(&wav_file).is_err());

        for file in [mp3, flac, ogg, wav_art, wav_file] {
            let _ = fs::remove_file(file);
        }
    }
}
