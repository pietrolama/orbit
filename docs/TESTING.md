# Test e benchmark

Tutti i test lavorano in directory temporanee (`tempfile`), non leggono il filesystem reale dell'host oltre alle proprie fixture e funzionano offline una volta scaricate le dipendenze. L'unico test che invoca uno strumento esterno è il confronto con GNU `du` ereditato da DiskGarden.

## Comandi

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
node --test crates/orbit-app/tests/ui_gravity.test.js
cargo run --release -p orbit-cli -- bench --files 100000
```

La CI (`.github/workflows/ci.yml`) esegue gli stessi comandi su ogni push e pull request (il benchmark con 5 000 file come smoke test).

## Suite

| file | test | copertura |
|---|---:|---|
| `orbit-core/tests/scanner_synthetic_tests.rs` | 12 | ereditati da DiskGarden: gerarchie, Unicode e non UTF-8, symlink e cicli, permessi negati, file sparsi, hard link, cancellazione, directory scomparse, mount, confronto con `du` |
| `orbit-core/tests/identity_tests.rs` | 14 | identità invariata, rinomina/spostamento di file e directory, modifica in place vs salvataggio atomico, eliminazione solo «probabile», permessi negati → non osservato, scansioni annullate rifiutate, round trip non UTF-8, sotto-radici, riuso di inode, spostamenti cross-device univoci/ambigui, copie in altre radici, migrazioni, recupero da crash, schema più recente rifiutato, prune |
| `orbit-core/tests/relation_tests.rs` | 12 | radici dei nomi, genealogia spiegata, gruppi grandi e generici, regole temporali (stessa directory, timestamp mancanti/futuri, lontananza), burst, determinismo e assenza di auto-archi, sparsità su 100k nodi, fingerprint a due stadi con collisione di campioni e invalidazione, parsing Markdown/testo sicuro, archi di riferimento diretti e file binari, relazioni manuali persistenti, nascoste che restano nascoste, archi obsoleti, persistenza incrementale |
| `orbit-core/tests/graph_tests.rs` | 11 | layout deterministico e dipendente dal seed, nessuna sovrapposizione tra fratelli, inserimenti che non spostano i nodi esistenti, spostamenti con traslazione dei discendenti e pin fermi, relayout che preserva pin/manuali, store che non sovrascrive posizioni manuali, vicini e satelliti, limiti di overview/region, parsing delle query, ranking riproducibile (nessun risultato, uno, molti, filtri, `related-to`, troncamento), ricerca senza effetti sulla persistenza |
| `orbit-core/tests/history_tests.rs` | 2 | spostati/modificati/nuovi/eliminati, snapshot parziale → non osservato |
| `orbit-app/tests/ui_api_integration_tests.rs` | 10 | token assente/errato, token in query rifiutato, codice di avvio monouso, Host/Origin/Referer/Sec-Fetch-Site, Content-Type, corpi e intestazioni troppo grandi, CSP, apertura file (eseguibili mai aperti, file sostituiti rifiutati), viste del grafo, anteprime, ricerca read-only, relazioni manuali, posizioni e pin resistenti a refresh e reset, costellazioni, job di scansione, history, recover |
| `orbit-app/tests/ui_gravity.test.js` (Node) | 5 | bersagli gravitazionali deterministici con pin fermi, ripristino bit-esatto delle coordinate, isolamento dello snapshot, riposizionamento continuo, matematica della camera |

Totale: 61 test Rust + 5 test JavaScript, tutti verdi all'ultima esecuzione (21/10/2026).

I test sui permessi vengono saltati (con messaggio) se eseguiti da root.

## Benchmark

`orbit bench` crea un dataset sintetico in una directory temporanea (rimossa al termine): directory a due livelli con 100 file ciascuna, nomi con famiglie di versioni (`<parola>_<proprietario>_<n><suffisso>`), Unicode, un nome non UTF-8, ~2 % di duplicati di contenuto, file da 64 B a 350 KiB, sessioni di lavoro con mtime vicini, note Markdown con link a file esistenti, una directory illeggibile (scansione parziale). Esegue la pipeline due volte (a freddo e con il disco invariato) e misura ogni stadio.

### Risultati misurati (100 000 file)

Macchina: Intel Core Ultra 7 255H (16 thread), 30 GiB RAM, dataset su tmpfs (page cache calda), build `--release`, 21/10/2026. Un solo run: i valori variano tra esecuzioni e macchine; su disco fisico a cache fredda la scansione sarà più lenta.

| misura | valore |
|---|---:|
| voci scansionate | 101 028 (100 001 file, 1 026 directory, 681.9 MiB) |
| scansione | 0.14 s (seconda scansione invariata: 0.43 s) |
| indicizzazione a freddo totale | 12.3 s |
| · registrazione snapshot + identità | 2.8 s |
| · fingerprint (campioni) | 0.27 s |
| · riferimenti | 0.23 s |
| · calcolo relazioni | 0.98 s |
| · persistenza archi (309 221 inserimenti) | 5.9 s |
| · layout | 1.4 s |
| indicizzazione a caldo totale | 9.3 s |
| · registrazione snapshot + identità | 4.3 s |
| · persistenza archi (diff) | 1.4 s |
| · layout (nessun nodo nuovo) | 0.19 s |
| caricamento del grafo in memoria | 0.87 s |
| vista Galassia | 0.12 s |
| vista regione (2 500 nodi restituiti) | 20 ms |
| ricerca, mediana su 5 | 12–27 ms |
| archi | 309 221: 203 066 temporali, 97 048 nominali, 8 716 riferimenti, 321 duplicati verificati, 70 fingerprint |
| memoria di picco del processo (VmHWM) | 764 MiB |

Osservazioni:

- La persistenza degli archi domina l'indicizzazione a freddo; la registrazione a caldo era 8.9 s prima dell'ottimizzazione che salta le scritture delle righe invariate (ora 4.3 s).
- Gli archi temporali sono ~2 per file: lineari ma numerosi. Un taglio più aggressivo è una scelta di prodotto aperta.
- La memoria di picco include due `ScanResult` completi tenuti dal benchmark, il grafo e la cache delle pagine SQLite.
- Il frame rate della UI non è misurato automaticamente; le viste sono limitate lato server (≤ 2 500 nodi, ≤ 1 500 archi per regione) per contenere il costo di disegno.
