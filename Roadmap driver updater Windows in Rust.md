# Roadmap: driver updater Windows in Rust

Oct 25, 2026 · @Michele

## Obiettivo e perimetro

La v1 è un servizio Windows in Rust che trova e installa aggiornamenti driver da due sole fonti affidabili: Windows Update (tramite WUA) e i cataloghi ufficiali di Dell, HP e Lenovo. Niente scraping di siti di produttori.

Dentro la v1:

- Inventario di dispositivi e driver installati
- Ricerca dei driver disponibili su Windows Update
- Rilevamento del modello del PC e lettura del catalogo OEM corrispondente
- Confronto delle versioni con regole di priorità
- Installazione con punto di ripristino, backup e rollback
- UI per rivedere e approvare gli aggiornamenti

Fuori dalla v1:

- Siti di Intel, AMD, NVIDIA, Realtek e dei produttori di schede madri: non hanno API, servirebbe scraping
- PC assemblati oltre a quanto offre Windows Update
- BIOS e firmware: i cataloghi OEM li contengono, ma il rischio è troppo alto per una prima versione
- Ridistribuzione dei pacchetti dai propri server: si scarica sempre dalla fonte originale

## Architettura

Tutto il lavoro privilegiato vive in un servizio Windows in Rust; la UI (egui) gira come utente normale e parla col servizio tramite named pipe.

&#91;embedded content: architettura · servizio, provider, UI\]

I due provider producono candidati nello stesso formato; il matching decide cosa proporre, e l'installer passa sempre dal modulo di sicurezza.

| Crate | Uso |
| --- | --- |
| `windows` | WUA via COM, SetupAPI, newdev, System Restore, WinTrust |
| `windows-service` | Servizio Windows |
| `wmi` | Modello e identificativi macchina |
| `reqwest`, `tokio` | Download asincroni dei cataloghi |
| `quick-xml`, `serde` | Parsing dei cataloghi |
| `cab` | Estrazione dei cataloghi compressi |
| `sha2` | Verifica degli hash |
| `rusqlite` | Stato, storico, cache |
| `tracing` | Log strutturati |
| `eframe` / `egui` | UI desktop nativa |

## Fasi

Otto fasi in sequenza: prima si eliminano i rischi tecnici con un prototipo a riga di comando, poi si costruiscono i provider, e solo alla fine servizio, UI e distribuzione. Ogni fase ha un criterio di completamento verificabile.

### Fase 0 — Spike tecnici

- [x] Chiamare WUA da Rust via COM: `IUpdateSession` → `IUpdateSearcher.Search` con criterio `IsInstalled=0 and Type='Driver'`, stampando titolo, hardware ID e data versione di ogni risultato (`IWindowsDriverUpdate`)
- [x] Enumerare i dispositivi con `SetupDiGetClassDevs` e leggere hardware ID, versione e data del driver dalle device property
- [x] Leggere produttore, modello e identificativo macchina da WMI/SMBIOS
- [x] Scaricare e parsare un catalogo per ciascuna marca

Fatto quando: su tre PC reali, uno per marca, un CLI elenca dispositivi, driver disponibili su Windows Update e voci del catalogo OEM per quel modello.

### Fase 1 — Inventario

- [x] Modello dati `Device`: instance ID, hardware ID e compatible ID, classe, provider, versione, data, nome INF
- [x] Snapshot serializzabile in JSON, utile per test e diagnostica
- [ ] Test su VM con hardware virtuale noto (test pronto: `WDU_EXPECT_VM=hyperv cargo test -- --ignored`, da eseguire in una VM Hyper-V gen 2)

Fatto quando: l'inventario coincide con quanto mostra Gestione dispositivi.

### Fase 2 — Provider Windows Update

