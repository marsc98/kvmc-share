# kvm-share — contexto do projeto

Este documento resume a conversa que originou o projeto `kvm-share`, pra
servir de contexto pro Claude Code (ou qualquer outra ferramenta) entender o
"porquê" por trás das decisões de arquitetura, não só o código em si.

## Ponto de partida

O usuário (Marco, Pop!\_OS com o compositor COSMIC) procurava relembrar o
nome de uma ferramenta simples que já usou antes para compartilhar
mouse/teclado entre computadores diferentes na mesma rede. Identificamos que
era o **Barrier** (fork do antigo Synergy).

## Por que o Barrier não funciona no setup atual

- O Pop!\_OS hoje roda o **COSMIC**, um compositor Wayland baseado em
  Smithay (não em wlroots).
- Ferramentas desse tipo — Barrier, e seus sucessores mantidos ativamente
  **Input Leap** e **Deskflow** — dependem, no Wayland, do portal
  `org.freedesktop.portal.InputCapture` (via `libei`) pra conseguir capturar
  input globalmente de forma sandboxed. Esse portal já está implementado no
  GNOME 46+ e no KDE Plasma 6.1+.
- **O COSMIC ainda não implementa esse portal.** Confirmado em duas issues
  abertas no próprio repositório do Pop!\_OS:
  - [pop-os/xdg-desktop-portal-cosmic#217](https://github.com/pop-os/xdg-desktop-portal-cosmic/issues/217)
    — pede a implementação de `InputCapture`; aberta, sem PR vinculado.
  - [pop-os/cosmic-comp#980](https://github.com/pop-os/cosmic-comp/issues/980)
    — mesmo pedido, do lado do compositor.
- O próprio Barrier tem uma issue antiga pedindo suporte a Wayland
  ([debauchee/barrier#109](https://github.com/debauchee/barrier/issues/109)),
  com uma campanha de doação que bateu a meta, mas o projeto está sem
  manutenção ativa hoje — não é o caminho certo de qualquer forma.
- Também verificamos o **lan-mouse** (alternativa moderna em Rust,
  multiplataforma) como possível solução pronta — mas ele também depende do
  `libei`/portal (GNOME/KDE) ou dos protocolos de layer-shell do wlroots
  (Sway/Hyprland/Wayfire). Como o COSMIC não é wlroots nem tem o portal, ele
  também não cobre esse caso.
- Existe uma proposta de plugin nativo pro ecossistema COSMIC
  ([cosmic-ext-connect-desktop-app#61](https://github.com/olafkfreund/cosmic-ext-connect-desktop-app/issues/61),
  "MouseKeyboardShare", citando explicitamente Synergy/Barrier como
  inspiração) — mas é só uma feature request, não implementada.
- O único workaround real disponível hoje é abandonar o Wayland: logar na
  sessão **"Pop on X11"** (Xorg) que o Pop!\_OS ainda oferece na tela de
  login, onde o Barrier funciona do jeito clássico.

## Decisão: construir uma solução própria

Dado que nada pronto cobre o COSMIC, a saída escolhida foi **contornar o
Wayland inteiramente**, em vez de esperar o portal ser implementado ou
depender de qualquer protocolo específico de compositor. A abordagem atua
uma camada abaixo do Wayland, direto no kernel Linux:

- **Captura**: ler eventos brutos de `/dev/input/eventX` via `evdev`. Isso
  funciona independente do compositor, porque o kernel expõe esses eventos
  pra qualquer processo com permissão (grupo `input` ou root) — não passa
  pelo Wayland de forma alguma.
- **Injeção**: recriar esses eventos na máquina de destino como um
  dispositivo "de verdade" via `/dev/uinput`. O kernel do lado receptor
  enxerga isso como hardware físico — de novo, sem depender de nenhum
  protocolo Wayland específico, então funciona em qualquer compositor.

Essa é essencialmente a mesma técnica usada por baixo dos panos por
ferramentas de automação de input no Linux — só que aplicada aqui
especificamente pra recriar o comportamento do Barrier sem a peça que falta
no COSMIC.

**Escopo definido com o usuário:** protótipo em **Rust**, assumindo
**Linux nos dois lados** (Pop!\_OS/COSMIC nas duas máquinas), o que simplifica
bastante porque `evdev`+`uinput` funcionam nativamente nas duas pontas sem
precisar de nenhuma camada de compatibilidade extra.

## O que foi implementado

Projeto Cargo `kvm-share` com dois binários e uma lib compartilhada:

- **`src/lib.rs`** — protocolo de rede compartilhado: cada `InputEvent`
  evdev é serializado num frame binário fixo de 8 bytes
  (`[type: u16][code: u16][value: i32]`, little-endian), sem dependências
  externas (nada de serde/bincode). Inclui testes unitários de roundtrip
  (serialização/desserialização) e do caso de EOF no meio de um frame.
  Define também `TOGGLE_KEY = KeyCode::KEY_SCROLLLOCK`.

- **`src/bin/capture.rs`** — roda na máquina com o mouse/teclado físico.
  Abre um ou mais dispositivos via `evdev::Device::open`, uma thread por
  dispositivo. Ao detectar a tecla de alternância (Scroll Lock, tap simples)
  pressionada: dá `device.grab()` (EVIOCGRAB, exclusividade — a área de
  trabalho local para de receber os eventos) e passa a encaminhar cada
  evento subsequente por TCP pra máquina alvo; ao pressionar de novo, solta
  o grab (`ungrab()`) e devolve o controle local. A conexão TCP é
  reaproveitada e reconectada sob demanda.

- **`src/bin/inject.rs`** — roda na máquina que recebe o controle. Escuta
  uma porta TCP, cria um dispositivo virtual via
  `evdev::uinput::VirtualDevice::builder()` cobrindo o range padrão de
  teclas de teclado + botões de mouse + eixos relativos (`REL_X`, `REL_Y`,
  `REL_WHEEL`, `REL_HWHEEL`), e repete (`emit`) cada evento recebido nele.

- **`README.md`** — documentação completa: como descobrir os paths dos
  dispositivos (`/dev/input/by-id/`), permissões necessárias (grupo `input`
  pra leitura, regra de udev liberando `/dev/uinput` pra escrita), como
  compilar (`cargo build --release`) e rodar dos dois lados, aviso de
  segurança (o protocolo **não é criptografado nem autenticado** — recomenda
  rodar dentro de uma VPN tipo Tailscale/WireGuard em vez de expor a porta
  direto), limitações conhecidas e ideias de evolução futura.

## Verificação feita nesta sessão

- `cargo build` e `cargo build --release` sem warnings.
- `cargo fmt` aplicado.
- `cargo test`: 3 testes unitários cobrindo o protocolo de serialização —
  todos passando.
- **Não foi possível testar em hardware real** (sem mouse/teclado físico e
  sem acesso root a `/dev/input`/`/dev/uinput` no ambiente de sandbox usado
  pra construir isso). A validação em condições reais — abrir os
  dispositivos certos, grab/ungrab funcionando, o dispositivo virtual sendo
  reconhecido pelo COSMIC do lado receptor — só pode acontecer nas duas
  máquinas do usuário.

## Limitações conhecidas (deliberadas, é um protótipo/MVP)

- Troca de controle só por tecla de atalho (Scroll Lock) — sem detecção de
  borda de tela como no Barrier/Synergy original (exigiria rastrear posição
  de cursor por conta própria, já que mouse relativo não dá coordenada
  absoluta de graça).
- Sem clipboard compartilhado.
- Pequeno atraso possível entre apertar a tecla e o _outro_ dispositivo
  (ex: mouse, se o toggle foi detectado pela thread do teclado) ser
  efetivamente grabbed — cosmético, não afeta o funcionamento.
- Pensado pra duas máquinas; sem lógica de múltiplas telas lado a lado.
- Layout de teclado é definido pela configuração da máquina de **destino**,
  já que o que é encaminhado é o scancode bruto, não o caractere traduzido.
- Sem criptografia/autenticação no protocolo — mitigação recomendada é rodar
  sobre VPN, não expor a porta na internet.

## Próximos passos possíveis

- Testar nas duas máquinas reais, ajustar paths de dispositivo e regras de
  udev conforme o hardware específico.
- Se funcionar bem no básico: considerar detecção de borda de tela,
  criptografia/autenticação embutida (ex: crate `snow` pra Noise Protocol),
  rodar como serviço systemd, clipboard sync, suporte a mais de duas
  máquinas.
- Acompanhar as issues do COSMIC (#217 e #980) — se/quando o portal
  `InputCapture` for implementado nativamente, dá pra migrar pro
  Deskflow/Input Leap "de verdade" e aposentar este workaround.

## Entregável desta sessão

Código-fonte completo (`Cargo.toml`, `src/lib.rs`, `src/bin/capture.rs`,
`src/bin/inject.rs`, `README.md`) entregue como `kvm-share.zip`.
