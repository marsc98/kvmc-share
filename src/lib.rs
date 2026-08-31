//! kvm-share: compartilhamento de mouse/teclado entre duas máquinas Linux
//! contornando a ausência do portal `org.freedesktop.portal.InputCapture`
//! em compositores Wayland que ainda não o implementam (ex: COSMIC/Pop!_OS).
//!
//! A ideia: em vez de depender de qualquer protocolo do Wayland, o `capture`
//! lê eventos brutos direto de `/dev/input/eventX` (evdev) e o `inject` os
//! recria como um dispositivo "de verdade" via `/dev/uinput`. Isso funciona
//! independente do compositor, porque atua uma camada abaixo do Wayland.
//!
//! Protocolo de rede: cada evento evdev vira um frame binário de 8 bytes,
//! little-endian: `[type: u16][code: u16][value: i32]`. Sem dependências
//! extras (serde/bincode) — é só o `input_event` do kernel sem o timestamp.
//!
//! IMPORTANTE: isto é um protótipo funcional, não um produto endurecido.
//! Rode apenas em rede confiável (idealmente dentro de uma VPN como
//! WireGuard/Tailscale) — o protocolo aqui não criptografa nem autentica o
//! tráfego. Veja o README para detalhes de permissões e segurança.

pub mod cursor;

use anyhow::{Context, Result, bail};
use evdev::{InputEvent, KeyCode};
use std::io::{Read, Write};

pub mod config;

pub mod devices;

/// Tecla usada para alternar entre "controle local" e "encaminhar para a
/// outra máquina". Scroll Lock foi escolhida por ser praticamente inutilizada
/// hoje em dia. Pressione e solte para alternar (não precisa segurar).
pub const TOGGLE_KEY: KeyCode = KeyCode::KEY_SCROLLLOCK;

/// Tamanho fixo de cada frame no protocolo de rede.
pub const FRAME_LEN: usize = 8;

/// Codifica um `InputEvent` no formato de fio (8 bytes) e escreve no stream.
pub fn write_event(stream: &mut impl Write, ev: &InputEvent) -> Result<()> {
    let mut buf = [0u8; FRAME_LEN];
    buf[0..2].copy_from_slice(&ev.event_type().0.to_le_bytes());
    buf[2..4].copy_from_slice(&ev.code().to_le_bytes());
    buf[4..8].copy_from_slice(&ev.value().to_le_bytes());
    stream
        .write_all(&buf)
        .context("falha ao escrever evento no socket")?;
    Ok(())
}

/// Lê um frame de 8 bytes do stream e reconstrói o `InputEvent`.
/// Retorna `Ok(None)` em EOF limpo (conexão encerrada entre frames).
pub fn read_event(stream: &mut impl Read) -> Result<Option<InputEvent>> {
    let mut buf = [0u8; FRAME_LEN];
    match read_exact_or_eof(stream, &mut buf)? {
        false => Ok(None),
        true => {
            let type_ = u16::from_le_bytes([buf[0], buf[1]]);
            let code = u16::from_le_bytes([buf[2], buf[3]]);
            let value = i32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
            Ok(Some(InputEvent::new(type_, code, value)))
        }
    }
}

/// Como `Read::read_exact`, mas trata EOF-antes-do-primeiro-byte como fim de
/// stream normal (retorna `Ok(false)`) em vez de erro, e qualquer EOF no meio
/// de um frame como erro de fato (conexão caiu de forma inesperada).
fn read_exact_or_eof(stream: &mut impl Read, buf: &mut [u8]) -> Result<bool> {
    let mut read = 0;
    while read < buf.len() {
        match stream.read(&mut buf[read..]) {
            Ok(0) => {
                if read == 0 {
                    return Ok(false);
                }
                bail!(
                    "conexão encerrada no meio de um frame ({read}/{} bytes)",
                    buf.len()
                );
            }
            Ok(n) => read += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e).context("falha ao ler evento do socket"),
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use evdev::EventType;
    use std::io::Cursor;

    #[test]
    fn roundtrip_single_event() {
        let ev = InputEvent::new(EventType::KEY.0, TOGGLE_KEY.code(), 1);
        let mut buf = Vec::new();
        write_event(&mut buf, &ev).unwrap();
        assert_eq!(buf.len(), FRAME_LEN);

        let mut cursor = Cursor::new(buf);
        let decoded = read_event(&mut cursor)
            .unwrap()
            .expect("evento decodificado");
        assert_eq!(decoded.event_type(), ev.event_type());
        assert_eq!(decoded.code(), ev.code());
        assert_eq!(decoded.value(), ev.value());
    }

    #[test]
    fn roundtrip_multiple_events_in_sequence() {
        let events = vec![
            InputEvent::new(EventType::RELATIVE.0, 0x00 /* REL_X */, 5),
            InputEvent::new(EventType::RELATIVE.0, 0x01 /* REL_Y */, -3),
            InputEvent::new(EventType::SYNCHRONIZATION.0, 0, 0),
        ];
        let mut buf = Vec::new();
        for ev in &events {
            write_event(&mut buf, ev).unwrap();
        }

        let mut cursor = Cursor::new(buf);
        for original in &events {
            let decoded = read_event(&mut cursor)
                .unwrap()
                .expect("evento decodificado");
            assert_eq!(decoded.event_type(), original.event_type());
            assert_eq!(decoded.code(), original.code());
            assert_eq!(decoded.value(), original.value());
        }
        // stream esgotado -> EOF limpo, não erro
        assert!(read_event(&mut cursor).unwrap().is_none());
    }

    #[test]
    fn eof_in_middle_of_frame_is_an_error() {
        let mut buf = Vec::new();
        write_event(&mut buf, &InputEvent::new(EventType::KEY.0, 1, 1)).unwrap();
        buf.truncate(FRAME_LEN - 1); // corta o último byte do frame
        let mut cursor = Cursor::new(buf);
        assert!(read_event(&mut cursor).is_err());
    }
}
