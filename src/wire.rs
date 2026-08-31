//! Protocolo de aplicação (`WireMessage`) transportado sobre um
//! `EncryptedChannel` (ver `kvm_share::noise`) — nunca em texto claro.

use crate::noise::EncryptedChannel;
use crate::{FRAME_LEN, read_event, write_event};
use anyhow::{Context, Result, anyhow, bail};
use evdev::InputEvent;
use std::io::{Read, Write};

const TAG_INPUT_EVENT: u8 = 0x01;
const TAG_CLIPBOARD_TEXT: u8 = 0x02;
const TAG_CLIPBOARD_IMAGE: u8 = 0x03;
const TAG_FOCUS_HANDOFF: u8 = 0x04;
const TAG_HEARTBEAT: u8 = 0x05;

#[derive(Debug)]
pub enum WireMessage {
    InputEvent(InputEvent),
    ClipboardText(String),
    ClipboardImage { mime: String, bytes: Vec<u8> },
    FocusHandoff,
    Heartbeat,
}

pub fn write_message<S: Read + Write>(
    channel: &mut EncryptedChannel<S>,
    msg: &WireMessage,
) -> Result<()> {
    channel.send(&encode(msg)?)
}

/// `Ok(None)` em EOF limpo (conexão encerrada entre mensagens).
pub fn read_message<S: Read + Write>(
    channel: &mut EncryptedChannel<S>,
) -> Result<Option<WireMessage>> {
    let Some(bytes) = channel.recv()? else {
        return Ok(None);
    };
    decode(&bytes).map(Some)
}

fn encode(msg: &WireMessage) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    match msg {
        WireMessage::InputEvent(ev) => {
            buf.push(TAG_INPUT_EVENT);
            write_event(&mut buf, ev)?;
        }
        WireMessage::ClipboardText(text) => {
            buf.push(TAG_CLIPBOARD_TEXT);
            buf.extend_from_slice(text.as_bytes());
        }
        WireMessage::ClipboardImage { mime, bytes } => {
            buf.push(TAG_CLIPBOARD_IMAGE);
            buf.extend_from_slice(&(mime.len() as u16).to_be_bytes());
            buf.extend_from_slice(mime.as_bytes());
            buf.extend_from_slice(bytes);
        }
        WireMessage::FocusHandoff => buf.push(TAG_FOCUS_HANDOFF),
        WireMessage::Heartbeat => buf.push(TAG_HEARTBEAT),
    }
    Ok(buf)
}

fn decode(bytes: &[u8]) -> Result<WireMessage> {
    let (&tag, payload) = bytes
        .split_first()
        .ok_or_else(|| anyhow!("mensagem vazia"))?;
    match tag {
        TAG_INPUT_EVENT => {
            if payload.len() != FRAME_LEN {
                bail!(
                    "frame de InputEvent com tamanho inesperado: {} bytes",
                    payload.len()
                );
            }
            let ev = read_event(&mut std::io::Cursor::new(payload))
                .context("falha decodificando InputEvent")?
                .ok_or_else(|| anyhow!("frame de InputEvent vazio"))?;
            Ok(WireMessage::InputEvent(ev))
        }
        TAG_CLIPBOARD_TEXT => {
            let text = String::from_utf8(payload.to_vec()).context("clipboard text não é utf-8")?;
            Ok(WireMessage::ClipboardText(text))
        }
        TAG_CLIPBOARD_IMAGE => {
            let mime_len = *payload
                .first_chunk::<2>()
                .ok_or_else(|| anyhow!("payload de ClipboardImage truncado (mime_len)"))?;
            let mime_len = u16::from_be_bytes(mime_len) as usize;
            let rest = &payload[2..];
            if rest.len() < mime_len {
                bail!("payload de ClipboardImage truncado (mime)");
            }
            let mime = String::from_utf8(rest[..mime_len].to_vec()).context("mime não é utf-8")?;
            let bytes = rest[mime_len..].to_vec();
            Ok(WireMessage::ClipboardImage { mime, bytes })
        }
        TAG_FOCUS_HANDOFF => Ok(WireMessage::FocusHandoff),
        TAG_HEARTBEAT => Ok(WireMessage::Heartbeat),
        other => bail!("tag de WireMessage desconhecida: {other:#x}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::noise::{handshake_as_initiator, handshake_as_responder};
    use evdev::EventType;
    use std::os::unix::net::UnixStream;
    use std::thread;

    fn channel_pair() -> (EncryptedChannel<UnixStream>, EncryptedChannel<UnixStream>) {
        let psk = [5u8; 32];
        let (a, b) = UnixStream::pair().unwrap();
        let initiator_handle = thread::spawn(move || handshake_as_initiator(a, "a", &psk));
        let responder =
            handshake_as_responder(b, move |name| (name == "a").then_some(psk)).unwrap();
        let initiator = initiator_handle.join().unwrap().unwrap();
        (initiator, responder)
    }

    fn roundtrip(msg: WireMessage) -> WireMessage {
        let (mut tx, mut rx) = channel_pair();
        let handle = thread::spawn(move || {
            write_message(&mut tx, &msg).unwrap();
        });
        let received = read_message(&mut rx).unwrap().unwrap();
        handle.join().unwrap();
        received
    }

    #[test]
    fn input_event_roundtrips_and_matches_raw_frame_format() {
        let ev = InputEvent::new(EventType::RELATIVE.0, 0x00, 5);
        match roundtrip(WireMessage::InputEvent(ev)) {
            WireMessage::InputEvent(decoded) => {
                assert_eq!(decoded.event_type(), ev.event_type());
                assert_eq!(decoded.code(), ev.code());
                assert_eq!(decoded.value(), ev.value());
            }
            other => panic!("esperava InputEvent, veio {other:?}"),
        }

        let mut raw = Vec::new();
        write_event(&mut raw, &ev).unwrap();
        let wire_encoded = encode(&WireMessage::InputEvent(ev)).unwrap();
        assert_eq!(&wire_encoded[1..], raw.as_slice());
    }

    #[test]
    fn clipboard_text_roundtrips() {
        match roundtrip(WireMessage::ClipboardText("olá, mundo".into())) {
            WireMessage::ClipboardText(text) => assert_eq!(text, "olá, mundo"),
            other => panic!("esperava ClipboardText, veio {other:?}"),
        }
    }

    #[test]
    fn clipboard_image_roundtrips_with_large_payload() {
        let bytes: Vec<u8> = (0..200_000u32).map(|i| (i % 256) as u8).collect();
        let msg = WireMessage::ClipboardImage {
            mime: "image/png".into(),
            bytes: bytes.clone(),
        };
        match roundtrip(msg) {
            WireMessage::ClipboardImage {
                mime,
                bytes: decoded,
            } => {
                assert_eq!(mime, "image/png");
                assert_eq!(decoded, bytes);
            }
            other => panic!("esperava ClipboardImage, veio {other:?}"),
        }
    }

    #[test]
    fn focus_handoff_roundtrips() {
        assert!(matches!(
            roundtrip(WireMessage::FocusHandoff),
            WireMessage::FocusHandoff
        ));
    }

    #[test]
    fn heartbeat_roundtrips() {
        assert!(matches!(
            roundtrip(WireMessage::Heartbeat),
            WireMessage::Heartbeat
        ));
    }
}
