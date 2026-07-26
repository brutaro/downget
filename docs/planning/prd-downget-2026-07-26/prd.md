---
title: "PRD — downget: downloader CLI resiliente para macOS"
status: final
created: 2026-07-26
updated: 2026-07-26
canonical_handoff: "../../../HANDOFF.md"
---

# PRD — downget: downloader CLI resiliente para macOS

## 0. Propósito, status e leitura

Este PRD torna verificável o escopo canônico de [HANDOFF.md](../../../HANDOFF.md) para quem fará a arquitetura, a implementação e os testes do `downget`. Ele descreve o comportamento observável do produto; não escolhe formato de persistência, bibliotecas ou estrutura interna quando essas escolhas não são necessárias para o usuário obter o resultado.

O produto está no fluxo Software Forge **discovery → especificação → arquitetura → build → teste**. Este documento cobre especificação e não autoriza código nesta etapa. As decisões de cancelamento, URL assinada, checksum, Fonte sem intervalos, nome de destino, detecção de intervalos, retry, configuração e testes locais estão fechadas. A nota `downget-direcionamento-de` foi reconciliada sem impacto de escopo. Em qualquer conflito, o `HANDOFF.md` permanece a fonte canônica.

## 1. Problema e tese de produto

Downloads grandes por HTTP(S) falham em conexões instáveis e frequentemente deixam o usuário sem uma forma confiável de continuar do ponto em que parou. A falha é especialmente frustrante quando o arquivo é grande, o servidor limita conexões ou o link recebido é temporário.

O `downget` é um utilitário **CLI-first para macOS** que baixa uma Fonte HTTP(S) direta com prioridade em continuidade e integridade. Quando a Fonte suporta solicitações por intervalo de bytes, ele baixa Segmentos em paralelo de forma conservadora; quando não suporta, informa a limitação e realiza uma transferência simples com reintentos. Em ambos os casos, ele deve comunicar o estado com clareza e jamais apresentar como válido um Arquivo Parcial ou dados de uma Fonte diferente.

**Tese do MVP:** a recuperação confiável de um download interrompido vale mais do que aumentar o número de conexões. Por isso, o padrão é duas conexões e a concorrência é limitada a 1–8.

## 2. Usuários, necessidades e limites de público

### 2.1 Usuário-alvo

O usuário-alvo é uma pessoa em macOS que trabalha no terminal e precisa transferir um arquivo grande por uma URL HTTP(S) direta, acompanhar o andamento e recuperar o trabalho após uma interrupção.

### 2.2 Jobs to be Done

- Quando uma transferência grande cai, quero retomar somente o que falta para não desperdiçar tempo, banda ou dados já corretos.
- Quando o servidor não permite retomada por intervalos, quero saber isso antes de confiar na recuperação automática.
- Quando recebo um erro de URL expirada ou de servidor, quero uma explicação e o próximo comando seguro a executar.
- Quando concluo uma transferência, quero confiar que o arquivo tem o tamanho esperado e, se eu fornecer um SHA-256, que ele foi validado antes de ser entregue como final.

### 2.3 Não usuários no MVP

- Quem precisa de login, cookies, cabeçalhos `Authorization` ou automação de navegador.
- Quem baixa BitTorrent, HLS, FTP, SFTP ou mídia de serviços de vídeo.
- Quem busca interface gráfica, daemon em segundo plano ou múltiplos espelhos.

### 2.4 Casos de uso essenciais

- **UC-1 — iniciar Fonte com intervalos:** o usuário adiciona uma URL HTTP(S) direta e observa download segmentado com duas conexões, progresso e estimativa.
- **UC-2 — lidar com Fonte sem intervalos:** o usuário adiciona uma Fonte sem `Range Requests`, recebe aviso explícito e acompanha uma transferência simples com reintentos.
- **UC-3 — retomar após interrupção:** o usuário interrompe o processo e usa o identificador do Job para continuar somente os Segmentos ausentes, depois de validar a Identidade da Fonte.
- **UC-4 — substituir URL expirada:** após um 403 de URL temporária, o usuário informa uma nova URL em `resume`; o produto reaproveita apenas dados cuja Identidade da Fonte coincida.
- **UC-5 — diagnosticar e agir:** o usuário consulta a lista de Jobs, identifica o estado e executa a próxima ação indicada sem precisar interpretar logs técnicos.

## 3. Glossário

