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

# --- Camada 1: helpers ------------------------------------------------------

_use_color() { [ -t 2 ] && [ -z "${NO_COLOR:-}" ]; }

_log() { # $1=código de cor ANSI  $2=símbolo  resto=mensagem
	local color="$1" sym="$2"
	shift 2
	if _use_color; then
		printf >&2 '\033[0;%sm%s\033[0m %s\n' "$color" "$sym" "$*"
	else
		printf >&2 '%s %s\n' "$sym" "$*"
	fi
}

log_info() { _log 34 '•' "$@"; }
log_ok() { _log 32 '✓' "$@"; }
log_warn() { _log 33 '!' "$@"; }
log_err() { _log 31 '✗' "$@"; }

die() {
	log_err "$@"
	exit 1
}

# confirm PERGUNTA — 0 se sim, 1 caso contrário. Default não.
# Em stdin não-interativo, loga e retorna 1 (assume não).
confirm() {
	local ans
	if [ ! -t 0 ]; then
		log_warn "não-interativo: assumindo 'não' para: $1"
		return 1
	fi
	read -r -p "$1 [s/N] " ans
	[[ "$ans" == [sSyY] || "$ans" == "sim" || "$ans" == "yes" ]]
}

# strip_ansi — remove sequências SGR (\e[...m) de stdin.
strip_ansi() { sed $'s/\x1b\\[[0-9;]*m//g'; }

# run_priv CMD... — executa via sudo, ecoando o comando antes.
# Sem sudo: imprime o comando e exige confirmação de que foi rodado como root.
run_priv() {
	if command -v sudo >/dev/null 2>&1; then
		printf >&2 '+ sudo %s\n' "$*"
		sudo "$@"
	else
		printf >&2 '+ %s\n' "$*"
		log_warn "sudo indisponível — rode o comando acima como root"
		confirm "já executei o comando acima como root?" ||
			die "passo privilegiado não confirmado"
	fi
}

# run_priv_tee ARQUIVO — grava stdin em ARQUIVO (root) via sudo tee.
run_priv_tee() {
	local dest="$1"
	if command -v sudo >/dev/null 2>&1; then
		printf >&2 '+ sudo tee %s\n' "$dest"
		sudo tee "$dest" >/dev/null
	else
		printf >&2 '+ escreva o conteúdo abaixo em %s (como root):\n' "$dest"
		cat >&2
		confirm "arquivo $dest criado como root?" ||
			die "escrita privilegiada não confirmada"
	fi
}

# tcp_probe HOST PORTA [TIMEOUT=3] — ecoa reachable|refused|timeout; 0 só em reachable.
tcp_probe() {
	local host="$1" port="$2" t="${3:-3}" rc
	if timeout "$t" bash -c "exec 3<>/dev/tcp/${host}/${port}" 2>/dev/null; then
		echo reachable
		return 0
	fi
	rc=$?
	if [ "$rc" -eq 124 ]; then
		echo timeout
		return 1
	fi
	if command -v nc >/dev/null 2>&1; then
		if nc -z -w "$t" "$host" "$port" >/dev/null 2>&1; then
			echo reachable
			return 0
		fi
	fi
	echo refused
	return 1
}

# backup_file ARQUIVO — copia para ARQUIVO.bak.<epoch>[-N] e ecoa o path do backup.
backup_file() {
	local f="$1" base dest n=0
	base="$f.bak.$(date +%s)"
	dest="$base"
	while [ -e "$dest" ]; do
		n=$((n + 1))
		dest="$base-$n"
	done
	cp -p -- "$f" "$dest"
	echo "$dest"
}

# psk_name A B — nome determinístico do arquivo PSK do par: "<menor>--<maior>".
psk_name() {
	local pair
	mapfile -t pair < <(printf '%s\n%s\n' "$1" "$2" | LC_ALL=C sort)
	printf '%s--%s' "${pair[0]}" "${pair[1]}"
}

