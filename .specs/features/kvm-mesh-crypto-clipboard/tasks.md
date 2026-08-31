# kvm-share — Malha Criptografada com Borda de Tela e Clipboard Sync — Tasks

**Design**: `.specs/features/kvm-mesh-crypto-clipboard/design.md`
**Status**: T1-T13 implementadas e commitadas; T14 (verificação manual em hardware) pendente

---

## Execution Plan

### Phase 1: Foundation (T1 sequential, depois paralelo)
```
T1 ──┬──→ T2 [P]
     ├──→ T3 [P]
     └──→ T7 [P]

T4 (repo copied, independente, roda em paralelo com tudo)
```

### Phase 2: Core protocol (parcialmente paralelo)
```
T1 ──→ T5 ──→ T6
```

### Phase 3: Integração (sequencial, depende de Fase 1+2)
```
T2, T3, T6, T7 ──→ T8
T4, T1 ──→ T9
T8, T9 ──→ T10
T2, T5 ──→ T11
T10, T11 ──→ T12 ──→ T13
T12 ──→ T14 (verificação manual)
```

---

## Task Breakdown

### T1: Atualizar `Cargo.toml` com novas dependências

**What**: Adicionar `snow`, `serde` (+ `derive`), `toml`, e a dependência git fixada em rev de `copied-core`; manter `evdev`/`anyhow`.
**Where**: `Cargo.toml`
**Depends on**: None
**Reuses**: `Cargo.toml` atual
**Requirement**: CRYPTO-01, CLIP-05

**Tools**:
- MCP: NONE
- Skill: `find-docs` (confirmar versão estável do `snow` e sintaxe de git dependency com `rev`)

**Done when**:
- [x] `cargo build` resolve as dependências sem erro
- [x] `copied-core` referenciada como `{ git = "...", rev = "<hash>" }`, não path dependency
- [x] Gate check passa: `cargo build`

**Tests**: none (mudança de manifest)
**Gate**: build

---

### T2: `src/config.rs` — parsing de `peers.toml` [P]

**What**: Structs `LocalConfig`, `PeerConfig`, `Direction`, função `load()` que lê `~/.config/kvm-share/peers.toml` e valida que os arquivos de PSK referenciados existem.
**Where**: `src/config.rs`
**Depends on**: T1
**Reuses**: nenhum
**Requirement**: MESH-02, EDGE-01

**Tools**:
- MCP: NONE
- Skill: NONE

**Done when**:
- [x] `load()` retorna erro claro se o arquivo não existe, é malformado, ou referencia PSK inexistente
- [x] `Direction` cobre `left`/`right`/`up`/`down`
- [x] Teste unitário: parse de TOML válido com 2+ peers
- [x] Teste unitário: erro em TOML malformado
- [x] Teste unitário: erro quando `psk_path` não existe
- [x] Gate check passa: `cargo test`, `cargo fmt --check`, `cargo clippy -- -D warnings`

**Tests**: unit (inline `#[cfg(test)]`)
**Gate**: quick

---

### T3: `src/cursor.rs` — rastreamento de borda por deltas [P]

**What**: `CursorTracker` que acumula `REL_X`/`REL_Y` e retorna a direção cruzada quando a posição virtual sai dos limites da resolução configurada, resetando a posição pro lado oposto.
**Where**: `src/cursor.rs`
**Depends on**: T1
**Reuses**: nenhum
**Requirement**: EDGE-01, EDGE-02, EDGE-04

**Tools**:
- MCP: NONE
- Skill: NONE

**Done when**:
- [x] `accumulate(dx, dy)` retorna `None` enquanto dentro dos limites
- [x] Retorna `Some(Direction::Right)` ao cruzar a borda direita (e equivalente pras outras 3 direções)
- [x] Reposiciona a origem no lado oposto da tela de destino após cruzar (para consistência ao entrar na próxima tela)
- [x] Teste unitário por direção (4 testes) + teste de não cruzar dentro dos limites
- [x] Gate check passa: `cargo test`, `cargo fmt --check`, `cargo clippy -- -D warnings`

**Tests**: unit
**Gate**: quick

---

### T4: Patch no `copied` — `Command::GetLatestText` (+ imagem)

**What**: Adicionar ao `copied-core` (repo `/home/marco/setup/copied`) um novo `Command::GetLatestText` (retorna o texto completo do item mais recente da pilha, não truncado) e um equivalente para imagem (ex: `Command::GetLatestImageBytes`), implementados em `copied-daemon/src/ipc.rs`. Criar uma tag/rev nesse repo pra fixar a dependência git do kvm-share.
**Where**: `/home/marco/setup/copied/crates/copied-core/src/lib.rs`, `/home/marco/setup/copied/crates/copied-daemon/src/ipc.rs`
**Depends on**: None (repositório e ciclo de release independentes)
**Reuses**: `Command::CopyToClipboard`/`GetImageBytes` como referência de padrão no mesmo arquivo
**Requirement**: CLIP-01, CLIP-04

