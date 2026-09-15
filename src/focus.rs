//! Máquina de estados de foco: decide se eventos de input vão pro dispositivo
//! local ou são encaminhados a um peer, e implementa o relay em cadeia (uma
//! máquina em `Receiving` que detecta o cursor injetado cruzando outra borda
//! vira `Capturing` pro próximo peer, sem que a origem original saiba).
//!
//! `LocalInjector`/`PeerSender` abstraem `devices`/`wire` pra permitir testar
//! a máquina de estados sem `/dev/uinput` nem rede real.

use crate::config::{Direction, PeerConfig};
use crate::cursor::{CursorTracker, Direction as CursorDirection};
use crate::wire::WireMessage;
use evdev::{EventType, InputEvent, RelativeAxisCode};

pub type PeerId = String;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FocusState {
    Local,
    /// `resume` é o estado de onde essa captura partiu (`Local` numa captura
    /// inicial, `Receiving { source }` num relay em cadeia) — usado por
    /// `on_focus_handoff` pra voltar ao lugar certo quando `target` devolve o
    /// controle, em vez de sempre pousar em `Receiving`.
    Capturing {
        target: PeerId,
        resume: Box<FocusState>,
    },
    Receiving { source: PeerId },
}

/// Injeta um evento no dispositivo virtual local (implementação real usa
/// `devices::build_virtual_device`; aqui só o trait pra permitir mock).
pub trait LocalInjector {
    fn inject(&mut self, ev: InputEvent);
}

/// Envia uma mensagem pra um peer (implementação real usa
/// `wire::write_message` sobre o `EncryptedChannel` daquele peer).
pub trait PeerSender {
    fn send_to(&mut self, peer: &PeerId, msg: &WireMessage);
}

fn to_config_direction(dir: CursorDirection) -> Direction {
    match dir {
        CursorDirection::Left => Direction::Left,
        CursorDirection::Right => Direction::Right,
        CursorDirection::Up => Direction::Up,
        CursorDirection::Down => Direction::Down,
    }
}

pub struct Focus<I: LocalInjector, S: PeerSender> {
    state: FocusState,
    peers: Vec<PeerConfig>,
    cursor: CursorTracker,
    last_target: Option<PeerId>,
    injector: I,
    sender: S,
}

impl<I: LocalInjector, S: PeerSender> Focus<I, S> {
    pub fn new(peers: Vec<PeerConfig>, width: u32, height: u32, injector: I, sender: S) -> Self {
        Self {
            state: FocusState::Local,
            peers,
            cursor: CursorTracker::new(width, height),
            last_target: None,
            injector,
            sender,
        }
    }

    pub fn state(&self) -> &FocusState {
        &self.state
    }

    /// Evento lido do dispositivo local (só relevante em `Local`, pra
    /// detectar cruzamento de borda, e em `Capturing`, pra encaminhar).
    pub fn on_input_event(&mut self, ev: InputEvent) {
        match &self.state {
            FocusState::Local => self.maybe_capture(ev),
            FocusState::Capturing { target, .. } => {
                let target = target.clone();
                self.sender.send_to(&target, &WireMessage::InputEvent(ev));
            }
            FocusState::Receiving { .. } => {}
        }
    }

    pub fn on_wire_message(&mut self, from: PeerId, msg: WireMessage) {
        match msg {
            WireMessage::FocusHandoff => self.on_focus_handoff(from),
            WireMessage::InputEvent(ev) => self.on_wire_input_event(from, ev),
            WireMessage::ClipboardText(_) | WireMessage::ClipboardImage { .. } => {}
            WireMessage::Heartbeat => {}
        }
    }

    /// Alterna `Local <-> Capturing` manualmente. Não afeta `Receiving`
    /// (retorno de `Receiving` é só via queda de conexão, ver diagrama).
    pub fn on_toggle_key(&mut self) {
        match &self.state {
            FocusState::Capturing { .. } => self.state = FocusState::Local,
            FocusState::Local => {
                if let Some(target) = self.last_target.clone() {
                    self.sender.send_to(&target, &WireMessage::FocusHandoff);
                    self.state = FocusState::Capturing {
                        target,
                        resume: Box::new(FocusState::Local),
                    };
                }
            }
            FocusState::Receiving { .. } => {}
        }
    }

    /// Nunca fica "sem controle": queda de conexão com o peer envolvido no
    /// estado atual (source ou target) sempre retorna pra `Local`.
    pub fn on_peer_disconnected(&mut self, peer: &PeerId) {
        let involved = match &self.state {
            FocusState::Capturing { target, .. } => target == peer,
            FocusState::Receiving { source } => source == peer,
            FocusState::Local => false,
        };
        if involved {
            self.state = FocusState::Local;
        }
    }

