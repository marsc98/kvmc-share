//! Parsing e validação de `~/.config/kvmc-share/peers.toml`: config local da
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

impl Direction {
    fn as_str(self) -> &'static str {
        match self {
            Direction::Left => "left",
            Direction::Right => "right",
            Direction::Up => "up",
            Direction::Down => "down",
        }
    }
}

impl std::str::FromStr for Direction {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "left" => Ok(Direction::Left),
            "right" => Ok(Direction::Right),
            "up" => Ok(Direction::Up),
            "down" => Ok(Direction::Down),
            other => bail!("direção inválida: '{other}' (esperado left, right, up ou down)"),
        }
    }
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

/// Retorna `~/.config/kvmc-share/peers.toml` resolvido.
pub fn default_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("variável de ambiente HOME não definida")?;
    Ok(PathBuf::from(home).join(".config/kvmc-share/peers.toml"))
}

/// Lê e valida `~/.config/kvmc-share/peers.toml`.
pub fn load() -> Result<(LocalConfig, Vec<PeerConfig>)> {
    let path = default_path()?;
    let toml_str = std::fs::read_to_string(&path)
        .with_context(|| format!("falha ao ler {}", path.display()))?;
    parse_and_validate(&toml_str)
}

/// Inverso de `expand_home`: se `path` está sob `$HOME`, devolve a notação
/// `~/...`; caso contrário devolve o path absoluto como está.
pub fn contract_home(path: &Path) -> String {
    let Ok(home) = std::env::var("HOME") else {
        return path.display().to_string();
    };
    match path.strip_prefix(&home) {
        Ok(rest) => PathBuf::from("~").join(rest).display().to_string(),
        Err(_) => path.display().to_string(),
    }
}

/// Path convencional da PSK de um peer: `~/.config/kvmc-share/peers/<peer_name>.psk` —
/// mesma lógica hoje inline em `keygen()` (`kvmc-share.rs:80`).
pub fn default_psk_path(peer_name: &str) -> PathBuf {
    PathBuf::from(".config/kvmc-share/peers").join(format!("{peer_name}.psk"))
}

