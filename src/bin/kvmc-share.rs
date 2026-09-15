//! `kvmc-share run`: daemon único de malha — substitui os binários separados
//! `capture`/`inject` (T12 vai removê-los). Carrega `peers.toml`, conecta
//! com cada peer via Noise (dial pro peer com nome lexicograficamente maior
//! que o local, escuta pros demais, evitando dois lados discarem ao mesmo
//! tempo), abre os dispositivos locais e roda o loop de despacho pra
//! `focus::Focus`.

use anyhow::{Context, Result, bail};
use evdev::uinput::VirtualDevice;
use evdev::{EventType, InputEvent};
use kvmc_share::config::{Direction, LocalConfig, PeerConfig, default_path, default_psk_path, expand_home};
use kvmc_share::focus::{Focus, FocusState, LocalInjector, PeerId, PeerSender};
use kvmc_share::noise::{EncryptedChannel, handshake_as_initiator, handshake_as_responder};
use kvmc_share::wire::{self, WireMessage};
use kvmc_share::{TOGGLE_KEY, devices};
use std::collections::HashMap;
use std::io::Read;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Nomes dos dispositivos evdev locais a capturar. `peers.toml` (T2) ainda
/// não expõe esse campo, então lemos de uma variável de ambiente
/// (`caminho1:caminho2:...`, no estilo de `PATH`) até essa lacuna ser
/// fechada numa task futura de config.
const DEVICE_PATHS_ENV: &str = "KVMC_SHARE_DEVICES";

const HANDSHAKE_AND_IDLE_READ_TIMEOUT: Duration = Duration::from_millis(500);

enum Event {
    Local(InputEvent),
    Wire(PeerId, WireMessage),
    PeerDisconnected(PeerId),
}

fn main() -> Result<()> {
    match std::env::args().nth(1).as_deref() {
        None => run(vec![]),
        Some("run") => run(std::env::args().skip(2).collect()),
        Some("keygen") => keygen(std::env::args().skip(2).collect()),
        _ => {
            eprintln!("uso: kvmc-share [run [--to nome1,nome2]|keygen <peer-name> <ip>]");
            std::process::exit(1);
        }
    }
}

