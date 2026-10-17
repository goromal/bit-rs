use anyhow::{bail, ensure, Context, Result};
use async_trait::async_trait;
use bit_rs::plugin::BitCheck;
use serde::Deserialize;
use std::time::Duration;
use tokio::{io::AsyncReadExt, net::TcpStream};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Ardupilot {
    /// MAVLink TCP endpoint, e.g. the local mavlink-router's GCS port.
    address: String,
    #[serde(default = "max_seconds")]
    max_seconds: f64,
    /// Only accept heartbeats from this MAVLink system ID.
    system_id: Option<u8>,
}
fn max_seconds() -> f64 {
    3.0
}

const STX_V1: u8 = 0xFE;
const STX_V2: u8 = 0xFD;
const HEARTBEAT_CRC_EXTRA: u8 = 50;
const MAV_AUTOPILOT_INVALID: u8 = 8;
const MAX_BUFFER: usize = 64 * 1024;

#[derive(Debug, PartialEq)]
struct Heartbeat {
    system_id: u8,
    component_id: u8,
    vehicle_type: u8,
    autopilot: u8,
    system_status: u8,
}

fn crc_accumulate(crc: u16, byte: u8) -> u16 {
    let mut tmp = byte ^ (crc & 0xff) as u8;
    tmp ^= tmp << 4;
    (crc >> 8) ^ (u16::from(tmp) << 8) ^ (u16::from(tmp) << 3) ^ (u16::from(tmp) >> 4)
}

enum Frame {
    /// A CRC-valid heartbeat frame of this many bytes.
    Heartbeat(usize, Heartbeat),
    /// Possibly a frame, but more bytes are needed to tell.
    Partial,
    /// Not a heartbeat at this position.
    Other,
}

/// Parse a MAVLink v1 or v2 HEARTBEAT at the start of `buf`. Only heartbeats
/// are validated (their CRC extra is known), so other messages are skipped
/// byte by byte rather than trusted for their length: a stray start byte in
/// the stream must not swallow the frame after it.
fn parse_heartbeat(buf: &[u8]) -> Frame {
    let (header, msgid_len, trailer) = match buf.first() {
        Some(&STX_V1) => (6, 1, 2),
        Some(&STX_V2) => {
            let Some(&incompat) = buf.get(2) else {
                return Frame::Partial;
            };
            (10, 3, if incompat & 1 != 0 { 2 + 13 } else { 2 })
        }
        _ => return Frame::Other,
    };
    if buf.len() < header {
        return Frame::Partial;
    }
    let msgid_at = header - msgid_len;
    if buf[msgid_at..header].iter().any(|&b| b != 0) {
        return Frame::Other;
    }
    let len = usize::from(buf[1]);
    let total = header + len + trailer;
    if buf.len() < total {
        return Frame::Partial;
    }
    let crc = buf[1..header + len]
        .iter()
        .chain(std::iter::once(&HEARTBEAT_CRC_EXTRA))
        .fold(0xffff, |crc, &b| crc_accumulate(crc, b));
    if crc.to_le_bytes() != buf[header + len..header + len + 2] {
        return Frame::Other;
    }
    // MAVLink 2 truncates trailing zero bytes of the payload.
    let mut payload = [0u8; 9];
    let kept = len.min(payload.len());
    payload[..kept].copy_from_slice(&buf[header..header + kept]);
    Frame::Heartbeat(
        total,
        Heartbeat {
            system_id: buf[msgid_at - 2],
            component_id: buf[msgid_at - 1],
            vehicle_type: payload[4],
            autopilot: payload[5],
            system_status: payload[7],
        },
    )
}

/// Scan `buf` for a heartbeat sent by an autopilot (optionally from
/// `system_id`), consuming the bytes before it. Without one, drops everything
/// that cannot be the start of a frame still arriving.
fn take_autopilot_heartbeat(buf: &mut Vec<u8>, system_id: Option<u8>) -> Option<Heartbeat> {
    let mut keep_from = buf.len();
    for at in 0..buf.len() {
        match parse_heartbeat(&buf[at..]) {
            Frame::Heartbeat(n, hb)
                if hb.autopilot != MAV_AUTOPILOT_INVALID
                    && system_id.is_none_or(|id| id == hb.system_id) =>
            {
                buf.drain(..at + n);
                return Some(hb);
            }
            Frame::Partial => keep_from = keep_from.min(at),
            _ => {}
        }
    }
    buf.drain(..keep_from);
    None
}

