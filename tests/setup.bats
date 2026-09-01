#!/usr/bin/env bats
#
# Testes de scripts/setup.sh — cobrem só funções puras (sem efeito no sistema).
# Partes que mexem em sudo/udev/systemd/ssh têm verificação manual documentada
# nos blocos "Verify" de .specs/features/setup-script/tasks.md.

SETUP="${BATS_TEST_DIRNAME}/../scripts/setup.sh"
FIXTURES="${BATS_TEST_DIRNAME}/fixtures"

# Roda `func args...` num bash isolado que só faz source do script (o guard de
# BASH_SOURCE impede a execução de main). Popula $status e $output.
call() {
	run bash -c 'source "$1"; shift; "$@"' _ "$SETUP" "$@"
}

# --- dispatch (smoke) ------------------------------------------------------

@test "smoke: --help lista subcomandos e sai 0" {
	run bash "$SETUP" --help
	[ "$status" -eq 0 ]
	[[ "$output" == *deps* ]]
	[[ "$output" == *keygen* ]]
}

@test "smoke: subcomando desconhecido sai 2 com uso" {
	run bash "$SETUP" bogus
	[ "$status" -eq 2 ]
	[[ "$output" == *"uso: setup.sh"* ]]
}

# --- psk_name ------------------------------------------------------------

@test "psk_name: ordena lexicograficamente e junta com --" {
	call psk_name tv desktop
	[ "$status" -eq 0 ]
	[ "$output" = "desktop--tv" ]
}

@test "psk_name: mesma saída independente da ordem dos argumentos" {
	call psk_name laptop desktop
	local a="$output"
	call psk_name desktop laptop
	[ "$output" = "$a" ]
	[ "$output" = "desktop--laptop" ]
}

@test "psk_name: usa colação C (maiúsculas antes de minúsculas)" {
	LC_ALL=en_US.UTF-8 call psk_name banana Apple
	[ "$output" = "Apple--banana" ]
}

# --- add_port_if_missing ----------------------------------------------------

@test "add_port_if_missing: anexa porta default quando falta" {
	call add_port_if_missing 10.0.0.2
	[ "$output" = "10.0.0.2:7532" ]
}

@test "add_port_if_missing: preserva porta existente" {
	call add_port_if_missing 10.0.0.2:9999
	[ "$output" = "10.0.0.2:9999" ]
}

@test "add_port_if_missing: preserva user@ e anexa porta" {
	call add_port_if_missing user@host
	[ "$output" = "user@host:7532" ]
}

@test "add_port_if_missing: preserva user@host:porta" {
	call add_port_if_missing user@host:22
	[ "$output" = "user@host:22" ]
}

@test "add_port_if_missing: IPv6 literal sem porta" {
	call add_port_if_missing '[::1]'
	[ "$output" = "[::1]:7532" ]
}

@test "add_port_if_missing: IPv6 literal com porta" {
	call add_port_if_missing '[fe80::1]:7000'
	[ "$output" = "[fe80::1]:7000" ]
}

# --- strip_ansi -----------------------------------------------------------

@test "strip_ansi: remove sequências SGR" {
	run bash -c 'source "$1"; printf "\033[1;32mverde\033[0m normal" | strip_ansi' _ "$SETUP"
	[ "$status" -eq 0 ]
	[ "$output" = "verde normal" ]
}

@test "strip_ansi: passa texto sem escape intacto" {
	run bash -c 'source "$1"; printf "1920x1080" | strip_ansi' _ "$SETUP"
	[ "$output" = "1920x1080" ]
}

# --- ini_get ------------------------------------------------------------

@test "ini_get: chave da seção [local]" {
	call ini_get "$FIXTURES/peers.sample.toml" '[local]' name
	[ "$output" = "desktop" ]
}

@test "ini_get: valor com dois-pontos preservado" {
	call ini_get "$FIXTURES/peers.sample.toml" '[local]' listen
	[ "$output" = "0.0.0.0:7532" ]
}