/// Gera 32 bytes aleatórios de `/dev/urandom` e grava em `path` com
/// permissão `0600`. Sem rede nem stdin, pra ser testável isoladamente.
fn generate_and_save_psk(path: &Path) -> Result<()> {
    let mut psk = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .context("falha ao abrir /dev/urandom")?
        .read_exact(&mut psk)
        .context("falha ao ler bytes aleatórios de /dev/urandom")?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("falha ao criar diretório {}", parent.display()))?;
    }
    std::fs::write(path, psk).with_context(|| format!("falha ao gravar {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("falha ao definir permissão de {}", path.display()))?;
    Ok(())
}

/// `kvmc-share keygen <peer-name> <ip>`: gera a PSK compartilhada com um
/// peer e tenta distribuí-la via `scp` pro mesmo path relativo no destino.
/// `<ip>` aceita tanto um host puro (assume que SSH config/agent resolve o
/// usuário) quanto `user@host`, já que ambos são repassados como estão pro
/// `scp`.
fn keygen(args: Vec<String>) -> Result<()> {
    let [peer_name, host] = args.as_slice() else {
        bail!("uso: kvmc-share keygen <peer-name> <ip>");
    };

    let relative_path = PathBuf::from(".config/kvmc-share/peers").join(format!("{peer_name}.psk"));
    let path = expand_home(&Path::new("~").join(&relative_path))?;

    if path.exists() && !confirm_overwrite(&path)? {
        println!("cancelado, PSK existente mantida");
        return Ok(());
    }

    generate_and_save_psk(&path)?;
    println!("PSK gerada em {}", path.display());

    let remote = format!("{host}:{}", relative_path.display());
    let status = std::process::Command::new("scp")
        .arg(&path)
        .arg(&remote)
        .status();

    match status {
        Ok(s) if s.success() => println!("PSK copiada com sucesso para {remote}"),
        _ => {
            println!("não consegui copiar a PSK via scp automaticamente.");
            println!("copie manualmente com:");
            println!(
                "  ssh {host} 'mkdir -p ~/.config/kvmc-share/peers' && scp {} {remote}",
                path.display()
            );
        }
    }

    Ok(())
}

/// Imprime `prompt`, lê uma linha de `reader` e devolve `true` pra
/// "s"/"S"/"y"/"Y" — qualquer outra coisa (incluindo vazio) é `false`.
/// Recebe o reader como parâmetro pra ser testável sem stdin real.
fn confirm(reader: &mut impl std::io::BufRead, prompt: &str) -> Result<bool> {
    print!("{prompt}");
    std::io::Write::flush(&mut std::io::stdout()).ok();
    let mut answer = String::new();
    reader
        .read_line(&mut answer)
        .context("falha ao ler resposta de stdin")?;
    let answer = answer.trim().to_lowercase();
    Ok(answer == "s" || answer == "y")
}

fn confirm_overwrite(path: &Path) -> Result<bool> {
    confirm(
        &mut std::io::stdin().lock(),
        &format!("PSK já existe em {} — sobrescrever? (s/N): ", path.display()),
    )
}

/// Extrai e remove `--flag valor` de `args`, na primeira ocorrência.
/// `None` (sem alterar `args`) se a flag não aparecer.
fn take_flag(args: &mut Vec<String>, flag: &str) -> Option<String> {
    let idx = args.iter().position(|a| a == flag)?;
    if idx + 1 >= args.len() {
        return None;
    }
    args.remove(idx);
    Some(args.remove(idx))
}

/// Formata a lista de peers pra saída textual: uma linha por peer com
/// nome, addr e direção. Lista vazia produz uma mensagem explícita.
fn format_peer_list(peers: &[PeerConfig]) -> String {
    if peers.is_empty() {
        return "nenhum peer cadastrado (rode 'kvmc-share peer add')".to_string();
    }
    peers
        .iter()
        .map(|p| format!("{}\t{}\t{}", p.name, p.addr, p.direction))
        .collect::<Vec<_>>()
        .join("\n")
}

fn cmd_peer_list() -> Result<()> {
    let (_local, peers) = kvmc_share::config::load()?;
    println!("{}", format_peer_list(&peers));
    Ok(())
}

/// Monta um `PeerConfig` novo a partir de strings brutas de CLI (pura,
/// sem tocar disco): parseia `addr`/`direction`, deriva `psk_path` pela
/// convenção padrão ou usa `psk_path_override` quando informado.
fn build_new_peer(
    name: &str,
    addr: &str,
    direction: &str,
    psk_path_override: Option<PathBuf>,
) -> Result<PeerConfig> {
    let addr: SocketAddr = addr
        .parse()
        .with_context(|| format!("--addr inválido: '{addr}'"))?;
    let direction: Direction = direction.parse()?;
    let psk_path = match psk_path_override {
        Some(p) => p,
        None => expand_home(&Path::new("~").join(default_psk_path(name)))?,
    };
    Ok(PeerConfig {
        name: name.to_string(),
        addr,
        psk_path,
        direction,
    })
}

/// Insere `peer` em `peers` se o nome ainda não existir (pura, sem tocar disco).
fn add_peer_to_list(peers: &mut Vec<PeerConfig>, peer: PeerConfig) -> Result<()> {
    if peers.iter().any(|p| p.name == peer.name) {
        bail!("peer '{}' já cadastrado", peer.name);
    }
    peers.push(peer);
    Ok(())
}

/// `kvmc-share peer add <nome> --addr <ip:porta> --direction <dir> [--psk-path <caminho>]`.
fn cmd_peer_add(mut args: Vec<String>) -> Result<()> {
    let addr = take_flag(&mut args, "--addr")
        .context("uso: kvmc-share peer add <nome> --addr <ip:porta> --direction <dir>")?;
    let direction = take_flag(&mut args, "--direction").context("--direction é obrigatório")?;
    let psk_path_override = take_flag(&mut args, "--psk-path").map(PathBuf::from);
    let name = args
        .into_iter()
        .next()
        .context("uso: kvmc-share peer add <nome> --addr <ip:porta> --direction <dir>")?;

    let (local, mut peers) = kvmc_share::config::load()?;
    let peer = build_new_peer(&name, &addr, &direction, psk_path_override)?;
    add_peer_to_list(&mut peers, peer)?;
    kvmc_share::config::save(&default_path()?, &local, &peers)?;
    println!("peer '{name}' adicionado");
    Ok(())
}

/// Aplica os campos informados (`Some`) a um peer existente, preservando os
/// demais (pura, sem tocar disco).
fn apply_peer_edit(
    peers: &mut [PeerConfig],
    name: &str,
    addr: Option<SocketAddr>,
    direction: Option<Direction>,
) -> Result<()> {
    let peer = peers
        .iter_mut()
        .find(|p| p.name == name)
        .with_context(|| format!("peer '{name}' não encontrado"))?;
    if let Some(addr) = addr {
        peer.addr = addr;
    }
    if let Some(direction) = direction {
        peer.direction = direction;
    }
    Ok(())
}

/// `kvmc-share peer edit <nome> [--addr ...] [--direction ...]`. Sem flags,
/// mostra os valores atuais e sai sem escrever.
fn cmd_peer_edit(mut args: Vec<String>) -> Result<()> {
    let addr_flag = take_flag(&mut args, "--addr");
    let direction_flag = take_flag(&mut args, "--direction");
    let name = args
        .into_iter()
        .next()
        .context("uso: kvmc-share peer edit <nome> [--addr ...] [--direction ...]")?;

    let (local, mut peers) = kvmc_share::config::load()?;

    if addr_flag.is_none() && direction_flag.is_none() {
        let peer = peers
            .iter()
            .find(|p| p.name == name)
            .with_context(|| format!("peer '{name}' não encontrado"))?;
        println!("{}\t{}\t{}", peer.name, peer.addr, peer.direction);
        return Ok(());
    }

    let addr = addr_flag
        .map(|a| a.parse::<SocketAddr>())
        .transpose()
        .context("--addr inválido")?;
    let direction = direction_flag.map(|d| d.parse::<Direction>()).transpose()?;

    apply_peer_edit(&mut peers, &name, addr, direction)?;
    kvmc_share::config::save(&default_path()?, &local, &peers)?;
    println!(
        "peer '{name}' atualizado — reinicie o daemon (systemctl --user restart kvmc-share) pra aplicar"
    );
    Ok(())
}

/// Remove e devolve a entrada `name` de `peers` (pura, sem tocar disco).
fn remove_peer(peers: &mut Vec<PeerConfig>, name: &str) -> Result<PeerConfig> {
    let idx = peers
        .iter()
        .position(|p| p.name == name)
        .with_context(|| format!("peer '{name}' não encontrado"))?;
    Ok(peers.remove(idx))
}

/// Apaga o arquivo de PSK em `path`, tolerando ausência (já removido antes,
/// ou nunca gerado via `keygen`).
fn remove_psk_file(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => {
            Err(e).with_context(|| format!("falha ao remover PSK {}", path.display()))
        }
    }
}

