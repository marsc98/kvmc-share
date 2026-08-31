//! Sessão Noise Protocol (`Noise_XXpsk3_25519_ChaChaPoly_BLAKE2s`) autenticada por PSK.
//!
//! Antes do handshake em si, o iniciador manda um preâmbulo em texto claro
//! `[u8 len][nome]` — necessário pro respondedor escolher a PSK certa entre
//! múltiplos peers escutando na mesma porta. O nome não é segredo: quem
//! autentica de fato é a PSK, mixada na 3ª mensagem do handshake (`psk3`).

use crate::read_exact_or_eof;
use anyhow::{Context, Result, anyhow};
use snow::{Builder, TransportState, params::NoiseParams};
use std::io::{Read, Write};
use std::sync::LazyLock;

static PARAMS: LazyLock<NoiseParams> =
    LazyLock::new(|| "Noise_XXpsk3_25519_ChaChaPoly_BLAKE2s".parse().unwrap());

/// Maior payload que cabe cifrado num único frame de transporte Noise
/// (limite de mensagem do protocolo é 65535 bytes, menos os 16 bytes de tag).
const MAX_PLAINTEXT_PER_FRAME: usize = 65519;

/// Canal autenticado e cifrado sobre um stream já conectado (`TcpStream`,
/// `UnixStream`, ou qualquer `Read + Write`).
pub struct EncryptedChannel<S> {
    stream: S,
    transport: TransportState,
}

/// Realiza o handshake como iniciador (quem discou a conexão): manda o
/// preâmbulo de identidade e conduz as 3 mensagens do `XXpsk3`.
pub fn handshake_as_initiator<S: Read + Write>(
    mut stream: S,
    local_name: &str,
    psk: &[u8; 32],
) -> Result<EncryptedChannel<S>> {
    write_preamble(&mut stream, local_name)?;

    let keypair = Builder::new(PARAMS.clone()).generate_keypair()?;
    let mut noise = Builder::new(PARAMS.clone())
        .local_private_key(&keypair.private)?
        .psk(3, psk)?
        .build_initiator()?;

    let mut buf = [0u8; 65535];
    let mut rbuf = [0u8; 65535];

    let n = noise.write_message(&[], &mut buf)?;
    write_frame(&mut stream, &buf[..n])?;

    let msg = read_frame(&mut stream)?;
    noise.read_message(&msg, &mut rbuf)?;

    let n = noise.write_message(&[], &mut buf)?;
    write_frame(&mut stream, &buf[..n])?;

    let transport = noise.into_transport_mode()?;
    Ok(EncryptedChannel { stream, transport })
}

/// Realiza o handshake como respondedor: lê o preâmbulo primeiro pra
/// resolver a PSK do peer que está discando, depois conduz o `XXpsk3`.
pub fn handshake_as_responder<S: Read + Write>(
    mut stream: S,
    resolve_psk: impl Fn(&str) -> Option<[u8; 32]>,
) -> Result<EncryptedChannel<S>> {
    let peer_name = read_preamble(&mut stream)?;
    let psk = resolve_psk(&peer_name).ok_or_else(|| anyhow!("peer desconhecido: {peer_name}"))?;

    let keypair = Builder::new(PARAMS.clone()).generate_keypair()?;
    let mut noise = Builder::new(PARAMS.clone())
        .local_private_key(&keypair.private)?
        .psk(3, &psk)?
        .build_responder()?;

    let mut buf = [0u8; 65535];
    let mut rbuf = [0u8; 65535];

    let msg = read_frame(&mut stream)?;
    noise.read_message(&msg, &mut rbuf)?;

    let n = noise.write_message(&[], &mut buf)?;
    write_frame(&mut stream, &buf[..n])?;

    let msg = read_frame(&mut stream)?;
    noise
        .read_message(&msg, &mut rbuf)
        .context("falha decifrando a 3a mensagem do handshake (PSK provavelmente divergente)")?;

    let transport = noise.into_transport_mode()?;
    Ok(EncryptedChannel { stream, transport })
}

impl<S: Read + Write> EncryptedChannel<S> {
    /// Cifra e envia `plaintext`, fragmentando em blocos de até
    /// `MAX_PLAINTEXT_PER_FRAME` bytes quando necessário. Um cabeçalho com o
    /// tamanho total (cifrado como o primeiro frame) permite ao lado
    /// receptor saber quantos blocos esperar.
    pub fn send(&mut self, plaintext: &[u8]) -> Result<()> {
        let total_len = (plaintext.len() as u32).to_be_bytes();
        let header = self.encrypt_chunk(&total_len)?;
        write_frame(&mut self.stream, &header)?;

        for chunk in plaintext.chunks(MAX_PLAINTEXT_PER_FRAME) {
            let ciphertext = self.encrypt_chunk(chunk)?;
            write_frame(&mut self.stream, &ciphertext)?;
        }
        Ok(())
    }

