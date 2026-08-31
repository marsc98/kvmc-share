//! Parsing e validação de `~/.config/kvm-share/peers.toml`: config local da
//! máquina e lista de peers da malha (endereço, PSK, direção de tela).

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct LocalConfig {
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub listen: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PeerConfig {
    pub name: String,
    pub addr: SocketAddr,
    pub psk_path: PathBuf,
    pub direction: Direction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Deserialize)]
struct RawConfig {
    local: LocalConfig,
    #[serde(rename = "peer", default)]
    peers: Vec<RawPeerConfig>,
}

#[derive(Deserialize)]
struct RawPeerConfig {
    name: String,
    addr: SocketAddr,
    psk_path: PathBuf,
    direction: Direction,
}

/// Expande um `~` inicial usando a variável de ambiente `HOME`. Caminhos que
/// não começam com `~` são retornados como estão.
pub fn expand_home(path: &Path) -> Result<PathBuf> {
    let Ok(rest) = path.strip_prefix("~") else {
        return Ok(path.to_path_buf());
    };
    let home = std::env::var("HOME").context("variável de ambiente HOME não definida")?;
    Ok(PathBuf::from(home).join(rest))
}

/// Parseia o TOML e resolve/valida os `psk_path` de cada peer contra o
/// disco. Separado de `load` para permitir teste sem depender de `HOME`
/// real ou de arquivos em `~/.config`.
fn parse_and_validate(toml_str: &str) -> Result<(LocalConfig, Vec<PeerConfig>)> {
    let raw: RawConfig = toml::from_str(toml_str).context("peers.toml malformado")?;

    let peers = raw
        .peers
        .into_iter()
        .map(|p| {
            let psk_path = expand_home(&p.psk_path)?;
            if !psk_path.exists() {
                bail!(
                    "psk_path do peer '{}' não existe: {}",
                    p.name,
                    psk_path.display()
                );
            }
            Ok(PeerConfig {
                name: p.name,
                addr: p.addr,
                psk_path,
                direction: p.direction,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    Ok((raw.local, peers))
}

/// Lê e valida `~/.config/kvm-share/peers.toml`.
pub fn load() -> Result<(LocalConfig, Vec<PeerConfig>)> {
    let home = std::env::var("HOME").context("variável de ambiente HOME não definida")?;
    let path = PathBuf::from(home).join(".config/kvm-share/peers.toml");
    let toml_str = std::fs::read_to_string(&path)
        .with_context(|| format!("falha ao ler {}", path.display()))?;
    parse_and_validate(&toml_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, b"psk-bytes").unwrap();
        path
    }

    #[test]
    fn parses_valid_toml_with_multiple_peers() {
        let dir = std::env::temp_dir().join("kvm-share-test-valid");
        std::fs::create_dir_all(&dir).unwrap();
        let laptop_psk = touch(&dir, "laptop.psk");
        let tablet_psk = touch(&dir, "tablet.psk");

        let toml_str = format!(
            r#"
[local]
name = "desktop"
width = 2560
height = 1440
listen = "0.0.0.0:7532"

[[peer]]
name = "laptop"
addr = "192.168.1.50:7532"
psk_path = "{}"
direction = "right"

[[peer]]
name = "tablet"
addr = "192.168.1.51:7532"
psk_path = "{}"
direction = "left"
"#,
            laptop_psk.display(),
            tablet_psk.display()
        );

        let (local, peers) = parse_and_validate(&toml_str).unwrap();
        assert_eq!(local.name, "desktop");
        assert_eq!(local.width, 2560);
        assert_eq!(local.listen, "0.0.0.0:7532");
        assert_eq!(peers.len(), 2);
        assert_eq!(peers[0].name, "laptop");
        assert_eq!(peers[0].direction, Direction::Right);
        assert_eq!(peers[1].direction, Direction::Left);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn errors_on_malformed_toml() {
        let err = parse_and_validate("this is not [ valid toml").unwrap_err();
        assert!(err.to_string().contains("malformado"));
    }

    #[test]
    fn errors_when_psk_path_missing() {
        let toml_str = r#"
[local]
name = "desktop"
width = 2560
height = 1440
listen = "0.0.0.0:7532"

[[peer]]
name = "laptop"
addr = "192.168.1.50:7532"
psk_path = "/nonexistent/path/laptop.psk"
direction = "right"
"#;
        let err = parse_and_validate(toml_str).unwrap_err();
        assert!(err.to_string().contains("psk_path"));
    }
}