/// `kvmc-share peer rm <nome>`. Confirma antes de remover a entrada e sua PSK.
fn cmd_peer_rm(mut args: Vec<String>) -> Result<()> {
    let name = args
        .drain(..)
        .next()
        .context("uso: kvmc-share peer rm <nome>")?;

    let (local, mut peers) = kvmc_share::config::load()?;
    if !peers.iter().any(|p| p.name == name) {
        bail!("peer '{name}' não encontrado");
    }

    if !confirm(
        &mut std::io::stdin().lock(),
        &format!("remover peer '{name}' e sua PSK? (s/N): "),
    )? {
        println!("cancelado");
        return Ok(());
    }

    let removed = remove_peer(&mut peers, &name)?;
    kvmc_share::config::save(&default_path()?, &local, &peers)?;
    remove_psk_file(&removed.psk_path)?;
    println!(
        "peer '{name}' removido — reinicie o daemon (systemctl --user restart kvmc-share) pra aplicar"
    );
    Ok(())
}

/// Aplica os campos informados (`Some`) a `local`, preservando os demais
/// (pura, sem tocar disco).
fn apply_local_edit(
    local: &mut LocalConfig,
    name: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    listen: Option<String>,
) {
    if let Some(name) = name {
        local.name = name;
    }
    if let Some(width) = width {
        local.width = width;
    }
    if let Some(height) = height {
        local.height = height;
    }
    if let Some(listen) = listen {
        local.listen = listen;
    }
}

/// `kvmc-share local edit [--name ...] [--width ...] [--height ...] [--listen ...]`.
/// Sem flags, mostra os valores atuais e sai sem escrever.
fn cmd_local_edit(mut args: Vec<String>) -> Result<()> {
    let name = take_flag(&mut args, "--name");
    let width = take_flag(&mut args, "--width")
        .map(|w| w.parse::<u32>())
        .transpose()
        .context("--width inválido")?;
    let height = take_flag(&mut args, "--height")
        .map(|h| h.parse::<u32>())
        .transpose()
        .context("--height inválido")?;
    let listen = take_flag(&mut args, "--listen");

    let (mut local, peers) = kvmc_share::config::load()?;

    if name.is_none() && width.is_none() && height.is_none() && listen.is_none() {
        println!(
            "{}\t{}x{}\t{}",
            local.name, local.width, local.height, local.listen
        );
        return Ok(());
    }

    apply_local_edit(&mut local, name, width, height, listen);
    kvmc_share::config::save(&default_path()?, &local, &peers)?;
    println!(
        "config local atualizada — reinicie o daemon (systemctl --user restart kvmc-share) pra aplicar"
    );
    Ok(())
}

