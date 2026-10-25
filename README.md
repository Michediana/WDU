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
```

`cargo test` include confronti live dell'inventario con `Get-PnpDevice` e
`Win32_PnPSignedDriver`; il test su hardware virtuale noto va lanciato in una VM
Hyper-V gen 2 con `WDU_EXPECT_VM=hyperv cargo test -- --ignored`.

Struttura:

- `crates/wdu-core` — modello dati, inventario (SetupAPI), identità macchina (WMI),
  ricerca Windows Update (WUA/COM), parser dei cataloghi Dell, HP e Lenovo
- `crates/wdu-cli` — CLI `wdu`
