# kvm-share — Malha Criptografada com Borda de Tela e Clipboard Sync — Interview Decisions

**Date:** 2026-08-31
**Scope:** Evoluir o `kvm-share` para (1) protocolo de rede criptografado/autenticado, (2) suporte a N máquinas em malha, (3) troca de controle por detecção de borda de tela (além do Scroll Lock), e (4) sincronização de clipboard entre as máquinas, integrando com o daemon `copied` já existente.
**Source:** Informal discussion + código-fonte existente (`src/lib.rs`, `src/bin/capture.rs`, `src/bin/inject.rs`, `.specs/general-context.md`) e análise do projeto `copied` (`/home/marco/setup/copied`).

---

## Decisões

### Criptografia / Autenticação

- Protocolo de rede passa a usar **Noise Protocol, padrão `Noise_XX`, com PSK (chave pré-compartilhada) por par de máquinas**, via crate `snow`.
- **Geração e distribuição de chave:** `kvm-share keygen <ip-da-máquina-alvo>` gera a PSK localmente (`~/.config/kvm-share/psk`, permissão 600) e tenta enviar via `scp` automaticamente pra máquina de destino.
  - Se o `scp` falhar (sem SSH configurado no destino): avisa o usuário que a chave já existe localmente, mostra o path do arquivo, e instrui a copiar manualmente.
- **Rationale:** `Noise_XX`+PSK dá autenticação mútua e forward secrecy sem exigir gestão de certificados/CA — consistente com o porte do projeto (uso pessoal, doméstico). Automatizar o `scp` reduz fricção de pareamento sem abrir mão de exigir acesso SSH real (não inventa um canal alternativo inseguro).

### Topologia de N Máquinas

- **Config estática** por arquivo `~/.config/kvm-share/peers.toml`, listando nome, `ip:porta` e a PSK de cada peer. Sem descoberta automática (mDNS/broadcast).
- **Malha direta (peer-to-peer)** — cada máquina conecta diretamente nas vizinhas relevantes; sem hub/coordenador central.
- **Rationale:** como a PSK já é o mecanismo de pareamento, config estática não introduz uma fase de "anúncio" não-autenticada na rede (evita vazamento de metadado e superfície de código extra que a descoberta automática exigiria). Malha direta evita ponto único de falha e é suficiente porque o roteamento de foco é sempre uma decisão local entre máquinas adjacentes (borda de tela).

### Papéis capture/inject

- **Unificados em um único binário/daemon** (`kvm-share`) rodando em todas as máquinas simultaneamente. Cada instância abre dispositivos `evdev` locais e escuta rede ao mesmo tempo; qual papel está ativo com qual peer é decidido pelo estado de foco (quem tem o controle agora), não por qual binário foi iniciado.
- **Rationale:** com malha de N máquinas e borda de tela, o foco pode se mover em qualquer direção a qualquer momento — a distinção `capture`/`inject` como binários separados deixa de fazer sentido; vira um estado, não um processo.

### Detecção de Borda de Tela

- **Layout declarado manualmente** em `peers.toml`: cada peer ganha uma direção relativa (`left`/`right`/`up`/`down`) em relação à máquina atual.
- **Rastreamento de cursor por deltas:** o kvm-share acumula os deltas relativos do mouse (desde que assumiu o controle) contra a resolução da tela local; ao cruzar a borda configurada, transfere o foco pro peer daquele lado.
- **Convive com o Scroll Lock existente:** borda de tela é o método principal (automático); Scroll Lock permanece como atalho manual de emergência (útil em caso de drift acumulado no rastreamento por deltas, ou pra forçar a troca sem mover o cursor).
- **Rationale:** evita qualquer dependência de API de compositor Wayland (motivo original do projeto existir); manter o Scroll Lock é baixo custo (já implementado) e dá uma via de escape manual.

### Clipboard Sync

- **Integração com `copied` via `copied-core` como dependência git fixada em rev** (não path dependency) — kvm-share e `copied` seguem como produtos/repositórios independentes, cada um com seu próprio ciclo de release.
- **Dependência de runtime opcional:** kvm-share detecta se o socket do `copied` (`$XDG_RUNTIME_DIR/copied.sock`) existe; se não existir, clipboard sync fica desligado, mas o KVM (mouse/teclado) continua funcionando normalmente.
- **Patch necessário no `copied`:** adicionar `Command::GetLatestText` (ou equivalente) ao protocolo — hoje `Command::List` só devolve preview truncado em 200 caracteres, insuficiente pra encaminhar o conteúdo completo. `Command::CopyText` já cobre a escrita, sem necessidade de mudança.
- **Gatilho de sincronização:** no momento da troca de foco — a máquina que perde o foco lê o clipboard local (via `GetLatestText`) e envia pro peer que ganha o foco, que escreve via `CopyText`. Sem polling contínuo.
- **Escopo do MVP: texto + imagem** (não só texto).
- **Rationale:** dependência git fixada mantém os dois projetos desacoplados como aplicações independentes (cada um versiona e libera sozinho) mas com checagem de protocolo em compile-time, evitando duplicar structs à mão com risco de drift. Sync no momento da troca de foco cobre o caso de uso real sem manter canal extra sempre ativo. Imagem entra desde o MVP porque o `copied` já modela os dois tipos (`ItemKindView::Text`/`Image`) — a estrutura de dados já suporta, o esforço extra é só no framing de rede.

---

## Agent's Discretion

Nenhuma área foi delegada como "você decide" nesta interview — todas as decisões foram explicitamente escolhidas pelo usuário.

---

## Deferred Ideas

- Descoberta automática de peers via mDNS/broadcast — descartada em favor de config estática, mas pode ser revisitada se a malha crescer muito.
- Hub/coordenador central pra arbitrar foco — descartado em favor de malha direta.
- Detecção de borda via API absoluta do compositor (ex: alguma API específica do COSMIC) — descartada porque reintroduziria a dependência de compositor que o projeto existe pra evitar.
- Sync contínuo de clipboard (fora do momento de troca de foco) — descartado, sync só dispara na troca de foco.
- Mexer na arquitetura interna do `copied` além do necessário (novo command `GetLatestText`) — fora de escopo; o `copied` continua um produto independente.

## Open Questions

- Formato exato do framing de rede para o clipboard sync (texto + imagem, tamanho variável) — decisão de design técnico, não de produto; deve ser resolvida em `/design`.
- Detalhes do handshake `Noise_XX` (ordem de mensagens, quem inicia) — decisão de design técnico a resolver em `/design`.
- Nome final do novo command no `copied-core` (`GetLatestText` foi usado aqui como placeholder) e se essa mudança será feita pelo próprio usuário no repo do `copied` antes ou durante a implementação do kvm-share.