/// Restringe `peers` aos nomes em `to` (vazio = sem filtro, mantém todos).
/// Nomes duplicados são deduplicados; nome inexistente é erro fatal.
fn filter_peers_by_to(peers: Vec<PeerConfig>, to: &[String]) -> Result<Vec<PeerConfig>> {
    if to.is_empty() {
        return Ok(peers);
    }
    let wanted: std::collections::HashSet<&str> = to.iter().map(String::as_str).collect();
    for name in &wanted {
        if !peers.iter().any(|p| p.name == *name) {
            bail!("peer '{name}' informado em --to não existe em peers.toml");
        }
    }
    Ok(peers
        .into_iter()
        .filter(|p| wanted.contains(p.name.as_str()))
        .collect())
}

/// Extrai todas as ocorrências de `--to` de `args` (flag repetida e/ou
/// valores separados por vírgula), descartando entradas vazias. Vazio de
/// volta = sem filtro.
fn collect_to_names(args: &mut Vec<String>) -> Vec<String> {
    let mut to = Vec::new();
    while let Some(v) = take_flag(args, "--to") {
        to.extend(v.split(',').map(str::to_string).filter(|s| !s.is_empty()));
    }
    to
}

fn run(mut args: Vec<String>) -> Result<()> {
    let to = collect_to_names(&mut args);

    let (local, peers) = kvmc_share::config::load()?;
    let peers = filter_peers_by_to(peers, &to)?;
    let psks: HashMap<String, [u8; 32]> = peers
        .iter()
        .map(|p| Ok((p.name.clone(), read_psk(&p.psk_path)?)))
        .collect::<Result<_>>()?;

    let (event_tx, event_rx) = mpsc::channel::<Event>();

    let mut peer_senders = HashMap::new();
    let mut pending_outbound = HashMap::new();
    for peer in &peers {
        let (tx, rx) = mpsc::channel::<WireMessage>();
        peer_senders.insert(peer.name.clone(), tx);
        pending_outbound.insert(peer.name.clone(), rx);
    }

    for peer in &peers {
        if peer.name <= local.name {
            continue;
        }
        let outbound = pending_outbound
            .remove(&peer.name)
            .expect("peer registrado em pending_outbound");
        spawn_dial_thread(peer.clone(), local.name.clone(), outbound, event_tx.clone());
    }

    let pending_outbound = Arc::new(Mutex::new(pending_outbound));
    spawn_listener_thread(
        local.listen.clone(),
        psks,
        Arc::clone(&pending_outbound),
        event_tx.clone(),
    );

    let capturing = Arc::new(AtomicBool::new(false));
    spawn_capture_threads(&capturing, event_tx.clone())?;

    if kvmc_share::clipboard::is_available() {
        println!("clipboard: daemon copied disponível");
    } else {
        println!("clipboard: daemon copied indisponível (integração de clipboard desativada)");
    }

    let injector = VirtualDeviceInjector(devices::build_virtual_device()?);
    let sender = ChannelPeerSender(peer_senders);
    let mut focus = Focus::new(peers, local.width, local.height, injector, sender);

    println!("kvmc-share: '{}' escutando em {}", local.name, local.listen);

    for event in event_rx {
        match event {
            Event::Local(ev) if is_toggle_press(&ev) => focus.on_toggle_key(),
            Event::Local(ev) => focus.on_input_event(ev),
            Event::Wire(from, msg) => focus.on_wire_message(from, msg),
            Event::PeerDisconnected(peer) => focus.on_peer_disconnected(&peer),
        }
        capturing.store(
            matches!(focus.state(), FocusState::Capturing { .. }),
            Ordering::SeqCst,
        );
    }

    Ok(())
}

fn read_psk(path: &Path) -> Result<[u8; 32]> {
    let bytes = std::fs::read(path).with_context(|| format!("falha ao ler {}", path.display()))?;
    bytes.try_into().map_err(|b: Vec<u8>| {
        anyhow::anyhow!(
            "psk em {} deveria ter 32 bytes, tem {}",
            path.display(),
            b.len()
        )
    })
}

fn is_toggle_press(ev: &InputEvent) -> bool {
    ev.event_type() == EventType::KEY && ev.code() == TOGGLE_KEY.code() && ev.value() == 1
}