# add_port_if_missing ADDR [PORTA] — anexa :PORTA se ADDR não trouxer porta.
# Preserva prefixo "user@" e literais IPv6 "[::1]".
add_port_if_missing() {
	local addr="$1" port="${2:-$SERVICE_PORT_DEFAULT}" userpart="" hostpart
	case "$addr" in
	*@*)
		userpart="${addr%@*}@"
		hostpart="${addr#*@}"
		;;
	*) hostpart="$addr" ;;
	esac
	case "$hostpart" in
	\[*\]) printf '%s%s:%s' "$userpart" "$hostpart" "$port" ;;
	*:*) printf '%s%s' "$userpart" "$hostpart" ;;
	*) printf '%s%s:%s' "$userpart" "$hostpart" "$port" ;;
	esac
}

# ini_get ARQUIVO CABECALHO CHAVE — 1º valor de CHAVE dentro da 1ª seção cujo
# cabeçalho literal seja CABECALHO (ex.: "[local]" ou "[[peer]]"). Best-effort
# (sem parser TOML): tira aspas e espaços. Vazio se não achar.
ini_get() {
	local file="$1" sec="$2" key="$3"
	awk -v sec="$sec" -v key="$key" '
		{ line = $0; sub(/[[:space:]]+$/, "", line) }
		line == sec { insec = 1; next }
		insec && substr(line, 1, 1) == "[" { insec = 0 }
		insec {
			n = index(line, "=")
			if (n == 0) next
			k = substr(line, 1, n - 1)
			gsub(/^[[:space:]]+|[[:space:]]+$/, "", k)
			if (k != key) next
			v = substr(line, n + 1)
			gsub(/^[[:space:]]+|[[:space:]]+$/, "", v)
			gsub(/^"|"$/, "", v)
			gsub(/^\047|\047$/, "", v)
			print v
			exit
		}
	' "$file"
}

# _bin_runs — 0 se $BIN existe, é executável e roda nesta arquitetura.
_bin_runs() {
	[ -x "$BIN" ] || return 1
	local rc=0
	"$BIN" --kvmc-probe >/dev/null 2>&1 || rc=$?
	[ "$rc" -ne 126 ] && [ "$rc" -ne 127 ]
}

# need_bin — garante um binário kvmc-share utilizável em $BIN.
# Ordem: cache -> KVMC_BIN -> binário já presente -> cargo build -> apt install
# cargo -> erro com link do rustup. Resultado cacheado em _BIN_OK.
need_bin() {
	[ -n "${_BIN_OK:-}" ] && return 0

	if [ -n "${KVMC_BIN:-}" ]; then
		[ -x "$KVMC_BIN" ] || die "KVMC_BIN=$KVMC_BIN não é um executável"
		BIN="$KVMC_BIN"
		_BIN_OK=1
		return 0
	fi

	if _bin_runs; then
		_BIN_OK=1
		return 0
	fi

	if [ ! -f "$REPO_ROOT/Cargo.toml" ]; then
		die "binário não encontrado e $REPO_ROOT não parece o repo do kvmc-share — rode de dentro do repo ou defina KVMC_BIN"
	fi

	if command -v cargo >/dev/null 2>&1; then
		log_info "compilando kvmc-share (cargo build --release)…"
		(cd "$REPO_ROOT" && cargo build --release)
		_bin_runs || die "compilei mas $BIN ainda não roda"
		_BIN_OK=1
		return 0
	fi

	if command -v apt >/dev/null 2>&1 && confirm "cargo não encontrado — instalar via apt?"; then
		run_priv apt install -y cargo
		command -v cargo >/dev/null 2>&1 || die "apt install cargo não resolveu"
		log_info "compilando kvmc-share (cargo build --release)…"
		(cd "$REPO_ROOT" && cargo build --release)
		_bin_runs || die "compilei mas $BIN ainda não roda"
		_BIN_OK=1
		return 0
	fi

	die "sem binário e sem cargo. Instale o Rust: https://rustup.rs"
}

# --- Camada 1b: parsers ---------------------------------------------------

# _res_grep MARCADOR — de stdin, ecoa "W H" do 1º token NxN na 1ª linha que
# contém a substring literal MARCADOR. Vazio se nada casar.
_res_grep() {
	awk -v mark="$1" '
		index($0, mark) {
			for (i = 1; i <= NF; i++)
				if ($i ~ /^[0-9]+x[0-9]+$/) {
					split($i, d, "x")
					print d[1], d[2]
					exit
				}
		}
	'
}

