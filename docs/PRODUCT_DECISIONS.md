# Decisioni di prodotto e tecniche

Registro delle decisioni prese durante l'implementazione, con motivazione. Le decisioni marcate *rivedibile* sono state scelte per non bloccare il lavoro e possono cambiare senza migrazioni distruttive.

1. **Complementare al file manager.** ORBIT apre file e cartelle con l'applicazione predefinita (`xdg-open`) e non offre copia/sposta/elimina.
2. **Nessuna certezza di eliminazione.** Gli stati sono `present`, `unobserved`, `probably_deleted`. Un errore di permesso, un'esclusione o una scansione parziale producono solo `unobserved`. Uno stato `probably_deleted` non viene «declassato» a `unobserved` da una scansione meno informativa.
3. **Le scansioni annullate non diventano snapshot.** Le scansioni parziali sì (con stato `partial`).
4. **ParentChild non è memorizzato negli archi.** La gerarchia fisica è la fonte di verità e vive in `file_entities.parent_entity_id`; l'esportazione e la UI la presentano come arco `parent_child` con origine `filesystem`.
5. **Ricalcolo globale, persistenza incrementale.** Il calcolo dei candidati è rifatto a ogni indicizzazione (O(N log N), ~1 s a 100k file) mentre fingerprint e riferimenti sono in cache per versione e la scrittura degli archi è un diff. Un ricalcolo per «insiemi sporchi» è un'ottimizzazione futura *rivedibile*.
6. **SQLite via `dlopen`** (ereditato da DiskGarden) invece di `rusqlite`: nessun toolchain C al build e nessuna copia di SQLite nel binario. Richiede `libsqlite3.so.0` a runtime, presente in tutte le distribuzioni desktop.
7. **BLAKE3** per entrambi gli stadi del fingerprint, con feature `pure` (solo Rust). Un hash parziale non produce mai `VerifiedDuplicate`.
8. **Nessun WebView incorporato in Rust.** `wry`/`webkit2gtk` aggiungerebbero dipendenze di sistema pesanti; si riusa la strategia di DiskGarden (Qt WebEngine via `qml6`, poi Chromium `--app`, poi `xdg-open`). *Rivedibile* se si sceglie un canale IPC dedicato.
9. **Codice di avvio monouso nel fragment** invece del token nell'URL (vedi `SECURITY.md`): il token di sessione non appare mai negli argomenti di un processo.
10. **Trascinare un nodo = posizione manuale.** La posizione diventa `source = manual` e il layout automatico non la toccherà più; il pin è un flag distinto (blocca anche `layout reset`). «Layout automatico» nel pannello restituisce il nodo al motore senza spostarlo subito.
11. **Trascinare una directory porta con sé i discendenti automatici**; quelli fissati o manuali restano dove sono.
12. **Le coordinate della ricerca non esistono lato server.** Il server restituisce solo coordinate persistenti; l'attrazione è un'animazione della UI, e il drag è disabilitato durante una ricerca.
13. **Le costellazioni sono entità spaziali** con posizione propria (centroide dei membri al momento della creazione) e cancellazione morbida.
14. **Esclusioni predefinite** `/proc`, `/sys`, `/dev`, `/run`: configurabili (`--exclude`, `--no-default-excludes`) e sempre visibili come entry `excluded_mount` con messaggio, mai nascoste. Le directory `.git` *non* sono escluse per non nascondere dati in modo sorprendente.
15. **Lingua.** Interfaccia e messaggi in italiano; identificatori e commenti del codice in inglese.
16. **Pesi delle relazioni** (`RelationConfig`): valori iniziali scelti a mano e testati per il comportamento atteso (ordinamenti, soglie), non calibrati statisticamente.