fn spawn_dial_thread(
    peer: PeerConfig,
    local_name: String,
    outbound: Receiver<WireMessage>,
    events: Sender<Event>,
) {
    std::thread::spawn(move || {
        let psk = match read_psk(&peer.psk_path) {
            Ok(psk) => psk,
            Err(e) => {
                eprintln!("[{}] não consegui ler a PSK: {e:#}", peer.name);
                return;
            }
        };
        let stream = match TcpStream::connect(peer.addr) {
            Ok(s) => s,
            Err(e) => {
                eprintln!(
                    "[{}] não consegui conectar em {}: {e:#}",
                    peer.name, peer.addr
                );
                return;
            }
        };
        if let Err(e) = stream.set_nodelay(true) {
            eprintln!("[{}] falha ao desativar Nagle (TCP_NODELAY): {e:#}", peer.name);
        }
        if let Err(e) = stream.set_read_timeout(Some(HANDSHAKE_AND_IDLE_READ_TIMEOUT)) {
            eprintln!(
                "[{}] falha ao configurar timeout de leitura: {e:#}",
                peer.name
            );
            return;
        }
        let channel = match handshake_as_initiator(stream, &local_name, &psk) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[{}] handshake (iniciador) falhou: {e:#}", peer.name);
                return;
            }
        };
        println!("[{}] conectado (iniciador)", peer.name);
        run_peer_channel(channel, peer.name, outbound, events);
    });
}

fn spawn_listener_thread(
    listen: String,
    psks: HashMap<String, [u8; 32]>,
    pending_outbound: Arc<Mutex<HashMap<String, Receiver<WireMessage>>>>,
    events: Sender<Event>,
) {
    std::thread::spawn(move || {
        let listener = match TcpListener::bind(&listen) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("falha ao escutar em {listen}: {e:#}");
                return;
            }
        };
        for incoming in listener.incoming() {
            let stream = match incoming {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("falha ao aceitar conexão: {e:#}");
                    continue;
                }
            };
            if let Err(e) = stream.set_nodelay(true) {
                eprintln!("falha ao desativar Nagle (TCP_NODELAY): {e:#}");
            }
            if let Err(e) = stream.set_read_timeout(Some(HANDSHAKE_AND_IDLE_READ_TIMEOUT)) {
                eprintln!("falha ao configurar timeout de leitura: {e:#}");
                continue;
            }

            let resolved_name = Arc::new(Mutex::new(None));
            let resolved_name_writer = Arc::clone(&resolved_name);
            let psks = psks.clone();
            let channel = match handshake_as_responder(stream, move |name| {
                *resolved_name_writer.lock().unwrap() = Some(name.to_string());
                psks.get(name).copied()
            }) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("conexão recusada: {e:#}");
                    continue;
                }
            };
            let peer_name = resolved_name
                .lock()
                .unwrap()
                .clone()
                .expect("handshake bem-sucedido sempre resolve um nome de peer");

            let Some(outbound) = pending_outbound.lock().unwrap().remove(&peer_name) else {
                eprintln!("[{peer_name}] conexão aceita, mas já havia uma sessão ativa; ignorando");
                continue;
            };
            println!("[{peer_name}] conectado (respondedor)");
            let events = events.clone();
            std::thread::spawn(move || run_peer_channel(channel, peer_name, outbound, events));
        }
    });
}

fn run_peer_channel(
    mut channel: EncryptedChannel<TcpStream>,
    peer_name: String,
    outbound: Receiver<WireMessage>,
    events: Sender<Event>,
) {
    loop {
        while let Ok(msg) = outbound.try_recv() {
            if let Err(e) = wire::write_message(&mut channel, &msg) {
                eprintln!("[{peer_name}] falha ao enviar mensagem: {e:#}");
            }
        }
        match wire::read_message(&mut channel) {
            Ok(Some(msg)) => {
                if events.send(Event::Wire(peer_name.clone(), msg)).is_err() {
                    return;
                }
            }
            Ok(None) => break,
            Err(e) if is_read_timeout(&e) => continue,
            Err(e) => {
                eprintln!("[{peer_name}] erro de leitura, encerrando conexão: {e:#}");
                break;
            }
        }
    }
    let _ = events.send(Event::PeerDisconnected(peer_name));
}

fn is_read_timeout(err: &anyhow::Error) -> bool {
    err.chain()
        .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
        .any(|io_err| {
            matches!(
                io_err.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            )
        })
}