- **Fonte** — recurso HTTP(S) solicitado pelo usuário. Pode envolver URL original, redirecionamentos e URL atual; no MVP, deve ser uma fonte direta e sem autenticação.
- **Identidade da Fonte** — evidência usada para impedir a combinação de versões diferentes do arquivo: `ETag` tem precedência; na ausência dele, tamanho e `Last-Modified` são usados em conjunto quando disponíveis.
- **Job** — registro persistente de uma transferência, identificado por ID, Fonte, destino, estado, política de retry e número de conexões.
- **Segmento** — intervalo contíguo de bytes de uma Fonte com suporte a `Range Requests`, com início, fim, bytes concluídos, estado e tentativas.
- **Arquivo Parcial** — arquivo `<destino>.part` que recebe bytes antes da verificação final; não é uma entrega concluída.
- **Estado Persistente** — metadados do Job e dos Segmentos que sobrevivem ao encerramento do terminal, queda de energia e reinício do programa.
- **Retomável** — Job cujo Estado Persistente e cuja Identidade da Fonte permitem continuar sem combinar dados de fontes diferentes.
- **Transferência Simples** — transferência de uma única conexão, usada quando a Fonte não oferece `Range Requests`; ela não é Retomável por Segmentos.
- **Transferência Segmentada** — transferência de uma Fonte com `Range Requests` por Segmentos gravados em offsets definidos do mesmo Arquivo Parcial.
- **Finalização** — validação de tamanho e, quando fornecido, SHA-256, seguida da promoção do Arquivo Parcial ao destino final.

## 4. Escopo do MVP e prioridades

### 4.1 P0 — necessário para uma entrega confiável

- Comandos `add`, `list`, `resume`, `cancel` e `config set`.
- Inspeção da Fonte e detecção de suporte a `Range Requests`.
- Transferência Simples com retry e Transferência Segmentada com duas conexões por padrão.
- Arquivo Parcial, Estado Persistente, retomada validada e desligamento seguro por `Ctrl+C`.
- Validação de tamanho; SHA-256 quando fornecido; proteção contra mistura de fontes.
- Mensagens de terminal, privacidade de valores sensíveis e testes de falhas essenciais.

### 4.2 P1 — somente após o MVP confiável

- Extensão Chrome mínima com Native Messaging para captar URL final e, quando autorizado pelo usuário, dados de sessão.
- Armazenamento de segredos no Keychain do macOS caso a fase de extensão exija persistência.

### 4.3 Fora de escopo no MVP

- Interface gráfica.
- Captura automática no navegador, cookies, `Authorization`, OAuth ou integração com API do OneDrive.
- BitTorrent, HLS, FTP, SFTP, download de vídeo e múltiplas URLs espelho.
- Serviço/daemon em segundo plano.

## 5. Contrato de CLI e experiência no terminal

### 5.1 Superfície pública mínima

```text
downget add <URL> [--output <arquivo-ou-diretório>] [--sha256 <64-hex>]
downget list
downget resume <ID> [--url <NOVA_URL>] [--sha256 <64-hex>]
downget cancel <ID> [--discard]
downget config set concurrency <1..8>
```

Depois de qualquer reinício de processo de Job com URL assinada, `resume <ID>` exige `--url <NOVA_URL>`, mesmo sem 403. Checksum é aceito exclusivamente como `--sha256 <64-hex>` em `add` e `resume`; `cancel` preserva por padrão e somente `--discard` descarta dados após parada confirmada. `downget config set concurrency <1..8>` define a concorrência global persistida. Esses comportamentos são contrato de MVP, não decisões para o build.

### 5.2 Exibição durante transferência

Durante uma Transferência ativa, o terminal deve ser atualizado sem excesso de logs e mostrar, no mínimo: nome do destino, percentual, progresso em bytes, velocidade, ETA quando calculável, conexões ativas, tentativa atual e indicação de Retomável. A forma visual pode variar, desde que preserve essas informações e não exponha valores sensíveis.

Exemplo informativo esperado:

```text
ubuntu.iso       62.4%  ████████████░░░░░░  38.2 MB/s  ETA 04:12
5.3 GB / 8.5 GB  |  2/2 conexões ativas  |  tentativa 0  |  retomável
```

### 5.3 Linguagem de erro e retomada

Erros recuperáveis devem informar causa observável e próxima ação concreta. Para uma Fonte que retorna 403 e pode ter expirado, a orientação deve equivaler a:

```text
Pausado: a URL retornou 403 e pode ter expirado.
Use `downget resume 42 --url "NOVA_URL"` para continuar os blocos já válidos.
```

O ID, o status HTTP e a sugestão podem mudar conforme o contexto; a mensagem não pode revelar cookies, tokens, cabeçalhos sensíveis ou URL assinada.

## 6. Requisitos funcionais

### 6.1 Gestão de Job e inspeção de Fonte

#### FR-1 — Adicionar uma Fonte HTTP(S) e resolver destino

