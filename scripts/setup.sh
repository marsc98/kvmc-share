#!/usr/bin/env bash
#
# setup.sh — prepara uma máquina para rodar o kvmc-share.
#
# Sem argumentos: wizard interativo (deps -> config -> keygen -> subida).
# Com subcomando: executa só aquela etapa.
#
#   deps       grupo input, regra udev de /dev/uinput, binário
#   config     dispositivos de captura + peers.toml
#   keygen     PSK por par de peers (gera/recebe, distribui via scp)
#   run        sobe o daemon em foreground (lê ~/.config/kvmc-share/env)
#   service    instala unit systemd --user + linger
#   doctor     diagnóstico: o que está e o que não está pronto
#   uninstall  remove unit, regra udev e ~/.config/kvmc-share
#
# Respeita KVMC_BIN (caminho do binário) e NO_COLOR.

# Constantes da Camada 0 são um bloco compartilhado consumido pelos cmd_*;
# ficam definidas antes do primeiro uso.
# shellcheck disable=SC2034

set -euo pipefail
trap 'printf >&2 "\n[erro] abortado\n"; exit 130' INT

# --- Camada 0: constantes -----------------------------------------------------

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${KVMC_BIN:-$REPO_ROOT/target/release/kvmc-share}"

CONF_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/kvmc-share"
ENV_FILE="$CONF_DIR/env"
PEERS_TOML="$CONF_DIR/peers.toml"
PSK_DIR="$CONF_DIR/peers"

UDEV_RULE="/etc/udev/rules.d/99-kvmc-share-uinput.rules"
UDEV_LINE='KERNEL=="uinput", GROUP="input", MODE="0660"'

UNIT="$HOME/.config/systemd/user/kvmc-share.service"

SERVICE_PORT_DEFAULT=7532

# --- Subcomandos (stubs — preenchidos nas próximas tasks) --------------------

cmd_deps() { printf >&2 'deps: não implementado\n'; exit 1; }
cmd_config() { printf >&2 'config: não implementado\n'; exit 1; }
cmd_keygen() { printf >&2 'keygen: não implementado\n'; exit 1; }
cmd_run() { printf >&2 'run: não implementado\n'; exit 1; }
cmd_service() { printf >&2 'service: não implementado\n'; exit 1; }
cmd_doctor() { printf >&2 'doctor: não implementado\n'; exit 1; }
cmd_uninstall() { printf >&2 'uninstall: não implementado\n'; exit 1; }
cmd_wizard() { printf >&2 'wizard: não implementado\n'; exit 1; }

# --- Dispatch ---------------------------------------------------------------

usage() {
	cat <<'EOF'
uso: setup.sh [subcomando]

sem subcomando        wizard interativo (deps -> config -> keygen -> subida)

subcomandos:
  deps        grupo input, regra udev de /dev/uinput, binário
  config      dispositivos de captura + peers.toml
  keygen      PSK por par de peers (gera/recebe, distribui via scp)
  run         sobe o daemon em foreground
  service     instala unit systemd --user + linger
  doctor      diagnóstico do setup
  uninstall   remove unit, regra udev e ~/.config/kvmc-share

env:
  KVMC_BIN    caminho do binário kvmc-share (default: target/release/kvmc-share)
  NO_COLOR    desliga cor na saída
EOF
}

main() {
	case "${1:-}" in
	deps | config | keygen | run | service | doctor | uninstall) "cmd_${1}" ;;
	"") cmd_wizard ;;
	-h | --help) usage; exit 0 ;;
	*) usage; exit 2 ;;
	esac
}

main "$@"
