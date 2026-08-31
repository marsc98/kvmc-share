//! capture: roda na máquina onde você usa o mouse/teclado físico.
//!
//! Lê eventos de um ou mais dispositivos evdev (tipicamente um teclado e um
//! mouse), e quando a tecla de alternância (Scroll Lock, ver `TOGGLE_KEY`) é
//! pressionada, passa a "agarrar" esses dispositivos com exclusividade
//! (EVIOCGRAB) e encaminhar cada evento pela rede para a máquina alvo, em vez
//! de deixá-los chegar à sua área de trabalho local. Pressione de novo pra
//! devolver o controle pra esta máquina.
//!
//! Uso:
//!   capture <host:porta-destino> <dispositivo1> [dispositivo2 ...]
//!
//! Exemplo:
//!   capture 192.168.1.50:7532 \
//!       /dev/input/by-id/usb-Logitech_Keyboard-event-kbd \
//!       /dev/input/by-id/usb-Logitech_Mouse-event-mouse
//!
//! Use `evtest` ou `cat /proc/bus/input/devices` pra descobrir os paths.
//! Prefira os symlinks estáveis em /dev/input/by-id/ em vez de eventN, que
//! pode mudar de número entre boots.

use anyhow::{Context, Result};
use evdev::Device;
use kvm_share::{TOGGLE_KEY, write_event};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!(
            "uso: {} <host:porta-destino> <dispositivo1> [dispositivo2 ...]",
            args.first().map(String::as_str).unwrap_or("capture")
        );
        std::process::exit(1);
    }
    let target_addr = args[1].clone();
    let device_paths = args[2..].to_vec();

    println!("kvm-share capture");
    println!("  destino: {target_addr}");
    println!("  dispositivos: {device_paths:?}");
    println!("  tecla de alternância: Scroll Lock (pressione e solte pra ligar/desligar)");
    println!();

    // Estado compartilhado entre as threads (uma por dispositivo).
    let forwarding = Arc::new(AtomicBool::new(false));
    // Conexão TCP compartilhada e reaproveitada entre as threads; None
    // quando ainda não conectada ou quando a última tentativa falhou.
    let conn: Arc<Mutex<Option<TcpStream>>> = Arc::new(Mutex::new(None));

    let mut handles = Vec::new();
    for path in device_paths {
        let device = Device::open(&path)
            .with_context(|| format!("não consegui abrir {path} (rode como root ou adicione seu usuário ao grupo 'input')"))?;
        let name = device.name().unwrap_or("dispositivo sem nome").to_string();
        println!("aberto: {path} ({name})");

        let forwarding = Arc::clone(&forwarding);
        let conn = Arc::clone(&conn);
        let target_addr = target_addr.clone();
        let path = path.clone();

        handles.push(std::thread::spawn(move || {
            if let Err(e) = run_device_loop(device, &path, target_addr, forwarding, conn) {
                eprintln!("[{path}] thread encerrada com erro: {e:#}");
            }
        }));
    }

    for h in handles {
        let _ = h.join();
    }
    Ok(())
}

fn run_device_loop(
    mut device: Device,
    path: &str,
    target_addr: String,
    forwarding: Arc<AtomicBool>,
    conn: Arc<Mutex<Option<TcpStream>>>,
) -> Result<()> {
    loop {
        // Coleta os eventos num Vec antes de processar: o iterador de
        // fetch_events() mantém `device` emprestado, e mais abaixo
        // precisamos chamar device.grab()/ungrab(), que exigem &mut device
        // livre.
        let events: Vec<evdev::InputEvent> = device
            .fetch_events()
            .with_context(|| format!("falha ao ler eventos de {path}"))?
            .collect();

        for ev in events {
            let is_toggle = ev.event_type() == evdev::EventType::KEY
                && ev.code() == TOGGLE_KEY.code()
                && ev.value() == 1; // 1 = tecla pressionada (down); ignora repeat(2)/up(0)

            if is_toggle {
                let now_forwarding = !forwarding.fetch_xor(true, Ordering::SeqCst);
                if now_forwarding {
                    match device.grab() {
                        Ok(()) => println!("[{path}] modo remoto ATIVADO (grab ok)"),
                        Err(e) => eprintln!(
                            "[{path}] modo remoto ativado, mas grab falhou: {e} (eventos locais também vão continuar chegando)"
                        ),
                    }
                    ensure_connected(&target_addr, &conn);
                } else {
                    if let Err(e) = device.ungrab() {
                        eprintln!("[{path}] falha ao soltar o dispositivo (ungrab): {e}");
                    }
                    println!(
                        "[{path}] modo remoto DESATIVADO (controle de volta pra esta máquina)"
                    );
                }
                // Não encaminha o próprio evento de alternância.
                continue;
            }

            if forwarding.load(Ordering::SeqCst) {
                forward(&ev, &target_addr, &conn);
            }
            // Quando não está em modo remoto, o dispositivo não está grabbed,
            // então o evento já chega normalmente à sua área de trabalho
            // local sem precisarmos fazer nada.
        }
    }
}

/// Garante que existe uma conexão TCP válida em `conn`, tentando reconectar
/// se necessário. Erros de conexão só são logados — o loop principal segue
/// tentando a cada evento subsequente enquanto o modo remoto estiver ativo.
fn ensure_connected(target_addr: &str, conn: &Arc<Mutex<Option<TcpStream>>>) {
    let mut guard = conn.lock().unwrap();
    if guard.is_some() {
        return;
    }
    match TcpStream::connect(target_addr) {
        Ok(s) => {
            let _ = s.set_nodelay(true);
            let _ = s.set_write_timeout(Some(Duration::from_millis(500)));
            println!("conectado a {target_addr}");
            *guard = Some(s);
        }
        Err(e) => eprintln!(
            "não consegui conectar a {target_addr}: {e} (vou tentar de novo no próximo evento)"
        ),
    }
}

fn forward(ev: &evdev::InputEvent, target_addr: &str, conn: &Arc<Mutex<Option<TcpStream>>>) {
    let mut guard = conn.lock().unwrap();
    if guard.is_none() {
        drop(guard);
        ensure_connected(target_addr, conn);
        guard = conn.lock().unwrap();
    }
    let Some(stream) = guard.as_mut() else {
        return;
    };
    if let Err(e) = write_event(stream, ev) {
        eprintln!("falha ao encaminhar evento, vou tentar reconectar: {e:#}");
        *guard = None;
    }
}
