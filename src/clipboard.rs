//! Ponte com o daemon `copied` (gerenciador de área de transferência) via seu
//! socket Unix (`copied_core::socket_path()`), usando o protocolo linha-JSON
//! por conexão (`Command`/`Response` de `copied-core`).

use anyhow::{Context, Result, bail};
use copied_core::{Command, Response};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

/// Timeout de leitura/escrita no socket: nunca travar indefinidamente se o
/// daemon estiver num estado ruim (pendurado, sem responder etc).
const SOCKET_TIMEOUT: Duration = Duration::from_millis(500);

/// Limite de payload enviado ao daemon: recusa antes mesmo de tentar
/// conectar, evitando prender a conexão local com uma imagem gigante.
const MAX_CLIPBOARD_BYTES: usize = 5 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardContent {
    Text(String),
    Image { mime: String, bytes: Vec<u8> },
}

/// Verifica só a existência do socket — não tenta conectar (evita travar em
/// contextos onde `is_available` precisa ser rápido, ex: barra de status).
pub fn is_available() -> bool {
    copied_core::socket_path().is_ok_and(|path| path.exists())
}

pub fn read_latest() -> Result<Option<ClipboardContent>> {
    let path = copied_core::socket_path().context("XDG_RUNTIME_DIR não setada")?;
    read_latest_at(&path)
}

pub fn write(content: &ClipboardContent) -> Result<()> {
    let path = copied_core::socket_path().context("XDG_RUNTIME_DIR não setada")?;
    write_at(&path, content)
}

/// Qualquer erro de I/O, timeout ou JSON malformado vira "indisponível" —
/// nunca panic, o daemon `copied` é um componente opcional do sistema.
fn read_latest_at(socket: &Path) -> Result<Option<ClipboardContent>> {
    let Ok(text_response) = request(socket, &Command::GetLatestText) else {
        return Ok(None);
    };
    match text_response {
        Response::Text(text) => Ok(Some(ClipboardContent::Text(text))),
        Response::Error { .. } => match request(socket, &Command::GetLatestImageBytes) {
            Ok(Response::ImageBytes { mime, data_base64 }) => {
                let bytes = base64_decode(&data_base64)?;
                Ok(Some(ClipboardContent::Image { mime, bytes }))
            }
            _ => Ok(None),
        },
        _ => Ok(None),
    }
}

fn write_at(socket: &Path, content: &ClipboardContent) -> Result<()> {
    let text = match content {
        ClipboardContent::Text(text) => text,
        // `copied-core` ainda não expõe um comando de escrita de imagem no
        // clipboard via socket (só leitura, `GetLatestImageBytes`).
        ClipboardContent::Image { .. } => {
            bail!("escrita de imagem não é suportada pelo daemon copied atual")
        }
    };
    if text.len() > MAX_CLIPBOARD_BYTES {
        bail!(
            "payload de {} bytes excede o limite de {MAX_CLIPBOARD_BYTES} bytes",
            text.len()
        );
    }
    match request(socket, &Command::CopyText { text: text.clone() })? {
        Response::Ack => Ok(()),
        Response::Error { message } => bail!("daemon copied recusou o comando: {message}"),
        other => bail!("resposta inesperada do daemon copied: {other:?}"),
    }
}

fn request(socket: &Path, command: &Command) -> Result<Response> {
    let mut stream = UnixStream::connect(socket).context("falha ao conectar no socket copied")?;
    stream.set_read_timeout(Some(SOCKET_TIMEOUT))?;
    stream.set_write_timeout(Some(SOCKET_TIMEOUT))?;

    let mut line = serde_json::to_string(command).context("falha ao serializar comando")?;
    line.push('\n');
    stream
        .write_all(line.as_bytes())
        .context("falha ao escrever no socket copied")?;

    let mut response = String::new();
    BufReader::new(stream)
        .read_line(&mut response)
        .context("falha ao ler resposta do socket copied")?;
    serde_json::from_str(&response).context("resposta malformada do daemon copied")
}

fn base64_decode(data: &str) -> Result<Vec<u8>> {
    use base64_lite::decode;
    decode(data).context("base64 inválido na resposta do daemon copied")
}

/// Decodificador base64 minimalista (só o alfabeto padrão + `=` de padding),
/// pra não puxar uma dependência nova só pra decodificar `data_base64`.
mod base64_lite {
    use anyhow::{Result, bail};

    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn decode(input: &str) -> Result<Vec<u8>> {
        let input = input.trim_end_matches('=');
        let mut out = Vec::with_capacity(input.len() * 3 / 4);
        let mut buf = 0u32;
        let mut bits = 0u32;
        for c in input.bytes() {
            let value = ALPHABET
                .iter()
                .position(|&b| b == c)
                .ok_or_else(|| anyhow::anyhow!("caractere base64 inválido: {}", c as char))?;
            buf = (buf << 6) | value as u32;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push((buf >> bits) as u8);
            }
        }
        if bits >= 6 {
            bail!("base64 truncado");
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::thread;

    struct TestSocket(PathBuf);

    impl TestSocket {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "kvm-share-test-{}-{name}-{}.sock",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            Self(path)
        }
    }

