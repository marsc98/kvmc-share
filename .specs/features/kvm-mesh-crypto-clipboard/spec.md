# kvm-share — Malha Criptografada com Borda de Tela e Clipboard Sync — Specification

## Problem Statement

O `kvm-share` hoje resolve compartilhar mouse/teclado entre **duas** máquinas Linux sem depender do portal Wayland `InputCapture` (ausente no COSMIC/Pop!_OS), mas com três limitações que travam o uso real: o protocolo de rede não é criptografado nem autenticado (qualquer um na rede pode ler ou injetar eventos), só suporta duas máquinas fixas (`capture`/`inject`) trocadas por atalho de teclado, e não sincroniza clipboard entre elas.

## Proposed Solution

O `kvm-share` passa a ser um único daemon (não mais dois binários separados) que roda em todas as máquinas de uma malha. Cada instância se autentica com as demais via chave pré-compartilhada (PSK) usando Noise Protocol, sabe qual peer fica em qual direção da tela (config estática), transfere o controle de mouse/teclado automaticamente quando o cursor cruza a borda da tela (ou manualmente via Scroll Lock), e sincroniza o clipboard (texto e imagem) no momento da troca de foco, via integração com o daemon `copied` já existente.

## Goals

- [ ] Nenhum evento de mouse/teclado trafega em texto claro entre as máquinas.
- [ ] Usuário consegue montar uma malha de 3+ máquinas e mover o controle entre elas cruzando bordas de tela, sem reiniciar processos.
- [ ] Clipboard (texto e imagem) acompanha a troca de foco sem ação manual do usuário.
- [ ] Pareamento de uma nova máquina na malha não exige mais que um comando (`kvm-share keygen <ip>`) mais, na ausência de SSH, uma cópia manual de arquivo.

## Out of Scope

| Feature | Reason |
| --- | --- |
| Descoberta automática de peers (mDNS/broadcast) | Decidido na interview: config estática evita superfície de anúncio não-autenticada na rede |
| Hub/coordenador central de foco | Decidido na interview: malha direta (peer-to-peer) evita ponto único de falha |
| Detecção de borda via API do compositor (ex: COSMIC) | Reintroduziria a dependência de compositor que o projeto existe pra evitar |
| Sync contínuo de clipboard (fora da troca de foco) | Decidido na interview: sync só dispara na troca de foco, sem polling |
| Mudanças na arquitetura interna do `copied` além do novo `Command::GetLatestText` | `copied` continua produto independente; só o contrato de protocolo é estendido |
| Suporte a Windows/macOS | Projeto assume Linux com `evdev`/`uinput` nas duas pontas |

---

## User Stories

### P1: Canal de rede criptografado e autenticado ⭐ MVP

**User Story**: Como usuário rodando o kvm-share fora de uma VPN controlada, quero que a comunicação entre as máquinas seja criptografada e autenticada, para que ninguém na mesma rede consiga ler ou injetar eventos de mouse/teclado.

**Why P1**: É o motivo desta evolução — sem isso, o protocolo atual (texto claro) é a limitação mais grave do protótipo original.

**Acceptance Criteria**:

1. WHEN duas instâncias do `kvm-share` estabelecem conexão THEN o sistema SHALL negociar a sessão via Noise Protocol, padrão `Noise_XX`, usando a PSK do peer correspondente.
2. WHEN a PSK usada por uma das pontas não corresponde à esperada pelo outro lado THEN o sistema SHALL rejeitar a conexão e logar o motivo, sem processar nenhum evento.
3. WHEN a sessão é estabelecida com sucesso THEN todo evento de mouse/teclado trocado nessa conexão SHALL ser cifrado (nenhum byte de `type`/`code`/`value` trafega em texto claro).
4. WHEN o usuário roda `kvm-share keygen <ip-da-máquina-alvo>` THEN o sistema SHALL gerar uma PSK local em `~/.config/kvm-share/psk` (permissão `0600`) e tentar enviá-la via `scp` para o destino.
5. WHEN o `scp` falha (ex: SSH não configurado no destino) THEN o sistema SHALL informar que a chave já foi gerada localmente, mostrar o caminho do arquivo, e instruir a cópia manual — sem tentar nenhum canal alternativo.

