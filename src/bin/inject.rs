//! inject: roda na máquina que vai RECEBER o controle (a que não tem o
//! mouse/teclado físico no momento).
//!
//! Escuta uma porta TCP, e para cada evento recebido do `capture`, recria o
//! evento num dispositivo virtual criado via /dev/uinput. Pro kernel dessa
//! máquina, esse dispositivo virtual é indistinguível de um mouse/teclado
//! físico de verdade — por isso funciona em qualquer compositor Wayland
//! (COSMIC incluso), já que não depende de nenhum protocolo do Wayland.
//!
//! Uso:
//!   inject <host:porta-escuta>
//!
//! Exemplo:
//!   inject 0.0.0.0:7532

use anyhow::{Context, Result};
use evdev::{AttributeSet, KeyCode, RelativeAxisCode};
use kvm_share::read_event;
use std::net::{TcpListener, TcpStream};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 2 {
        eprintln!(
            "uso: {} <host:porta-escuta>",
            args.first().map(String::as_str).unwrap_or("inject")
        );
        std::process::exit(1);
    }
    let listen_addr = &args[1];

    let listener = TcpListener::bind(listen_addr)
        .with_context(|| format!("não consegui escutar em {listen_addr}"))?;
    println!("kvm-share inject");
    println!("  escutando em {listen_addr}");
    println!("  (dica: crie o dispositivo virtual só quando a primeira conexão chegar? não —");
    println!("   ele é criado uma vez no início e reaproveitado entre conexões)");
    println!();

    let mut device = build_virtual_device().context(
        "falha ao criar dispositivo virtual via /dev/uinput \
         (rode como root ou libere /dev/uinput pro seu usuário via regra udev — veja o README)",
    )?;
    println!("dispositivo virtual criado com sucesso.");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let peer = stream
                    .peer_addr()
                    .map(|a| a.to_string())
                    .unwrap_or_default();
                println!("conexão recebida de {peer}");
                if let Err(e) = handle_connection(stream, &mut device) {
                    eprintln!("conexão com {peer} encerrada: {e:#}");
                }
            }
            Err(e) => eprintln!("erro ao aceitar conexão: {e}"),
        }
    }
    Ok(())
}

fn handle_connection(
    mut stream: TcpStream,
    device: &mut evdev::uinput::VirtualDevice,
) -> Result<()> {
    stream.set_nodelay(true).ok();
    loop {
        match read_event(&mut stream)? {
            None => {
                println!("conexão encerrada pelo outro lado");
                return Ok(());
            }
            Some(ev) => {
                device
                    .emit(&[ev])
                    .context("falha ao injetar evento no dispositivo virtual")?;
            }
        }
    }
}

/// Monta um dispositivo virtual "genérico" cobrindo o teclado padrão, os
/// botões comuns de mouse e os eixos relativos (movimento + scroll). Cobre o
/// que a grande maioria dos setups precisa; se faltar alguma tecla especial
/// do seu teclado, é só adicionar o código dela nos ranges abaixo.
fn build_virtual_device() -> Result<evdev::uinput::VirtualDevice> {
    let mut keys = AttributeSet::<KeyCode>::new();
    // Teclado "padrão" (KEY_ESC=1 até KEY_MICMUTE=248, cobre letras, números,
    // função, navegação, multimídia comum etc — ver input-event-codes.h).
    for code in 1u16..248 {
        keys.insert(KeyCode::new(code));
    }
    // Botões de mouse (BTN_LEFT..BTN_TASK).
    for code in 0x110u16..0x118 {
        keys.insert(KeyCode::new(code));
    }

    let mut axes = AttributeSet::<RelativeAxisCode>::new();
    axes.insert(RelativeAxisCode::REL_X);
    axes.insert(RelativeAxisCode::REL_Y);
    axes.insert(RelativeAxisCode::REL_WHEEL);
    axes.insert(RelativeAxisCode::REL_HWHEEL);

    let device = evdev::uinput::VirtualDevice::builder()?
        .name("kvm-share virtual input")
        .with_keys(&keys)?
        .with_relative_axes(&axes)?
        .build()?;

    Ok(device)
}
