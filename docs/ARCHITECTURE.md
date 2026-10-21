# Architettura di ORBIT

## Audit di DiskGarden (Milestone 0)

Repository di partenza: `~/projects/diskGarden` (GPL-3.0-or-later, stessa licenza di ORBIT; dipendenze: `serde`, `serde_json`, `libc`, `thiserror`, `tempfile`). Baseline verificata prima del riuso: `cargo test --workspace` → 19 test superati.

| Componente DiskGarden | Decisione | Note |
|---|---|---|
| `scanner.rs` (1596 righe) | **Riusato** quasi invariato | `lstat`, permessi negati ≠ directory vuota, hard link (dedup `dev,ino`), symlink e cicli, confini di mount, cancellazione cooperativa, I/O parallelo a batch, percorsi non UTF-8. Unica modifica: le esclusioni `/proc`, `/sys`, `/dev`, `/run` sono ora `ScanConfig::excluded_paths`, configurabili e registrate esplicitamente come `ExcludedMount`. |
| `model.rs` | **Riusato** ed esteso | Arena della scansione invariata; aggiunto il livello ORBIT (entità, archi, nodi del grafo). |
| `analyzer.rs` | **Riuso parziale** | Tenuti ordinamento, filtri, top-N, statistiche per estensione. Rimossi treemap, viste di navigazione e qualificazione *Recover* di DiskGarden; il report spazio di ORBIT è nuovo e lavora sul grafo. |
| `snapshot.rs` | **Riuso parziale** | Tenuto il wrapper `dlopen` di `libsqlite3.so.0` (ora `sqlite.rs`, con `busy_timeout`, `double`, transazioni). Lo store è nuovo: DiskGarden identifica le righe per percorso dentro uno snapshot, ORBIT ha bisogno di identità persistenti. |
| `diff.rs` | **Non importato** | Il confronto per percorso non distingue uno spostamento da eliminazione+creazione; sostituito da `history.rs` basato sulle entità. |
| Test sintetici dello scanner | **Riusati** (12 test) | Rimosse solo le asserzioni sulla treemap. Incluso il confronto con GNU `du`. |
| Server HTTP (`disk-garden-app`) | **Riuso del trasporto, indurito** | Problemi trovati: corpo della richiesta senza limite (`Content-Length` arbitrario → allocazione), token di sessione nell'URL e quindi negli argomenti del processo finestra, file QML temporaneo con il token leggibile da altri utenti, `xdg-open` su qualunque file. Tutti corretti (vedi `SECURITY.md`). |
| UI treemap (`ui/index.html`) | **Non importata** | Sostituita dalla UI Canvas di ORBIT. |
| Schema SQL | **Sostituito** | Inoltre lo schema v1 di DiskGarden non ammetteva `symlink_cycle_skipped` nel `CHECK` di `access_status` (lo snapshot di una scansione con cicli avrebbe fallito l'inserimento): corretto nello schema ORBIT. |

## Workspace

```
orbit-core  (nessuna dipendenza da UI)
 ├─ scanner, model          scansione e arena (DiskGarden)
 ├─ identity                identità best-effort tra scansioni
 ├─ sqlite, store           persistenza (schema v1, migrazioni)
 ├─ fingerprint             campioni BLAKE3 + hash completo opzionale
 ├─ references              link Markdown / percorsi nel testo
 ├─ relations               motore delle relazioni (candidati indicizzati)
 ├─ layout                  layout deterministico, persistente, incrementale
 ├─ graph                   modello di lettura + viste LOD
 ├─ search                  parsing e ranking delle query
 ├─ history                 diff tra snapshot per entità
 ├─ analyzer                helper di scansione + report spazio
 └─ index                   pipeline che orchestra gli stadi
orbit-cli   → orbit-core, orbit-app      binario `orbit`
orbit-app   → orbit-core                 server API + UI statica, binario `orbit-app`
```

Dipendenze esterne: `serde`, `serde_json`, `libc`, `thiserror`, `blake3` (feature `pure`, nessun codice C); `tempfile` solo nei test. Nessun framework HTTP, nessun WebView in Rust: la finestra è `qml6` (Qt WebEngine) o un browser Chromium in modalità app, come in DiskGarden.

## Pipeline

```
scan_directory ──► OrbitStore::record_scan ──► refresh:
                    │ resolve_identities        fingerprint (cache per versione)
                    │ snapshot_entries          references  (cache per versione)
                    │ file_entities             relations   (calcolo O(N log N))
                    │ co_activity               apply_auto_edges (diff persistito)
                    ▼                           update_layout (solo nodi nuovi/spostati)
                 snapshot
```

## Modello dati

Tre livelli distinti (vedi `docs/SCHEMA_V1.sql`):

- **`snapshot_entries`** — osservazioni immutabili di una scansione (percorso, metadati, stato di accesso, *come* sono state collegate a un'entità e con quale confidenza).
- **`file_entities`** — identità persistente best-effort con stato `present` / `unobserved` / `probably_deleted`. Un'entità mai vista da una scansione completa la cui directory antenata più vicina è leggibile resta `unobserved`, mai `probably_deleted`. Nessuno stato «eliminato con certezza».
- **`spatial_entities` + `node_positions`** — ciò che l'utente riconosce nello spazio: posizione, raggio, `pinned`, `source` (`auto`/`manual`), `cluster_id` (genitore fisico al momento del piazzamento), versione del layout, per canvas.

### Identità

`identity::resolve_identities` collega le osservazioni alle entità in tre stadi (stesso percorso → stesso inode → stesso nome+dimensione+mtime), dal più forte al più debole, e registra in `identity_candidates` le corrispondenze plausibili ma insufficienti (riuso di inode, copie multiple) invece di inventarle. Le entità presenti in *altre* radici non sono mai candidate a uno spostamento. Tabella completa nel commento del modulo.

## Layout

Circle packing gerarchico: ogni directory è un disco i cui figli sono piazzati su una spirale ad angolo aureo attorno a un nucleo riservato, saltando le posizioni che sovrappongono un fratello (griglia spaziale). Il packing è calcolato dal basso per i sottoalberi senza posizione, poi il sottoalbero viene piazzato vicino al genitore già posizionato. La fase angolare deriva da `hash(seed, id)`, mai dall'ordine di iterazione.

Regole: le posizioni esistenti non vengono ricalcolate; pin e posizioni manuali non vengono mai mossi (garantito anche in SQL: l'upsert automatico ha `WHERE pinned = 0 AND source = 'auto'`); un nodo automatico il cui genitore fisico è cambiato viene ripiazzato vicino al nuovo genitore e i suoi discendenti automatici traslati con lui. `orbit layout reset` ricalcola tutto tranne i nodi bloccati.

## Zoom semantico

| livello | endpoint | contenuto | limiti |
|---|---|---|---|
| Galassia | `GET /api/graph/overview` | cluster (directory) scelti dividendo i più popolosi fino al limite; flussi = relazioni aggregate tra cluster per tipo | ≤ 300 cluster, ≤ 250 flussi (configurabili) |
| Sistema | `GET /api/graph/region` | nodi nel viewport con raggio a schermo ≥ soglia, archi più forti tra essi, tutti gli archi del nodo a fuoco | ≤ 2500 nodi, ≤ 1500 archi |
| File | `GET /api/graph/entity` | metadati, fingerprint, relazioni spiegate (anche nascoste/eliminate, ripristinabili), satelliti, cronologia, identità incerte, anteprima | — |

I *satelliti* sono entità correlate fuori dal sottoalbero del nodo a fuoco: la UI li disegna su un'orbita tratteggiata con un filo verso la posizione reale, così non sembrano figli fisici.

## Ricerca

`search::search` è un ranking puro sul modello di lettura: punteggio per termine (nome uguale 1.0 > prefisso 0.8 > confine di parola 0.65 > contenuto 0.5 > percorso 0.25), piccoli bonus per file e attività recente, ordinamento deterministico (punteggio, poi dimensione o mtime, poi id). Per i primi risultati restituisce i vicini relazionali con punteggio. La UI anima i risultati verso il centro (`GravitySession` in `renderer.js`) partendo da una copia delle coordinate persistenti e, alla chiusura, termina esattamente su quei valori. Debounce 180 ms, query superate scartate per numero di sequenza.

## API

Versione del contratto: `X-Orbit-Api: 1` su ogni risposta, `api_version` in `/api/status`. Errori: `{"error": "..."}` con 400/401/403/404/405/409/413/415/431/500/503. Durante una scansione le viste servono il grafo precedente (consistente); `POST /api/scan/start` restituisce 409 se un'altra è in corso.

| metodo e percorso | corpo / query | risposta |
|---|---|---|
| `POST /api/session` | `{launch_code}` | `{token, api_version}` (codice monouso) |
| `GET /api/status` | — | job, progressi, ultimo report, statistiche |
| `POST /api/scan/start` | `{root_path, follow_symlinks?, cross_mounts?, verify_duplicates?}` | `{started, job_id}` |
| `POST /api/scan/cancel` | `{}` | `{cancelled}` |
| `POST /api/relations/refresh` | `{verify_duplicates?}` | report di indicizzazione |
| `GET /api/graph/overview` | `max_clusters, max_flows` | `OverviewView` |
| `GET /api/graph/region` | `x0,y0,x1,y1,scale,min_px?,max_nodes?,max_edges?,min_strength?,kinds?,focus?` | `RegionView` |
| `GET /api/graph/entity` | `id` | `EntityDetail` |
| `POST /api/search` | `{query, limit?}` | `{search: SearchResponse, nodes: [NodeView]}` |
| `GET /api/history/snapshots` | — | `[SnapshotHeader]` |
| `POST /api/history/diff` | `{base, target, limit?}` | `HistoryDiff` |
| `POST /api/positions/save` | `{positions:[{entity_id,x,y}], with_descendants?, pin?}` | `{saved}` |
| `POST /api/positions/pin` | `{entity_ids, pinned}` | `{updated}` |
| `POST /api/positions/release` | `{entity_ids}` | `{updated}` |
| `POST /api/layout/reset` | `{seed?}` | report layout |
| `GET /api/constellations` | — | elenco |
| `POST /api/constellations/create` | `{name, color?, members}` | costellazione |
| `POST /api/constellations/update` | `{id, name?, color?, add?, remove?, deleted?}` | costellazione |
| `POST /api/relations/create` | `{a, b, note?}` | arco manuale |
| `POST /api/relations/delete` | `{edge_id}` | manuale → `deleted`, automatica → `hidden` |
| `POST /api/relations/restore` | `{edge_id}` | arco attivo |
| `POST /api/relations/explain` | `{a, b}` | archi + punteggio combinato |
| `POST /api/open-file` | `{entity_id, mode: open\|reveal}` | `{opened, …}` |
| `GET /api/preview` | `id` (+ `token` in query ammesso) | testo JSON o bytes immagine |
| `GET /api/recover` | — | `SpaceReport` |
| `GET /api/fs/list-dirs` | `path` | sottodirectory |

## Persistenza

SQLite in WAL, `foreign_keys=ON`, una connessione per operazione (lo store è `Send + Sync`), aggiornamenti multi-statement in `BEGIN IMMEDIATE`. Uno snapshot nasce in stato `saving` e diventa `completed`/`partial` solo a fine transazione; un crash lo lascia `saving` e la riapertura lo marca `interrupted` (mai usato come baseline, rimosso per primo da `snapshot prune`). Migrazioni: `PRAGMA user_version` + `schema_migrations`; un database più recente del binario viene rifiutato, non riscritto.