**Tools**:
- MCP: NONE
- Skill: NONE

**Done when**:
- [x] `Command::GetLatestText` retorna `Response::Items`-like com texto completo do topo da pilha (não preview truncado) ou `Response::Error` se a pilha estiver vazia/topo não for texto
- [x] Equivalente pra imagem retorna bytes completos (reaproveitando o padrão de `GetImageBytes`)
- [x] Testes unitários no próprio `copied` (roundtrip serde + `handle_command`) seguindo o padrão já existente em `ipc.rs`
- [x] Gate check do `copied` passa: `cargo test`, `cargo fmt --check`, `cargo clippy -- -D warnings`
- [x] Tag/rev do commit registrada pra uso em T1

**Tests**: unit (no repo `copied`)
**Gate**: quick (gate do projeto `copied`, não do `kvm-share`)

**Nota**: task roda em outro repositório — não faz parte do gate/CI do `kvm-share`, mas é bloqueadora de fato pra T9.

---

### T5: `src/noise.rs` — handshake e canal cifrado

**What**: `EncryptedChannel` com `handshake_as_initiator`/`handshake_as_responder` usando `Noise_XXpsk3_25519_ChaChaPoly_BLAKE2s`, incluindo o preâmbulo em texto claro de identidade (nome do peer) antes do handshake, e `send`/`recv` com chunking para payloads > 65519 bytes.
**Where**: `src/noise.rs`
**Depends on**: T1
**Reuses**: exemplo de referência da doc oficial do `snow` (handshake `Noise_XXpsk3` completo)
**Requirement**: CRYPTO-01, CRYPTO-02, CRYPTO-03

**Tools**:
- MCP: NONE
- Skill: `find-docs` (revalidar API do `snow` ao implementar — builder, `into_transport_mode`, tamanhos máximos de mensagem)

**Done when**:
- [x] Handshake completo entre um par initiator/responder em memória (via `std::io::Cursor`/pipe) com a mesma PSK
- [x] Handshake falha quando as PSKs divergem (nenhum dado após o handshake é aceito)
- [x] `send`/`recv` fazem roundtrip de payload maior que um único frame Noise (testa chunking)
- [x] Preâmbulo de identidade permite ao respondedor escolher a PSK certa entre 2+ candidatas simuladas
- [x] Gate check passa: `cargo test`, `cargo fmt --check`, `cargo clippy -- -D warnings`

**Tests**: unit (handshake completo em memória, sem rede real)
**Gate**: quick

---

### T6: `src/wire.rs` — protocolo de aplicação (`WireMessage`)

**What**: `enum WireMessage` (`InputEvent`/`ClipboardText`/`ClipboardImage`/`FocusHandoff`/`Heartbeat`) com `write_message`/`read_message` sobre `EncryptedChannel`, reaproveitando o frame de 8 bytes existente pra `InputEvent`.
**Where**: `src/wire.rs`
**Depends on**: T5
**Reuses**: `write_event`/`read_event` de `src/lib.rs`
**Requirement**: CRYPTO-03, CLIP-01, CLIP-02, CLIP-04

**Tools**:
- MCP: NONE
- Skill: NONE

**Done when**:
- [x] Roundtrip de cada variante de `WireMessage` através de um par `EncryptedChannel` em memória
- [x] `InputEvent` serializado é byte-a-byte compatível com o `write_event`/`read_event` atual (mesmo frame de 8 bytes por dentro)
- [x] `ClipboardImage` com payload grande faz roundtrip correto (usa o chunking de T5)
- [x] Gate check passa: `cargo test`, `cargo fmt --check`, `cargo clippy -- -D warnings`

**Tests**: unit
**Gate**: quick

---

### T7: `src/devices.rs` — extrair captura/injeção [P]

**What**: Mover `open`/`grab`/`ungrab`/`fetch_events` (de `capture.rs`) e `build_virtual_device`/`emit` (de `inject.rs`) para um módulo compartilhado, sem o `AtomicBool` global — API por instância.
**Where**: `src/devices.rs`
**Depends on**: T1
**Reuses**: `src/bin/capture.rs::run_device_loop` (parcial), `src/bin/inject.rs::build_virtual_device` (integral)
**Requirement**: MESH-01

**Tools**:
- MCP: NONE
- Skill: NONE

**Done when**:
- [x] `build_virtual_device()` idêntico em comportamento ao de `inject.rs` (mesmos ranges de teclas/eixos)
- [x] Funções de captura não dependem mais de estado global compartilhado (`Arc<AtomicBool>`) — o chamador decide quando grab/ungrab
- [x] Gate check passa: `cargo build`, `cargo fmt --check`, `cargo clippy -- -D warnings` (sem hardware real disponível em CI — sem teste unitário de I/O real, ver T14)
- [x] Comentário no módulo aponta que testes de integração real ficam em T14 (verificação manual)