# detect_resolution — ecoa "W H" da saída ativa. Ordem: cosmic-randr (nativo do
# COSMIC, que não é wlroots) -> wlr-randr -> xrandr. Vazio se nenhuma detectar.
detect_resolution() {
	local out=""
	if command -v cosmic-randr >/dev/null 2>&1; then
		out=$(cosmic-randr list 2>/dev/null | strip_ansi | _res_grep '(current)')
	fi
	if [ -z "$out" ] && command -v wlr-randr >/dev/null 2>&1; then
		out=$(wlr-randr 2>/dev/null | strip_ansi | _res_grep 'current')
	fi
	if [ -z "$out" ] && command -v xrandr >/dev/null 2>&1; then
		out=$(xrandr 2>/dev/null | _res_grep '*')
	fi
	[ -n "$out" ] && printf '%s\n' "$out"
}

# validate_peers_toml ARQUIVO — checagem sintática leve (sem parser TOML):
# tem [local] com name/width/height/listen e um psk_path por [[peer]].
# 0 se ok; 1 + motivo em stderr caso contrário.
validate_peers_toml() {
	local f="$1" peers psks miss=() k
	[ -f "$f" ] || {
		echo "arquivo não existe: $f" >&2
		return 1
	}
	grep -qE '^\[local\][[:space:]]*$' "$f" || {
		echo "falta a seção [local]" >&2
		return 1
	}
	for k in name width height listen; do
		ini_get "$f" '[local]' "$k" | grep -q . || miss+=("local.$k")
	done
	peers=$(grep -cE '^\[\[peer\]\][[:space:]]*$' "$f" || true)
	psks=$(grep -cE '^[[:space:]]*psk_path[[:space:]]*=' "$f" || true)
	if [ "$peers" -ne "$psks" ]; then
		echo "número de [[peer]] ($peers) difere de psk_path ($psks)" >&2
		return 1
	fi
	if [ "${#miss[@]}" -gt 0 ]; then
		echo "campos ausentes em [local]: ${miss[*]}" >&2
		return 1
	fi
	return 0
}