O usuário inicia um Job no processo atual com `downget add <URL>` e indica destino com `--output` quando necessário. A Fonte deve ser HTTP(S) direta e não autenticada no MVP. `--output` tem prioridade sobre o nome derivado. Quando o destino for diretório ou não houver arquivo explícito, o produto sanitiza o nome de `Content-Disposition`; sem nome confiável, usa `download-<id>`. O produto não sobrescreve um destino existente. Realiza UC-1 e UC-2.

**Consequências verificáveis:**

- URL fora do protocolo aceito recebe erro claro e não cria Job ativo.
- Uma Fonte aceita cria um Job identificável por ID antes ou no início da transferência.
- Um `Content-Disposition` malicioso ou inutilizável não produz caminho fora do destino escolhido; o fallback é `download-<id>`.
- Se o destino já existir, o Job não o sobrescreve e a CLI informa a colisão.

#### FR-2 — Seguir redirecionamentos e inspecionar capacidades

Ao adicionar ou retomar uma Fonte, o produto segue redirecionamentos e coleta, quando disponíveis, nome, tamanho, `ETag`, `Last-Modified` e `Accept-Ranges: bytes` como indício. A confirmação de Transferência Segmentada pertence a FR-5 e não pode ser inferida apenas pelo cabeçalho.

**Consequências verificáveis:**

- Uma cadeia de redirecionamentos chega à Fonte final ou produz erro acionável.
- A ausência de um metadado não é reportada como se ele tivesse sido validado.

#### FR-3 — Listar Jobs acionáveis

`downget list` mostra cada Job com ID, destino, estado, progresso quando existir e a informação necessária para distinguir concluído, ativo, Retomável, não Retomável e dependente de ação do usuário. Realiza UC-5.

**Consequências verificáveis:**

- O usuário consegue selecionar um ID existente para `resume` ou `cancel` sem consultar arquivos internos.
- A listagem não imprime valores sensíveis da Fonte.

#### FR-4 — Configurar concorrência global conservadora

O produto usa duas conexões por padrão em Transferência Segmentada. `downget config set concurrency <1..8>` define a concorrência global, persistida entre processos, para novos Jobs; valor fora desse intervalo falha com mensagem clara e não altera a configuração válida existente.

**Consequências verificáveis:**

- Uma instalação sem configuração explícita inicia uma Fonte com intervalos com duas conexões.
- Um processo novo observa o valor global configurado ao criar um Job novo.
- Os limites 1 e 8 são aceitos; 0 e 9 são rejeitados.

### 6.2 Estratégia de transferência e controle de concorrência

#### FR-5 — Confirmar intervalos por resposta HTTP antes de segmentar

O produto só confirma `Range Requests` quando uma requisição de confirmação com intervalo recebe `206` e `Content-Range` é coerente com início, fim e tamanho total esperados. Somente então divide tamanho total conhecido em Segmentos e usa a concorrência configurada; os Segmentos escrevem em offsets definidos do mesmo Arquivo Parcial. Durante a confirmação, resposta `200` ou tamanho total desconhecido marca a Fonte como não segmentável nessa execução e inicia Transferência Simples do byte zero, sem aceitar o corpo como Segmento. Depois de iniciada a segmentação, `200`, `Content-Range` inválido ou `416` para intervalo esperado interrompe requisições por intervalo, não aceita o corpo afetado nem marca Segmento como concluído, persiste estado seguro de falha e impede Finalização até nova inspeção/retomada segura. Realiza UC-1.

**Consequências verificáveis:**

- Duas solicitações de intervalo distintas podem permanecer ativas no padrão somente após confirmação `206`/`Content-Range`, sem concatenar arquivos temporários ao final.
- Os bytes de cada Segmento ocupam somente seu intervalo definido no Arquivo Parcial depois de validação de `Content-Range`.
- Durante a confirmação, `200` ou tamanho total desconhecido leva a Transferência Simples do byte zero; depois de iniciada a segmentação, `200`, `Content-Range` inválido ou `416` deixa Job persistido em falha segura, sem arquivo final.

#### FR-6 — Usar Transferência Simples sem intervalos e reiniciar com segurança

Quando a Fonte não suporta `Range Requests`, o produto usa uma Transferência Simples de uma conexão com retry e informa explicitamente que a retomada por Segmentos não está disponível. Após falha definitiva, preserva Arquivo Parcial e Estado Persistente até `downget resume <ID>`. Nesse comando, informa que não pode retomar bytes, descarta o Arquivo Parcial antigo com segurança e reinicia a Transferência Simples do zero automaticamente. Realiza UC-2.

**Consequências verificáveis:**