**Tests**: none (depende de hardware real — `/dev/input`, `/dev/uinput`)
**Gate**: build

---

### T8: `src/focus.rs` — máquina de estados de foco

**What**: `FocusState` (`Local`/`Capturing`/`Receiving`), `on_input_event`, `on_wire_message` (incluindo relay em cadeia ao receber `FocusHandoff` e detectar novo cruzamento de borda), `on_toggle_key`, e o desempate determinístico (nome do peer menor vence) para cruzamento simultâneo.
**Where**: `src/focus.rs`
**Depends on**: T2, T3, T6, T7
**Reuses**: lógica de alternância de `src/bin/capture.rs::run_device_loop`
**Requirement**: EDGE-01, EDGE-02, EDGE-03, EDGE-04, MESH-01, MESH-03

**Tools**:
- MCP: NONE
- Skill: NONE

**Done when**:
- [x] Transição `Local → Capturing` ao cruzar borda com peer configurado na direção
- [x] Permanece `Local` ao cruzar borda sem peer configurado naquela direção (EDGE-04)
- [x] `Capturing ↔ Local` via tecla de alternância, independente do estado acumulado de deltas
- [x] `Receiving` que detecta novo cruzamento de borda dispara relay (`Capturing` para o próximo peer) sem tocar a máquina de origem original
- [x] Corrida de cruzamento simultâneo resolvida deterministicamente (teste simula duas transições concorrentes)
- [x] Queda de conexão em `Capturing`/`Receiving` retorna pra `Local` (nunca fica "sem controle")
- [x] Gate check passa: `cargo test`, `cargo fmt --check`, `cargo clippy -- -D warnings`

**Tests**: unit (máquina de estados isolada, com mocks de `devices`/`wire`)
**Gate**: quick

---

### T9: `src/clipboard.rs` — ponte com `copied`

**What**: `is_available()`, `read_latest()`, `write()` usando `copied-core::{Command, Response}` sobre o socket Unix, com timeout/erro tratado como "indisponível" (nunca panic).
**Where**: `src/clipboard.rs`
**Depends on**: T1 (dependência `copied-core` já resolvida), T4 (command novo disponível na rev fixada)
**Reuses**: tipos `Command`/`Response`/`socket_path()` de `copied-core`
**Requirement**: CLIP-01, CLIP-02, CLIP-03, CLIP-04, CLIP-05

**Tools**:
- MCP: NONE
- Skill: NONE

**Done when**:
- [x] `is_available()` retorna `false` sem erro quando o socket não existe
- [x] `read_latest()`/`write()` fazem roundtrip contra uma instância real de `copied-daemon` rodando em ambiente de teste (ou mock de `UnixListener` respondendo o protocolo)
- [x] Payload acima do limite configurado é recusado antes de enviar pela rede (log, sem panic)
- [x] Gate check passa: `cargo test`, `cargo fmt --check`, `cargo clippy -- -D warnings`

**Tests**: unit (mock de socket Unix local — não precisa do `copied` real rodando pro teste automatizado)
**Gate**: quick

---

### T10: `src/bin/kvm-share.rs` — subcomando `run`

**What**: Sobe o daemon: carrega config (T2), cria `EncryptedChannel`s (T5) com cada peer (dial pro peer com nome lexicograficamente maior, escuta pros demais), abre dispositivos (T7), inicializa `FocusState` (T8) e `clipboard` (T9), e roda o loop principal despachando eventos de rede/dispositivo pra `focus.rs`.
**Where**: `src/bin/kvm-share.rs`
**Depends on**: T8, T9
**Reuses**: estrutura de `main()`/threads de `src/bin/capture.rs` e `src/bin/inject.rs`
**Requirement**: MESH-01, MESH-02, MESH-04, CRYPTO-04 (indiretamente, via config)

**Tools**:
- MCP: NONE
- Skill: NONE

**Done when**:
- [x] `kvm-share run` sobe sem erro com um `peers.toml` válido de exemplo
- [x] Conexão rejeitada (peer não listado) não derruba o processo, só loga e fecha aquela conexão (MESH-04)
- [x] Gate check passa: `cargo build`, `cargo fmt --check`, `cargo clippy -- -D warnings`

**Tests**: none neste binário (lógica pesada já testada nos módulos T2/T5/T6/T8/T9; este arquivo é fiação/`main`)
**Gate**: build

---

### T11: `src/bin/kvm-share.rs` — subcomando `keygen`