@test "ini_get: chave dentro de [[peer]]" {
	call ini_get "$FIXTURES/peers.sample.toml" '[[peer]]' direction
	[ "$output" = "right" ]
}

@test "ini_get: chave ausente devolve vazio" {
	call ini_get "$FIXTURES/peers.sample.toml" '[local]' inexistente
	[ "$output" = "" ]
}

@test "ini_get: não vaza chave de outra seção" {
	# 'direction' só existe em [[peer]], não em [local]
	call ini_get "$FIXTURES/peers.sample.toml" '[local]' direction
	[ "$output" = "" ]
}

# --- backup_file --------------------------------------------------------

@test "backup_file: cria cópia com sufixo .bak.<epoch> e ecoa o path" {
	run bash -c '
		source "$1"
		d=$(mktemp -d)
		f="$d/peers.toml"
		printf "conteudo\n" > "$f"
		b=$(backup_file "$f")
		echo "$b"
		[ -f "$b" ]
		[ "$(cat "$b")" = "conteudo" ]
	' _ "$SETUP"
	[ "$status" -eq 0 ]
	[[ "${lines[0]}" =~ \.bak\.[0-9]+$ ]]
}

@test "backup_file: colisão de timestamp gera sufixo -N" {
	run bash -c '
		source "$1"
		date() { echo 1700000000; }
		d=$(mktemp -d)
		f="$d/peers.toml"
		printf "x\n" > "$f"
		b1=$(backup_file "$f")
		b2=$(backup_file "$f")
		b3=$(backup_file "$f")
		echo "$b1"; echo "$b2"; echo "$b3"
	' _ "$SETUP"
	[ "$status" -eq 0 ]
	[[ "${lines[0]}" == *.bak.1700000000 ]]
	[[ "${lines[1]}" == *.bak.1700000000-1 ]]
	[[ "${lines[2]}" == *.bak.1700000000-2 ]]
}

# --- need_bin (só ramos puros, sem build/apt) ----------------------------

@test "need_bin: KVMC_BIN executável é aceito e vira \$BIN" {
	run bash -c 'source "$1"; KVMC_BIN=/bin/true; need_bin; echo "$BIN $_BIN_OK"' _ "$SETUP"
	[ "$status" -eq 0 ]
	[ "$output" = "/bin/true 1" ]
}

@test "need_bin: KVMC_BIN inexistente aborta" {
	run bash -c 'source "$1"; KVMC_BIN=/no/such/bin; need_bin' _ "$SETUP"
	[ "$status" -eq 1 ]
	[[ "$output" == *"não é um executável"* ]]
}

@test "need_bin: fora do repo e sem KVMC_BIN aborta com dica" {
	run bash -c '
		source "$1"
		REPO_ROOT=$(mktemp -d)
		BIN="$REPO_ROOT/target/release/kvmc-share"
		unset KVMC_BIN
		need_bin
	' _ "$SETUP"
	[ "$status" -eq 1 ]
	[[ "$output" == *"rode de dentro do repo ou defina KVMC_BIN"* ]]
}

# --- detect_resolution / _res_grep ---------------------------------------

@test "_res_grep: extrai WxH da linha (current) do cosmic-randr (com ANSI)" {
	run bash -c 'source "$1"; strip_ansi < "$2" | _res_grep "(current)"' _ "$SETUP" "$FIXTURES/cosmic-randr.out"
	[ "$status" -eq 0 ]
	[ "$output" = "1920 1080" ]
}

@test "_res_grep: wlr-randr — linha 'current'" {
	run bash -c 'source "$1"; _res_grep current < "$2"' _ "$SETUP" "$FIXTURES/wlr-randr.out"
	[ "$output" = "1920 1080" ]
}

@test "_res_grep: xrandr — linha com '*'" {
	run bash -c 'source "$1"; _res_grep "*" < "$2"' _ "$SETUP" "$FIXTURES/xrandr.out"
	[ "$output" = "1920 1080" ]
}