fn spawn_capture_threads(capturing: &Arc<AtomicBool>, events: Sender<Event>) -> Result<()> {
    let paths: Vec<String> = std::env::var(DEVICE_PATHS_ENV)
        .unwrap_or_default()
        .split(':')
        .filter(|p| !p.is_empty())
        .map(String::from)
        .collect();
    if paths.is_empty() {
        println!(
            "nenhum dispositivo de captura configurado (defina {DEVICE_PATHS_ENV}=dev1:dev2 pra capturar entrada local)"
        );
        return Ok(());
    }

    for mut device in devices::open_capture_devices(&paths)? {
        let capturing = Arc::clone(capturing);
        let events = events.clone();
        std::thread::spawn(move || {
            let mut grabbed = false;
            loop {
                let should_grab = capturing.load(Ordering::SeqCst);
                if should_grab != grabbed {
                    let result = if should_grab {
                        devices::grab(&mut device)
                    } else {
                        devices::ungrab(&mut device)
                    };
                    if let Err(e) = result {
                        eprintln!("falha ao (des)agarrar dispositivo: {e:#}");
                    }
                    grabbed = should_grab;
                }

                let fetched = match device.fetch_events() {
                    Ok(events) => events.collect::<Vec<_>>(),
                    Err(e) => {
                        eprintln!("falha ao ler eventos do dispositivo: {e:#}");
                        return;
                    }
                };
                for ev in fetched {
                    if events.send(Event::Local(ev)).is_err() {
                        return;
                    }
                }
            }
        });
    }
    Ok(())
}

struct VirtualDeviceInjector(VirtualDevice);

impl LocalInjector for VirtualDeviceInjector {
    fn inject(&mut self, ev: InputEvent) {
        if let Err(e) = self.0.emit(&[ev]) {
            eprintln!("falha ao injetar evento no dispositivo virtual: {e:#}");
        }
    }
}

struct ChannelPeerSender(HashMap<PeerId, Sender<WireMessage>>);

impl PeerSender for ChannelPeerSender {
    fn send_to(&mut self, peer: &PeerId, msg: &WireMessage) {
        let Some(tx) = self.0.get(peer) else {
            eprintln!("[{peer}] sem canal de saída (peer não conectado)");
            return;
        };
        if tx.send(clone_wire_message(msg)).is_err() {
            eprintln!("[{peer}] thread de conexão encerrada, não consegui enviar");
        }
    }
}