- Nenhuma solicitação `Range` é apresentada como estratégia de retomada para essa Fonte.
- A saída deixa claro que a operação não é Retomável por Segmentos.
- Após falha definitiva, Arquivo Parcial e Estado Persistente permanecem disponíveis até `resume`.
- `resume` para esse Job comunica a reinicialização, remove o parcial antigo com segurança e inicia do byte zero; não afirma que reaproveitou bytes.

#### FR-7 — Reagir a rejeição de paralelismo

Quando a Fonte rejeita ou limita requisições paralelas, o produto reduz a concorrência ou avisa o usuário de forma acionável; não deve continuar fingindo que o paralelismo solicitado é saudável.

**Consequências verificáveis:**

- Um servidor de teste que rejeita intervalos paralelos produz redução controlada ou aviso explícito.
- O estado do Job registra a condição necessária para o usuário decidir a próxima ação.

### 6.3 Persistência, interrupção e retomada segura

#### FR-8 — Manter Arquivo Parcial, Estado Persistente e expectativa de checksum

Cada Job grava o conteúdo em `<destino>.part` e mantém Estado Persistente suficiente para identificar a Fonte, o progresso, cada Segmento, tentativas usadas e, quando informado, o SHA-256 esperado já normalizado. Para URL assinada, o Estado Persistente guarda somente marcador/redação, nunca a URL; ela fica somente em memória do processo. A persistência é atômica após Segmentos concluídos e em intervalos durante transferências longas.

**Consequências verificáveis:**

- Encerramento inesperado não torna o Estado Persistente parcialmente legível como se fosse válido.
- Ao reiniciar, o produto identifica quais Segmentos terminaram e quais ainda faltam.
- Após reinício de Job com URL assinada, o Estado Persistente não permite reconstruir a URL e sinaliza que `resume --url` é obrigatório.

#### FR-9 — Tratar `Ctrl+C` sem perder retomabilidade

Ao receber `Ctrl+C`, o produto para de iniciar novas requisições, persiste o Estado Persistente e deixa o Job em condição Retomável quando a Fonte permitir. Realiza UC-3.

**Consequências verificáveis:**

- Uma interrupção durante Transferência Segmentada não renomeia o Arquivo Parcial como final.
- `downget resume <ID>` retoma somente os Segmentos faltantes após validação de Fonte.

#### FR-10 — Validar Identidade da Fonte antes de retomar

Antes de reutilizar dados de um Job, o produto valida a Identidade da Fonte: prefere `ETag`; caso ele não exista, compara tamanho e `Last-Modified` quando disponíveis. Não pode concatenar dados de uma Fonte diferente. Realiza UC-3.

**Consequências verificáveis:**

- Uma mudança de `ETag` bloqueia a retomada e explica a divergência.
- Sem `ETag`, uma divergência de tamanho ou `Last-Modified` bloqueia a retomada quando esses metadados estiverem disponíveis.
- Se não houver evidência suficiente para validar a Identidade da Fonte, o produto não reutiliza Segmentos silenciosamente.

#### FR-11 — Retomar com URL substituta apenas quando segura

`downget resume <ID> --url <NOVA_URL>` pode trocar a URL atual de um Job, mas somente reaproveita Segmentos já gravados se a nova Fonte passar na validação de Identidade da Fonte. Depois de qualquer reinício de processo, Job cuja URL era assinada exige `--url <NOVA_URL>` mesmo que não tenha recebido 403; `resume <ID>` sem essa opção falha de modo seguro, sem solicitar a Fonte nem alterar o Estado Persistente. Realiza UC-4.

**Consequências verificáveis:**

- Uma nova URL para o mesmo arquivo permite continuar os Segmentos faltantes.
- Uma nova URL para arquivo diferente preserva o Arquivo Parcial sem o tratar como conclusão válida e indica a ação segura ao usuário.
- Após reinício de processo com URL assinada, ausência de `--url` produz erro acionável e não expõe nem reconstrói a URL anterior.

#### FR-12 — Pausar ou descartar um Job explicitamente

`downget cancel <ID>` pausa o Job indicado e preserva Arquivo Parcial e Estado Persistente para retomada. Para Job ativo, o produto solicita parada, aguarda confirmação de que as requisições em andamento cessaram e persiste o estado pausado antes de retornar sucesso. Somente `downget cancel <ID> --discard` remove esses dados; a presença de `--discard` é confirmação explícita, o descarte é irreversível e a documentação do comando deve declarar essa consequência. Para Job ativo, `--discard` primeiro executa a parada confirmada; se ela não puder ser confirmada, o comando falha sem descartar dados.

**Consequências verificáveis:**

- ID inexistente ou já finalizado recebe resposta inequívoca.
- Cancelamento de Job ativo só retorna sucesso após confirmação de parada e persistência do estado pausado.
- Sem `--discard`, nenhum Arquivo Parcial ou Estado Persistente é removido.
- Com `--discard`, Arquivo Parcial e Estado Persistente são removidos somente depois da parada confirmada; `resume <ID>` não encontra Job retomável.

