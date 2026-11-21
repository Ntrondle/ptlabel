//! TIFF PackBits as specified for the `M 02` compression mode.

/// Compress one 16-byte raster line. Runs of >= 2 identical bytes become
/// `(1 - n) as i8, byte`; everything else is a literal `(n - 1), bytes...`.
/// If the result exceeds `len` bytes it is sent as one literal of `len + 1` bytes
/// (spec p.35: more than 16 bytes -> all different, 17 bytes).
pub fn encode(line: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(line.len() + 1);
    let mut i = 0;
    while i < line.len() {
        let mut run = 1;
        while i + run < line.len() && line[i + run] == line[i] && run < 128 {
            run += 1;
        }
        if run >= 2 {
            out.push((1i16 - run as i16) as i8 as u8);
            out.push(line[i]);
            i += run;
            continue;
        }
        let start = i;
        i += 1;
        while i < line.len() && i - start < 128 {
            if i + 1 < line.len() && line[i] == line[i + 1] {
                break;
            }
            i += 1;
        }
        out.push((i - start - 1) as u8);
        out.extend_from_slice(&line[start..i]);
    }
    // One literal holds at most 128 bytes (header 0x80 would be a PackBits no-op).
    if line.len() <= 128 && out.len() > line.len() {
        let mut lit = Vec::with_capacity(line.len() + 1);
        lit.push((line.len() - 1) as u8);
        lit.extend_from_slice(line);
        return lit;
    }
    out
}

#[cfg(test)]
pub fn decode(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < data.len() {
        let n = data[i] as i8;
        i += 1;
        if n >= 0 {
            let len = n as usize + 1;
            out.extend_from_slice(&data[i..i + len]);
            i += len;
        } else if n != -128 {
            let len = (1 - n as i16) as usize;
            out.extend(std::iter::repeat_n(data[i], len));
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_example() {
        // Raster Command Reference p.35
        let mut line = vec![0u8; 20];
        line.extend_from_slice(&[0x22, 0x22, 0x23, 0xBA, 0xBF, 0xA2, 0x22, 0x2B]);
        assert_eq!(
            encode(&line),
            vec![0xED, 0x00, 0xFF, 0x22, 0x05, 0x23, 0xBA, 0xBF, 0xA2, 0x22, 0x2B]
        );
    }

    #[test]
    fn roundtrip_and_bound() {
        let cases: Vec<Vec<u8>> = vec![
            vec![0; 16],
            vec![0xFF; 16],
            (0..16).collect(),
            vec![1, 1, 2, 3, 3, 4, 5, 5, 6, 7, 7, 8, 9, 9, 10, 11],
            vec![0, 0, 0, 0, 0, 0, 0, 0x0F, 0xF0, 0, 0, 0, 0, 0, 0, 0],
        ];
        for c in cases {
            let e = encode(&c);
            assert!(e.len() <= 17, "{c:?} -> {e:?}");
            assert_eq!(decode(&e), c);
        }
    }

    #[test]
    fn driver_sample_lines() {
        // Raster Command Reference p.11: the driver sends `G 02 00 F1 00` and `F6 00 04 16 99 6C 98 6C`.
        assert_eq!(encode(&[0; 16]), vec![0xF1, 0x00]);
        let mut line = vec![0u8; 11];
        line.extend_from_slice(&[0x16, 0x99, 0x6C, 0x98, 0x6C]);
        assert_eq!(encode(&line), vec![0xF6, 0x00, 0x04, 0x16, 0x99, 0x6C, 0x98, 0x6C]);
    }

    #[test]
    fn more_than_16_bytes_becomes_one_literal() {
        // Greedy encoding is FF 01 | 01 02 03 | FF 04 .. FF 09 = 17 bytes -> spec: all different.
        let line = [1, 1, 2, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9];
        let e = encode(&line);
        assert_eq!(e[0], 0x0F);
        assert_eq!(&e[1..], &line);
        // Alternating bytes: one 16-byte literal; alternating pairs: eight 2-byte runs.
        let alt: Vec<u8> = (0..16).map(|i| if i % 2 == 0 { 0xAA } else { 0x55 }).collect();
        assert_eq!(encode(&alt).len(), 17);
        let pairs: Vec<u8> = (0..16).map(|i| if i / 2 % 2 == 0 { 0xAA } else { 0x55 }).collect();
        assert_eq!(encode(&pairs), [0xFFu8, 0xAA, 0xFF, 0x55].repeat(4));
    }

    #[test]
    fn run_and_literal_limits() {
        assert!(encode(&[]).is_empty());
        assert_eq!(encode(&[7]), vec![0x00, 7]);
        assert_eq!(encode(&[7; 128]), vec![0x81, 7]);
        assert_eq!(encode(&[7; 129]), vec![0x81, 7, 0x00, 7]);
        assert_eq!(encode(&[7; 130]), vec![0x81, 7, 0xFF, 7]);
        let lit: Vec<u8> = (0..=255u8).map(|b| b.wrapping_mul(37)).collect(); // all distinct
        for n in [127, 128, 129, 200, 256] {
            let e = encode(&lit[..n]);
            assert_ne!(e[0], 0x80, "n={n}");
            assert_eq!(e[0], (n.min(128) - 1) as u8, "n={n}");
            assert_eq!(decode(&e), &lit[..n], "n={n}");
        }
    }

    #[test]
    fn random_lines_roundtrip() {
        let mut x = 0x2545_F491u32;
        for _ in 0..20_000 {
            let mut line = [0u8; 16];
            for b in &mut line {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                // Few distinct values so runs and literals mix.
                *b = [0x00, 0xFF, 0x0F, 0x81][(x & 3) as usize];
            }
            let e = encode(&line);
            assert!(e.len() <= 17, "{line:?} -> {e:?}");
            assert_eq!(decode(&e), line, "{e:?}");
        }
    }
}
