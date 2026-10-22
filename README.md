# mmap-wal-log

Log append-only sobre un archivo mapeado en memoria, con CRC32C por registro, recuperación de cola rota y un lector de rescate.

Append-only log on a memory-mapped file, with a CRC32C per record, torn-tail recovery and a salvage reader.

```rust
let (mut log, report) = Log::open("data.log", Options::default())?;
let seq = log.append(b"hello")?;   // visible to iter() at once
log.flush()?;                      // durable: msync of the dirty range (+ fsync if the file grew)
for rec in log.iter() { /* rec.seq, rec.offset, rec.payload */ }
```

## Español

**Formato**: cabecera de 24 bytes (`MWL1`, longitud, número de secuencia `u64`, CRC32C sobre longitud+secuencia+payload, 4 bytes reservados en cero), payload y relleno en cero hasta múltiplo de 8. El resto del archivo está en cero; el archivo arranca con 1 MiB (configurable) y se duplica al llenarse.

**Recuperación al abrir**

- Registro roto o parcial al final (append interrumpido): se descarta, la cola se pone en cero y se hace `msync` **antes** de aceptar appends nuevos. Si no, un crash posterior podría resucitar registros viejos con los mismos números de secuencia que se van a reusar.
- Daño seguido de registros válidos (bit rot, escritura perdida): por defecto `open` **se niega** (`InvalidData`) y no toca el archivo. `OnDamage::DiscardRest` conserva lo anterior y borra el resto; `salvage()` lee en solo lectura saltando el daño (resincroniza en la grilla de 8 bytes con magic + CRC) e informa los rangos dañados y las secuencias perdidas.
- Un registro válido con secuencia equivocada (copiado de otro log, resto viejo) no se acepta.
- Limitación inherente: un bit dañado en el **último** registro es indistinguible de un append cortado y se recupera como cola rota (se pierde ese registro).

**Pruebas** (`cargo test`)

- Unitarias (5): formato en disco byte a byte, reapertura, crecimiento con remapeo, payload vacío.
- Recuperación (6): archivo cortado en **cada byte** de los dos últimos registros (y el log sigue funcionando); cabecera rota con basura después; cabecera nunca escrita; daño en el medio con las tres políticas; registro viejo con secuencia equivocada; y **cada uno de los 15 296 bits** de un log de 12 registros invertido de a uno: 14 976 aperturas rechazadas, 320 recuperadas como cola rota, y ni `open` ni `salvage` devolvieron nunca un registro alterado.
- Modelo (proptest, 200 casos): appends, flush, reaperturas y cortes del archivo contra un `Vec`.
- Crash: un proceso escritor (`mwl append-loop`) muerto con SIGKILL en momentos aleatorios, con capacidad inicial de 4 KiB para que muera también durante remapeos. Todo registro que reportó durable está, byte a byte. Corrida local de 40 rondas: 602 122 registros, 3 rondas con cola rota recuperada. SIGKILL pierde el proceso, no la page cache: esto prueba el formato y la recuperación, **no** durabilidad ante un corte de luz.
- Mutaciones de control: sin chequeo de CRC fallan 3 de 6 tests de recuperación y el de modelo; aceptando cualquier secuencia falla el test del registro viejo.

**Rendimiento** (criterion, appends/s, mediana; VM de 8 vCPU Intel Xeon, ext4 sobre disco virtual; `write` = el mismo formato con un `write(2)` por registro y `fdatasync`):

| payload | flush | mmap | write(2) |
|---|---|---|---|
| 64 B | nunca (solo al final) | 6.41 M/s | 652 k/s |
| 1 KiB | nunca | 645 k/s | 363 k/s |
| 64 B | cada 64 | 518 k/s | 277 k/s |
| 1 KiB | cada 64 | 268 k/s | 182 k/s |
| 64 B | cada registro | 39.2 k/s | 11.8 k/s |

Lectura honesta: sin flush el mmap evita una syscall por registro, pero con 1 KiB domina el costo de fallos de página sobre un archivo recién extendido. Con flush por registro la diferencia depende del sistema de archivos y del disco virtual (`msync` de una página contra `fdatasync`); no generalizar estos números a hardware real.

**Límites**: un solo escritor; nadie debe truncar el archivo mientras está mapeado (sería SIGBUS); no hay compactación ni recorte del inicio; payload máximo 1 GiB.

## English

**Format**: 24-byte header (`MWL1`, length, `u64` sequence number, CRC32C over length+sequence+payload, 4 reserved zero bytes), payload, and zero padding to a multiple of 8. The rest of the file is zero; the file starts at 1 MiB (configurable) and doubles when full.

**Recovery on open**

- A broken or partial record at the end (an interrupted append) is dropped, the tail is zeroed and `msync`ed **before** new appends are accepted. Otherwise a later crash could resurrect old records with the very sequence numbers about to be reused.
- Damage followed by valid records (bit rot, a lost write): by default `open` **refuses** (`InvalidData`) and leaves the file alone. `OnDamage::DiscardRest` keeps what came before and erases the rest; `salvage()` reads read-only around the damage (resyncing on the 8-byte grid with magic + CRC) and reports damaged ranges and lost sequence numbers.
- A valid record with the wrong sequence number (copied from another log, a stale leftover) is not accepted.
- Inherent limit: a damaged bit in the **last** record is indistinguishable from a cut-off append and is recovered as a torn tail (that record is lost).

**Tests** (`cargo test`)

- Unit (5): on-disk format byte by byte, reopen, growth with remap, empty payload.
- Recovery (6): file cut at **every byte** of the last two records (and the log keeps working); broken header with junk after it; header never written; mid-log damage under all three policies; stale record with the wrong sequence; and **every one of the 15,296 bits** of a 12-record log flipped one at a time: 14,976 opens refused, 320 recovered as a torn tail, and neither `open` nor `salvage` ever returned an altered record.
- Model (proptest, 200 cases): appends, flushes, reopens and file cuts against a `Vec`.
- Crash: a writer process (`mwl append-loop`) killed with SIGKILL at random times, with a 4 KiB initial capacity so it also dies mid-remap. Every record it reported durable is there, byte-exact. Local 40-round run: 602,122 records, 3 rounds recovered a torn tail. SIGKILL loses the process, not the page cache: this tests the format and recovery, **not** durability across power loss.
- Control mutations: without the CRC check 3 of 6 recovery tests and the model test fail; accepting any sequence number fails the stale-record test.

**Performance**: see the table above (criterion, appends/s, median; 8-vCPU Intel Xeon VM, ext4 on a virtual disk; `write` = the same format with one `write(2)` per record and `fdatasync`).

Honest reading: without flushes mmap saves one syscall per record, but at 1 KiB page faults on a freshly extended file dominate. With a flush per record the gap depends on the filesystem and virtual disk (`msync` of one page vs `fdatasync`); do not generalize these numbers to real hardware.

**Limits**: single writer; nobody may truncate the file while it is mapped (that would be SIGBUS); no compaction or head trimming; 1 GiB max payload.

## CLI

```
cargo run --release --bin mwl -- verify data.log
cargo run --release --bin mwl -- salvage data.log
```

## License

Apache-2.0