- [x] Ricerca e mappatura di ogni `IUpdate` in un candidato comune (hardware ID, modello, provider, classe, data)
- [x] Download e installazione con `IUpdateDownloader` e `IUpdateInstaller`, gestendo l'eventuale riavvio richiesto
- [x] Ricerca su un thread dedicato con timeout: WUA è lento e bloccante
- [x] Rilevare quando una policy aziendale esclude i driver da Windows Update e segnalarlo

Fatto quando: su VM installa un driver da Windows Update e registra l'esito.

Stato: implementato (`wdu wu-install`, `wdu update --only`, storico in `%ProgramData%\WDU\history.jsonl`); la ricerca asincrona è verificata su PC reale, l'installazione va ancora provata su VM con un driver applicabile.

### Fase 3 — Provider cataloghi OEM

Ogni marca identifica la macchina in modo diverso e pubblica un formato diverso. Gli endpoint vanno verificati sulla documentazione attuale, perché cambiano nel tempo.

| Marca | Identificativo macchina | Catalogo | App ufficiale di confronto |
| --- | --- | --- | --- |
| Dell | System ID (SKU) | XML compresso in CAB, catalogo unico filtrato per modello | Dell Command Update |
| HP | Platform ID della scheda madre | File di riferimento XML per piattaforma e versione di Windows | HP Image Assistant |
| Lenovo | Machine type (primi 4 caratteri del modello) | XML per machine type, più un descrittore XML per pacchetto | Lenovo System Update |

- [x] Trait `Provider` comune, un'implementazione per marca
- [x] Cache dei cataloghi con ETag e Last-Modified
- [x] Verifica dell'hash dichiarato nel catalogo e della firma Authenticode del pacchetto
- [x] Strategia di installazione: estrarre l'INF e installarlo via PnP quando possibile, altrimenti lanciare l'installer del produttore con i suoi switch silenziosi

Fatto quando: per ogni marca, su un modello reale, la lista proposta coincide con quella dell'app ufficiale.

Stato: implementato; hash e firma verificati su un pacchetto reale per marca (firmatari `Dell Technologies Inc.`, `HP Inc.`, `Lenovo`). L'INF si installa per ora con `pnputil /add-driver /install`, sostituito in fase 5. Estrazione e installer silenziosi vanno provati su modelli reali con privilegi di amministratore, e il confronto con le app ufficiali resta da fare.

### Fase 4 — Matching e policy

- [x] Confronto per hardware ID, dal più specifico al più generico
- [x] Confronto della versione a quattro parti presa da `DriverVer`, poi della data
- [x] Priorità: catalogo OEM prima di Windows Update; mai downgrade
- [x] Allow list e deny list per classe di dispositivo
- [x] Modalità dry-run che mostra cosa farebbe senza installare

Fatto quando: il dry-run sul parco di prova non produce falsi positivi noti.

Stato: implementato (`wdu update`, policy JSON con `--policy`). Il dry-run va eseguito sul parco di prova.

### Fase 5 — Installazione sicura

- [x] Sequenza: punto di ripristino, backup del driver attuale, installazione, verifica dello stato del dispositivo, rollback automatico se fallisce
- [x] Installazione INF con `UpdateDriverForPlugAndPlayDevices` o `DiInstallDriver`

Fatto quando: il rollback funziona provocando un errore deliberato su VM.

Stato: implementato (`session.rs`): punto di ripristino per sessione, backup del driver attuale (export dal driver store per i pacchetti `oemNN.inf`, INF di sistema per quelli inbox), installazione INF con `UpdateDriverForPlugAndPlayDevices`, verifica di presenza e problem code (il codice 14 vale come riavvio necessario), reinstallazione forzata del driver precedente. Per la prova su VM: `wdu update --only <ID> --inject-failure` (da amministratore) fa fallire la verifica e scatena il rollback; l'esito finisce nello storico come `rolled_back`. Il rollback ripristina solo il driver: il software aggiunto da un installer del produttore resta.

### Fase 6 — Servizio e UI