### 6.4 Retry, validação e finalização

#### FR-13 — Limitar e persistir retries de falhas transitórias

O produto aplica no máximo cinco tentativas totais por requisição ou Segmento, incluindo a tentativa inicial, com espera progressiva e jitter a timeout, falhas transitórias, HTTP 408, 429 e 5xx. Ao esgotar esse orçamento, marca o Job ou Segmento em falha terminal, persiste tentativas usadas e o motivo, e não inicia nova tentativa automática.

**Consequências verificáveis:**

- Uma falha transitória simulada gera nova tentativa sem corromper Estado Persistente ou Arquivo Parcial.
- A interface mostra tentativa e estado sem despejar logs de cada operação bem-sucedida.
- A sexta tentativa não ocorre; o estado terminal e a contagem usada sobrevivem ao reinício do processo.

#### FR-14 — Respeitar `Retry-After`

Quando a resposta aplicável inclui `Retry-After`, o produto respeita o intervalo informado antes de nova tentativa.

**Consequências verificáveis:**

- Um servidor de teste com 429 e `Retry-After` permite verificar que a nova tentativa não ocorre antes do intervalo indicado.

#### FR-15 — Validar tamanho final

Quando todos os Segmentos ou a Transferência Simples terminarem, o produto valida o tamanho final conhecido antes de Finalização.

**Consequências verificáveis:**

- Divergência entre tamanho esperado e Arquivo Parcial impede a Finalização.
- Sem tamanho conhecido, o produto não afirma que executou uma validação de tamanho que não pôde executar.

#### FR-16 — Registrar e validar SHA-256 fornecido na CLI

O produto aceita checksum somente por `--sha256 <64-hex>` em `downget add` e `downget resume`. Trata hexadecimal sem diferença entre maiúsculas e minúsculas, persiste a expectativa normalizada e a valida antes da Finalização. Se um Job já tiver SHA-256 esperado, uma nova tentativa de informar valor diferente deve ser rejeitada sem substituir a expectativa armazenada. Divergência entre arquivo e expectativa impede a entrega como arquivo final.

**Consequências verificáveis:**

- Um checksum correto permite Finalização após as demais verificações.
- Um checksum incorreto deixa o arquivo fora do estado concluído e informa a falha.
- Um valor fora de 64 caracteres hexadecimais é rejeitado antes de criar ou alterar a expectativa do Job.
- O mesmo checksum em caixa diferente é equivalente; checksum normalizado diferente é rejeitado e não modifica o valor persistido.

#### FR-17 — Promover somente arquivo verificado

O produto renomeia o Arquivo Parcial para o destino final somente depois das verificações aplicáveis de tamanho e SHA-256. Realiza UC-1, UC-2 e UC-3.

**Consequências verificáveis:**

- Arquivo incompleto ou com checksum inválido não aparece no nome de destino final.
- Arquivo concluído respeita o destino escolhido pelo usuário.

### 6.5 Privacidade e erros

#### FR-18 — Proteger dados sensíveis e retomar URL assinada sem Keychain

O produto não expõe tokens, cookies, cabeçalhos sensíveis ou URLs assinadas em saída padrão, logs ou Estado Persistente. URL assinada fica somente em memória e o Estado Persistente mantém apenas marcador/redação. O MVP não persiste URL assinada no Keychain. Depois de qualquer reinício de processo de Job com URL assinada, o usuário deve fornecer `downget resume <ID> --url <NOVA_URL>`, mesmo sem 403; o produto só reaproveita Segmentos depois de validar a Identidade da Fonte.

**Consequências verificáveis:**

- Testes de saída, logs e arquivos de estado não encontram valores de teste classificados como sensíveis.
- A mensagem de 403 oferece retomada sem imprimir a URL substituta completa.
- O MVP não cria nem consulta entrada de Keychain para URL assinada.
- `resume` sem `--url` após reinício de Job com URL assinada falha de forma segura e não revela a URL anterior.
- `resume --url` após reinício não reaproveita Segmentos quando a Identidade da Fonte diverge.

#### FR-19 — Explicar erro e próxima ação

Para erros que exigem intervenção — em especial reinício de Job com URL assinada sem `--url`, 403 potencialmente expirado, divergência de Identidade da Fonte, ausência de suporte a intervalos, intervalo inválido/416 e orçamento de retry esgotado — o produto mostra causa resumida e próximo comando ou decisão possível. Realiza UC-4 e UC-5.

**Consequências verificáveis:**

