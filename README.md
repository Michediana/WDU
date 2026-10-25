# WDU — Windows driver updater

Prototipo in Rust (vedi la [roadmap](Roadmap%20driver%20updater%20Windows%20in%20Rust.md)).

```
cargo build
target/debug/wdu machine                      # produttore, modello, ID per il catalogo OEM
target/debug/wdu devices [--hidden] [--all]   # dispositivi e driver installati
target/debug/wdu snapshot -o inventory.json   # snapshot JSON (diagnostica e test)
target/debug/wdu wu                           # driver disponibili su Windows Update
target/debug/wdu catalog                      # catalogo OEM di questa macchina
target/debug/wdu catalog --brand dell --id 0A20
target/debug/wdu catalog --brand hp --id 8549 --win10
target/debug/wdu catalog --brand lenovo --id 20XW --resolve
target/debug/wdu report -o report.json        # tutto insieme (criterio della fase 0)

target/debug/wdu wu-policy                    # GPO/MDM/WSUS che limitano i driver
target/debug/wdu update                       # dry-run: cosa installerebbe e perché
target/debug/wdu update --skipped             # anche ogni candidato scartato, col motivo
target/debug/wdu update --policy policy.json  # allow/deny list per classe
target/debug/wdu update --only <ID>           # installa (da amministratore)
target/debug/wdu update --all                 # installa tutto ciò che propone
target/debug/wdu wu-install <UPDATE_ID>       # installa da Windows Update per ID
target/debug/wdu history                      # esiti delle installazioni
target/debug/wdu verify <file>                # firma Authenticode
```

Policy (`policy.json`, tutti i campi facoltativi):

```json
{ "allow_classes": [], "deny_classes": ["Display"], "oem_exclusive": true }
```

`oem_exclusive` (default `true`): se il catalogo OEM copre un dispositivo, Windows
Update non viene mai proposto per quel dispositivo, anche se offre un driver più
recente.

I cataloghi sono in cache in `%LOCALAPPDATA%\WDU\cache` (rivalidati con ETag e
Last-Modified, `--no-cache` per scaricarli di nuovo); pacchetti e storico sono in
`%ProgramData%\WDU`.

`cargo test` include confronti live dell'inventario con `Get-PnpDevice` e
`Win32_PnPSignedDriver`; il test su hardware virtuale noto va lanciato in una VM
Hyper-V gen 2 con `WDU_EXPECT_VM=hyperv cargo test -- --ignored`.

Struttura:

- `crates/wdu-core` — modello dati, inventario (SetupAPI), identità macchina (WMI),
  Windows Update (WUA/COM: ricerca, installazione, policy), cataloghi Dell, HP e
  Lenovo con cache, trait `Provider` e candidati comuni, matching e policy,
  verifica Authenticode, installazione dei pacchetti OEM, storico
- `crates/wdu-cli` — CLI `wdu`
