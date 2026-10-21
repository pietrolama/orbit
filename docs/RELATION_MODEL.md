# Modello delle relazioni

Ogni arco registra: sorgente, destinazione, tipo, **peso** (`[0,1]`, intensità euristica), **confidenza** (`[0,1]`, quanto l'evidenza è affidabile), **provenienza** (stringa macchina, es. `name:root=contratto`, `markdown_link:line=12`, `blake3:full=…`), **motivazioni** leggibili, **origine** (`auto` / `manual`) e **stato** (`active`, `hidden`, `stale`, `deleted`). Nessun valore è una probabilità calibrata.

## Tipi

| tipo | fonte | direzione | peso | confidenza |
|---|---|---|---|---|
| `ParentChild` | `file_entities.parent_entity_id` (non memorizzato: derivato) | genitore → figlio | 1 | 1 |
| `NameSimilarity` | radice del nome normalizzata | no | 0.55 base, +0.25 marcatori di versione, +0.1 stessa estensione, +0.05 stessa directory | 0.5 (0.7 con marcatori), ×0.75 nei gruppi grandi |
| `TemporalAffinity` | mtime in directory diverse | no | `exp(-Δt/τ)`, τ = 30 min | ≤ 0.4; ×0.8 se i timestamp non hanno frazioni di secondo; ridotta con la densità |
| `TemporalAffinity` (co-attività) | file cambiati insieme in ≥ 2 scansioni | no | `1 - 0.5^n` | 0.3 + 0.05·n, ≤ 0.6 |
| `FingerprintMatch` | stessa dimensione + BLAKE3(dim ‖ primi 64 KiB ‖ ultimi 64 KiB) | no | 0.9 | 0.7 (0.4 se < 64 B) |
| `VerifiedDuplicate` | BLAKE3 del contenuto completo uguale (o campione che copre l'intero file ≤ 128 KiB) | no | 1 | 0.99 (0.6 se < 64 B) |
| `ExplicitReference` | link Markdown, definizioni, autolink `file://`, percorsi nel testo | sorgente → destinazione | 0.9 | 0.9 (0.75 per percorsi nel testo) |
| `ManualAssociation` | utente | no | 1 | 1 |
| `ProjectMembership` | appartenenza a una costellazione (`constellation_members`) | — | — | — |

## Generazione dei candidati (nessun confronto O(N²))

- **Nomi**: mappa *radice → file*. Gruppi ≤ 8: tutte le coppie; ≤ 150: catena per mtime; > 150: considerati generici, nessun arco (riportato in `name_groups_generic_skipped`). Radici troppo corte o generiche (`index`, `readme`, `main`, `screenshot`, …) escluse.
- **Tempo**: file ordinati per mtime, finestra scorrevole (≤ 4 h, peso ≥ 0.2). Densità = file entro ±τ: oltre 60 il file è parte di un'operazione di massa (estrazione, clone, copia) e non genera affinità. Si tengono i top-3 vicini per nodo (unione). Timestamp mancanti, zero o futuri sono scartati.
- **Fingerprint**: solo file con dimensione condivisa da almeno un altro file; gruppi per `(dimensione, hash campione)`; archi a stella verso il primo membro. Hard link esclusi (stesso inode, non copie).
- **Riferimenti**: mappa *percorso assoluto → entità presente*.

Complessità complessiva `O(N log N)`. Su 100 000 nodi sintetici il test `candidate_generation_stays_sparse_on_100k_nodes` verifica che gli archi restino ≤ 10·N.

## Punteggio combinato

Per una coppia con più archi attivi: `1 - Π (1 - w_tipo · peso · confidenza)` (noisy-or), con `w_tipo` configurabile (`RelationConfig::kind_weights`). Usato per ordinare vicini, satelliti e `related-to:`.

## Ricalcolo e decisioni dell'utente

Il calcolo produce l'insieme completo e ordinato degli archi automatici; `apply_auto_edges` lo applica come **diff** sul database:

- chiave nuova → inserita `active`;
- chiave esistente → aggiornata solo se peso/confidenza/motivazioni cambiano; `hidden` resta `hidden`; `stale` torna `active`;
- arco automatico attivo non più prodotto → `stale` (mai cancellato).

Gli archi manuali non vengono mai toccati dal ricalcolo. Cancellare un arco manuale è reversibile (`deleted` → `active`); «rimuovere» un arco automatico lo nasconde (`hidden`), ripristinabile dalla UI o con `orbit relations show`.

## Fingerprint e validità

Un fingerprint è legato alla versione osservata (`size`, `mtime`, `dev`, `inode`). Prima di leggere, il file viene ri-`lstat`ato: se è cambiato dopo la scansione viene saltato. Un fingerprint non valido per la versione corrente non produce archi e viene ricalcolato. L'hash completo è calcolato solo con `--verify-duplicates` (o la casella nella UI), con budget di 2 GiB per passata.

## Limiti attuali

- Il parsing dei riferimenti copre solo Markdown e testo semplice; il codice sorgente richiederà parser per linguaggio.
- La co-attività richiede almeno due scansioni con modifiche e ignora insiemi di modifiche > 64 file.
- `ProjectMembership` automatica (marcatori di progetto come `.git`, `Cargo.toml`) non è ancora implementata.