    /// Recebe e decifra a próxima mensagem completa. `Ok(None)` em EOF limpo
    /// (conexão encerrada entre mensagens).
    pub fn recv(&mut self) -> Result<Option<Vec<u8>>> {
        let Some(header) = read_frame_or_eof(&mut self.stream)? else {
            return Ok(None);
        };
        let total_len = u32::from_be_bytes(self.decrypt_chunk(&header)?[..4].try_into().unwrap());
        let total_len = total_len as usize;

        let mut out = Vec::with_capacity(total_len);
        while out.len() < total_len {
            let frame = read_frame(&mut self.stream)?;
            out.extend_from_slice(&self.decrypt_chunk(&frame)?);
        }
        Ok(Some(out))
    }

    fn encrypt_chunk(&mut self, chunk: &[u8]) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; chunk.len() + 16];
        let n = self
            .transport
            .write_message(chunk, &mut buf)
            .map_err(|e| anyhow!("falha cifrando frame: {e}"))?;
        buf.truncate(n);
        Ok(buf)
    }

    fn decrypt_chunk(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; ciphertext.len()];
        let n = self
            .transport
            .read_message(ciphertext, &mut buf)
            .map_err(|e| anyhow!("falha decifrando frame: {e}"))?;
        buf.truncate(n);
        Ok(buf)
    }
}

fn write_preamble(stream: &mut impl Write, name: &str) -> Result<()> {
    let bytes = name.as_bytes();
    stream.write_all(&[bytes.len() as u8])?;
    stream.write_all(bytes)?;
    Ok(())
}

fn read_preamble(stream: &mut impl Read) -> Result<String> {
    let mut len_buf = [0u8; 1];
    stream.read_exact(&mut len_buf)?;
    let mut buf = vec![0u8; len_buf[0] as usize];
    stream.read_exact(&mut buf)?;
    String::from_utf8(buf).context("preâmbulo de identidade inválido (utf-8)")
}

fn write_frame(stream: &mut impl Write, data: &[u8]) -> Result<()> {
    stream.write_all(&(data.len() as u32).to_be_bytes())?;
    stream.write_all(data)?;
    Ok(())
}

fn read_frame(stream: &mut impl Read) -> Result<Vec<u8>> {
    read_frame_or_eof(stream)?.ok_or_else(|| anyhow!("conexão encerrada antes do fim do handshake"))
}

fn read_frame_or_eof(stream: &mut impl Read) -> Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    if !read_exact_or_eof(stream, &mut len_buf)? {
        return Ok(None);
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    stream
        .read_exact(&mut buf)
        .context("conexão encerrada no meio de um frame")?;
    Ok(Some(buf))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;
    use std::thread;

    fn handshake_pair(
        psk_a: [u8; 32],
        psk_b: [u8; 32],
    ) -> (
        Result<EncryptedChannel<UnixStream>>,
        Result<EncryptedChannel<UnixStream>>,
    ) {
        let (initiator_sock, responder_sock) = UnixStream::pair().unwrap();

        let initiator_handle =
            thread::spawn(move || handshake_as_initiator(initiator_sock, "initiator", &psk_a));
        let responder_result = handshake_as_responder(responder_sock, move |name| {
            (name == "initiator").then_some(psk_b)
        });
        let initiator_result = initiator_handle.join().unwrap();

        (initiator_result, responder_result)
    }

    #[test]
    fn handshake_succeeds_with_matching_psk() {
        let psk = [7u8; 32];
        let (initiator, responder) = handshake_pair(psk, psk);
        assert!(initiator.is_ok());
        assert!(responder.is_ok());
    }

    #[test]
    fn handshake_fails_when_psks_diverge() {
        let (_initiator, responder) = handshake_pair([1u8; 32], [2u8; 32]);
        assert!(responder.is_err());
    }

    #[test]
    fn send_recv_roundtrip_small_payload() {
        let psk = [9u8; 32];
        let (initiator, responder) = handshake_pair(psk, psk);
        let mut initiator = initiator.unwrap();
        let mut responder = responder.unwrap();

        let handle = thread::spawn(move || {
            initiator.send(b"ola mundo cifrado").unwrap();
            initiator
        });
        let received = responder.recv().unwrap().unwrap();
        handle.join().unwrap();

        assert_eq!(received, b"ola mundo cifrado");
    }

    #[test]
    fn send_recv_roundtrip_payload_larger_than_single_noise_frame() {
        let psk = [3u8; 32];
        let (initiator, responder) = handshake_pair(psk, psk);
        let mut initiator = initiator.unwrap();
        let mut responder = responder.unwrap();

        let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 256) as u8).collect();
        let payload_clone = payload.clone();

        let handle = thread::spawn(move || {
            initiator.send(&payload_clone).unwrap();
            initiator
        });
        let received = responder.recv().unwrap().unwrap();
        handle.join().unwrap();

        assert_eq!(received, payload);
    }

    #[test]
    fn preamble_lets_responder_pick_psk_among_multiple_candidates() {
        let psk_laptop = [11u8; 32];
        let psk_desktop = [22u8; 32];

        let (initiator_sock, responder_sock) = UnixStream::pair().unwrap();
        let initiator_handle =
            thread::spawn(move || handshake_as_initiator(initiator_sock, "laptop", &psk_laptop));
        let responder = handshake_as_responder(responder_sock, move |name| match name {
            "laptop" => Some(psk_laptop),
            "desktop" => Some(psk_desktop),
            _ => None,
        });
        let initiator = initiator_handle.join().unwrap();

        assert!(initiator.is_ok());
        assert!(responder.is_ok());
    }
}
