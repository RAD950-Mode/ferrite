//! Paired, deterministic minute-receiver sensitivity experiment. Run with --release.
use urad950::{encode_tones, Callsign, Decoded, Frame, Modulator, Receiver60, DEFAULT_SPACING_HZ, MAX_DECODED};

const RATE: u32 = 24_000;
const SIGMA: f64 = 1500.0;
const REPEATS: usize = 5;

fn cq(call: &[u8]) -> Frame {
    Frame::Cq { sender: Callsign::encode(call).unwrap(), grid4: 1234, tx_pwr: 0, reserved: 0 }
}

fn waveform(frame: Frame, hz: u32, db: f64, offset_ms: usize) -> Vec<i16> {
    let amplitude = (2.0 * SIGMA.powi(2) * 2500.0 / (RATE / 2) as f64 * 10f64.powf(db / 10.0)).sqrt().round();
    let mut wave = vec![0; RATE as usize * 10];
    let mut tx = Modulator::new(encode_tones(frame), RATE, hz, DEFAULT_SPACING_HZ, amplitude as i16);
    for v in &mut wave[offset_ms * 24..] {
        let Some(sample) = tx.next_sample() else {
            break;
        };
        *v = sample;
    }
    wave
}

fn noise(seed: u64) -> Vec<i16> {
    let mut state = seed;
    (0..RATE as usize * 10 * REPEATS)
        .map(|_| {
            let mut sum = -6.0;
            for _ in 0..12 {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                sum += (state >> 16) as u32 as f64 / u32::MAX as f64;
            }
            (sum * SIGMA).round() as i16
        })
        .collect()
}

fn decode(mut rx: Receiver60, noise: &[i16], weak: &[i16], strong: &[i16]) -> (usize, Vec<Frame>) {
    for (sub, samples) in noise.chunks_exact(weak.len()).enumerate() {
        if sub > 0 {
            rx.start_subslot();
        }
        for (i, chunk) in samples.as_chunks::<192>().0.iter().enumerate() {
            let pcm: [i16; 192] = core::array::from_fn(|j| {
                chunk[j].saturating_add(weak[i * 192 + j]).saturating_add(strong[i * 192 + j])
            });
            rx.push_samples(&pcm);
        }
    }
    let mut out = [Decoded::EMPTY; MAX_DECODED];
    let (primary, total) = rx.finish_minute_with_counts(&mut out);
    (primary, out[..total].iter().map(|d| d.frame).collect())
}

fn main() {
    eprintln!("Receiver60: {} bytes", std::mem::size_of::<Receiver60>());
    let trials: usize = std::env::args().nth(1).map_or(16, |n| n.parse().unwrap());
    let first: u64 = std::env::args().nth(2).map_or(1, |n| n.parse().unwrap());
    let adjacent_offset_ms: usize = std::env::args().nth(3).map_or(440, |n| n.parse().unwrap());
    let noise_only = std::env::args().nth(4).as_deref() == Some("noise-only");
    assert!(adjacent_offset_ms <= 1200);
    assert!(trials > 0 && first > 0);
    let target = cq(b"NQ6N");
    let neighbor = cq(b"K1ABC");
    let silent = vec![0; RATE as usize * 10];
    println!("seed,hz,snr,neighbor,weak,strong,unexpected,weak_primary,strong_primary");
    for seed in first..first + trials as u64 {
        let noise = noise(seed.wrapping_mul(0x9e3779b97f4a7c15));
        println!("{seed},0,0,false,0,0,{},0,0", decode(Receiver60::new(), &noise, &silent, &silent).1.len());
        if noise_only {
            continue;
        }
        for hz in [1275, 2175] {
            for db in [-23, -22, -21, -20, -19] {
                let weak = waveform(target, hz, db as f64, 400);
                let strong = waveform(neighbor, hz - 225, db as f64 + 18.0, adjacent_offset_ms);
                for adjacent in [false, true] {
                    let (primary, decoded) =
                        decode(Receiver60::new(), &noise, &weak, if adjacent { &strong } else { &silent });
                    let unexpected =
                        decoded.iter().filter(|&&frame| frame != target && !(adjacent && frame == neighbor)).count();
                    println!(
                        "{seed},{hz},{db},{adjacent},{},{},{unexpected},{},{}",
                        decoded.contains(&target) as u8,
                        decoded.contains(&neighbor) as u8,
                        decoded[..primary].contains(&target) as u8,
                        decoded[..primary].contains(&neighbor) as u8
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upper_channel_survives_stronger_neighbor() {
        let target = cq(b"NQ6N");
        let neighbor = cq(b"K1ABC");
        let samples = noise(1008u64.wrapping_mul(0x9e3779b97f4a7c15));
        let weak = waveform(target, 2175, -21.0, 400);
        let strong = waveform(neighbor, 1950, -3.0, 440);
        let (_, decoded) = decode(Receiver60::new(), &samples, &weak, &strong);
        assert_eq!(decoded.len(), 2);
        assert!(decoded.contains(&target) && decoded.contains(&neighbor));
    }
}
