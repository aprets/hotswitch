pub const AUDIO_PORT: u16 = 24802;
pub const SAMPLE_RATE: u32 = 48000;
pub const CHANNELS: u16 = 2;

/// Max UDP payload: 1472 bytes (1500 MTU - 20 IP - 8 UDP).
/// Header: 1 byte tag + 4 bytes sequence + 2 bytes channels.
/// Remaining bytes / 4 bytes per f32 = max samples.
const HEADER_SIZE: usize = 7;
const MAX_PAYLOAD: usize = 1472;
pub const MAX_SAMPLES_PER_PACKET: usize = (MAX_PAYLOAD - HEADER_SIZE) / 4;

const TAG_AUDIO: u8 = 0x07;

pub fn audio_to_bytes(seq: u32, channels: u16, samples: &[f32], buf: &mut [u8]) -> usize {
    let count = samples.len().min(MAX_SAMPLES_PER_PACKET);
    buf[0] = TAG_AUDIO;
    buf[1..5].copy_from_slice(&seq.to_be_bytes());
    buf[5..7].copy_from_slice(&channels.to_be_bytes());
    for (i, &s) in samples[..count].iter().enumerate() {
        let off = HEADER_SIZE + i * 4;
        buf[off..off + 4].copy_from_slice(&s.to_le_bytes());
    }
    HEADER_SIZE + count * 4
}

pub fn audio_from_bytes(buf: &[u8]) -> Option<(u32, u16, &[u8])> {
    if buf.len() < HEADER_SIZE || buf[0] != TAG_AUDIO {
        return None;
    }
    let seq = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]);
    let channels = u16::from_be_bytes([buf[5], buf[6]]);
    Some((seq, channels, &buf[HEADER_SIZE..]))
}

/// Interpret raw audio bytes as f32 samples (little-endian).
pub fn raw_to_samples(raw: &[u8]) -> impl Iterator<Item = f32> + '_ {
    raw.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_audio() {
        let samples = vec![0.5f32, -0.25, 1.0, -1.0];
        let mut buf = [0u8; MAX_PAYLOAD];
        let len = audio_to_bytes(42, 2, &samples, &mut buf);
        let (seq, channels, raw) = audio_from_bytes(&buf[..len]).unwrap();
        assert_eq!(seq, 42);
        assert_eq!(channels, 2);
        let decoded: Vec<f32> = raw_to_samples(raw).collect();
        assert_eq!(decoded, samples);
    }

    #[test]
    fn rejects_wrong_tag() {
        let buf = [0x01, 0, 2];
        assert!(audio_from_bytes(&buf).is_none());
    }
}
