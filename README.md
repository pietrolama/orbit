# ORBIT

**Memoria spaziale e relazionale del filesystem.** ORBIT affianca il file manager: oltre a *dove* si trova un file, mostra *a cosa appartiene* — versioni dello stesso documento, file modificati insieme, duplicati, riferimenti espliciti e associazioni create dall'utente — anche quando vivono in directory lontane.

- **Local-first e privato**: scansione, database SQLite e motore delle relazioni girano in locale. Nessun cloud, nessuna telemetria, nessun LLM.
- **Relazioni spiegabili**: ogni arco ha tipo, peso, confidenza, provenienza e motivazioni leggibili.
- **Identità persistente**: rinomine e spostamenti non fanno perdere storia, posizioni e relazioni; l'incertezza è esplicita.
- **Orientamento stabile**: layout deterministico e incrementale, pin e posizioni manuali mai sovrascritti, ricerca reversibile.
- **Mai distruttivo**: ORBIT non elimina né modifica i file analizzati.

> Stato: prototipo funzionale (v0.1.0). Linux.

## Installazione

Requisiti: Rust stabile (≥ 1.80), `libsqlite3.so.0` (caricata a runtime), `xdg-open` per aprire i file. Per la finestra desktop ORBIT usa, se presenti, `qml6` (Qt 6 WebEngine) o un browser Chromium in modalità app; altrimenti stampa un indirizzo locale da aprire.

```bash
cargo build --release
./target/release/orbit --help
# installazione per l'utente corrente (~/.local/bin + voce di menu):
packaging/linux/install-local.sh
```

## Uso rapido

```bash
orbit scan ~/projects                  # ogni sottocartella diventa un progetto (dipendenze e build saltate)
orbit scan ~/Documenti                 # scansione + snapshot + relazioni + layout
orbit scan ~/Documenti --verify-duplicates   # aggiunge l'hash completo dei candidati duplicati
orbit search "fattura 2026"            # ricerca con motivazioni e vicini relazionali
orbit search "ext:pdf size:>10MB modified:last-30d"
orbit entity 1234                      # dettaglio, relazioni spiegate, cronologia
orbit history diff previous latest     # spostati / modificati / nuovi / non osservati
orbit relations add 12 34 --note "stesso cliente"
orbit recover                          # report informativo sullo spazio (non elimina nulla)
orbit ui                               # interfaccia grafica locale (Explore / History / Recover)
orbit status
orbit bench --files 100000             # benchmark su dataset sintetico in una dir temporanea
```

Il database è in `$ORBIT_DB`, altrimenti `~/.local/share/orbit/orbit.db` (`--db` per cambiarlo).

### Sintassi di ricerca

| clausola | significato |
|---|---|
| `fattura 2026`, `"nota spese"` | ogni termine nel nome o nel percorso |
| `name:` / `path:` | termine nel nome / nel percorso |
| `ext:pdf,docx` | estensione |
| `type:file` / `type:dir` | tipo di entità |
| `size:>10MB`, `size:<=4k`, `size:1MB..1GB` | dimensione (base 1024) |
| `modified:last-7d` (`m`,`h`,`d`,`w`), `modified:>2026-01-01`, `modified:2026-01-01..2026-02-01` | data di modifica |
| `related-to:<id>` | entità collegate, ordinate per punteggio relazionale |

### Interfaccia

- **Progetti**: la vista di partenza. Scegli un progetto e vedi solo il suo albero; attorno orbitano gli altri progetti (o cartelle) che contengono file collegati — riferimenti, duplicati, file identici — con una linea da ciascun file coinvolto. Seleziona un file o una cartella del progetto per evidenziare solo i suoi collegamenti; «deboli» mostra anche quelli basati solo su nomi simili o modifiche ravvicinate.
- **Explore**: canvas con pan (trascina il vuoto), zoom (rotella), selezione (clic, Maiusc+clic per selezione multipla), trascinamento dei nodi (posizione manuale; le directory portano con sé i figli), pin, collegamenti manuali, costellazioni. Tre livelli: **Galassia** (cluster e flussi aggregati), **Sistema** (directory, file, relazioni forti, *satelliti* che vivono altrove), **File** (metadati, anteprima locale, relazioni spiegate, cronologia).
- **Ricerca gravitazionale**: i risultati e i loro vicini vengono attratti verso il centro, il resto si attenua; `Esc` riporta tutto alle coordinate originali esatte. Nulla viene salvato.
- **History**: confronto tra snapshot basato sulle identità (spostato ≠ eliminato + creato).
- **Recover**: file grandi, duplicati verificati e candidati, cache note — spazio *rilevato* distinto da quello *recuperabile*.

Scorciatoie: `/` cerca, `Esc` annulla ricerca/selezione/collegamento, `F` inquadra tutto.

## Struttura

```
crates/orbit-core   core indipendente dalla UI (scanner, identità, store, relazioni, layout, ricerca)
crates/orbit-cli    binario `orbit` (CLI headless + `orbit ui` + `orbit bench`)
crates/orbit-app    server API locale + UI Canvas (binario `orbit-app`)
docs/               architettura, decisioni, schema SQL, modello delle relazioni, test e benchmark
```

Documentazione: [ARCHITECTURE](docs/ARCHITECTURE.md) · [PRODUCT_DECISIONS](docs/PRODUCT_DECISIONS.md) · [RELATION_MODEL](docs/RELATION_MODEL.md) · [SCHEMA_V1.sql](docs/SCHEMA_V1.sql) · [TESTING](docs/TESTING.md) · [SECURITY](SECURITY.md)

ORBIT nasce dal riuso selettivo di [DiskGarden](https://github.com/pietrolama/diskgarden) (scanner, modello della scansione, wrapper SQLite, test sintetici, trasporto HTTP locale).

## Licenza

GPL-3.0-or-later. Vedi [LICENSE](LICENSE).