fn clone_wire_message(msg: &WireMessage) -> WireMessage {
    match msg {
        WireMessage::InputEvent(ev) => WireMessage::InputEvent(*ev),
        WireMessage::ClipboardText(text) => WireMessage::ClipboardText(text.clone()),
        WireMessage::ClipboardImage { mime, bytes } => WireMessage::ClipboardImage {
            mime: mime.clone(),
            bytes: bytes.clone(),
        },
        WireMessage::FocusHandoff => WireMessage::FocusHandoff,
        WireMessage::Heartbeat => WireMessage::Heartbeat,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_and_save_psk_writes_32_bytes_with_0600_permissions() {
        let path = std::env::temp_dir().join("kvmc-share-test-keygen.psk");
        std::fs::remove_file(&path).ok();

        generate_and_save_psk(&path).unwrap();

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.len(), 32);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn confirm_accepts_s_and_y_case_insensitive() {
        for input in ["s\n", "S\n", "y\n", "Y\n"] {
            let mut reader = std::io::Cursor::new(input);
            assert!(confirm(&mut reader, "confirma? ").unwrap());
        }
    }

    #[test]
    fn confirm_rejects_anything_else_including_empty() {
        for input in ["n\n", "\n", "talvez\n"] {
            let mut reader = std::io::Cursor::new(input);
            assert!(!confirm(&mut reader, "confirma? ").unwrap());
        }
    }

    #[test]
    fn take_flag_removes_flag_and_value_when_found() {
        let mut args = vec!["peer".into(), "add".into(), "--addr".into(), "1.2.3.4:7532".into()];
        let value = take_flag(&mut args, "--addr");
        assert_eq!(value.as_deref(), Some("1.2.3.4:7532"));
        assert_eq!(args, vec!["peer".to_string(), "add".to_string()]);
    }

    #[test]
    fn take_flag_returns_none_and_keeps_args_when_not_found() {
        let mut args = vec!["peer".to_string(), "list".to_string()];
        let value = take_flag(&mut args, "--addr");
        assert_eq!(value, None);
        assert_eq!(args, vec!["peer".to_string(), "list".to_string()]);
    }

    #[test]
    fn format_peer_list_prints_one_line_per_peer() {
        let peers = vec![
            PeerConfig {
                name: "laptop".into(),
                addr: "192.168.1.50:7532".parse().unwrap(),
                psk_path: PathBuf::from("/dev/null"),
                direction: kvmc_share::config::Direction::Right,
            },
            PeerConfig {
                name: "tablet".into(),
                addr: "192.168.1.51:7532".parse().unwrap(),
                psk_path: PathBuf::from("/dev/null"),
                direction: kvmc_share::config::Direction::Left,
            },
        ];
        let out = format_peer_list(&peers);
        assert!(out.contains("laptop") && out.contains("192.168.1.50:7532") && out.contains("right"));
        assert!(out.contains("tablet") && out.contains("192.168.1.51:7532") && out.contains("left"));
        assert_eq!(out.lines().count(), 2);
    }

    #[test]
    fn format_peer_list_empty_shows_explicit_message() {
        let out = format_peer_list(&[]);
        assert!(!out.is_empty());
        assert!(out.contains("nenhum"));
    }

    #[test]
    fn build_new_peer_derives_psk_path_by_convention() {
        unsafe { std::env::set_var("HOME", "/home/marco") };
        let peer = build_new_peer("laptop", "192.168.1.50:7532", "right", None).unwrap();
        assert_eq!(peer.name, "laptop");
        assert_eq!(peer.direction, kvmc_share::config::Direction::Right);
        assert_eq!(
            peer.psk_path,
            PathBuf::from("/home/marco/.config/kvmc-share/peers/laptop.psk")
        );
    }

    #[test]
    fn build_new_peer_respects_psk_path_override() {
        let peer = build_new_peer(
            "laptop",
            "192.168.1.50:7532",
            "right",
            Some(PathBuf::from("/custom/path.psk")),
        )
        .unwrap();
        assert_eq!(peer.psk_path, PathBuf::from("/custom/path.psk"));
    }

    #[test]
    fn build_new_peer_rejects_invalid_direction() {
        let err = build_new_peer("laptop", "192.168.1.50:7532", "diagonal", None).unwrap_err();
        assert!(err.to_string().contains("diagonal"));
    }

    #[test]
    fn build_new_peer_rejects_malformed_addr() {
        let err = build_new_peer("laptop", "not-an-addr", "right", None).unwrap_err();
        assert!(err.to_string().contains("--addr inválido"));
    }

    #[test]
    fn add_peer_to_list_rejects_duplicate_name_without_mutating() {
        let mut peers = vec![PeerConfig {
            name: "laptop".into(),
            addr: "192.168.1.50:7532".parse().unwrap(),
            psk_path: PathBuf::from("/dev/null"),
            direction: kvmc_share::config::Direction::Right,
        }];
        let dup = PeerConfig {
            name: "laptop".into(),
            addr: "192.168.1.99:7532".parse().unwrap(),
            psk_path: PathBuf::from("/dev/null"),
            direction: kvmc_share::config::Direction::Left,
        };
        let err = add_peer_to_list(&mut peers, dup).unwrap_err();
        assert!(err.to_string().contains("já cadastrado"));
        assert_eq!(peers.len(), 1);
    }

    fn sample_peer(name: &str) -> PeerConfig {
        PeerConfig {
            name: name.into(),
            addr: "192.168.1.50:7532".parse().unwrap(),
            psk_path: PathBuf::from("/dev/null"),
            direction: kvmc_share::config::Direction::Right,
        }
    }

    #[test]
    fn apply_peer_edit_addr_only_preserves_direction_and_psk_path() {
        let mut peers = vec![sample_peer("laptop")];
        let new_addr: SocketAddr = "10.0.0.9:9999".parse().unwrap();
        apply_peer_edit(&mut peers, "laptop", Some(new_addr), None).unwrap();
        assert_eq!(peers[0].addr, new_addr);
        assert_eq!(peers[0].direction, kvmc_share::config::Direction::Right);
        assert_eq!(peers[0].psk_path, PathBuf::from("/dev/null"));
    }

    #[test]
    fn apply_peer_edit_direction_only_preserves_addr() {
        let mut peers = vec![sample_peer("laptop")];
        let original_addr = peers[0].addr;
        apply_peer_edit(
            &mut peers,
            "laptop",
            None,
            Some(kvmc_share::config::Direction::Left),
        )
        .unwrap();
        assert_eq!(peers[0].addr, original_addr);
        assert_eq!(peers[0].direction, kvmc_share::config::Direction::Left);
    }

    #[test]
    fn apply_peer_edit_unknown_name_fails_without_mutating() {
        let mut peers = vec![sample_peer("laptop")];
        let err = apply_peer_edit(&mut peers, "ghost", None, None).unwrap_err();
        assert!(err.to_string().contains("não encontrado"));
        assert_eq!(peers[0], sample_peer("laptop"));
    }

    #[test]
    fn remove_peer_removes_and_returns_entry() {
        let mut peers = vec![sample_peer("laptop"), sample_peer("tablet")];
        let removed = remove_peer(&mut peers, "laptop").unwrap();
        assert_eq!(removed.name, "laptop");
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].name, "tablet");
    }

    #[test]
    fn remove_peer_unknown_name_fails_without_mutating() {
        let mut peers = vec![sample_peer("laptop")];
        let err = remove_peer(&mut peers, "ghost").unwrap_err();
        assert!(err.to_string().contains("não encontrado"));
        assert_eq!(peers.len(), 1);
    }

    #[test]
    fn remove_psk_file_deletes_existing_file() {
        let path = std::env::temp_dir().join("kvmc-share-test-rm-psk-existing.psk");
        std::fs::write(&path, b"psk-bytes").unwrap();
        remove_psk_file(&path).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn remove_psk_file_tolerates_missing_file() {
        let path = std::env::temp_dir().join("kvmc-share-test-rm-psk-missing.psk");
        std::fs::remove_file(&path).ok();
        remove_psk_file(&path).unwrap();
    }

    fn sample_local() -> LocalConfig {
        LocalConfig {
            name: "desktop".into(),
            width: 1920,
            height: 1080,
            listen: "0.0.0.0:7532".into(),
        }
    }

    #[test]
    fn apply_local_edit_no_flags_leaves_unchanged() {
        let mut local = sample_local();
        apply_local_edit(&mut local, None, None, None, None);
        assert_eq!(local, sample_local());
    }

    #[test]
    fn apply_local_edit_partial_flags_update_only_those_fields() {
        let mut local = sample_local();
        apply_local_edit(&mut local, None, Some(2560), Some(1440), None);
        assert_eq!(local.name, "desktop");
        assert_eq!(local.width, 2560);
        assert_eq!(local.height, 1440);
        assert_eq!(local.listen, "0.0.0.0:7532");
    }

    #[test]
    fn apply_local_edit_all_flags_update_everything() {
        let mut local = sample_local();
        apply_local_edit(
            &mut local,
            Some("laptop".into()),
            Some(1280),
            Some(720),
            Some("0.0.0.0:9999".into()),
        );
        assert_eq!(local.name, "laptop");
        assert_eq!(local.width, 1280);
        assert_eq!(local.height, 720);
        assert_eq!(local.listen, "0.0.0.0:9999");
    }

    #[test]
    fn filter_peers_by_to_empty_keeps_all_peers() {
        let peers = vec![sample_peer("laptop"), sample_peer("tablet")];
        let filtered = filter_peers_by_to(peers.clone(), &[]).unwrap();
        assert_eq!(filtered, peers);
    }

    #[test]
    fn filter_peers_by_to_restricts_to_named_peers() {
        let peers = vec![sample_peer("laptop"), sample_peer("tablet")];
        let filtered = filter_peers_by_to(peers, &["laptop".to_string()]).unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "laptop");
    }

    #[test]
    fn filter_peers_by_to_dedupes_repeated_names_without_error() {
        let peers = vec![sample_peer("laptop"), sample_peer("tablet")];
        let filtered = filter_peers_by_to(
            peers,
            &["laptop".to_string(), "laptop".to_string()],
        )
        .unwrap();
        assert_eq!(filtered.len(), 1);
    }

    #[test]
    fn filter_peers_by_to_unknown_name_fails_before_returning() {
        let peers = vec![sample_peer("laptop")];
        let err = filter_peers_by_to(peers, &["ghost".to_string()]).unwrap_err();
        assert!(err.to_string().contains("ghost"));
    }

    #[test]
    fn collect_to_names_empty_flag_value_yields_no_filter() {
        let mut args = vec!["--to".to_string(), "".to_string()];
        assert_eq!(collect_to_names(&mut args), Vec::<String>::new());
    }

    #[test]
    fn collect_to_names_only_commas_yields_no_filter() {
        let mut args = vec!["--to".to_string(), ",,".to_string()];
        assert_eq!(collect_to_names(&mut args), Vec::<String>::new());
    }

    #[test]
    fn collect_to_names_supports_comma_list_and_repeated_flag() {
        let mut args = vec![
            "--to".to_string(),
            "laptop,tablet".to_string(),
            "--to".to_string(),
            "tv".to_string(),
        ];
        assert_eq!(
            collect_to_names(&mut args),
            vec!["laptop".to_string(), "tablet".to_string(), "tv".to_string()]
        );
    }
}
