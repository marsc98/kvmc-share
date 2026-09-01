#!/usr/bin/env bats
#
# Testes de scripts/setup.sh — cobrem só funções puras (sem efeito no sistema).
# Partes que mexem em sudo/udev/systemd/ssh têm verificação manual documentada
# nos blocos "Verify" de .specs/features/setup-script/tasks.md.

SETUP="${BATS_TEST_DIRNAME}/../scripts/setup.sh"

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