    /// Se `from` é justamente o peer pro qual eu abri uma captura (`target`),
    /// esse handoff é ele devolvendo o controle — volto pro `resume` guardado,
    /// não pra `Receiving`. Caso contrário é uma captura nova chegando (ou
    /// interrompendo um relay em andamento), mesma lógica de antes.
    fn on_focus_handoff(&mut self, from: PeerId) {
        if let FocusState::Capturing { target, resume } = &self.state
            && *target == from
        {
            self.state = *resume.clone();
            return;
        }
        let keep_current =
            matches!(&self.state, FocusState::Receiving { source } if *source < from);
        if !keep_current {
            self.state = FocusState::Receiving { source: from };
        }
    }

    fn on_wire_input_event(&mut self, from: PeerId, ev: InputEvent) {
        match &self.state {
            FocusState::Receiving { source } if *source == from => {
                self.injector.inject(ev);
                self.maybe_capture(ev);
            }
            FocusState::Capturing { target, .. } => {
                let target = target.clone();
                self.sender.send_to(&target, &WireMessage::InputEvent(ev));
            }
            _ => {}
        }
    }

    /// Cruzamento de borda leva a `Capturing` pro peer daquela direção,
    /// tanto a partir de `Local` (captura inicial) quanto de `Receiving`
    /// (relay adiante) — mesma transição, origem diferente.
    fn maybe_capture(&mut self, ev: InputEvent) {
        let Some(direction) = self.accumulate(ev) else {
            return;
        };
        let Some(target) = self.peer_for(direction) else {
            return;
        };
        let resume = Box::new(self.state.clone());
        self.sender.send_to(&target, &WireMessage::FocusHandoff);
        self.last_target = Some(target.clone());
        self.state = FocusState::Capturing { target, resume };
    }

    fn accumulate(&mut self, ev: InputEvent) -> Option<Direction> {
        if ev.event_type() != EventType::RELATIVE {
            return None;
        }
        let (dx, dy) = match RelativeAxisCode(ev.code()) {
            RelativeAxisCode::REL_X => (ev.value(), 0),
            RelativeAxisCode::REL_Y => (0, ev.value()),
            _ => return None,
        };
        self.cursor.accumulate(dx, dy).map(to_config_direction)
    }

