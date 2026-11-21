//! Minimal SNMPv1 GET (one OID, OCTET STRING result). Enough for the Brother status block.

use anyhow::{Context, Result, anyhow, bail};
use std::net::UdpSocket;
use std::time::Duration;

/// Brother private MIB: 32-byte raster status block (verified on PT-E550W).
pub const BROTHER_STATUS_OID: &[u32] = &[1, 3, 6, 1, 4, 1, 2435, 3, 3, 9, 1, 6, 1, 0];

fn push_len(out: &mut Vec<u8>, len: usize) {
    if len < 0x80 {
        out.push(len as u8);
    } else if len < 0x100 {
        out.extend_from_slice(&[0x81, len as u8]);
    } else {
        out.extend_from_slice(&[0x82, (len >> 8) as u8, len as u8]);
    }
}

fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut v = vec![tag];
    push_len(&mut v, content.len());
    v.extend_from_slice(content);
    v
}

fn int(v: u32) -> Vec<u8> {
    let mut b = v.to_be_bytes().to_vec();
    while b.len() > 1 && b[0] == 0 && b[1] & 0x80 == 0 {
        b.remove(0);
    }
    if b[0] & 0x80 != 0 {
        b.insert(0, 0);
    }
    tlv(0x02, &b)
}

fn encode_oid(oid: &[u32]) -> Vec<u8> {
    let mut b = vec![];
    // X.690 8.19.4: the first two arcs form one subidentifier, itself base-128 (2.999 -> 88 37).
    for n in std::iter::once(oid[0] * 40 + oid[1]).chain(oid[2..].iter().copied()) {
        let mut stack = vec![(n & 0x7F) as u8];
        let mut n = n >> 7;
        while n > 0 {
            stack.push((n & 0x7F) as u8 | 0x80);
            n >>= 7;
        }
        b.extend(stack.iter().rev());
    }
    tlv(0x06, &b)
}

pub fn get_request(community: &str, oid: &[u32], request_id: u32) -> Vec<u8> {
    let varbind = tlv(0x30, &[encode_oid(oid), vec![0x05, 0x00]].concat());
    let pdu = tlv(
        0xA0,
        &[int(request_id), int(0), int(0), tlv(0x30, &varbind)].concat(),
    );
    tlv(0x30, &[int(0), tlv(0x04, community.as_bytes()), pdu].concat())
}

/// Returns (tag, content, rest).
fn read_tlv(b: &[u8]) -> Result<(u8, &[u8], &[u8])> {
    if b.len() < 2 {
        bail!("truncated BER");
    }
    let tag = b[0];
    let (len, hdr) = match b[1] {
        l if l < 0x80 => (l as usize, 2),
        0x81 => (*b.get(2).ok_or_else(|| anyhow!("truncated"))? as usize, 3),
        0x82 => {
            if b.len() < 4 {
                bail!("truncated");
            }
            (((b[2] as usize) << 8) | b[3] as usize, 4)
        }
        l => bail!("unsupported BER length 0x{l:02X}"),
    };
    if b.len() < hdr + len {
        bail!("truncated BER value");
    }
    Ok((tag, &b[hdr..hdr + len], &b[hdr + len..]))
}

fn read_int(b: &[u8]) -> Result<(i64, &[u8])> {
    let (tag, c, rest) = read_tlv(b)?;
    if tag != 0x02 {
        bail!("expected INTEGER");
    }
    // X.690 8.3.1: one or more octets; more than 8 would silently overflow the i64.
    if c.is_empty() || c.len() > 8 {
        bail!("bad INTEGER length {}", c.len());
    }
    let mut v: i64 = if c.first().is_some_and(|&x| x & 0x80 != 0) { -1 } else { 0 };
    for &x in c {
        v = (v << 8) | x as i64;
    }
    Ok((v, rest))
}

