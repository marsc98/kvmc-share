# Testing

## Rust (crate `kvmc-share`)

- Testes unitários inline (`#[cfg(test)] mod tests`) nos módulos.
- Comando: `cargo test`.

## `scripts/setup.sh` (bash)

Decidido na entrevista da feature `setup-script`.

### Gate `quick` (roda em toda task de código do script)

```bash
bash -n scripts/setup.sh && shellcheck -x scripts/setup.sh && bats tests/
```

- `bash -n` — checagem de sintaxe.
- `shellcheck -x` — lint, **zero achado** exigido.
- `bats tests/` — testes das funções puras.

### Gate `full` (última task da feature)

`quick` + cenário de sucesso ponta a ponta numa VM Pop!_OS (checklist manual
anexado ao PR): `./scripts/setup.sh` → `doctor` exit 0; 2ª execução não altera
arquivos (fora `peers.toml.bak.*` se o usuário optar por refazer o `config`).

### Escopo do `bats`

Cobre **apenas funções puras**, sem efeito no sistema:

- `psk_name`, `add_port_if_missing`, `strip_ansi`, `ini_get`, `backup_file`
- `detect_resolution` — via fixtures em `tests/fixtures/` (`cosmic-randr.out`, etc.)
- `validate_peers_toml` — via fixtures (`peers.ok.toml`, `peers.missing-psk.toml`, …)
- `parse_input_devices` — via fixture `proc-input-devices.txt` (override `DEVICES_FILE`)

Partes que tocam `sudo` / udev / `systemd` / `ssh`/`scp` **não** têm bats —
verificação manual documentada nos blocos `Verify` de
`.specs/features/setup-script/tasks.md`.

### Dependências

`shellcheck` e `bats` via `apt` (`sudo apt install -y shellcheck bats`).

### Contagem de testes

`scripts/setup.sh`: **72 testes bats** (`bats tests/`), `shellcheck -x` limpo.
