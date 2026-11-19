//! TIFF PackBits as specified for the `M 02` compression mode.

/// Compress one 16-byte raster line. Runs of >= 2 identical bytes become
/// `(1 - n) as i8, byte`; everything else is a literal `(n - 1), bytes...`.
/// If the result would exceed `len + 1` bytes it is sent as one literal.
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
    if out.len() > line.len() + 1 {
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
        } else {
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
}
