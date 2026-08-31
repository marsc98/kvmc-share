//! `kvm-share run`: daemon único de malha — substitui os binários separados
//! `capture`/`inject` (T12 vai removê-los). Carrega `peers.toml`, conecta
//! com cada peer via Noise (dial pro peer com nome lexicograficamente maior
//! que o local, escuta pros demais, evitando dois lados discarem ao mesmo
//! tempo), abre os dispositivos locais e roda o loop de despacho pra
//! `focus::Focus`.

use anyhow::{Context, Result};
use evdev::uinput::VirtualDevice;
use evdev::{EventType, InputEvent};
use kvm_share::config::PeerConfig;
use kvm_share::focus::{Focus, FocusState, LocalInjector, PeerId, PeerSender};
use kvm_share::noise::{EncryptedChannel, handshake_as_initiator, handshake_as_responder};
use kvm_share::wire::{self, WireMessage};
use kvm_share::{TOGGLE_KEY, devices};
use std::collections::HashMap;
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Nomes dos dispositivos evdev locais a capturar. `peers.toml` (T2) ainda
/// não expõe esse campo, então lemos de uma variável de ambiente
/// (`caminho1:caminho2:...`, no estilo de `PATH`) até essa lacuna ser
/// fechada numa task futura de config.
const DEVICE_PATHS_ENV: &str = "KVM_SHARE_DEVICES";

const HANDSHAKE_AND_IDLE_READ_TIMEOUT: Duration = Duration::from_millis(500);

enum Event {
    Local(InputEvent),
    Wire(PeerId, WireMessage),
    PeerDisconnected(PeerId),
}

fn main() -> Result<()> {
    match std::env::args().nth(1).as_deref() {
        None | Some("run") => run(),
        _ => {
            eprintln!("uso: kvm-share [run]");
            std::process::exit(1);
        }
    }
}

fn run() -> Result<()> {
    let (local, peers) = kvm_share::config::load()?;
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

    if kvm_share::clipboard::is_available() {
        println!("clipboard: daemon copied disponível");
    } else {
        println!("clipboard: daemon copied indisponível (integração de clipboard desativada)");
    }

    let injector = VirtualDeviceInjector(devices::build_virtual_device()?);
    let sender = ChannelPeerSender(peer_senders);
    let mut focus = Focus::new(peers, local.width, local.height, injector, sender);

    println!("kvm-share: '{}' escutando em {}", local.name, local.listen);

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