@test "_res_grep: sem marcador casando devolve vazio" {
	run bash -c 'source "$1"; _res_grep "(current)" < "$2"' _ "$SETUP" "$FIXTURES/xrandr.out"
	[ "$output" = "" ]
}

@test "detect_resolution: se alguma ferramenta existe, saída é 'N N' ou vazia" {
	if ! command -v cosmic-randr >/dev/null && ! command -v wlr-randr >/dev/null && ! command -v xrandr >/dev/null; then
		skip "nenhuma ferramenta de randr"
	fi
	run bash -c 'source "$1"; detect_resolution' _ "$SETUP"
	[ "$status" -eq 0 ]
	[[ "$output" == "" || "$output" =~ ^[0-9]+\ [0-9]+$ ]]
}

# --- validate_peers_toml ----------------------------------------------------

@test "validate_peers_toml: fixture válido passa" {
	run bash -c 'source "$1"; validate_peers_toml "$2"' _ "$SETUP" "$FIXTURES/peers.sample.toml"
	[ "$status" -eq 0 ]
}

@test "validate_peers_toml: sem [local] falha" {
	run bash -c '
		source "$1"
		f=$(mktemp)
		printf "[[peer]]\nname = \"x\"\npsk_path = \"/tmp/x\"\n" > "$f"
		validate_peers_toml "$f"
	' _ "$SETUP"
	[ "$status" -eq 1 ]
	[[ "$output" == *"[local]"* ]]
}

@test "validate_peers_toml: [[peer]] sem psk_path falha" {
	run bash -c '
		source "$1"
		f=$(mktemp)
		cat > "$f" <<TOML
[local]
name = "d"
width = 1
height = 1
listen = "0.0.0.0:7532"

[[peer]]
name = "l"
addr = "1.2.3.4:7532"
direction = "right"
TOML
		validate_peers_toml "$f"
	' _ "$SETUP"
	[ "$status" -eq 1 ]
	[[ "$output" == *"psk_path"* ]]
}

@test "validate_peers_toml: campo ausente em [local] falha" {
	run bash -c '
		source "$1"
		f=$(mktemp)
		printf "[local]\nname = \"d\"\nlisten = \"0.0.0.0:7532\"\n" > "$f"
		validate_peers_toml "$f"
	' _ "$SETUP"
	[ "$status" -eq 1 ]
	[[ "$output" == *"width"* ]]
}

@test "validate_peers_toml: arquivo inexistente falha" {
	run bash -c 'source "$1"; validate_peers_toml /no/such/peers.toml' _ "$SETUP"
	[ "$status" -eq 1 ]
}

# --- parse_input_devices --------------------------------------------------

@test "parse_input_devices: mapeia eventN -> Nome do fixture" {
	run bash -c 'DEVICES_FILE="$2" bash -c "source \"$1\"; parse_input_devices"' _ "$SETUP" "$FIXTURES/proc-input-devices.txt"
	[ "$status" -eq 0 ]
	[[ "$output" == *"event3"*"AT Translated Set 2 keyboard"* ]]
	[[ "$output" == *"event4"*"Logitech USB Receiver Mouse"* ]]
	[[ "$output" == *"event5"*"SynPS/2 Synaptics TouchPad"* ]]
}

@test "parse_input_devices: arquivo ausente retorna 1" {
	run bash -c 'DEVICES_FILE=/no/such/file bash -c "source \"$1\"; parse_input_devices"' _ "$SETUP"
	[ "$status" -eq 1 ]
}

# --- _list_capture_devices ----------------------------------------------

