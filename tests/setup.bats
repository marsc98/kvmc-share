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