#[async_trait]
impl BitCheck for Ardupilot {
    fn remediation(&self) -> String {
        format!(
            "No autopilot heartbeat arrived through {} within {} s.\n\
             - Services: `systemctl status ardurouter ardusitl`; logs: `journalctl -u ardurouter -n 100`.\n\
             - Router listening: `ss -tlnp | grep {}`.\n\
             - Hardware FC: check the USB/serial cable and that the device in \
             drone.base.fcSerialDevice exists (`ls -l /dev/ttyACM*`).\n\
             - SITL: `journalctl -u ardusitl -n 100` for crashes or a missing eeprom/params.",
            self.address,
            self.max_seconds,
            self.address.rsplit(':').next().unwrap_or_default()
        )
    }
    async fn check(&self) -> Result<()> {
        ensure!(
            self.max_seconds.is_finite() && self.max_seconds > 0.0,
            "max_seconds must be positive"
        );
        let deadline = Duration::from_secs_f64(self.max_seconds);
        let heartbeat = tokio::time::timeout(deadline, async {
            let mut stream = TcpStream::connect(&self.address)
                .await
                .with_context(|| format!("connect to {}", self.address))?;
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = stream.read(&mut chunk).await?;
                if n == 0 {
                    bail!("{} closed the connection", self.address);
                }
                buf.extend_from_slice(&chunk[..n]);
                if let Some(hb) = take_autopilot_heartbeat(&mut buf, self.system_id) {
                    return Ok(hb);
                }
                buf.drain(..buf.len().saturating_sub(MAX_BUFFER));
            }
        })
        .await
        .with_context(|| {
            format!(
                "no autopilot heartbeat from {} within {} s",
                self.address, self.max_seconds
            )
        })??;
        println!(
            "heartbeat from system {} component {} (type {}, autopilot {}, status {})",
            heartbeat.system_id,
            heartbeat.component_id,
            heartbeat.vehicle_type,
            heartbeat.autopilot,
            heartbeat.system_status
        );
        Ok(())
    }
}
#[tokio::main]
async fn main() -> Result<()> {
    bit_rs::plugin::run::<Ardupilot>().await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_crc(mut frame: Vec<u8>) -> Vec<u8> {
        let header = if frame[0] == STX_V1 { 6 } else { 10 };
        let crc = frame[1..]
            .iter()
            .chain(std::iter::once(&HEARTBEAT_CRC_EXTRA))
            .fold(0xffff, |crc, &b| crc_accumulate(crc, b));
        assert_eq!(frame.len(), header + usize::from(frame[1]));
        frame.extend_from_slice(&crc.to_le_bytes());
        frame
    }
    /// ArduCopter (system 1): quadrotor, ArduPilot autopilot, standby.
    fn copter_v2() -> Vec<u8> {
        with_crc(vec![
            0xfd, 0x09, 0x00, 0x00, 0x2a, 0x01, 0x01, 0x00, 0x00, 0x00, 0, 0, 0, 0, 2, 3, 0x51, 3,
            3,
        ])
    }
    /// A ground station (system 255): MAV_TYPE_GCS, MAV_AUTOPILOT_INVALID.
    fn gcs_v1() -> Vec<u8> {
        with_crc(vec![
            0xfe, 0x09, 0x07, 0xff, 0xbe, 0x00, 0, 0, 0, 0, 6, 8, 0, 0, 3,
        ])
    }

    #[test]
    fn crc_matches_reference_vector() {
        // MCRF4XX of "123456789" is 0x6F91.
        assert_eq!(
            b"123456789"
                .iter()
                .fold(0xffff, |c, &b| crc_accumulate(c, b)),
            0x6f91
        );
    }

    #[test]
    fn finds_autopilot_heartbeat_among_noise_and_gcs_frames() {
        let mut buf = vec![0x00, 0x13, STX_V2];
        buf.extend(gcs_v1());
        buf.extend(copter_v2());
        let hb = take_autopilot_heartbeat(&mut buf, Some(1)).unwrap();
        assert_eq!(
            hb,
            Heartbeat {
                system_id: 1,
                component_id: 1,
                vehicle_type: 2,
                autopilot: 3,
                system_status: 3,
            }
        );
        assert!(buf.is_empty());
    }

    #[test]
    fn ignores_gcs_wrong_system_bad_crc_and_waits_for_partial_frames() {
        let mut buf = gcs_v1();
        assert_eq!(take_autopilot_heartbeat(&mut buf, None), None);
        let mut buf = copter_v2();
        assert_eq!(take_autopilot_heartbeat(&mut buf, Some(2)), None);
        let mut bad = copter_v2();
        *bad.last_mut().unwrap() ^= 0xff;
        assert_eq!(take_autopilot_heartbeat(&mut bad, None), None);
        let frame = copter_v2();
        let mut buf = frame[..10].to_vec();
        assert_eq!(take_autopilot_heartbeat(&mut buf, None), None);
        assert_eq!(buf.len(), 10, "partial frame kept for the next read");
        buf.extend_from_slice(&frame[10..]);
        assert!(take_autopilot_heartbeat(&mut buf, None).is_some());
    }

    #[test]
    fn truncated_v2_payload_is_zero_extended() {
        // custom_mode 0 and trailing zero fields dropped: payload [0,0,0,0,2,3].
        let frame = with_crc(vec![
            0xfd, 0x06, 0x00, 0x00, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00, 0, 0, 0, 0, 2, 3,
        ]);
        let mut buf = frame;
        let hb = take_autopilot_heartbeat(&mut buf, None).unwrap();
        assert_eq!((hb.vehicle_type, hb.autopilot, hb.system_status), (2, 3, 0));
    }
}