@test "_list_capture_devices: casa symlink by-id -> eventN -> nome" {
	run bash -c '
		source "$1"
		d=$(mktemp -d); mkdir "$d/by-id"
		ln -s /dev/input/event3 "$d/by-id/usb-Foo_Kbd-event-kbd"
		ln -s /dev/input/event4 "$d/by-id/usb-Bar_Mouse-event-mouse"
		BYID_DIR="$d/by-id" DEVICES_FILE="$2" _list_capture_devices
	' _ "$SETUP" "$FIXTURES/proc-input-devices.txt"
	[ "$status" -eq 0 ]
	[[ "$output" == *"usb-Foo_Kbd-event-kbd"*"AT Translated Set 2 keyboard"* ]]
	[[ "$output" == *"usb-Bar_Mouse-event-mouse"*"Logitech USB Receiver Mouse"* ]]
}

@test "_list_capture_devices: diretório ausente retorna 1" {
	run bash -c 'source "$1"; BYID_DIR=/no/such _list_capture_devices' _ "$SETUP"
	[ "$status" -eq 1 ]
}

@test "_list_capture_devices: sem symlinks casando devolve vazio, rc 0" {
	run bash -c '
		source "$1"
		d=$(mktemp -d); mkdir "$d/by-id"
		BYID_DIR="$d/by-id" DEVICES_FILE="$2" _list_capture_devices
	' _ "$SETUP" "$FIXTURES/proc-input-devices.txt"
	[ "$status" -eq 0 ]
	[ "$output" = "" ]
}

# --- write_env --------------------------------------------------------

@test "write_env: grava KVMC_SHARE_DEVICES com modo 600" {
	run bash -c '
		source "$1"
		CONF_DIR=$(mktemp -d)/kvmc-share
		ENV_FILE="$CONF_DIR/env"
		write_env "/dev/input/eventA:/dev/input/eventB"
		echo "---"
		cat "$ENV_FILE"
		stat -c "%a" "$ENV_FILE"
	' _ "$SETUP"
	[ "$status" -eq 0 ]
	[[ "$output" == *"KVMC_SHARE_DEVICES=/dev/input/eventA:/dev/input/eventB"* ]]
	[[ "$output" == *$'\n600' ]]
}

# --- write_peers_toml -------------------------------------------------

@test "write_peers_toml: gera TOML válido com psk_path <menor>--<maior>" {
	run bash -c '
		source "$1"
		CONF_DIR=$(mktemp -d)/kvmc-share
		PEERS_TOML="$CONF_DIR/peers.toml"
		write_peers_toml desktop 1920 1080 0.0.0.0:7532 \
			"$(printf "laptop\t192.168.1.50:7532\tright")"
		echo "==="
		cat "$PEERS_TOML"
		validate_peers_toml "$PEERS_TOML"
	' _ "$SETUP"
	[ "$status" -eq 0 ]
	[[ "$output" == *'psk_path = "~/.config/kvmc-share/peers/desktop--laptop.psk"'* ]]
	[[ "$output" == *'direction = "right"'* ]]
}

@test "write_peers_toml: dois peers, psk_path por par" {
	run bash -c '
		source "$1"
		CONF_DIR=$(mktemp -d)/kvmc-share
		PEERS_TOML="$CONF_DIR/peers.toml"
		write_peers_toml laptop 1920 1080 0.0.0.0:7532 \
			"$(printf "desktop\t10.0.0.1:7532\tleft")" \
			"$(printf "tv\t10.0.0.3:7532\tright")"
		grep -c "^\[\[peer\]\]" "$PEERS_TOML"
		grep psk_path "$PEERS_TOML"
		validate_peers_toml "$PEERS_TOML"
	' _ "$SETUP"
	[ "$status" -eq 0 ]
	[[ "$output" == *"peers/desktop--laptop.psk"* ]]
	[[ "$output" == *"peers/laptop--tv.psk"* ]]
}