- [x] Servizio LocalSystem con il crate `windows-service`
- [x] Named pipe con ACL ristretta e protocollo JSON versionato
- [x] UI egui non elevata: lista aggiornamenti, approvazione, storico, pianificazione
- [x] Controllo periodico; installazione solo su approvazione o secondo policy

Fatto quando: un utente standard completa il ciclo dalla UI senza prompt UAC per ogni operazione.

Stato: implementato. Pipe `\\.\pipe\wdu`: client remoti rifiutati; DACL che dà agli utenti interattivi lettura e scrittura ma non la creazione di istanze; prima istanza esclusiva, così nessun altro processo si spaccia per il servizio. Messaggi JSON su una riga con versione `v`. Il servizio accetta solo installazioni di ID proposti dall'ultima scansione, valida la configurazione e esegue un'operazione alla volta. Provati in console come utente standard: protocollo, scansione e UI. Da provare con il servizio installato (`wdu-service install` da amministratore) il ciclo completo con un aggiornamento reale.

### Fase 7 — Distribuzione

- [ ] Firma del codice di eseguibili e installer
- [ ] Pacchetto MSI (per esempio con `cargo-wix`) e aggiornamento automatico dell'app
- [ ] Log locali esportabili per il supporto
- [ ] Beta su un gruppo di macchine reali miste Dell, HP, Lenovo

Fatto quando: la beta gira senza BSOD né regressioni funzionali segnalate.

## Sicurezza e affidabilità

Un driver sbagliato può rendere un PC non avviabile, quindi ogni installazione deve essere reversibile e ogni pacchetto verificato prima di toccare il sistema.

- **Privilegi minimi.** Solo il servizio è elevato; la UI non lo è mai. La pipe accetta connessioni solo da utenti autorizzati e il servizio valida ogni richiesta.
- **Origine dei pacchetti.** Download solo in HTTPS da domini in allow list. Si controllano l'hash dichiarato nel catalogo e la firma Authenticode (`WinVerifyTrust`); i driver kernel devono essere firmati.
- **Punto di ripristino.** Creato con `SRSetRestorePoint` prima di ogni sessione. Windows può avere Ripristino configurazione disattivato o limitare la frequenza dei punti, quindi non è l'unica rete di sicurezza.
- **Backup del driver attuale.** Export dal driver store (come fa `pnputil /export-driver`) prima di installare; il rollback reinstalla l'INF precedente.
- **Verifica post-installazione.** Lettura dello stato del dispositivo e del suo problem code; in caso di errore, rollback automatico.
- **Policy per classi delicate.** Audio, GPU su portatili ibridi, Wi-Fi e Bluetooth sui portatili: solo da catalogo OEM o Windows Update, mai pacchetti generici. Un driver con extension INF dell'OEM non viene sostituito.
- **Riavvii.** Mai forzati: il servizio segnala che serve un riavvio e l'utente decide quando.
- **BIOS e firmware.** Esclusi dalla v1, anche se presenti nei cataloghi.

## Rischi aperti e decisioni

Il rischio principale non è tecnico: i cataloghi OEM non sono API contrattuali e possono cambiare formato o termini d'uso.

Rischi:

- **Cataloghi che cambiano.** Mitigazione: provider isolati dietro il trait comune e test automatici periodici contro i cataloghi reali.
- **Termini d'uso.** Da verificare se Dell, HP e Lenovo permettono l'uso automatizzato dei cataloghi da parte di app di terzi.
- **Policy aziendali.** Le GPO possono escludere i driver da Windows Update o bloccare installazioni; il tool deve rilevarlo e non aggirarlo.
- **Installer dei produttori.** Gli switch silenziosi variano da pacchetto a pacchetto e possono riavviare il PC da soli.

Decisioni da prendere:

- [ ] Target: uso personale/consumer o parco macchine aziendale con gestione centralizzata?
- [ ] Installare solo INF estratti o anche eseguire gli installer dei produttori?
- [ ] Inserire BIOS e firmware in una v2?
- [ ] Licenza e modello di distribuzione: open source come Comprimio o prodotto commerciale?
