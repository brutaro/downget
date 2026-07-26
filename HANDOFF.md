# Handoff — downget

## Planejamento vinculado

Este arquivo permanece a **fonte canônica de entrega** do downget. O documento complementar [PRD do downget](docs/planning/prd-downget-2026-07-26/prd.md) traduz o escopo abaixo em requisitos verificáveis, prioridades, riscos e marcos; ele não substitui as decisões válidas deste handoff.

- **Estado do PRD e arquitetura:** especificação final e [arquitetura implementável do MVP](docs/architecture.md) aprovadas e implementadas. As decisões de cancelamento, URLs assinadas, checksum, fontes sem Range, nome de destino, Range, retry, configuração, persistência, coordenação entre processos e testes locais foram incorporadas.
- **Fluxo Software Forge:** discovery → especificação → arquitetura → build → teste. Todas as etapas do MVP foram concluídas, incluindo a matriz de evidências AC-1 a AC-19.
- **Gate Sentinel:** aprovado em 2026-07-26 após reprodução independente de formatação, 42 testes automatizados (12 unitários e 30 de integração) e lint estrito; o cenário crítico de SIGINT seguido de retomada foi repetido cinco vezes após a correção de durabilidade.
- **Nota conectada:** `downget-direcionamento-de` foi reconciliada e não alterou o escopo canônico.


## Objetivo

Construir um downloader de linha de comando para macOS, confiável para arquivos grandes e conexões instáveis. O programa deve baixar arquivos HTTP(S) em blocos paralelos quando o servidor suportar `Range Requests`, persistir o progresso em disco e retomar apenas os blocos ausentes após interrupções.

O produto é **CLI-first**: não haverá interface gráfica. A experiência deve ser clara no terminal, com barras de progresso, velocidade, ETA, conexões ativas e erros compreensíveis.

## Contexto do problema

Downloads grandes iniciados no navegador estão sendo interrompidos e nem sempre podem ser retomados. Gerenciadores testados com extensão de navegador também falharam em links do OneDrive.

Pontos importantes:

- Dividir o arquivo em partes pode acelerar transferências, mas não deve vir antes da estabilidade. Alguns servidores limitam muitas conexões paralelas.
- Links de compartilhamento do OneDrive podem ser páginas intermediárias, depender de sessão/cookies ou expirar. Nenhum cliente HTTP consegue retomar um link expirado sem receber uma URL válida nova.
- O primeiro objetivo é um downloader robusto para URLs diretas. Captura automática no navegador é uma segunda fase.

## Escopo do MVP

Implementar um binário chamado `downget` com:

```text
downget add <URL> [--output <arquivo-ou-diretório>] [--sha256 <64-hex>]
downget list
downget resume <ID> [--url <nova-url>] [--sha256 <64-hex>]
downget cancel <ID> [--discard]
downget config set concurrency <1..8>
```

Comportamentos esperados:

1. `add` inicia o Job no processo atual. Ao adicionar uma URL, seguir redirecionamentos e identificar nome, tamanho, `ETag`, `Last-Modified` e indício de `Accept-Ranges: bytes` quando disponíveis. `--output` tem prioridade; ao derivar o nome, sanitizar `Content-Disposition` e, sem nome seguro, usar `download-<id>`. Nunca sobrescrever destino existente.
2. Uma Fonte só é confirmada para Transferência Segmentada após uma requisição de confirmação por intervalo retornar `206` com `Content-Range` coerente com o intervalo solicitado e o tamanho total. Cabeçalho `Accept-Ranges` é apenas indício. Durante a confirmação, resposta `200` ou tamanho total desconhecido inicia Transferência Simples do zero, sem aceitar corpo como Segmento. Depois de iniciada a segmentação, `200`, `Content-Range` inválido ou `416` interrompe os intervalos, não aceita o corpo afetado, persiste estado seguro de falha e não entrega arquivo final até nova inspeção/retomada segura. Para Fonte confirmada, iniciar com 2 conexões por padrão e dividir o arquivo em blocos. `downget config set concurrency <1..8>` define a concorrência global persistida para novos Jobs; o limite é 1 a 8.
3. Para fontes sem Range, usar uma única transferência com reintentos; informar explicitamente que retomada por blocos não está disponível.
4. Gravar o arquivo parcial em `<destino>.part` e metadados em `<destino>.downget.json` ou em SQLite. O estado precisa sobreviver a fechamento do terminal, queda de energia e reinício do programa.
5. Em `resume`, validar que a origem ainda corresponde ao arquivo original (preferir `ETag`, depois tamanho e `Last-Modified`). Não concatenar dados de uma versão diferente do arquivo.
6. Aplicar no máximo 5 tentativas totais por requisição ou Segmento, incluindo a tentativa inicial, com espera progressiva e jitter para falhas transitórias, timeout, 408, 429 e 5xx. Respeitar `Retry-After` quando presente. Ao esgotar o limite, registrar falha terminal e Estado Persistente retomável quando aplicável.
7. Quando todos os blocos terminarem, validar tamanho final; aceitar checksum exclusivamente como `--sha256 <64-hex>` em `add` ou `resume`, normalizar o hexadecimal, persistir a expectativa e validar SHA-256 antes de renomear `.part` para o nome final. Não aceitar substituição por checksum diferente do já registrado.
8. Tratar `Ctrl+C` de forma segura: parar novas requisições, persistir o estado e deixar o download retomável.
9. Para Job ativo, `downget cancel <id>` confirma a parada das requisições em andamento, persiste o estado e então pausa, preservando `.part` e metadados. O descarte ocorre somente em `downget cancel <id> --discard`; a presença de `--discard` é a confirmação explícita, a operação é irreversível e deve ser documentada. Em Job ativo, descarte só acontece após a parada ser confirmada; se ela não puder ser confirmada, nenhum dado é descartado.
10. Após falha definitiva de Fonte sem Range, preservar parcial e estado até `resume`. Nesse comando, informar que a retomada por bytes não é possível, descartar o parcial antigo com segurança e reiniciar a Transferência Simples do zero automaticamente.
11. URL assinada fica somente em memória. Após qualquer reinício de processo de Job, `resume <id>` exige `--url <nova-url>` mesmo sem 403; o Estado Persistente guarda somente marcador/redação da URL. A nova Fonte precisa passar na validação de identidade antes de reaproveitar Segmentos.