@test "write_peers_toml: faz backup .bak.<epoch> se já existir" {
	run bash -c '
		source "$1"
		date() { echo 1700000000; }
		CONF_DIR=$(mktemp -d)/kvmc-share
		PEERS_TOML="$CONF_DIR/peers.toml"
		mkdir -p "$CONF_DIR"; printf "antigo\n" > "$PEERS_TOML"
		write_peers_toml desktop 1 1 0.0.0.0:7532 "$(printf "l\t1.2.3.4:7532\tright")"
		ls "$CONF_DIR"
		cat "$CONF_DIR/peers.toml.bak.1700000000"
	' _ "$SETUP"
	[ "$status" -eq 0 ]
	[[ "$output" == *"peers.toml.bak.1700000000"* ]]
	[[ "$output" == *"antigo"* ]]
}

# --- collect_peers (não-interativo via heredoc) --------------------------

@test "collect_peers: rejeita nome repetido, nome=local e direção repetida" {
	run bash -c '
		source "$1"
		collect_peers desktop 2>/dev/null <<IN
2
desktop
laptop
192.168.1.50
right
laptop
tv
10.0.0.3
right
up
IN
	' _ "$SETUP"
	[ "$status" -eq 0 ]
	# peer 1: "desktop" rejeitado (==local) -> "laptop"; addr; "right"
	# peer 2: "laptop" rejeitado (repetido) -> "tv"; addr; "right" rejeitado -> "up"
	[ "${lines[0]}" = "$(printf 'laptop\t192.168.1.50:7532\tright')" ]
	[ "${lines[1]}" = "$(printf 'tv\t10.0.0.3:7532\tup')" ]
}

# --- _peer_list -------------------------------------------------------

@test "_peer_list: um peer do fixture simples" {
	run bash -c 'source "$1"; _peer_list "$2"' _ "$SETUP" "$FIXTURES/peers.sample.toml"
	[ "$status" -eq 0 ]
	[ "$output" = "$(printf 'laptop\t192.168.1.50:7532')" ]
}

@test "_peer_list: dois peers, ordem preservada, addr com user@" {
	run bash -c 'source "$1"; _peer_list "$2"' _ "$SETUP" "$FIXTURES/peers.multi.toml"
	[ "$status" -eq 0 ]
	[ "${lines[0]}" = "$(printf 'desktop\t10.0.0.1:7532')" ]
	[ "${lines[1]}" = "$(printf 'tv\tuser@10.0.0.3:7532')" ]
}

# --- cmd_keygen / _keygen_peer -----------------------------------------

@test "_keygen_peer: papel 'gero' cria PSK de 32 bytes, modo 600, sem .tmp" {
	run bash -c '
		source "$1"
		ssh() { return 0; }
		scp() { return 0; }
		PSK_DIR=$(mktemp -d)/peers
		printf "g\n" | _keygen_peer desktop laptop 192.168.1.50:7532
		f="$PSK_DIR/desktop--laptop.psk"
		echo "size=$(stat -c%s "$f")"
		echo "mode=$(stat -c%a "$f")"
		echo "tmp=$(ls "$PSK_DIR"/*.tmp 2>/dev/null | wc -l)"
	' _ "$SETUP"
	[ "$status" -eq 0 ]
	[[ "$output" == *"size=32"* ]]
	[[ "$output" == *"mode=600"* ]]
	[[ "$output" == *"tmp=0"* ]]
}

@test "_keygen_peer: papel 'gero' com scp falhando mostra bloco manual e pausa" {
	run bash -c '
		source "$1"
		ssh() { return 1; }
		scp() { return 1; }
		PSK_DIR=$(mktemp -d)/peers
		printf "g\n\n" | _keygen_peer desktop laptop 192.168.1.50:7532 2>&1
	' _ "$SETUP"
	[ "$status" -eq 0 ]
	[[ "$output" == *"scp falhou — copie manualmente"* ]]
	[[ "$output" == *"scp "*"peers/desktop--laptop.psk"* ]]
}

