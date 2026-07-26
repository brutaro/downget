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

O MVP aceita somente URLs HTTP(S) diretas; cookies, autenticação, OneDrive por
navegador, daemon e GUI ficam fora do escopo.