    impl Drop for TestSocket {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// Sobe um listener que responde uma única conexão com `response`,
    /// simulando o `copied-daemon` (framing: linha JSON + `\n`).
    fn spawn_mock_daemon(path: PathBuf, response: Response) {
        let listener = UnixListener::bind(path).expect("bind mock socket");
        thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut line = String::new();
            reader.read_line(&mut line).expect("read command");

            let mut writer = stream;
            let json = serde_json::to_string(&response).expect("serialize response");
            writer.write_all(json.as_bytes()).expect("write response");
            writer.write_all(b"\n").expect("write newline");
        });
    }

    #[test]
    fn is_available_false_when_socket_missing() {
        let socket = TestSocket::new("missing");
        assert!(!socket.0.exists());
    }

    #[test]
    fn read_latest_roundtrips_text() {
        let socket = TestSocket::new("read-text");
        spawn_mock_daemon(
            socket.0.clone(),
            Response::Text("olá clipboard".to_string()),
        );

        let content = read_latest_at(&socket.0).unwrap();
        assert_eq!(
            content,
            Some(ClipboardContent::Text("olá clipboard".to_string()))
        );
    }

    #[test]
    fn read_latest_roundtrips_image() {
        let socket = TestSocket::new("read-image");
        // primeira tentativa (GetLatestText) falha -> cai pro fallback de imagem
        let listener = UnixListener::bind(&socket.0).expect("bind");
        thread::spawn(move || {
            for _ in 0..2 {
                let (stream, _) = listener.accept().expect("accept");
                let mut reader = BufReader::new(stream.try_clone().expect("clone"));
                let mut line = String::new();
                reader.read_line(&mut line).expect("read command");

                let command: Command = serde_json::from_str(line.trim()).expect("parse command");
                let response = match command {
                    Command::GetLatestText => Response::Error {
                        message: "sem texto".into(),
                    },
                    Command::GetLatestImageBytes => Response::ImageBytes {
                        mime: "image/png".into(),
                        // "hi" em base64
                        data_base64: "aGk=".into(),
                    },
                    _ => unreachable!(),
                };

                let mut writer = stream;
                let json = serde_json::to_string(&response).expect("serialize");
                writer.write_all(json.as_bytes()).expect("write");
                writer.write_all(b"\n").expect("write newline");
            }
        });

        let content = read_latest_at(&socket.0).unwrap();
        assert_eq!(
            content,
            Some(ClipboardContent::Image {
                mime: "image/png".to_string(),
                bytes: b"hi".to_vec(),
            })
        );
    }

    #[test]
    fn read_latest_none_when_socket_missing() {
        let socket = TestSocket::new("nonexistent");
        let content = read_latest_at(&socket.0).unwrap();
        assert_eq!(content, None);
    }

    #[test]
    fn write_text_roundtrips() {
        let socket = TestSocket::new("write-text");
        spawn_mock_daemon(socket.0.clone(), Response::Ack);

        let result = write_at(&socket.0, &ClipboardContent::Text("copiado".to_string()));
        assert!(result.is_ok());
    }

    #[test]
    fn write_image_is_unsupported() {
        let socket = TestSocket::new("write-image");
        let result = write_at(
            &socket.0,
            &ClipboardContent::Image {
                mime: "image/png".to_string(),
                bytes: vec![1, 2, 3],
            },
        );
        assert!(result.is_err());
    }

    #[test]
    fn write_rejects_oversized_payload_without_connecting() {
        let socket = TestSocket::new("write-oversized");
        let huge = "a".repeat(MAX_CLIPBOARD_BYTES + 1);

        let result = write_at(&socket.0, &ClipboardContent::Text(huge));

        assert!(result.is_err());
        assert!(
            !socket.0.exists(),
            "não deveria ter tentado conectar/bindar nada"
        );
    }

    #[test]
    fn write_error_response_is_propagated() {
        let socket = TestSocket::new("write-error");
        spawn_mock_daemon(
            socket.0.clone(),
            Response::Error {
                message: "algo deu errado".into(),
            },
        );

        let result = write_at(&socket.0, &ClipboardContent::Text("x".to_string()));
        assert!(result.is_err());
    }
}