    fn peer_for(&self, direction: Direction) -> Option<PeerId> {
        self.peers
            .iter()
            .find(|p| p.direction == direction)
            .map(|p| p.name.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct MockInjector {
        injected: Vec<InputEvent>,
    }

    impl LocalInjector for MockInjector {
        fn inject(&mut self, ev: InputEvent) {
            self.injected.push(ev);
        }
    }

    #[derive(Default)]
    struct MockSender {
        sent: Vec<(PeerId, String)>,
    }

    impl PeerSender for MockSender {
        fn send_to(&mut self, peer: &PeerId, msg: &WireMessage) {
            self.sent.push((peer.clone(), format!("{msg:?}")));
        }
    }

    fn peer(name: &str, direction: Direction) -> PeerConfig {
        PeerConfig {
            name: name.into(),
            addr: "127.0.0.1:7532".parse().unwrap(),
            psk_path: "/dev/null".into(),
            direction,
        }
    }

    fn rel_x(value: i32) -> InputEvent {
        InputEvent::new(EventType::RELATIVE.0, RelativeAxisCode::REL_X.0, value)
    }

    fn focus(peers: Vec<PeerConfig>) -> Focus<MockInjector, MockSender> {
        Focus::new(
            peers,
            100,
            100,
            MockInjector::default(),
            MockSender::default(),
        )
    }

    #[test]
    fn crosses_border_with_configured_peer_starts_capturing() {
        let mut f = focus(vec![peer("laptop", Direction::Right)]);
        f.on_input_event(rel_x(1000));
        assert_eq!(
            f.state(),
            &FocusState::Capturing {
                target: "laptop".into(),
                resume: Box::new(FocusState::Local)
            }
        );
        assert_eq!(
            f.sender.sent,
            vec![("laptop".into(), "FocusHandoff".into())]
        );
    }

    #[test]
    fn crosses_border_without_configured_peer_stays_local() {
        let mut f = focus(vec![peer("laptop", Direction::Right)]);
        f.on_input_event(rel_x(-1000));
        assert_eq!(f.state(), &FocusState::Local);
        assert!(f.sender.sent.is_empty());
    }

    #[test]
    fn toggle_key_flips_between_capturing_and_local_ignoring_deltas() {
        let mut f = focus(vec![peer("laptop", Direction::Right)]);
        f.on_input_event(rel_x(1000));
        assert!(matches!(f.state(), FocusState::Capturing { .. }));

        f.on_input_event(rel_x(5));
        f.on_toggle_key();
        assert_eq!(f.state(), &FocusState::Local);

        f.on_toggle_key();
        assert_eq!(
            f.state(),
            &FocusState::Capturing {
                target: "laptop".into(),
                resume: Box::new(FocusState::Local)
            }
        );
    }

    #[test]
    fn receiving_relays_to_next_peer_when_injected_cursor_crosses_border() {
        let mut f = focus(vec![peer("desktop-b", Direction::Right)]);
        f.on_wire_message("desktop-a".into(), WireMessage::FocusHandoff);
        assert_eq!(
            f.state(),
            &FocusState::Receiving {
                source: "desktop-a".into()
            }
        );

        f.on_wire_message("desktop-a".into(), WireMessage::InputEvent(rel_x(1000)));

        assert_eq!(f.injector.injected.len(), 1);
        assert_eq!(
            f.state(),
            &FocusState::Capturing {
                target: "desktop-b".into(),
                resume: Box::new(FocusState::Receiving {
                    source: "desktop-a".into()
                })
            }
        );
        assert!(
            f.sender
                .sent
                .contains(&("desktop-b".into(), "FocusHandoff".into()))
        );
    }

    #[test]
    fn receiving_forwards_wire_events_onward_after_relay_without_touching_origin() {
        let mut f = focus(vec![peer("desktop-b", Direction::Right)]);
        f.on_wire_message("desktop-a".into(), WireMessage::FocusHandoff);
        f.on_wire_message("desktop-a".into(), WireMessage::InputEvent(rel_x(1000)));

        f.on_wire_message("desktop-a".into(), WireMessage::InputEvent(rel_x(5)));

        assert_eq!(
            f.injector.injected.len(),
            1,
            "não injeta mais localmente após o relay"
        );
        assert!(f.sender.sent.iter().any(|(peer, msg)| peer == "desktop-b"
            && msg.contains("InputEvent")
            && msg.contains("value: 5")));
    }

    #[test]
    fn concurrent_focus_handoff_resolves_deterministically_by_smaller_name() {
        let mut f = focus(vec![]);
        f.on_wire_message("beta".into(), WireMessage::FocusHandoff);
        f.on_wire_message("alpha".into(), WireMessage::FocusHandoff);
        assert_eq!(
            f.state(),
            &FocusState::Receiving {
                source: "alpha".into()
            }
        );

        let mut f2 = focus(vec![]);
        f2.on_wire_message("alpha".into(), WireMessage::FocusHandoff);
        f2.on_wire_message("beta".into(), WireMessage::FocusHandoff);
        assert_eq!(
            f2.state(),
            &FocusState::Receiving {
                source: "alpha".into()
            }
        );
    }

    #[test]
    fn peer_disconnected_returns_to_local_from_capturing() {
        let mut f = focus(vec![peer("laptop", Direction::Right)]);
        f.on_input_event(rel_x(1000));
        f.on_peer_disconnected(&"laptop".to_string());
        assert_eq!(f.state(), &FocusState::Local);
    }

    #[test]
    fn peer_disconnected_returns_to_local_from_receiving() {
        let mut f = focus(vec![]);
        f.on_wire_message("desktop-a".into(), WireMessage::FocusHandoff);
        f.on_peer_disconnected(&"desktop-a".to_string());
        assert_eq!(f.state(), &FocusState::Local);
    }

    #[test]
    fn peer_disconnected_ignores_unrelated_peer() {
        let mut f = focus(vec![peer("laptop", Direction::Right)]);
        f.on_input_event(rel_x(1000));
        f.on_peer_disconnected(&"someone-else".to_string());
        assert_eq!(
            f.state(),
            &FocusState::Capturing {
                target: "laptop".into(),
                resume: Box::new(FocusState::Local)
            }
        );
    }

    #[test]
    fn focus_handoff_from_current_target_returns_to_resume_state() {
        let mut f = focus(vec![peer("meu", Direction::Right)]);
        f.on_input_event(rel_x(1000));
        assert!(matches!(f.state(), FocusState::Capturing { .. }));

        f.on_wire_message("meu".into(), WireMessage::FocusHandoff);

        assert_eq!(
            f.state(),
            &FocusState::Local,
            "handoff de quem eu capturei deve voltar pro estado de origem, não Receiving"
        );
    }

    #[test]
    fn focus_handoff_from_target_mid_chain_resumes_receiving_not_local() {
        let mut f = focus(vec![peer("terceiro", Direction::Right)]);
        f.on_wire_message("nav".into(), WireMessage::FocusHandoff);
        f.on_wire_message("nav".into(), WireMessage::InputEvent(rel_x(1000)));
        assert_eq!(
            f.state(),
            &FocusState::Capturing {
                target: "terceiro".into(),
                resume: Box::new(FocusState::Receiving {
                    source: "nav".into()
                })
            }
        );

        f.on_wire_message("terceiro".into(), WireMessage::FocusHandoff);

        assert_eq!(
            f.state(),
            &FocusState::Receiving {
                source: "nav".into()
            },
            "no meio de uma cadeia, retorno deve resumir Receiving da origem, não Local"
        );
    }
}