pub fn parse_response(b: &[u8], request_id: u32) -> Result<Vec<u8>> {
    let (_, msg, _) = read_tlv(b)?;
    let (_, rest) = read_int(msg)?; // version
    let (_, _, rest) = read_tlv(rest)?; // community
    let (tag, pdu, _) = read_tlv(rest)?;
    if tag != 0xA2 {
        bail!("not a GetResponse (0x{tag:02X})");
    }
    let (id, rest) = read_int(pdu)?;
    if id != request_id as i64 {
        bail!("request id mismatch");
    }
    let (err, rest) = read_int(rest)?;
    let (_, rest) = read_int(rest)?;
    if err != 0 {
        bail!("SNMP error status {err}");
    }
    let (_, vbl, _) = read_tlv(rest)?;
    let (_, vb, _) = read_tlv(vbl)?;
    let (_, _, val) = read_tlv(vb)?; // skip OID
    let (tag, val, _) = read_tlv(val)?;
    if tag != 0x04 {
        bail!("value is not an OCTET STRING (0x{tag:02X})");
    }
    Ok(val.to_vec())
}

pub fn get(host: &str, oid: &[u32], timeout: Duration) -> Result<Vec<u8>> {
    let sock = UdpSocket::bind("0.0.0.0:0")?;
    sock.connect((host, 161)).with_context(|| format!("resolve {host}"))?;
    sock.set_read_timeout(Some(timeout))?;
    let id = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos()
        & 0x7FFF_FFFF)
        | 1;
    let req = get_request("public", oid, id);
    let mut buf = [0u8; 1500];
    for _ in 0..3 {
        sock.send(&req)?;
        match sock.recv(&mut buf) {
            Ok(n) => return parse_response(&buf[..n], id),
            Err(e)
                if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) =>
            {
                continue;
            }
            Err(e) => return Err(e.into()),
        }
    }
    bail!("no SNMP reply from {host}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_brother_oid() {
        let o = encode_oid(BROTHER_STATUS_OID);
        assert_eq!(
            o,
            vec![0x06, 0x0E, 0x2B, 6, 1, 4, 1, 0x93, 0x03, 3, 3, 9, 1, 6, 1, 0]
        );
    }

    #[test]
    fn parses_response() {
        // GetResponse with id 7 and a 4-byte octet string
        let vb = tlv(0x30, &[encode_oid(BROTHER_STATUS_OID), tlv(0x04, &[1, 2, 3, 4])].concat());
        let pdu = tlv(0xA2, &[int(7), int(0), int(0), tlv(0x30, &vb)].concat());
        let msg = tlv(0x30, &[int(0), tlv(0x04, b"public"), pdu].concat());
        assert_eq!(parse_response(&msg, 7).unwrap(), vec![1, 2, 3, 4]);
        assert!(parse_response(&msg, 8).is_err());
    }

    #[test]
    fn get_request_bytes() {
        #[rustfmt::skip]
        let want = [
            0x30, 0x2D, 0x02, 0x01, 0x00, 0x04, 0x06, b'p', b'u', b'b', b'l', b'i', b'c',
            0xA0, 0x20, 0x02, 0x02, 0x12, 0x34, 0x02, 0x01, 0x00, 0x02, 0x01, 0x00,
            0x30, 0x14, 0x30, 0x12,
            0x06, 0x0E, 0x2B, 6, 1, 4, 1, 0x93, 0x03, 3, 3, 9, 1, 6, 1, 0, 0x05, 0x00,
        ];
        assert_eq!(get_request("public", BROTHER_STATUS_OID, 0x1234), want);
        // Request ids with the top bit of a byte set need a leading 00 (positive INTEGER).
        assert_eq!(int(0x80), vec![0x02, 0x02, 0x00, 0x80]);
        assert_eq!(int(0x7FFF_FFFF), vec![0x02, 0x04, 0x7F, 0xFF, 0xFF, 0xFF]);
        assert_eq!(int(0xFFFF_FFFF), vec![0x02, 0x05, 0x00, 0xFF, 0xFF, 0xFF, 0xFF]);
        assert_eq!(int(0), vec![0x02, 0x01, 0x00]);
    }

    #[test]
    fn encodes_large_first_arc() {
        assert_eq!(encode_oid(&[2, 999, 3]), vec![0x06, 0x03, 0x88, 0x37, 0x03]);
    }

    /// GetResponse as the PT-E550W sends it: 32-byte status block (PLAN.md: 80 20 42 30 66 … 0C 01 … 01 08).
    #[rustfmt::skip]
    const E550W_REPLY: [u8; 81] = [
        0x30, 0x4F, 0x02, 0x01, 0x00, 0x04, 0x06, b'p', b'u', b'b', b'l', b'i', b'c',
        0xA2, 0x42, 0x02, 0x04, 0x01, 0x02, 0x03, 0x05, 0x02, 0x01, 0x00, 0x02, 0x01, 0x00,
        0x30, 0x34, 0x30, 0x32,
        0x06, 0x0E, 0x2B, 6, 1, 4, 1, 0x93, 0x03, 3, 3, 9, 1, 6, 1, 0,
        0x04, 0x20,
        0x80, 0x20, 0x42, 0x30, 0x66, 0x30, 0x01, 0x00, 0x00, 0x00, 0x0C, 0x01, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];

    #[test]
    fn parses_e550w_reply_and_rejects_every_truncation() {
        let v = parse_response(&E550W_REPLY, 0x0102_0305).unwrap();
        let st = crate::proto::Status::parse(&v).unwrap();
        assert_eq!((st.model_name(), st.width()), (Some("PT-E550W"), 12));
        assert!(parse_response(&E550W_REPLY, 0x0102_0306).is_err());
        for n in 0..E550W_REPLY.len() {
            assert!(parse_response(&E550W_REPLY[..n], 0x0102_0305).is_err(), "prefix {n}");
        }
        // Garbage must not panic either.
        for seed in 0..=255u8 {
            let mut b = E550W_REPLY;
            b[(seed as usize * 7) % b.len()] ^= seed;
            let _ = parse_response(&b, 0x0102_0305);
        }
    }

    #[test]
    fn long_form_lengths_and_error_status() {
        let big = vec![0xAB; 300];
        let vb = tlv(0x30, &[encode_oid(BROTHER_STATUS_OID), tlv(0x04, &big)].concat());
        let pdu = tlv(0xA2, &[int(9), int(0), int(0), tlv(0x30, &vb)].concat());
        let msg = tlv(0x30, &[int(0), tlv(0x04, b"public"), pdu].concat());
        assert_eq!(&msg[..4], &[0x30, 0x82, 0x01, 0x60]);
        assert_eq!(parse_response(&msg, 9).unwrap(), big);

        let vb = tlv(0x30, &[encode_oid(BROTHER_STATUS_OID), vec![0x05, 0x00]].concat());
        let pdu = tlv(0xA2, &[int(9), int(2), int(1), tlv(0x30, &vb)].concat());
        let msg = tlv(0x30, &[int(0), tlv(0x04, b"public"), pdu].concat());
        assert!(format!("{}", parse_response(&msg, 9).unwrap_err()).contains("error status 2"));
    }

    #[test]
    fn integer_decoding() {
        assert_eq!(read_int(&[0x02, 0x01, 0xFF]).unwrap().0, -1);
        assert_eq!(read_int(&[0x02, 0x02, 0xFF, 0x7F]).unwrap().0, -129);
        assert_eq!(read_int(&[0x02, 0x05, 0x00, 0xFF, 0xFF, 0xFF, 0xFF]).unwrap().0, 0xFFFF_FFFF);
        assert!(read_int(&[0x02, 0x00]).is_err());
        assert!(read_int(&[0x02, 0x09, 0, 0, 0, 0, 0, 0, 0, 0, 1]).is_err());
    }
}