@test "_keygen_peer: papel 'recebo' sem PSK aborta com instrução" {
	run bash -c '
		source "$1"
		PSK_DIR=$(mktemp -d)/peers; mkdir -p "$PSK_DIR"
		printf "r\n" | _keygen_peer desktop laptop 192.168.1.50:7532 2>&1
	' _ "$SETUP"
	[ "$status" -eq 1 ]
	[[ "$output" == *"rode 'keygen' no papel 'gero'"* ]]
}

@test "_keygen_peer: papel 'recebo' com PSK de 32 bytes reporta OK + sha256" {
	run bash -c '
		source "$1"
		PSK_DIR=$(mktemp -d)/peers; mkdir -p "$PSK_DIR"
		head -c 32 /dev/urandom > "$PSK_DIR/desktop--laptop.psk"
		ssh() { return 1; }
		printf "r\n" | _keygen_peer desktop laptop 10.255.255.1:7532 2>&1
	' _ "$SETUP"
	[ "$status" -eq 0 ]
	[[ "$output" == *"PSK presente"* ]]
	[[ "$output" == *"sha256:"* ]]
}

@test "cmd_keygen: sem peers.toml aborta" {
	run bash -c 'source "$1"; PEERS_TOML=/no/such/peers.toml; cmd_keygen' _ "$SETUP"
	[ "$status" -eq 1 ]
	[[ "$output" == *"rode 'config' antes"* ]]
}

# --- cmd_run --------------------------------------------------------

@test "cmd_run: sem env aborta pedindo 'config'" {
	run bash -c 'source "$1"; ENV_FILE=/no/such/env; cmd_run' _ "$SETUP"
	[ "$status" -eq 1 ]
	[[ "$output" == *"rode 'config' antes"* ]]
}

@test "cmd_run: carrega o env e faz exec do binário com 'run'" {
	run bash -c '
		source "$1"
		ENV_FILE=$(mktemp)
		printf "KVMC_SHARE_DEVICES=/dev/input/eventZ\n" > "$ENV_FILE"
		# binário fake que só ecoa argv e o env relevante
		KVMC_BIN=$(mktemp)
		cat > "$KVMC_BIN" <<EOS
#!/usr/bin/env bash
echo "argv=\$*"
echo "dev=\$KVMC_SHARE_DEVICES"
EOS
		chmod +x "$KVMC_BIN"
		cmd_run
		echo "NAO DEVERIA CHEGAR AQUI"
	' _ "$SETUP"
	[ "$status" -eq 0 ]
	[[ "$output" == *"argv=run"* ]]
	[[ "$output" == *"dev=/dev/input/eventZ"* ]]
	[[ "$output" != *"NAO DEVERIA CHEGAR"* ]]
}

# --- cmd_service / _service_unit / _exec_start -------------------------

@test "_exec_start: usa %h quando sob \$HOME" {
	run bash -c 'source "$1"; HOME=/home/x; _exec_start /home/x/projetos/kvmc-share/target/release/kvmc-share' _ "$SETUP"
	[ "$output" = "%h/projetos/kvmc-share/target/release/kvmc-share" ]
}

@test "_exec_start: caminho fora de \$HOME fica absoluto" {
	run bash -c 'source "$1"; HOME=/home/x; _exec_start /opt/kvmc/kvmc-share' _ "$SETUP"
	[ "$output" = "/opt/kvmc/kvmc-share" ]
}

@test "_service_unit: campos obrigatórios presentes" {
	run bash -c 'source "$1"; _service_unit "%h/bin/kvmc-share"' _ "$SETUP"
	[ "$status" -eq 0 ]
	[[ "$output" == *"ExecStart=%h/bin/kvmc-share run"* ]]
	[[ "$output" == *"EnvironmentFile=%h/.config/kvmc-share/env"* ]]
	[[ "$output" == *"Restart=on-failure"* ]]
	[[ "$output" == *"RestartSec=2"* ]]
	[[ "$output" == *"WantedBy=default.target"* ]]
}

