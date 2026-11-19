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
    let mut b = vec![(oid[0] * 40 + oid[1]) as u8];
    for &n in &oid[2..] {
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
}