## Experiência de terminal

Durante um download ativo, mostrar uma tela atualizada sem excesso de logs:

```text
ubuntu.iso       62.4%  ████████████░░░░░░  38.2 MB/s  ETA 04:12
5.3 GB / 8.5 GB  |  2/2 conexões ativas  |  tentativa 0  |  retomável
```

Em erro, informar causa e próxima ação concreta, por exemplo:

```text
Pausado: a URL retornou 403 e pode ter expirado.
Use `downget resume 42 --url "NOVA_URL"` para continuar os blocos já válidos.
```

## Arquitetura recomendada

Usar **Rust** pela segurança de memória, concorrência previsível e binário distribuível.

Bibliotecas sugeridas:

- `tokio`: runtime assíncrono e cancelamento;
- `reqwest` com `rustls`: HTTP(S), redirects e headers;
- `clap`: CLI;
- `indicatif`: barras de progresso;
- `serde` + `serde_json`: metadados;
- `rusqlite` ou arquivos JSON atômicos: estado persistente;
- `sha2`: checksum;
- `tracing` + `tracing-subscriber`: logs opcionais (`--verbose`).

Modelo interno:

```text
DownloadJob
  id, URL atual ou marcador de URL assinada, URL original redigida, destino, tamanho, ETag, Last-Modified, SHA-256 esperado
  status, política de retry (tentativas usadas/limite), número de conexões

Segment
  início, fim, bytes concluídos, status, tentativas
```

Os segmentos devem escrever em offsets definidos do mesmo `.part`, sem concatenar arquivos ao final. Persistir o progresso de forma atômica após blocos concluídos e em intervalos curtos durante transferências longas.

## OneDrive e autenticação

Não prometer suporte universal a links do OneDrive no MVP.

- Uma URL de página de compartilhamento não é necessariamente a URL direta do arquivo.
- Links diretos assinados ficam somente em memória e não vão ao Keychain nem ao Estado Persistente. Após qualquer reinício de processo, `resume --url <nova-url>` é obrigatório, mesmo sem 403, e reaproveita somente os segmentos cuja identidade do arquivo ainda coincida.
- URLs que exigem cookies, `Authorization` ou outros headers ficam fora do MVP inicial.

Fase posterior: extensão Chrome mínima + Native Messaging. A extensão captura a URL final e, quando necessário, cookies/headers permitidos pelo usuário, entregando-os ao CLI local. Não registrar nem exibir cookies, tokens ou URLs assinadas em logs. Restringir o host nativo ao ID da extensão e guardar segredos no Keychain do macOS, se forem persistidos.

## Não escopo inicial

- Interface gráfica;
- extensão de navegador;
- BitTorrent, HLS, FTP, SFTP ou download de vídeo;
- login OAuth/integração com a API do OneDrive;
- múltiplas URLs espelho para o mesmo arquivo;
- execução como serviço em segundo plano.

## Estrutura inicial sugerida

```text
downget/
  Cargo.toml
  README.md
  src/
    main.rs
    cli.rs
    download.rs
    segments.rs
    state.rs
    retry.rs
    progress.rs
    error.rs
  tests/
    range_server.rs
    resume.rs
    retries.rs
```

## Plano de implementação

1. Criar o projeto Rust, comandos `add` e download simples de uma conexão, com barra de progresso.
2. Implementar arquivo `.part`, metadados e retomada sequencial por Range.
3. Implementar segmentação fixa de 2 conexões, escrita por offset e persistência de segmentos.
4. Adicionar política de retry, cancelamento seguro e mensagens de erro.
5. Adicionar testes com servidor HTTP local que simule: suporte/ausência de Range, queda no meio da transferência, 429/503, ETag alterado e arquivo corrompido.
6. Documentar instalação e exemplos no README.

## Critérios de aceite do MVP

- Baixa um arquivo HTTP de vários GB de um servidor com Range usando 2 conexões.
- Ao interromper o processo no meio e executar `resume`, transfere somente os bytes faltantes.
- Após falha de rede simulada, retoma automaticamente e conclui sem corromper o arquivo.
- Quando o servidor não suporta Range, deixa isso claro e não finge que poderá retomar.
- Não expõe tokens, cookies, headers sensíveis ou URLs assinadas em saída padrão, logs ou arquivos de estado.
- `cargo test` cobre os cenários de retomada e falha essenciais.

## Decisão de produto

Priorizar **confiabilidade sobre número de conexões**. A configuração padrão deve ser conservadora (2 conexões) e o programa deve reduzir a concorrência ou avisar o usuário quando o servidor rejeitar requisições paralelas. O objetivo é recuperar downloads difíceis, não apenas maximizar velocidade.