@test "cmd_service: sem bus de usuário aborta sem escrever unit" {
	run bash -c '
		source "$1"
		systemctl() { return 1; }
		UNIT=$(mktemp -d)/kvmc-share.service
		cmd_service
		[ -e "$UNIT" ] && echo "UNIT ESCRITO" || echo "unit ausente"
	' _ "$SETUP"
	[ "$status" -eq 1 ]
	[[ "$output" == *"sem bus de usuário"* ]]
	[[ "$output" != *"UNIT ESCRITO"* ]]
}

@test "cmd_service: idempotente — unit igual não é reescrito nem recarregado" {
	run bash -c '
		source "$1"
		calls=""
		systemctl() { calls="$calls systemctl:$*"; return 0; }
		loginctl() { return 0; }
		KVMC_BIN=/bin/true
		UNIT=$(mktemp -d)/kvmc-share.service
		# primeira passada escreve
		cmd_service >/dev/null 2>&1
		before=$(cat "$UNIT")
		calls=""
		# segunda passada: conteúdo idêntico
		cmd_service >/dev/null 2>&1
		echo "reload:$(echo "$calls" | grep -c daemon-reload)"
		[ "$(cat "$UNIT")" = "$before" ] && echo "unchanged"
	' _ "$SETUP"
	[ "$status" -eq 0 ]
	[[ "$output" == *"reload:0"* ]]
	[[ "$output" == *"unchanged"* ]]
}

# --- cmd_doctor -----------------------------------------------------

@test "cmd_doctor: ambiente vazio -> exit 1 e itens obrigatórios pendentes" {
	run bash -c '
		source "$1"
		d=$(mktemp -d)
		ENV_FILE="$d/env"; PEERS_TOML="$d/peers.toml"; PSK_DIR="$d/peers"
		KVMC_BIN=/no/such/bin
		cmd_doctor
	' _ "$SETUP"
	[ "$status" -eq 1 ]
	[[ "$output" == *"sem "*"/env"* ]]
	[[ "$output" == *"peers.toml ausente/inválido"* ]]
	[[ "$output" == *"itens obrigatórios pendentes"* ]]
}

@test "cmd_doctor: XDG_RUNTIME_DIR ausente -> clipboard indeterminado, sem crash" {
	run bash -c '
		source "$1"
		d=$(mktemp -d)
		ENV_FILE="$d/env"; PEERS_TOML="$d/peers.toml"; PSK_DIR="$d/peers"
		KVMC_BIN=/bin/true
		unset XDG_RUNTIME_DIR
		cmd_doctor
	' _ "$SETUP"
	[[ "$output" == *"clipboard: indeterminado"* ]]
}

@test "cmd_doctor: peers.toml válido + PSKs de 32 bytes reportam ok" {
	run bash -c '
		source "$1"
		d=$(mktemp -d)
		PEERS_TOML="$2"
		PSK_DIR="$d/peers"; mkdir -p "$PSK_DIR"
		head -c 32 /dev/urandom > "$PSK_DIR/me--near.psk"
		ENV_FILE="$d/env"
		KVMC_BIN=/bin/true
		XDG_RUNTIME_DIR="$d"
		cmd_doctor
	' _ "$SETUP" "$FIXTURES/peers.doctor.toml"
	[[ "$output" == *"peers.toml válido"* ]]
	[[ "$output" == *"PSK near: ok"* ]]
	[[ "$output" == *"peer near (127.0.0.1:1): refused"* ]]
}

@test "cmd_doctor: PSK ausente aponta 'keygen'" {
	run bash -c '
		source "$1"
		d=$(mktemp -d)
		PEERS_TOML="$2"
		PSK_DIR="$d/peers"; mkdir -p "$PSK_DIR"
		ENV_FILE="$d/env"; KVMC_BIN=/bin/true; XDG_RUNTIME_DIR="$d"
		cmd_doctor
	' _ "$SETUP" "$FIXTURES/peers.doctor.toml"
	[ "$status" -eq 1 ]
	[[ "$output" == *"PSK near ausente"*"rode 'keygen'"* ]]
}