- 403 potencialmente expirado orienta `resume <ID> --url <NOVA_URL>`.
- Reinício de Job com URL assinada sem `--url` orienta o mesmo comando, mesmo sem 403.
- Divergência de Fonte não recomenda continuar os bytes existentes como se fossem confiáveis.

## 7. Requisitos não funcionais

### NFR-1 — Integridade de dados

Estado Persistente e Arquivo Parcial devem sobreviver a interrupções sem promover arquivo incompleto ou misturar bytes de Fontes diferentes. A segurança de dados prevalece sobre a tentativa de manter alta concorrência.

### NFR-2 — Confiabilidade de rede

O produto deve tolerar as falhas transitórias definidas em FR-13 dentro do limite de cinco tentativas totais por requisição/Segmento e reiniciar a partir de Estado Persistente válido quando a Fonte for Retomável. Não há meta de velocidade mínima: a medida de sucesso é uma transferência correta e explicável.

### NFR-3 — Segurança e privacidade

O produto deve tratar como sensíveis cookies, tokens, cabeçalhos de autorização e URLs assinadas, não os exibindo nem persistindo em locais definidos por FR-18. O MVP não tenta autenticar em Fonte privada.

### NFR-4 — Usabilidade da CLI

O terminal deve mostrar progresso, conexões ativas, tentativa e retomabilidade em operação ativa; logs detalhados não podem esconder a informação operacional. Erros devem incluir a próxima ação concreta quando ela existir.

### NFR-5 — Plataforma e distribuição

O MVP é um binário de linha de comando para macOS. A decisão arquitetural de Rust, runtime assíncrono, cliente HTTP, armazenamento de estado e biblioteca de progresso será tratada na etapa de arquitetura; este PRD não fixa implementações.

### NFR-6 — Testabilidade e gate

O produto deve poder ser testado contra servidor HTTP local controlado que simule intervalos, ausência de intervalos, queda de conexão, 429/503, mudança de `ETag`, corrupção, resposta 200 a requisição por intervalo, `Content-Range` inválido, 416, tamanho total desconhecido e rejeição de paralelismo. Antes de entrega de código, a evidência dos critérios AC-1 a AC-19 deve passar pelo gate do Sentinel.

### 7.1 Plano de testes locais

O servidor HTTP local controlado deve cobrir todo o conjunto AC-1 a AC-19. Além dos cenários base, ele deve exercitar: reinício de processo de Job com URL assinada (AC-14); cancelamento de Job ativo por segundo processo e descarte após parada confirmada (AC-15); confirmação de intervalos e estados seguros para 200, `Content-Range` inválido, 416 e tamanho desconhecido (AC-16); esgotamento do orçamento de retry e `Retry-After` (AC-17); `list` sem dados sensíveis (AC-18); e rejeição de paralelismo com redução ou aviso persistido (AC-19).

## 8. Critérios de aceite verificáveis

