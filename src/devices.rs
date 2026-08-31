//! Captura e injeção de eventos evdev/uinput. Testes de integração com hardware real ficam pra verificação manual (T14).

use anyhow::{Context, Result};
use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, Device, KeyCode, RelativeAxisCode};

pub fn open_capture_devices(paths: &[String]) -> Result<Vec<Device>> {
    paths
        .iter()
        .map(|path| {
            Device::open(path).with_context(|| {
                format!(
                    "não consegui abrir {path} (rode como root ou adicione seu usuário ao grupo 'input')"
                )
            })
        })
        .collect()
}

pub fn grab(device: &mut Device) -> Result<()> {
    device.grab().context("falha ao agarrar (grab) dispositivo")
}

pub fn ungrab(device: &mut Device) -> Result<()> {
    device
        .ungrab()
        .context("falha ao soltar (ungrab) dispositivo")
}

/// Monta um dispositivo virtual "genérico" cobrindo o teclado padrão, os
/// botões comuns de mouse e os eixos relativos (movimento + scroll). Cobre o
/// que a grande maioria dos setups precisa; se faltar alguma tecla especial
/// do seu teclado, é só adicionar o código dela nos ranges abaixo.
pub fn build_virtual_device() -> Result<VirtualDevice> {
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

    let device = VirtualDevice::builder()?
        .name("kvm-share virtual input")
        .with_keys(&keys)?
        .with_relative_axes(&axes)?
        .build()?;

    Ok(device)
}
