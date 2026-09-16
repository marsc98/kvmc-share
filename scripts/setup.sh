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

# Regras geradas pra periféricos sem symlink nativo em /dev/input/by-id
# (ex: mouse/teclado Bluetooth) — uma linha por dispositivo, chaveada por
# ATTRS{uniq} (MAC/serial). Ver ensure_stable_device_path.
PERIPHERALS_RULES_FILE="/etc/udev/rules.d/99-kvmc-share-peripherals.rules"

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

# run_priv_tee_append ARQUIVO — acrescenta stdin (uma linha) a ARQUIVO (root)
# via sudo tee -a. Mesmo padrão de run_priv_tee, mas sem truncar o arquivo.
run_priv_tee_append() {
	local dest="$1"
	if command -v sudo >/dev/null 2>&1; then
		printf >&2 '+ sudo tee -a %s\n' "$dest"
		sudo tee -a "$dest" >/dev/null
	else
		printf >&2 '+ acrescente a linha abaixo em %s (como root):\n' "$dest"
		cat >&2
		confirm "linha adicionada em $dest como root?" ||
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

# _peer_list ARQUIVO — ecoa "name<TAB>addr" para cada bloco [[peer]].
_peer_list() {
	awk '
		function flush() { if (name != "") print name "\t" addr; name = ""; addr = "" }
		{ line = $0; sub(/[[:space:]]+$/, "", line) }
		line == "[[peer]]" { flush(); inpeer = 1; next }
		inpeer && substr(line, 1, 1) == "[" { flush(); inpeer = 0 }
		inpeer {
			n = index(line, "=")
			if (n == 0) next
			k = substr(line, 1, n - 1); gsub(/^[[:space:]]+|[[:space:]]+$/, "", k)
			v = substr(line, n + 1); gsub(/^[[:space:]]+|[[:space:]]+$/, "", v)
			gsub(/^"|"$/, "", v)
			if (k == "name") name = v
			else if (k == "addr") addr = v
		}
		END { flush() }
	' "$1"
}

# _classify_input_device EVENTNODE — ecoa "kbd"/"mouse" via tags ID_INPUT_*
# do udev; vazio se não for nenhum dos dois (ou se udevadm falhar). Em teste,
# UDEV_PROPS_DIR/$EVENTNODE substitui a chamada real a udevadm (evita tocar
# em dispositivos reais).
_classify_input_device() {
	local ev="$1" props
	if [ -n "${UDEV_PROPS_DIR:-}" ]; then
		props="$(cat "$UDEV_PROPS_DIR/$ev" 2>/dev/null || true)"
	else
		props="$(udevadm info --query=property --name="/dev/input/$ev" 2>/dev/null)" || return 0
	fi
	if printf '%s\n' "$props" | grep -q '^ID_INPUT_KEYBOARD=1'; then
		echo kbd
	elif printf '%s\n' "$props" | grep -q '^ID_INPUT_MOUSE=1'; then
		echo mouse
	fi
}

# _byid_for_event EVENTNODE DIR — ecoa o symlink em DIR que aponta pra esse
# eventN, se existir (falha se não achar nenhum).
_byid_for_event() {
	local ev="$1" dir="$2" link
	for link in "$dir"/*-event-kbd "$dir"/*-event-mouse; do
		[ -e "$link" ] || continue
		[ "$(basename "$(readlink -f "$link")")" = "$ev" ] && {
			echo "$link"
			return 0
		}
	done
	return 1
}

# _list_capture_devices — ecoa "path<TAB>nome<TAB>kbd|mouse" para cada
# teclado/mouse detectado via ID_INPUT_KEYBOARD/MOUSE do udev (não só os que
# têm symlink em /dev/input/by-id — cobre Bluetooth e outros sem by-id
# nativo). Prefere o symlink estável quando existe; cai pro /dev/input/eventN
# cru quando não — select_devices trata esse caso via ensure_stable_device_path.
_list_capture_devices() {
	local dir="${BYID_DIR:-/dev/input/by-id}" devmap ev name kind path
	devmap="$(parse_input_devices || true)"
	[ -n "$devmap" ] || return 1
	while IFS=$'\t' read -r ev name; do
		[ -n "$ev" ] || continue
		kind="$(_classify_input_device "$ev")"
		[ -n "$kind" ] || continue
		path="$(_byid_for_event "$ev" "$dir" || true)"
		[ -n "$path" ] || path="/dev/input/$ev"
		printf '%s\t%s\t%s\n' "$path" "${name:-$ev}" "$kind"
	done <<<"$devmap"
}

# _stable_uniq_for DEV — ecoa o ATTRS{uniq} (MAC/serial) do dispositivo mais
# próximo na árvore sysfs, se houver.
_stable_uniq_for() {
	local dev="$1"
	udevadm info -a --name="$dev" 2>/dev/null |
		grep -m1 'ATTRS{uniq}==' | sed -E 's/.*=="([^"]*)".*/\1/'
}

# _slugify NOME — normaliza pra usar em symlink/nome de arquivo (minúsculas,
# só a-z0-9-, sem repetição/bordas de hífen).
_slugify() {
	printf '%s\n' "$1" | tr '[:upper:] ' '[:lower:]-' | tr -cd 'a-z0-9-' |
		sed -E 's/-+/-/g; s/^-|-$//g'
}

# ensure_stable_device_path DEV NOME — se DEV já é um symlink em by-id, ecoa
# sem mudar nada. Senão (ex: Bluetooth sem by-id nativo), tenta gerar uma
# regra udev chaveada por ATTRS{uniq} em $PERIPHERALS_RULES_FILE e ecoa o novo
# symlink persistente. Sem uniq disponível: avisa e ecoa o path original
# (instável entre reconexões/reboots — limite conhecido, sem stable id).
ensure_stable_device_path() {
	local dev="$1" name="$2" uniq slug link_name line
	case "$dev" in
	*/by-id/*)
		echo "$dev"
		return 0
		;;
	esac

	uniq="$(_stable_uniq_for "$dev")"
	if [ -z "$uniq" ]; then
		log_warn "$name sem identificador estável (ATTRS{uniq} vazio) — usando $dev direto; o número pode mudar após reconexão/reboot"
		echo "$dev"
		return 0
	fi

	slug="$(_slugify "$name")"
	link_name="kvmc-${slug:-periferico}"
	line="SUBSYSTEM==\"input\", ATTRS{uniq}==\"$uniq\", ENV{ID_INPUT}==\"1\", SYMLINK+=\"input/by-id/$link_name\""

	if [ -e "$PERIPHERALS_RULES_FILE" ] && grep -qF "$uniq" "$PERIPHERALS_RULES_FILE"; then
		log_ok "regra udev pra $name já existe em $PERIPHERALS_RULES_FILE"
	else
		log_info "gerando regra udev persistente pra $name (sem by-id nativo)"
		printf '%s\n' "$line" | run_priv_tee_append "$PERIPHERALS_RULES_FILE"
		run_priv udevadm control --reload-rules
		run_priv udevadm trigger --action=add --subsystem-match=input
	fi

	echo "/dev/input/by-id/$link_name"
}

# validate_device_live DEV NOME [TIMEOUT=5] — pede pro usuário mexer/apertar
# DEV e confirma recebimento real de bytes (não só permissão de leitura).
# 0 se recebeu algo dentro do TIMEOUT, 1 caso contrário.
validate_device_live() {
	local dev="$1" name="$2" t="${3:-5}"
	printf >&2 '  mexa/aperte "%s" agora (%ss)... ' "$name" "$t"
	if timeout "$t" head -c 24 "$dev" >/dev/null 2>&1; then
		printf >&2 'ok\n'
		return 0
	fi
	printf >&2 'nada recebido\n'
	return 1
}

# write_env DEVICES — grava KVMC_SHARE_DEVICES=... em $ENV_FILE (0600).
write_env() {
	mkdir -p "$CONF_DIR"
	printf 'KVMC_SHARE_DEVICES=%s\n' "$1" >"$ENV_FILE"
	chmod 600 "$ENV_FILE"
	log_ok "gravado $ENV_FILE"
}

# --- Subcomandos (stubs — preenchidos nas próximas tasks) --------------------

# ensure_global_cli BIN — verifica se 'kvmc-share' está disponível
# globalmente (achável sem `./target/release/...` ou `KVMC_BIN=`) e, se não
# estiver (ou estiver desatualizado em relação a BIN), pergunta antes de
# copiar pra CARGO_BIN_DIR (default ~/.cargo/bin — já no PATH em instalações
# padrão do rustup). Em teste, CARGO_BIN_DIR restringe a busca a esse dir,
# ignorando o PATH real (evita pegar um kvmc-share já instalado na máquina).
ensure_global_cli() {
	local bin="$1" cargo_bin_dir="${CARGO_BIN_DIR:-$HOME/.cargo/bin}" target lookup=""
	target="$cargo_bin_dir/kvmc-share"

	if [ -n "${CARGO_BIN_DIR:-}" ]; then
		[ -x "$target" ] && lookup="$target"
	else
		lookup="$(command -v kvmc-share 2>/dev/null || true)"
	fi

	if [ -n "$lookup" ] && cmp -s "$lookup" "$bin" 2>/dev/null; then
		log_ok "kvmc-share global já atualizado ($lookup)"
		return 0
	fi

	if [ -n "$lookup" ]; then
		confirm "kvmc-share global em $lookup parece desatualizado — atualizar?" ||
			{
				log_info "mantendo $lookup como está"
				return 0
			}
	else
		confirm "kvmc-share não está disponível globalmente — copiar pra $target?" ||
			{
				log_info "kvmc-share só disponível via $bin"
				return 0
			}
	fi

	mkdir -p "$cargo_bin_dir"
	cp "$bin" "$target"
	log_ok "kvmc-share instalado em $target"

	case ":$PATH:" in
	*":$cargo_bin_dir:"*) ;;
	*)
		log_warn "$cargo_bin_dir não está no \$PATH — 'kvmc-share' não vai ser achado até você adicionar (ex: 'export PATH=\"\$HOME/.cargo/bin:\$PATH\"' no seu .bashrc/.zshrc)"
		;;
	esac
}

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
	ensure_global_cli "$BIN"

	if [ "$relogar" -eq 1 ]; then
		log_warn "faça logout/login (ou reboot) antes de 'run' — o grupo 'input' só vale em sessão nova"
	fi
}
# select_devices — menu de teclado/mouse (inclui os sem by-id nativo,
# ex: Bluetooth), pré-seleção do 1º de cada tipo, múltipla escolha; valida
# permissão + captura ao vivo de cada um; resolve path estável (gerando regra
# udev quando falta); persiste KVMC_SHARE_DEVICES.
select_devices() {
	local -a cand=() sel_path=() sel_name=()
	mapfile -t cand < <(_list_capture_devices || true)

	if [ "${#cand[@]}" -eq 0 ]; then
		log_warn "nenhum teclado/mouse detectado via udev — informe paths de /dev/input/event* à mão"
		local -a manual=()
		read -r -p "paths (espaço-separados): " -a manual
		local m
		for m in "${manual[@]}"; do
			sel_path+=("$m")
			sel_name+=("$m")
		done
	else
		local i path name kind first_kbd="" first_mouse=""
		for i in "${!cand[@]}"; do
			IFS=$'\t' read -r path name kind <<<"${cand[$i]}"
			printf '  [%d] %s\n      %s (%s)\n' "$((i + 1))" "$path" "$name" "$kind"
			[ "$kind" = kbd ] && [ -z "$first_kbd" ] && first_kbd="$((i + 1))"
			[ "$kind" = mouse ] && [ -z "$first_mouse" ] && first_mouse="$((i + 1))"
		done

		local -a def_idx=()
		[ -n "$first_kbd" ] && def_idx+=("$first_kbd")
		[ -n "$first_mouse" ] && def_idx+=("$first_mouse")

		local reply
		read -r -p "números a capturar (espaço-separados) [${def_idx[*]}]: " reply
		[ -z "$reply" ] && reply="${def_idx[*]}"
		local -a nums=()
		read -ra nums <<<"$reply"
		local n
		for n in "${nums[@]}"; do
			if [ "$n" -ge 1 ] 2>/dev/null && [ "$n" -le "${#cand[@]}" ]; then
				IFS=$'\t' read -r path name kind <<<"${cand[$((n - 1))]}"
				sel_path+=("$path")
				sel_name+=("$name")
			else
				log_warn "índice inválido ignorado: $n"
			fi
		done
	fi

	[ "${#sel_path[@]}" -eq 0 ] && die "nenhum dispositivo selecionado"

	local p bad=0
	for p in "${sel_path[@]}"; do
		[ -r "$p" ] || {
			log_warn "sem permissão de leitura: $p  (rode 'deps' e relogue)"
			bad=1
		}
	done
	[ "$bad" -eq 1 ] && log_warn "prossigo; ajuste as permissões antes do 'run'"

	# validação ao vivo, sequencial, uma por uma — bloqueia em falha e deixa
	# tentar de novo ou remover da seleção, em vez de persistir um device errado.
	local idx=0
	while [ "$idx" -lt "${#sel_path[@]}" ]; do
		p="${sel_path[$idx]}"
		name="${sel_name[$idx]}"
		if validate_device_live "$p" "$name"; then
			idx=$((idx + 1))
			continue
		fi
		log_warn "não recebi eventos de '$name' ($p)"
		read -r -p "  [t]entar de novo / [r]emover da seleção: " ans
		case "$ans" in
		r | R)
			sel_path=("${sel_path[@]:0:$idx}" "${sel_path[@]:$((idx + 1))}")
			sel_name=("${sel_name[@]:0:$idx}" "${sel_name[@]:$((idx + 1))}")
			;;
		*) : ;; # tenta de novo no mesmo índice
		esac
	done

	[ "${#sel_path[@]}" -eq 0 ] && die "nenhum dispositivo restou após a validação"

	local -a resolved=()
	for i in "${!sel_path[@]}"; do
		resolved+=("$(ensure_stable_device_path "${sel_path[$i]}" "${sel_name[$i]}")")
	done

	local joined
	joined="$(printf '%s:' "${resolved[@]}")"
	write_env "${joined%:}"
}

_in_list() {
	local x="$1" e
	shift
	for e in "$@"; do [ "$e" = "$x" ] && return 0; done
	return 1
}

# collect_local — pergunta [local]; ecoa "name<TAB>width<TAB>height<TAB>listen".
collect_local() {
	local def_name def_listen name listen res w h ans
	def_name="$(ini_get "$PEERS_TOML" '[local]' name 2>/dev/null || true)"
	[ -n "$def_name" ] || def_name="$(hostname -s)"
	read -r -p "nome desta máquina [$def_name]: " name || true
	name="${name:-$def_name}"

	def_listen="$(ini_get "$PEERS_TOML" '[local]' listen 2>/dev/null || true)"
	[ -n "$def_listen" ] || def_listen="0.0.0.0:$SERVICE_PORT_DEFAULT"
	read -r -p "endereço de escuta [$def_listen]: " listen || true
	listen="${listen:-$def_listen}"

	res="$(detect_resolution || true)"
	if [ -n "$res" ]; then
		w="${res% *}"
		h="${res#* }"
		read -r -p "resolução detectada ${w}x${h} — confirmar? [S/n]: " ans || true
		[[ "$ans" == [nN]* ]] && res=""
	fi
	if [ -z "$res" ]; then
		read -r -p "largura (px): " w || die "entrada interrompida"
		read -r -p "altura (px): " h || die "entrada interrompida"
	fi

	printf '%s\t%s\t%s\t%s\n' "$name" "$w" "$h" "$listen"
}

# collect_peers LOCAL_NAME — pergunta N vizinhos; ecoa uma linha
# "name<TAB>addr<TAB>direction" por peer. Rejeita nome/direção repetidos.
collect_peers() {
	local lname="$1" n i pname paddr pdir
	local -a used_dirs=() used_names=()
	read -r -p "quantos vizinhos imediatos? [1]: " n || true
	n="${n:-1}"
	for ((i = 1; i <= n; i++)); do
		printf '— peer %d/%d —\n' "$i" "$n" >&2
		while :; do
			read -r -p "  nome: " pname || die "entrada interrompida"
			[ -z "$pname" ] && {
				echo "  nome vazio" >&2
				continue
			}
			[ "$pname" = "$lname" ] && {
				echo "  não pode ser o nome desta máquina" >&2
				continue
			}
			_in_list "$pname" ${used_names[@]+"${used_names[@]}"} && {
				echo "  nome repetido" >&2
				continue
			}
			break
		done
		read -r -p "  host[:porta]: " paddr || die "entrada interrompida"
		paddr="$(add_port_if_missing "$paddr")"
		while :; do
			read -r -p "  direção (left/right/up/down): " pdir || die "entrada interrompida"
			case "$pdir" in
			left | right | up | down) ;;
			*)
				echo "  direção inválida" >&2
				continue
				;;
			esac
			_in_list "$pdir" ${used_dirs[@]+"${used_dirs[@]}"} && {
				echo "  direção repetida" >&2
				continue
			}
			break
		done
		used_names+=("$pname")
		used_dirs+=("$pdir")
		printf '%s\t%s\t%s\n' "$pname" "$paddr" "$pdir"
	done
}

# write_peers_toml LNAME W H LISTEN  PEER_SPEC...
# PEER_SPEC = "name<TAB>addr<TAB>direction". Faz backup se já existir.
write_peers_toml() {
	local lname="$1" w="$2" h="$3" listen="$4"
	shift 4
	mkdir -p "$CONF_DIR"
	[ -e "$PEERS_TOML" ] && log_info "backup: $(backup_file "$PEERS_TOML")"
	{
		printf '[local]\nname = "%s"\nwidth = %s\nheight = %s\nlisten = "%s"\n' \
			"$lname" "$w" "$h" "$listen"
		local spec pname paddr pdir
		for spec in "$@"; do
			IFS=$'\t' read -r pname paddr pdir <<<"$spec"
			printf '\n[[peer]]\nname = "%s"\naddr = "%s"\npsk_path = "~/.config/kvmc-share/peers/%s.psk"\ndirection = "%s"\n' \
				"$pname" "$paddr" "$(psk_name "$lname" "$pname")" "$pdir"
		done
	} >"$PEERS_TOML"
	log_ok "gravado $PEERS_TOML"
}

cmd_config() {
	select_devices

	local lname w h listen
	IFS=$'\t' read -r lname w h listen < <(collect_local)

	local -a peerspecs=()
	mapfile -t peerspecs < <(collect_peers "$lname")
	[ "${#peerspecs[@]}" -eq 0 ] && die "nenhum peer configurado"

	write_peers_toml "$lname" "$w" "$h" "$listen" "${peerspecs[@]}"
	validate_peers_toml "$PEERS_TOML" || die "peers.toml gerado é inválido"
}
# _keygen_crosscheck PSK_FILE HOST BASE — compara sha256 local x remoto via SSH.
_keygen_crosscheck() {
	local psk_file="$1" host="$2" base="$3" local_sum remote_sum
	command -v ssh >/dev/null 2>&1 || return 0
	local_sum="$(sha256sum "$psk_file" | cut -d' ' -f1)"
	remote_sum="$(ssh -o BatchMode=yes -o ConnectTimeout=5 "$host" \
		"sha256sum ~/.config/kvmc-share/peers/$base 2>/dev/null | cut -d' ' -f1" 2>/dev/null || true)"
	if [ -z "$remote_sum" ]; then
		log_info "cross-check pulado (sem SSH sem senha para $host)"
	elif [ "$local_sum" = "$remote_sum" ]; then
		log_ok "sha256 confere nos dois lados"
	else
		log_warn "sha256 DIVERGE — local $local_sum vs remoto $remote_sum"
	fi
}

# _keygen_peer LNAME PNAME PADDR — trata a PSK de um par (papel gero/recebo).
_keygen_peer() {
	local lname="$1" pname="$2" paddr="$3" psk_file host base role
	psk_file="$PSK_DIR/$(psk_name "$lname" "$pname").psk"
	host="${paddr%:*}"
	base="$(basename "$psk_file")"

	printf '\n=== par %s <-> %s ===\n' "$lname" "$pname" >&2
	read -r -p "papel neste par? [g]ero / [r]ecebo: " role || true

	case "$role" in
	r | recebo)
		if [ -s "$psk_file" ] && [ "$(stat -c%s "$psk_file")" -eq 32 ]; then
			log_ok "PSK presente: $psk_file"
			printf 'sha256: %s\n' "$(sha256sum "$psk_file" | cut -d' ' -f1)" >&2
		else
			die "falta $psk_file (32 bytes) — rode 'keygen' no papel 'gero' em $pname"
		fi
		;;
	*)
		if [ -e "$psk_file" ] && ! confirm "PSK $psk_file já existe — sobrescrever?"; then
			log_info "mantida a PSK existente"
		else
			mkdir -p "$PSK_DIR"
			(
				umask 077
				head -c 32 /dev/urandom >"$psk_file.tmp"
			)
			if [ "$(stat -c%s "$psk_file.tmp" 2>/dev/null || echo 0)" -ne 32 ]; then
				rm -f "$psk_file.tmp"
				die "leitura de /dev/urandom devolveu menos de 32 bytes"
			fi
			mv "$psk_file.tmp" "$psk_file"
			chmod 600 "$psk_file"
			log_ok "PSK gerada: $psk_file"

			if ssh "$host" 'mkdir -p ~/.config/kvmc-share/peers' &&
				scp "$psk_file" "$host:.config/kvmc-share/peers/$base"; then
				log_ok "PSK copiada para $host"
			else
				log_warn "scp falhou — copie manualmente:"
				printf "  ssh %s 'mkdir -p ~/.config/kvmc-share/peers'\n" "$host" >&2
				printf '  scp %s %s:.config/kvmc-share/peers/%s\n' "$psk_file" "$host" "$base" >&2
				read -r -p "  copiei — Enter para seguir " _ || true
			fi
		fi
		;;
	esac

	_keygen_crosscheck "$psk_file" "$host" "$base"
}

cmd_keygen() {
	[ -f "$PEERS_TOML" ] || die "sem $PEERS_TOML — rode 'config' antes"
	local lname pname paddr
	lname="$(ini_get "$PEERS_TOML" '[local]' name)"
	[ -n "$lname" ] || die "não achei [local].name em $PEERS_TOML"

	while IFS=$'\t' read -r pname paddr; do
		[ -n "$pname" ] || continue
		_keygen_peer "$lname" "$pname" "$paddr"
	done < <(_peer_list "$PEERS_TOML")
}
cmd_run() {
	[ -e "$ENV_FILE" ] || die "sem $ENV_FILE — rode 'config' antes"
	need_bin
	set -a
	# shellcheck source=/dev/null
	. "$ENV_FILE"
	set +a
	log_info "kvmc-share run  (Ctrl-C encerra)"
	exec "$BIN" run
}
# _exec_start CAMINHO — ecoa o caminho com %h no lugar de $HOME quando aplicável.
_exec_start() {
	case "$1" in
	"$HOME"/*) printf '%%h/%s' "${1#"$HOME"/}" ;;
	*) printf '%s' "$1" ;;
	esac
}

# _service_unit EXECSTART — ecoa o conteúdo do unit systemd --user.
_service_unit() {
	cat <<EOF
[Unit]
Description=kvmc-share input/clipboard mesh
After=network.target

[Service]
Type=simple
ExecStart=$1 run
EnvironmentFile=%h/.config/kvmc-share/env
Restart=on-failure
RestartSec=2

[Install]
WantedBy=default.target
EOF
}

cmd_service() {
	systemctl --user show-environment >/dev/null 2>&1 ||
		die "sem bus de usuário (systemctl --user indisponível) — use 'run' em foreground ou uma sessão gráfica"
	need_bin

	local content
	content="$(_service_unit "$(_exec_start "$BIN")")"

	if [ -f "$UNIT" ] && [ "$(cat "$UNIT")" = "$content" ]; then
		log_ok "unit já atualizado: $UNIT"
	else
		mkdir -p "$(dirname "$UNIT")"
		printf '%s\n' "$content" >"$UNIT"
		log_ok "gravado $UNIT"
		systemctl --user daemon-reload
	fi

	systemctl --user enable --now kvmc-share.service
	loginctl enable-linger "$USER" 2>/dev/null ||
		log_warn "não consegui habilitar linger — o serviço só sobe com sessão aberta"
	systemctl --user status kvmc-share.service --no-pager || true
}
cmd_doctor() {
	local fail=0

	# (a) binário
	if _bin_runs; then
		log_ok "binário: $BIN"
	else
		log_err "binário ausente/inválido em $BIN — rode 'deps'"
		fail=1
	fi

	# (b) grupo input
	if id -nG | grep -qw input; then
		log_ok "grupo 'input' ativo na sessão"
	elif getent group input | grep -qw "$USER"; then
		log_err "grupo 'input' na base mas não na sessão — faça logout/login"
		fail=1
	else
		log_err "usuário fora do grupo 'input' — rode 'deps'"
		fail=1
	fi

	# (c) /dev/uinput gravável
	if [ -w /dev/uinput ]; then
		log_ok "/dev/uinput gravável"
	else
		log_err "/dev/uinput não gravável — rode 'deps' e relogue"
		fail=1
	fi

	# (d) env + dispositivos
	if [ -e "$ENV_FILE" ]; then
		local devline dev missing=0
		devline="$(grep -E '^KVMC_SHARE_DEVICES=' "$ENV_FILE" | head -1 | cut -d= -f2-)"
		if [ -z "$devline" ]; then
			log_err "$ENV_FILE sem KVMC_SHARE_DEVICES — rode 'config'"
			fail=1
		else
			local -a devs=()
			IFS=: read -ra devs <<<"$devline"
			for dev in "${devs[@]}"; do
				{ [ -e "$dev" ] && [ -r "$dev" ]; } || {
					log_warn "  dispositivo inacessível: $dev"
					missing=1
				}
			done
			if [ "$missing" -eq 0 ]; then
				log_ok "dispositivos de captura acessíveis (${#devs[@]})"
			else
				log_err "dispositivos inacessíveis — rode 'deps'/'config'"
				fail=1
			fi
		fi
	else
		log_err "sem $ENV_FILE — rode 'config'"
		fail=1
	fi

	# (e) peers.toml
	local lname=""
	if [ -e "$PEERS_TOML" ] && validate_peers_toml "$PEERS_TOML" 2>/dev/null; then
		log_ok "peers.toml válido"
		lname="$(ini_get "$PEERS_TOML" '[local]' name)"
	else
		log_err "peers.toml ausente/inválido — rode 'config'"
		fail=1
	fi

	# (f) PSKs
	if [ -n "$lname" ]; then
		local pname paddr pskf pfail=0
		while IFS=$'\t' read -r pname paddr; do
			[ -n "$pname" ] || continue
			pskf="$PSK_DIR/$(psk_name "$lname" "$pname").psk"
			if [ -s "$pskf" ] && [ "$(stat -c%s "$pskf")" -eq 32 ]; then
				log_ok "PSK $pname: ok"
			else
				log_err "PSK $pname ausente/tamanho != 32 ($pskf) — rode 'keygen'"
				pfail=1
			fi
		done < <(_peer_list "$PEERS_TOML")
		[ "$pfail" -eq 1 ] && fail=1
	fi

	# (g) serviço (forte, mas não conta pro exit)
	if systemctl --user is-active --quiet kvmc-share.service 2>/dev/null; then
		log_ok "serviço systemd --user ativo"
	else
		log_info "serviço systemd --user inativo (use 'service' ou 'run')"
	fi

	# (h) conectividade por peer (informativo)
	if [ -e "$PEERS_TOML" ]; then
		local pname paddr host port st
		while IFS=$'\t' read -r pname paddr; do
			[ -n "$pname" ] || continue
			host="${paddr%:*}"
			port="${paddr##*:}"
			host="${host##*@}"
			st="$(tcp_probe "$host" "$port" 3 || true)"
			log_info "peer $pname ($host:$port): $st"
		done < <(_peer_list "$PEERS_TOML")
	fi

	# (i) clipboard (puramente informativo)
	if [ -z "${XDG_RUNTIME_DIR:-}" ]; then
		log_info "clipboard: indeterminado (XDG_RUNTIME_DIR não definido)"
	elif [ -S "$XDG_RUNTIME_DIR/copied.sock" ] && pgrep -x copied >/dev/null 2>&1; then
		log_ok "clipboard: copied ativo"
	else
		log_info "clipboard: copied inativo (opcional) — https://github.com/marsc98/copied"
	fi

	if [ "$fail" -eq 0 ]; then
		log_ok "tudo pronto"
	else
		log_err "itens obrigatórios pendentes"
	fi
	return "$fail"
}
cmd_uninstall() {
	confirm "remover unit systemd, regra udev e $CONF_DIR?" || {
		log_info "cancelado"
		return 0
	}
	if compgen -G "$PSK_DIR/*.psk" >/dev/null 2>&1; then
		confirm "há PSKs em $PSK_DIR — apagar as chaves também?" || {
			log_info "cancelado"
			return 0
		}
	fi

	if [ -e "$UNIT" ]; then
		systemctl --user disable --now kvmc-share.service 2>/dev/null || true
		rm -f "$UNIT"
		systemctl --user daemon-reload 2>/dev/null || true
		log_ok "unit removido"
	else
		log_info "unit já ausente"
	fi

	if [ -e "$UDEV_RULE" ]; then
		run_priv rm -f "$UDEV_RULE"
		run_priv udevadm control --reload-rules
		log_ok "regra udev removida"
	else
		log_info "regra udev já ausente"
	fi

	if [ -e "$PERIPHERALS_RULES_FILE" ]; then
		run_priv rm -f "$PERIPHERALS_RULES_FILE"
		run_priv udevadm control --reload-rules
		log_ok "regras de periféricos removidas"
	else
		log_info "regras de periféricos já ausentes"
	fi

	if [ -e "$CONF_DIR" ]; then
		rm -rf "$CONF_DIR"
		log_ok "$CONF_DIR removido"
	else
		log_info "$CONF_DIR já ausente"
	fi

	log_warn "mantidos: usuário no grupo 'input' e enable-linger — remova à mão se quiser"
}
# --- wizard --------------------------------------------------------

_deps_done() { _bin_runs && id -nG | grep -qw input && [ -w /dev/uinput ]; }

_config_done() {
	[ -e "$ENV_FILE" ] && [ -e "$PEERS_TOML" ] &&
		validate_peers_toml "$PEERS_TOML" 2>/dev/null
}

_keygen_done() {
	[ -e "$PEERS_TOML" ] || return 1
	local lname pname rest pskf
	lname="$(ini_get "$PEERS_TOML" '[local]' name)"
	[ -n "$lname" ] || return 1
	while IFS=$'\t' read -r pname rest; do
		[ -n "$pname" ] || continue
		pskf="$PSK_DIR/$(psk_name "$lname" "$pname").psk"
		{ [ -s "$pskf" ] && [ "$(stat -c%s "$pskf")" -eq 32 ]; } || return 1
	done < <(_peer_list "$PEERS_TOML")
	return 0
}

# _wizard_step NOME DESC DONECHECK — roda cmd_NOME, pulando se DONECHECK passa
# e o usuário não quiser refazer.
_wizard_step() {
	local name="$1" desc="$2" donecheck="$3"
	printf '\n=== %s: %s ===\n' "$name" "$desc" >&2
	if "$donecheck" && ! confirm "'$name' já parece pronto — refazer?"; then
		log_info "pulando '$name'"
		return 0
	fi
	"cmd_$name"
}

cmd_wizard() {
	log_info "kvmc-share — assistente de configuração"

	_wizard_step deps "grupo input, regra udev, binário" _deps_done
	_wizard_step config "dispositivos + peers.toml" _config_done
	_wizard_step keygen "PSK dos pares" _keygen_done

	local choice
	read -r -p "subir agora? [f]oreground / [s]erviço systemd / [n]ada: " choice || true
	case "$choice" in
	f | foreground) cmd_run ;;
	s | servico | serviço | service) cmd_service ;;
	*) log_info "depois rode 'setup.sh run' ou 'setup.sh service'" ;;
	esac

	log_info "clipboard: o 'copied' é opcional e não foi configurado (veja o README)"
}

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