**Independent Test**: Rodar `keygen` entre duas VMs sem SSH configurado, confirmar fallback manual; configurar SSH, confirmar `scp` automático; capturar o tráfego de rede (`tcpdump`) durante uma sessão ativa e confirmar que não há payload de evento em texto claro.

---

### P1: Malha de N máquinas com papéis unificados ⭐ MVP

**User Story**: Como usuário com mais de duas máquinas, quero que qualquer uma delas possa ser fonte ou destino do controle, para não precisar decidir de antemão qual roda `capture` e qual roda `inject`.

**Why P1**: Decidido como parte do MVP único — sem isso, borda de tela com N máquinas não tem como funcionar (o modelo atual fixa os papéis).

**Acceptance Criteria**:

1. WHEN o `kvm-share` é iniciado em uma máquina THEN o sistema SHALL abrir os dispositivos `evdev` locais e escutar conexões de rede simultaneamente, sem exigir escolha de "modo" na inicialização.
2. WHEN uma máquina lê `~/.config/kvm-share/peers.toml` THEN o sistema SHALL reconhecer cada entrada com nome, `ip:porta`, PSK e direção relativa (`left`/`right`/`up`/`down`).
3. WHEN o controle está com a máquina A e o foco passa para a máquina B (peer válido em `peers.toml`) THEN o sistema SHALL parar de encaminhar eventos para B e A SHALL passar a receber (papel de destino) enquanto B assume a captura local.
4. WHEN uma máquina tenta se conectar sem estar listada no `peers.toml` do destino THEN o sistema SHALL rejeitar a conexão.

**Independent Test**: Configurar 3 VMs em malha (A-B, B-C), mover o foco de A para B e de B para C, e confirmar que cada uma alterna corretamente entre capturar localmente e receber eventos remotos.

---

### P1: Troca de controle por borda de tela ⭐ MVP

**User Story**: Como usuário, quero mover o cursor até a borda da tela para trocar automaticamente o controle para a máquina vizinha, sem precisar apertar Scroll Lock toda vez.

**Why P1**: Parte do MVP único decidido na interview; é a paridade de UX que faltava em relação ao Barrier/Synergy.

**Acceptance Criteria**:

1. WHEN o kvm-share está capturando localmente e o cursor acumula deltas suficientes para cruzar a borda configurada para um peer (direção em `peers.toml`) THEN o sistema SHALL transferir o foco para esse peer automaticamente.
2. WHEN o foco é transferido por borda de tela THEN o sistema SHALL reposicionar a origem do rastreamento de deltas na máquina que assume o controle, de forma consistente com o lado por onde o cursor entrou.
3. WHEN o usuário pressiona Scroll Lock THEN o sistema SHALL alternar o foco manualmente, independente do estado acumulado de deltas (via para casos de drift ou preferência do usuário).
4. WHEN não há peer configurado na direção em que o cursor tentou cruzar a borda THEN o sistema SHALL manter o controle local (sem erro visível ao usuário, cursor simplesmente para na borda).

**Independent Test**: Configurar 2 máquinas lado a lado (`right`/`left`), mover o cursor até a borda direita da máquina A e confirmar transferência automática para B; testar Scroll Lock isoladamente; testar borda sem peer configurado (ex: `up`) e confirmar que não transfere.

---

### P1: Clipboard sync na troca de foco ⭐ MVP

**User Story**: Como usuário, quero que o texto ou imagem que copiei numa máquina esteja disponível pra colar assim que o controle passa pra outra, para não precisar copiar/colar manualmente entre elas.

**Why P1**: Parte do MVP único decidido na interview.

**Acceptance Criteria**:

1. WHEN o foco muda de uma máquina para outra E o socket do `copied` (`$XDG_RUNTIME_DIR/copied.sock`) existe na máquina que perde o foco THEN o sistema SHALL ler o conteúdo atual do clipboard via `Command::GetLatestText` (novo command no `copied-core`) e enviá-lo, cifrado, para a máquina que ganha o foco.
2. WHEN a máquina que ganha o foco recebe conteúdo de clipboard E seu socket do `copied` existe THEN o sistema SHALL escrever esse conteúdo via `Command::CopyText` (texto) ou comando equivalente para imagem.
3. WHEN o socket do `copied` não existe em uma das pontas (não instalado ou não rodando) THEN o sistema SHALL pular a sincronização de clipboard silenciosamente (log informativo, não erro) e continuar a troca de foco normalmente.
4. WHEN o conteúdo do clipboard é uma imagem THEN o sistema SHALL sincronizar os bytes completos (não um preview truncado).
5. WHEN o `copied-core` é atualizado no repositório do `copied` THEN o kvm-share SHALL consumir essa lib como dependência git fixada em uma rev específica (não path dependency), preservando os dois projetos como aplicações independentes.

**Independent Test**: Com `copied` rodando nas duas máquinas, copiar um texto longo (>200 caracteres) e uma imagem na máquina A, trocar foco pra B, confirmar que ambos aparecem no clipboard de B; desligar o `copied` numa das máquinas e confirmar que a troca de foco continua funcionando sem erro.

---

## Edge Cases

- WHEN a conexão de rede cai no meio de uma sessão de captura ativa THEN o sistema SHALL devolver o controle local à máquina de origem (nunca deixar o usuário "sem mouse/teclado" em nenhuma máquina).
- WHEN duas máquinas tentam transferir o foco uma pra outra simultaneamente (ex: cursores cruzando bordas opostas ao mesmo tempo em topologias cíclicas) THEN o sistema SHALL resolver por prioridade determinística (ex: menor identificador de peer vence) sem deixar as duas em estado de captura simultânea.
- WHEN o arquivo `peers.toml` está malformado ou referencia uma PSK inexistente THEN o sistema SHALL falhar na inicialização com mensagem clara, não parcialmente.
- WHEN o conteúdo de clipboard a sincronizar é muito grande (ex: imagem grande) THEN o sistema SHALL aplicar um limite de tamanho e pular a sincronização com log, em vez de travar a troca de foco.
- WHEN `scp` do `keygen` é executado contra um host já pareado (PSK já existe no destino) THEN o sistema SHALL avisar e pedir confirmação antes de sobrescrever.

---

## Requirement Traceability

| Requirement ID | Story | Phase | Status |
| --- | --- | --- | --- |
| CRYPTO-01 | P1: Canal de rede criptografado | Design | Pending |
| CRYPTO-02 | P1: Canal de rede criptografado | Design | Pending |
| CRYPTO-03 | P1: Canal de rede criptografado | Design | Pending |
| CRYPTO-04 | P1: Canal de rede criptografado | Design | Pending |
| CRYPTO-05 | P1: Canal de rede criptografado | Design | Pending |
| MESH-01 | P1: Malha de N máquinas | Design | Pending |
| MESH-02 | P1: Malha de N máquinas | Design | Pending |
| MESH-03 | P1: Malha de N máquinas | Design | Pending |
| MESH-04 | P1: Malha de N máquinas | Design | Pending |
| EDGE-01 | P1: Troca de controle por borda | Design | Pending |
| EDGE-02 | P1: Troca de controle por borda | Design | Pending |
| EDGE-03 | P1: Troca de controle por borda | Design | Pending |
| EDGE-04 | P1: Troca de controle por borda | Design | Pending |
| CLIP-01 | P1: Clipboard sync | Design | Pending |
| CLIP-02 | P1: Clipboard sync | Design | Pending |
| CLIP-03 | P1: Clipboard sync | Design | Pending |
| CLIP-04 | P1: Clipboard sync | Design | Pending |
| CLIP-05 | P1: Clipboard sync | Design | Pending |

**Coverage:** 18 total, 0 mapped to tasks, 18 unmapped ⚠️ (aguardando `/design` e `/taskify`)

---

## Success Criteria

- [ ] `tcpdump` numa sessão ativa não revela nenhum byte de evento em texto claro.
- [ ] Malha de 3 máquinas reais (não só VMs) funciona com transferência de foco por borda em ambas as direções configuradas.
- [ ] Clipboard (texto >200 caracteres e imagem) sincroniza corretamente na troca de foco entre duas máquinas com `copied` instalado.
- [ ] Ausência do `copied` numa máquina não impede o uso do KVM (degradação graciosa confirmada em teste manual).