/// Regenera `peers.toml` inteiro a partir de `local`/`peers` (mesmo formato
/// produzido por `write_peers_toml` no `setup.sh`) — sem edição incremental,
/// já que o arquivo não tem comentários/formatação livre a preservar.
pub fn save(path: &Path, local: &LocalConfig, peers: &[PeerConfig]) -> Result<()> {
    let mut out = format!(
        "[local]\nname = \"{}\"\nwidth = {}\nheight = {}\nlisten = \"{}\"\n",
        local.name, local.width, local.height, local.listen
    );
    for peer in peers {
        out.push_str(&format!(
            "\n[[peer]]\nname = \"{}\"\naddr = \"{}\"\npsk_path = \"{}\"\ndirection = \"{}\"\n",
            peer.name,
            peer.addr,
            contract_home(&peer.psk_path),
            peer.direction.as_str()
        ));
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("falha ao criar diretório {}", parent.display()))?;
    }
    std::fs::write(path, out).with_context(|| format!("falha ao gravar {}", path.display()))
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
        let dir = std::env::temp_dir().join("kvmc-share-test-valid");
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

    #[test]
    fn contract_home_replaces_home_prefix_with_tilde() {
        // SAFETY: teste single-threaded pra variável de ambiente; sem concorrência com outros testes que leem HOME.
        unsafe { std::env::set_var("HOME", "/home/marco") };
        let path = Path::new("/home/marco/.config/kvmc-share/peers/laptop.psk");
        assert_eq!(
            contract_home(path),
            "~/.config/kvmc-share/peers/laptop.psk"
        );
    }

    #[test]
    fn contract_home_keeps_paths_outside_home_absolute() {
        unsafe { std::env::set_var("HOME", "/home/marco") };
        let path = Path::new("/etc/kvmc-share/peers/laptop.psk");
        assert_eq!(
            contract_home(path),
            "/etc/kvmc-share/peers/laptop.psk"
        );
    }

    #[test]
    fn default_psk_path_matches_keygen_convention() {
        assert_eq!(
            default_psk_path("laptop"),
            PathBuf::from(".config/kvmc-share/peers/laptop.psk")
        );
    }

    #[test]
    fn direction_from_str_accepts_all_four_values() {
        assert_eq!("left".parse::<Direction>().unwrap(), Direction::Left);
        assert_eq!("right".parse::<Direction>().unwrap(), Direction::Right);
        assert_eq!("up".parse::<Direction>().unwrap(), Direction::Up);
        assert_eq!("down".parse::<Direction>().unwrap(), Direction::Down);
    }

    #[test]
    fn direction_from_str_rejects_invalid_value_with_clear_message() {
        let err = "diagonal".parse::<Direction>().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("left"));
        assert!(msg.contains("right"));
        assert!(msg.contains("up"));
        assert!(msg.contains("down"));
    }

    #[test]
    fn default_path_resolves_under_home() {
        unsafe { std::env::set_var("HOME", "/home/marco") };
        assert_eq!(
            default_path().unwrap(),
            PathBuf::from("/home/marco/.config/kvmc-share/peers.toml")
        );
    }

    #[test]
    fn save_then_load_round_trips_with_multiple_peers() {
        let dir = std::env::temp_dir().join("kvmc-share-test-save-roundtrip");
        std::fs::create_dir_all(&dir).unwrap();
        let laptop_psk = touch(&dir, "laptop.psk");
        let tablet_psk = touch(&dir, "tablet.psk");

        let local = LocalConfig {
            name: "desktop".into(),
            width: 2560,
            height: 1440,
            listen: "0.0.0.0:7532".into(),
        };
        let peers = vec![
            PeerConfig {
                name: "laptop".into(),
                addr: "192.168.1.50:7532".parse().unwrap(),
                psk_path: laptop_psk,
                direction: Direction::Right,
            },
            PeerConfig {
                name: "tablet".into(),
                addr: "192.168.1.51:7532".parse().unwrap(),
                psk_path: tablet_psk,
                direction: Direction::Left,
            },
        ];

        let path = dir.join("peers.toml");
        save(&path, &local, &peers).unwrap();
        let toml_str = std::fs::read_to_string(&path).unwrap();
        let (loaded_local, loaded_peers) = parse_and_validate(&toml_str).unwrap();

        assert_eq!(loaded_local, local);
        assert_eq!(loaded_peers, peers);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_with_no_peers_round_trips() {
        let dir = std::env::temp_dir().join("kvmc-share-test-save-no-peers");
        std::fs::create_dir_all(&dir).unwrap();

        let local = LocalConfig {
            name: "desktop".into(),
            width: 1920,
            height: 1080,
            listen: "0.0.0.0:7532".into(),
        };

        let path = dir.join("peers.toml");
        save(&path, &local, &[]).unwrap();
        let toml_str = std::fs::read_to_string(&path).unwrap();
        let (loaded_local, loaded_peers) = parse_and_validate(&toml_str).unwrap();

        assert_eq!(loaded_local, local);
        assert!(loaded_peers.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_writes_psk_path_with_tilde_notation_under_home() {
        unsafe { std::env::set_var("HOME", "/home/marco") };
        let dir = std::env::temp_dir().join("kvmc-share-test-save-tilde");
        std::fs::create_dir_all(&dir).unwrap();

        let local = LocalConfig {
            name: "desktop".into(),
            width: 1920,
            height: 1080,
            listen: "0.0.0.0:7532".into(),
        };
        let peers = vec![PeerConfig {
            name: "laptop".into(),
            addr: "192.168.1.50:7532".parse().unwrap(),
            psk_path: PathBuf::from("/home/marco/.config/kvmc-share/peers/laptop.psk"),
            direction: Direction::Right,
        }];

        let path = dir.join("peers.toml");
        save(&path, &local, &peers).unwrap();
        let toml_str = std::fs::read_to_string(&path).unwrap();

        assert!(toml_str.contains(r#"psk_path = "~/.config/kvmc-share/peers/laptop.psk""#));

        std::fs::remove_dir_all(&dir).ok();
    }
}
