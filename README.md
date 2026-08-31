# kvm-share

Protótipo de compartilhamento de mouse/teclado entre duas máquinas Linux,
feito especificamente porque o COSMIC (compositor do Pop!_OS) ainda não
implementa o portal `org.freedesktop.portal.InputCapture` do Wayland — o que
faz o Barrier, o Input Leap e o Deskflow não funcionarem nele (veja as
issues [pop-os/xdg-desktop-portal-cosmic#217](https://github.com/pop-os/xdg-desktop-portal-cosmic/issues/217)
e [pop-os/cosmic-comp#980](https://github.com/pop-os/cosmic-comp/issues/980)).

## Como funciona

Em vez de depender de qualquer protocolo do Wayland (portal, wlroots
layer-shell, etc.), este projeto atua uma camada abaixo, direto no kernel:

- **`capture`** roda na máquina onde está o mouse/teclado físico. Lê os
  eventos brutos de `/dev/input/eventX` via `evdev`. Ao pressionar **Scroll
  Lock**, "agarra" os dispositivos com exclusividade (`EVIOCGRAB` — a área de
  trabalho local para de receber os eventos) e passa a encaminhar cada evento
  pela rede.
- **`inject`** roda na máquina que vai *receber* o controle. Recebe os
  eventos pela rede e os recria com um dispositivo virtual criado via
  `/dev/uinput`. Para o kernel dessa máquina, esse dispositivo é
  indistinguível de um mouse/teclado físico — por isso funciona em qualquer
  compositor, COSMIC incluso, já que não depende de nenhuma API do Wayland.

Pressione Scroll Lock de novo na máquina de origem pra devolver o controle
pra ela.

## Por que isso funciona onde o Barrier não funciona

Barrier/Input Leap/Deskflow modernos usam o portal `InputCapture` do Wayland
(via `libei`) — que é o jeito "certo" e sandboxed de fazer isso, mas depende
do compositor implementar esse portal. GNOME 46+ e KDE Plasma 6.1+ já
implementam; o COSMIC ainda não (issue aberta, sem PR até o momento desta
pesquisa). Ler `/dev/input` e escrever em `/dev/uinput` diretamente contorna
essa dependência inteiramente — é o mesmo mecanismo de baixo nível que o
próprio Wayland usa por baixo dos panos, só que acessado diretamente.

## Requisitos

- Rust (`cargo`) instalado nas duas máquinas — ou compile numa e copie o
  binário `--release` pra outra (mesma arquitetura).
- Seu usuário precisa conseguir **ler** os dispositivos em `/dev/input/` (na
  máquina do `capture`) e **escrever** em `/dev/uinput` (na máquina do
  `inject`). Veja a seção de permissões abaixo.

## Compilando

```bash
cargo build --release
# gera target/release/capture e target/release/inject
```

## Descobrindo os paths dos dispositivos (máquina do `capture`)

```bash
# lista nome + arquivo event de cada dispositivo
cat /proc/bus/input/devices | grep -E "Name|Handlers"

# ou, mais estável entre reboots (recomendado usar estes paths):
ls -l /dev/input/by-id/
```

Procure algo como `usb-SEU_TECLADO-event-kbd` e `usb-SEU_MOUSE-event-mouse`.

## Permissões

**Leitura de `/dev/input/eventX`** (máquina do `capture`): normalmente já
liberado pro grupo `input`. Confira se seu usuário está nele:

```bash
groups $USER   # deve listar "input"; se não, rode:
sudo usermod -aG input $USER
# depois faça logout/login (ou reboot) pra valer
```

**Escrita em `/dev/uinput`** (máquina do `inject`): geralmente é restrito a
root por padrão. Crie uma regra de udev pra liberar pro seu grupo:

```bash
echo 'KERNEL=="uinput", GROUP="input", MODE="0660"' | \
  sudo tee /etc/udev/rules.d/99-kvm-share-uinput.rules
sudo udevadm control --reload-rules
sudo udevadm trigger
```

Se preferir não mexer em regras de udev, rodar os binários com `sudo`
resolve os dois lados rapidamente (menos elegante, mas funciona pra testar).

## Rodando

Na máquina que vai **receber** o controle (ex: seu notebook secundário),
primeiro suba o `inject` — ele precisa estar escutando antes do `capture`
tentar conectar:

```bash
./target/release/inject 0.0.0.0:7532
```

Na máquina de **origem** (onde está seu mouse/teclado físico agora):

```bash
./target/release/capture 192.168.1.50:7532 \
  /dev/input/by-id/usb-SEU_TECLADO-event-kbd \
  /dev/input/by-id/usb-SEU_MOUSE-event-mouse
```

(troque `192.168.1.50` pelo IP da máquina que está rodando o `inject`.)

Pressione **Scroll Lock** pra alternar o controle entre as duas máquinas.

## Segurança — leia antes de usar

O protocolo de rede aqui **não é criptografado nem autenticado** — é só uma
sequência de eventos em texto binário puro. Isso é intencional pra manter o
protótipo simples, mas significa que qualquer um na mesma rede pode ler ou
injetar eventos na porta 7532 se ela ficar exposta. Recomendações:

- Rode isso só dentro de uma rede que você controla (ex: sua LAN doméstica).
- Melhor ainda: rode as duas pontas dentro de uma VPN mesh como
  [Tailscale](https://tailscale.com/) ou WireGuard, e aponte o `capture` pro
  IP da VPN da outra máquina em vez do IP da LAN. Isso te dá criptografia e
  autenticação "de graça" sem precisar implementar nada aqui.
- Evite expor a porta do `inject` na internet.

Também vale lembrar: enquanto o `capture` está rodando, ele tem acesso bruto
a tudo que seu teclado digita nesse dispositivo (é assim que consegue
detectar a tecla de alternância). Isso é inerente à abordagem — só rode
binários que você compilou/revisou você mesmo.

## Limitações conhecidas deste protótipo

- **Sem detecção de borda de tela**: a troca é só por tecla de atalho
  (Scroll Lock), não por mover o cursor até a borda da tela como no
  Barrier/Synergy. Dá pra evoluir depois, mas exigiria rastrear a posição do
  cursor por conta própria (o evdev não te dá coordenada absoluta de mouse
  relativo de graça).
- **Sem clipboard compartilhado.**
- Com múltiplos dispositivos (teclado + mouse em threads separadas), pode
  haver um pequeno atraso entre apertar Scroll Lock e o *outro* dispositivo
  ser efetivamente "agarrado" — na prática, imperceptível, mas vale saber que
  existe.
- Suporta duas máquinas (uma origem, um destino) por enquanto — nada impede
  de rodar várias instâncias de `capture` mirando `inject`s diferentes, mas
  não há lógica de "várias telas lado a lado" nenhuma.
- Layout de teclado: como encaminhamos o *scancode* bruto (não o caractere
  já traduzido), quem decide o layout final é a configuração de teclado da
  máquina de **destino** — funciona bem se as duas máquinas usam o mesmo
  layout; pode dar diferença se forem layouts distintos.

## Ideias de evolução (se quiser continuar)

- Detecção real de borda de tela (rastrear posição virtual do cursor).
- Autenticação (token compartilhado) + criptografia (ex: usar o crate
  `snow` pra Noise Protocol) direto no protocolo, sem depender de VPN.
- Rodar como serviço systemd nas duas máquinas.
- Clipboard sync.
- Suporte a mais de duas máquinas (roteamento por qual tem o "foco").
