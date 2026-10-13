# Tusic

Player de música para terminal Linux, com busca no Audius e streaming de faixas públicas completas. Sem login e sem salvar arquivos de música: o `mpv` mantém apenas o buffer temporário em memória.

## Executar

Instale Rust e `mpv` (no Debian/Ubuntu: `sudo apt install mpv`).

```sh
cargo run --release
```

Use um terminal com pelo menos 70 colunas e 16 linhas. É necessária conexão à internet; o catálogo Audius não equivale ao catálogo do YouTube Music. Faixas restritas são excluídas da busca. A fila existe apenas durante a sessão.

## Controles

- Digite na busca e pressione Enter; Tab ou Shift+Tab muda o foco.
- Setas selecionam resultados e músicas na fila; Enter toca a seleção.
- Nos controles, use ←/→ e Enter. Todos os botões também aceitam clique.
- Clique seleciona uma faixa; a roda do mouse navega na lista.
- `a` adiciona o resultado selecionado à fila; Delete remove da fila.
- Espaço pausa/retoma; `+` e `-` ajustam o volume.
- Escape sai da edição da busca; `q` fora da busca ou Ctrl+C encerra.

A fila avança ao terminar uma faixa e para quando acaba. “Tocar” uma faixa da fila remove essa entrada da fila. “Anterior” retorna à faixa anterior ouvida na sessão.

## Verificar

```sh
cargo fmt -- --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Os testes de integração requerem `mpv`. A busca usa a [API pública Audius](https://docs.audius.co/api/); a reprodução usa o [IPC JSON do mpv](https://mpv.io/manual/stable/#json-ipc).

## License

MIT — veja [LICENSE](LICENSE).