# parse_input_devices — lê /proc/bus/input/devices (ou $DEVICES_FILE) e ecoa
# "eventN<TAB>Nome" para cada handler de evento.
parse_input_devices() {
	local f="${DEVICES_FILE:-/proc/bus/input/devices}"
	[ -f "$f" ] || return 1
	awk '
		/^N: Name=/ {
			name = $0
			sub(/^N: Name="/, "", name)
			sub(/"[[:space:]]*$/, "", name)
		}
		/^H: Handlers=/ {
			for (i = 1; i <= NF; i++)
				if ($i ~ /^event[0-9]+$/) print $i "\t" name
		}
	' "$f"
}

# _list_capture_devices — ecoa "path<TAB>nome" para cada symlink de teclado/mouse
# em /dev/input/by-id (override BYID_DIR). Nome resolvido via parse_input_devices.
_list_capture_devices() {
	local dir="${BYID_DIR:-/dev/input/by-id}" link ev name devmap
	[ -d "$dir" ] || return 1
	devmap="$(parse_input_devices || true)"
	for link in "$dir"/*-event-kbd "$dir"/*-event-mouse; do
		[ -e "$link" ] || continue
		ev="$(basename "$(readlink -f "$link")")"
		name="$(printf '%s\n' "$devmap" | awk -F'\t' -v e="$ev" '$1 == e { print $2; exit }')"
		printf '%s\t%s\n' "$link" "${name:-$ev}"
	done
}

# write_env DEVICES — grava KVMC_SHARE_DEVICES=... em $ENV_FILE (0600).
write_env() {
	mkdir -p "$CONF_DIR"
	printf 'KVMC_SHARE_DEVICES=%s\n' "$1" >"$ENV_FILE"
	chmod 600 "$ENV_FILE"
	log_ok "gravado $ENV_FILE"
}

# --- Subcomandos (stubs — preenchidos nas próximas tasks) --------------------

cmd_deps() {
	local relogar=0

	# --- grupo input (leitura de /dev/input/eventX) ---
	# getent = base (vale após relogin); `id -nG` sem arg = sessão atual.
	if getent group input | grep -qw "$USER"; then
		log_ok "usuário no grupo 'input' (base do sistema)"
		id -nG | grep -qw input || relogar=1
	else
		log_info "adicionando $USER ao grupo 'input'"
		run_priv usermod -aG input "$USER"
		relogar=1
	fi

	# --- regra udev p/ escrita em /dev/uinput ---
	if [ -e "$UDEV_RULE" ] && [ "$(cat "$UDEV_RULE")" = "$UDEV_LINE" ]; then
		log_ok "regra udev de /dev/uinput já instalada"
	else
		log_info "instalando regra udev em $UDEV_RULE"
		printf '%s\n' "$UDEV_LINE" | run_priv_tee "$UDEV_RULE"
		run_priv udevadm control --reload-rules
		run_priv udevadm trigger
	fi
	if [ "$(stat -c '%a %G' /dev/uinput 2>/dev/null || true)" = "660 input" ]; then
		log_ok "/dev/uinput gravável pelo grupo 'input'"
	else
		log_warn "/dev/uinput ainda não está 660 root:input — confira 'sudo modprobe uinput' e replug/reboot p/ a regra valer"
	fi

	# --- binário ---
	need_bin
	log_ok "binário: $BIN"

	if [ "$relogar" -eq 1 ]; then
		log_warn "faça logout/login (ou reboot) antes de 'run' — o grupo 'input' só vale em sessão nova"
	fi
}
# select_devices — menu de teclado/mouse, pré-seleção do 1º de cada, múltipla
# escolha; valida leitura; persiste KVMC_SHARE_DEVICES.
select_devices() {
	local -a cand=() sel=()
	mapfile -t cand < <(_list_capture_devices || true)

	if [ "${#cand[@]}" -eq 0 ]; then
		log_warn "nada em /dev/input/by-id/ — informe paths de /dev/input/event* à mão"
		local -a manual=()
		read -r -p "paths (espaço-separados): " -a manual
		sel=("${manual[@]}")
	else
		local i path name first_kbd="" first_mouse=""
		for i in "${!cand[@]}"; do
			path="${cand[$i]%%$'\t'*}"
			name="${cand[$i]#*$'\t'}"
			printf '  [%d] %s\n      %s\n' "$((i + 1))" "$path" "$name"
			case "$path" in
			*-event-kbd) [ -z "$first_kbd" ] && first_kbd="$path" ;;
			*-event-mouse) [ -z "$first_mouse" ] && first_mouse="$path" ;;
			esac
		done

		local -a def_idx=()
		for i in "${!cand[@]}"; do
			path="${cand[$i]%%$'\t'*}"
			[ "$path" = "$first_kbd" ] && def_idx+=("$((i + 1))")
			[ "$path" = "$first_mouse" ] && def_idx+=("$((i + 1))")
		done

		local reply
		read -r -p "números a capturar (espaço-separados) [${def_idx[*]}]: " reply
		[ -z "$reply" ] && reply="${def_idx[*]}"
		local -a nums=()
		read -ra nums <<<"$reply"
		local n
		for n in "${nums[@]}"; do
			if [ "$n" -ge 1 ] 2>/dev/null && [ "$n" -le "${#cand[@]}" ]; then
				sel+=("${cand[$((n - 1))]%%$'\t'*}")
			else
				log_warn "índice inválido ignorado: $n"
			fi
		done
	fi

	[ "${#sel[@]}" -eq 0 ] && die "nenhum dispositivo selecionado"

	local p bad=0
	for p in "${sel[@]}"; do
		[ -r "$p" ] || {
			log_warn "sem permissão de leitura: $p  (rode 'deps' e relogue)"
			bad=1
		}
	done
	[ "$bad" -eq 1 ] && log_warn "prossigo; ajuste as permissões antes do 'run'"

	local joined
	joined="$(printf '%s:' "${sel[@]}")"
	write_env "${joined%:}"
}

cmd_config() {
	select_devices
}
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

if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then
	main "$@"
fi
