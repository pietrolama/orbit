# Sicurezza

ORBIT legge il filesystem dell'utente ed espone un'API locale per l'interfaccia grafica. Questo documento descrive le garanzie implementate e i limiti noti.

## Garanzie

1. **Nessuna azione distruttiva.** ORBIT scrive solo nel proprio database SQLite. Non esistono endpoint o comandi che eliminino, spostino o modifichino file analizzati. *Recover* è informativo.
2. **Nessun comando di shell.** I file vengono aperti con `xdg-open` tramite vettore di argomenti (`Command::new("xdg-open").arg(path)`), mai interpolando stringhe in una shell.
3. **Apertura file verificata.** `POST /api/open-file` accetta solo un *id di entità*; il percorso viene risolto dal database e riverificato su disco (`lstat`: tipo, `dev`, `inode`). Se il percorso indica ora un oggetto diverso, la richiesta viene rifiutata (409). File eseguibili (bit `x`), script e lanciatori (`.desktop`, `.sh`, `.AppImage`, …) non vengono mai aperti: è disponibile solo «Mostra nella cartella».
4. **Lettura limitata dei contenuti.** Fingerprint: 2 × 64 KiB per file candidato, hash completo solo su richiesta esplicita e con budget. Riferimenti: solo Markdown/testo ≤ 1 MiB, file con byte NUL scartati. Anteprime: testo ≤ 16 KiB, immagini raster ≤ 8 MiB (niente SVG), servite con `X-Content-Type-Options: nosniff`. Nessun contenuto viene eseguito.
5. **Escaping nella UI.** Nomi, percorsi e motivazioni sono inseriti con `textContent`, mai come HTML. CSP della pagina: `default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data: blob:; connect-src 'self'; frame-ancestors 'none'`.

## Server locale

Il loopback (`127.0.0.1`) **non** è considerato sufficiente. Misure:

- **Codice di avvio monouso → token di sessione.** All'avvio vengono generati da `/dev/urandom` un codice di avvio (256 bit) e un token di sessione (256 bit). L'URL di avvio contiene solo il codice, nel *fragment* (`/#launch=…`), che non viene mai inviato nella riga di richiesta HTTP. La pagina scambia il codice con il token (`POST /api/session`), dopodiché il codice è invalidato. Il token resta in `sessionStorage` della scheda e non compare in URL, argomenti di processo o log.
- **Autorizzazione su tutte le API.** Ogni `/api/*` (tranne lo scambio) richiede `X-Orbit-Token`, confrontato in tempo costante. Solo `GET /api/preview` accetta anche `?token=` (necessario per `<img>`).
- **Host / Origin / Referer / Sec-Fetch-Site.** `Host` deve essere `127.0.0.1` o `localhost` con la porta della sessione (anti DNS-rebinding); `Origin` e `Referer`, se presenti, devono essere la stessa origine; `Sec-Fetch-Site: cross-site` è rifiutato.
- **`POST` solo con `Content-Type: application/json`**: un form HTML di un sito esterno non può produrre una richiesta valida.
- **Limiti**: corpo ≤ 1 MiB, riga d'intestazione ≤ 8 KiB, ≤ 64 intestazioni, ≤ 64 connessioni simultanee, timeout socket, `Transfer-Encoding` rifiutato, al massimo 10 000 posizioni per salvataggio.
- **Percorsi**: `POST /api/scan/start` accetta solo percorsi assoluti, canonicalizzati, che siano directory.

I test negativi sono in `crates/orbit-app/tests/ui_api_integration_tests.rs` (token assente/errato, codice riusato, host/origini estranee, cross-site, content-type errato, corpi enormi, intestazioni troppo lunghe, eseguibili, file sostituiti).

## Limiti noti

- Un altro processo dello **stesso utente** può leggere il database e la memoria di ORBIT: il modello di minaccia non include processi ostili con gli stessi privilegi.
- L'URL di avvio passato a `qml6`/Chromium compare brevemente negli argomenti del processo finestra (leggibili da altri utenti locali via `/proc`). Il codice è monouso: viene invalidato al primo caricamento della pagina. Il file QML temporaneo è creato con permessi `0600` in una directory `0700` e rimosso subito dopo l'avvio.
- `/api/fs/list-dirs` elenca i nomi delle sottodirectory di un percorso (per il selettore di cartelle): è protetto dal token.
- Il database non è cifrato: contiene percorsi, metadati, hash e relazioni.

## Segnalazioni

Apri una segnalazione privata tramite GitHub Security Advisories su https://github.com/pietrolama/orbit.
