# downget

`downget` é um downloader HTTP(S) para macOS voltado a arquivos grandes e
conexões instáveis. Ele confirma `Range Requests` antes de usar segmentos,
grava em `<destino>.part` e só promove o arquivo após validar tamanho e o
SHA-256 opcional.

## Instalação

Requer Rust estável e ferramentas de compilação do macOS:

```sh
cargo install --path .
```

O estado local fica em `~/Library/Application Support/downget/state.sqlite3`.
Para testes automatizados, `DOWNGET_STATE_DIR` pode apontar para um diretório
temporário; isso não é necessário no uso normal.

## Uso

```sh
downget add 'https://example.org/archive.iso'
downget add 'https://example.org/archive.iso' --output ~/Downloads/archive.iso
downget add 'https://example.org/archive.iso' --sha256 0123...cdef
downget list
downget resume 42
downget resume 42 --url 'https://example.org/new-signed-link'
downget cancel 42
downget cancel 42 --discard
downget config set concurrency 2
```

`cancel` pausa e preserva o arquivo parcial e o estado. `cancel --discard`
remove ambos de forma irreversível, somente depois de confirmar a parada do
Job.

URLs com query string, fragmento ou credenciais são tratadas como efêmeras:
elas nunca são gravadas no SQLite ou exibidas na listagem. Após encerrar o
processo, retome esse tipo de Job somente com `resume <ID> --url <NOVA_URL>`.

## Links públicos do OneDrive

`downget` também aceita links públicos curtos `https://1drv.ms/...`. Ele usa o
primeiro redirecionamento HTTPS exato para `https://onedrive.live.com/redir`,
identifica o tipo do item pelo `ithint` e tenta a rota pública de download.
Os parâmetros do link só existem em memória durante a execução: o Job é
sempre tratado como efêmero e uma retomada exige `--url`.

- Links de arquivo precisam ser entregues como `Content-Disposition:
  attachment`; páginas HTML/XHTML e respostas ambíguas são recusadas.
- Links de pasta podem ser entregues pela Microsoft como um arquivo ZIP. Se o
  provedor não entregar um anexo (`Content-Disposition: attachment`) com
  `application/zip` ou nome `.zip`, o download é interrompido antes de criar
  um `.part`.
- Respostas `text/html` ou XHTML são recusadas como páginas de aterrissagem,
  nunca salvas como se fossem o arquivo pedido.
- Se a Microsoft responder 403, compartilhe como **Anyone with the link** com
  download permitido, ou use uma URL direta de arquivo.

Não há login, cookies, OAuth, Graph API ou automação de navegador neste fluxo.

## Garantias do MVP

- Confirma segmentação por `GET Range: bytes=0-0` com `206` e
  `Content-Range` coerente; `Accept-Ranges` sozinho não basta.
- Usa duas conexões por padrão (configurável de 1 a 8) e reduz para uma após
  429/503 durante trabalho paralelo.
- Limita a cinco tentativas totais por requisição/segmento, com backoff e
  suporte a `Retry-After`.
- Valida identidade por ETag forte, ou por tamanho e `Last-Modified`, antes de
  reutilizar segmentos de uma fonte substituta.
- Nunca sobrescreve o destino final existente e não promove `.part` inválido.

O MVP aceita URLs HTTP(S) diretas e links públicos curtos `1drv.ms`; cookies,
autenticação, OneDrive por navegador, daemon e GUI ficam fora do escopo.