| ID | Cenário e evidência esperada | Requisitos |
| --- | --- | --- |
| AC-1 | Com uma Fonte HTTP de vários GB que aceita intervalos, `add` inicia Transferência Segmentada com duas conexões por padrão; o servidor de teste observa intervalos distintos e o Arquivo Parcial recebe bytes nos offsets corretos. | FR-1, FR-2, FR-4, FR-5 |
| AC-2 | Com uma Fonte sem `Range Requests`, o produto usa uma única Transferência Simples, tenta falhas transitórias e informa que retomada por Segmentos não está disponível. Após falha definitiva, preserva parcial/estado; em `resume`, anuncia a reinicialização, descarta o parcial com segurança e baixa novamente do byte zero. | FR-2, FR-6, FR-13 |
| AC-3 | Após interrupção no meio de Transferência Segmentada, o Estado Persistente é válido; `resume <ID>` solicita somente os Segmentos/bytes faltantes e conclui sem rebaixar o arquivo final. | FR-8, FR-9, FR-17 |
| AC-4 | Se `ETag` mudar entre interrupção e retomada, a retomada é bloqueada; se não houver `ETag`, divergência de tamanho ou `Last-Modified` disponível também bloqueia. | FR-10 |
| AC-5 | `resume <ID> --url <NOVA_URL>` reaproveita dados quando a nova Fonte tem Identidade da Fonte compatível e os rejeita quando ela diverge. | FR-11, FR-19 |
| AC-6 | Timeout, 408, 429 e 5xx simulados acionam retry progressivo com jitter sem corromper o Job; 429 com `Retry-After` não é repetido antes do intervalo anunciado. | FR-13, FR-14 |
| AC-7 | `Ctrl+C` para novas requisições, não finaliza o Arquivo Parcial e mantém o Job Retomável quando a Fonte permite. | FR-9 |
| AC-8 | `add --sha256` e `resume --sha256` aceitam somente 64 caracteres hexadecimais, normalizam a caixa e persistem a expectativa. Valor equivalente em outra caixa é aceito; valor normalizado diferente para Job existente é rejeitado sem alterar a expectativa. Tamanho divergente ou SHA-256 incorreto não produz destino final; valor correto promove o Arquivo Parcial. | FR-8, FR-15, FR-16, FR-17 |
| AC-9 | A configuração de concorrência aceita 1–8, usa 2 na ausência de configuração e rejeita valores fora da faixa sem alterar o valor anterior. | FR-4 |
| AC-10 | Saída padrão, logs e Estado Persistente de cenários com dados de teste sensíveis não contêm cookies, tokens, cabeçalhos sensíveis nem URL assinada completa; o MVP não cria nem consulta entrada de Keychain para essas URLs. | FR-18 |
| AC-11 | Uma resposta 403 potencialmente expirada mostra estado pausado e instrução para `resume <ID> --url <NOVA_URL>`, sem expor a URL; Fonte substituta de identidade diferente não reaproveita Segmentos. | FR-11, FR-18, FR-19 |
| AC-12 | `cancel <ID>` pausa e preserva Arquivo Parcial/Estado Persistente, e `resume <ID>` encontra o Job. Somente `cancel <ID> --discard` descarta ambos de forma irreversível; após ele, `resume <ID>` não encontra Job retomável e a documentação declara o descarte. | FR-12 |
| AC-13 | `--output` tem prioridade; nome de `Content-Disposition` é sanitizado e, sem nome seguro, o destino é `download-<id>`. Um destino existente não é sobrescrito e a CLI informa a colisão. | FR-1 |
| AC-14 | Um Job de URL assinada é interrompido sem 403 e o processo encerra. O Estado Persistente contém somente marcador/redação; em novo processo, `resume <ID>` sem `--url` falha de modo seguro sem solicitar a URL anterior. Com `--url` de Identidade da Fonte compatível, somente Segmentos válidos são reutilizados. | FR-8, FR-11, FR-18, FR-19 |
| AC-15 | Um segundo processo executa `cancel <ID>` durante transferência ativa. O Job só retorna pausado após confirmação de parada e persistência; então `resume <ID>` o encontra. Com `cancel <ID> --discard`, dados só são removidos depois da mesma confirmação; se a parada não for confirmada, nenhum dado é descartado. | FR-12 |
| AC-16 | Um servidor local responde à requisição de confirmação por intervalo com: 206 e `Content-Range` coerente (segmentação permitida); 200 ou tamanho total desconhecido (Transferência Simples do byte zero, sem corpo aceito como Segmento). Depois de iniciada a segmentação, 200, `Content-Range` inválido ou 416 interrompe intervalos, persiste falha segura e não produz arquivo final. | FR-2, FR-5, FR-15 |
| AC-17 | Em timeout, 408, 429 e 5xx persistentes, cada requisição/Segmento faz no máximo cinco tentativas totais, respeita `Retry-After`, não inicia sexta tentativa e persiste contagem/motivo em estado terminal. | FR-13, FR-14 |
| AC-18 | `downget list` apresenta ID, destino, estado, progresso quando houver e próxima ação para Jobs ativos, pausados, falhos terminais, concluídos e dependentes de URL; não revela URL assinada, tokens, cookies ou cabeçalhos sensíveis. | FR-3, FR-18, FR-19 |
| AC-19 | Quando servidor local rejeita paralelismo, o produto reduz concorrência ou mostra aviso acionável; o Job persiste a condição e a escolha/resultante para `list` e retomada. | FR-7, FR-8, FR-19 |

## 9. Riscos, tensões e mitigação

| Risco ou tensão | Impacto | Mitigação exigida nesta fase |
| --- | --- | --- |
| Fonte anuncia ou responde intervalos de modo inconsistente, ou limita paralelismo | Corrupção, dados redundantes, falhas repetidas | Confirmar somente por 206/Content-Range; para 200/tamanho desconhecido usar Transferência Simples; para Content-Range inválido/416 persistir falha segura; reduzir concorrência ou avisar conforme FR-5 e FR-7. |
| Fonte muda enquanto o Job está pausado | Corrupção silenciosa | Validar Identidade da Fonte antes de qualquer retomada; nunca combinar dados diferentes. |
| URL de compartilhamento do OneDrive é página intermediária, exige sessão ou expira | MVP não consegue baixar ou retomar | Delimitar a Fonte direta e não autenticada; orientar URL nova quando aplicável; manter extensão/autenticação fora do MVP. |
| URL assinada expira e não pode aparecer nem persistir em Estado Persistente | Retomada exige intervenção do usuário | Não usar Keychain no MVP; exigir `resume --url <nova-url>` e validar Identidade da Fonte antes de reaproveitar Segmentos. |
| Queda de energia ou `Ctrl+C` durante gravação | Estado inconsistente ou arquivo considerado final | Persistência atômica, Arquivo Parcial separado e validação antes de Finalização. |
| Retentativas excessivas agravam rate limiting | Bloqueio do servidor ou experiência ruim | Espera progressiva, jitter e respeito a `Retry-After`. |
| Escopo cresce para navegador, login ou serviço em segundo plano | Atraso e riscos de segurança | Manter P1 e não objetivos explícitos; nenhuma dependência de navegador no MVP. |