**What**: `kvm-share keygen <peer-name> <ip>` gera PSK de 32 bytes aleatórios, salva em `~/.config/kvm-share/peers/<peer-name>.psk` (permissão `0600`), tenta `scp` pro mesmo path relativo no destino; em falha do `scp`, imprime o path local e instruções de cópia manual.
**Where**: `src/bin/kvm-share.rs`
**Depends on**: T2 (formato/paths de config), T5 (formato de PSK de 32 bytes)
**Reuses**: nenhum
**Requirement**: CRYPTO-04, CRYPTO-05

**Tools**:
- MCP: NONE
- Skill: NONE

**Done when**:
- [x] Arquivo de PSK gerado tem exatamente 32 bytes e permissão `0600`
- [x] `scp` bem-sucedido não imprime instrução de cópia manual
- [x] `scp` falho imprime path local + instrução, sem tentar outro canal
- [x] PSK já existente pro mesmo peer pede confirmação antes de sobrescrever (edge case da spec)
- [x] Gate check passa: `cargo test` (função de geração/permissão testável sem rede), `cargo fmt --check`, `cargo clippy -- -D warnings`

**Tests**: unit (geração de PSK e permissão de arquivo; parte de rede/`scp` fica pra verificação manual em T14)
**Gate**: quick

---

### T12: Remover binários antigos e atualizar `src/lib.rs`

**What**: Remover `src/bin/capture.rs` e `src/bin/inject.rs`, ajustar `src/lib.rs` pra reexportar só o que os novos módulos precisam (`TOGGLE_KEY`, frame de 8 bytes reaproveitado por `wire.rs`).
**Where**: `src/lib.rs`, `src/bin/capture.rs` (remover), `src/bin/inject.rs` (remover)
**Depends on**: T10, T11
**Reuses**: `src/lib.rs` atual
**Requirement**: MESH-01

**Tools**:
- MCP: NONE
- Skill: NONE

**Done when**:
- [x] `cargo build` só gera o binário `kvm-share`
- [x] Testes de roundtrip existentes em `src/lib.rs` continuam passando (reaproveitados por `wire.rs`)
- [x] Gate check passa: `cargo test`, `cargo fmt --check`, `cargo clippy -- -D warnings`

**Tests**: unit (os já existentes, sem regressão)
**Gate**: quick

---

### T13: Atualizar `README.md`

**What**: Documentar o novo fluxo: instalação, `keygen`, formato de `peers.toml`, aviso de segurança atualizado (já não é mais "sem criptografia"), limitações remanescentes.
**Where**: `README.md`
**Depends on**: T12
**Reuses**: estrutura atual do README
**Requirement**: N/A (documentação)

**Tools**:
- MCP: NONE
- Skill: NONE

**Done when**:
- [x] Seção de segurança reflete o novo modelo (Noise_XXpsk3, não mais "rode em VPN por sua conta e risco")
- [x] Exemplo completo de `peers.toml` pra 3 máquinas
- [x] Passo a passo de pareamento (`keygen`) documentado, incluindo o caso de fallback manual

**Tests**: none
**Gate**: none (documentação)

---

### T14: Verificação manual — malha real de 3 máquinas

**What**: Validar em hardware real (não CI) os cenários que dependem de `evdev`/`uinput`/rede real entre máquinas, listados no spec.
**Where**: N/A (procedimento manual)
**Depends on**: T12
**Reuses**: N/A
**Requirement**: CRYPTO-01..05, MESH-01..04, EDGE-01..04, CLIP-01..05 (validação end-to-end)

**Tools**:
- MCP: NONE
- Skill: NONE

**Done when**:
- [ ] `tcpdump` numa sessão ativa não revela byte de evento em texto claro
- [ ] Malha A-B-C: cursor cruza de A pra B pra C e volta, controle muda corretamente em cada borda (valida o relay em cadeia)
- [ ] Scroll Lock funciona como alternativa manual em qualquer ponto da malha
- [ ] Clipboard (texto >200 chars e imagem) sincroniza corretamente na troca de foco, com `copied` rodando nas 3 máquinas
- [ ] Desligar `copied` numa máquina não impede a troca de foco (degradação graciosa)
- [ ] Queda de conexão de rede no meio de uma sessão devolve o controle local, sem travar
- [ ] `keygen` com e sem SSH configurado, nos dois caminhos (scp automático e fallback manual)

**Tests**: manual (checklist acima)
**Gate**: N/A — checklist manual documentado no PR/commit

---

## Requirement Traceability (atualizado)

| Requirement ID | Task(s) |
| --- | --- |
| CRYPTO-01..03 | T5, T6, T14 |
| CRYPTO-04, CRYPTO-05 | T11, T14 |
| MESH-01..04 | T7, T8, T10, T14 |
| EDGE-01..04 | T2, T3, T8, T14 |
| CLIP-01..05 | T4, T9, T14 |

**Coverage:** 18 requisitos, 18 mapeados a tasks, 0 sem mapeamento ✅