## 10. Backlog e marcos de planejamento

| Marco | Resultado de planejamento/entrega | Itens prioritários | Condição de saída |
| --- | --- | --- | --- |
| M0 — Fechamento de discovery | Nota `downget-direcionamento-de` reconciliada sem alteração de escopo | Nenhum | Completo; especificação final liberada para arquitetura. |
| M1 — Contrato de CLI e Job | Superfície de comandos, configuração global, destino, ciclo observável do Job e inspeção de Fonte especificados | FR-1 a FR-4, FR-12, FR-16, FR-19 | Arquitetura consegue definir armazenamento e máquina de estados sem inventar contrato. |
| M2 — Recuperação básica | Transferência Simples, Arquivo Parcial, Estado Persistente, orçamento de retry e retomada segura definidos | FR-6, FR-8 a FR-10, FR-13 a FR-15 | Testes locais cobrem queda, retry terminal, reinicialização sem Range e identidade da Fonte. |
| M3 — Transferência Segmentada | Confirmação 206/Content-Range, Segmentos por offset, concorrência conservadora e retomada por URL substituta definidos | FR-5, FR-7, FR-11 | Cenários AC-1, AC-3 a AC-5, AC-14, AC-16 e AC-19 passam. |
| M4 — Integridade, UX e privacidade | Finalização, SHA-256, mensagens, URL assinada e cancelamento ativo definidos | FR-12, FR-16 a FR-19 | Cenários AC-8, AC-10 a AC-12, AC-15, AC-17 e AC-18 passam. |
| M5 — Pronto para build/test | Arquitetura aprovada, plano de testes local implementável e evidência para Sentinel definidos | NFR-1 a NFR-6, AC-1 a AC-19 | Gate de preparação aprova avanço para build; implementação ainda precisa passar o Sentinel. |

O backlog acima é sequencial porque confiabilidade de transferência depende da semântica de Estado Persistente e de Identidade da Fonte. Ele não é uma ordem para escrever código antes de M0 e da arquitetura.

## 11. Premissas e decisões ainda não fixadas

### Premissas confirmadas pelo handoff

- O MVP é CLI-first, para macOS e para Fontes HTTP(S) diretas.
- A prioridade é confiabilidade, não maximização de velocidade.
- O padrão de concorrência é duas conexões; a faixa permitida é 1–8.
- O arquivo final só pode ser promovido após verificações aplicáveis.
- Captura em navegador, autenticação e OneDrive universal não pertencem ao MVP.
- `cancel <ID>` pausa e preserva; `cancel <ID> --discard` é o único descarte, irreversível e explicitamente documentado.
- URLs assinadas não usam Keychain no MVP; depois de expiração, `resume --url` é obrigatório e depende de validação da Identidade da Fonte.
- SHA-256 entra somente por `--sha256 <64-hex>` em `add` ou `resume`, é normalizado e não pode ser substituído por valor diferente.
- Falha definitiva sem Range preserva parcial/estado até `resume`, que reinicia com segurança do byte zero.
- `--output` prevalece; Content-Disposition é sanitizado; o fallback é `download-<id>`; destinos existentes não são sobrescritos.
- `add` inicia o Job no processo atual; a configuração disponível é global em `config set concurrency <1..8>`.
- URL assinada permanece somente em memória; após reinício de processo, `resume --url` é obrigatório mesmo sem 403.
- Intervalos são confirmados somente por 206 com Content-Range coerente; o orçamento de retry é de cinco tentativas totais por requisição/Segmento.

## 12. Perguntas abertas indispensáveis

Não há lacunas materiais abertas. A nota `downget-direcionamento-de` foi reconciliada sem alterar o escopo.

## 13. Próximo passo no Software Forge

A próxima etapa é a arquitetura, que decide somente os meios técnicos ainda abertos — formato de Estado Persistente, estratégia de escrita atômica, modelo de estados, escolha de bibliotecas e plano de testes — sem alterar os requisitos ou não objetivos deste documento. Só então o fluxo avança para build e teste, com o Sentinel avaliando a evidência dos critérios de aceite.
